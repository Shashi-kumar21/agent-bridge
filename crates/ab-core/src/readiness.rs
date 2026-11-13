//! Heuristic input-readiness detection from output quiescence.
//!
//! This is deliberately a heuristic, not a guarantee: a long-running command
//! that prints nothing looks the same as a process waiting at a prompt.

use std::time::Duration;

use crate::protocol::{Millis, Readiness};

/// Environment variable for the quiet period in milliseconds.
pub const QUIET_ENV: &str = "AGENT_BRIDGE_QUIET_MS";
/// Environment variable for an optional literal prompt suffix.
pub const PROMPT_ENV: &str = "AGENT_BRIDGE_PROMPT_PATTERN";

/// Default quiet period before an agent is considered idle.
pub const DEFAULT_QUIET_MS: u64 = 500;
/// Shortest accepted quiet period.
pub const MIN_QUIET_MS: u64 = 50;
/// Longest accepted quiet period.
pub const MAX_QUIET_MS: u64 = 60_000;
/// Longest accepted prompt suffix, in characters.
pub const MAX_PROMPT_CHARS: usize = 128;
/// How many trailing bytes are scanned for the prompt suffix.
pub const PROMPT_SCAN_BYTES: usize = 4096;

/// Parse a quiet-period override, falling back to the default.
fn quiet_from_value(raw: Option<&str>) -> u64 {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_QUIET_MS)
}

/// Per-agent readiness configuration, sampled when the agent is spawned.
#[derive(Debug, Clone)]
pub struct ReadinessConfig {
    quiet: Duration,
    prompt: Option<String>,
}

impl Default for ReadinessConfig {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(DEFAULT_QUIET_MS),
            prompt: None,
        }
    }
}

impl ReadinessConfig {
    /// Build a config, clamping the quiet period and trimming the prompt.
    #[must_use]
    pub fn new(quiet_ms: u64, prompt: Option<String>) -> Self {
        let quiet = Duration::from_millis(quiet_ms.clamp(MIN_QUIET_MS, MAX_QUIET_MS));
        let prompt = prompt.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.chars().take(MAX_PROMPT_CHARS).collect())
            }
        });
        Self { quiet, prompt }
    }

    /// Read `AGENT_BRIDGE_QUIET_MS` and `AGENT_BRIDGE_PROMPT_PATTERN`.
    ///
    /// Unparsable values fall back to the default; out-of-range values are
    /// clamped to 50..60000 ms by [`Self::new`].
    #[must_use]
    pub fn from_env() -> Self {
        let quiet_ms = quiet_from_value(std::env::var(QUIET_ENV).ok().as_deref());
        Self::new(quiet_ms, std::env::var(PROMPT_ENV).ok())
    }

    /// Configured quiet period.
    #[must_use]
    pub fn quiet(&self) -> Duration {
        self.quiet
    }

    /// Optional literal suffix that marks the last output line as a prompt.
    #[must_use]
    pub fn prompt(&self) -> Option<&str> {
        self.prompt.as_deref()
    }

    /// Classify one observation.
    ///
    /// A stopped agent and an agent without a visible output line are
    /// `unknown` - terminal control sequences alone are not output. A
    /// configured prompt suffix on the newest line is an early `idle` signal;
    /// otherwise output newer than the quiet period is `busy` and older output
    /// is `idle`.
    #[must_use]
    pub fn classify(
        &self,
        running: bool,
        last_output_at: Option<Millis>,
        last_line: Option<&str>,
        now: Millis,
    ) -> Readiness {
        if !running {
            return Readiness::Unknown;
        }
        let Some(line) = last_line else {
            return Readiness::Unknown;
        };
        if let Some(prompt) = self.prompt.as_deref() {
            if line.trim_end().ends_with(prompt) {
                return Readiness::Idle;
            }
        }
        let Some(at) = last_output_at else {
            return Readiness::Unknown;
        };
        let quiet_ms = u64::try_from(self.quiet.as_millis()).unwrap_or(u64::MAX);
        if now.saturating_sub(at) >= quiet_ms {
            Readiness::Idle
        } else {
            Readiness::Busy
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReadinessConfig, MAX_PROMPT_CHARS, MIN_QUIET_MS};
    use crate::protocol::Readiness;

    fn config(quiet_ms: u64) -> ReadinessConfig {
        ReadinessConfig::new(quiet_ms, None)
    }

    #[test]
    fn unknown_without_a_process_or_visible_output() {
        let config = config(500);
        assert_eq!(
            config.classify(false, Some(1), Some("done"), 1_000),
            Readiness::Unknown
        );
        assert_eq!(config.classify(true, None, None, 1_000), Readiness::Unknown);
        assert_eq!(
            config.classify(true, Some(999), None, 1_000),
            Readiness::Unknown,
            "control bytes alone do not make an agent busy"
        );
    }

    #[test]
    fn recent_output_is_busy_and_quiet_output_is_idle() {
        let config = config(500);
        assert_eq!(
            config.classify(true, Some(900), Some("building"), 1_000),
            Readiness::Busy
        );
        assert_eq!(
            config.classify(true, Some(500), Some("building"), 1_000),
            Readiness::Idle
        );
        assert_eq!(
            config.classify(true, Some(1), Some("building"), 1_000),
            Readiness::Idle
        );
    }

    #[test]
    fn prompt_suffix_marks_idle_before_the_quiet_period() {
        let config = ReadinessConfig::new(10_000, Some("READY".to_string()));
        assert_eq!(
            config.classify(true, Some(999), Some("build READY"), 1_000),
            Readiness::Idle
        );
        assert_eq!(
            config.classify(true, Some(999), Some("still building"), 1_000),
            Readiness::Busy
        );
    }

    #[test]
    fn unparsable_quiet_periods_fall_back_to_the_default() {
        use super::{quiet_from_value, DEFAULT_QUIET_MS};
        assert_eq!(quiet_from_value(None), DEFAULT_QUIET_MS);
        assert_eq!(quiet_from_value(Some("junk")), DEFAULT_QUIET_MS);
        assert_eq!(quiet_from_value(Some(" 2000 ")), 2_000);
        assert_eq!(quiet_from_value(Some("-5")), DEFAULT_QUIET_MS);
    }

    #[test]
    fn quiet_period_is_clamped_and_prompts_are_trimmed() {
        let config = ReadinessConfig::new(1, Some("  >  ".to_string()));
        assert_eq!(config.quiet().as_millis(), u128::from(MIN_QUIET_MS));
        assert_eq!(config.prompt(), Some(">"));
        let empty = ReadinessConfig::new(500, Some("   ".to_string()));
        assert_eq!(empty.prompt(), None);
        let long = ReadinessConfig::new(500, Some("x".repeat(MAX_PROMPT_CHARS + 10)));
        assert_eq!(
            long.prompt().map(str::len),
            Some(MAX_PROMPT_CHARS),
            "the suffix is bounded"
        );
    }
}
