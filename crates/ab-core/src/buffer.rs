//! Bounded output buffer with ANSI-aware line extraction and byte cursors.

use std::collections::VecDeque;

use crate::protocol::Offset;

/// Default retained bytes per agent (256 KiB).
pub const DEFAULT_CAPACITY: usize = 256 * 1024;
/// Environment variable for the per-agent retained output size.
pub const CAPACITY_ENV: &str = "AGENT_BRIDGE_BUFFER_BYTES";
/// Shortest accepted retained-output size.
pub const MIN_CAPACITY: usize = 1024;
/// Longest accepted retained-output size (16 MiB).
pub const MAX_CAPACITY: usize = 16 * 1024 * 1024;

/// Clamp a requested buffer size into the documented range.
#[must_use]
pub fn clamp_capacity(capacity: usize) -> usize {
    capacity.clamp(MIN_CAPACITY, MAX_CAPACITY)
}

/// Parse a capacity override, falling back to the default.
fn capacity_from_value(raw: Option<&str>) -> usize {
    raw.and_then(|raw| raw.trim().parse::<usize>().ok())
        .map_or(DEFAULT_CAPACITY, clamp_capacity)
}

/// Read `AGENT_BRIDGE_BUFFER_BYTES`, falling back to the default.
///
/// Invalid values fall back to the default; out-of-range values are clamped.
#[must_use]
pub fn capacity_from_env() -> usize {
    capacity_from_value(std::env::var(CAPACITY_ENV).ok().as_deref())
}

/// One bounded slice of the retained output, addressed by absolute byte offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorChunk {
    /// Absolute offset of the first returned byte.
    pub start_offset: Offset,
    /// Raw bytes in `start_offset..next_offset`.
    pub data: Vec<u8>,
    /// Absolute offset just past the last returned byte.
    pub next_offset: Offset,
    /// True when the requested offset had already rotated out, so the chunk
    /// starts at the oldest retained byte instead.
    pub dropped: bool,
}

/// Ring buffer holding the most recent terminal output of one agent.
#[derive(Debug)]
pub struct OutputBuffer {
    data: VecDeque<u8>,
    capacity: usize,
    truncated: bool,
    /// Total bytes ever pushed, including bytes that have rotated out.
    total: u64,
}

impl OutputBuffer {
    /// Create a buffer retaining at most `capacity` bytes.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity.min(64 * 1024)),
            capacity: clamp_capacity(capacity),
            truncated: false,
            total: 0,
        }
    }

    /// Append a chunk, dropping the oldest bytes on overflow.
    pub fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        self.data.extend(chunk.iter().copied());
        while self.data.len() > self.capacity {
            let overflow = self.data.len() - self.capacity;
            self.data.drain(..overflow);
            self.truncated = true;
        }
    }

    /// Raw buffered bytes.
    #[must_use]
    pub fn bytes(&self) -> Vec<u8> {
        self.data.iter().copied().collect()
    }

    /// Number of retained bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Whether older content was rotated out.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Absolute offset of the oldest retained byte.
    ///
    /// Grows with every byte pushed; the difference between this and
    /// [`Self::end_offset`] is [`Self::len`].
    #[must_use]
    pub fn start_offset(&self) -> Offset {
        self.total.saturating_sub(self.len() as Offset)
    }

    /// Absolute offset just past the newest retained byte.
    #[must_use]
    pub fn end_offset(&self) -> Offset {
        self.total
    }

    /// Read at most `max_bytes` from `since` onwards.
    ///
    /// Returns `None` when `since` is ahead of the newest byte: such a cursor
    /// does not belong to this stream (offsets reset when the daemon or agent
    /// restarts). A cursor older than the retained window is clamped to the
    /// oldest retained byte and reported with [`CursorChunk::dropped`], so the
    /// caller can tell that a gap was skipped. An empty buffer with
    /// `since == 0` returns an empty chunk.
    #[must_use]
    pub fn read_from(&self, since: Offset, max_bytes: usize) -> Option<CursorChunk> {
        if since > self.total {
            return None;
        }
        let start = since.max(self.start_offset());
        let skip = usize::try_from(start - self.start_offset()).ok()?;
        let take = self.data.len().saturating_sub(skip).min(max_bytes);
        let data: Vec<u8> = self.data.iter().skip(skip).take(take).copied().collect();
        Some(CursorChunk {
            start_offset: start,
            next_offset: start + data.len() as Offset,
            dropped: since < self.start_offset(),
            data,
        })
    }

    /// Last `n` lines with ANSI escapes removed.
    #[must_use]
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        let text = strip_ansi(&String::from_utf8_lossy(&self.bytes()));
        let mut lines: Vec<String> = text
            .split('\n')
            .map(|line| line.trim_end_matches('\r').to_string())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        if n >= lines.len() {
            lines
        } else {
            lines.split_off(lines.len() - n)
        }
    }

    /// Newest non-empty line with ANSI escapes removed.
    ///
    /// Scans at most `limit` trailing bytes so a configured prompt suffix does
    /// not require stripping the whole retained buffer.
    #[must_use]
    pub fn last_line(&self, limit: usize) -> Option<String> {
        let start = self.data.len().saturating_sub(limit);
        let tail: Vec<u8> = self.data.iter().skip(start).copied().collect();
        let text = strip_ansi(&String::from_utf8_lossy(&tail));
        text.lines().rev().find_map(|line| {
            let trimmed = line.trim_end_matches(&['\r', '\n'][..]);
            let visible = trimmed.chars().any(|c| !c.is_control());
            if trimmed.trim().is_empty() || !visible {
                None
            } else {
                Some(trimmed.to_string())
            }
        })
    }
}

/// Remove ANSI/VT escape sequences so output is safe to print as text.
///
/// Handles CSI (`ESC [`), OSC (`ESC ]`), DCS (`ESC P`), APC (`ESC _`), PM
/// (`ESC ^`), SOS (`ESC X`) and the two-byte charset designators (`ESC (` ...).
#[must_use]
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for inner in chars.by_ref() {
                    if ('@'..='~').contains(&inner) {
                        break;
                    }
                }
            }
            Some(']' | 'P' | '_' | '^' | 'X') => {
                chars.next();
                while let Some(inner) = chars.next() {
                    if inner == '\u{7}' {
                        break;
                    }
                    if inner == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some('(' | ')' | '*' | '+') => {
                chars.next();
                chars.next();
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{strip_ansi, OutputBuffer};

    #[test]
    fn keeps_only_recent_bytes() {
        let mut buffer = OutputBuffer::new(1024);
        buffer.push(&vec![b'a'; 2048]);
        assert_eq!(buffer.len(), 1024);
        assert!(buffer.truncated());
    }

    #[test]
    fn returns_last_lines() {
        let mut buffer = OutputBuffer::new(DEFAULT_TEST_CAPACITY);
        buffer.push(b"one\ntwo\nthree\n");
        assert_eq!(buffer.tail_lines(2), vec!["two", "three"]);
    }

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
    }

    #[test]
    fn finds_the_newest_visible_line() {
        let mut buffer = OutputBuffer::new(1024);
        buffer.push(b"prompt> \n\n\x1b[32mdone\x1b[0m\r\n\r\n");
        assert_eq!(buffer.last_line(1024).as_deref(), Some("done"));
        buffer.push(b"partial");
        assert_eq!(buffer.last_line(1024).as_deref(), Some("partial"));
        buffer.push(b"\n   \n");
        assert_eq!(buffer.last_line(1024).as_deref(), Some("partial"));
    }

    #[test]
    fn control_bytes_alone_are_not_a_visible_line() {
        let mut buffer = OutputBuffer::new(1024);
        buffer.push(b"\x07\x00\x1b[2J");
        assert_eq!(buffer.last_line(1024), None);
        buffer.push(b"\n\x07done");
        assert_eq!(buffer.last_line(1024).as_deref(), Some("\x07done"));
    }

    #[test]
    fn capacity_values_fall_back_or_clamp() {
        assert_eq!(super::capacity_from_value(None), super::DEFAULT_CAPACITY);
        assert_eq!(
            super::capacity_from_value(Some("junk")),
            super::DEFAULT_CAPACITY
        );
        assert_eq!(super::capacity_from_value(Some(" 4096 ")), 4096);
        assert_eq!(super::capacity_from_value(Some("0")), super::MIN_CAPACITY);
        assert_eq!(
            super::capacity_from_value(Some("999999999")),
            super::MAX_CAPACITY
        );
    }

    #[test]
    fn offsets_advance_across_a_wrap() {
        let mut buffer = OutputBuffer::new(1024);
        buffer.push(&vec![b'a'; 1500]);
        assert_eq!(buffer.end_offset(), 1500);
        assert_eq!(buffer.start_offset(), 476);
        assert_eq!(buffer.len(), 1024);

        let chunk = buffer.read_from(0, 64 * 1024).expect("cursor");
        assert!(chunk.dropped, "offset 0 rotated out");
        assert_eq!(chunk.start_offset, 476);
        assert_eq!(chunk.next_offset, 1500);
        assert_eq!(chunk.data.len(), 1024);
    }

    #[test]
    fn repeated_reads_cover_each_byte_once() {
        let mut buffer = OutputBuffer::new(1024);
        buffer.push(b"abcdef");

        let first = buffer.read_from(0, 3).expect("first chunk");
        assert_eq!(first.data, b"abc");
        assert!(!first.dropped);
        let second = buffer
            .read_from(first.next_offset, 3)
            .expect("second chunk");
        assert_eq!(second.data, b"def");
        assert_eq!(second.next_offset, 6);
        let empty = buffer
            .read_from(second.next_offset, 3)
            .expect("empty chunk");
        assert_eq!(empty.data.len(), 0);
        assert_eq!(empty.next_offset, 6);
        assert!(!empty.dropped);
    }

    #[test]
    fn a_cursor_ahead_of_the_stream_is_rejected() {
        let mut buffer = OutputBuffer::new(1024);
        buffer.push(b"abc");
        assert!(buffer.read_from(4, 16).is_none());
        assert_eq!(buffer.read_from(3, 16).map(|c| c.data), Some(Vec::new()));
    }

    #[test]
    fn capacity_is_clamped() {
        assert_eq!(super::clamp_capacity(1), super::MIN_CAPACITY);
        assert_eq!(super::clamp_capacity(usize::MAX), super::MAX_CAPACITY);
        assert_eq!(super::clamp_capacity(4096), 4096);
    }

    #[test]
    fn strips_dcs_apc_and_charset() {
        // DCS payload, APC payload, charset designator, then plain text.
        assert_eq!(strip_ansi("\u{1b}P1;2|payload\u{1b}\\ok"), "ok");
        assert_eq!(strip_ansi("\u{1b}_apc\u{7}after"), "after");
        assert_eq!(strip_ansi("\u{1b}(Btext"), "text");
        // CSI with private markers must not leak digits.
        assert_eq!(strip_ansi("\u{1b}[?25hvisible"), "visible");
    }

    const DEFAULT_TEST_CAPACITY: usize = 4096;
}
