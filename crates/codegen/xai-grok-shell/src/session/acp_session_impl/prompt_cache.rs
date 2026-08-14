//! Main-turn prompt-cache recording and log emission.

use super::*;
use xai_grok_sampling_types::{TokenUsage, format_cache_hit_rate};

impl SessionActor {
    /// Fingerprint the request that just completed, fold provider cache
    /// numbers, and log a break or provider miss. Auxiliary calls must not
    /// use this — they would poison the next main-turn prefix.
    pub(crate) fn record_and_log_prompt_cache(
        &self,
        request: &ConversationRequest,
        usage: &TokenUsage,
        cache_key_forwarded: bool,
        loop_index: u32,
    ) {
        let mut tracker = self.prompt_cache.lock();
        let diff = tracker.record_request(request);
        let outcome = tracker.record_usage(usage, cache_key_forwarded, "turn", Some(loop_index));
        drop(tracker);

        let session_id = self.session_info.id.0.as_ref();
        let payload = serde_json::json!({
            "loop_index": loop_index,
            "prompt_tokens": outcome.prompt_tokens,
            "cached_prompt_tokens": outcome.cached_tokens,
            "hit_rate_percent": outcome.hit_rate,
            "cache_key_forwarded": cache_key_forwarded,
            "prefix": outcome.diff.summary(),
            "provider_miss": outcome.provider_miss,
            "break_section": outcome.diff.break_section(),
        });
        xai_grok_telemetry::unified_log::info(
            "shell.turn.prompt_cache",
            Some(session_id),
            Some(payload.clone()),
        );
        tracing::info!(
            target: SESSION_LOG,
            loop_index,
            cached_prompt_tokens = outcome.cached_tokens,
            prompt_tokens = outcome.prompt_tokens,
            hit_rate = format_cache_hit_rate(outcome.hit_rate),
            cache_key_forwarded,
            prefix = %outcome.diff.summary(),
            "prompt cache"
        );

        if outcome.diff.is_break() {
            xai_grok_telemetry::unified_log::info(
                "shell.turn.prompt_cache_break",
                Some(session_id),
                Some(payload),
            );
            tracing::info!(
                target: SESSION_LOG,
                loop_index,
                section = outcome.diff.break_section().unwrap_or("unknown"),
                prefix = %diff.summary(),
                hit_rate = format_cache_hit_rate(outcome.hit_rate),
                cached_prompt_tokens = outcome.cached_tokens,
                prompt_tokens = outcome.prompt_tokens,
                "prompt cache prefix broke"
            );
        } else if outcome.provider_miss {
            xai_grok_telemetry::unified_log::info(
                "shell.turn.prompt_cache_miss",
                Some(session_id),
                Some(payload),
            );
            tracing::info!(
                target: SESSION_LOG,
                loop_index,
                prefix = %outcome.diff.summary(),
                prompt_tokens = outcome.prompt_tokens,
                cache_key_forwarded,
                "prompt cache provider miss"
            );
        }
    }
}
