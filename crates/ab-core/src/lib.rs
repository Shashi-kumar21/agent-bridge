//! # agent-bridge-core
//!
//! Cross-platform "session bus" for terminal agents.
//!
//! A single daemon owns every agent terminal through a PTY (`ConPTY` on
//! Windows, `openpty` on Unix), exposes a local socket, and lets any client:
//!
//! * enumerate live agents ([`protocol::Request::List`])
//! * type a message into another agent's terminal
//!   ([`protocol::Request::Send`])
//! * replay or stream its output ([`protocol::Request::Read`],
//!   [`protocol::Request::Attach`])
//!
//! No `tmux`, no X server, no platform specific shell integration required.

pub mod agent;
pub mod buffer;
pub mod client;
pub mod daemon;
pub mod error;
pub mod paths;
pub mod protocol;
pub mod reaper;
pub mod transport;

pub use error::{BridgeError, Result};
pub use protocol::{
    AgentInfo, AgentStatus, ErrorCode, Event, Message, Request, Response, PROTOCOL_VERSION,
};

/// Current wall clock in milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> protocol::Millis {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |delta| {
            protocol::Millis::try_from(delta.as_millis()).unwrap_or(protocol::Millis::MAX)
        })
}
