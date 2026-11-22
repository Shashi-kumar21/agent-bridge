//! Endpoint access-control tests.
//!
//! The endpoint is the only authorization boundary (see `SECURITY.md`), so the
//! mode bits on Unix and the DACL on Windows are part of the product, not an
//! implementation detail. This is its own test binary because the endpoint is
//! selected through process-wide environment variables.

use std::path::PathBuf;

#[cfg(unix)]
use agent_bridge_core::paths;
use agent_bridge_core::transport;

/// Unique state directory for this binary's single test.
fn state_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agent-bridge-perm-{tag}-{}-{}",
        std::process::id(),
        agent_bridge_core::now_ms()
    ));
    std::fs::create_dir_all(&dir).expect("state dir");
    std::env::set_var("AGENT_BRIDGE_HOME", &dir);
    dir
}

/// A permissive directory is tightened, not trusted, by `ensure_base_dir`.
///
/// Connecting needs no account of its own, so the socket mode is the only
/// thing keeping other users out; this asserts both the directory and the
/// socket are owner-only after the daemon binds, and that a same-user client
/// can still connect through the hardened endpoint.
#[cfg(unix)]
#[test]
fn unix_endpoint_is_owner_only() {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let dir = state_dir("unix");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("widen dir");

    paths::ensure_base_dir().expect("ensure_base_dir");

    let dir_mode = std::fs::metadata(&dir)
        .expect("dir metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        dir_mode, 0o700,
        "the state directory must be owner-only; a pre-existing 0755 directory must be tightened"
    );

    let listener = transport::create_listener().expect("bind socket");
    let socket = paths::socket_path();
    let metadata = std::fs::metadata(&socket).expect("socket metadata");

    assert!(
        metadata.file_type().is_socket(),
        "{} must be a Unix socket",
        socket.display()
    );
    let socket_mode = metadata.permissions().mode() & 0o777;
    assert_eq!(
        socket_mode, 0o600,
        "the socket must be owner-only, not merely hidden by the directory mode"
    );

    // The hardened endpoint is still usable by the owner: a refused connection
    // here would mean the chmod broke the transport instead of securing it.
    drop(transport::connect().expect("same-user connect"));

    drop(listener);
    std::fs::remove_dir_all(&dir).ok();
}

/// The Windows pipe carries the explicit DACL and belongs to exactly one
/// listener, so a second process cannot shadow the daemon's endpoint.
///
/// Cross-user denial is not asserted here: it would need a second account and
/// a Win32 ACL query, neither of which is available without new dependencies
/// or `unsafe` (see `SECURITY.md` known gaps). The DACL string itself is
/// asserted in `transport.rs`'s unit tests.
#[cfg(windows)]
#[test]
fn windows_pipe_is_exclusive_and_usable() {
    let dir = state_dir("windows");
    std::env::set_var(
        "AGENT_BRIDGE_PIPE",
        format!(
            "agent-bridge-perm-{}-{}",
            std::process::id(),
            agent_bridge_core::now_ms()
        ),
    );

    assert!(
        transport::endpoint().starts_with(r"\\.\pipe\"),
        "the endpoint must stay a namespaced pipe, not a filesystem path"
    );

    let listener = transport::create_listener().expect("bind pipe");
    assert!(
        transport::create_listener().is_err(),
        "a second listener on the same pipe name must fail instead of shadowing the daemon"
    );
    drop(transport::connect().expect("same-user connect"));

    drop(listener);
    std::fs::remove_dir_all(&dir).ok();
}
