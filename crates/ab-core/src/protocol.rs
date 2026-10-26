//! Wire protocol between the CLI/app and the daemon.
//!
//! Framing is newline-delimited JSON: one [`Request`] per line from the client,
//! one [`Response`] (or a stream of [`Event`]s for `attach`) per line from the
//! daemon. Binary PTY output travels base64 encoded so the frames stay valid
//! UTF-8 JSON.

use serde::{Deserialize, Serialize};

/// Bumped whenever the request/response shape changes incompatibly.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default terminal width for spawned agents.
pub const DEFAULT_COLS: u16 = 120;

/// Default terminal height for spawned agents.
pub const DEFAULT_ROWS: u16 = 32;

/// Monotonic per-agent output sequence number.
pub type Seq = u64;

/// Milliseconds since the Unix epoch.
pub type Millis = u64;

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
    /// Number of live attach subscribers.
    pub subscribers: usize,
    /// PTY width.
    pub cols: u16,
    /// PTY height.
    pub rows: u16,
}

/// One delivered inter-agent message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Recipient agent name.
    pub target: String,
    /// Optional sender label (`--from`).
    pub from: Option<String>,
    /// Message body as delivered to the PTY.
    pub text: String,
    /// Delivery timestamp (ms since epoch).
    pub at: Millis,
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
        /// Replay buffered output starting after this sequence number.
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
        /// Daemon process id, so callers can target it for teardown.
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
    /// Message accepted and written to the PTY.
    Sent {
        /// Recipient.
        target: String,
        /// Bytes written including the trailing newline.
        bytes: usize,
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
    /// A message was delivered to this agent.
    Message {
        /// Agent name.
        agent: String,
        /// Sender label.
        from: Option<String>,
        /// Message body.
        text: String,
        /// Timestamp.
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
    use super::validate_name;

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
}
