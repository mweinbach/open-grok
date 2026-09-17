//! `SessionMemory` — memory subsystem state for the session actor.
//!
//! Groups storage, flush config, injection state, v2 workers, and telemetry
//! counters. Experience-memory fields stay fork-local and only apply while
//! the session uses the legacy pipeline.

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

#[derive(Debug, thiserror::Error)]
pub(crate) enum MemoryInitializationError {
    #[error("memory storage initialization failed: {0}")]
    Storage(#[source] std::io::Error),
    #[error("memory storage initialization task failed: {0}")]
    TaskJoin(#[source] tokio::task::JoinError),
}

pub(crate) async fn run_v2_initialization_blocking<F, T>(
    initialize: F,
) -> Result<T, MemoryInitializationError>
where
    F: FnOnce() -> std::io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(initialize)
        .await
        .map_err(MemoryInitializationError::TaskJoin)?
        .map_err(MemoryInitializationError::Storage)
}

/// Initialize a session's configured memory storage without running v2's
/// filesystem and SQLite setup on the single-threaded actor runtime.
///
/// Legacy initialization remains inline to preserve its existing behavior.
pub(crate) async fn initialize_memory_storage(
    storage: crate::session::memory::MemoryStorage,
) -> Result<(), MemoryInitializationError> {
    if storage.mode().is_legacy() {
        return storage
            .ensure_initialized()
            .map_err(MemoryInitializationError::Storage);
    }
    run_v2_initialization_blocking(move || storage.ensure_initialized()).await
}

pub(crate) struct CaptureWorker {
    cancel: tokio_util::sync::CancellationToken,
    task: tokio_util::task::AbortOnDropHandle<()>,
}

impl CaptureWorker {
    fn new(cancel: tokio_util::sync::CancellationToken, task: tokio::task::JoinHandle<()>) -> Self {
        Self {
            cancel,
            task: tokio_util::task::AbortOnDropHandle::new(task),
        }
    }

    fn is_finished(&self) -> bool {
        self.task.is_finished()
    }

    async fn cancel_and_join(mut self) {
        self.cancel.cancel();
        if let Err(error) = (&mut self.task).await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "memory-v2 capture worker join failed");
        }
    }

    async fn join_finished(mut self) {
        debug_assert!(self.task.is_finished());
        if let Err(error) = (&mut self.task).await
            && !error.is_cancelled()
        {
            tracing::warn!(%error, "memory-v2 finished capture worker join failed");
        }
    }
}

impl Drop for CaptureWorker {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(crate) struct V2DreamWorkers {
    cancel: RefCell<tokio_util::sync::CancellationToken>,
    tasks: RefCell<Vec<tokio_util::task::AbortOnDropHandle<()>>>,
}

impl Default for V2DreamWorkers {
    fn default() -> Self {
        Self {
            cancel: RefCell::new(tokio_util::sync::CancellationToken::new()),
            tasks: RefCell::new(Vec::new()),
        }
    }
}

impl V2DreamWorkers {
    pub(crate) fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.borrow().clone()
    }

    pub(crate) fn track(&self, handle: tokio::task::JoinHandle<()>) {
        let mut tasks = self.tasks.borrow_mut();
        tasks.retain(|task| !task.is_finished());
        tasks.push(tokio_util::task::AbortOnDropHandle::new(handle));
    }

    pub(crate) async fn cancel_and_join(&self) {
        self.cancel.borrow().cancel();
        let tasks = std::mem::take(&mut *self.tasks.borrow_mut());
        for task in tasks {
            if let Err(error) = task.await {
                tracing::warn!(error = %error, "memory-v2 Dream worker join failed");
            }
        }
        *self.cancel.borrow_mut() = tokio_util::sync::CancellationToken::new();
    }
}

impl Drop for V2DreamWorkers {
    fn drop(&mut self) {
        self.cancel.get_mut().cancel();
        self.tasks.get_mut().clear();
    }
}

/// Memory subsystem state for a session.
pub(crate) struct SessionMemory {
    /// Fresh identity for this actor activation, distinct from the persistent
    /// session ID so a resumed session starts an independent experience run.
    pub experience_run_id: String,
    /// Tool results already present when this actor was created. Their calls
    /// must not be attributed to this activation after a persisted resume.
    pub experience_prior_tool_result_ids: HashSet<String>,
    /// Provider active when the memory backend was created. Embeddings may use
    /// an independently resolved xAI route even when chat runs on Codex.
    pub embedding_provider: xai_grok_sampling_types::ModelProvider,
    /// Live provider for the active chat model (telemetry/state only).
    pub active_provider: std::cell::Cell<xai_grok_sampling_types::ModelProvider>,
    /// Mode resolved when the session was spawned. Kept even while memory is
    /// disabled so toggles cannot switch the on-disk root mid-session.
    pub configured_mode: Option<crate::config::MemoryMode>,
    /// Rollout and kill switches resolved once at session spawn.
    pub v2_config: crate::config::MemoryV2Config,
    /// Storage layout resolved at spawn. Retained while disabled so re-enabling
    /// restores the pinned mode and any configured root override.
    pub configured_storage: Option<crate::session::memory::MemoryStorage>,
    /// Memory storage handle for writing flush output (None when memory disabled).
    /// Wrapped in `RefCell` to allow `/memory on|off` toggle from `&Arc<SessionActor>`.
    pub storage: RefCell<Option<crate::session::memory::MemoryStorage>>,
    /// Whether to write a session summary to memory on session end.
    pub save_on_end: bool,
    /// Shared params for building a fully-configured memory backend.
    /// `None` when memory is disabled or v2 is selected.
    pub backend_params: Option<crate::session::memory::MemoryBackendParams>,
    /// First-turn memory injection behavior resolved from local + remote config.
    pub initial_injection_config: crate::config::MemoryInitialInjectionConfig,
    /// Per-process latch: the first-turn injection decision already ran in
    /// this session segment. Cross-segment idempotency comes from
    /// `conversation_has_memory_context`, not this flag.
    pub context_injected: AtomicBool,
    /// Memory flush configuration (from MemoryConfig).
    pub flush_config: crate::config::MemoryFlushConfig,
    /// When `true`, auto-compact checks are suppressed during the legacy
    /// in-context memory flush. Memory-v2 extraction uses its own worker latch.
    pub is_flushing: Arc<AtomicBool>,
    /// Cooperatively-cancelled, joinable memory-v2 worker.
    pub capture_worker: RefCell<Option<CaptureWorker>>,
    /// Owns every capture-triggered Dream task until session teardown.
    pub dream_workers: V2DreamWorkers,
    /// Class of the most recent capture failure reported by this process.
    pub last_capture_failure:
        RefCell<Option<xai_grok_telemetry::memory_telemetry::MemoryV2FailureClass>>,
    /// The compaction count at which the last flush ran (once-per-cycle guard).
    pub last_flush_compaction: AtomicU64,
    /// Number of flushes executed in this session.
    pub flush_count: AtomicU64,
    /// Content from the most recent successful flush, used for delta prompts.
    pub last_flush_content: RefCell<Option<String>>,
    /// Number of successful flushes.
    pub flush_success_count: AtomicU64,
    /// Number of failed flushes.
    pub flush_error_count: AtomicU64,
    /// Counts model-initiated `memory_search` tool calls.
    pub search_counter: RefCell<Option<Arc<AtomicU64>>>,
    /// Counts first-turn memory context injections.
    pub injection_count: AtomicU64,
    /// Counts post-compaction memory re-injection searches.
    pub compaction_recovery_count: AtomicU64,
    /// Total memory chunks added across all sources.
    pub chunks_added: Arc<AtomicU64>,
    /// Handle to the startup reindex+embed task, taken and awaited by the launch dream.
    pub init_reindex_handle: RefCell<Option<tokio::task::JoinHandle<()>>>,
    /// autoDream consolidation config.
    pub dream_config: crate::config::MemoryDreamConfig,
    /// Number of dream consolidations attempted.
    pub dream_count: AtomicU64,
    /// Number of successful dream consolidations.
    pub dream_success_count: AtomicU64,
    /// Number of failed dream consolidations.
    pub dream_error_count: AtomicU64,
    pub token_totals: MemoryV2TokenTotals,
}

#[derive(Default)]
pub(crate) struct MemoryV2TokenTotals {
    pub capture_prompt_tokens: AtomicU64,
    pub capture_completion_tokens: AtomicU64,
    pub capture_cost_usd_ticks: AtomicU64,
    pub dream_prompt_tokens: AtomicU64,
    pub dream_completion_tokens: AtomicU64,
    pub dream_cost_usd_ticks: AtomicU64,
    pub injected_bytes: AtomicU64,
}

impl SessionMemory {
    /// Disabled session memory with fork experience identity filled in.
    pub(crate) fn empty() -> Self {
        Self {
            experience_run_id: uuid::Uuid::now_v7().to_string(),
            experience_prior_tool_result_ids: HashSet::new(),
            embedding_provider: xai_grok_sampling_types::ModelProvider::Xai,
            active_provider: std::cell::Cell::new(xai_grok_sampling_types::ModelProvider::Xai),
            configured_mode: None,
            v2_config: Default::default(),
            configured_storage: None,
            storage: RefCell::new(None),
            save_on_end: true,
            backend_params: None,
            initial_injection_config: Default::default(),
            context_injected: AtomicBool::new(false),
            flush_config: Default::default(),
            is_flushing: Arc::new(AtomicBool::new(false)),
            capture_worker: RefCell::new(None),
            dream_workers: V2DreamWorkers::default(),
            last_capture_failure: RefCell::new(None),
            last_flush_compaction: AtomicU64::new(0),
            flush_count: AtomicU64::new(0),
            last_flush_content: RefCell::new(None),
            flush_success_count: AtomicU64::new(0),
            flush_error_count: AtomicU64::new(0),
            search_counter: RefCell::new(None),
            injection_count: AtomicU64::new(0),
            compaction_recovery_count: AtomicU64::new(0),
            chunks_added: Arc::new(AtomicU64::new(0)),
            init_reindex_handle: RefCell::new(None),
            dream_config: Default::default(),
            dream_count: AtomicU64::new(0),
            dream_success_count: AtomicU64::new(0),
            dream_error_count: AtomicU64::new(0),
            token_totals: MemoryV2TokenTotals::default(),
        }
    }

    /// Stable, safe attribution identity for this session actor activation.
    pub(crate) fn experience_run_id(&self) -> &str {
        &self.experience_run_id
    }

    /// Collect completed tool-result identities without excluding inherited
    /// assistant calls that have not produced an output yet.
    pub(crate) fn collect_prior_tool_result_ids(
        conversation: &[crate::sampling::ConversationItem],
    ) -> HashSet<String> {
        conversation
            .iter()
            .filter_map(|item| match item {
                crate::sampling::ConversationItem::ToolResult(result) => {
                    Some(result.tool_call_id.clone())
                }
                crate::sampling::ConversationItem::CustomToolOutput(output) => {
                    Some(output.call_id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Result identities inherited from an earlier activation of this session.
    pub(crate) fn experience_prior_tool_result_ids(&self) -> &HashSet<String> {
        &self.experience_prior_tool_result_ids
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.storage.borrow().is_some()
    }

    pub(crate) fn mode(&self) -> Option<crate::config::MemoryMode> {
        self.configured_mode
            .or_else(|| self.storage.borrow().as_ref().map(|storage| storage.mode()))
    }

    pub(crate) fn uses_legacy_pipeline(&self) -> bool {
        self.is_enabled()
            && self
                .mode()
                .is_some_and(crate::config::MemoryMode::is_legacy)
    }

    pub(crate) fn can_capture_v2(&self) -> bool {
        self.is_enabled()
            && self.mode().is_some_and(crate::config::MemoryMode::is_v2)
            && self.v2_config.can_capture()
    }

    pub(crate) fn can_expose_v2(&self) -> bool {
        self.is_enabled()
            && self.mode().is_some_and(crate::config::MemoryMode::is_v2)
            && self.v2_config.can_expose_memory()
    }

    /// Why memory is off, or `None` while it is on.
    pub(crate) fn disabled_reason(
        &self,
    ) -> Option<crate::extensions::notification::MemoryDisabledReason> {
        use crate::extensions::notification::MemoryDisabledReason;
        if self.is_enabled() {
            return None;
        }
        let v2_restricted = self.mode() == Some(crate::config::MemoryMode::V2)
            && (self.v2_config.rollout == crate::config::MemoryV2Rollout::Off
                || !self.v2_config.file_writes_enabled);
        Some(if v2_restricted {
            MemoryDisabledReason::RolloutRestricted
        } else if self.configured_storage.is_none() {
            MemoryDisabledReason::NotConfigured
        } else {
            MemoryDisabledReason::SessionToggle
        })
    }

    pub(crate) fn storage(&self) -> Option<crate::session::memory::MemoryStorage> {
        self.storage.borrow().clone()
    }

    pub(crate) fn try_acquire_flush_lock(&self) -> bool {
        self.is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn release_flush_lock(&self) {
        self.is_flushing
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn track_capture_worker(
        &self,
        cancel: tokio_util::sync::CancellationToken,
        task: tokio::task::JoinHandle<()>,
    ) {
        debug_assert!(!self.capture_worker_is_running());
        self.capture_worker
            .replace(Some(CaptureWorker::new(cancel, task)));
    }

    pub(crate) fn capture_worker_is_running(&self) -> bool {
        self.capture_worker
            .borrow()
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }

    pub(crate) async fn join_finished_capture_worker(&self) {
        let worker = {
            let mut slot = self.capture_worker.borrow_mut();
            if slot.as_ref().is_some_and(CaptureWorker::is_finished) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(worker) = worker {
            worker.join_finished().await;
        }
    }

    pub(crate) async fn stop_capture_worker(&self) {
        let worker = self.capture_worker.borrow_mut().take();
        if let Some(worker) = worker {
            worker.cancel_and_join().await;
        }
    }

    pub(crate) fn record_capture_failure(
        &self,
        failure_class: Option<xai_grok_telemetry::memory_telemetry::MemoryV2FailureClass>,
    ) {
        self.last_capture_failure.replace(failure_class);
    }

    pub(crate) fn last_capture_failure(
        &self,
    ) -> Option<xai_grok_telemetry::memory_telemetry::MemoryV2FailureClass> {
        *self.last_capture_failure.borrow()
    }

    pub(crate) fn record_flush_result(&self, outcome: &str) {
        self.flush_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match outcome {
            "written" => {
                self.flush_success_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            "error" => {
                self.flush_error_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub(crate) fn record_dream_result(&self, success: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        self.dream_count.fetch_add(1, Relaxed);
        if success {
            self.dream_success_count.fetch_add(1, Relaxed);
        } else {
            self.dream_error_count.fetch_add(1, Relaxed);
        }
    }

    pub(crate) fn record_dream_neutral(&self) {
        self.dream_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn record_capture_usage(
        &self,
        usage: &xai_grok_telemetry::memory_telemetry::MemoryV2ModelUsage,
    ) {
        add_model_usage(
            usage,
            &self.token_totals.capture_prompt_tokens,
            &self.token_totals.capture_completion_tokens,
            &self.token_totals.capture_cost_usd_ticks,
        );
    }

    pub(crate) fn record_dream_usage(
        &self,
        usage: &xai_grok_telemetry::memory_telemetry::MemoryV2ModelUsage,
    ) {
        add_model_usage(
            usage,
            &self.token_totals.dream_prompt_tokens,
            &self.token_totals.dream_completion_tokens,
            &self.token_totals.dream_cost_usd_ticks,
        );
    }

    pub(crate) fn record_injected_bytes(&self, bytes: u64) {
        self.token_totals
            .injected_bytes
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn open_index(
        &self,
        storage: &crate::session::memory::MemoryStorage,
    ) -> Option<crate::session::memory::MemoryIndex> {
        if !self.uses_legacy_pipeline() {
            return None;
        }
        let embed_dims = self
            .backend_params
            .as_ref()
            .and_then(|p| p.embed_config.as_ref())
            .map_or(1024, |c| c.dimensions);
        let db_path = storage.workspace_dir().join("index.sqlite");
        crate::session::memory::MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            Default::default(),
            embed_dims,
        )
        .ok()
    }

    pub(crate) async fn await_init_reindex(&self) {
        let handle = self.init_reindex_handle.borrow_mut().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    pub(crate) async fn reindex_and_embed(&self, path: &std::path::Path, source: &str) {
        let Some(storage) = self.storage() else {
            return;
        };
        if let Some(mut index) = self.open_index(&storage) {
            let _ = index.reindex_file(path, source);
            if let Some(ref params) = self.backend_params
                && let Some(provider) = params.make_embedding_provider().await
            {
                crate::session::memory::embed_missing_chunks(&index, &provider).await;
            }
        }
    }

    pub(crate) fn delete_paths_from_index(&self, paths: &[std::path::PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let Some(storage) = self.storage() else {
            return;
        };
        if let Some(mut index) = self.open_index(&storage) {
            let mut total_removed = 0usize;
            for path in paths {
                match index.delete_path(path) {
                    Ok(n) => total_removed += n,
                    Err(e) => {
                        tracing::warn!(
                            target: xai_grok_telemetry::memory_log::TARGET,
                            path = %path.display(),
                            error = %e,
                            "DREAM_CLEANUP: failed to remove chunks from index"
                        );
                    }
                }
            }
            if total_removed > 0 {
                tracing::info!(
                    target: xai_grok_telemetry::memory_log::TARGET,
                    chunks_removed = total_removed,
                    files = paths.len(),
                    "DREAM_CLEANUP: removed stale chunks from index"
                );
            }
        }
    }

    pub(crate) fn telemetry_snapshot(&self) -> MemoryTelemetry {
        use std::sync::atomic::Ordering::Relaxed;
        MemoryTelemetry {
            flush_count: self.flush_count.load(Relaxed),
            flush_success_count: self.flush_success_count.load(Relaxed),
            flush_error_count: self.flush_error_count.load(Relaxed),
            tool_search_count: self
                .search_counter
                .borrow()
                .as_ref()
                .map_or(0, |c| c.load(Relaxed)),
            injection_count: self.injection_count.load(Relaxed),
            compaction_recovery_count: self.compaction_recovery_count.load(Relaxed),
            chunks_added: self.chunks_added.load(Relaxed),
            dream_count: self.dream_count.load(Relaxed),
            dream_success_count: self.dream_success_count.load(Relaxed),
            dream_error_count: self.dream_error_count.load(Relaxed),
            capture_prompt_tokens: self.token_totals.capture_prompt_tokens.load(Relaxed),
            capture_completion_tokens: self.token_totals.capture_completion_tokens.load(Relaxed),
            capture_cost_usd_ticks: self.token_totals.capture_cost_usd_ticks.load(Relaxed),
            dream_prompt_tokens: self.token_totals.dream_prompt_tokens.load(Relaxed),
            dream_completion_tokens: self.token_totals.dream_completion_tokens.load(Relaxed),
            dream_cost_usd_ticks: self.token_totals.dream_cost_usd_ticks.load(Relaxed),
            injected_bytes: self.token_totals.injected_bytes.load(Relaxed),
        }
    }
}

fn add_model_usage(
    usage: &xai_grok_telemetry::memory_telemetry::MemoryV2ModelUsage,
    prompt_tokens: &AtomicU64,
    completion_tokens: &AtomicU64,
    cost_usd_ticks: &AtomicU64,
) {
    use std::sync::atomic::Ordering::Relaxed;
    if let Some(tokens) = usage.prompt_tokens {
        prompt_tokens.fetch_add(u64::from(tokens), Relaxed);
    }
    if let Some(tokens) = usage.completion_tokens {
        completion_tokens.fetch_add(u64::from(tokens), Relaxed);
    }
    if let Some(ticks) = usage
        .cost_usd_ticks
        .and_then(|ticks| u64::try_from(ticks).ok())
    {
        cost_usd_ticks.fetch_add(ticks, Relaxed);
    }
}

#[must_use]
#[cfg(test)]
pub(crate) struct FlushLockGuard {
    is_flushing: Arc<AtomicBool>,
}

#[cfg(test)]
impl FlushLockGuard {
    fn try_acquire(is_flushing: Arc<AtomicBool>) -> Option<Self> {
        is_flushing
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
            .then_some(Self { is_flushing })
    }
}

#[cfg(test)]
impl Drop for FlushLockGuard {
    fn drop(&mut self) {
        self.is_flushing
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Snapshot of memory telemetry counters for session-end logging.
pub(crate) struct MemoryTelemetry {
    pub flush_count: u64,
    pub flush_success_count: u64,
    pub flush_error_count: u64,
    pub tool_search_count: u64,
    pub injection_count: u64,
    pub compaction_recovery_count: u64,
    pub chunks_added: u64,
    pub dream_count: u64,
    pub dream_success_count: u64,
    pub dream_error_count: u64,
    pub capture_prompt_tokens: u64,
    pub capture_completion_tokens: u64,
    pub capture_cost_usd_ticks: u64,
    pub dream_prompt_tokens: u64,
    pub dream_completion_tokens: u64,
    pub dream_cost_usd_ticks: u64,
    pub injected_bytes: u64,
}

#[cfg(test)]
#[path = "memory_state_tests.rs"]
mod worker_tests;

#[cfg(test)]
mod tests {
    use super::SessionMemory;
    use crate::sampling::conversation::{ConversationItem, CustomToolOutputItem, ToolCall};

    #[test]
    fn prior_tool_result_ids_include_outputs_but_not_pending_assistant_calls() {
        let conversation = vec![
            ConversationItem::assistant_tool_calls(vec![ToolCall {
                id: "inherited-pending-call".into(),
                name: "run_terminal_command".to_owned(),
                arguments: "{}".into(),
            }]),
            ConversationItem::tool_result("inherited-direct-result", "exit: 0"),
            ConversationItem::custom_tool_output(CustomToolOutputItem::text(
                "inherited-custom-result",
                "exit: 0",
            )),
            ConversationItem::tool_result("inherited-direct-result", "duplicate output"),
        ];

        let prior_results = SessionMemory::collect_prior_tool_result_ids(&conversation);

        assert_eq!(prior_results.len(), 2);
        assert!(prior_results.contains("inherited-direct-result"));
        assert!(prior_results.contains("inherited-custom-result"));
        assert!(!prior_results.contains("inherited-pending-call"));
    }
}
