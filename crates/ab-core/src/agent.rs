//! One PTY-backed agent session owned by the daemon.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use crossbeam_channel::{bounded, Receiver, Sender};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tracing::{debug, warn};

use crate::buffer::OutputBuffer;
use crate::error::{BridgeError, Result};
use crate::protocol::{AgentInfo, AgentStatus, Event, Message, Millis};
use crate::reaper::{self, ProcessReaper};

const MAX_MESSAGES: usize = 500;

/// Per-subscriber event queue depth. A consumer that falls this far behind is
/// disconnected instead of growing daemon memory without bound.
pub const SUBSCRIBER_QUEUE: usize = 1024;

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
    running: AtomicBool,
    exit_code: Mutex<Option<i32>>,
    exited_at: Mutex<Option<Millis>>,
    last_output_at: Mutex<Option<Millis>>,
    buffer: Mutex<OutputBuffer>,
    messages: Mutex<VecDeque<Message>>,
    writer: Mutex<Box<dyn Write + Send>>,
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
        let process_reaper = reaper::install(child.as_ref());
        let master = pair.master;
        let writer = master.take_writer()?;
        let reader = master.try_clone_reader()?;

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
            running: AtomicBool::new(true),
            exit_code: Mutex::new(None),
            exited_at: Mutex::new(None),
            last_output_at: Mutex::new(None),
            buffer: Mutex::new(OutputBuffer::new(crate::buffer::DEFAULT_CAPACITY)),
            messages: Mutex::new(VecDeque::new()),
            writer: Mutex::new(writer),
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
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = &chunk[..n];
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
        debug!(agent = %self.name, "pty reader closed");
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
            subscribers: self.subscriber_count.load(Ordering::SeqCst),
            cols: self.cols.load(Ordering::SeqCst),
            rows: self.rows.load(Ordering::SeqCst),
        }
    }

    /// Write raw bytes into the PTY (as if typed).
    pub fn write_bytes(&self, bytes: &[u8]) -> Result<usize> {
        if !self.is_running() {
            return Err(BridgeError::AgentExited(self.name.clone()));
        }
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(bytes.len())
    }

    /// Replay of the retained output.
    #[must_use]
    pub fn buffer_bytes(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .map_or_else(|e| e.into_inner().bytes(), |b| b.bytes())
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
        let _ = self.write_bytes(b"\x03");
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
        // Read the liveness flag before taking the reaper: on Unix a force kill
        // after the leader exited must not signal a recycled process group.
        let leader_alive = self.is_running();
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

        let tree_error = match reaper.terminate(leader_alive) {
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
