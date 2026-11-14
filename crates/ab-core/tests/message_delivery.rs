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
use agent_bridge_core::protocol::{
    AgentInfo, AgentStatus, DeliveryStatus, ErrorCode, Message, PasteMode, Readiness, Request,
    Response,
};

/// The daemon endpoint is chosen through process-wide environment variables, so
/// these tests must not overlap (the default test harness runs them in threads).
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const ENV_KEYS: [&str; 7] = [
    "AGENT_BRIDGE_HOME",
    "AGENT_BRIDGE_PIPE",
    "AGENT_BRIDGE_QUEUE_LIMIT",
    "AGENT_BRIDGE_QUEUE_BYTES",
    "AGENT_BRIDGE_QUIET_MS",
    "AGENT_BRIDGE_PROMPT_PATTERN",
    "AGENT_BRIDGE_BUFFER_BYTES",
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
        for key in [
            "AGENT_BRIDGE_QUIET_MS",
            "AGENT_BRIDGE_PROMPT_PATTERN",
            "AGENT_BRIDGE_BUFFER_BYTES",
        ] {
            std::env::remove_var(key);
        }

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
        require_ack: None,
        wait_idle: None,
        wait_reply: None,
        timeout_ms: None,
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
        since: None,
    })
    .expect("read")
    {
        Response::Output { lines, .. } => lines.join("\n"),
        other => panic!("unexpected read response: {other:?}"),
    }
}

fn agent_info(name: &str) -> AgentInfo {
    match client::request(&Request::List).expect("list") {
        Response::Agents { agents } => agents
            .into_iter()
            .find(|agent| agent.name == name)
            .unwrap_or_else(|| panic!("agent {name} is missing")),
        other => panic!("unexpected list response: {other:?}"),
    }
}

/// Poll `listagents` until the agent reports the expected readiness.
fn wait_until_readiness(name: &str, expected: Readiness, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let info = agent_info(name);
        if info.readiness == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} stayed {:?}, expected {expected:?} (status {:?}, last_output_at {:?}, output: {:?})",
            info.readiness,
            info.status,
            info.last_output_at,
            output_text(name)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Command that prints one line and then stays silent.
fn prompt_command() -> Vec<String> {
    if cfg!(windows) {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![
            shell,
            "/c".to_string(),
            r"echo AGENT-READY & %SystemRoot%\System32\ping.exe -n 60 127.0.0.1 >nul".to_string(),
        ]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "printf 'AGENT-READY
'; sleep 30"
                .to_string(),
        ]
    }
}

/// Command that never writes to its PTY.
fn silent_command() -> Vec<String> {
    if cfg!(windows) {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![
            shell,
            "/c".to_string(),
            r"%SystemRoot%\System32\ping.exe -n 60 127.0.0.1 >nul".to_string(),
        ]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "sleep 30".to_string(),
        ]
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

/// Send with `require_ack: true`, which is what `--require-ack` sets.
fn send_requiring_ack(target: &str, message: &str) -> (u64, DeliveryStatus) {
    send_with_paste_and_ack(target, message.to_string(), PasteMode::Auto, true)
}

fn message_record(id: u64) -> Message {
    match client::request(&Request::MessageStatus { id }).expect("message status") {
        Response::Message { message } => message,
        other => panic!("unexpected status response: {other:?}"),
    }
}

fn ack(target: &str, id: u64, note: Option<&str>) -> Response {
    client::request(&Request::Ack {
        target: target.to_string(),
        id,
        note: note.map(ToString::to_string),
    })
    .expect("ack request")
}

/// One `Send` frame with explicit `paste` and `require_ack` values.
fn send_with_paste_and_ack(
    target: &str,
    message: String,
    paste: PasteMode,
    require_ack: bool,
) -> (u64, DeliveryStatus) {
    let response = client::request(&Request::Send {
        target: target.to_string(),
        message,
        enter: Some(true),
        from: Some("qa".to_string()),
        queue_timeout_ms: Some(5_000),
        write_timeout_ms: Some(10_000),
        wait: Some(true),
        paste: Some(paste),
        require_ack: Some(require_ack),
        wait_idle: None,
        wait_reply: None,
        timeout_ms: None,
    })
    .expect("send message");
    match response {
        Response::Sent {
            message_id, status, ..
        } => (message_id, status),
        other => panic!("expected a sent response, got {other:?}"),
    }
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

/// A message sent with `require_ack` stays `awaiting_ack` until the receiving
/// integration explicitly acknowledges it, then becomes `acknowledged`.
#[test]
fn require_ack_waits_for_an_explicit_acknowledgement() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("dev");

    let (id, status) = send_requiring_ack("dev", "run the suite");
    assert_eq!(status, DeliveryStatus::AwaitingAck);
    assert!(
        pending("dev").contains(&id),
        "an unacknowledged message stays in flight"
    );
    let record = message_record(id);
    assert_eq!(record.status, DeliveryStatus::AwaitingAck);
    assert!(record.ack.is_none());
    assert!(record.bytes > 0, "the bytes were written");

    let at = record.at;
    match ack("dev", id, Some("ran the suite")) {
        Response::Message { message } => {
            assert_eq!(message.status, DeliveryStatus::Acknowledged);
            let acked = message.ack.expect("ack recorded");
            assert_eq!(acked.note.as_deref(), Some("ran the suite"));
            assert!(acked.at >= at);
        }
        other => panic!("expected the acknowledged record, got {other:?}"),
    }

    let record = message_record(id);
    assert_eq!(record.status, DeliveryStatus::Acknowledged);
    assert!(!pending("dev").contains(&id), "an ack settles the message");

    // Retrying is idempotent and keeps the first note.
    match ack("dev", id, Some("second note")) {
        Response::Message { message } => {
            assert_eq!(message.status, DeliveryStatus::Acknowledged);
            assert_eq!(
                message.ack.as_ref().and_then(|ack| ack.note.as_deref()),
                Some("ran the suite")
            );
        }
        other => panic!("expected the stored record, got {other:?}"),
    }
    kill("dev");
}

/// `written` is not `acknowledged`, but an integration may ack a message the
/// sender did not mark as requiring it.
#[test]
fn a_plain_message_can_still_be_acknowledged() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("dev");

    let (id, status) = send("dev", "hello".to_string(), true);
    assert_eq!(status, DeliveryStatus::Written);
    assert!(message_record(id).ack.is_none());

    match ack("dev", id, None) {
        Response::Message { message } => {
            assert_eq!(message.status, DeliveryStatus::Acknowledged);
            assert!(message.ack.is_some());
        }
        other => panic!("expected the acknowledged record, got {other:?}"),
    }
    kill("dev");
}

/// Only the target may acknowledge, and an id must exist.
#[test]
fn ack_rejects_other_targets_and_unknown_ids() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("dev");
    create_agent("qa");

    let (id, _) = send_requiring_ack("dev", "hello");

    match ack("qa", id, None) {
        Response::Error { code, message } => {
            assert_eq!(code, ErrorCode::NotFound);
            assert!(message.contains("not found"), "got {message}");
        }
        other => panic!("expected a not-found error, got {other:?}"),
    }
    match ack("dev", id + 10_000, None) {
        Response::Error { code, .. } => assert_eq!(code, ErrorCode::NotFound),
        other => panic!("expected a not-found error, got {other:?}"),
    }
    assert_eq!(
        message_record(id).status,
        DeliveryStatus::AwaitingAck,
        "a rejected ack must not change the record"
    );
    kill("dev");
    kill("qa");
}

/// The note is part of the bounded history, so an oversized one is rejected
/// before it is stored.
#[test]
fn ack_rejects_an_oversized_note() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("dev");
    let (id, _) = send_requiring_ack("dev", "hello");

    let huge = "x".repeat(1025);
    match ack("dev", id, Some(&huge)) {
        Response::Error { code, message } => {
            assert_eq!(code, ErrorCode::BadRequest);
            assert!(message.contains("ack note"), "got {message}");
        }
        other => panic!("expected a bad-request error, got {other:?}"),
    }
    assert!(message_record(id).ack.is_none());

    // The boundary itself is accepted, and a retry with an oversized note now
    // returns the stored record instead of re-validating it.
    let largest = "y".repeat(1024);
    match ack("dev", id, Some(&largest)) {
        Response::Message { message } => {
            assert_eq!(
                message.ack.as_ref().and_then(|ack| ack.note.as_deref()),
                Some(largest.as_str())
            );
        }
        other => panic!("expected the acknowledged record, got {other:?}"),
    }
    match ack("dev", id, Some(&huge)) {
        Response::Message { message } => assert_eq!(
            message.ack.as_ref().and_then(|ack| ack.note.as_deref()),
            Some(largest.as_str()),
            "a retry must keep the first note"
        ),
        other => panic!("expected the stored record, got {other:?}"),
    }
    kill("dev");
}

/// A message that never reached the PTY cannot be acknowledged.
#[test]
fn ack_rejects_a_message_that_was_never_written() {
    let _harness = Harness::start(1, 1024);
    create_agent("tiny");

    let (id, status) = send_requiring_ack("tiny", &"x".repeat(2048));
    assert!(matches!(
        status,
        DeliveryStatus::Failed {
            code: ErrorCode::QueueFull,
            ..
        }
    ));

    match ack("tiny", id, None) {
        Response::Error { code, message } => {
            assert_eq!(code, ErrorCode::Conflict);
            assert!(message.contains("never written"), "got {message}");
        }
        other => panic!("expected a conflict error, got {other:?}"),
    }
    kill("tiny");
}

/// Readiness follows output quiescence: a recent echo is busy, and the same
/// agent becomes idle once the configured quiet period passes.
#[test]
fn readiness_turns_busy_then_idle_after_the_quiet_period() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);

    std::env::set_var("AGENT_BRIDGE_QUIET_MS", "10000");
    create_agent("busy");
    let before = agent_info("busy").last_output_at;
    let (_, status) = send("busy", "echo readiness-busy".to_string(), true);
    assert_eq!(status, DeliveryStatus::Written);
    let deadline = Instant::now() + Duration::from_secs(5);
    while agent_info("busy").last_output_at <= before {
        assert!(Instant::now() < deadline, "the echo produced no output");
        thread::sleep(Duration::from_millis(50));
    }
    wait_until_readiness("busy", Readiness::Busy, Duration::from_secs(5));
    kill("busy");

    std::env::set_var("AGENT_BRIDGE_QUIET_MS", "100");
    create_agent("idle");
    let (_, status) = send("idle", "echo readiness-idle".to_string(), true);
    assert_eq!(status, DeliveryStatus::Written);
    wait_until_readiness("idle", Readiness::Idle, Duration::from_secs(5));
    kill("idle");
}

/// Without output there is nothing to infer from, and a stopped agent is
/// always `unknown`.
#[test]
fn readiness_is_unknown_without_output_and_after_exit() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);

    std::env::set_var("AGENT_BRIDGE_QUIET_MS", "100");
    create_agent_with("silent", silent_command());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let info = agent_info("silent");
        if info.readiness == Readiness::Unknown {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a silent agent must stay unknown, got {:?}",
            info.readiness
        );
        thread::sleep(Duration::from_millis(50));
    }

    kill("silent");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let info = agent_info("silent");
        if info.status == AgentStatus::Exited {
            assert_eq!(info.readiness, Readiness::Unknown);
            break;
        }
        assert!(Instant::now() < deadline, "the silent agent did not exit");
        thread::sleep(Duration::from_millis(50));
    }
}

/// A configured prompt suffix marks idle even while the quiet period is still
/// running; the same output without the suffix stays busy.
#[test]
fn readiness_uses_a_configured_prompt_suffix() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);

    std::env::set_var("AGENT_BRIDGE_QUIET_MS", "10000");
    create_agent_with("plain", prompt_command());
    wait_until_readiness("plain", Readiness::Busy, Duration::from_secs(5));

    std::env::set_var("AGENT_BRIDGE_PROMPT_PATTERN", "AGENT-READY");
    create_agent_with("prompt", prompt_command());
    wait_until_readiness("prompt", Readiness::Idle, Duration::from_secs(5));

    kill("plain");
    kill("prompt");
}

/// Send with the Phase 3B wait options and return the raw response.
fn send_waiting(
    target: &str,
    message: String,
    wait_idle: bool,
    wait_reply: bool,
    timeout_ms: u64,
) -> Response {
    client::request(&Request::Send {
        target: target.to_string(),
        message,
        enter: Some(true),
        from: Some("qa".to_string()),
        queue_timeout_ms: Some(5_000),
        write_timeout_ms: Some(10_000),
        wait: Some(true),
        paste: Some(PasteMode::Auto),
        require_ack: None,
        wait_idle: Some(wait_idle),
        wait_reply: Some(wait_reply),
        timeout_ms: Some(timeout_ms),
    })
    .expect("send message")
}

/// Poll the retained output until it contains `needle`.
fn wait_for_output(name: &str, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if output_text(name).contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name} never printed {needle:?} (output: {:?})",
            output_text(name)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// `--wait-idle` holds the write until output has been quiet for the configured
/// period, then writes normally.
#[test]
fn wait_idle_holds_the_message_until_the_target_is_quiet() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    std::env::set_var("AGENT_BRIDGE_QUIET_MS", "1500");
    create_agent("held");

    let (_, status) = send("held", "echo warm-up".to_string(), true);
    assert_eq!(status, DeliveryStatus::Written);
    wait_until_readiness("held", Readiness::Busy, Duration::from_secs(10));

    let started = Instant::now();
    let response = send_waiting("held", "echo after-idle".to_string(), true, false, 15_000);
    let elapsed = started.elapsed();
    match response {
        Response::Sent { status, .. } => assert_eq!(status, DeliveryStatus::Written),
        other => panic!("expected a sent response, got {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(900),
        "wait-idle returned after {elapsed:?}, before the quiet period elapsed"
    );
    wait_for_output("held", "after-idle", Duration::from_secs(5));
    kill("held");
}

/// A target that never becomes quiet fails `--wait-idle` with `timeout`, and
/// nothing is written or recorded.
#[test]
fn wait_idle_times_out_without_writing() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    std::env::set_var("AGENT_BRIDGE_QUIET_MS", "10000");
    create_agent("stuck");

    let (_, status) = send("stuck", "echo warm-up".to_string(), true);
    assert_eq!(status, DeliveryStatus::Written);
    wait_until_readiness("stuck", Readiness::Busy, Duration::from_secs(10));
    let before = agent_info("stuck").message_count;

    let response = send_waiting("stuck", "echo must-not-land".to_string(), true, false, 300);
    match response {
        Response::Error { code, .. } => assert_eq!(code, ErrorCode::Timeout),
        other => panic!("expected a timeout response, got {other:?}"),
    }
    assert_eq!(
        agent_info("stuck").message_count,
        before,
        "a timed-out wait-idle must not record a message"
    );
    assert!(
        !output_text("stuck").contains("must-not-land"),
        "a timed-out wait-idle must not write the payload"
    );
    kill("stuck");
}

/// `--wait-reply` returns the output that settled after the write.
#[test]
fn wait_reply_returns_settled_output() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("reply");

    let response = send_waiting(
        "reply",
        "echo reply-out-4242".to_string(),
        false,
        true,
        15_000,
    );
    match response {
        Response::Sent { status, reply, .. } => {
            assert_eq!(status, DeliveryStatus::Written);
            let reply = reply.expect("wait-reply must attach a reply");
            assert!(reply.settled, "expected settled output, got {reply:?}");
            assert!(!reply.timed_out, "expected no timeout, got {reply:?}");
            assert!(!reply.truncated, "the small reply must not be truncated");
            assert!(
                reply
                    .lines
                    .iter()
                    .any(|line| line.contains("reply-out-4242")),
                "reply did not contain the marker: {reply:?}"
            );
        }
        other => panic!("expected a sent response, got {other:?}"),
    }
    kill("reply");
}

/// A silent target yields an explicit `timed_out` reply instead of hanging.
#[test]
fn wait_reply_times_out_on_a_silent_target() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent_with("mute", silent_command());

    // `0` is the documented "check once" deadline, so this is deterministic
    // even on a PTY that echoes typed input.
    let response = send_waiting("mute", "hello".to_string(), false, true, 0);
    match response {
        Response::Sent { status, reply, .. } => {
            assert_eq!(status, DeliveryStatus::Written);
            let reply = reply.expect("wait-reply must attach a reply");
            assert!(reply.timed_out, "expected a timeout, got {reply:?}");
            assert!(!reply.settled, "a timeout is not settled: {reply:?}");
        }
        other => panic!("expected a sent response, got {other:?}"),
    }
    kill("mute");
}

/// Reads one line from the PTY, prints a marker and exits immediately.
fn echo_then_exit_command() -> Vec<String> {
    if cfg!(windows) {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![
            shell,
            "/v:on".to_string(),
            "/c".to_string(),
            "set /p x= & echo GOT-!x!".to_string(),
        ]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "read x; echo GOT-$x".to_string(),
        ]
    }
}

/// The PTY pump can publish the target's last line at the same moment the child
/// exits. `--wait-reply` must drain that output before settling, otherwise the
/// caller sees `settled` with only the terminal echo.
#[test]
fn wait_reply_drains_output_from_a_target_that_exits() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    // Force the exit path: with a long quiet period the reply cannot settle on
    // quiescence, so only draining the reader's last frames can return the
    // marker. Repeat to cover the publish/close interleaving. Safe to mutate:
    // the harness holds the process-wide `SERIAL` guard for this test.
    std::env::set_var("AGENT_BRIDGE_QUIET_MS", "10000");
    for round in 0..8 {
        let name = format!("one-shot-{round}");
        create_agent_with(&name, echo_then_exit_command());
        let marker = format!("final-{round}");
        let response = send_waiting(&name, marker.clone(), false, true, 15_000);
        match response {
            Response::Sent { status, reply, .. } => {
                assert_eq!(status, DeliveryStatus::Written);
                let reply = reply.expect("wait-reply must attach a reply");
                assert!(
                    reply.settled,
                    "round {round}: expected settled, got {reply:?}"
                );
                assert!(!reply.timed_out, "round {round}: unexpected timeout");
                let text = reply.lines.join("\n");
                assert!(
                    text.contains(&format!("GOT-{marker}")),
                    "round {round}: the final marker was dropped: {reply:?}"
                );
            }
            other => panic!("round {round}: expected a sent response, got {other:?}"),
        }
    }
}

/// `unknown` readiness counts as ready, so a fresh silent target is written to
/// immediately instead of blocking until the deadline.
#[test]
fn wait_idle_proceeds_on_an_unknown_target() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent_with("quiet", silent_command());
    assert_eq!(agent_info("quiet").readiness, Readiness::Unknown);

    let started = Instant::now();
    let response = send_waiting("quiet", "hello".to_string(), true, false, 10_000);
    let elapsed = started.elapsed();
    match response {
        Response::Sent { status, .. } => assert_eq!(status, DeliveryStatus::Written),
        other => panic!("expected a sent response, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(3),
        "an unknown target must not wait for the deadline, took {elapsed:?}"
    );
    kill("quiet");
}

/// `wait_reply` needs a completed write; `wait: false` is a conflict and
/// nothing is recorded.
#[test]
fn wait_reply_requires_a_completed_write() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent("no-wait");

    let response = client::request(&Request::Send {
        target: "no-wait".to_string(),
        message: "echo nope".to_string(),
        enter: Some(true),
        from: None,
        queue_timeout_ms: None,
        write_timeout_ms: None,
        wait: Some(false),
        paste: None,
        require_ack: None,
        wait_idle: None,
        wait_reply: Some(true),
        timeout_ms: None,
    })
    .expect("send");
    match response {
        Response::Error { code, .. } => assert_eq!(code, ErrorCode::Conflict),
        other => panic!("expected a conflict response, got {other:?}"),
    }
    assert_eq!(agent_info("no-wait").message_count, 0);
    kill("no-wait");
}

/// Request one cursor chunk from an absolute byte offset.
fn read_chunk(target: &str, since: u64) -> Response {
    client::request(&Request::Read {
        target: target.to_string(),
        lines: None,
        since: Some(since),
    })
    .expect("cursor read")
}

/// Decode a `Chunk` response into `(bytes, offset, next_offset, dropped)`.
fn chunk_parts(response: Response) -> (Vec<u8>, u64, u64, bool) {
    use base64::Engine as _;
    match response {
        Response::Chunk {
            data,
            offset,
            next_offset,
            dropped,
            ..
        } => {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .expect("base64 chunk");
            (bytes, offset, next_offset, dropped)
        }
        other => panic!("expected a chunk response, got {other:?}"),
    }
}

/// A line read reports the absolute offset just past the newest byte.
fn line_read_next_offset(target: &str) -> u64 {
    match client::request(&Request::Read {
        target: target.to_string(),
        lines: Some(20),
        since: None,
    })
    .expect("read")
    {
        Response::Output { next_offset, .. } => next_offset,
        other => panic!("expected an output response, got {other:?}"),
    }
}

/// Wait until the agent has produced at least `min_bytes`.
fn wait_for_offset(target: &str, min_bytes: u64) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while line_read_next_offset(target) < min_bytes {
        assert!(
            Instant::now() < deadline,
            "agent never produced {min_bytes} bytes"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn output_lines_command(count: usize) -> Vec<String> {
    if cfg!(windows) {
        let shell = std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![
            shell,
            "/c".to_string(),
            format!(
                "for /L %i in (1,1,{count}) do @echo cursor-line-%i-abcdefghijklmnopqrstuvwxyz"
            ),
        ]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            // The suffix keeps the Windows and Unix producers byte-comparable:
            // the cursor tests assert on `cursor-line-N-`, so both platforms
            // must emit the dash plus the trailing alphabet.
            format!("seq 1 {count} | sed 's/^/cursor-line-/;s/$/-abcdefghijklmnopqrstuvwxyz/'"),
        ]
    }
}

fn large_output_command() -> Vec<String> {
    output_lines_command(4000)
}

/// Wait until the agent process exits and its reader has published the tail.
fn wait_for_exit(target: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while agent_info(target).status != AgentStatus::Exited {
        assert!(Instant::now() < deadline, "agent never exited");
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(300));
}

/// Resuming from `next_offset` must return every byte exactly once, even when
/// the stream is larger than one 64 KiB chunk and is still growing.
#[test]
fn cursor_reads_return_each_byte_once() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent_with("stream", large_output_command());

    let mut cursor = 0_u64;
    let mut collected: Vec<u8> = Vec::new();
    let mut quiet_since: Option<Instant> = None;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (bytes, offset, next_offset, dropped) = chunk_parts(read_chunk("stream", cursor));
        assert!(!dropped, "the whole stream fits in the default buffer");
        assert_eq!(offset, cursor, "a chunk started at the wrong offset");
        assert!(bytes.len() <= 64 * 1024, "chunk exceeded the 64 KiB cap");
        cursor = next_offset;
        collected.extend_from_slice(&bytes);

        if bytes.is_empty() {
            let quiet = *quiet_since.get_or_insert_with(Instant::now);
            // The producer exits, so once it is gone a short quiet period means
            // the reader has published its tail. The 5 s fallback keeps a slow
            // CI runner from ending the loop while output is still arriving.
            let exited = agent_info("stream").status == AgentStatus::Exited;
            if (exited && quiet.elapsed() >= Duration::from_millis(200))
                || quiet.elapsed() >= Duration::from_secs(5)
            {
                break;
            }
        } else {
            quiet_since = None;
        }
        assert!(
            Instant::now() < deadline,
            "stream never settled ({} bytes)",
            collected.len()
        );
        thread::sleep(Duration::from_millis(25));
    }

    assert!(
        collected.len() as u64 > 64 * 1024,
        "the test needs more than one chunk, got {} bytes",
        collected.len()
    );
    assert_eq!(
        collected.len() as u64,
        cursor,
        "offsets and bytes disagree, so a span was skipped or repeated"
    );
    let text = String::from_utf8_lossy(&collected);
    assert!(text.contains("cursor-line-1-"), "first line missing");
    assert!(text.contains("cursor-line-4000-"), "last line missing");
    assert_eq!(
        text.matches("cursor-line-2-").count(),
        1,
        "a byte span was returned twice"
    );

    kill("stream");
}

/// A cursor older than the retained window is clamped to the oldest byte and
/// flagged, and the next read from `next_offset` has no gap.
#[test]
fn cursor_older_than_the_buffer_reports_a_gap() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    // Sampled when the agent spawns, so set it before `create_agent_with`.
    std::env::set_var("AGENT_BRIDGE_BUFFER_BYTES", "1024");
    create_agent_with("wrapped", output_lines_command(200));
    // A finite producer: once it exits the retained window stops moving, so a
    // cursor taken from the first read cannot rotate out before the second.
    wait_for_exit("wrapped");
    wait_for_offset("wrapped", 1024);

    let total = line_read_next_offset("wrapped");
    assert!(total >= 1024);
    let (bytes, offset, next_offset, dropped) = chunk_parts(read_chunk("wrapped", 0));
    assert!(dropped, "offset 0 should have rotated out");
    assert_eq!(offset, total - 1024, "the chunk starts at the oldest byte");
    assert_eq!(next_offset, total);
    assert_eq!(bytes.len(), 1024, "the whole retained window is returned");

    let (more, offset, after, dropped) = chunk_parts(read_chunk("wrapped", next_offset));
    assert!(!dropped, "continuing from the newest offset has no gap");
    assert_eq!(offset, next_offset);
    assert_eq!(after, offset + more.len() as u64);

    kill("wrapped");
}

/// A cursor past the newest byte is rejected, while `since == total` is a
/// valid empty chunk.
#[test]
fn cursor_ahead_of_the_stream_is_rejected() {
    let _harness = Harness::start(DEFAULT_QUEUE_LIMIT, DEFAULT_QUEUE_BYTES);
    create_agent_with("quiet-cursor", silent_command());
    thread::sleep(Duration::from_millis(300));

    let total = line_read_next_offset("quiet-cursor");
    match read_chunk("quiet-cursor", total + 1) {
        Response::Error { code, message } => {
            assert_eq!(code, ErrorCode::BadRequest);
            assert!(message.contains("ahead"), "unexpected message: {message}");
        }
        other => panic!("expected an error response, got {other:?}"),
    }

    let (bytes, offset, next_offset, dropped) = chunk_parts(read_chunk("quiet-cursor", total));
    assert_eq!(bytes.len(), 0);
    assert_eq!(offset, total);
    assert_eq!(next_offset, total);
    assert!(!dropped);

    kill("quiet-cursor");
}
