//! Process-lifetime regression tests: an agent tree must follow its daemon and
//! must disappear when it is killed on purpose.
//!
//! * `agent_dies_when_daemon_is_hard_killed` - a `kill -9` (Unix) or Task
//!   Manager kill (Windows) never runs the daemon's shutdown path, so the
//!   guarantee has to come from the OS. Why the grandchild matters: closing the
//!   `ConPTY` already takes down the direct client, so the interesting orphan is
//!   a process started in its own console. On Windows that is what the job
//!   object in `ab_core::reaper` is for; on Unix the kernel sends `SIGHUP`
//!   through the PTY and descendants that ignore the hangup can still survive,
//!   which is documented in `SECURITY.md`.
//! * `kill_force_reaps_descendants` - `agent-bridge kill --force` must take the
//!   agent's process tree down and must not disturb the daemon. Scope differs by
//!   platform: Windows reaps the whole job object (a grandchild in its own
//!   console included), Unix `SIGKILL`s the agent's process group, which is why
//!   the Unix case uses a non-interactive shell - an interactive shell would
//!   move the background job into a group of its own (see `SECURITY.md`).
//! * `kill_force_reaps_after_client_exit` - the same teardown must run when the
//!   direct client has already exited and only its descendants are left; that
//!   is the case `kill --force` used to skip by returning early on liveness.
//!
//! All tests start a real daemon process and a real PTY agent.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use agent_bridge_core::client;
use agent_bridge_core::protocol::{AgentStatus, Request, Response};

/// Time the OS gets to tear an agent tree down.
const DEATH_TIMEOUT: Duration = Duration::from_secs(15);

/// `AGENT_BRIDGE_HOME` / `AGENT_BRIDGE_PIPE` are process-wide, so the tests in
/// this binary must not run at the same time.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// A shell that stays alive until it is killed.
fn shell_command() -> Vec<String> {
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

/// Daemon under test. Owning the child means a failed assertion still tears it
/// down (and its agents) instead of leaking a daemon into the next run.
struct DaemonProcess(Child);

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the daemon with its stderr captured next to the socket.
fn start_daemon(home: &Path) -> DaemonProcess {
    let log = std::fs::File::create(home.join("daemon.log")).expect("create daemon log");
    let child = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .args(["daemon", "run"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .expect("spawn daemon");
    DaemonProcess(child)
}

/// Best-effort removal; Windows can hold the copied image for a moment.
fn remove_home(home: &Path) {
    // The detached grandchild runs a copy of ping.exe. Retry the image delete
    // together with the directory removal: an AV scanner or a not-yet-released
    // handle can make the first attempt fail.
    for _ in 0..20 {
        if let Ok(entries) = std::fs::read_dir(home) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("ab-orphan-")
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        if std::fs::remove_dir_all(home).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    eprintln!("warning: could not remove {}", home.display());
}

/// Daemon log, or a readable placeholder - used in failure messages.
fn dump_log(home: &Path) -> String {
    let path = home.join("daemon.log");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| format!("<could not read {}: {err}>", path.display()))
}

fn wait_until_ready(home: &Path) -> Response {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(version @ Response::Version { .. }) = client::request(&Request::Version) {
            return version;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not answer within 20s\n--- daemon.log ---\n{}",
            dump_log(home)
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_agent(command: Vec<String>) -> u32 {
    let created = client::request(&Request::New {
        name: "orphan".to_string(),
        command: Some(command),
        cwd: None,
        cols: None,
        rows: None,
    })
    .expect("spawn agent");
    assert!(matches!(created, Response::Agent { .. }), "got {created:?}");
    agent_pid()
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

/// Poll until the daemon reports the agent as exited.
fn wait_for_exit(agent: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Response::Agents { agents }) = client::request(&Request::List) {
            if agents
                .iter()
                .any(|info| info.name == agent && matches!(info.status, AgentStatus::Exited))
            {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the agent '{agent}' should exit on its own"
        );
        thread::sleep(Duration::from_millis(100));
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

/// Poll the agent's output for a `<marker><pid>` number.
#[cfg(unix)]
fn read_pid(agent: &str, marker: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Response::Output { lines, .. }) = client::request(&Request::Read {
            target: agent.to_string(),
            lines: Some(80),
        }) {
            for line in &lines {
                if let Some((_, rest)) = line.split_once(marker) {
                    if let Ok(pid) = rest.trim().parse::<u32>() {
                        return pid;
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("the agent never printed {marker}<pid>");
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

/// Assert every pid dies within [`DEATH_TIMEOUT`], showing the daemon log when
/// they do not.
fn assert_all_dead(tracked: &[u32], home: &Path) {
    let deadline = Instant::now() + DEATH_TIMEOUT;
    loop {
        let survivors: Vec<u32> = tracked
            .iter()
            .copied()
            .filter(|pid| is_alive(*pid))
            .collect();
        if survivors.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "still running after {DEATH_TIMEOUT:?}: {survivors:?}\n--- daemon.log ---\n{}",
            dump_log(home)
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn agent_dies_when_daemon_is_hard_killed() {
    let _env = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let home = isolate();
    let mut daemon = start_daemon(&home);

    let Response::Version {
        pid: daemon_pid, ..
    } = wait_until_ready(&home)
    else {
        panic!("expected a version response");
    };

    let agent = spawn_agent(shell_command());
    // On Unix the daemon-death guarantee is the PTY hangup, which reaches the
    // foreground group only - a background job would not be covered, so it is
    // not tracked here (see SECURITY.md).
    #[cfg(windows)]
    let tracked = vec![agent, spawn_detached_grandchild("orphan", &home)];
    #[cfg(unix)]
    let tracked = vec![agent];

    let _cleanup = Cleanup(tracked.clone());
    for pid in &tracked {
        assert!(
            is_alive(*pid),
            "pid {pid} should be running before the kill"
        );
    }

    hard_kill(daemon_pid);
    let _ = daemon.0.wait();
    assert!(!is_alive(daemon_pid), "the daemon should be gone");

    assert_all_dead(&tracked, &home);
    drop(daemon);
    remove_home(&home);
}

#[test]
fn kill_force_reaps_descendants() {
    let _env = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let home = isolate();
    let daemon = start_daemon(&home);
    wait_until_ready(&home);

    // Windows: a grandchild in its own console (job object must reap it).
    // Unix: a non-interactive shell, so `sleep 600 &` stays in the agent's
    // process group. An interactive shell would give it a group of its own,
    // which SIGKILL to the agent's group cannot reach by design.
    #[cfg(windows)]
    let tracked = {
        let agent = spawn_agent(shell_command());
        vec![agent, spawn_detached_grandchild("orphan", &home)]
    };
    #[cfg(unix)]
    let tracked = {
        let agent = spawn_agent(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 600 & echo ORPHAN:$!; wait".to_string(),
        ]);
        vec![agent, read_pid("orphan", "ORPHAN:")]
    };

    let _cleanup = Cleanup(tracked.clone());
    for pid in &tracked {
        assert!(
            is_alive(*pid),
            "pid {pid} should be running before the kill"
        );
    }

    let killed = client::request(&Request::Kill {
        name: "orphan".to_string(),
        force: Some(true),
    })
    .expect("kill agent");
    assert!(matches!(killed, Response::Ok), "got {killed:?}");

    assert_all_dead(&tracked, &home);
    assert!(
        client::is_running(),
        "killing an agent must not take the daemon down"
    );
    drop(daemon);
    remove_home(&home);
}

/// A force kill must still reach the tree after the direct client exited - the
/// case where `kill --force` used to return early and leave descendants alive
/// until the daemon stopped.
#[test]
fn kill_force_reaps_after_client_exit() {
    let _env = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let home = isolate();
    let daemon = start_daemon(&home);
    wait_until_ready(&home);

    // Windows: the shell exits, the detached grandchild (own console) stays in
    // the job object.
    #[cfg(windows)]
    let (agent, orphan) = {
        let agent = spawn_agent(shell_command());
        let orphan = spawn_detached_grandchild("orphan", &home);
        send("orphan", "exit");
        (agent, orphan)
    };
    // Unix: the shell exits, the background job ignores SIGHUP (so the kernel's
    // session-leader hangup does not take it down) and stays in the agent's
    // process group, where only `killpg` can reach it. The trap is installed in
    // the parent *before* the fork: setting it inside the subshell loses a race
    // against the SIGHUP the kernel sends when the leader exits.
    #[cfg(unix)]
    let (agent, orphan) = {
        let agent = spawn_agent(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "trap '' HUP; (exec sleep 600) & echo ORPHAN:$!; exit 0".to_string(),
        ]);
        (agent, read_pid("orphan", "ORPHAN:"))
    };

    let _cleanup = Cleanup(vec![agent, orphan]);
    assert!(
        is_alive(orphan),
        "the orphan should be running before the kill"
    );

    wait_for_exit("orphan");
    assert!(!is_alive(agent), "the direct client should be gone");

    let killed = client::request(&Request::Kill {
        name: "orphan".to_string(),
        force: Some(true),
    })
    .expect("kill agent");
    assert!(matches!(killed, Response::Ok), "got {killed:?}");

    assert_all_dead(&[orphan], &home);
    assert!(
        client::is_running(),
        "killing an agent must not take the daemon down"
    );
    drop(daemon);
    remove_home(&home);
}
