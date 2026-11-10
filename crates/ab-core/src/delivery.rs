//! Delivery bookkeeping shared by the daemon and the CLI.
//!
//! Phase 2A keeps the write path synchronous; this module only owns the parts
//! that both sides need: the message id counter, the message size limit, and
//! the status vocabulary. The bounded writer queue lands in Phase 2B.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::protocol::MessageId;

/// Default maximum payload per `sendmessage` (64 KiB).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Hard cap for `AGENT_BRIDGE_MAX_MESSAGE`.
pub const MAX_MESSAGE_BYTES_CAP: usize = 1024 * 1024;

/// Environment variable that raises or lowers the message limit.
pub const MAX_MESSAGE_ENV: &str = "AGENT_BRIDGE_MAX_MESSAGE";

/// Daemon-wide monotonic source of [`MessageId`] values.
static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate the next message id. Unique for the daemon lifetime.
#[must_use]
pub fn next_message_id() -> MessageId {
    NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
}

/// Current message size limit in bytes.
///
/// Reads [`MAX_MESSAGE_ENV`] once per call and clamps it to
/// `1 KiB ..= 1 MiB`; values that do not parse fall back to the default.
#[must_use]
pub fn max_message_bytes() -> usize {
    parse_max_message_bytes(std::env::var(MAX_MESSAGE_ENV).ok().as_deref())
}

fn parse_max_message_bytes(raw: Option<&str>) -> usize {
    raw.and_then(|value| value.trim().parse::<usize>().ok())
        .map_or(DEFAULT_MAX_MESSAGE_BYTES, |value| {
            value.clamp(1024, MAX_MESSAGE_BYTES_CAP)
        })
}

#[cfg(test)]
mod tests {
    use super::{
        next_message_id, parse_max_message_bytes, DEFAULT_MAX_MESSAGE_BYTES, MAX_MESSAGE_BYTES_CAP,
    };

    #[test]
    fn ids_are_unique_and_monotonic() {
        let first = next_message_id();
        let second = next_message_id();
        assert!(second > first);
    }

    #[test]
    fn parse_clamps_and_falls_back() {
        assert_eq!(parse_max_message_bytes(None), DEFAULT_MAX_MESSAGE_BYTES);
        assert_eq!(
            parse_max_message_bytes(Some("nope")),
            DEFAULT_MAX_MESSAGE_BYTES
        );
        assert_eq!(parse_max_message_bytes(Some(" 2048 ")), 2048);
        assert_eq!(parse_max_message_bytes(Some("1")), 1024);
        assert_eq!(
            parse_max_message_bytes(Some("99999999")),
            MAX_MESSAGE_BYTES_CAP
        );
    }
}
