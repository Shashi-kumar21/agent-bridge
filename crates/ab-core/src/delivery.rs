//! Delivery bookkeeping shared by the daemon and the CLI.
//!
//! This module owns the daemon-wide message id counter, the message size limit,
//! and the bounded per-agent write queue. Every agent has one writer thread that
//! performs one `write_all` + `flush` per job, so a slow PTY never blocks a
//! connection thread and two senders cannot interleave bytes inside a payload.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, RecvTimeoutError, SendTimeoutError, Sender, TryRecvError};

use crate::protocol::{DeliveryStatus, ErrorCode, MessageId, PasteMode};

/// Default maximum payload per `sendmessage` (64 KiB).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Hard cap for `AGENT_BRIDGE_MAX_MESSAGE`.
pub const MAX_MESSAGE_BYTES_CAP: usize = 1024 * 1024;

/// Environment variable that raises or lowers the message limit.
pub const MAX_MESSAGE_ENV: &str = "AGENT_BRIDGE_MAX_MESSAGE";

/// Default number of jobs that may wait for one agent's writer.
pub const DEFAULT_QUEUE_LIMIT: usize = 128;

/// Hard cap for `AGENT_BRIDGE_QUEUE_LIMIT`.
pub const QUEUE_LIMIT_CAP: usize = 4096;

/// Environment variable that changes the queue length.
pub const QUEUE_LIMIT_ENV: &str = "AGENT_BRIDGE_QUEUE_LIMIT";

/// Default total payload bytes that may wait for one agent's writer (2 MiB, so
/// the largest allowed single message fits).
pub const DEFAULT_QUEUE_BYTES: usize = 2 * 1024 * 1024;

/// Hard cap for `AGENT_BRIDGE_QUEUE_BYTES`.
pub const QUEUE_BYTES_CAP: usize = 64 * 1024 * 1024;

/// Environment variable that changes the queue byte budget.
pub const QUEUE_BYTES_ENV: &str = "AGENT_BRIDGE_QUEUE_BYTES";

/// Capacity of the urgent channel (Ctrl-C, attach keystrokes).
pub const URGENT_CAPACITY: usize = 8;

/// Default time a caller waits for a free queue slot.
pub const DEFAULT_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default time a caller waits for one write to finish.
pub const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default deadline for `wait_idle` and `wait_reply`.
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound for a caller-supplied timeout.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(3600);

/// How often the writer wakes to notice an exited agent or a dropped queue.
const WRITER_POLL: Duration = Duration::from_millis(100);

/// Daemon-wide monotonic source of [`MessageId`] values.
static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate the next message id. Unique for the daemon lifetime.
#[must_use]
pub fn next_message_id() -> MessageId {
    NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Current message size limit in bytes.
///
/// Reads [`MAX_MESSAGE_ENV`] once per call and clamps it to
/// `1 KiB ..= 1 MiB`; values that do not parse fall back to the default.
#[must_use]
pub fn max_message_bytes() -> usize {
    parse_max_message_bytes(std::env::var(MAX_MESSAGE_ENV).ok().as_deref())
}

fn parse_max_message_bytes(raw: Option<&str>) -> usize {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(DEFAULT_MAX_MESSAGE_BYTES, |value| {
            value.clamp(1024, MAX_MESSAGE_BYTES_CAP)
        })
}

/// Convert a wire timeout into a duration.
///
/// `None` keeps `default`; `Some(0)` means "fail fast" for admission and
/// "wait forever" for a write. Values above [`MAX_TIMEOUT`] are clamped.
#[must_use]
pub fn timeout_from_millis(value: Option<u64>, default: Duration) -> Duration {
    value.map_or(default, |millis| {
        Duration::from_millis(millis).min(MAX_TIMEOUT)
    })
}

/// Bracketed-paste start marker.
pub const PASTE_START: &[u8] = b"\x1b[200~";

/// Bracketed-paste end marker.
pub const PASTE_END: &[u8] = b"\x1b[201~";

/// Compose the exact bytes written to the PTY for one message.
///
/// `Auto` wraps text that contains a line break only when the target
/// application announced bracketed paste (`target_supports_paste`); `Bracketed`
/// always wraps, `Plain` never does. The optional `\r` is appended after the
/// closing marker so Enter submits the pasted block instead of ending the paste.
#[must_use]
pub fn compose_payload(
    text: &str,
    enter: bool,
    paste: PasteMode,
    target_supports_paste: bool,
) -> Vec<u8> {
    let multiline = text.contains('\n') || text.contains('\r');
    let bracketed = match paste {
        PasteMode::Bracketed => true,
        PasteMode::Plain => false,
        PasteMode::Auto => multiline && target_supports_paste,
    };
    let mut payload =
        Vec::with_capacity(text.len() + usize::from(enter) + if bracketed { 12 } else { 0 });
    if bracketed {
        payload.extend_from_slice(PASTE_START);
    }
    payload.extend_from_slice(text.as_bytes());
    if bracketed {
        payload.extend_from_slice(PASTE_END);
    }
    if enter {
        payload.push(b'\r');
    }
    payload
}

/// The result of handing bytes to the PTY.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// `write_all` and `flush` returned.
    Written {
        /// Payload bytes, including an appended carriage return.
        bytes: usize,
    },
    /// The bytes were not accepted by the PTY.
    Failed {
        /// Machine readable cause.
        code: ErrorCode,
        /// Human readable detail.
        detail: String,
    },
}

impl WriteOutcome {
    /// Terminal delivery status for this outcome.
    #[must_use]
    pub fn status(&self) -> DeliveryStatus {
        match self {
            Self::Written { .. } => DeliveryStatus::Written,
            Self::Failed { code, detail } => DeliveryStatus::Failed {
                code: *code,
                detail: detail.clone(),
            },
        }
    }

    /// Bytes accepted by the PTY (0 when the write failed).
    #[must_use]
    pub fn bytes(&self) -> usize {
        match self {
            Self::Written { bytes } => *bytes,
            Self::Failed { .. } => 0,
        }
    }
}

/// Progress reported by the writer for one job.
#[derive(Debug, Clone)]
pub enum JobEvent {
    /// The writer is about to call `write_all`.
    Started,
    /// The job reached a terminal outcome.
    Done(WriteOutcome),
}

/// Observer invoked by the writer thread. It must not block on the writer.
pub type JobObserver = Box<dyn Fn(JobEvent) + Send>;

/// One unit of work for an agent's writer thread.
///
/// A job is written with a single `write_all` + `flush`, so jobs never split
/// each other's bytes. The writer reports back through [`Job::observer`] and
/// [`Job::ack`].
pub struct Job {
    /// Complete payload for one write.
    pub bytes: Vec<u8>,
    /// Optional status callback (message jobs).
    pub observer: Option<JobObserver>,
    /// Optional completion channel (callers that wait).
    pub ack: Option<Sender<WriteOutcome>>,
    /// Urgent jobs are selected before queued messages between writes and do
    /// not consume the message byte budget.
    pub urgent: bool,
}

/// Why a job could not be admitted to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionError {
    /// The queue or its byte budget stayed full until the deadline.
    Full,
    /// The agent exited or the queue was closed.
    Closed,
}

/// Queue bounds and timings, read once per agent at spawn.
#[derive(Debug, Clone, Copy)]
pub struct QueueConfig {
    /// Maximum queued jobs.
    pub max_jobs: usize,
    /// Maximum queued payload bytes.
    pub max_bytes: usize,
    /// Capacity of the urgent channel.
    pub urgent_capacity: usize,
}

impl QueueConfig {
    /// Read `AGENT_BRIDGE_QUEUE_LIMIT` and `AGENT_BRIDGE_QUEUE_BYTES`.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            max_jobs: parse_clamped(
                std::env::var(QUEUE_LIMIT_ENV).ok().as_deref(),
                DEFAULT_QUEUE_LIMIT,
                1,
                QUEUE_LIMIT_CAP,
            ),
            max_bytes: parse_clamped(
                std::env::var(QUEUE_BYTES_ENV).ok().as_deref(),
                DEFAULT_QUEUE_BYTES,
                1024,
                QUEUE_BYTES_CAP,
            ),
            urgent_capacity: URGENT_CAPACITY,
        }
    }
}

fn parse_clamped(raw: Option<&str>, default: usize, min: usize, max: usize) -> usize {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(default, |value| value.clamp(min, max))
}

struct BudgetState {
    bytes: usize,
    closed: bool,
}

/// Shared byte budget plus a wake-up for blocked admissions.
struct Budget {
    max_bytes: usize,
    state: Mutex<BudgetState>,
    wake: Condvar,
}

impl Budget {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            state: Mutex::new(BudgetState {
                bytes: 0,
                closed: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn reserve(&self, bytes: usize, deadline: Instant) -> Result<(), AdmissionError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if state.closed {
                return Err(AdmissionError::Closed);
            }
            if bytes > self.max_bytes {
                // Waiting cannot free enough budget for this payload.
                return Err(AdmissionError::Full);
            }
            if state.bytes.saturating_add(bytes) <= self.max_bytes {
                state.bytes += bytes;
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(AdmissionError::Full);
            }
            let (guard, waited) = self
                .wake
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = guard;
            if waited.timed_out()
                && state.bytes.saturating_add(bytes) > self.max_bytes
                && !state.closed
            {
                return Err(AdmissionError::Full);
            }
        }
    }

    fn release(&self, bytes: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.bytes = state.bytes.saturating_sub(bytes);
        drop(state);
        self.wake.notify_all();
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        drop(state);
        self.wake.notify_all();
    }
}

/// Bounded per-agent write queue with one writer thread.
pub struct WriteQueue {
    normal: Sender<Job>,
    urgent: Sender<Job>,
    budget: Arc<Budget>,
    closed: AtomicBool,
}

impl WriteQueue {
    /// Create the queue and start its writer thread.
    ///
    /// The thread owns `sink`; it finishes when the queue is dropped, the agent
    /// exits (`running` becomes false), or both channels disconnect.
    pub fn spawn<S>(
        sink: S,
        config: QueueConfig,
        running: Arc<AtomicBool>,
        agent: String,
    ) -> crate::Result<Arc<Self>>
    where
        S: Write + Send + 'static,
    {
        let (normal, normal_rx) = bounded(config.max_jobs.max(1));
        let (urgent, urgent_rx) = bounded(config.urgent_capacity.max(1));
        let budget = Arc::new(Budget::new(config.max_bytes));
        let queue = Arc::new(Self {
            normal,
            urgent,
            budget: Arc::clone(&budget),
            closed: AtomicBool::new(false),
        });
        let writer_budget = Arc::clone(&budget);
        thread::Builder::new()
            .name(format!("pty-write-{agent}"))
            .spawn(move || {
                writer_loop(sink, normal_rx, urgent_rx, &writer_budget, &running, &agent);
            })?;
        Ok(queue)
    }

    /// Admit a message job, waiting up to `timeout` for a slot.
    ///
    /// `timeout` of zero fails immediately when the queue is full.
    pub fn submit(&self, job: Job, timeout: Duration) -> Result<(), AdmissionError> {
        let deadline = Instant::now() + timeout.min(MAX_TIMEOUT);
        self.budget.reserve(job.bytes.len(), deadline)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.normal.send_timeout(job, remaining) {
            Ok(()) => Ok(()),
            Err(SendTimeoutError::Timeout(job)) => {
                self.budget.release(job.bytes.len());
                Err(AdmissionError::Full)
            }
            Err(SendTimeoutError::Disconnected(job)) => {
                self.budget.release(job.bytes.len());
                Err(AdmissionError::Closed)
            }
        }
    }

    /// Admit an urgent job (Ctrl-C or a keystroke). Urgent jobs are not counted
    /// against the message byte budget.
    pub fn submit_urgent(&self, job: Job, timeout: Duration) -> Result<(), AdmissionError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(AdmissionError::Closed);
        }
        match self.urgent.send_timeout(job, timeout.min(MAX_TIMEOUT)) {
            Ok(()) => Ok(()),
            Err(SendTimeoutError::Timeout(_)) => Err(AdmissionError::Full),
            Err(SendTimeoutError::Disconnected(_)) => Err(AdmissionError::Closed),
        }
    }

    /// Jobs waiting for the writer, not counting the write in flight.
    #[must_use]
    pub fn depth(&self) -> usize {
        self.normal.len() + self.urgent.len()
    }

    /// Stop admitting work and wake blocked callers. The writer drains what is
    /// already queued and marks it failed.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.budget.close();
    }
}

#[allow(clippy::needless_pass_by_value)]
fn writer_loop<S: Write>(
    mut sink: S,
    normal: crossbeam_channel::Receiver<Job>,
    urgent: crossbeam_channel::Receiver<Job>,
    budget: &Budget,
    running: &AtomicBool,
    agent: &str,
) {
    let mut urgent_gone = false;
    loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }

        let mut job = None;
        if !urgent_gone {
            match urgent.try_recv() {
                Ok(next) => job = Some(next),
                Err(TryRecvError::Disconnected) => urgent_gone = true,
                Err(TryRecvError::Empty) => {}
            }
        }
        if job.is_none() {
            if urgent_gone {
                match normal.recv_timeout(WRITER_POLL) {
                    Ok(next) => job = Some(next),
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                crossbeam_channel::select! {
                    recv(urgent) -> message => match message {
                        Ok(next) => job = Some(next),
                        Err(_) => urgent_gone = true,
                    },
                    recv(normal) -> message => match message {
                        Ok(next) => job = Some(next),
                        Err(_) => break,
                    },
                    default(WRITER_POLL) => continue,
                }
            }
        }

        let Some(job) = job else { continue };
        if !running.load(Ordering::SeqCst) {
            // Do not start a new write once the agent is gone.
            finish(&job, AgentExit::outcome(agent), budget);
            continue;
        }
        run_job(&mut sink, &job, budget);
    }

    // Fail whatever was queued when the agent exited or the queue dropped.
    while let Ok(job) = urgent.try_recv() {
        finish(&job, AgentExit::outcome(agent), budget);
    }
    while let Ok(job) = normal.try_recv() {
        finish(&job, AgentExit::outcome(agent), budget);
    }
}

fn run_job<S: Write>(sink: &mut S, job: &Job, budget: &Budget) {
    if let Some(observer) = &job.observer {
        observer(JobEvent::Started);
    }
    let outcome = match sink.write_all(&job.bytes).and_then(|()| sink.flush()) {
        Ok(()) => WriteOutcome::Written {
            bytes: job.bytes.len(),
        },
        Err(err) => WriteOutcome::Failed {
            code: ErrorCode::Internal,
            detail: format!("pty write failed: {err}"),
        },
    };
    finish(job, outcome, budget);
}

/// Release the byte budget and deliver the outcome to the observer and waiter.
fn finish(job: &Job, outcome: WriteOutcome, budget: &Budget) {
    if !job.urgent {
        budget.release(job.bytes.len());
    }
    if let Some(observer) = &job.observer {
        observer(JobEvent::Done(outcome.clone()));
    }
    if let Some(ack) = &job.ack {
        let _ = ack.send(outcome);
    }
}

struct AgentExit;

impl AgentExit {
    fn outcome(agent: &str) -> WriteOutcome {
        WriteOutcome::Failed {
            code: ErrorCode::Conflict,
            detail: format!("agent '{agent}' exited before the write"),
        }
    }
}

/// Map an admission failure to the status returned to the caller.
#[must_use]
pub fn admission_status(error: AdmissionError) -> DeliveryStatus {
    match error {
        AdmissionError::Full => DeliveryStatus::Failed {
            code: ErrorCode::QueueFull,
            detail: "write queue is full; raise AGENT_BRIDGE_QUEUE_LIMIT / \
                     AGENT_BRIDGE_QUEUE_BYTES or retry"
                .to_string(),
        },
        AdmissionError::Closed => DeliveryStatus::Failed {
            code: ErrorCode::Conflict,
            detail: "agent exited before the message was queued".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use super::{
        admission_status, compose_payload, max_message_bytes, next_message_id, parse_clamped,
        parse_max_message_bytes, timeout_from_millis, AdmissionError, Job, JobEvent, JobObserver,
        QueueConfig, WriteOutcome, WriteQueue, DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_QUEUE_TIMEOUT,
        MAX_MESSAGE_BYTES_CAP, MAX_TIMEOUT, PASTE_END, PASTE_START,
    };
    use crate::protocol::{DeliveryStatus, ErrorCode, PasteMode};

    #[test]
    fn ids_are_unique_and_monotonic() {
        let first = next_message_id();
        let second = next_message_id();
        assert!(second > first);
    }

    #[test]
    fn parse_clamps_and_falls_back() {
        assert_eq!(parse_max_message_bytes(None), DEFAULT_MAX_MESSAGE_BYTES);
        assert_eq!(
            parse_max_message_bytes(Some("nope")),
            DEFAULT_MAX_MESSAGE_BYTES
        );
        assert_eq!(parse_max_message_bytes(Some(" 2048 ")), 2048);
        assert_eq!(parse_max_message_bytes(Some("1")), 1024);
        assert_eq!(
            parse_max_message_bytes(Some("99999999")),
            MAX_MESSAGE_BYTES_CAP
        );
        assert_eq!(parse_clamped(None, 7, 1, 9), 7);
        assert_eq!(parse_clamped(Some("0"), 7, 1, 9), 1);
        assert_eq!(parse_clamped(Some("99"), 7, 1, 9), 9);
        assert_eq!(parse_clamped(Some("x"), 7, 1, 9), 7);
    }

    #[test]
    fn timeout_defaults_and_clamps() {
        assert_eq!(
            timeout_from_millis(None, DEFAULT_QUEUE_TIMEOUT),
            DEFAULT_QUEUE_TIMEOUT
        );
        assert_eq!(
            timeout_from_millis(Some(250), DEFAULT_QUEUE_TIMEOUT),
            Duration::from_millis(250)
        );
        assert_eq!(
            timeout_from_millis(Some(0), DEFAULT_QUEUE_TIMEOUT),
            Duration::ZERO
        );
        assert_eq!(
            timeout_from_millis(Some(u64::MAX), DEFAULT_QUEUE_TIMEOUT),
            MAX_TIMEOUT
        );
    }

    #[test]
    fn max_message_bytes_reads_the_environment() {
        // The env var is process-wide; keep the assertion scoped and restore it.
        let previous = std::env::var("AGENT_BRIDGE_MAX_MESSAGE").ok();
        std::env::set_var("AGENT_BRIDGE_MAX_MESSAGE", "2048");
        assert_eq!(max_message_bytes(), 2048);
        match previous {
            Some(value) => std::env::set_var("AGENT_BRIDGE_MAX_MESSAGE", value),
            None => std::env::remove_var("AGENT_BRIDGE_MAX_MESSAGE"),
        }
    }

    #[test]
    fn admission_errors_map_to_statuses() {
        assert!(matches!(
            admission_status(AdmissionError::Full),
            DeliveryStatus::Failed {
                code: ErrorCode::QueueFull,
                ..
            }
        ));
        assert!(matches!(
            admission_status(AdmissionError::Closed),
            DeliveryStatus::Failed {
                code: ErrorCode::Conflict,
                ..
            }
        ));
    }

    /// In-memory sink that can block or fail on demand.
    #[derive(Clone, Default)]
    struct MockSink {
        inner: std::sync::Arc<MockState>,
    }

    #[derive(Default)]
    struct MockState {
        block: std::sync::Mutex<bool>,
        fail: std::sync::Mutex<bool>,
        started: std::sync::atomic::AtomicUsize,
        written: std::sync::Mutex<Vec<u8>>,
        wake: std::sync::Condvar,
    }

    impl MockSink {
        fn block(&self, value: bool) {
            *self.inner.block.lock().expect("block") = value;
        }

        fn fail(&self, value: bool) {
            *self.inner.fail.lock().expect("fail") = value;
        }

        fn release(&self) {
            *self.inner.block.lock().expect("block") = false;
            self.inner.wake.notify_all();
        }

        fn written(&self) -> Vec<u8> {
            self.inner.written.lock().expect("written").clone()
        }

        fn wait_started(&self, deadline: Instant) -> bool {
            while Instant::now() < deadline {
                if self.inner.started.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            false
        }
    }

    impl io::Write for MockSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner
                .started
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            {
                let mut state = self.inner.block.lock().expect("block");
                while *state {
                    state = self.inner.wake.wait(state).expect("wake");
                }
            }
            if *self.inner.fail.lock().expect("fail") {
                return Err(io::Error::other("sink failure"));
            }
            self.inner
                .written
                .lock()
                .expect("written")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn job(bytes: &[u8], urgent: bool) -> (Job, crossbeam_channel::Receiver<WriteOutcome>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (
            Job {
                bytes: bytes.to_vec(),
                observer: None,
                ack: Some(tx),
                urgent,
            },
            rx,
        )
    }

    fn queue(
        sink: MockSink,
        max_jobs: usize,
        max_bytes: usize,
    ) -> (std::sync::Arc<WriteQueue>, std::sync::Arc<AtomicBool>) {
        let running = std::sync::Arc::new(AtomicBool::new(true));
        let queue = WriteQueue::spawn(
            sink,
            QueueConfig {
                max_jobs,
                max_bytes,
                urgent_capacity: 8,
            },
            std::sync::Arc::clone(&running),
            "test".to_string(),
        )
        .expect("queue");
        (queue, running)
    }

    #[test]
    fn writes_are_serialised_and_acked() {
        let sink = MockSink::default();
        let (queue, _running) = queue(sink.clone(), 8, 1024 * 1024);

        let (first, first_rx) = job(b"one\n", false);
        let (second, second_rx) = job(b"two\n", false);
        queue.submit(first, Duration::from_secs(1)).expect("first");
        queue
            .submit(second, Duration::from_secs(1))
            .expect("second");

        assert!(matches!(
            first_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { bytes: 4 })
        ));
        assert!(matches!(
            second_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { bytes: 4 })
        ));
        assert_eq!(sink.written(), b"one\ntwo\n");
    }

    #[test]
    fn urgent_jobs_preempt_queued_messages_between_writes() {
        let sink = MockSink::default();
        sink.block(true);
        let (queue, _running) = queue(sink.clone(), 8, 1024 * 1024);

        let (first, first_rx) = job(b"1", false);
        let (second, second_rx) = job(b"2", false);
        let (urgent, urgent_rx) = job(b"u", true);
        queue.submit(first, Duration::from_secs(1)).expect("first");
        assert!(
            sink.wait_started(Instant::now() + Duration::from_secs(2)),
            "the writer should start the first job"
        );
        queue
            .submit(second, Duration::from_secs(1))
            .expect("second");
        queue
            .submit_urgent(urgent, Duration::from_secs(1))
            .expect("urgent");
        sink.release();

        assert!(matches!(
            first_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
        assert!(matches!(
            urgent_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
        assert!(matches!(
            second_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
        assert_eq!(sink.written(), b"1u2");
    }

    #[test]
    fn full_queue_fails_fast_or_after_the_deadline() {
        let sink = MockSink::default();
        sink.block(true);
        let (queue, _running) = queue(sink.clone(), 1, 1024 * 1024);

        let (first, first_rx) = job(b"1", false);
        let (second, second_rx) = job(b"2", false);
        let (third, _third_rx) = job(b"3", false);
        queue.submit(first, Duration::from_secs(1)).expect("first");
        assert!(
            sink.wait_started(Instant::now() + Duration::from_secs(2)),
            "the writer should start the first job"
        );
        queue
            .submit(second, Duration::from_secs(1))
            .expect("second fills the channel");
        assert_eq!(queue.depth(), 1, "in-flight jobs are not queued");

        let started = Instant::now();
        assert_eq!(
            queue.submit(third, Duration::ZERO),
            Err(AdmissionError::Full)
        );
        assert!(started.elapsed() < Duration::from_millis(200));

        sink.release();
        assert!(matches!(
            first_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
        assert!(matches!(
            second_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
    }

    #[test]
    fn byte_budget_is_released_after_each_job() {
        let sink = MockSink::default();
        sink.block(true);
        let (queue, _running) = queue(sink.clone(), 8, 4);

        let (first, first_rx) = job(b"123", false);
        let (second, _second_rx) = job(b"456", false);
        queue.submit(first, Duration::from_secs(1)).expect("first");
        assert_eq!(
            queue.submit(second, Duration::ZERO),
            Err(AdmissionError::Full),
            "the byte budget must be enforced"
        );

        sink.release();
        assert!(matches!(
            first_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));

        let (third, third_rx) = job(b"789", false);
        queue
            .submit(third, Duration::from_secs(1))
            .expect("budget is released");
        assert!(matches!(
            third_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { bytes: 3 })
        ));
    }

    #[test]
    fn a_blocked_write_times_out_but_the_job_still_finishes() {
        let sink = MockSink::default();
        sink.block(true);
        let (queue, _running) = queue(sink.clone(), 8, 1024 * 1024);

        let (job, rx) = job(b"slow", false);
        queue.submit(job, Duration::from_secs(1)).expect("submit");

        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(100)),
                Err(crossbeam_channel::RecvTimeoutError::Timeout)
            ),
            "the caller must observe a write timeout"
        );

        sink.release();
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { bytes: 4 })
        ));
        assert_eq!(sink.written(), b"slow");
    }

    #[test]
    fn a_failed_write_reports_the_error() {
        let sink = MockSink::default();
        sink.fail(true);
        let (queue, _running) = queue(sink, 8, 1024 * 1024);

        let (job, rx) = job(b"nope", false);
        queue.submit(job, Duration::from_secs(1)).expect("submit");
        match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(WriteOutcome::Failed { code, detail }) => {
                assert_eq!(code, ErrorCode::Internal);
                assert!(detail.contains("sink failure"));
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn byte_budget_admission_waits_for_its_deadline() {
        let sink = MockSink::default();
        sink.block(true);
        // Eight channel slots, but room for only one six-byte payload.
        let (queue, _running) = queue(sink.clone(), 8, 8);

        let (first, first_rx) = job(b"aaaaaa", false);
        queue.submit(first, Duration::from_secs(1)).expect("first");
        assert!(
            sink.wait_started(Instant::now() + Duration::from_secs(2)),
            "the writer should start the first job"
        );

        let started = Instant::now();
        let (second, _rejected) = job(b"bbbbbb", false);
        assert_eq!(
            queue.submit(second, Duration::from_millis(150)),
            Err(AdmissionError::Full),
            "the byte budget must hold the second payload back"
        );
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(100),
            "admission should wait for the deadline, waited {waited:?}"
        );

        // Once the blocked write finishes its bytes are released and the same
        // payload is admitted.
        sink.release();
        assert!(matches!(
            first_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
        let (second, second_rx) = job(b"bbbbbb", false);
        queue
            .submit(second, Duration::from_secs(1))
            .expect("budget released");
        assert!(matches!(
            second_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
    }

    #[test]
    fn queued_jobs_fail_when_the_agent_exits() {
        let sink = MockSink::default();
        sink.block(true);
        let (queue, running) = queue(sink.clone(), 8, 1024 * 1024);

        let (first, first_rx) = job(b"1", false);
        let (second, second_rx) = job(b"2", false);
        queue.submit(first, Duration::from_secs(1)).expect("first");
        assert!(
            sink.wait_started(Instant::now() + Duration::from_secs(2)),
            "the writer should start the first job"
        );
        queue
            .submit(second, Duration::from_secs(1))
            .expect("second");

        running.store(false, std::sync::atomic::Ordering::SeqCst);
        queue.close();
        sink.release();

        assert!(matches!(
            first_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
        match second_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(WriteOutcome::Failed { code, detail }) => {
                assert_eq!(code, ErrorCode::Conflict);
                assert!(detail.contains("exited"));
            }
            other => panic!("the queued job must fail: {other:?}"),
        }
    }

    #[test]
    fn observer_sees_started_then_done() {
        let sink = MockSink::default();
        let (queue, _running) = queue(sink, 8, 1024 * 1024);

        let events: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&events);
        let observer: JobObserver = Box::new(move |event| {
            let label = match event {
                JobEvent::Started => "started",
                JobEvent::Done(_) => "done",
            };
            seen.lock().expect("events").push(label);
        });
        let (tx, rx) = crossbeam_channel::unbounded();
        let job = Job {
            bytes: b"x".to_vec(),
            observer: Some(observer),
            ack: Some(tx),
            urgent: false,
        };
        queue.submit(job, Duration::from_secs(1)).expect("submit");
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { .. })
        ));
        assert_eq!(&*events.lock().expect("events"), &["started", "done"]);
    }

    #[test]
    fn unicode_and_large_payloads_are_written_whole() {
        let sink = MockSink::default();
        let (queue, _running) = queue(sink.clone(), 8, 1024 * 1024);

        let text = "नमस्ते 🚀\n";
        let (unicode, unicode_rx) = job(text.as_bytes(), false);
        queue
            .submit(unicode, Duration::from_secs(1))
            .expect("unicode");
        assert!(matches!(
            unicode_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { bytes }) if bytes == text.len()
        ));

        let large = vec![b'z'; 48 * 1024];
        let (large_job, large_rx) = job(&large, false);
        queue
            .submit(large_job, Duration::from_secs(1))
            .expect("large");
        assert!(matches!(
            large_rx.recv_timeout(Duration::from_secs(2)),
            Ok(WriteOutcome::Written { bytes }) if bytes == large.len()
        ));

        let mut expected = text.as_bytes().to_vec();
        expected.extend_from_slice(&large);
        assert_eq!(sink.written(), expected);
    }

    #[test]
    fn compose_payload_keeps_single_line_messages_plain() {
        assert_eq!(
            compose_payload("echo hi", true, PasteMode::Auto, true),
            b"echo hi\r"
        );
        assert_eq!(
            compose_payload("echo hi", false, PasteMode::Bracketed, false),
            [PASTE_START, b"echo hi".as_slice(), PASTE_END].concat()
        );
    }

    #[test]
    fn compose_payload_wraps_multi_line_only_when_supported() {
        let text = "one\ntwo\nthree";
        assert_eq!(
            compose_payload(text, true, PasteMode::Auto, true),
            [PASTE_START, text.as_bytes(), PASTE_END, b"\r".as_slice()].concat(),
            "a supporting target sees one paste plus one Enter"
        );
        assert_eq!(
            compose_payload(text, true, PasteMode::Auto, false),
            b"one\ntwo\nthree\r",
            "without an announcement the fallback is plain bytes"
        );
    }

    #[test]
    fn compose_payload_honours_explicit_modes() {
        let text = "one\ntwo";
        assert_eq!(
            compose_payload(text, false, PasteMode::Bracketed, false),
            [PASTE_START, text.as_bytes(), PASTE_END].concat(),
            "bracketed is forced even when the target did not announce it"
        );
        assert_eq!(
            compose_payload(text, false, PasteMode::Plain, true),
            text.as_bytes(),
            "plain never wraps"
        );
        assert_eq!(compose_payload("x", true, PasteMode::Plain, true), b"x\r");
    }
}
