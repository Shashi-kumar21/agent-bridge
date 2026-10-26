//! Regression test: an agent tree must not outlive a daemon that is killed hard.
//!
//! A `kill -9` (Unix) or Task Manager kill (Windows) never runs the daemon's
//! shutdown path, so the guarantee has to come from the OS. This test starts a
//! real daemon process and a real PTY agent, terminates the daemon without
//! warning, and asserts that the agent (and, on Windows, a grandchild that
//! escaped the pseudo console) is gone shortly afterwards.
//!
//! Why the grandchild matters: closing the `ConPTY` already takes down the
//! direct client, so the interesting orphan is a process started in its own
//! console. On Windows that is what the job object in `ab_core::reaper` is for.
//! On Unix the kernel sends `SIGHUP` through the PTY, and descendants that
//! ignore the hangup can still survive; that gap is documented in `SECURITY.md`.

#[cfg(windows)]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use agent_bridge_core::client;
use agent_bridge_core::protocol::{Request, Response};

/// Time the OS gets to tear the agent tree down after the daemon disappears.
const DEATH_TIMEOUT: Duration = Duration::from_secs(15);

/// A shell that stays alive until it is killed.
fn long_lived_command() -> Vec<String> {
    if cfg!(windows) {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![shell]
    } else {
        vec!["/bin/sh".to_string()]
    }
}

/// Point this process (and therefore the spawned daemon) at a private socket.
fn isolate() -> PathBuf {
    let stamp = agent_bridge_core::now_ms();
    let home = std::env::temp_dir().join(format!("agent-bridge-orphan-{stamp}"));
    std::fs::create_dir_all(&home).expect("create temp home");
    std::env::set_var("AGENT_BRIDGE_HOME", &home);
    std::env::set_var("AGENT_BRIDGE_PIPE", format!("agent-bridge-orphan-{stamp}"));
    home
}

fn start_daemon() -> Child {
    Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["daemon", "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon")
}

fn wait_until_ready() -> Response {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(version @ Response::Version { .. }) = client::request(&Request::Version) {
            return version;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not answer within 20s"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn agent_pid() -> u32 {
    match client::request(&Request::List).expect("list agents") {
        Response::Agents { agents } => agents
            .iter()
            .find(|agent| agent.name == "orphan")
            .and_then(|agent| agent.pid)
            .expect("the agent should report a pid"),
        other => panic!("unexpected list response: {other:?}"),
    }
}

#[cfg(windows)]
fn send(target: &str, message: &str) {
    let sent = client::request(&Request::Send {
        target: target.to_string(),
        message: message.to_string(),
        enter: Some(true),
        from: Some("qa".to_string()),
    })
    .expect("send message");
    assert!(matches!(sent, Response::Sent { .. }), "got {sent:?}");
}

/// Kill `pid` without giving it a chance to clean up.
fn hard_kill(pid: u32) {
    #[cfg(windows)]
    let status = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(unix)]
    let status = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = status;
}

/// `true` when `tasklist` reports `pid`.
#[cfg(windows)]
fn tasklist_has_pid(extra_args: [&str; 2], needle: &str) -> bool {
    Command::new("tasklist")
        .args(extra_args)
        .args(["/NH", "/FO", "CSV"])
        .output()
        .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(needle))
}

/// Whether `pid` is still a live (non-zombie) process.
fn is_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        tasklist_has_pid(["/FI", &format!("PID eq {pid}")], &format!("\"{pid}\""))
    }
    #[cfg(unix)]
    {
        // `ps` is present on every Unix this project targets and, unlike
        // `kill -0`, it distinguishes zombies from live processes.
        match Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
        {
            Ok(out) => {
                let state = String::from_utf8_lossy(&out.stdout);
                let state = state.trim();
                !state.is_empty() && !state.starts_with('Z')
            }
            Err(_) => false,
        }
    }
}

/// On Windows, start a detached grandchild (own console) inside the agent and
/// return its pid. This is the process the job object has to reap.
#[cfg(windows)]
fn spawn_detached_grandchild(agent: &str, home: &Path) -> u32 {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let source = Path::new(&system_root).join("System32").join("ping.exe");
    let image = home.join(format!("ab-orphan-{}.exe", agent_bridge_core::now_ms()));
    std::fs::copy(&source, &image).expect("copy ping.exe");
    let name = image
        .file_name()
        .and_then(|name| name.to_str())
        .expect("image name");
    send(
        agent,
        &format!("start \"\" /min \"{}\" -t 127.0.0.1", image.display()),
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(out) = Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {name}"), "/NH", "/FO", "CSV"])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(pid) = text.lines().find_map(|line| {
                let mut fields = line.split(',');
                let image = fields.next()?.trim_matches('"');
                (image == name)
                    .then(|| fields.next()?.trim_matches('"').parse::<u32>().ok())
                    .flatten()
            }) {
                return pid;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("the detached grandchild never started");
}

/// Best-effort cleanup so a failing assertion does not leak a shell.
struct Cleanup(Vec<u32>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in &self.0 {
            if is_alive(*pid) {
                hard_kill(*pid);
            }
        }
    }
}

#[test]
fn agent_dies_when_daemon_is_hard_killed() {
    let home = isolate();
    let mut daemon = start_daemon();

    let Response::Version {
        pid: daemon_pid, ..
    } = wait_until_ready()
    else {
        panic!("expected a version response");
    };

    let created = client::request(&Request::New {
        name: "orphan".to_string(),
        command: Some(long_lived_command()),
        cwd: None,
        cols: None,
        rows: None,
    })
    .expect("spawn agent");
    assert!(matches!(created, Response::Agent { .. }), "got {created:?}");

    #[cfg(windows)]
    let tracked = vec![agent_pid(), spawn_detached_grandchild("orphan", &home)];
    #[cfg(unix)]
    let tracked = vec![agent_pid()];

    let _cleanup = Cleanup(tracked.clone());
    for pid in &tracked {
        assert!(
            is_alive(*pid),
            "pid {pid} should be running before the kill"
        );
    }

    hard_kill(daemon_pid);
    let _ = daemon.wait();
    assert!(!is_alive(daemon_pid), "the daemon should be gone");

    let deadline = Instant::now() + DEATH_TIMEOUT;
    loop {
        let survivors: Vec<u32> = tracked
            .iter()
            .copied()
            .filter(|pid| is_alive(*pid))
            .collect();
        if survivors.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "survived their daemon for {DEATH_TIMEOUT:?}: {survivors:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    let _ = std::fs::remove_dir_all(&home);
}
