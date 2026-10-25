//! Bounded output buffer with ANSI-aware line extraction.

use std::collections::VecDeque;

/// Default retained bytes per agent (256 KiB).
pub const DEFAULT_CAPACITY: usize = 256 * 1024;

/// Ring buffer holding the most recent terminal output of one agent.
#[derive(Debug)]
pub struct OutputBuffer {
    data: VecDeque<u8>,
    capacity: usize,
    truncated: bool,
}

impl OutputBuffer {
    /// Create a buffer retaining at most `capacity` bytes.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity.min(64 * 1024)),
            capacity: capacity.max(1024),
            truncated: false,
        }
    }

    /// Append a chunk, dropping the oldest bytes on overflow.
    pub fn push(&mut self, chunk: &[u8]) {
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
