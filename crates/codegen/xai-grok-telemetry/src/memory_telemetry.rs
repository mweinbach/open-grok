//! Memory subsystem telemetry. Routes through `log_event` (product tier,
//! `Enabled` mode only). No PII or user content -- only counts, scores,
//! durations, and config values.

use serde::Serialize;

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryMode {
    #[default]
    Legacy,
    V2,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2Rollout {
    Off,
    RecordOnly,
    Shadow,
    #[default]
    Active,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2CaptureStage {
    #[default]
    Queued,
    Claimed,
    Completed,
    Noop,
    Retry,
    Failed,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2FailureClass {
    #[default]
    Disabled,
    Storage,
    Lease,
    Model,
    MalformedOutput,
    EmptyOutput,
    Timeout,
    Convergence,
    AccessPolicy,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2DreamDisposition {
    Ineligible,
    Ready,
    Coalesced,
    Busy,
    Noop,
    Shadow,
    Committed,
    Reconciled,
    Retry,
    #[default]
    Failed,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2FlushOutcome {
    #[default]
    Success,
    RetryableFailure,
    TerminalFailure,
    Timeout,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2TargetKind {
    #[default]
    Observation,
    Topic,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryV2Component {
    #[default]
    Capture,
    Flush,
    Dream,
    GarbageCollection,
    Forget,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2ControlsPinned {
    pub rollout: MemoryV2Rollout,
    pub capture_enabled: bool,
    pub automatic_dream_enabled: bool,
    pub manual_dream_enabled: bool,
    pub file_writes_enabled: bool,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct MemoryV2ModelUsage {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
    /// USD ticks (1e10 ticks = $1); `None` when unpriced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd_ticks: Option<i64>,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2CaptureLifecycle {
    pub stage: MemoryV2CaptureStage,
    pub from_turn: u32,
    pub through_turn: u32,
    pub attempt: u32,
    pub observation_count: usize,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<MemoryV2FailureClass>,
    #[serde(flatten)]
    pub usage: MemoryV2ModelUsage,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2FlushResult {
    pub outcome: MemoryV2FlushOutcome,
    pub target_cursor: u32,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<MemoryV2FailureClass>,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2DreamLifecycle {
    pub disposition: MemoryV2DreamDisposition,
    pub observation_count: usize,
    pub topic_change_count: usize,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<MemoryV2FailureClass>,
    #[serde(flatten)]
    pub usage: MemoryV2ModelUsage,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2GcCompleted {
    pub archived_observations_removed: u64,
    pub terminal_jobs_removed: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2Forgotten {
    pub target_kind: MemoryV2TargetKind,
    pub was_already_forgotten: bool,
    pub tombstone_count: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct MemoryV2FailClosed {
    pub component: MemoryV2Component,
    pub reason: MemoryV2FailureClass,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchSource {
    #[default]
    Tool,
    Injection,
    CompactionRecovery,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchMode {
    #[default]
    FtsOnly,
    Hybrid,
    EmbeddingFallback,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchOutcome {
    #[default]
    Results,
    Empty,
    Error,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySearchErrorClass {
    IndexOpen,
    Fts,
    Vector,
}

#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryInjectionOutcome {
    #[default]
    Results,
    Empty,
    Error,
    Skipped,
}

#[derive(Serialize)]
pub struct MemorySessionInit {
    pub session_id: String,
    pub memory_enabled: bool,
    pub watcher_config_enabled: bool,
    pub watcher_started: bool,
    pub temporal_decay_enabled: bool,
    pub mmr_enabled: bool,
    pub mmr_lambda: f64,
    pub half_life_days: f64,
    pub embedding_dimensions: usize,
    pub total_chunks: usize,
    pub total_files: usize,
    pub has_global_memory_md: bool,
    pub has_workspace_memory_md: bool,
}

#[derive(Serialize)]
pub struct MemorySearch {
    pub session_id: String,
    pub query_length: usize,
    pub keyword_count: usize,
    pub result_count: usize,
    pub top_score: f64,
    pub min_score_threshold: f64,
    pub search_mode: String,
    pub duration_ms: u64,
    pub vec_available: bool,
    pub source: String,
}

#[derive(Serialize)]
pub struct MemorySearchEmpty {
    pub session_id: String,
    pub query_length: usize,
    pub keyword_count: usize,
    pub min_score_threshold: f64,
    pub search_mode: String,
    pub duration_ms: u64,
    pub vec_available: bool,
    pub source: String,
}

#[derive(Serialize)]
pub struct MemoryFlushStart {
    pub session_id: String,
    pub trigger: String,
    pub conversation_len: usize,
    pub user_message_count: usize,
}

#[derive(Serialize)]
pub struct MemoryFlushComplete {
    pub session_id: String,
    pub trigger: String,
    pub outcome: String,
    pub duration_ms: u64,
    pub response_length: usize,
    pub accepted_length: usize,
    pub was_truncated: bool,
}

#[derive(Serialize)]
pub struct MemoryInjection {
    pub session_id: String,
    pub was_greeting_fallback: bool,
    pub result_count: usize,
    pub total_snippet_chars: usize,
    pub top_score: f64,
    pub configured_min_score: f64,
    pub injection_duration_ms: u64,
}

#[derive(Serialize)]
pub struct MemoryReindex {
    pub session_id: String,
    pub source: String,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub embedded: usize,
    pub duration_ms: u64,
    pub trigger: String,
}

#[derive(Serialize)]
pub struct MemoryWatcherSync {
    pub session_id: String,
    pub dirty_file_count: usize,
    pub claimed: bool,
    pub reindexed_count: usize,
    pub embedded_count: usize,
    pub duration_ms: u64,
}

#[derive(Serialize)]
pub struct MemorySessionSummary {
    pub session_id: String,
    pub session_duration_secs: u64,
    pub flush_count: u64,
    pub flush_success_count: u64,
    pub flush_error_count: u64,
    pub tool_search_count: u64,
    pub injection_count: u64,
    pub recovery_search_count: u64,
    pub total_chunks_at_end: usize,
    pub chunks_added_this_session: usize,
    pub session_end_result: String,
    pub dream_count: u64,
    pub dream_success_count: u64,
    pub dream_error_count: u64,
    #[serde(default)]
    pub memory_enabled: bool,
    #[serde(default)]
    pub memory_mode: MemoryMode,
    #[serde(default)]
    pub capture_prompt_tokens: u64,
    #[serde(default)]
    pub capture_completion_tokens: u64,
    #[serde(default)]
    pub capture_cost_usd_ticks: u64,
    #[serde(default)]
    pub dream_prompt_tokens: u64,
    #[serde(default)]
    pub dream_completion_tokens: u64,
    #[serde(default)]
    pub dream_cost_usd_ticks: u64,
    #[serde(default)]
    pub injected_bytes: u64,
}
