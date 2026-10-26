//! End-to-end test: real daemon, real PTY, real local socket.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use agent_bridge_core::client;
use agent_bridge_core::daemon::Daemon;
use agent_bridge_core::protocol::{AgentStatus, Request, Response};
use agent_bridge_core::Event;

/// Command that echoes whatever is typed into its PTY.
fn agent_command() -> Vec<String> {
    if cfg!(windows) {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![shell]
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), "cat".to_string()]
    }
}

fn spawn_request(name: &str, cols: Option<u16>, rows: Option<u16>) -> Request {
    Request::New {
        name: name.to_string(),
        command: Some(agent_command()),
        cwd: None,
        cols,
        rows,
    }
}

/// Point every client at an isolated socket, then serve until shutdown.
fn start_daemon() -> (thread::JoinHandle<()>, PathBuf) {
    let stamp = agent_bridge_core::now_ms();
    let home = std::env::temp_dir().join(format!("agent-bridge-test-{stamp}"));
    std::fs::create_dir_all(&home).expect("temp dir");
    std::env::set_var("AGENT_BRIDGE_HOME", &home);
    std::env::set_var("AGENT_BRIDGE_PIPE", format!("agent-bridge-test-{stamp}"));

    let daemon = Daemon::new();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let running = thread::spawn(move || {
        daemon
            .run(move || {
                let _ = ready_tx.send(());
            })
            .expect("daemon run");
    });
    ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("daemon became ready");
    (running, home)
}

fn send_message(target: &str, message: &str) {
    let sent = client::request(&Request::Send {
        target: target.to_string(),
        message: message.to_string(),
        enter: Some(true),
        from: Some("qa".to_string()),
    })
    .expect("send message");
    assert!(matches!(sent, Response::Sent { .. }), "got {sent:?}");
}

/// Poll `read` until the PTY echoes `needle`.
fn wait_for_echo(target: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(Response::Output { lines, .. }) = client::request(&Request::Read {
            target: target.to_string(),
            lines: Some(50),
        }) {
            if lines.iter().any(|line| line.contains(needle)) {
                return;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("the PTY should echo {needle:?}");
}

fn assert_create_rules() {
    let created = client::request(&spawn_request("e2e", Some(80), Some(24))).expect("create agent");
    assert!(matches!(created, Response::Agent { .. }), "got {created:?}");

    let duplicate = client::request(&spawn_request("e2e", None, None)).expect("duplicate create");
    assert!(matches!(duplicate, Response::Error { .. }));

    let invalid = client::request(&spawn_request("bad name!", None, None)).expect("invalid create");
    assert!(matches!(invalid, Response::Error { .. }));
}

fn assert_list_and_log() {
    match client::request(&Request::List).expect("list") {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].name, "e2e");
            assert_eq!(agents[0].status, AgentStatus::Running);
            assert_eq!(agents[0].message_count, 2);
        }
        other => panic!("unexpected list response: {other:?}"),
    }

    match client::request(&Request::Log { limit: Some(10) }).expect("log") {
        Response::Messages { messages } => {
            assert_eq!(messages.len(), 2);
            assert!(messages.iter().all(|m| m.from.as_deref() == Some("qa")));
        }
        other => panic!("unexpected log response: {other:?}"),
    }
}

/// Attach must stream `Hello`, then the `Message` and the echoed `Output`.
fn assert_attach_stream() {
    let mut subscriber = client::Subscriber::attach("e2e", None).expect("attach");
    assert!(matches!(
        subscriber.next_event().expect("hello").as_ref(),
        Some(Event::Hello { .. })
    ));

    send_message("e2e", "attach-ping");

    let mut saw_message = false;
    let mut saw_output = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && !(saw_message && saw_output) {
        match subscriber.next_event() {
            Ok(Some(Event::Message { text, .. })) if text.contains("attach-ping") => {
                saw_message = true;
            }
            Ok(Some(Event::Output { .. })) => saw_output = true,
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    assert!(
        saw_message,
        "attach stream should deliver the message event"
    );
    assert!(saw_output, "attach stream should deliver output events");
}

fn shutdown(running: thread::JoinHandle<()>, home: &std::path::Path) {
    let killed = client::request(&Request::Kill {
        name: "e2e".to_string(),
        force: Some(true),
    })
    .expect("kill");
    assert!(matches!(killed, Response::Ok));

    assert!(matches!(
        client::request(&Request::Shutdown).expect("shutdown"),
        Response::Ok
    ));
    running.join().expect("daemon exits");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn daemon_lists_sends_and_reads() {
    let (running, home) = start_daemon();

    assert!(matches!(
        client::request(&Request::Ping).expect("ping"),
        Response::Pong
    ));

    assert_create_rules();
    send_message("e2e", "bridge-ping");
    wait_for_echo("e2e", "bridge-ping");
    assert_attach_stream();
    assert_list_and_log();

    shutdown(running, &home);
}
