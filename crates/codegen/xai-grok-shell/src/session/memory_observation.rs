use xai_grok_memory::{
    MemoryObservationSink, MemoryRetrievalMode, MemorySearchErrorClass, MemorySearchObservation,
    MemorySearchSource, MemoryWatcherSyncObservation,
};
use xai_grok_telemetry::memory_telemetry::{
    MemoryInjection, MemoryInjectionOutcome, MemorySearch, MemorySearchEmpty, MemoryWatcherSync,
};

pub(crate) struct TelemetryMemoryObservationSink {
    pub(crate) session_id: String,
}

#[derive(Default)]
pub(crate) struct MemoryInjectionMetrics {
    pub(crate) is_greeting_fallback: bool,
    pub(crate) result_count: usize,
    pub(crate) total_snippet_chars: usize,
    pub(crate) top_score: f64,
    pub(crate) configured_min_score: f64,
    pub(crate) duration_ms: u64,
    pub(crate) injected_bytes: u64,
    pub(crate) estimated_tokens: u64,
    pub(crate) global_entry_count: usize,
    pub(crate) workspace_entry_count: usize,
    pub(crate) was_reused: bool,
}

pub(crate) fn log_memory_injection(
    session_id: String,
    _outcome: MemoryInjectionOutcome,
    metrics: MemoryInjectionMetrics,
) {
    let _ = (_outcome, metrics.was_reused);
    xai_grok_telemetry::session_ctx::log_event(MemoryInjection {
        session_id,
        was_greeting_fallback: metrics.is_greeting_fallback,
        result_count: metrics.result_count,
        total_snippet_chars: metrics.total_snippet_chars,
        top_score: metrics.top_score,
        configured_min_score: metrics.configured_min_score,
        injection_duration_ms: metrics.duration_ms,
    });
}

pub(crate) fn memory_v2_model_usage(
    model: &str,
    response: &xai_grok_sampling_types::ConversationResponse,
) -> xai_grok_telemetry::memory_telemetry::MemoryV2ModelUsage {
    xai_grok_telemetry::memory_telemetry::MemoryV2ModelUsage {
        model_id: Some(model.to_owned()),
        prompt_tokens: response.usage.as_ref().map(|usage| usage.prompt_tokens),
        completion_tokens: response.usage.as_ref().map(|usage| usage.completion_tokens),
        reasoning_tokens: response.usage.as_ref().map(|usage| usage.reasoning_tokens),
        cached_prompt_tokens: response
            .usage
            .as_ref()
            .map(|usage| usage.cached_prompt_tokens),
        cache_creation_tokens: response
            .usage
            .as_ref()
            .map(|usage| usage.cache_creation_prompt_tokens),
        cost_usd_ticks: response.cost_usd_ticks,
    }
}

fn search_mode_label(mode: MemoryRetrievalMode) -> &'static str {
    match mode {
        MemoryRetrievalMode::FtsOnly => "fts_only",
        MemoryRetrievalMode::Hybrid => "hybrid",
        MemoryRetrievalMode::EmbeddingFallback => "embedding_fallback",
    }
}

fn search_source_label(source: MemorySearchSource) -> &'static str {
    match source {
        MemorySearchSource::Tool => "tool",
        MemorySearchSource::Injection => "injection",
        MemorySearchSource::CompactionRecovery => "compaction_recovery",
    }
}

impl MemoryObservationSink for TelemetryMemoryObservationSink {
    fn observe_search(&self, observation: MemorySearchObservation) {
        let search_mode = search_mode_label(observation.mode).to_owned();
        let source = search_source_label(observation.source).to_owned();
        match observation.outcome {
            xai_grok_memory::MemorySearchOutcome::Empty
            | xai_grok_memory::MemorySearchOutcome::Error => {
                xai_grok_telemetry::session_ctx::log_event(MemorySearchEmpty {
                    session_id: self.session_id.clone(),
                    query_length: observation.query_length,
                    keyword_count: observation.keyword_count,
                    min_score_threshold: observation.min_score_threshold,
                    search_mode,
                    duration_ms: observation.duration_ms,
                    vec_available: observation.is_vector_available,
                    source,
                });
            }
            xai_grok_memory::MemorySearchOutcome::Results => {
                xai_grok_telemetry::session_ctx::log_event(MemorySearch {
                    session_id: self.session_id.clone(),
                    query_length: observation.query_length,
                    keyword_count: observation.keyword_count,
                    result_count: observation.result_count,
                    top_score: observation.top_score,
                    min_score_threshold: observation.min_score_threshold,
                    search_mode,
                    duration_ms: observation.duration_ms,
                    vec_available: observation.is_vector_available,
                    source,
                });
            }
        }
        let _ = observation.error_class;
    }

    fn observe_watcher_sync(&self, observation: MemoryWatcherSyncObservation) {
        xai_grok_telemetry::session_ctx::log_event(MemoryWatcherSync {
            session_id: self.session_id.clone(),
            dirty_file_count: observation.dirty_file_count,
            claimed: observation.is_claimed,
            reindexed_count: observation.reindexed_count,
            embedded_count: observation.embedded_count,
            duration_ms: observation.duration_ms,
        });
    }
}
