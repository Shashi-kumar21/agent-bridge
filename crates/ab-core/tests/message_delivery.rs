//! End-to-end delivery tests: bounded queue, concurrency and status.
//!
//! This is a separate test binary from `daemon_e2e` because the daemon endpoint
//! is selected through process-wide environment variables.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use agent_bridge_core::client;
use agent_bridge_core::daemon::Daemon;
use agent_bridge_core::delivery::{DEFAULT_QUEUE_BYTES, DEFAULT_QUEUE_LIMIT};
use agent_bridge_core::protocol::{DeliveryStatus, ErrorCode, Request, Response};

/// The daemon endpoint is chosen through process-wide environment variables, so
/// these tests must not overlap (the default test harness runs them in threads).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Agent command: a shell that echoes whatever is typed into its PTY.
fn agent_command() -> Vec<String> {
    if cfg!(windows) {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![shell]
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), "cat".to_string()]
    }
}

fn start_daemon(queue_limit: usize, queue_bytes: usize) -> (thread::JoinHandle<()>, PathBuf) {
    let stamp = agent_bridge_core::now_ms();
    let home = std::env::temp_dir().join(format!("agent-bridge-queue-{stamp}"));
    std::fs::create_dir_all(&home).expect("temp dir");
    std::env::set_var("AGENT_BRIDGE_HOME", &home);
    std::env::set_var("AGENT_BRIDGE_PIPE", format!("agent-bridge-queue-{stamp}"));
    // Set both explicitly so one test cannot leak its limits into the next.
    std::env::set_var("AGENT_BRIDGE_QUEUE_LIMIT", queue_limit.to_string());
    std::env::set_var("AGENT_BRIDGE_QUEUE_BYTES", queue_bytes.to_string());

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

fn create_agent_with(name: &str, command: Vec<String>) {
    let created = client::request(&Request::New {
        name: name.to_string(),
        command: Some(command),
        cwd: None,
        cols: Some(100),
        rows: Some(40),
    })
    .expect("create agent");
    assert!(matches!(created, Response::Agent { .. }), "got {created:?}");
}

fn create_agent(name: &str) {
    create_agent_with(name, agent_command());
}

fn send_with(
    target: &str,
    message: String,
    wait: bool,
    queue_timeout_ms: u64,
    write_timeout_ms: u64,
) -> (u64, DeliveryStatus) {
    let response = client::request(&Request::Send {
        target: target.to_string(),
        message,
        enter: Some(true),
        from: Some("qa".to_string()),
        queue_timeout_ms: Some(queue_timeout_ms),
        write_timeout_ms: Some(write_timeout_ms),
        wait: Some(wait),
    })
    .expect("send message");
    match response {
        Response::Sent {
            message_id, status, ..
        } => (message_id, status),
        other => panic!("expected a sent response, got {other:?}"),
    }
}

fn send(target: &str, message: String, wait: bool) -> (u64, DeliveryStatus) {
    send_with(target, message, wait, 5_000, 10_000)
}

fn terminal_status(id: u64) -> DeliveryStatus {
    match client::request(&Request::MessageStatus { id }).expect("message status") {
        Response::Message { message } => message.status,
        other => panic!("unexpected status response: {other:?}"),
    }
}

fn output_text(target: &str) -> String {
    match client::request(&Request::Read {
        target: target.to_string(),
        lines: Some(10_000),
    })
    .expect("read")
    {
        Response::Output { lines, .. } => lines.join("\n"),
        other => panic!("unexpected read response: {other:?}"),
    }
}

fn shutdown(running: thread::JoinHandle<()>, home: PathBuf) {
    assert!(matches!(
        client::request(&Request::Shutdown).expect("shutdown"),
        Response::Ok
    ));
    running.join().expect("daemon exits");
    let _ = std::fs::remove_dir_all(home);
}

fn kill(name: &str) {
    assert!(matches!(
        client::request(&Request::Kill {
            name: name.to_string(),
            force: Some(true),
        })
        .expect("kill"),
        Response::Ok
    ));
}

/// The roadmap acceptance test: 8 threads x 50 sends to one agent, every
/// payload complete and unique. A split payload would corrupt its marker.
#[test]
fn concurrent_sends_do_not_interleave() {
    const THREADS: usize = 8;
    const PER_THREAD: usize = 50;

    let _serial = serial();
    let (running, home) = start_daemon(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("busy");

    let mut workers = Vec::with_capacity(THREADS);
    for thread_index in 0..THREADS {
        workers.push(thread::spawn(move || {
            let mut markers = Vec::with_capacity(PER_THREAD);
            for message_index in 0..PER_THREAD {
                let marker = format!("k{thread_index:02}{message_index:03}");
                let (id, status) = send("busy", format!("echo {marker}"), true);
                assert!(
                    id > 0,
                    "every message gets a daemon-wide id (thread {thread_index})"
                );
                assert_eq!(
                    status,
                    DeliveryStatus::Written,
                    "thread {thread_index} message {message_index} (id {id})"
                );
                markers.push(marker);
            }
            markers
        }));
    }

    let mut markers = Vec::with_capacity(THREADS * PER_THREAD);
    for worker in workers {
        markers.extend(worker.join().expect("sender thread"));
    }
    assert_eq!(markers.len(), THREADS * PER_THREAD);
    assert_eq!(
        markers
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        markers.len(),
        "markers must be unique"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = output_text("busy");
        let missing: Vec<&String> = markers
            .iter()
            .filter(|marker| !text.contains(marker.as_str()))
            .collect();
        if missing.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the PTY never showed {} complete messages (first missing: {:?})",
            missing.len(),
            missing.first()
        );
        thread::sleep(Duration::from_millis(100));
    }

    // Every marker also has a terminal status and drains out of `pending`.
    match client::request(&Request::Pending {
        target: "busy".to_string(),
        limit: None,
    })
    .expect("pending")
    {
        Response::Messages { messages } => {
            assert!(messages.is_empty(), "queue should be drained: {messages:?}");
        }
        other => panic!("unexpected pending response: {other:?}"),
    }

    kill("busy");
    shutdown(running, home);
}

/// `--no-wait` returns while the message is still queued or being written, and
/// the status becomes terminal without another client action.
#[test]
fn no_wait_returns_before_the_write_and_settles() {
    let _serial = serial();
    let (running, home) = start_daemon(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);

    // On Unix use a target that does not read stdin for a second, so a large
    // payload cannot finish before the daemon builds its reply. ConPTY buffers
    // input eagerly on Windows, so there the payload stays small.
    #[cfg(unix)]
    let (command, payload) = (
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 1; cat".to_string(),
        ],
        format!("echo {}", "n".repeat(48 * 1024)),
    );
    #[cfg(windows)]
    let (command, payload) = (agent_command(), "echo nowait".to_string());

    create_agent_with("quick", command);
    let (id, status) = send("quick", payload, false);
    assert!(id > 0);

    #[cfg(unix)]
    assert!(
        matches!(status, DeliveryStatus::Queued | DeliveryStatus::Writing),
        "no-wait must reply before the write finishes, got {status:?}"
    );
    #[cfg(windows)]
    assert!(
        matches!(
            status,
            DeliveryStatus::Queued | DeliveryStatus::Writing | DeliveryStatus::Written
        ),
        "unexpected early status: {status:?}"
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let current = terminal_status(id);
        if matches!(current, DeliveryStatus::Written) {
            break;
        }
        assert!(
            !matches!(current, DeliveryStatus::Failed { .. }),
            "no-wait delivery failed: {current:?}"
        );
        assert!(
            Instant::now() < deadline,
            "message {id} never reached written: {current:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    kill("quick");
    shutdown(running, home);
}

/// A payload larger than the whole byte budget can never be admitted, so it
/// fails fast with `queue_full` and no partial write.
#[test]
fn oversize_payload_fails_with_queue_full() {
    let _serial = serial();
    let (running, home) = start_daemon(1, 1024);
    create_agent("tiny");

    let (id, status) = send("tiny", "x".repeat(2048), true);
    match status {
        DeliveryStatus::Failed {
            code: ErrorCode::QueueFull,
            ref detail,
        } => assert!(detail.contains("queue is full"), "detail: {detail}"),
        other => panic!("expected queue_full, got {other:?}"),
    }

    // The writer never saw the job, so nothing can have reached the PTY.
    assert!(
        !output_text("tiny").contains("xxxx"),
        "an unadmitted payload must not be written"
    );

    match client::request(&Request::MessageStatus { id }).expect("status") {
        Response::Message { message } => assert!(matches!(
            message.status,
            DeliveryStatus::Failed {
                code: ErrorCode::QueueFull,
                ..
            }
        )),
        other => panic!("unexpected status response: {other:?}"),
    }

    kill("tiny");
    shutdown(running, home);
}

/// A write that blocks inside the PTY returns `unknown` at the caller deadline
/// and is amended to `written` once the target drains its input.
///
/// Unix only: `ConPTY` buffers input eagerly, so the write cannot be blocked
/// deterministically on Windows. The delivery path itself is platform-neutral.
#[cfg(unix)]
#[test]
fn blocked_write_is_unknown_then_amended() {
    let _serial = serial();
    let (running, home) = start_daemon(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent_with(
        "slow",
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 2; cat".to_string(),
        ],
    );

    // Far larger than the kernel PTY input queue (a few KiB).
    let payload = format!("echo {}", "b".repeat(48 * 1024));
    let (id, status) = send_with("slow", payload, true, 5_000, 200);
    assert!(
        matches!(status, DeliveryStatus::Unknown),
        "a blocked write must time out as unknown, got {status:?}"
    );

    // `unknown` stays pending while the write is still in flight.
    match client::request(&Request::Pending {
        target: "slow".to_string(),
        limit: None,
    })
    .expect("pending")
    {
        Response::Messages { messages } => assert!(
            messages.iter().any(|message| message.id == id),
            "an unknown message still in flight must stay pending: {messages:?}"
        ),
        other => panic!("unexpected pending response: {other:?}"),
    }

    // Once `cat` drains stdin the writer completes and amends the record.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let current = terminal_status(id);
        if matches!(current, DeliveryStatus::Written) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "message {id} was never amended from unknown: {current:?}"
        );
        thread::sleep(Duration::from_millis(100));
    }

    kill("slow");
    shutdown(running, home);
}
