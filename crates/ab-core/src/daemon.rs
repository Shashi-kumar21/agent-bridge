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
use crate::error::{BridgeError, Result};
use crate::protocol::{
    validate_name, AgentInfo, ErrorCode, Event, Message, Millis, Request, Response, DEFAULT_COLS,
    DEFAULT_ROWS, PROTOCOL_VERSION,
};
use crate::transport;

const MAX_LOG: usize = 1000;
const HEARTBEAT: Duration = Duration::from_secs(10);

struct Inner {
    agents: Mutex<BTreeMap<String, Arc<Agent>>>,
    log: Mutex<VecDeque<Message>>,
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
                log: Mutex::new(VecDeque::new()),
                started_at: crate::now_ms(),
                shutdown: AtomicBool::new(false),
                endpoint,
            }),
        }
    }

    /// Bind the socket and serve connections until shutdown.
    pub fn run(self, on_ready: impl FnOnce()) -> Result<()> {
        let listener = transport::create_listener()?;
        listener.set_nonblocking(interprocess::local_socket::ListenerNonblockingMode::Both)?;
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
            let _ = agent.kill(true);
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
            } => {
                let agent = self.get(&target)?;
                let mut payload = message.into_bytes();
                if enter.unwrap_or(true) {
                    payload.push(b'\r');
                }
                let bytes = agent.write_bytes(&payload)?;
                let record = Message {
                    target: target.clone(),
                    from: from.clone(),
                    text: String::from_utf8_lossy(&payload[..payload.len().saturating_sub(1)])
                        .to_string(),
                    at: crate::now_ms(),
                };
                agent.record_message(record.clone());
                self.push_log(record.clone());
                agent_publish(&agent, record);
                Ok(Response::Sent { target, bytes })
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

    fn push_log(&self, message: Message) {
        let mut log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        log.push_back(message);
        while log.len() > MAX_LOG {
            log.pop_front();
        }
    }

    fn log(&self, limit: usize) -> Vec<Message> {
        let log = self
            .log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let skip = log.len().saturating_sub(limit);
        log.iter().skip(skip).cloned().collect()
    }
}

fn agent_publish(agent: &Arc<Agent>, message: Message) {
    agent.publish(Event::Message {
        agent: message.target.clone(),
        from: message.from,
        text: message.text,
        at: message.at,
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
    serde_json::to_writer(&mut *writer, value).map_err(std::io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
