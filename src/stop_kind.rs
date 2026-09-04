//! Normalisation of a provider's raw `finish_reason` into PromptOn's `stop_kind`.
//!
//! Comparison lowercases and trims, so Google's `STOP` and `MAX_TOKENS` land correctly, and the
//! mapping is idempotent — feeding an already-normalised value back in returns itself, which
//! matters because the server re-normalises whatever the client sent.
//!
//! Two traps worth repeating: Google's `SAFETY` and `RECITATION` map to [`StopKind::Other`], not
//! `ContentFilter` (only the literal string `content_filter` lands there), and `tool_calls` is
//! **not** a truncation — only `length` sets [`StopKind::truncated`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Why the model stopped generating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopKind {
    /// The model finished on its own (`stop`, `end_turn`, `stop_sequence`).
    Stop,
    /// The output hit the token cap (`length`, `max_tokens`) — the answer is cut off.
    Length,
    /// The model asked for a tool (`tool_calls`, `tool_use`, `tool_call`).
    ToolCall,
    /// The provider's content filter fired (`content_filter`).
    ContentFilter,
    /// Anything else, empty, or absent.
    Other,
}

impl StopKind {
    /// Normalises a raw `finish_reason`. `None` and unknown values become [`StopKind::Other`].
    pub fn normalize(finish_reason: Option<&str>) -> StopKind {
        match finish_reason {
            None => StopKind::Other,
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "stop" | "end_turn" | "stop_sequence" => StopKind::Stop,
                "length" | "max_tokens" => StopKind::Length,
                "tool_call" | "tool_calls" | "tool_use" => StopKind::ToolCall,
                "content_filter" => StopKind::ContentFilter,
                _ => StopKind::Other,
            },
        }
    }

    /// Whether the output was cut off. True only for [`StopKind::Length`].
    pub fn truncated(self) -> bool {
        self == StopKind::Length
    }

    /// The wire value (`"stop"`, `"length"`, `"tool_call"`, `"content_filter"`, `"other"`).
    pub fn as_str(self) -> &'static str {
        match self {
            StopKind::Stop => "stop",
            StopKind::Length => "length",
            StopKind::ToolCall => "tool_call",
            StopKind::ContentFilter => "content_filter",
            StopKind::Other => "other",
        }
    }
}

impl fmt::Display for StopKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for StopKind {
    type Err = std::convert::Infallible;

    /// Never fails: an unknown reason is [`StopKind::Other`], exactly like [`StopKind::normalize`].
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(StopKind::normalize(Some(s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_provider_reasons() {
        assert_eq!(StopKind::normalize(Some("end_turn")), StopKind::Stop);
        assert_eq!(StopKind::normalize(Some("MAX_TOKENS")), StopKind::Length);
        assert_eq!(StopKind::normalize(Some("  stop  ")), StopKind::Stop);
        assert_eq!(StopKind::normalize(Some("SAFETY")), StopKind::Other);
        assert_eq!(StopKind::normalize(None), StopKind::Other);
    }

    #[test]
    fn is_idempotent() {
        for kind in [
            StopKind::Stop,
            StopKind::Length,
            StopKind::ToolCall,
            StopKind::ContentFilter,
            StopKind::Other,
        ] {
            assert_eq!(StopKind::normalize(Some(kind.as_str())), kind);
        }
    }

    #[test]
    fn only_length_is_a_truncation() {
        assert!(StopKind::Length.truncated());
        assert!(!StopKind::ToolCall.truncated());
    }
}
