//! Wire protocol between the CLI/app and the daemon.
//!
//! Framing is newline-delimited JSON: one [`Request`] per line from the client,
//! one [`Response`] (or a stream of [`Event`]s for `attach`) per line from the
//! daemon. Binary PTY output travels base64 encoded so the frames stay valid
//! UTF-8 JSON.

use serde::{Deserialize, Serialize};

/// Bumped whenever the wire shape changes. New `cmd`/`event` tags are unknown
/// to older daemons, so additive changes bump it too; the CLI compares it for
/// equality and restarts a daemon only when no agent is live.
pub const PROTOCOL_VERSION: u32 = 3;

/// Default terminal width for spawned agents.
pub const DEFAULT_COLS: u16 = 120;

/// Default terminal height for spawned agents.
pub const DEFAULT_ROWS: u16 = 32;

/// Monotonic per-agent output sequence number.
pub type Seq = u64;

/// Milliseconds since the Unix epoch.
pub type Millis = u64;

/// Daemon-wide message identifier, starting at 1.
///
/// Unique for the lifetime of one daemon process. The message history is
/// in-memory only, so an id from a previous daemon is eventually `not_found`.
pub type MessageId = u64;

/// Machine readable error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed request or invalid argument.
    BadRequest,
    /// Unknown agent.
    NotFound,
    /// Name collision or dead agent.
    Conflict,
    /// Daemon is not reachable.
    NotRunning,
    /// Unexpected internal failure.
    Internal,
    /// The message is larger than the configured limit.
    TooLarge,
    /// The per-agent write queue is full.
    QueueFull,
    /// A queue or write deadline expired.
    Timeout,
}

impl ErrorCode {
    /// Stable `snake_case` label, identical to the wire form.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::NotRunning => "not_running",
            Self::Internal => "internal",
            Self::TooLarge => "too_large",
            Self::QueueFull => "queue_full",
            Self::Timeout => "timeout",
        }
    }
}

/// Lifecycle state of an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// Process is alive and attached to its PTY.
    Running,
    /// Process has terminated.
    Exited,
}

/// Snapshot of one agent managed by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Stable short identifier.
    pub id: String,
    /// Unique, human friendly name used for addressing.
    pub name: String,
    /// Current lifecycle state.
    pub status: AgentStatus,
    /// OS process id while running.
    pub pid: Option<u32>,
    /// Full command line that was spawned.
    pub command: String,
    /// Working directory of the child process.
    pub cwd: String,
    /// Creation timestamp (ms since epoch).
    pub created_at: Millis,
    /// Last time the process wrote output.
    pub last_output_at: Option<Millis>,
    /// Exit timestamp, once terminated.
    pub exited_at: Option<Millis>,
    /// Exit code, once terminated.
    pub exit_code: Option<i32>,
    /// Number of messages routed to this agent.
    pub message_count: usize,
    /// Jobs waiting in the agent's write queue, not counting the write in
    /// flight.
    #[serde(default)]
    pub queued: usize,
    /// Number of live attach subscribers.
    pub subscribers: usize,
    /// PTY width.
    pub cols: u16,
    /// PTY height.
    pub rows: u16,
}

/// Delivery lifecycle of one message.
///
/// `Queued` and `Writing` are transient. `Written` and `Failed` are terminal;
/// `Unknown` means the write started but the caller stopped waiting, and the
/// daemon may still amend it to `Written` or `Failed`. A message is never
/// reported as delivered or acknowledged: writing bytes into a PTY only proves
/// the bytes were accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Accepted by the daemon and waiting in the agent's write queue.
    Queued,
    /// The writer is currently writing this job into the PTY.
    Writing,
    /// The write and flush returned successfully.
    #[default]
    Written,
    /// The message was never written, or the write failed.
    Failed {
        /// Machine readable cause.
        code: ErrorCode,
        /// Human readable detail.
        detail: String,
    },
    /// The write started but its outcome was not confirmed before the
    /// caller's deadline; the daemon may still amend it to `Written`/`Failed`.
    Unknown,
}

impl DeliveryStatus {
    /// `true` while the daemon may still change the status.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Queued | Self::Writing | Self::Unknown)
    }

    /// Short label used by the CLI and the message log.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Writing => "writing",
            Self::Written => "written",
            Self::Failed { .. } => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// One inter-agent message and its delivery status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Daemon-wide identifier (0 for records from a v1 daemon).
    #[serde(default)]
    pub id: MessageId,
    /// Recipient agent name.
    pub target: String,
    /// Optional sender label (`--from`).
    pub from: Option<String>,
    /// Message body as delivered to the PTY.
    pub text: String,
    /// Enqueue timestamp (ms since epoch).
    pub at: Millis,
    /// Last status change (ms since epoch).
    #[serde(default)]
    pub updated_at: Millis,
    /// Payload bytes accepted by the PTY (0 until written).
    #[serde(default)]
    pub bytes: usize,
    /// Current delivery status (v1 records default to `written`).
    #[serde(default)]
    pub status: DeliveryStatus,
}

/// Client to daemon commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Liveness probe.
    Ping,
    /// Protocol handshake details.
    Version,
    /// List every agent.
    List,
    /// Spawn a new agent inside a daemon-owned PTY.
    New {
        /// Unique name.
        name: String,
        /// Explicit argv; defaults to the platform shell.
        #[serde(default)]
        command: Option<Vec<String>>,
        /// Working directory; defaults to the daemon's cwd.
        #[serde(default)]
        cwd: Option<String>,
        /// PTY width.
        #[serde(default)]
        cols: Option<u16>,
        /// PTY height.
        #[serde(default)]
        rows: Option<u16>,
    },
    /// Type a message into another agent's terminal.
    Send {
        /// Recipient name.
        target: String,
        /// Text to deliver.
        message: String,
        /// Append a carriage return (simulates pressing Enter).
        #[serde(default)]
        enter: Option<bool>,
        /// Optional sender label recorded in the message log.
        #[serde(default)]
        from: Option<String>,
        /// Milliseconds to wait for a free queue slot. `0` fails fast.
        #[serde(default)]
        queue_timeout_ms: Option<u64>,
        /// Milliseconds to wait for the write. `0` waits forever. Ignored
        /// when `wait` is false.
        #[serde(default)]
        write_timeout_ms: Option<u64>,
        /// Wait for a terminal status before replying (default true).
        #[serde(default)]
        wait: Option<bool>,
    },
    /// Read the tail of an agent's output buffer.
    Read {
        /// Agent name.
        target: String,
        /// Number of trailing lines.
        #[serde(default)]
        lines: Option<usize>,
    },
    /// Terminate an agent.
    Kill {
        /// Agent name.
        name: String,
        /// Skip the graceful signal and kill immediately.
        #[serde(default)]
        force: Option<bool>,
    },
    /// Subscribe to an agent's live output.
    Attach {
        /// Agent name.
        target: String,
        /// Do not replay buffered output when this is at or above the agent's
        /// current sequence number.
        #[serde(default)]
        from_seq: Option<Seq>,
    },
    /// Forward keystrokes to an agent (used by attach).
    Input {
        /// Agent name.
        target: String,
        /// Base64 encoded bytes.
        data: String,
    },
    /// Resize the agent's PTY.
    Resize {
        /// Agent name.
        target: String,
        /// New width.
        cols: u16,
        /// New height.
        rows: u16,
    },
    /// Global message history.
    Log {
        /// Maximum entries to return.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Delivery status of one message.
    MessageStatus {
        /// Identifier returned by `Send` (or the message event).
        id: MessageId,
    },
    /// Messages that may still change: queued, writing or unknown.
    Pending {
        /// Agent name.
        target: String,
        /// Maximum entries to return.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Ask the daemon to exit.
    Shutdown,
}

/// Daemon replies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// Liveness reply.
    Pong,
    /// Handshake reply.
    Version {
        /// Protocol version.
        protocol: u32,
        /// Daemon crate version.
        daemon: String,
        /// Daemon process id, or `0` when the daemon predates this field.
        #[serde(default)]
        pid: u32,
        /// Socket path in use.
        socket: String,
        /// Daemon start time (ms since epoch).
        started_at: Millis,
    },
    /// Generic success without payload.
    Ok,
    /// Failure.
    Error {
        /// Machine readable category.
        code: ErrorCode,
        /// Human readable detail.
        message: String,
    },
    /// Agent list.
    Agents {
        /// All known agents.
        agents: Vec<AgentInfo>,
    },
    /// Details for a single agent.
    Agent {
        /// The agent.
        agent: AgentInfo,
    },
    /// Buffered output.
    Output {
        /// Agent name.
        agent: String,
        /// Trailing lines (ANSI stripped).
        lines: Vec<String>,
        /// True when the buffer rotated out older content.
        truncated: bool,
        /// Last sequence number included.
        seq: Seq,
    },
    /// Message accepted, with its delivery status.
    Sent {
        /// Recipient.
        target: String,
        /// Bytes accepted by the PTY, including the appended carriage return
        /// (0 while queued or when the write failed).
        bytes: usize,
        /// Daemon-wide message identifier.
        #[serde(default)]
        message_id: MessageId,
        /// Status at the time the response was sent.
        #[serde(default)]
        status: DeliveryStatus,
    },
    /// One message record.
    Message {
        /// The message and its current status.
        message: Message,
    },
    /// Message history.
    Messages {
        /// Entries, oldest first.
        messages: Vec<Message>,
    },
}

/// Streamed daemon events (only sent on `attach` connections).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Connection accepted.
    Hello {
        /// Agent name.
        agent: String,
        /// Protocol version.
        protocol: u32,
    },
    /// A chunk of terminal output.
    ///
    /// The daemon replays the retained buffer right after `Hello` and then
    /// streams live chunks, so the same sequence number can be observed twice.
    /// Clients **must** drop any `Output` frame whose `seq` is not greater than
    /// the highest `seq` already seen.
    Output {
        /// Agent name.
        agent: String,
        /// Monotonic sequence number.
        seq: Seq,
        /// Base64 encoded raw bytes.
        data: String,
    },
    /// A message was accepted for this agent.
    Message {
        /// Agent name.
        agent: String,
        /// Daemon-wide message identifier.
        #[serde(default)]
        id: MessageId,
        /// Sender label.
        from: Option<String>,
        /// Message body.
        text: String,
        /// Timestamp.
        at: Millis,
        /// Delivery status at enqueue time.
        #[serde(default)]
        status: DeliveryStatus,
    },
    /// A message changed delivery status.
    MessageStatus {
        /// Agent name.
        agent: String,
        /// Daemon-wide message identifier.
        id: MessageId,
        /// New status.
        status: DeliveryStatus,
        /// Timestamp of the change.
        at: Millis,
    },
    /// The agent process terminated.
    Exit {
        /// Agent name.
        agent: String,
        /// Exit code, when available.
        code: Option<i32>,
    },
    /// Keepalive so dead attach connections are detected.
    Heartbeat {
        /// Timestamp.
        at: Millis,
    },
}

/// Validate an agent name: 1-32 chars of `[A-Za-z0-9_-]`.
pub fn validate_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::{
        validate_name, DeliveryStatus, ErrorCode, Event, Message, Request, Response,
        PROTOCOL_VERSION,
    };

    #[test]
    fn accepts_reasonable_names() {
        for name in ["dev", "qa-1", "agent_two", "A1"] {
            assert!(validate_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_unsafe_names() {
        for name in [
            "",
            "has space",
            "semi;colon",
            "../escape",
            "toolongname_toolongname_toolongname",
        ] {
            assert!(!validate_name(name), "{name} should be rejected");
        }
    }

    /// A new client must be able to talk to a daemon that predates `pid`.
    #[test]
    fn version_frame_without_pid_still_parses() {
        let legacy = r#"{"kind":"version","protocol":1,"daemon":"0.1.0","socket":"\\\\.\\pipe\\x","started_at":0}"#;
        let parsed: Response = serde_json::from_str(legacy).expect("legacy version frame");
        assert!(matches!(
            parsed,
            Response::Version {
                protocol: 1,
                pid: 0,
                ..
            }
        ));
    }

    #[test]
    fn protocol_is_three() {
        assert_eq!(PROTOCOL_VERSION, 3);
    }

    #[test]
    fn error_code_labels_match_the_wire_form() {
        for code in [
            ErrorCode::BadRequest,
            ErrorCode::NotFound,
            ErrorCode::Conflict,
            ErrorCode::NotRunning,
            ErrorCode::Internal,
            ErrorCode::TooLarge,
            ErrorCode::QueueFull,
            ErrorCode::Timeout,
        ] {
            let wire = serde_json::to_string(&code).expect("serialize");
            assert_eq!(wire, format!("\"{}\"", code.label()));
        }
    }

    /// A message from a v1 daemon has no id, status or byte count.
    #[test]
    fn legacy_message_frame_defaults_to_written() {
        let legacy = r#"{"target":"dev","from":"qa","text":"hi","at":7}"#;
        let parsed: Message = serde_json::from_str(legacy).expect("legacy message");
        assert_eq!(parsed.id, 0);
        assert_eq!(parsed.bytes, 0);
        assert_eq!(parsed.status, DeliveryStatus::Written);
        assert!(matches!(parsed.status, DeliveryStatus::Written));
    }

    #[test]
    fn delivery_status_round_trips_with_tag() {
        let failed = DeliveryStatus::Failed {
            code: ErrorCode::TooLarge,
            detail: "too big".to_string(),
        };
        let json = serde_json::to_string(&failed).expect("serialize");
        assert!(json.contains("\"state\":\"failed\""));
        assert!(json.contains("\"code\":\"too_large\""));
        let parsed: DeliveryStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, failed);

        let queued = serde_json::to_string(&DeliveryStatus::Queued).expect("serialize");
        assert_eq!(queued, r#"{"state":"queued"}"#);
    }

    #[test]
    fn sent_frame_round_trips() {
        let sent = Response::Sent {
            target: "dev".to_string(),
            bytes: 3,
            message_id: 42,
            status: DeliveryStatus::Written,
        };
        let json = serde_json::to_string(&sent).expect("serialize");
        let parsed: Response = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            Response::Sent {
                message_id,
                status,
                bytes,
                ..
            } => {
                assert_eq!(message_id, 42);
                assert_eq!(bytes, 3);
                assert_eq!(status, DeliveryStatus::Written);
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    /// A v1 `Sent` frame carries no id or status; v2 clients must not fail.
    #[test]
    fn legacy_sent_frame_still_parses() {
        let legacy = r#"{"kind":"sent","target":"dev","bytes":3}"#;
        let parsed: Response = serde_json::from_str(legacy).expect("legacy sent");
        assert!(matches!(
            parsed,
            Response::Sent {
                message_id: 0,
                status: DeliveryStatus::Written,
                ..
            }
        ));
    }

    #[test]
    fn message_status_request_round_trips() {
        let request = Request::MessageStatus { id: 9 };
        let json = serde_json::to_string(&request).expect("serialize");
        assert!(json.contains("\"cmd\":\"message_status\""));
        let parsed: Request = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(parsed, Request::MessageStatus { id: 9 }));
    }

    /// v2A `Send` frames have no queue/write timeout or `wait`; they must still
    /// parse and use the daemon defaults.
    #[test]
    fn legacy_send_frame_parses_with_defaults() {
        let legacy = r#"{"cmd":"send","target":"dev","message":"hi"}"#;
        let parsed: Request = serde_json::from_str(legacy).expect("legacy send");
        match parsed {
            Request::Send {
                target,
                message,
                enter,
                from,
                queue_timeout_ms,
                write_timeout_ms,
                wait,
            } => {
                assert_eq!(target, "dev");
                assert_eq!(message, "hi");
                assert_eq!(enter, None);
                assert_eq!(from, None);
                assert_eq!(queue_timeout_ms, None);
                assert_eq!(write_timeout_ms, None);
                assert_eq!(wait, None);
            }
            other => panic!("unexpected request: {other:?}"),
        }
    }

    #[test]
    fn send_timeouts_round_trip() {
        let request = Request::Send {
            target: "dev".to_string(),
            message: "hi".to_string(),
            enter: Some(false),
            from: Some("qa".to_string()),
            queue_timeout_ms: Some(250),
            write_timeout_ms: Some(0),
            wait: Some(false),
        };
        let json = serde_json::to_string(&request).expect("serialize");
        assert!(json.contains("\"queue_timeout_ms\":250"));
        assert!(json.contains("\"wait\":false"));
        let parsed: Request = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            parsed,
            Request::Send {
                queue_timeout_ms: Some(250),
                write_timeout_ms: Some(0),
                wait: Some(false),
                ..
            }
        ));
    }

    #[test]
    fn message_status_event_round_trips() {
        let event = Event::MessageStatus {
            agent: "dev".to_string(),
            id: 1,
            status: DeliveryStatus::Writing,
            at: 5,
        };
        let json = serde_json::to_string(&event).expect("serialize");
        assert!(json.contains("\"event\":\"message_status\""));
        let parsed: Event = serde_json::from_str(&json).expect("deserialize");
        assert!(matches!(
            parsed,
            Event::MessageStatus {
                id: 1,
                status: DeliveryStatus::Writing,
                ..
            }
        ));
    }
}
