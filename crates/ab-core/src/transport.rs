//! Cross-platform local socket transport (Unix socket / Windows named pipe).

use std::io;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{Listener, ListenerOptions, Stream};

/// Pipe name used on Windows (namespaced so it never touches the filesystem).
#[cfg(windows)]
fn pipe_name() -> String {
    if let Ok(custom) = std::env::var("AGENT_BRIDGE_PIPE") {
        return custom;
    }
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
    let sanitized: String = user
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("agent-bridge-{sanitized}")
}

/// Human readable endpoint description.
#[must_use]
pub fn endpoint() -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\{}", pipe_name())
    }
    #[cfg(unix)]
    {
        crate::paths::socket_display()
    }
}

/// Bind the daemon endpoint.
pub fn create_listener() -> io::Result<Listener> {
    #[cfg(windows)]
    {
        let name = pipe_name();
        let name = name
            .as_str()
            .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?;
        ListenerOptions::new().name(name).create_sync()
    }
    #[cfg(unix)]
    {
        let path = crate::paths::socket_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        let name = path
            .as_path()
            .to_fs_name::<interprocess::local_socket::GenericFilePath>()?;
        let listener = ListenerOptions::new().name(name).create_sync();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if listener.is_ok() {
                // Only the owning user may connect.
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        listener
    }
}

/// Connect to the daemon endpoint.
pub fn connect() -> io::Result<Stream> {
    #[cfg(windows)]
    {
        let name = pipe_name();
        let name = name
            .as_str()
            .to_ns_name::<interprocess::local_socket::GenericNamespaced>()?;
        Stream::connect(name)
    }
    #[cfg(unix)]
    {
        let path = crate::paths::socket_path();
        let name = path
            .as_path()
            .to_fs_name::<interprocess::local_socket::GenericFilePath>()?;
        Stream::connect(name)
    }
}
