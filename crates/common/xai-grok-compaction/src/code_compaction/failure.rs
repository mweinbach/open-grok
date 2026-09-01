//! Deterministic-vs-transient failure classification for compaction
//! LLM calls.
//!
//! The *policy* lives here (shared across harnesses); the per-harness error
//! types and their wrapping (e.g. grok-build's `SamplingError` →
//! `CompactFailure(acp::Error)`) stay in thin host wrappers that delegate the
//! status/message decisions to these functions.

/// Whether a compaction-call failure is worth retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Retrying the same payload will hit the same failure — the retry loop
    /// should bail without sleeping or re-issuing.
    Deterministic,
    /// Failure may resolve on retry (network blips, 5xx, rate limits).
    Transient,
}

impl FailureKind {
    /// `true` for [`FailureKind::Deterministic`].
    pub fn is_deterministic(self) -> bool {
        matches!(self, Self::Deterministic)
    }
}

/// True when an error message indicates a context-window overflow. Backends report
/// this inconsistently with no stable error code, so we match the message text; it's
/// deterministic (re-sending the same payload always fails), so callers must not retry.
pub fn is_context_length_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("too long for this model")
        || message.contains("prompt is too long")
        || message.contains("maximum prompt length")
        || message.contains("maximum context length")
        || message.contains("maximum allowed number of bytes")
        || (message.contains("current message") && message.contains("exceeds budget"))
        || is_anchored(&message, "request too large")
        || has_size_slug(&message)
        || has_rendered_413_phrase(&message)
        || has_input_length_pair(&message)
}
fn is_anchored(message: &str, needle: &str) -> bool {
    message
        .split(": ")
        .any(|segment| segment.starts_with(needle))
}
fn has_size_slug(message: &str) -> bool {
    [
        "context_length_exceeded",
        "exceed_context_size_error",
        "payload_too_large",
    ]
    .iter()
    .any(|slug| is_anchored(message, slug))
}
fn has_rendered_413_phrase(message: &str) -> bool {
    message.contains("413 payload too large")
        || message.contains("413 content too large")
        || message.contains("413 request entity too large")
}
fn has_input_length_pair(message: &str) -> bool {
    message.contains("input length") && message.contains("exceeds the maximum allowed length")
}

/// Classify an HTTP API failure (status + message) for the compaction retry
/// loop.
///
/// 4xx responses other than 408 (timeout) and 429 (rate limit) are
/// deterministic; a context-length overflow message is deterministic
/// regardless of status (backends sometimes dress it as a synthesized 500).
/// Everything else (5xx, 408, 429) is transient.
pub fn classify_http_status(status: u16, message: &str) -> FailureKind {
    if is_context_length_error(message)
        || ((400..500).contains(&status) && status != 408 && status != 429)
    {
        FailureKind::Deterministic
    } else {
        FailureKind::Transient
    }
}

/// Classify a provider-style stream error event (`ResponseError` /
/// `ResponseFailed.error`) for the compaction retry loop.
///
/// `code` is the structured `code` field on the event (typically a numeric
/// HTTP status as a string, but some providers also use error-type strings like
/// `"invalid_request_error"`). `message` is the human-readable detail.
///
/// Numeric codes are classified by HTTP-status range. The
/// `invalid_request_error` marker, which can appear in either field, always
/// maps to `Deterministic` (schema violations cannot be fixed by re-sending
/// the same payload). The check order is semantic — marker, then numeric
/// code, then context-length message, then default-to-transient.
pub fn classify_stream_event_error(code: Option<&str>, message: &str) -> FailureKind {
    if matches!(code, Some("invalid_request_error")) || message.contains("invalid_request_error") {
        return FailureKind::Deterministic;
    }

    if let Some(status_code) = code.and_then(|c| c.parse::<u16>().ok())
        && (400..500).contains(&status_code)
        && status_code != 408
        && status_code != 429
    {
        return FailureKind::Deterministic;
    }

    // Size overflow arrives here with no parseable code (`code="none"`); the
    // message is the only signal that re-sending cannot help.
    if is_context_length_error(message) {
        return FailureKind::Deterministic;
    }

    FailureKind::Transient
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det_status(status: u16) -> bool {
        classify_http_status(status, "test").is_deterministic()
    }

    #[test]
    fn http_4xx_is_deterministic_except_408_and_429() {
        assert!(det_status(400));
        assert!(det_status(401));
        assert!(det_status(403));
        assert!(det_status(404));
        assert!(det_status(413));
        assert!(!det_status(408));
        assert!(!det_status(429));
        assert!(!det_status(500));
        assert!(!det_status(502));
        assert!(!det_status(503));
    }

    #[test]
    fn http_500_with_context_length_message_is_deterministic() {
        // The sampler synthesizes status=500 from a streamed size overflow, so
        // status alone reads transient; the message must still short-circuit.
        assert!(
            classify_http_status(
                500,
                "API error (status 500 Internal Server Error): \
                 The prompt is too long for this model's context window."
            )
            .is_deterministic()
        );
    }

    #[test]
    fn stream_event_invalid_request_error_marker_is_deterministic() {
        assert!(
            classify_stream_event_error(
                Some("invalid_request_error"),
                "messages.27.content.1: ..."
            )
            .is_deterministic()
        );
        assert!(
            classify_stream_event_error(
                Some("400"),
                "Provider returned invalid_request_error: messages.X..."
            )
            .is_deterministic()
        );
        assert!(
            classify_stream_event_error(None, "messages.X.content.Y: invalid_request_error: ...")
                .is_deterministic()
        );
    }

    #[test]
    fn stream_event_numeric_codes_match_http_classification() {
        let det = |c: &str| classify_stream_event_error(Some(c), "msg").is_deterministic();
        assert!(det("400"));
        assert!(det("401"));
        assert!(det("403"));
        assert!(det("404"));
        assert!(!det("408"));
        assert!(!det("429"));
        assert!(!det("500"));
        assert!(!det("503"));
    }

    #[test]
    fn stream_event_unknown_code_defaults_to_transient() {
        assert!(!classify_stream_event_error(None, "msg").is_deterministic());
        assert!(!classify_stream_event_error(Some("error"), "msg").is_deterministic());
        assert!(!classify_stream_event_error(Some("overloaded_error"), "msg").is_deterministic());
    }

    #[test]
    fn stream_event_context_length_message_is_deterministic() {
        assert!(
            classify_stream_event_error(
                None,
                "The prompt is too long for this model's context window."
            )
            .is_deterministic()
        );
    }

    #[test]
    fn context_length_error_matches_known_messages() {
        for msg in [
            "The text is too long for this model.",
            "The prompt is too long for this model's context window.",
            "prompt is too long: 250000 tokens > 200000 maximum",
            "exceeds the maximum prompt length",
            "This model's maximum context length is 128000 tokens",
            "error code: context_length_exceeded",
            "Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "compact failed: API error (status 400 Bad Request): invalid-argument: Failed to start sampling: [conversation] Current message (1000000 tokens) exceeds budget (500000 tokens)",
            "Current message (600000) exceeds budget (500000)",
            "API error (status 413 Payload Too Large): Request failed (HTTP 413).",
            "API error (status 413 Payload Too Large): payload_too_large: Chat history exceeds the 800-message limit",
            "API error (status 400 Bad Request): exceed_context_size_error: request (300000 tokens) exceeds the model context size",
            "stream error (BAD_REQUEST): Input length (300000 tokens) exceeds the maximum allowed length (200000 tokens)",
            "API error (status 413 Content Too Large): Request failed (HTTP 413).",
            "upstream returned 413 Request Entity Too Large",
            "request exceeds the maximum allowed number of bytes (10485760)",
            "Request too large",
            "compact failed: 413: Request too large",
            "API error (status 429 Too Many Requests): Request too large for model",
        ] {
            assert!(is_context_length_error(msg), "should match: {msg}");
        }
        for msg in [
            "internal server error",
            "rate limited",
            "connection reset by peer",
            "Attached file content (300000 tokens) causes message to exceed budget",
            "compact index estimate 2.0 GB exceeds budget 1.0 GB",
            "API error (status 400 Bad Request): invalid tool schema",
            "metadata value exceeds the maximum allowed length (512 characters)",
            "invalid_request_error: field description says request too large sometimes",
            "invalid_request_error: user note says the payload too large banner is confusing",
            "invalid_request_error: field description mentions a content too large warning",
            "invalid_request_error: docs mention a request entity too large response",
            "invalid_request_error: user asked what context_length_exceeded means",
            "invalid_request_error: docs mention the payload_too_large code",
            "invalid_request_error: line 413 of the doc mentions payload too large limits",
        ] {
            assert!(!is_context_length_error(msg), "should not match: {msg}");
        }
    }
}
