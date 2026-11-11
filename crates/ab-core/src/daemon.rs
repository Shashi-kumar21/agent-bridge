//! Daemon: owns every agent session and serves the local socket.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use base64::Engine as _;
use crossbeam_channel::select;
use interprocess::local_socket::traits::Listener;
use interprocess::local_socket::Stream;
use tracing::{debug, error, info};

use crate::agent::{Agent, SpawnConfig};
use crate::delivery::{self, JobEvent};
use crate::error::{BridgeError, Result};
use crate::protocol::{
    validate_name, AgentInfo, DeliveryStatus, ErrorCode, Event, Message, MessageId, Millis,
    Request, Response, DEFAULT_COLS, DEFAULT_ROWS, PROTOCOL_VERSION,
};
use crate::transport;

const MAX_LOG: usize = 2000;
const HEARTBEAT: Duration = Duration::from_secs(10);

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
            } => {
                let agent = self.get(&target)?;
                let enter = enter.unwrap_or(true);
                let mut payload = message.clone().into_bytes();
                if enter {
                    payload.push(b'\r');
                }
                let limit = delivery::max_message_bytes();
                if payload.len() > limit {
                    return Err(BridgeError::MessageTooLarge {
                        bytes: payload.len(),
                        limit,
                        env: delivery::MAX_MESSAGE_ENV,
                    });
                }

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
                };
                self.store_message(record.clone());
                agent.record_message(record.clone());
                agent_publish(&agent, &record);

                let wait = wait.unwrap_or(true);
                let queue_timeout = delivery::timeout_from_millis(
                    queue_timeout_ms,
                    delivery::DEFAULT_QUEUE_TIMEOUT,
                );
                let write_timeout = delivery::timeout_from_millis(
                    write_timeout_ms,
                    delivery::DEFAULT_WRITE_TIMEOUT,
                );

                let (ack, ack_rx) = crossbeam_channel::unbounded();
                let observer = self.message_observer(&agent, &target, id);
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
                    });
                }

                if !wait {
                    let status = self.current_status(id).unwrap_or(DeliveryStatus::Queued);
                    let bytes = if status == DeliveryStatus::Written {
                        payload.len()
                    } else {
                        0
                    };
                    return Ok(Response::Sent {
                        target,
                        bytes,
                        message_id: id,
                        status,
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
                    Ok(outcome) => Ok(Response::Sent {
                        target,
                        bytes: outcome.bytes(),
                        message_id: id,
                        status: outcome.status(),
                    }),
                    Err(WaitError::Timeout) => {
                        let status = self.mark_unknown(&agent, &target, id);
                        Ok(Response::Sent {
                            target,
                            bytes: 0,
                            message_id: id,
                            status,
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
                        })
                    }
                }
            }
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
        apply_status(&self.messages, agent, target, id, status, bytes);
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
    ) -> delivery::JobObserver {
        let messages = Arc::clone(&self.messages);
        let agent = Arc::clone(agent);
        let target = target.to_string();
        Box::new(move |event| match event {
            JobEvent::Started => apply_writing(&messages, &agent, &target, id),
            JobEvent::Done(outcome) => {
                let bytes = outcome.bytes();
                apply_status(&messages, &agent, &target, id, outcome.status(), bytes);
            }
        })
    }

    /// Current stored status for `id`.
    fn current_status(&self, id: MessageId) -> Option<DeliveryStatus> {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .map(|message| message.status.clone())
    }

    /// Mark a still-pending message `unknown` after the caller's write timeout.
    ///
    /// If the writer already recorded a terminal status, return that instead so
    /// the timeout cannot downgrade a completed write.
    fn mark_unknown(&self, agent: &Arc<Agent>, target: &str, id: MessageId) -> DeliveryStatus {
        let at = crate::now_ms();
        let mut log = self
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = log.entries.get_mut(&id) else {
            return DeliveryStatus::Unknown;
        };
        if !record.status.is_pending() {
            return record.status.clone();
        }
        record.status = DeliveryStatus::Unknown;
        record.bytes = 0;
        record.updated_at = at;
        // Keep the log lock until the agent copy and the event are updated, so
        // a concurrent `Done` cannot interleave a `written` before this
        // `unknown` and leave the two histories disagreeing.
        agent.update_message(id, DeliveryStatus::Unknown, 0, at);
        agent.publish(Event::MessageStatus {
            agent: target.to_string(),
            id,
            status: DeliveryStatus::Unknown,
            at,
        });
        drop(log);
        DeliveryStatus::Unknown
    }

    fn log(&self, limit: usize) -> Vec<Message> {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recent(limit)
    }
}

fn apply_writing(messages: &Mutex<MessageLog>, agent: &Agent, target: &str, id: MessageId) {
    let at = crate::now_ms();
    {
        let mut log = messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(record) = log.entries.get_mut(&id) else {
            return;
        };
        // A caller timeout may already have marked the record `unknown`; never
        // downgrade that to `writing` when the queued job finally starts.
        if !matches!(record.status, DeliveryStatus::Queued) {
            return;
        }
        record.status = DeliveryStatus::Writing;
        record.bytes = 0;
        record.updated_at = at;
        // Status writers serialize on this lock, so the agent copy and the
        // published event follow the same order as the daemon log.
        agent.update_message(id, DeliveryStatus::Writing, 0, at);
        agent.publish(Event::MessageStatus {
            agent: target.to_string(),
            id,
            status: DeliveryStatus::Writing,
            at,
        });
    }
}

fn apply_status(
    messages: &Mutex<MessageLog>,
    agent: &Agent,
    target: &str,
    id: MessageId,
    status: DeliveryStatus,
    bytes: usize,
) {
    let at = crate::now_ms();
    {
        let mut log = messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(record) = log.entries.get_mut(&id) {
            record.status = status.clone();
            record.bytes = bytes;
            record.updated_at = at;
        }
        agent.update_message(id, status.clone(), bytes, at);
        agent.publish(Event::MessageStatus {
            agent: target.to_string(),
            id,
            status,
            at,
        });
    }
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
    use super::{DeliveryStatus, Message, MessageLog, MAX_LOG};

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

        let pending: Vec<u64> = log.pending("dev", 10).iter().map(|m| m.id).collect();
        assert_eq!(pending, [2, 3]);
        assert_eq!(log.pending("dev", 1).len(), 1);
        assert_eq!(log.pending("qa", 10).len(), 1);
        assert!(log.pending("nobody", 10).is_empty());
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
}
