//! End-to-end delivery tests: bounded queue, concurrency and status.
//!
//! This is a separate test binary from `daemon_e2e` because the daemon endpoint
//! is selected through process-wide environment variables.

use std::ffi::OsString;
#[cfg(unix)]
use std::io::Read as _;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use agent_bridge_core::client;
use agent_bridge_core::daemon::Daemon;
use agent_bridge_core::delivery::{DEFAULT_QUEUE_BYTES, DEFAULT_QUEUE_LIMIT};
#[cfg(unix)]
use agent_bridge_core::delivery::{PASTE_END, PASTE_START};
#[cfg(unix)]
use agent_bridge_core::protocol::AgentInfo;
use agent_bridge_core::protocol::{DeliveryStatus, ErrorCode, PasteMode, Request, Response};

/// The daemon endpoint is chosen through process-wide environment variables, so
/// these tests must not overlap (the default test harness runs them in threads).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const ENV_KEYS: [&str; 4] = [
    "AGENT_BRIDGE_HOME",
    "AGENT_BRIDGE_PIPE",
    "AGENT_BRIDGE_QUEUE_LIMIT",
    "AGENT_BRIDGE_QUEUE_BYTES",
];

/// One daemon per test, with cleanup on both success and panic.
struct Harness {
    _serial: std::sync::MutexGuard<'static, ()>,
    running: Option<thread::JoinHandle<()>>,
    home: PathBuf,
    previous: Vec<(&'static str, Option<OsString>)>,
}

impl Harness {
    fn start(queue_limit: usize, queue_bytes: usize) -> Self {
        let serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let previous: Vec<(&'static str, Option<OsString>)> = ENV_KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();

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

        Self {
            _serial: serial,
            running: Some(running),
            home,
            previous,
        }
    }
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

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(running) = self.running.take() {
            // Best effort: on a panic the daemon may already be gone.
            let _ = client::request(&Request::Shutdown);
            let _ = running.join();
        }
        let _ = std::fs::remove_dir_all(&self.home);
        for (key, value) in self.previous.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
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

fn send_with(
    target: &str,
    message: String,
    wait: bool,
    queue_timeout_ms: u64,
    write_timeout_ms: u64,
) -> (u64, DeliveryStatus) {
    send_with_paste(
        target,
        message,
        wait,
        queue_timeout_ms,
        write_timeout_ms,
        PasteMode::Auto,
    )
}

fn send_with_paste(
    target: &str,
    message: String,
    wait: bool,
    queue_timeout_ms: u64,
    write_timeout_ms: u64,
    paste: PasteMode,
) -> (u64, DeliveryStatus) {
    let response = client::request(&Request::Send {
        target: target.to_string(),
        message,
        enter: Some(true),
        from: Some("qa".to_string()),
        queue_timeout_ms: Some(queue_timeout_ms),
        write_timeout_ms: Some(write_timeout_ms),
        wait: Some(wait),
        paste: Some(paste),
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

fn pending(target: &str) -> Vec<u64> {
    match client::request(&Request::Pending {
        target: target.to_string(),
        limit: None,
    })
    .expect("pending")
    {
        Response::Messages { messages } => messages.iter().map(|message| message.id).collect(),
        other => panic!("unexpected pending response: {other:?}"),
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

#[cfg(unix)]
fn agent_info(name: &str) -> AgentInfo {
    match client::request(&Request::List).expect("list") {
        Response::Agents { agents } => agents
            .into_iter()
            .find(|agent| agent.name == name)
            .unwrap_or_else(|| panic!("agent {name} is missing")),
        other => panic!("unexpected list response: {other:?}"),
    }
}

#[cfg(unix)]
fn wait_until_flag(name: &str, flag: fn(&AgentInfo) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if flag(&agent_info(name)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never reported the expected terminal capability"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn read_bytes(path: &Path) -> Vec<u8> {
    let mut file = std::fs::File::open(path).expect("capture file");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).expect("read capture file");
    bytes
}

#[cfg(unix)]
fn wait_for_capture(path: &Path, expected: &[u8]) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if path.exists()
            && read_bytes(path)
                .windows(expected.len())
                .any(|w| w == expected)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "capture file never contained the expected bytes"
        );
        thread::sleep(Duration::from_millis(50));
    }
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

    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
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
    // Poll: the last marker can be visible in the buffer before its `Done`
    // observer updates the log.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ids = pending("busy");
        if ids.is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "queue should drain: {ids:?} still pending"
        );
        thread::sleep(Duration::from_millis(50));
    }

    kill("busy");
}

/// `--no-wait` returns after admission and the status settles without another
/// client action. The write can be fast, so the reply may already be `written`;
/// the point is that it is never `unknown`/`failed` and it reaches `written`.
#[test]
fn no_wait_returns_after_admission_and_settles() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("quick");

    let (id, status) = send("quick", "echo nowait".to_string(), false);
    assert!(id > 0);
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
}

/// A payload larger than the whole byte budget can never be admitted, so it
/// fails fast with `queue_full` and no partial write.
#[test]
fn oversize_payload_fails_with_queue_full() {
    let _harness = Harness::start(1, 1024);
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
}

/// A request larger than the ~8 KiB Unix-socket buffer must be read whole.
/// macOS regression: `accept()` inherits `O_NONBLOCK` there, so the daemon
/// must clear it explicitly or the request is dropped mid-line.
#[test]
fn large_request_is_read_whole() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("echoer");

    // > 8 KiB, under the 64 KiB message limit.
    let (_, status) = send("echoer", "x".repeat(12 * 1024), true);
    assert_eq!(status, DeliveryStatus::Written);

    kill("echoer");
}

/// A response larger than the ~8 KiB Unix-socket buffer must be written whole.
/// Same macOS regression, in the daemon -> client direction.
#[test]
fn large_response_is_not_truncated() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    let command = if cfg!(windows) {
        vec![
            "cmd.exe".to_string(),
            "/c".to_string(),
            "for /L %i in (1,1,2000) do @echo marker-line-%i-0123456789".to_string(),
        ]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "seq 1 2000".to_string(),
        ]
    };
    create_agent_with("big", command);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let text = output_text("big");
        if text.contains("1999") || text.contains("marker-line-1999") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "large output never arrived intact ({} bytes)",
            text.len()
        );
        thread::sleep(Duration::from_millis(100));
    }

    kill("big");
}

/// Shell command that advertises bracketed paste and copies everything it
/// receives into `$0`.
#[cfg(unix)]
fn paste_capture_command(file: &Path) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        r#"printf '\033[?2004h'; cat > "$0""#.to_string(),
        file.display().to_string(),
    ]
}

/// A shell command that does not advertise bracketed paste.
#[cfg(unix)]
fn plain_capture_command(file: &Path) -> Vec<String> {
    vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "cat > \"$0\"".to_string(),
        file.display().to_string(),
    ]
}

/// `auto` bracket-pastes multi-line text once the target announces support, so
/// the terminal application sees one paste instead of one submission per line.
#[cfg(unix)]
#[test]
fn auto_paste_wraps_after_the_target_announces_it() {
    let harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    let file = harness.home.join("paste.txt");
    create_agent_with("paster", paste_capture_command(&file));
    wait_until_flag("paster", |agent| agent.bracketed_paste);

    let (_, status) = send_with("paster", "one\ntwo".to_string(), true, 5_000, 10_000);
    assert_eq!(status, DeliveryStatus::Written);

    let expected = [PASTE_START, b"one\ntwo", PASTE_END].concat();
    wait_for_capture(&file, &expected);
    kill("paster");
}

/// Without an announcement, `auto` falls back to plain bytes - no escape
/// markers are typed into the target.
#[cfg(unix)]
#[test]
fn auto_paste_falls_back_when_the_target_stays_plain() {
    let harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    let file = harness.home.join("plain.txt");
    create_agent_with("plain", plain_capture_command(&file));
    assert!(!agent_info("plain").bracketed_paste);

    let (_, status) = send_with("plain", "one\ntwo".to_string(), true, 5_000, 10_000);
    assert_eq!(status, DeliveryStatus::Written);

    wait_for_capture(&file, b"one\ntwo");
    let bytes = read_bytes(&file);
    assert!(
        !bytes.windows(PASTE_START.len()).any(|w| w == PASTE_START),
        "the fallback must not emit a paste marker: {bytes:?}"
    );
    assert!(
        !bytes.windows(PASTE_END.len()).any(|w| w == PASTE_END),
        "the fallback must not emit a paste marker: {bytes:?}"
    );
    kill("plain");
}

/// `--paste bracketed` forces the markers even when the target did not announce
/// support, which is the escape hatch for applications that support it but stay
/// silent.
#[cfg(unix)]
#[test]
fn forced_bracketed_paste_wraps_a_silent_target() {
    let harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    let file = harness.home.join("forced.txt");
    create_agent_with("silent", plain_capture_command(&file));
    assert!(!agent_info("silent").bracketed_paste);

    let (_, status) = send_with_paste(
        "silent",
        "one\ntwo".to_string(),
        true,
        5_000,
        10_000,
        PasteMode::Bracketed,
    );
    assert_eq!(status, DeliveryStatus::Written);

    let expected = [PASTE_START, b"one\ntwo", PASTE_END].concat();
    wait_for_capture(&file, &expected);
    kill("silent");
}
