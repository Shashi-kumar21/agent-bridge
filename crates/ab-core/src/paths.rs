//! Filesystem locations used by the daemon and the CLI.

use std::path::PathBuf;

/// Directory (inside the user's home) that holds runtime state.
pub const APP_DIR: &str = ".agent-bridge";

/// Environment variable that overrides the state directory.
pub const HOME_ENV: &str = "AGENT_BRIDGE_HOME";

fn home_dir() -> PathBuf {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .or_else(|| std::env::var_os("HOME"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// Root directory for daemon state, logs and the local socket.
#[must_use]
pub fn base_dir() -> PathBuf {
    if let Some(custom) = std::env::var_os(HOME_ENV) {
        pathbuf(custom)
    } else {
        home_dir().join(APP_DIR)
    }
}

fn pathbuf(value: std::ffi::OsString) -> PathBuf {
    PathBuf::from(value)
}

/// Local socket path (used directly on Unix, mapped to a named pipe on Windows).
#[must_use]
pub fn socket_path() -> PathBuf {
    base_dir().join("daemon.sock")
}

/// Human readable socket description for logs and `--json` output.
#[must_use]
pub fn socket_display() -> String {
    socket_path().display().to_string()
}

/// Daemon log file written when the daemon is started in the background.
#[must_use]
pub fn log_path() -> PathBuf {
    base_dir().join("daemon.log")
}

/// Create the state directory if it does not exist yet.
pub fn ensure_base_dir() -> std::io::Result<()> {
    let dir = base_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Runtime state (socket, logs) is private to the OS user.
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}
