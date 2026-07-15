//! 401 attribution: callback hook + shared helpers for tool HTTP clients.

use std::sync::Arc;

/// Bearer prefix length shared across crate boundaries.
pub const SENT_BEARER_PREFIX_LEN: usize = 12;

/// Which tool endpoint produced the 401.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConsumer {
    ImageGen,
    VideoGenStart,
    VideoGenPoll,
    WebSearch,
}

impl ToolConsumer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ImageGen => "ImageGen",
            Self::VideoGenStart => "VideoGen.start",
            Self::VideoGenPoll => "VideoGen.poll",
            Self::WebSearch => "WebSearch",
        }
    }
}

/// 401 attribution callback. Shell wires this to emit telemetry.
pub trait Auth401AttributionCallback: Send + Sync + std::fmt::Debug {
    /// `sent_bearer_prefix` is truncated to [`SENT_BEARER_PREFIX_LEN`]
    /// before crossing this boundary. `None` = no bearer was sent.
    fn record_401(&self, consumer: ToolConsumer, sent_bearer_prefix: Option<&str>);
}

/// Shared, cheap-to-clone alias for the attribution callback.
pub type SharedAttributionCallback = Arc<dyn Auth401AttributionCallback>;

/// Record a 401 attribution event if a callback is wired. Truncates
/// the bearer to [`SENT_BEARER_PREFIX_LEN`] before crossing the
/// trait boundary.
pub(crate) fn emit_401(
    callback: Option<&SharedAttributionCallback>,
    consumer: ToolConsumer,
    sent_bearer: Option<&str>,
) {
    if let Some(cb) = callback {
        let prefix = sent_bearer.map(|s| truncate_to_prefix(s.to_string()));
        cb.record_401(consumer, prefix.as_deref());
    }
}

/// Truncate a bearer string to the first [`SENT_BEARER_PREFIX_LEN`]
/// characters. Used by tool clients before passing the bearer across
/// the [`Auth401AttributionCallback`] boundary.
///
/// Bearer tokens are ASCII (per the `Authorization` header grammar)
/// so the byte index is always a char boundary; this function uses
/// `String::truncate` which would otherwise panic on a non-boundary
/// cut.
pub(crate) fn truncate_to_prefix(mut bearer: String) -> String {
    bearer.truncate(SENT_BEARER_PREFIX_LEN.min(bearer.len()));
    bearer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_to_prefix_long_string_cuts_at_12() {
        assert_eq!(
            truncate_to_prefix("xai-key-aaaaaaaaaaaaaaaaaaa".to_string()),
            "xai-key-aaaa"
        );
    }

    #[test]
    fn truncate_to_prefix_short_string_unchanged() {
        assert_eq!(truncate_to_prefix("abc".to_string()), "abc");
    }

    #[test]
    fn truncate_to_prefix_exact_12_unchanged() {
        assert_eq!(
            truncate_to_prefix("123456789012".to_string()),
            "123456789012"
        );
        assert_eq!(truncate_to_prefix("123456789012".to_string()).len(), 12);
    }

    #[test]
    fn truncate_to_prefix_empty_unchanged() {
        assert_eq!(truncate_to_prefix(String::new()), "");
    }

    #[test]
    fn tool_consumer_as_str_stable_identifiers() {
        assert_eq!(ToolConsumer::ImageGen.as_str(), "ImageGen");
        assert_eq!(ToolConsumer::VideoGenStart.as_str(), "VideoGen.start");
        assert_eq!(ToolConsumer::VideoGenPoll.as_str(), "VideoGen.poll");
        assert_eq!(ToolConsumer::WebSearch.as_str(), "WebSearch");
    }
}
