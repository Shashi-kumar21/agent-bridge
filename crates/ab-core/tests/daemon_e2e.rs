//! End-to-end test: real daemon, real PTY, real local socket.

use std::thread;
use std::time::{Duration, Instant};

use agent_bridge_core::client;
use agent_bridge_core::daemon::Daemon;
use agent_bridge_core::protocol::{AgentStatus, Request, Response};

fn agent_command() -> Vec<String> {
    if cfg!(windows) {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![shell]
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), "cat".to_string()]
    }
}

#[test]
fn daemon_lists_sends_and_reads() {
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

    assert!(matches!(
        client::request(&Request::Ping).expect("ping"),
        Response::Pong
    ));

    let created = client::request(&Request::New {
        name: "e2e".to_string(),
        command: Some(agent_command()),
        cwd: None,
        cols: Some(80),
        rows: Some(24),
    })
    .expect("create agent");
    assert!(matches!(created, Response::Agent { .. }), "got {created:?}");

    let duplicate = client::request(&Request::New {
        name: "e2e".to_string(),
        command: Some(agent_command()),
        cwd: None,
        cols: None,
        rows: None,
    })
    .expect("duplicate create");
    assert!(matches!(duplicate, Response::Error { .. }));

    let invalid = client::request(&Request::New {
        name: "bad name!".to_string(),
        command: Some(agent_command()),
        cwd: None,
        cols: None,
        rows: None,
    })
    .expect("invalid create");
    assert!(matches!(invalid, Response::Error { .. }));

    let sent = client::request(&Request::Send {
        target: "e2e".to_string(),
        message: "bridge-ping".to_string(),
        enter: Some(true),
        from: Some("qa".to_string()),
    })
    .expect("send message");
    assert!(matches!(sent, Response::Sent { .. }), "got {sent:?}");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut echoed = false;
    while Instant::now() < deadline {
        if let Ok(Response::Output { lines, .. }) = client::request(&Request::Read {
            target: "e2e".to_string(),
            lines: Some(50),
        }) {
            if lines.iter().any(|line| line.contains("bridge-ping")) {
                echoed = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    assert!(echoed, "the PTY should echo the delivered message");

    match client::request(&Request::List).expect("list") {
        Response::Agents { agents } => {
            assert_eq!(agents.len(), 1);
            assert_eq!(agents[0].name, "e2e");
            assert_eq!(agents[0].status, AgentStatus::Running);
            assert_eq!(agents[0].message_count, 1);
        }
        other => panic!("unexpected list response: {other:?}"),
    }

    match client::request(&Request::Log { limit: Some(10) }).expect("log") {
        Response::Messages { messages } => {
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].from.as_deref(), Some("qa"));
        }
        other => panic!("unexpected log response: {other:?}"),
    }

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
    let _ = std::fs::remove_dir_all(&home);
}
