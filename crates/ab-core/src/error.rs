//! Error types shared by the agent-bridge core and CLI.

use crate::protocol::ErrorCode;

/// Result alias used across the crate.
pub type Result<T> = std::result::Result<T, BridgeError>;

/// Every failure mode the bridge can report.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// Underlying OS / socket / PTY failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A frame could not be encoded or decoded.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// The daemon socket could not be reached.
    #[error("daemon is not running (expected socket at {0})")]
    DaemonNotRunning(String),

    /// No agent is registered under that name.
    #[error("agent '{0}' not found")]
    AgentNotFound(String),

    /// Name collision while creating an agent.
    #[error("agent '{0}' already exists")]
    AgentExists(String),

    /// Name violates the naming rules.
    #[error("invalid agent name '{0}': use 1-32 characters of a-z, A-Z, 0-9, '-' or '_'")]
    InvalidName(String),

    /// The agent process has already terminated.
    #[error("agent '{0}' has already exited")]
    AgentExited(String),

    /// The message exceeds the configured size limit.
    #[error("message is {bytes} bytes, limit is {limit} bytes (raise {env})")]
    MessageTooLarge {
        /// Payload size including the trailing carriage return.
        bytes: usize,
        /// Active limit.
        limit: usize,
        /// Environment variable that controls the limit.
        env: &'static str,
    },

    /// No message is registered under that id.
    #[error("message '{0}' not found (ids are per daemon; a restart clears history)")]
    MessageNotFound(u64),

    /// Generic daemon-side failure.
    #[error("{0}")]
    Daemon(String),
}

impl BridgeError {
    /// Stable machine-readable code carried on the wire.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::AgentNotFound(_) | Self::MessageNotFound(_) => ErrorCode::NotFound,
            Self::AgentExists(_) | Self::AgentExited(_) => ErrorCode::Conflict,
            Self::InvalidName(_) | Self::Protocol(_) => ErrorCode::BadRequest,
            Self::MessageTooLarge { .. } => ErrorCode::TooLarge,
            Self::DaemonNotRunning(_) | Self::Daemon(_) => ErrorCode::NotRunning,
            Self::Io(_) => ErrorCode::Internal,
        }
    }
}

impl From<serde_json::Error> for BridgeError {
    fn from(err: serde_json::Error) -> Self {
        Self::Protocol(err.to_string())
    }
}

impl From<anyhow::Error> for BridgeError {
    fn from(err: anyhow::Error) -> Self {
        Self::Daemon(err.to_string())
    }
}
