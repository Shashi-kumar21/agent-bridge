//! Daemon: owns every agent session and serves the local socket.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossbeam_channel::select;
use interprocess::local_socket::traits::{Listener, Stream as _};
use interprocess::local_socket::Stream;
use tracing::{debug, error, info};

use crate::agent::{Agent, SpawnConfig};
use crate::buffer::OutputBuffer;
use crate::delivery::{self, JobEvent};
use crate::error::{BridgeError, Result};
use crate::protocol::{
    validate_name, Ack, AgentInfo, DeliveryStatus, ErrorCode, Event, Message, MessageId, Millis,
    Readiness, Reply, Request, Response, DEFAULT_COLS, DEFAULT_ROWS, PROTOCOL_VERSION,
};
use crate::transport;

const MAX_LOG: usize = 2000;

/// Cap for the free-text note stored with an acknowledgement, so the bounded
/// history stays bounded.
const MAX_ACK_NOTE: usize = 1024;
const HEARTBEAT: Duration = Duration::from_secs(10);
/// How long `wait_idle` sleeps between readiness checks.
const IDLE_POLL: Duration = Duration::from_millis(25);
/// How often the `wait_reply` collector wakes without output.
const REPLY_POLL: Duration = Duration::from_millis(25);
/// Retained bytes for one collected reply.
const MAX_REPLY_BYTES: usize = 64 * 1024;
/// Trailing lines returned for one collected reply.
const MAX_REPLY_LINES: usize = 200;

/// Bounded, id-indexed message history.
struct MessageLog {
    entries: BTreeMap<MessageId, Message>,
    order: VecDeque<MessageId>,
}

impl MessageLog {
    fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, message: Message) {
        let id = message.id;
        self.entries.insert(id, message);
        self.order.push_back(id);
        while self.order.len() > MAX_LOG {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn get(&self, id: MessageId) -> Option<&Message> {
        self.entries.get(&id)
    }

    fn recent(&self, limit: usize) -> Vec<Message> {
        let skip = self.order.len().saturating_sub(limit);
        self.order
            .iter()
            .skip(skip)
            .filter_map(|id| self.entries.get(id).cloned())
            .collect()
    }

    fn pending(&self, target: &str, limit: usize) -> Vec<Message> {
        let mut messages: Vec<Message> = self
            .entries
            .values()
            .filter(|message| message.target == target && message.status.is_pending())
            .cloned()
            .collect();
        let skip = messages.len().saturating_sub(limit);
        messages.drain(..skip);
        messages
    }
}

/// Why a caller stopped waiting for a write outcome.
enum WaitError {
    /// The write deadline expired.
    Timeout,
    /// The writer channel closed without an outcome.
    Closed,
}

struct Inner {
    agents: Mutex<BTreeMap<String, Arc<Agent>>>,
    messages: Arc<Mutex<MessageLog>>,
    started_at: Millis,
    shutdown: AtomicBool,
    endpoint: String,
}

/// The daemon handle. Call [`Daemon::run`] to start serving.
pub struct Daemon {
    inner: Arc<Inner>,
}

impl Default for Daemon {
    fn default() -> Self {
        Self::new()
    }
}

impl Daemon {
    /// Create a daemon with default state dirs.
    #[must_use]
    pub fn new() -> Self {
        Self::with_endpoint(transport::endpoint())
    }

    /// Create a daemon that reports `endpoint` over the wire (used by tests).
    #[must_use]
    pub fn with_endpoint(endpoint: String) -> Self {
        Self {
            inner: Arc::new(Inner {
                agents: Mutex::new(BTreeMap::new()),
                messages: Arc::new(Mutex::new(MessageLog::new())),
                started_at: crate::now_ms(),
                shutdown: AtomicBool::new(false),
                endpoint,
            }),
        }
    }

    /// Bind the socket and serve connections until shutdown.
    pub fn run(self, on_ready: impl FnOnce()) -> Result<()> {
        let listener = transport::create_listener()?;
        listener.set_nonblocking(interprocess::local_socket::ListenerNonblockingMode::Accept)?;
        on_ready();
        info!(endpoint = %self.inner.endpoint, "daemon listening");

        while !self.inner.shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok(stream) => {
                    // BSD `accept()` inherits `O_NONBLOCK` from the listener
                    // (Linux does not), while `interprocess` only resets the
                    // mode for `ListenerNonblockingMode::Both`. Serve every
                    // connection in blocking mode explicitly; otherwise a
                    // request or response larger than the socket buffer can
                    // fail with `WouldBlock` on macOS.
                    if let Err(err) = stream.set_nonblocking(false) {
                        error!(error = %err, "could not make an accepted connection blocking");
                        continue;
                    }
                    let inner = Arc::clone(&self.inner);
                    thread::spawn(move || {
                        if let Err(err) = serve_connection(&inner, stream) {
                            debug!(error = %err, "connection closed");
                        }
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    error!(error = %err, "accept failed");
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }

        self.inner.stop_agents();
        info!("daemon stopped");
        Ok(())
    }
}

impl Inner {
    fn stop_agents(&self) {
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for agent in agents.values() {
            // Ctrl-C first so shells/agents can flush state, then force.
            let _ = agent.kill(false);
        }
    }

    fn handle(&self, request: Request) -> Response {
        match self.try_handle(request) {
            Ok(response) => response,
            Err(err) => Response::Error {
                code: err.code(),
                message: err.to_string(),
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    fn try_handle(&self, request: Request) -> Result<Response> {
        match request {
            Request::Ping => Ok(Response::Pong),
            Request::Version => Ok(Response::Version {
                protocol: PROTOCOL_VERSION,
                daemon: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                socket: self.endpoint.clone(),
                started_at: self.started_at,
            }),
            Request::List => Ok(Response::Agents {
                agents: self.list(),
            }),
            Request::New {
                name,
                command,
                cwd,
                cols,
                rows,
            } => {
                if !validate_name(&name) {
                    return Err(BridgeError::InvalidName(name));
                }
                let mut agents = self
                    .agents
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if agents.contains_key(&name) {
                    return Err(BridgeError::AgentExists(name));
                }
                let argv = command.unwrap_or_else(default_command);
                let config = SpawnConfig {
                    name: name.clone(),
                    command: argv,
                    cwd,
                    cols: cols.unwrap_or(DEFAULT_COLS),
                    rows: rows.unwrap_or(DEFAULT_ROWS),
                };
                let agent = Agent::spawn(&config)?;
                agents.insert(name, Arc::clone(&agent));
                Ok(Response::Agent {
                    agent: agent.info(),
                })
            }
            Request::Send {
                target,
                message,
                enter,
                from,
                queue_timeout_ms,
                write_timeout_ms,
                wait,
                paste,
                require_ack,
                wait_idle,
                wait_reply,
                timeout_ms,
            } => {
                let agent = self.get(&target)?;
                let enter = enter.unwrap_or(true);
                let require_ack = require_ack.unwrap_or(false);
                let wait = wait.unwrap_or(true);
                let wait_idle = wait_idle.unwrap_or(false);
                let wait_reply = wait_reply.unwrap_or(false);
                if wait_reply && !wait {
                    return Err(BridgeError::Conflict(
                        "wait_reply requires waiting for the write".to_string(),
                    ));
                }
                let wait_timeout =
                    delivery::timeout_from_millis(timeout_ms, delivery::DEFAULT_WAIT_TIMEOUT);
                let payload = delivery::compose_payload(
                    &message,
                    enter,
                    paste.unwrap_or_default(),
                    agent.bracketed_paste(),
                );
                let limit = delivery::max_message_bytes();
                if payload.len() > limit {
                    return Err(BridgeError::MessageTooLarge {
                        bytes: payload.len(),
                        limit,
                        env: delivery::MAX_MESSAGE_ENV,
                    });
                }

                if wait_idle {
                    wait_until_ready(&agent, wait_timeout)?;
                }
                let reply_rx = wait_reply.then(|| agent.subscribe());

                let id = delivery::next_message_id();
                let now = crate::now_ms();
                let record = Message {
                    id,
                    target: target.clone(),
                    from,
                    text: message,
                    at: now,
                    updated_at: now,
                    bytes: 0,
                    status: DeliveryStatus::Queued,
                    ack: None,
                };
                self.store_message(record.clone());
                agent.record_message(record.clone());
                agent_publish(&agent, &record);

                let queue_timeout = delivery::timeout_from_millis(
                    queue_timeout_ms,
                    delivery::DEFAULT_QUEUE_TIMEOUT,
                );
                let write_timeout = delivery::timeout_from_millis(
                    write_timeout_ms,
                    delivery::DEFAULT_WRITE_TIMEOUT,
                );

                let (ack, ack_rx) = crossbeam_channel::unbounded();
                let observer = self.message_observer(&agent, &target, id, require_ack);
                if let Err(error) = agent.enqueue_message(
                    payload.clone(),
                    observer,
                    wait.then_some(ack),
                    queue_timeout,
                ) {
                    let status = delivery::admission_status(error);
                    self.set_status(&agent, &target, id, status.clone(), 0);
                    return Ok(Response::Sent {
                        target,
                        bytes: 0,
                        message_id: id,
                        status,
                        reply: None,
                    });
                }

                if !wait {
                    let status = self.current_status(id).unwrap_or(DeliveryStatus::Queued);
                    let bytes = if status.is_written() {
                        payload.len()
                    } else {
                        0
                    };
                    return Ok(Response::Sent {
                        target,
                        bytes,
                        message_id: id,
                        status,
                        reply: None,
                    });
                }

                let waited = if write_timeout.is_zero() {
                    // `0` means wait forever; a dropped ack means the writer
                    // stopped without reporting.
                    ack_rx.recv().map_or(Err(WaitError::Closed), Ok)
                } else {
                    match ack_rx.recv_timeout(write_timeout) {
                        Ok(outcome) => Ok(outcome),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                            Err(WaitError::Timeout)
                        }
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                            Err(WaitError::Closed)
                        }
                    }
                };
                match waited {
                    Ok(outcome) => {
                        let status = settle(outcome.status(), require_ack);
                        let reply = if matches!(&outcome, delivery::WriteOutcome::Written { .. }) {
                            reply_rx.map(|rx| {
                                collect_reply(&agent, rx, agent.readiness_quiet(), wait_timeout)
                            })
                        } else {
                            None
                        };
                        Ok(Response::Sent {
                            target,
                            bytes: outcome.bytes(),
                            message_id: id,
                            status,
                            reply,
                        })
                    }
                    Err(WaitError::Timeout) => {
                        let status =
                            settle(self.status_on_timeout(&agent, &target, id), require_ack);
                        // A `Done` that landed just after the deadline still
                        // carries its byte count; everything else is 0.
                        let bytes = if status.is_written() {
                            payload.len()
                        } else {
                            0
                        };
                        Ok(Response::Sent {
                            target,
                            bytes,
                            message_id: id,
                            status,
                            reply: None,
                        })
                    }
                    Err(WaitError::Closed) => {
                        let status =
                            self.current_status(id)
                                .unwrap_or_else(|| DeliveryStatus::Failed {
                                    code: ErrorCode::Conflict,
                                    detail: "writer stopped before reporting the result"
                                        .to_string(),
                                });
                        Ok(Response::Sent {
                            target,
                            bytes: 0,
                            message_id: id,
                            status,
                            reply: None,
                        })
                    }
                }
            }
            Request::Ack { target, id, note } => self.acknowledge(&target, id, note),
            Request::Read { target, lines } => {
                let agent = self.get(&target)?;
                let (lines, truncated) = agent.tail_lines(lines.unwrap_or(40));
                Ok(Response::Output {
                    agent: target,
                    lines,
                    truncated,
                    seq: agent.seq(),
                })
            }
            Request::Kill { name, force } => {
                let agent = self.get(&name)?;
                agent.kill(force.unwrap_or(false))?;
                Ok(Response::Ok)
            }
            Request::Input { target, data } => {
                let agent = self.get(&target)?;
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|err| BridgeError::Protocol(err.to_string()))?;
                agent.write_bytes(&raw)?;
                Ok(Response::Ok)
            }
            Request::Resize { target, cols, rows } => {
                let agent = self.get(&target)?;
                agent.resize(cols, rows)?;
                Ok(Response::Ok)
            }
            Request::Log { limit } => Ok(Response::Messages {
                messages: self.log(limit.unwrap_or(50)),
            }),
            Request::MessageStatus { id } => {
                let message = self
                    .messages
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(id)
                    .cloned();
                match message {
                    Some(message) => Ok(Response::Message { message }),
                    None => Err(BridgeError::MessageNotFound(id)),
                }
            }
            Request::Pending { target, limit } => {
                // An unknown agent is an error, matching `sendmessage`.
                let _ = self.get(&target)?;
                let messages = self
                    .messages
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .pending(&target, limit.unwrap_or(50));
                Ok(Response::Messages { messages })
            }
            Request::Shutdown => {
                self.shutdown.store(true, Ordering::SeqCst);
                Ok(Response::Ok)
            }
            Request::Attach { .. } => Err(BridgeError::Daemon(
                "attach must be handled on a streaming connection".to_string(),
            )),
        }
    }

    fn get(&self, name: &str) -> Result<Arc<Agent>> {
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        agents
            .get(name)
            .cloned()
            .ok_or_else(|| BridgeError::AgentNotFound(name.to_string()))
    }

    fn list(&self) -> Vec<AgentInfo> {
        let agents = self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        agents.values().map(|agent| agent.info()).collect()
    }

    fn store_message(&self, message: Message) {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(message);
    }

    fn set_status(
        &self,
        agent: &Arc<Agent>,
        target: &str,
        id: MessageId,
        status: DeliveryStatus,
        bytes: usize,
    ) {
        let notify = |status: DeliveryStatus, bytes: usize, at: Millis| {
            agent.update_message(id, status.clone(), bytes, at);
            agent.publish(Event::MessageStatus {
                agent: target.to_string(),
                id,
                status,
                at,
            });
        };
        apply_status(&self.messages, id, status, bytes, &notify);
    }

    /// Status callback handed to the writer thread for one message job.
    ///
    /// The writer calls this from its own thread: `Started` publishes
    /// `writing`, `Done` publishes the terminal status. The job is already in
    /// the store, so an amended status (after a caller timeout) still lands.
    fn message_observer(
        &self,
        agent: &Arc<Agent>,
        target: &str,
        id: MessageId,
        require_ack: bool,
    ) -> delivery::JobObserver {
        let messages = Arc::clone(&self.messages);
        let agent = Arc::clone(agent);
        let target = target.to_string();
        Box::new(move |event| {
            let notify = |status: DeliveryStatus, bytes: usize, at: Millis| {
                agent.update_message(id, status.clone(), bytes, at);
                agent.publish(Event::MessageStatus {
                    agent: target.clone(),
                    id,
                    status,
                    at,
                });
            };
            match event {
                JobEvent::Started => apply_writing(&messages, id, &notify),
                JobEvent::Done(outcome) => {
                    let bytes = outcome.bytes();
                    let status = settle(outcome.status(), require_ack);
                    apply_status(&messages, id, status, bytes, &notify);
                }
            }
        })
    }

    /// Record an explicit acknowledgement from the receiving integration.
    ///
    /// Only the message's target may acknowledge it. A write that started
    /// (`written`, `awaiting_ack` or `unknown`) can be acknowledged; `unknown`
    /// is accepted because the acknowledgement itself proves the bytes
    /// arrived. Repeating the request returns the stored record so a retry
    /// cannot create a second acknowledgement or overwrite the note.
    fn acknowledge(&self, target: &str, id: MessageId, note: Option<String>) -> Result<Response> {
        let agent = self
            .agents
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(target)
            .cloned();
        let at = crate::now_ms();
        let mut log = self
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = log.entries.get_mut(&id) else {
            return Err(BridgeError::MessageNotFound(id));
        };
        if record.target != target {
            // An id that belongs to another agent is indistinguishable from an
            // unknown id: a caller cannot probe the global log through `ack`.
            return Err(BridgeError::MessageNotFound(id));
        }
        match record.status {
            DeliveryStatus::Acknowledged => {
                return Ok(Response::Message {
                    message: record.clone(),
                })
            }
            DeliveryStatus::Queued | DeliveryStatus::Writing => {
                return Err(BridgeError::Conflict(format!(
                    "message {id} has not been written yet ({})",
                    record.status.label()
                )))
            }
            DeliveryStatus::Failed { code, .. } => {
                return Err(BridgeError::Conflict(format!(
                    "message {id} was never written (failed: {})",
                    code.label()
                )))
            }
            DeliveryStatus::Written | DeliveryStatus::AwaitingAck | DeliveryStatus::Unknown => {}
        }
        if let Some(note) = &note {
            if note.len() > MAX_ACK_NOTE {
                return Err(BridgeError::Protocol(format!(
                    "ack note is {} bytes, limit is {MAX_ACK_NOTE}",
                    note.len()
                )));
            }
        }
        record.status = DeliveryStatus::Acknowledged;
        record.ack = Some(Ack { at, note });
        record.updated_at = at;
        let message = record.clone();
        drop(log);
        if let Some(agent) = agent {
            agent.update_message(id, DeliveryStatus::Acknowledged, message.bytes, at);
            agent.publish(Event::MessageStatus {
                agent: target.to_string(),
                id,
                status: DeliveryStatus::Acknowledged,
                at,
            });
        }
        Ok(Response::Message { message })
    }

    /// Current stored status for `id`.
    fn current_status(&self, id: MessageId) -> Option<DeliveryStatus> {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(|message| message.status.clone())
    }

    /// Status to report when the caller's write wait expires.
    ///
    /// Only a message whose write actually started becomes `unknown`: a message
    /// still waiting for a queue slot stays `queued`, and a terminal status is
    /// returned unchanged so a timeout can never downgrade a completed write.
    fn status_on_timeout(&self, agent: &Arc<Agent>, target: &str, id: MessageId) -> DeliveryStatus {
        let notify = |status: DeliveryStatus, bytes: usize, at: Millis| {
            agent.update_message(id, status.clone(), bytes, at);
            agent.publish(Event::MessageStatus {
                agent: target.to_string(),
                id,
                status,
                at,
            });
        };
        status_after_timeout(&self.messages, id, &notify)
    }

    fn log(&self, limit: usize) -> Vec<Message> {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recent(limit)
    }
}

type StatusNotify<'a> = &'a dyn Fn(DeliveryStatus, usize, Millis);

/// Publish `queued -> writing`.
///
/// Status writers serialize on the message-log lock, so the agent copy and the
/// event follow the same order as the daemon log. A caller timeout may already
/// have marked the record `unknown`; never downgrade that to `writing`.
fn apply_writing(messages: &Mutex<MessageLog>, id: MessageId, notify: StatusNotify<'_>) {
    let at = crate::now_ms();
    let mut log = messages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(record) = log.entries.get_mut(&id) else {
        return;
    };
    if !matches!(record.status, DeliveryStatus::Queued) {
        return;
    }
    record.status = DeliveryStatus::Writing;
    record.bytes = 0;
    record.updated_at = at;
    notify(DeliveryStatus::Writing, 0, at);
}

/// Block until the target is not `busy`, or the deadline expires.
///
/// `unknown` (no visible line yet) counts as ready so a fresh shell is not
/// blocked forever. A busy target that never settles fails the send instead of
/// typing into a running command.
fn wait_until_ready(agent: &Agent, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        match agent.readiness() {
            Readiness::Busy => {}
            _ => return Ok(()),
        }
        if started.elapsed() >= timeout {
            return Err(BridgeError::WaitIdleTimeout {
                target: agent.name().to_string(),
                millis: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            });
        }
        if !agent.is_running() {
            return Err(BridgeError::AgentExited(agent.name().to_string()));
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        thread::sleep(IDLE_POLL.min(remaining));
    }
}

/// Collect output published after the write until it settles or the deadline
/// expires. Best effort: bounded bytes and lines, ANSI stripped.
fn collect_reply(
    agent: &Agent,
    rx: crossbeam_channel::Receiver<Event>,
    quiet: Duration,
    timeout: Duration,
) -> Reply {
    let started = Instant::now();
    let deadline = started + timeout;
    let mut buffer = OutputBuffer::new(MAX_REPLY_BYTES);
    let mut last_output = started;
    let mut saw_output = false;
    let mut settled = false;
    let mut timed_out = false;
    loop {
        let now = Instant::now();
        if saw_output && now.duration_since(last_output) >= quiet {
            settled = true;
            break;
        }
        if !agent.is_running() {
            // Drain what the pump published before it noticed the exit; the
            // final marker is often emitted only a few milliseconds earlier.
            while let Ok(event) = rx.try_recv() {
                if let Event::Output { data, .. } = event {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                        buffer.push(&bytes);
                    }
                    saw_output = true;
                    last_output = Instant::now();
                }
            }
            if agent.output_closed() {
                // The pump stores the flag after its last publish, so the
                // channel is fully drained at this point.
                settled = true;
                break;
            }
        }
        if now >= deadline {
            timed_out = true;
            break;
        }
        let wait = deadline.saturating_duration_since(now).min(REPLY_POLL);
        match rx.recv_timeout(wait) {
            Ok(Event::Output { data, .. }) => {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) {
                    buffer.push(&bytes);
                }
                saw_output = true;
                last_output = Instant::now();
            }
            Ok(_) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                settled = true;
                break;
            }
        }
    }
    drop(rx);
    let lines = buffer.tail_lines(MAX_REPLY_LINES);
    Reply {
        lines,
        settled,
        timed_out,
        truncated: buffer.truncated(),
    }
}

/// Final status for a completed write.
///
/// A sender that asked for an acknowledgement sees `awaiting_ack` instead of
/// `written`; the bytes are on the wire either way.
fn settle(status: DeliveryStatus, require_ack: bool) -> DeliveryStatus {
    if require_ack && status == DeliveryStatus::Written {
        DeliveryStatus::AwaitingAck
    } else {
        status
    }
}

/// Publish the writer's terminal status (or amend an earlier `unknown`).
fn apply_status(
    messages: &Mutex<MessageLog>,
    id: MessageId,
    status: DeliveryStatus,
    bytes: usize,
    notify: StatusNotify<'_>,
) {
    let at = crate::now_ms();
    let mut log = messages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(record) = log.entries.get_mut(&id) {
        if matches!(record.status, DeliveryStatus::Acknowledged) {
            // An explicit ack proves the bytes arrived, so a late `unknown`
            // amendment must not downgrade it. Keep the byte count it reports.
            if status.is_written() && bytes > 0 {
                record.bytes = bytes;
            }
            return;
        }
        record.status = status.clone();
        record.bytes = bytes;
        record.updated_at = at;
        notify(status, bytes, at);
    }
}

/// Status to report when the caller's write wait expires.
///
/// Only a message whose write actually started becomes `unknown`: a message
/// still waiting for a queue slot stays `queued`, and a terminal status is
/// returned unchanged so a timeout can never downgrade a completed write.
fn status_after_timeout(
    messages: &Mutex<MessageLog>,
    id: MessageId,
    notify: StatusNotify<'_>,
) -> DeliveryStatus {
    let at = crate::now_ms();
    let mut log = messages
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(record) = log.entries.get_mut(&id) else {
        return DeliveryStatus::Unknown;
    };
    match record.status {
        DeliveryStatus::Queued => return DeliveryStatus::Queued,
        DeliveryStatus::Writing => {}
        _ => return record.status.clone(),
    }
    record.status = DeliveryStatus::Unknown;
    record.bytes = 0;
    record.updated_at = at;
    notify(DeliveryStatus::Unknown, 0, at);
    DeliveryStatus::Unknown
}

fn agent_publish(agent: &Arc<Agent>, message: &Message) {
    agent.publish(Event::Message {
        agent: message.target.clone(),
        id: message.id,
        from: message.from.clone(),
        text: message.text.clone(),
        at: message.at,
        status: message.status.clone(),
    });
}

fn default_command() -> Vec<String> {
    #[cfg(windows)]
    {
        vec!["cmd.exe".to_string()]
    }
    #[cfg(unix)]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        vec![shell]
    }
}

fn serve_connection(inner: &Inner, stream: Stream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }

    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(err) => {
            let mut stream = reader.into_inner();
            return write_frame(
                &mut stream,
                &Response::Error {
                    code: ErrorCode::BadRequest,
                    message: err.to_string(),
                },
            );
        }
    };

    if let Request::Attach { target, from_seq } = request {
        return stream_attach(inner, reader.into_inner(), &target, from_seq);
    }

    let response = inner.handle(request);
    let mut stream = reader.into_inner();
    write_frame(&mut stream, &response)
}

fn stream_attach(
    inner: &Inner,
    mut stream: Stream,
    target: &str,
    from_seq: Option<u64>,
) -> std::io::Result<()> {
    let agent = match inner.get(target) {
        Ok(agent) => agent,
        Err(err) => {
            return write_frame(
                &mut stream,
                &Response::Error {
                    code: err.code(),
                    message: err.to_string(),
                },
            );
        }
    };

    let receiver = agent.subscribe();
    write_frame(
        &mut stream,
        &Event::Hello {
            agent: target.to_string(),
            protocol: PROTOCOL_VERSION,
        },
    )?;

    let should_replay = from_seq.is_none_or(|seq| seq < agent.seq());
    if should_replay {
        let replay = agent.buffer_bytes();
        if !replay.is_empty() {
            write_frame(
                &mut stream,
                &Event::Output {
                    agent: target.to_string(),
                    seq: agent.seq(),
                    data: base64::engine::general_purpose::STANDARD.encode(replay),
                },
            )?;
        }
    }

    // An already exited agent must not wait for the heartbeat tick.
    if !agent.is_running() {
        let code = agent.info().exit_code;
        write_frame(
            &mut stream,
            &Event::Exit {
                agent: target.to_string(),
                code,
            },
        )?;
        return Ok(());
    }

    let ticker = crossbeam_channel::tick(HEARTBEAT);
    let mut abandon = false;
    loop {
        select! {
            recv(receiver) -> event => {
                match event {
                    Ok(event) => {
                        let exiting = matches!(event, Event::Exit { .. });
                        if write_frame(&mut stream, &event).is_err() {
                            break;
                        }
                        if exiting {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            recv(ticker) -> _ => {
                if !agent.is_running() {
                    break;
                }
                if write_frame(&mut stream, &Event::Heartbeat { at: crate::now_ms() }).is_err() {
                    abandon = true;
                    break;
                }
            }
        }
    }
    debug!(agent = %target, abandon, "attach stream closed");
    Ok(())
}

fn write_frame<W: Write, T: serde::Serialize>(writer: &mut W, value: &T) -> std::io::Result<()> {
    // Build the whole frame first: NDJSON framing must never interleave a
    // newline with a partially written frame.
    let mut frame = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    frame.push(b'\n');
    writer.write_all(&frame)?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::{
        apply_status, apply_writing, settle, status_after_timeout, DeliveryStatus, Message,
        MessageLog, Millis, Mutex, MAX_LOG,
    };

    fn record(id: u64, target: &str, status: DeliveryStatus) -> Message {
        Message {
            id,
            target: target.to_string(),
            from: None,
            text: format!("m{id}"),
            at: id,
            updated_at: id,
            bytes: 0,
            status,
            ack: None,
        }
    }

    #[test]
    fn recent_returns_the_last_entries_in_order() {
        let mut log = MessageLog::new();
        log.insert(record(1, "dev", DeliveryStatus::Written));
        log.insert(record(2, "dev", DeliveryStatus::Queued));
        log.insert(record(3, "dev", DeliveryStatus::Writing));

        let recent: Vec<u64> = log.recent(2).iter().map(|m| m.id).collect();
        assert_eq!(recent, [2, 3]);
        assert_eq!(log.recent(10).len(), 3);
    }

    #[test]
    fn pending_filters_by_target_and_status() {
        let mut log = MessageLog::new();
        log.insert(record(1, "dev", DeliveryStatus::Written));
        log.insert(record(2, "dev", DeliveryStatus::Queued));
        log.insert(record(3, "dev", DeliveryStatus::Writing));
        log.insert(record(4, "qa", DeliveryStatus::Queued));
        log.insert(record(5, "dev", DeliveryStatus::Unknown));

        let pending: Vec<u64> = log.pending("dev", 10).iter().map(|m| m.id).collect();
        assert_eq!(pending, [2, 3, 5], "unknown is still in flight");
        assert_eq!(log.pending("dev", 1).len(), 1);
        assert_eq!(log.pending("qa", 10).len(), 1);
        assert!(log.pending("nobody", 10).is_empty());
    }

    /// Timeout transitions: only a started write becomes `unknown`; a queued
    /// one stays queued and a terminal one is never downgraded.
    #[test]
    fn timeout_only_marks_a_started_write_unknown() {
        let log = Mutex::new(MessageLog::new());
        log.lock()
            .expect("log")
            .insert(record(1, "dev", DeliveryStatus::Queued));

        let seen: Mutex<Vec<(DeliveryStatus, usize)>> = Mutex::new(Vec::new());
        let notify = |status: DeliveryStatus, bytes: usize, _at: Millis| {
            seen.lock().expect("seen").push((status, bytes));
        };

        assert_eq!(
            status_after_timeout(&log, 1, &notify),
            DeliveryStatus::Queued,
            "a message that never started must not be reported unknown"
        );
        assert!(seen.lock().expect("seen").is_empty());

        apply_writing(&log, 1, &notify);
        assert_eq!(
            status_after_timeout(&log, 1, &notify),
            DeliveryStatus::Unknown
        );
        assert_eq!(
            *seen.lock().expect("seen"),
            [(DeliveryStatus::Writing, 0), (DeliveryStatus::Unknown, 0)]
        );

        // The writer finishing later amends the unknown record.
        apply_status(&log, 1, DeliveryStatus::Written, 7, &notify);
        assert_eq!(
            log.lock().expect("log").get(1).expect("record").status,
            DeliveryStatus::Written
        );
        assert_eq!(
            status_after_timeout(&log, 1, &notify),
            DeliveryStatus::Written,
            "a timeout must never downgrade a completed write"
        );
        assert_eq!(log.lock().expect("log").get(1).expect("record").bytes, 7);
    }

    #[test]
    fn the_log_is_bounded() {
        let mut log = MessageLog::new();
        for id in 1..=(MAX_LOG as u64 + 5) {
            log.insert(record(id, "dev", DeliveryStatus::Written));
        }
        assert_eq!(log.recent(MAX_LOG + 100).len(), MAX_LOG);
        assert!(log.get(1).is_none());
        assert!(log.get(MAX_LOG as u64 + 5).is_some());
    }

    #[test]
    fn settle_maps_a_completed_write_to_awaiting_ack_when_requested() {
        assert_eq!(
            settle(DeliveryStatus::Written, true),
            DeliveryStatus::AwaitingAck
        );
        assert_eq!(
            settle(DeliveryStatus::Written, false),
            DeliveryStatus::Written
        );
        let failed = DeliveryStatus::Failed {
            code: crate::protocol::ErrorCode::Conflict,
            detail: "gone".to_string(),
        };
        assert_eq!(
            settle(failed.clone(), true),
            failed,
            "a failed write never becomes awaiting_ack"
        );
        assert_eq!(
            settle(DeliveryStatus::Unknown, true),
            DeliveryStatus::Unknown
        );
    }

    /// An explicit ack is stronger than the write outcome: a late `done`
    /// event must not downgrade it, but may fill in the byte count.
    #[test]
    fn an_acknowledgement_survives_a_late_write_outcome() {
        let log = Mutex::new(MessageLog::new());
        log.lock()
            .expect("log")
            .insert(record(1, "dev", DeliveryStatus::Acknowledged));
        let seen = std::sync::atomic::AtomicUsize::new(0);
        let notify = |_status: DeliveryStatus, _bytes: usize, _at: Millis| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        };

        apply_status(&log, 1, DeliveryStatus::Written, 9, &notify);
        let guard = log.lock().expect("log");
        let message = guard.get(1).expect("record");
        assert_eq!(message.status, DeliveryStatus::Acknowledged);
        assert_eq!(message.bytes, 9, "the confirmed byte count is kept");
        drop(guard);
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no status event is emitted for a downgrade that did not happen"
        );
    }
}
