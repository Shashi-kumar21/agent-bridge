//! One PTY-backed agent session owned by the daemon.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use crossbeam_channel::{bounded, Receiver, Sender};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tracing::{debug, warn};

use crate::buffer::OutputBuffer;
use crate::delivery::{self, Job, WriteOutcome, WriteQueue};
use crate::error::{BridgeError, Result};
use crate::protocol::{
    AgentInfo, AgentStatus, DeliveryStatus, Event, Message, MessageId, Millis, Readiness,
};
use crate::readiness::{ReadinessConfig, PROMPT_SCAN_BYTES};
use crate::reaper::{self, ProcessReaper};

const MAX_MESSAGES: usize = 500;

/// How long an urgent write (Ctrl-C, attach keystroke) waits for the writer to
/// acknowledge before the caller moves on. The bytes are still queued.
const URGENT_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// How long an urgent write waits for a free urgent slot.
const URGENT_QUEUE_TIMEOUT: Duration = Duration::from_secs(2);

/// Per-subscriber event queue depth. A consumer that falls this far behind is
/// disconnected instead of growing daemon memory without bound.
pub const SUBSCRIBER_QUEUE: usize = 1024;

/// Tracks bracketed-paste support announced by the child application.
///
/// The application, not the terminal emulator, enables the mode by writing
/// `ESC[?2004h` to its output, and may toggle it per prompt (readline does), so
/// the last marker in the stream wins. A marker can straddle two reads, so a
/// few bytes are carried into the next chunk.
#[derive(Default)]
struct PasteDetector {
    carry: Vec<u8>,
}

impl PasteDetector {
    /// Feed one output chunk; `Some(enabled)` when the state changed.
    fn observe(&mut self, data: &[u8]) -> Option<bool> {
        const ON: &[u8] = b"\x1b[?2004h";
        const OFF: &[u8] = b"\x1b[?2004l";
        debug_assert_eq!(ON.len(), OFF.len());

        let mut joined = std::mem::take(&mut self.carry);
        joined.extend_from_slice(data);

        let mut last: Option<(usize, bool)> = None;
        for (index, window) in joined.windows(ON.len()).enumerate() {
            if window == ON {
                last = Some((index, true));
            } else if window == OFF {
                last = Some((index, false));
            }
        }

        let keep_from = joined.len().saturating_sub(ON.len() - 1);
        self.carry = joined[keep_from..].to_vec();
        last.map(|(_, enabled)| enabled)
    }
}

/// Process-wide monotonic suffix so two agents created in the same millisecond
/// still get distinct ids.
static AGENT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Parameters for spawning an agent.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Unique agent name.
    pub name: String,
    /// argv to execute.
    pub command: Vec<String>,
    /// Optional working directory.
    pub cwd: Option<String>,
    /// Initial PTY width.
    pub cols: u16,
    /// Initial PTY height.
    pub rows: u16,
}

/// A running (or exited) PTY session.
pub struct Agent {
    id: String,
    name: String,
    command: String,
    cwd: String,
    created_at: Millis,
    pid: Option<u32>,
    cols: AtomicU16,
    rows: AtomicU16,
    running: Arc<AtomicBool>,
    output_closed: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    exited_at: Mutex<Option<Millis>>,
    last_output_at: Mutex<Option<Millis>>,
    buffer: Mutex<OutputBuffer>,
    readiness: ReadinessConfig,
    messages: Mutex<VecDeque<Message>>,
    queue: Arc<WriteQueue>,
    /// Set when the child emits `ESC[?2004h` (bracketed paste enabled) and
    /// cleared by `ESC[?2004l`.
    bracketed_paste: AtomicBool,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    subscribers: Mutex<Vec<Sender<Event>>>,
    subscriber_count: AtomicUsize,
    seq: AtomicU64,
    /// Holds the OS-level "die with the daemon" link; taken when the agent tree
    /// is terminated on purpose.
    reaper: Mutex<Option<ProcessReaper>>,
}

impl Agent {
    /// Spawn `config.command` inside a fresh PTY and start pump threads.
    pub fn spawn(config: &SpawnConfig) -> Result<Arc<Self>> {
        let pty_system = native_pty_system();
        let size = PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system.openpty(size)?;

        let program = resolve_program(
            config
                .command
                .first()
                .ok_or_else(|| BridgeError::Daemon("empty command".to_string()))?,
        );
        let mut builder = CommandBuilder::new(program);
        if config.command.len() > 1 {
            builder.args(&config.command[1..]);
        }
        let cwd = config
            .cwd
            .clone()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.display().to_string())
            })
            .unwrap_or_else(|| ".".to_string());
        builder.cwd(&cwd);
        builder.env("TERM", "xterm-256color");
        builder.env("AGENT_BRIDGE_AGENT", &config.name);

        let child = pair.slave.spawn_command(builder)?;
        drop(pair.slave);
        let pid = child.process_id();
        // Must happen right after spawn: assign the agent to a job object so the
        // kernel kills it if the daemon dies without running its shutdown path.
        #[cfg(windows)]
        let process_reaper = reaper::install(child.as_ref());
        // The PTY slave is read from the child's stdin while it is still alive:
        // on Linux it is the only anchor that survives the client's exit.
        #[cfg(unix)]
        let process_reaper = reaper::install(child.as_ref(), reaper::pty_path(pid).as_deref());
        let master = pair.master;
        let writer = master.take_writer()?;
        let reader = master.try_clone_reader()?;
        let running = Arc::new(AtomicBool::new(true));
        let queue = WriteQueue::spawn(
            writer,
            delivery::QueueConfig::from_env(),
            Arc::clone(&running),
            config.name.clone(),
        )?;

        let agent = Arc::new(Self {
            id: format!(
                "{:x}-{:x}",
                crate::now_ms(),
                AGENT_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            name: config.name.clone(),
            command: render_command(&config.command),
            cwd,
            created_at: crate::now_ms(),
            pid,
            cols: AtomicU16::new(config.cols),
            rows: AtomicU16::new(config.rows),
            running,
            output_closed: AtomicBool::new(false),
            exit_code: Mutex::new(None),
            exited_at: Mutex::new(None),
            last_output_at: Mutex::new(None),
            buffer: Mutex::new(OutputBuffer::new(crate::buffer::capacity_from_env())),
            readiness: ReadinessConfig::from_env(),
            messages: Mutex::new(VecDeque::new()),
            queue,
            bracketed_paste: AtomicBool::new(false),
            master: Mutex::new(master),
            child: Mutex::new(child),
            subscribers: Mutex::new(Vec::new()),
            subscriber_count: AtomicUsize::new(0),
            seq: AtomicU64::new(0),
            reaper: Mutex::new(process_reaper),
        });

        let pump = Arc::clone(&agent);
        thread::Builder::new()
            .name(format!("pty-read-{}", config.name))
            .spawn(move || pump.pump_output(reader))?;

        let waiter = Arc::clone(&agent);
        thread::Builder::new()
            .name(format!("pty-wait-{}", config.name))
            .spawn(move || waiter.wait_child())?;

        Ok(agent)
    }

    fn pump_output(&self, mut reader: Box<dyn Read + Send>) {
        let mut chunk = [0_u8; 8192];
        let mut paste = PasteDetector::default();
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = &chunk[..n];
                    if let Some(enabled) = paste.observe(data) {
                        self.bracketed_paste.store(enabled, Ordering::SeqCst);
                    }
                    if let Ok(mut buffer) = self.buffer.lock() {
                        buffer.push(data);
                    }
                    *self
                        .last_output_at
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(crate::now_ms());
                    let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
                    self.publish(Event::Output {
                        agent: self.name.clone(),
                        seq,
                        data: base64::engine::general_purpose::STANDARD.encode(data),
                    });
                }
            }
        }
        self.output_closed.store(true, Ordering::SeqCst);
        debug!(agent = %self.name, "pty reader closed");
    }

    /// True once the PTY reader has stopped and published its last chunk.
    #[must_use]
    pub fn output_closed(&self) -> bool {
        self.output_closed.load(Ordering::SeqCst)
    }

    fn wait_child(&self) {
        loop {
            thread::sleep(Duration::from_millis(120));
            let exit = {
                let mut child = self
                    .child
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match child.try_wait() {
                    Ok(Some(status)) => Some(i32::try_from(status.exit_code()).unwrap_or(-1)),
                    Ok(None) => None,
                    Err(err) => {
                        // Only this daemon waits on the child, so an error means
                        // the handle is gone or the OS refused (ECHILD and
                        // friends). Treat it as an exit so the agent does not
                        // stay "running" forever; the reaper's pid guard is
                        // conservative for that case.
                        warn!(agent = %self.name, error = %err, "wait failed");
                        Some(-1)
                    }
                }
            };
            if let Some(code) = exit {
                if self.running.swap(false, Ordering::SeqCst) {
                    *self
                        .exit_code
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(code);
                    *self
                        .exited_at
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(crate::now_ms());
                    self.queue.close();
                    self.publish(Event::Exit {
                        agent: self.name.clone(),
                        code: Some(code),
                    });
                    debug!(agent = %self.name, code, "agent exited");
                }
                return;
            }
        }
    }

    /// Agent name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Current output sequence number.
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// Whether the process is still alive.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// True when the child application has bracketed paste enabled.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste.load(Ordering::SeqCst)
    }

    /// Heuristic readiness derived from output quiescence and an optional
    /// prompt suffix.
    ///
    /// Never a guarantee: a silent command that runs longer than the quiet
    /// period is reported `idle`, and a prompt that does not reprint cannot be
    /// detected without a configured suffix. Terminal control sequences alone
    /// are not output; such an agent stays `unknown`.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        let last_line = self
            .buffer
            .lock()
            .ok()
            .and_then(|buffer| buffer.last_line(PROMPT_SCAN_BYTES));
        let last_output_at = *self
            .last_output_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.readiness.classify(
            self.is_running(),
            last_output_at,
            last_line.as_deref(),
            crate::now_ms(),
        )
    }

    /// Snapshot for the wire.
    #[must_use]
    pub fn info(&self) -> AgentInfo {
        let message_count = self.messages.lock().map_or(0, |m| m.len());
        AgentInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            status: if self.is_running() {
                AgentStatus::Running
            } else {
                AgentStatus::Exited
            },
            readiness: self.readiness(),
            pid: self.pid,
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            created_at: self.created_at,
            last_output_at: *self
                .last_output_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            exited_at: *self
                .exited_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            exit_code: *self
                .exit_code
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            message_count,
            queued: self.queue.depth(),
            bracketed_paste: self.bracketed_paste(),
            subscribers: self.subscriber_count.load(Ordering::SeqCst),
            cols: self.cols.load(Ordering::SeqCst),
            rows: self.rows.load(Ordering::SeqCst),
        }
    }

    /// Queue one message payload for the agent's writer.
    ///
    /// Returns once the job is admitted, or an admission error when the queue
    /// stayed full until `timeout`. `observer`/`ack` travel with the job.
    pub fn enqueue_message(
        &self,
        bytes: Vec<u8>,
        observer: delivery::JobObserver,
        ack: Option<Sender<WriteOutcome>>,
        timeout: Duration,
    ) -> std::result::Result<(), delivery::AdmissionError> {
        if !self.is_running() {
            return Err(delivery::AdmissionError::Closed);
        }
        self.queue.submit(
            Job {
                bytes,
                observer: Some(observer),
                ack,
                urgent: false,
            },
            timeout,
        )
    }

    /// Write raw bytes into the PTY as an urgent job (Ctrl-C, attach input).
    ///
    /// Waits up to two seconds for the writer to acknowledge. A timeout is not
    /// an error: the bytes stay queued and the writer will still send them.
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<usize> {
        if !self.is_running() {
            return Err(BridgeError::AgentExited(self.name.clone()));
        }
        let rx = self.submit_urgent(bytes)?;
        match rx.recv_timeout(URGENT_ACK_TIMEOUT) {
            Ok(WriteOutcome::Written { bytes }) => Ok(bytes),
            Ok(WriteOutcome::Failed { detail, .. }) => Err(BridgeError::Daemon(detail)),
            Err(_) => Ok(bytes.len()),
        }
    }

    /// Submit an urgent job without waiting for its acknowledgement.
    ///
    /// Used by `kill`: Ctrl-C must be queued before the grace period starts, but
    /// shutdown must not stall for the full ack timeout per agent.
    fn submit_urgent(&self, bytes: &[u8]) -> Result<crossbeam_channel::Receiver<WriteOutcome>> {
        let (ack, rx) = crossbeam_channel::unbounded();
        self.queue
            .submit_urgent(
                Job {
                    bytes: bytes.to_vec(),
                    observer: None,
                    ack: Some(ack),
                    urgent: true,
                },
                URGENT_QUEUE_TIMEOUT,
            )
            .map_err(|error| match error {
                delivery::AdmissionError::Full => {
                    BridgeError::Daemon("urgent write queue is full".to_string())
                }
                delivery::AdmissionError::Closed => BridgeError::AgentExited(self.name.clone()),
            })?;
        Ok(rx)
    }

    /// Replay of the retained output.
    #[must_use]
    pub fn buffer_bytes(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .map_or_else(|e| e.into_inner().bytes(), |b| b.bytes())
    }

    /// Configured quiet period used to classify readiness.
    #[must_use]
    pub fn readiness_quiet(&self) -> Duration {
        self.readiness.quiet()
    }

    /// Trailing output lines with ANSI removed.
    #[must_use]
    pub fn tail_lines(&self, lines: usize) -> (Vec<String>, bool) {
        let buffer = self
            .buffer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (buffer.tail_lines(lines), buffer.truncated())
    }

    /// Record a delivered message (bounded history).
    pub fn record_message(&self, message: Message) {
        let mut messages = self
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        messages.push_back(message);
        while messages.len() > MAX_MESSAGES {
            messages.pop_front();
        }
    }

    /// Update the stored record for `id` when it is still in history.
    pub fn update_message(&self, id: MessageId, status: DeliveryStatus, bytes: usize, at: Millis) {
        let mut messages = self
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = messages.iter_mut().find(|record| record.id == id) {
            record.status = status;
            record.bytes = bytes;
            record.updated_at = at;
        }
    }

    /// Message history for this agent.
    #[must_use]
    pub fn history(&self) -> Vec<Message> {
        self.messages.lock().map_or_else(
            |e| e.into_inner().iter().cloned().collect(),
            |m| m.iter().cloned().collect(),
        )
    }

    /// Register a live event subscriber.
    ///
    /// The channel is bounded: a subscriber that cannot keep up is dropped
    /// rather than letting the daemon grow without limit.
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = bounded(SUBSCRIBER_QUEUE);
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscribers.push(tx);
        self.subscriber_count
            .store(subscribers.len(), Ordering::SeqCst);
        rx
    }

    /// Publish an event to every attached subscriber.
    #[allow(clippy::needless_pass_by_value)]
    pub fn publish(&self, event: Event) {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscribers.retain(|sender| match sender.try_send(event.clone()) {
            Ok(()) => true,
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                warn!(agent = %self.name, "dropping slow attach subscriber");
                false
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => false,
        });
        self.subscriber_count
            .store(subscribers.len(), Ordering::SeqCst);
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self
            .master
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        self.cols.store(cols, Ordering::SeqCst);
        self.rows.store(rows, Ordering::SeqCst);
        Ok(())
    }

    /// Terminate the agent: Ctrl-C first, then a hard kill if needed.
    ///
    /// `force` skips the grace period and tears the agent's process tree down
    /// (Windows: the whole job object; Unix: the agent's process group) instead
    /// of only the PTY client. It also runs when the client has already exited
    /// but its children have not - that is exactly the "descendants alive until
    /// daemon exit" case.
    pub fn kill(&self, force: bool) -> Result<()> {
        if force {
            return self.kill_tree();
        }
        if !self.is_running() {
            return Ok(());
        }
        let _ = self.submit_urgent(b"\x03");
        for _ in 0..10 {
            if !self.is_running() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(50));
        }
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        child.kill()?;
        Ok(())
    }

    /// `kill --force`: everything the reaper knows about.
    ///
    /// Errors from the tree teardown are reported to the caller once the
    /// fallback has been tried (or immediately when the client is already
    /// gone), so a script is not told the tree died when it did not.
    fn kill_tree(&self) -> Result<()> {
        // Read the liveness state before taking the reaper: on Unix a force kill
        // after the leader exited must not signal a recycled process group.
        let leader = if self.is_running() {
            reaper::LeaderState::Alive
        } else {
            reaper::LeaderState::Exited
        };
        let reaper = self
            .reaper
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(reaper) = reaper else {
            // No reaper: the OS cannot promise a tree teardown (install() already
            // logged that at spawn). Stop the client and report the degraded
            // path as success, as before this feature.
            let mut child = self
                .child
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if child.try_wait()?.is_none() {
                child.kill()?;
            }
            return Ok(());
        };

        let tree_error = match reaper.terminate(leader) {
            Ok(()) => None,
            Err(err) => {
                warn!(agent = %self.name, error = %err, "could not terminate the agent tree");
                Some(err)
            }
        };

        for _ in 0..10 {
            if !self.is_running() {
                return tree_error.map_or(Ok(()), Err);
            }
            thread::sleep(Duration::from_millis(50));
        }

        // The tree signal did not take the PTY client with it: at least stop
        // the client.
        let mut child = self
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let client_error = match child.try_wait() {
            Ok(Some(_)) => None,
            _ => child.kill().err(),
        };
        match (tree_error, client_error) {
            (None, None) => Ok(()),
            (Some(tree), None) => Err(tree),
            (None, Some(client)) => Err(client.into()),
            (Some(tree), Some(client)) => {
                debug!(agent = %self.name, error = %client, "client kill also failed");
                Err(tree)
            }
        }
    }
}

/// Resolve the program path so `CreateProcessW` succeeds on Windows.
///
/// `CreateProcessW` receives an explicit application name from `portable-pty`,
/// which bypasses the normal `PATH`/`PATHEXT` lookup and rejects forward
/// slashes, so both are handled here.
pub fn resolve_program(program: &str) -> String {
    #[cfg(windows)]
    {
        let normalized = program.replace('/', "\\");
        if normalized.contains('\\') || normalized.contains(':') {
            return normalized;
        }
        let extensions: Vec<String> = if normalized.contains('.') {
            vec![String::new()]
        } else {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
                .split(';')
                .map(str::to_ascii_lowercase)
                .collect()
        };
        if let Some(path) = std::env::var_os("PATH") {
            let mut dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
            if let Ok(root) = std::env::var("SystemRoot") {
                dirs.push(std::path::PathBuf::from(root).join("System32"));
            }
            for dir in dirs {
                for extension in &extensions {
                    let candidate = dir.join(format!("{normalized}{extension}"));
                    if candidate.is_file() {
                        return candidate.display().to_string();
                    }
                }
            }
        }
        normalized
    }
    #[cfg(unix)]
    {
        program.to_string()
    }
}

fn render_command(argv: &[String]) -> String {
    argv.iter()
        .map(|part| {
            if part.contains(' ') {
                format!("\"{part}\"")
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::PasteDetector;

    #[test]
    fn paste_detector_tracks_both_markers() {
        let mut detector = PasteDetector::default();
        assert_eq!(detector.observe(b"\x1b[?2004h"), Some(true));
        assert_eq!(detector.observe(b"prompt$ "), None);
        assert_eq!(detector.observe(b"\x1b[?2004l"), Some(false));
    }

    #[test]
    fn paste_detector_handles_a_marker_split_across_chunks() {
        let mut detector = PasteDetector::default();
        assert_eq!(detector.observe(b"\x1b[?20"), None);
        assert_eq!(detector.observe(b"04h"), Some(true));
        assert_eq!(detector.observe(b"text"), None);
    }

    #[test]
    fn paste_detector_uses_the_last_marker_in_one_chunk() {
        let mut detector = PasteDetector::default();
        assert_eq!(
            detector.observe(b"\x1b[?2004h enabled \x1b[?2004l disabled"),
            Some(false)
        );
        assert_eq!(
            detector.observe(b"\x1b[?2004l off \x1b[?2004h on"),
            Some(true)
        );
    }
}
