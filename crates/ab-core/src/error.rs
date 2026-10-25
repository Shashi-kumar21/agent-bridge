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

    /// Generic daemon-side failure.
    #[error("{0}")]
    Daemon(String),
}

impl BridgeError {
    /// Stable machine-readable code carried on the wire.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::AgentNotFound(_) => ErrorCode::NotFound,
            Self::AgentExists(_) | Self::AgentExited(_) => ErrorCode::Conflict,
            Self::InvalidName(_) | Self::Protocol(_) => ErrorCode::BadRequest,
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
