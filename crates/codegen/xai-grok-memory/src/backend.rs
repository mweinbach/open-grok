//! Concrete `MemoryBackend` implementation using hybrid search.
//!
//! `MemoryBackendImpl` combines FTS5 keyword search with optional vector
//! KNN similarity via `hybrid_search()`. When embeddings are available
//! (embedding config + API key), the query is vectorized and both signals
//! are merged with recency and source weights. When embeddings are
//! unavailable, gracefully degrades to FTS-only.
//!
//! `rusqlite::Connection` is `!Send + !Sync`, so we open a fresh `MemoryIndex`
//! per query. WAL mode ensures concurrent readers don't block.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xai_grok_tools::types::memory_backend::{
    ExperienceEvidenceReference, ExperienceSearchResult, MemoryBackend, MemorySearchResult,
};

use super::embedding::EmbeddingProvider as _;
use super::experience::extraction::redact_sensitive_text;
use super::experience::store::{ExperienceStore, source_reference_is_safe};
use super::experience::types::{EvidenceSignal, EvidenceVerdict, ExperienceQuery};
use super::storage::MemoryStorage;
use super::watcher::MemoryFileWatcher;

const MAX_EXPERIENCE_SEARCH_RESULTS: usize = 50;
const MAX_EXPERIENCE_SEARCH_CANDIDATES: usize = 256;
const MAX_EXPERIENCE_SEARCH_DETAILS: usize = 16;
const MAX_EXPERIENCE_SEARCH_FIELD_CHARS: usize = 2_048;

/// Embedding-client credentials scoped to a trusted endpoint. Only
/// [`Self::for_endpoint`] retains a live credential; the empty default fails closed.
#[derive(Clone, Default)]
pub struct EndpointScopedCredentials {
    endpoint: Option<reqwest::Url>,
    auth_credentials: Option<Arc<dyn xai_grok_auth::AuthCredentialProvider>>,
    api_key_provider: Option<xai_grok_tools::types::SharedApiKeyProvider>,
}

// Manual Debug that redacts the credential handles; only their presence shows.
impl std::fmt::Debug for EndpointScopedCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EndpointScopedCredentials")
            .field("endpoint", &self.endpoint)
            .field("has_auth_credentials", &self.auth_credentials.is_some())
            .field("has_api_key_provider", &self.api_key_provider.is_some())
            .finish()
    }
}

impl EndpointScopedCredentials {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.auth_credentials.is_none() && self.api_key_provider.is_none()
    }

    /// Retains the credentials only for a trusted, parsable `endpoint`; otherwise drops them.
    pub fn for_endpoint(
        endpoint: &str,
        is_trusted: impl FnOnce(&str) -> bool,
        auth_credentials: Option<Arc<dyn xai_grok_auth::AuthCredentialProvider>>,
        api_key_provider: Option<xai_grok_tools::types::SharedApiKeyProvider>,
    ) -> Self {
        if is_trusted(endpoint)
            && let Ok(url) = reqwest::Url::parse(endpoint)
        {
            return Self {
                endpoint: Some(url),
                auth_credentials,
                api_key_provider,
            };
        }
        if auth_credentials.is_some() || api_key_provider.is_some() {
            tracing::info!(
                target: xai_grok_telemetry::memory_log::TARGET,
                endpoint,
                "memory embeddings: session credentials withheld for non-first-party endpoint; its own key, if any, still applies"
            );
        }
        Self::none()
    }

    fn auth_credentials(&self) -> Option<&Arc<dyn xai_grok_auth::AuthCredentialProvider>> {
        self.auth_credentials.as_ref()
    }

    fn api_key_provider(&self) -> Option<&xai_grok_tools::types::SharedApiKeyProvider> {
        self.api_key_provider.as_ref()
    }

    fn approved_for(&self, base_url: &str) -> bool {
        match &self.endpoint {
            None => self.is_empty(),
            Some(endpoint) => reqwest::Url::parse(base_url).is_ok_and(|url| &url == endpoint),
        }
    }
}

/// All configuration needed to build a fully-wired [`MemoryBackendImpl`] for a live session.
///
/// Grouping these in one struct ensures every call site — ToolBridge, first-turn
/// injection, and post-compaction recovery — shares identical config.  Without it,
/// different paths silently fell back to FTS-only search and ignored
/// `[memory.search]` config because no single place applied all builder methods.
#[derive(Clone)]
pub struct MemoryBackendParams {
    /// Session ID for telemetry events.
    pub session_id: String,
    /// Embedding provider config — `None` forces FTS-only fallback everywhere.
    pub embed_config: Option<xai_grok_config_types::MemoryEmbeddingConfig>,
    /// Base URL for embedding API calls (CLI proxy). Must match the endpoint
    /// `embedding_credentials` was scoped to; mismatch fails closed.
    pub embed_base_url: String,
    /// API key for embedding API calls.
    pub embed_api_key: Option<String>,
    /// Hybrid search scoring config (weights, thresholds, decay, MMR).
    pub search_config: xai_grok_config_types::MemorySearchConfig,
    /// File watcher for sync-on-search — `None` disables external-edit detection.
    pub watcher: Option<Arc<MemoryFileWatcher>>,
    /// Seconds before a stale reindex claim is forcibly released.
    pub stale_claim_secs: i64,
    /// Telemetry label emitted with every search event from this backend.
    ///
    /// Differentiates the three runtime search paths in dashboards and logs:
    /// - `"tool"` — model-initiated `memory_search` tool call (ToolBridge)
    /// - `"injection"` — first-turn memory context injection
    /// - `"compaction_recovery"` — post-compaction context re-injection
    pub search_source: &'static str,
    pub embedding_credentials: EndpointScopedCredentials,
}

impl MemoryBackendParams {
    /// Async so `current_api_key_async` can drive the AuthManager
    /// refresh chain; reindex loops outlive the OIDC TTL.
    pub async fn make_embedding_provider(&self) -> Option<super::embedding::ApiEmbeddingProvider> {
        build_embedding_provider(
            self.embed_config.as_ref(),
            &self.embedding_credentials,
            self.embed_api_key.as_deref(),
            &self.embed_base_url,
        )
        .await
    }
}

async fn build_embedding_provider(
    config: Option<&xai_grok_config_types::MemoryEmbeddingConfig>,
    credentials: &EndpointScopedCredentials,
    static_api_key: Option<&str>,
    base_url: &str,
) -> Option<super::embedding::ApiEmbeddingProvider> {
    let config = config?;
    if config.model.as_ref().is_none_or(|m| m.is_empty()) {
        return None;
    }

    // Enforce at runtime, in release too: a `debug_assert` would compile out of
    // shipped binaries and let a scoped credential reach an unapproved URL.
    let credentials_approved = credentials.approved_for(base_url);
    if !credentials_approved {
        tracing::error!(
            target: xai_grok_telemetry::memory_log::TARGET,
            base_url,
            approved = ?credentials.endpoint,
            "memory embeddings: scoped credentials do not match the request URL; dropping them"
        );
    }

    if credentials_approved && let Some(creds) = credentials.auth_credentials() {
        let client = super::embedding::build_middleware_client(creds.clone());
        return super::embedding::ApiEmbeddingProvider::from_config(
            config,
            base_url.to_owned(),
            client,
        );
    }

    let per_call_key = if credentials_approved && let Some(p) = credentials.api_key_provider() {
        p.current_api_key_async().await
    } else {
        None
    };
    let api_key = per_call_key.or_else(|| static_api_key.map(|s| s.to_owned()))?;
    super::embedding::ApiEmbeddingProvider::from_session(config, base_url.to_owned(), api_key)
}

/// `MemoryBackend` implementation backed by hybrid search (FTS5 + vector KNN).
///
/// Stores only `Send + Sync` config data. The `MemoryIndex` and
/// `EmbeddingProvider` are constructed on demand per query.
pub struct MemoryBackendImpl {
    db_path: PathBuf,
    storage: MemoryStorage,
    /// Embedding config — `None` disables vector search (FTS-only fallback).
    embed_config: Option<xai_grok_config_types::MemoryEmbeddingConfig>,
    /// API base URL for embedding requests (cli-chat-proxy).
    embed_base_url: String,
    /// API key for embedding requests.
    embed_api_key: Option<String>,
    /// Search scoring config (weights, min_score, max_results).
    search_config: xai_grok_config_types::MemorySearchConfig,
    /// File watcher for detecting external memory edits.
    watcher: Option<Arc<MemoryFileWatcher>>,
    /// Stale claim threshold for reindex coordination.
    stale_claim_secs: i64,
    /// Session ID for telemetry events.
    session_id: String,
    /// Telemetry label for search events — mirrors [`MemoryBackendParams::search_source`].
    search_source: &'static str,
    /// Shared search counter — read by session summary telemetry.
    ///
    /// Only the ToolBridge backend's counter is shared back to the session actor;
    /// injection and compaction-recovery backends use their own local counters.
    pub search_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    embedding_credentials: EndpointScopedCredentials,
}

impl MemoryBackendImpl {
    /// Create a new backend. `db_path` must point to an existing SQLite
    /// database created by `MemoryIndex::open_or_create()`.
    pub fn new(db_path: PathBuf, storage: MemoryStorage) -> Self {
        Self {
            db_path,
            storage,
            embed_config: None,
            embed_base_url: String::new(),
            embed_api_key: None,
            search_config: xai_grok_config_types::MemorySearchConfig::default(),
            watcher: None,
            stale_claim_secs: 60,
            session_id: String::new(),
            search_source: "tool",
            embedding_credentials: EndpointScopedCredentials::none(),
            search_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Set the session ID for telemetry.
    pub fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = session_id;
        self
    }

    /// Configure the embedding provider for hybrid search.
    ///
    /// Without this, `search()` falls back to FTS-only.
    pub fn with_embedding(
        mut self,
        config: xai_grok_config_types::MemoryEmbeddingConfig,
        base_url: String,
        api_key: Option<String>,
    ) -> Self {
        self.embed_config = Some(config);
        self.embed_base_url = base_url;
        self.embed_api_key = api_key;
        self
    }

    /// Override the search scoring config (weights, limits, etc.).
    pub fn with_search_config(mut self, config: xai_grok_config_types::MemorySearchConfig) -> Self {
        self.search_config = config;
        self
    }

    /// Attach a file watcher for sync-on-search (reindex dirty files before querying).
    pub fn with_watcher(mut self, watcher: Arc<MemoryFileWatcher>, stale_claim_secs: i64) -> Self {
        self.watcher = Some(watcher);
        self.stale_claim_secs = stale_claim_secs;
        self
    }

    /// Open a read-only connection for simple queries (`total_chunks`, `get`).
    fn open_readonly(&self) -> Result<rusqlite::Connection, rusqlite::Error> {
        // Journal-mode-aware open (busy_timeout included): never mmap a legacy
        // WAL -shm on network mounts (SIGBUS); see JournalMode::open_readonly.
        xai_sqlite_journal::JournalMode::for_db_path(&self.db_path).open_readonly(&self.db_path)
    }

    async fn make_embedding_provider(&self) -> Option<super::embedding::ApiEmbeddingProvider> {
        build_embedding_provider(
            self.embed_config.as_ref(),
            &self.embedding_credentials,
            self.embed_api_key.as_deref(),
            &self.embed_base_url,
        )
        .await
    }

    fn search_experience_records(
        &self,
        query: &str,
        max_results: usize,
        outcome: Option<bool>,
    ) -> anyhow::Result<Vec<ExperienceSearchResult>> {
        if query.trim().is_empty() || max_results == 0 || self.storage.is_ephemeral() {
            return Ok(Vec::new());
        }

        let max_results = max_results.min(MAX_EXPERIENCE_SEARCH_RESULTS);
        let store = ExperienceStore::open(&self.db_path)?;
        let search = ExperienceQuery {
            text: query.to_owned(),
            repository_id: Some(self.storage.workspace_dir().to_string_lossy().into_owned()),
            repository_revision: super::experience::current_repository_revision(
                self.storage.workspace_path(),
            ),
            environment: Some(super::experience::execution_environment()),
            // SQLite applies outcome and exact-workspace filters before this
            // bounded candidate scan, preserving minority failures without
            // sacrificing ordinary evidence-aware ranking.
            limit: MAX_EXPERIENCE_SEARCH_CANDIDATES,
            ..Default::default()
        };
        let reference = query.trim();
        let ranked = if ["experience:", "run:", "session:"]
            .iter()
            .any(|prefix| reference.starts_with(prefix))
        {
            store.retrieve_reference(&search, reference, outcome)?
        } else {
            store.retrieve_with_outcome(&search, outcome)?
        };
        let mut session_ids_by_run = BTreeMap::<String, Option<String>>::new();
        let mut results = Vec::with_capacity(max_results.min(ranked.len()));

        for ranked in ranked {
            let memory = ranked.memory;
            let Some(successful) = memory.success else {
                // A hypothesis without an objective outcome is neither a
                // successful strategy nor an established failed strategy.
                continue;
            };
            if outcome.is_some_and(|requested| requested != successful)
                || !source_reference_is_safe(&memory.id)
                || memory.repository_id != self.storage.workspace_dir().to_string_lossy().as_ref()
            {
                continue;
            }

            let mut source_run_ids = Vec::new();
            let mut source_session_ids = Vec::new();
            let mut unique_runs = BTreeSet::new();
            let mut unique_sessions = BTreeSet::new();
            let evidenced_runs: BTreeSet<&str> = memory
                .evidence
                .iter()
                .chain(&memory.test_results)
                .filter(|signal| {
                    signal.is_objective()
                        && matches!(
                            signal.verdict,
                            EvidenceVerdict::Passed | EvidenceVerdict::Failed
                        )
                })
                .filter_map(|signal| signal.source_run_id.as_deref())
                .filter(|source_run_id| {
                    source_reference_is_safe(source_run_id)
                        && memory
                            .source_run_ids
                            .iter()
                            .any(|declared| declared.as_str() == *source_run_id)
                })
                .collect();
            let prioritized_run = if let Some(requested_run) = reference.strip_prefix("run:") {
                memory.source_run_ids.iter().find(|source_run_id| {
                    source_run_id.as_str() == requested_run
                        && evidenced_runs.contains(source_run_id.as_str())
                })
            } else if let Some(requested_session) = reference.strip_prefix("session:") {
                let mut matched_run = None;
                for source_run_id in &memory.source_run_ids {
                    if !evidenced_runs.contains(source_run_id.as_str()) {
                        continue;
                    }
                    if source_session_for_run(&store, &mut session_ids_by_run, source_run_id)?
                        .as_deref()
                        == Some(requested_session)
                    {
                        matched_run = Some(source_run_id);
                        break;
                    }
                }
                matched_run
            } else {
                None
            };

            for source_run_id in prioritized_run
                .into_iter()
                .chain(memory.source_run_ids.iter())
            {
                if source_run_ids.len() >= MAX_EXPERIENCE_SEARCH_DETAILS {
                    break;
                }
                if !evidenced_runs.contains(source_run_id.as_str())
                    || !unique_runs.insert(source_run_id.clone())
                {
                    continue;
                }

                let session_id =
                    source_session_for_run(&store, &mut session_ids_by_run, source_run_id)?;

                source_run_ids.push(source_run_id.clone());
                if let Some(session_id) = session_id
                    && unique_sessions.insert(session_id.clone())
                {
                    source_session_ids.push(session_id);
                }
            }

            if reference
                .strip_prefix("run:")
                .is_some_and(|run_id| !unique_runs.contains(run_id))
                || reference
                    .strip_prefix("session:")
                    .is_some_and(|session_id| !unique_sessions.contains(session_id))
            {
                continue;
            }

            let evidence = experience_evidence_references(
                &memory.evidence,
                &memory.test_results,
                &unique_runs,
                &session_ids_by_run,
            );
            let expected_verdict = if successful { "passed" } else { "failed" };
            if !evidence
                .iter()
                .any(|signal| signal.verdict == expected_verdict)
            {
                continue;
            }
            let category = serde_json::to_value(memory.category)?
                .as_str()
                .unwrap_or("unknown")
                .to_owned();

            results.push(ExperienceSearchResult {
                id: memory.id,
                category,
                task_summary: bounded_experience_detail(&memory.task_summary),
                lesson: bounded_experience_detail(&memory.lesson),
                strategy: bounded_experience_detail(&memory.strategy),
                outcome: successful,
                confidence: memory.confidence.clamp(0.0, 1.0),
                score: ranked.score,
                failure_reason: memory
                    .failure_reason
                    .as_deref()
                    .map(bounded_experience_detail),
                what_worked: bounded_experience_details(&memory.what_worked),
                what_failed: bounded_experience_details(&memory.what_failed),
                tests_run: bounded_experience_details(&memory.tests_run),
                source_run_ids,
                source_session_ids,
                evidence,
            });

            if results.len() >= max_results {
                break;
            }
        }

        Ok(results)
    }

    /// Build a fully configured backend for a live session.
    ///
    /// Prefer this over calling `new()` + individual builder methods: it ensures
    /// session_id, embeddings, search config, and the file watcher are applied
    /// consistently at every call site (ToolBridge, first-turn injection,
    /// post-compaction recovery).  Using the factory eliminates the silent
    /// per-site drift where some paths got hybrid search while others fell back
    /// to FTS-only, and where `[memory.search]` config was effectively ignored.
    pub fn from_session_params(storage: MemoryStorage, params: &MemoryBackendParams) -> Self {
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut backend = Self::new(db_path, storage)
            .with_session_id(params.session_id.clone())
            .with_search_config(params.search_config.clone());
        backend.search_source = params.search_source;
        if let Some(ec) = &params.embed_config {
            backend = backend.with_embedding(
                ec.clone(),
                params.embed_base_url.clone(),
                params.embed_api_key.clone(),
            );
        }
        if let Some(w) = &params.watcher {
            backend = backend.with_watcher(w.clone(), params.stale_claim_secs);
        }
        backend.embedding_credentials = params.embedding_credentials.clone();
        backend
    }
}

/// Test-only field accessors.
///
/// These expose private fields so tests can assert that `from_session_params`
/// actually stored the values it was given, without routing through a full
/// runtime search call whose semantics override some config fields.
#[cfg(test)]
impl MemoryBackendImpl {
    /// Returns the session ID stored in this backend.
    pub fn session_id_for_test(&self) -> &str {
        &self.session_id
    }

    /// Returns the search config stored in this backend.
    pub fn search_config_for_test(&self) -> &xai_grok_config_types::MemorySearchConfig {
        &self.search_config
    }
}

#[async_trait::async_trait]
impl MemoryBackend for MemoryBackendImpl {
    #[tracing::instrument(name = "memory.search", skip_all, fields(
        session_id = %self.session_id, max_results, min_score,
    ))]
    async fn search(
        &self,
        query: &str,
        max_results: usize,
        min_score: f64,
    ) -> Result<Vec<MemorySearchResult>, Box<dyn std::error::Error + Send + Sync>> {
        // Open a MemoryIndex for this query (open-per-query, ~1ms).
        //
        // IMPORTANT: `MemoryIndex` is `Send` but `!Sync`, so `&MemoryIndex`
        // is `!Send`. To keep this future `Send`, we must never hold a
        // `&index` borrow across an `.await` point. The code below is
        // structured into sync phases (borrow &index) and async phases
        // (no &index borrow) to satisfy this constraint.
        let embed_dims = self.embed_config.as_ref().map_or(1024, |ec| ec.dimensions);
        let mut index = super::index::MemoryIndex::open_or_create(
            &self.db_path,
            self.storage.clone(),
            xai_grok_config_types::MemoryIndexConfig::default(),
            embed_dims,
        )
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(std::io::Error::other(e.to_string()))
        })?;

        // ── Sync phase 1: reindex dirty files, collect chunks needing embeddings ──
        let mut reindex_chunks: Vec<(String, String)> = Vec::new();
        let mut needs_release = false;
        // Watcher-sync telemetry data (populated inside the claim guard below).
        let mut watcher_sync_stats: Option<(usize, usize, std::time::Instant)> = None;
        if let Some(ref watcher) = self.watcher
            && watcher.is_dirty()
            && index.try_claim_reindex(self.stale_claim_secs)
        {
            needs_release = true;
            let sync_start = std::time::Instant::now();
            let dirty_files = watcher.take_dirty();
            let dirty_count = dirty_files.len();
            // Sum of all index-chunk changes this cycle: chunks added/updated/
            // removed during reindex_file, plus chunks removed by delete_path.
            // Using one counter rather than two prevents telemetry from
            // under-reporting delete-only syncs (where reindex_file is never
            // called and the old `reindexed_count` would stay at 0).
            let mut changed_chunk_count: usize = 0;
            for file in &dirty_files {
                if file.exists() {
                    // File was created or modified — reindex it.
                    let source = self.storage.classify_source(file);
                    if let Ok(stats) = index.reindex_file(file, source) {
                        changed_chunk_count += stats.added + stats.updated + stats.removed;
                    }
                } else {
                    // File was deleted — remove its stale chunks from the index so
                    // they are no longer searchable.  Without this call, reindex_file
                    // returns early when the file is unreadable and leaves orphaned
                    // chunks behind indefinitely.
                    if let Ok(n) = index.delete_path(file) {
                        changed_chunk_count += n;
                    }
                }
            }
            if dirty_count > 0 {
                reindex_chunks = index.chunks_without_embeddings().unwrap_or_default();
            }
            watcher_sync_stats = Some((dirty_count, changed_chunk_count, sync_start));
        }

        // ── Async phase: embed missing chunks (no &index borrow) ──
        let provider = self.make_embedding_provider().await;
        let mut embedded_count: usize = 0;
        if !reindex_chunks.is_empty()
            && let Some(ref provider) = provider
        {
            let mut upserts: Vec<(String, Vec<f32>)> = Vec::new();
            for batch in reindex_chunks.chunks(32) {
                let texts: Vec<&str> = batch.iter().map(|(_, t)| t.as_str()).collect();
                match provider.embed_batch(&texts).await {
                    Ok(embeddings) => {
                        for ((chunk_id, _), emb) in batch.iter().zip(embeddings.into_iter()) {
                            upserts.push((chunk_id.clone(), emb));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: xai_grok_telemetry::memory_log::TARGET,
                            error = %e,
                            "embedding batch failed during sync-on-search, skipping"
                        );
                    }
                }
            }
            // Sync: upsert embeddings back (borrows &index, no await)
            for (chunk_id, emb) in &upserts {
                let _ = index.upsert_embedding(chunk_id, emb);
            }
            embedded_count = upserts.len();
        }
        if needs_release {
            index.release_claim();
            // Fire watcher-sync telemetry now that we know the embedded count.
            if let Some((dirty_count, reindexed_count, sync_start)) = watcher_sync_stats {
                xai_grok_telemetry::session_ctx::log_event(
                    xai_grok_telemetry::memory_telemetry::MemoryWatcherSync {
                        session_id: self.session_id.clone(),
                        dirty_file_count: dirty_count,
                        claimed: true,
                        reindexed_count,
                        embedded_count,
                        duration_ms: sync_start.elapsed().as_millis() as u64,
                    },
                );
            }
        }

        // ── Sync phase 2: FTS search ──
        let mut search_config = self.search_config.clone();
        search_config.max_results = max_results;
        search_config.min_score = min_score as f32;

        let search_start = std::time::Instant::now();
        let keyword_count = super::query_expansion::extract_keywords(query).len();
        let candidate_limit = search_config.max_results * 3;
        let mut fts_results = index.search_fts(query, candidate_limit).unwrap_or_default();

        // Supplemental evergreen query: ensure global/workspace MEMORY.md
        // chunks appear in candidates even when session volume crowds them
        // out of the base FTS results. Mirrors hybrid_search() in search.rs.
        let evergreen = index
            .search_fts_by_sources(query, candidate_limit, &["global", "workspace"])
            .unwrap_or_default();
        let existing: std::collections::HashSet<String> =
            fts_results.iter().map(|r| r.chunk_id.clone()).collect();
        for r in evergreen {
            if !existing.contains(&r.chunk_id) {
                fts_results.push(r);
            }
        }

        let vec_available = index.vec_available() && provider.is_some();

        // ── Async phase: embed query for vector search (no &index borrow) ──
        let query_embedding = if vec_available {
            if let Some(ref provider) = provider {
                match provider.embed_batch(&[query]).await {
                    Ok(embeddings) if !embeddings.is_empty() => {
                        Some(embeddings.into_iter().next().unwrap())
                    }
                    Ok(_) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, "embedding query failed, falling back to FTS-only");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // ── Sync phase 3: vector search + scoring + merge (borrows &index) ──
        let results = super::search::hybrid_search_merge(
            &index,
            fts_results,
            query_embedding.as_deref(),
            &search_config,
        )
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(std::io::Error::other(e.to_string()))
        })?;

        // Record accesses for the returned chunks so access_count and
        // last_accessed stay current.  Non-fatal: a failed write is a no-op
        // for the caller and does not affect the search response.
        for result in &results {
            let _ = index.record_access(&result.chunk_id);
        }

        let duration_ms = search_start.elapsed().as_millis() as u64;
        // A configured vector index is not enough to call this hybrid: if the
        // query embedding request failed, scoring actually ran FTS-only.
        let search_mode = if query_embedding.is_some() {
            "hybrid"
        } else {
            "fts_only"
        };
        let top_score = results.first().map_or(0.0, |r| r.score);

        if results.is_empty() {
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::memory_telemetry::MemorySearchEmpty {
                    session_id: self.session_id.clone(),
                    query_length: query.len(),
                    keyword_count,
                    min_score_threshold: min_score,
                    search_mode: search_mode.to_owned(),
                    duration_ms,
                    vec_available,
                    source: self.search_source.to_owned(),
                },
            );
        } else {
            xai_grok_telemetry::session_ctx::log_event(
                xai_grok_telemetry::memory_telemetry::MemorySearch {
                    session_id: self.session_id.clone(),
                    query_length: query.len(),
                    keyword_count,
                    result_count: results.len(),
                    top_score,
                    min_score_threshold: min_score,
                    search_mode: search_mode.to_owned(),
                    duration_ms,
                    vec_available,
                    source: self.search_source.to_owned(),
                },
            );
        }
        self.search_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        Ok(results
            .into_iter()
            .map(|r| MemorySearchResult {
                chunk_id: r.chunk_id,
                path: r.path,
                start_line: r.start_line,
                end_line: r.end_line,
                score: r.score,
                snippet: r.snippet,
                source: r.source,
                created_at: Some(r.created_at),
            })
            .collect())
    }

    fn search_experiences(
        &self,
        query: &str,
        max_results: usize,
        outcome: Option<bool>,
    ) -> Result<Vec<ExperienceSearchResult>, Box<dyn std::error::Error + Send + Sync>> {
        let results = self
            .search_experience_records(query, max_results, outcome)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.search_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(results)
    }

    fn get(
        &self,
        path: &str,
        from: Option<usize>,
        lines: Option<usize>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        Ok(self.storage.read_file(Path::new(path), from, lines)?)
    }

    fn total_chunks(&self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let conn = self.open_readonly()?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        Ok(count as usize)
    }

    /// Return the configured `max_results` from the stored search config.
    ///
    /// Overrides the trait default so the `memory_search` tool honours
    /// `[memory.search].max_results` from config when the model does not
    /// supply an explicit value.
    fn default_search_max_results(&self) -> usize {
        self.search_config.max_results
    }

    /// Return the configured `min_score` from the stored search config.
    fn default_search_min_score(&self) -> f64 {
        self.search_config.min_score as f64
    }
}

fn bounded_experience_detail(text: &str) -> String {
    redact_sensitive_text(text)
        .chars()
        .take(MAX_EXPERIENCE_SEARCH_FIELD_CHARS)
        .collect()
}

fn source_session_for_run(
    store: &ExperienceStore,
    cached_sessions: &mut BTreeMap<String, Option<String>>,
    source_run_id: &str,
) -> anyhow::Result<Option<String>> {
    match cached_sessions.get(source_run_id) {
        Some(session_id) => Ok(session_id.clone()),
        None => {
            let session_id = store.source_session_id(source_run_id)?;
            cached_sessions.insert(source_run_id.to_owned(), session_id.clone());
            Ok(session_id)
        }
    }
}

fn bounded_experience_details(details: &[String]) -> Vec<String> {
    details
        .iter()
        .take(MAX_EXPERIENCE_SEARCH_DETAILS)
        .map(|detail| bounded_experience_detail(detail))
        .collect()
}

fn experience_evidence_references(
    evidence: &[EvidenceSignal],
    test_results: &[EvidenceSignal],
    declared_runs: &BTreeSet<String>,
    session_ids_by_run: &BTreeMap<String, Option<String>>,
) -> Vec<ExperienceEvidenceReference> {
    let mut unique = BTreeSet::new();
    let mut references = Vec::new();

    for signal in evidence.iter().chain(test_results) {
        if references.len() >= MAX_EXPERIENCE_SEARCH_DETAILS {
            break;
        }
        if !signal.is_objective()
            || !matches!(
                signal.verdict,
                EvidenceVerdict::Passed | EvidenceVerdict::Failed
            )
        {
            continue;
        }
        let Some(source_run_id) = signal
            .source_run_id
            .as_ref()
            .filter(|source_run_id| declared_runs.contains(source_run_id.as_str()))
        else {
            continue;
        };

        let kind = serde_json::to_value(signal.kind)
            .ok()
            .and_then(|kind| kind.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());
        let verdict = serde_json::to_value(signal.verdict)
            .ok()
            .and_then(|verdict| verdict.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_owned());
        let command = signal.command.as_deref().map(bounded_experience_detail);
        let summary = bounded_experience_detail(&signal.summary);
        if !unique.insert((
            kind.clone(),
            verdict.clone(),
            command.clone(),
            summary.clone(),
            source_run_id.clone(),
        )) {
            continue;
        }

        references.push(ExperienceEvidenceReference {
            kind,
            verdict,
            command,
            summary,
            observed_at: signal.observed_at,
            source_run_id: Some(source_run_id.clone()),
            source_session_id: session_ids_by_run.get(source_run_id).cloned().flatten(),
        });
    }

    references
}

#[cfg(test)]
mod factory_tests {
    use super::*;
    use crate::index::{MemoryIndex, init_sqlite_vec};
    use crate::storage::MemoryStorage;
    use tempfile::TempDir;
    use xai_grok_config_types::{MemoryEmbeddingConfig, MemorySearchConfig};

    fn make_storage(tmp: &TempDir) -> MemoryStorage {
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        MemoryStorage::with_paths(global, workspace)
    }

    fn make_params_fts_only(session_id: &str) -> MemoryBackendParams {
        MemoryBackendParams {
            session_id: session_id.to_string(),
            embed_config: None,
            embed_base_url: String::new(),
            embed_api_key: None,
            search_config: MemorySearchConfig::default(),
            watcher: None,
            stale_claim_secs: 60,
            search_source: "tool",
            embedding_credentials: EndpointScopedCredentials::none(),
        }
    }

    /// from_session_params stores the session_id it was given.
    ///
    /// Direct assertion via the `#[cfg(test)]` accessor proves the factory
    /// actually stored the value rather than discarding it.  The counter
    /// increment check additionally confirms the backend is functional.
    #[tokio::test]
    async fn test_factory_sets_session_id() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            xai_grok_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let file = tmp.path().join("note.md");
        std::fs::write(&file, "# Facts\n\nRust is fast.").unwrap();
        idx.reindex_file(&file, "workspace").unwrap();
        drop(idx);

        let params = make_params_fts_only("test-session-abc");
        let backend = MemoryBackendImpl::from_session_params(storage, &params);

        // Direct assertion: the stored session_id matches what the factory was given.
        assert_eq!(
            backend.session_id_for_test(),
            "test-session-abc",
            "session_id must be stored exactly as supplied"
        );

        // Functional check: the backend actually runs a search.
        let before = backend
            .search_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        let _ = backend.search("rust", 5, 0.0).await;
        let after = backend
            .search_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after,
            before + 1,
            "search counter must increment per search"
        );
    }

    /// from_session_params stores the search_config it was given.
    ///
    /// Direct assertion via the `#[cfg(test)]` accessor proves the factory
    /// propagated the config into the backend rather than discarding it.
    /// `max_results` is verified because the `search()` method overrides it
    /// with the caller's argument — so checking the *stored* value is the only
    /// way to confirm the factory wired it correctly.
    #[tokio::test]
    async fn test_factory_wires_search_config() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            xai_grok_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        for i in 0..10 {
            let f = tmp.path().join(format!("note{i}.md"));
            std::fs::write(&f, format!("# Entry {i}\n\nRust tip number {i}.")).unwrap();
            idx.reindex_file(&f, "workspace").unwrap();
        }
        drop(idx);

        let params = MemoryBackendParams {
            search_config: MemorySearchConfig {
                max_results: 3,
                ..Default::default()
            },
            ..make_params_fts_only("test-search-config")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);

        // Direct: the stored config has exactly the value the factory was given.
        assert_eq!(
            backend.search_config_for_test().max_results,
            3,
            "stored max_results must equal what was supplied to the factory"
        );
    }

    /// from_session_params wires non-overridable config fields (MMR, temporal decay)
    /// that `search()` never replaces with caller arguments.
    ///
    /// This is the clearest proof that `[memory.search]` config is actually wired
    /// rather than silently ignored: fields the caller cannot override must arrive
    /// in the stored search_config exactly as given.
    #[test]
    fn test_factory_wires_non_overridable_search_config_fields() {
        let tmp = TempDir::new().unwrap();
        let storage = make_storage(&tmp);

        let custom_search = MemorySearchConfig {
            max_results: 7,
            mmr: xai_grok_config_types::MmrConfig {
                enabled: true,
                lambda: 0.42,
            },
            temporal_decay: xai_grok_config_types::TemporalDecayConfig {
                enabled: true,
                half_life_days: 14.0,
            },
            ..Default::default()
        };
        let params = MemoryBackendParams {
            search_config: custom_search,
            ..make_params_fts_only("test-full-config")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        let stored = backend.search_config_for_test();

        // None of these are overridden by the caller in search() — they must
        // survive the factory path unchanged.
        assert_eq!(stored.max_results, 7);
        assert!(stored.mmr.enabled, "MMR enabled must be stored");
        assert!(
            (stored.mmr.lambda - 0.42).abs() < f64::EPSILON,
            "MMR lambda must be stored exactly"
        );
        assert!(
            stored.temporal_decay.enabled,
            "temporal_decay enabled must be stored"
        );
        assert!(
            (stored.temporal_decay.half_life_days - 14.0).abs() < f64::EPSILON,
            "temporal_decay half_life_days must be stored exactly"
        );
    }

    /// from_session_params propagates search_source into the backend.
    ///
    /// Correctness test: every caller (tool, injection,
    /// compaction_recovery) must be able to set a distinct source label so
    /// dashboards can separate the three search paths.
    #[test]
    fn test_factory_propagates_search_source() {
        let tmp = TempDir::new().unwrap();
        let storage = make_storage(&tmp);

        for source in ["tool", "injection", "compaction_recovery"] {
            let params = MemoryBackendParams {
                search_source: source,
                ..make_params_fts_only("test-source")
            };
            let backend = MemoryBackendImpl::from_session_params(storage.clone(), &params);
            assert_eq!(
                backend.search_source, source,
                "search_source must be propagated for source='{source}'"
            );
        }
    }

    /// The default search_source is "tool" when constructing via new().
    #[test]
    fn test_default_search_source_is_tool() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage = make_storage(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);
        assert_eq!(backend.search_source, "tool");
    }

    /// MemoryBackendParams with different search_source values is Clone.
    #[test]
    fn test_params_clone_preserves_search_source() {
        let params = MemoryBackendParams {
            search_source: "injection",
            ..make_params_fts_only("test-clone-source")
        };
        let cloned = params.clone();
        assert_eq!(cloned.search_source, "injection");
    }

    /// Watcher startup telemetry reflects actual runtime state.
    ///
    /// `watcher.is_some()` is `true` only when the watcher started successfully.
    /// With a valid directory the watcher should start; without one it should return None.
    /// This guards the contract that `watcher_started` in telemetry must reflect
    /// runtime outcome, not configuration intent.
    #[test]
    fn test_params_watcher_started_reflects_runtime() {
        let tmp = TempDir::new().unwrap();

        // Success path: directory exists → watcher starts.
        let watch_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&watch_dir).unwrap();
        let watcher = crate::watcher::MemoryFileWatcher::start(&watch_dir);
        let params_with_watcher = MemoryBackendParams {
            watcher: watcher.map(std::sync::Arc::new),
            ..make_params_fts_only("test-watcher-runtime")
        };
        // watcher.is_some() reflects whether startup succeeded.
        // (On environments without inotify/FSEvents this may be None; skip rather than fail.)
        let _ = params_with_watcher.watcher.is_some(); // just verify it compiles

        // Failure path: non-existent directory → watcher must return None.
        let missing = tmp.path().join("does_not_exist");
        let no_watcher = crate::watcher::MemoryFileWatcher::start(&missing);
        assert!(
            no_watcher.is_none(),
            "watcher must return None for a non-existent directory"
        );
        let params_no_watcher = MemoryBackendParams {
            watcher: None,
            ..make_params_fts_only("test-no-watcher")
        };
        assert!(
            params_no_watcher.watcher.is_none(),
            "params.watcher.is_none() means telemetry reports watcher_started=false"
        );
    }

    /// default_search_max_results returns the configured value from search_config.
    ///
    /// Verifies that the MemoryBackend trait override in MemoryBackendImpl
    /// exposes search_config.max_results rather than the hardcoded default (6).
    #[test]
    fn test_default_search_max_results_from_config() {
        let tmp = TempDir::new().unwrap();
        let storage = make_storage(&tmp);

        let params = MemoryBackendParams {
            search_config: MemorySearchConfig {
                max_results: 12,
                ..Default::default()
            },
            ..make_params_fts_only("test-defaults")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        assert_eq!(
            backend.default_search_max_results(),
            12,
            "default_search_max_results must return search_config.max_results"
        );
    }

    /// default_search_min_score returns the configured value from search_config.
    #[test]
    fn test_default_search_min_score_from_config() {
        let tmp = TempDir::new().unwrap();
        let storage = make_storage(&tmp);

        let params = MemoryBackendParams {
            search_config: MemorySearchConfig {
                min_score: 0.42,
                ..Default::default()
            },
            ..make_params_fts_only("test-defaults")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        assert!(
            (backend.default_search_min_score() - 0.42_f64).abs() < 1e-6,
            "default_search_min_score must return search_config.min_score"
        );
    }

    /// from_session_params without embed_config produces a backend that does not panic
    /// and returns results using FTS-only path.
    #[tokio::test]
    async fn test_factory_fts_only_without_embed() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            xai_grok_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let f = tmp.path().join("note.md");
        std::fs::write(&f, "# Guide\n\nRust ownership rules.").unwrap();
        idx.reindex_file(&f, "workspace").unwrap();
        drop(idx);

        let params = make_params_fts_only("test-fts-only");
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        let results = backend.search("rust ownership", 5, 0.0).await.unwrap();
        assert!(
            !results.is_empty(),
            "FTS-only backend should return results"
        );
        let ts = results[0].created_at;
        assert!(
            ts.is_some() && ts.unwrap() > 0,
            "created_at must be Some(positive) after backend search (got {ts:?})"
        );
    }

    /// from_session_params with embed_config but no api_key gracefully falls back
    /// to FTS-only (the embedding provider requires a key).
    #[tokio::test]
    async fn test_factory_embed_config_without_key_falls_back_to_fts() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            xai_grok_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let f = tmp.path().join("note.md");
        std::fs::write(&f, "# Guide\n\nRust borrow checker.").unwrap();
        idx.reindex_file(&f, "workspace").unwrap();
        drop(idx);

        let params = MemoryBackendParams {
            embed_config: Some(MemoryEmbeddingConfig {
                model: Some("embed-test".to_owned()),
                ..Default::default()
            }),
            embed_base_url: "http://localhost".to_string(),
            embed_api_key: None, // no key → provider cannot be created
            ..make_params_fts_only("test-embed-no-key")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        // Must not panic; FTS results should still come back.
        let results = backend.search("rust borrow", 5, 0.0).await.unwrap();
        assert!(
            !results.is_empty(),
            "should fall back to FTS when api_key is None"
        );
    }

    /// MemoryBackendParams is Clone.
    #[test]
    fn test_params_is_clone() {
        let params = make_params_fts_only("clone-test");
        let _cloned = params.clone();
    }

    /// from_session_params without watcher produces a backend that searches correctly.
    #[tokio::test]
    async fn test_factory_no_watcher() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let storage = make_storage(&tmp);
        let db_path = storage.workspace_dir().join("index.sqlite");
        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage.clone(),
            xai_grok_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let f = tmp.path().join("note.md");
        std::fs::write(&f, "# Tip\n\nAlways write tests.").unwrap();
        idx.reindex_file(&f, "workspace").unwrap();
        drop(idx);

        let params = MemoryBackendParams {
            watcher: None,
            ..make_params_fts_only("test-no-watcher")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);
        let results = backend.search("tests", 5, 0.0).await.unwrap();
        assert!(
            !results.is_empty(),
            "no-watcher backend should still return results"
        );
    }

    /// `ensure_initialized` must be called before watcher startup.
    ///
    /// Regression test for the ordering fix: on a first-use machine the
    /// memory directories do not exist yet.  If the watcher tries to watch a
    /// non-existent directory it returns `None` (silently dropping the feature).
    /// After `ensure_initialized()` the directories exist and the watcher can
    /// start successfully.
    ///
    /// This mirrors the ordering enforced in `spawn_session_actor`:
    ///   1. `storage.ensure_initialized()`
    ///   2. `MemoryFileWatcher::start(storage.global_dir())`
    #[test]
    fn test_ensure_initialized_before_watcher_ordering() {
        let tmp = TempDir::new().unwrap();
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        let storage = MemoryStorage::with_paths(global.clone(), workspace.clone());

        // Precondition: neither directory exists yet (fresh machine simulation).
        assert!(
            !global.exists(),
            "global memory dir must not exist before initialization"
        );

        // --- Wrong ordering (watcher before init) ---
        // The watcher returns None because the directory does not exist.
        let watcher_before_init = crate::watcher::MemoryFileWatcher::start(&global);
        assert!(
            watcher_before_init.is_none(),
            "watcher must fail (None) when directory does not exist yet"
        );

        // --- Correct ordering (init, then watcher) ---
        // After ensure_initialized the directories and MEMORY.md templates exist.
        storage.ensure_initialized().unwrap();

        assert!(
            global.exists(),
            "global dir must exist after ensure_initialized"
        );
        assert!(
            workspace.exists(),
            "workspace dir must exist after ensure_initialized"
        );
        assert!(
            global.join("MEMORY.md").exists(),
            "global MEMORY.md template must exist"
        );
        assert!(
            workspace.join("MEMORY.md").exists(),
            "workspace MEMORY.md template must exist"
        );

        // Watcher now succeeds because the directory exists.
        // (Allowed to return None in environments without inotify/kqueue
        //  support — e.g. some CI containers — but must not error-panic.)
        let watcher_after_init = crate::watcher::MemoryFileWatcher::start(&global);
        // If a watcher was returned we can confirm it is usable (not dirty yet).
        if let Some(w) = watcher_after_init {
            assert!(
                !w.is_dirty(),
                "freshly started watcher must report no dirty files"
            );
        }
        // If None, the test environment does not support file-watching —
        // that is acceptable; the directories themselves are what matter here.
    }

    /// End-to-end regression test for the watcher-driven delete path.
    ///
    /// Tests the full chain:
    ///   1. file is indexed
    ///   2. watcher is started
    ///   3. first `backend.search()` confirms content is found
    ///   4. file is deleted (OS fires a Remove event to the watcher)
    ///   5. second `backend.search()` triggers sync-on-search, which calls
    ///      `delete_path()` because the file no longer exists
    ///   6. content is no longer returned
    ///
    /// This test guards against regressions in the `file.exists() → else
    /// delete_path()` branch that would be invisible to the `delete_path`
    /// unit tests alone.
    #[tokio::test]
    async fn test_watcher_delete_clears_stale_chunks() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();

        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();

        let storage = MemoryStorage::with_paths(global.clone(), workspace);
        let db_path = storage.workspace_dir().join("index.sqlite");

        // Step 1: Write + canonicalize the file path BEFORE indexing.
        //
        // On macOS, TempDir paths may live under /private/tmp (via a symlink
        // from /tmp).  FSEvents returns canonicalized paths, so the path stored
        // in the index must match what the watcher event delivers.
        let file_raw = global.join("note.md");
        std::fs::write(&file_raw, "# Unique\n\nXyzzy-watcher-delete-token.").unwrap();
        let file = dunce::canonicalize(&file_raw).unwrap_or(file_raw);

        {
            let mut idx = MemoryIndex::open_or_create(
                &db_path,
                storage.clone(),
                xai_grok_config_types::MemoryIndexConfig::default(),
                4,
            )
            .unwrap();
            // Index with the canonical path so DB key matches watcher event paths.
            idx.reindex_file(&file, "workspace").unwrap();
        }

        // Step 2: Start watcher AFTER indexing so the Remove event for the
        // upcoming deletion is the first event the watcher ever sees.
        let watch_dir = dunce::canonicalize(&global).unwrap_or(global.clone());
        let watcher = match crate::watcher::MemoryFileWatcher::start(&watch_dir) {
            Some(w) => w,
            None => {
                // File-watching not supported in this environment (e.g., some CI
                // containers without inotify/FSEvents).  Skip rather than fail.
                return;
            }
        };
        let watcher_arc = std::sync::Arc::new(watcher);

        let params = MemoryBackendParams {
            watcher: Some(watcher_arc.clone()),
            ..make_params_fts_only("test-watcher-delete")
        };
        let backend = MemoryBackendImpl::from_session_params(storage, &params);

        // Step 3: Confirm content is found before deletion.
        let before = backend
            .search("Xyzzy-watcher-delete-token", 5, 0.0)
            .await
            .unwrap();
        assert!(
            !before.is_empty(),
            "content must be found before file is deleted"
        );

        // Step 4: Delete the file — the OS will fire a Remove event.
        std::fs::remove_file(&file).unwrap();

        // Poll until the watcher detects the event (more reliable than a fixed
        // sleep on macOS where FSEvents delivery time varies considerably).
        // Give up after 2 s and skip the timing-sensitive assertion rather than
        // flake — delete_path unit tests cover the underlying logic.
        let mut event_delivered = false;
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if watcher_arc.is_dirty() {
                event_delivered = true;
                break;
            }
        }
        if !event_delivered {
            // FSEvents not delivered within 2 s — environment is too slow.
            // Skip silently; the logic is covered by delete_path unit tests.
            return;
        }

        // Step 5+6: search triggers sync-on-search, which detects file.exists()
        // == false and calls delete_path(), clearing all stale chunks.
        let after = backend
            .search("Xyzzy-watcher-delete-token", 5, 0.0)
            .await
            .unwrap();
        assert!(
            after.is_empty(),
            "deleted file's content must not appear after watcher-driven delete sync"
        );
    }

    /// Regression: provider build must use `current_api_key_async`,
    /// never sync. Prevents memory_search 401s on rotated tokens.
    #[tokio::test]
    async fn make_embedding_provider_uses_async_api_key_resolution() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use xai_grok_tools::types::ApiKeyProvider;

        struct AsyncProbe {
            sync_calls: Arc<AtomicU32>,
            async_calls: Arc<AtomicU32>,
        }
        impl ApiKeyProvider for AsyncProbe {
            fn current_api_key(&self) -> Option<String> {
                self.sync_calls.fetch_add(1, Ordering::SeqCst);
                Some("sync-stale".into())
            }
            fn current_api_key_async(
                &self,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + Send + '_>>
            {
                let counter = self.async_calls.clone();
                Box::pin(async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Some("async-fresh".into())
                })
            }
        }

        let sync_calls = Arc::new(AtomicU32::new(0));
        let async_calls = Arc::new(AtomicU32::new(0));
        let probe: xai_grok_tools::types::SharedApiKeyProvider = Arc::new(AsyncProbe {
            sync_calls: sync_calls.clone(),
            async_calls: async_calls.clone(),
        });

        let params = MemoryBackendParams {
            session_id: "s1".into(),
            embed_config: Some(MemoryEmbeddingConfig {
                model: Some("test-embed-model".into()),
                ..Default::default()
            }),
            embed_base_url: "http://example/v1".into(),
            embed_api_key: Some("static-fallback".into()),
            search_config: MemorySearchConfig::default(),
            watcher: None,
            stale_claim_secs: 60,
            search_source: "tool",
            // Trusted endpoint + no auth_credentials exercises the api_key_provider path.
            embedding_credentials: EndpointScopedCredentials::for_endpoint(
                "http://example/v1",
                |_| true,
                None,
                Some(probe),
            ),
        };

        let provider = params.make_embedding_provider().await;
        assert!(
            provider.is_some(),
            "provider must be built when model is set"
        );
        assert_eq!(
            async_calls.load(Ordering::SeqCst),
            1,
            "must call current_api_key_async exactly once per provider build"
        );
        assert_eq!(
            sync_calls.load(Ordering::SeqCst),
            0,
            "sync current_api_key must NOT be called — the async path is the contract"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experience::{EvidenceKind, ExperienceCategory, ExperienceMemory, ExperienceScope};
    use crate::index::{MemoryIndex, init_sqlite_vec};
    use tempfile::TempDir;
    use xai_grok_config_types::MemoryIndexConfig;

    /// An api-key provider that fails the test if its key is ever resolved,
    /// proving a scoped-away credential is never consulted.
    struct PanicKey;
    impl xai_grok_tools::types::ApiKeyProvider for PanicKey {
        fn current_api_key(&self) -> Option<String> {
            panic!("scoped-away credential must not be resolved");
        }
    }

    fn setup_index(tmp: &TempDir) -> (PathBuf, MemoryStorage) {
        init_sqlite_vec();
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        let storage = MemoryStorage::with_paths(global, workspace);
        let db_path = tmp.path().join("test.sqlite");

        let mut idx =
            MemoryIndex::open_or_create(&db_path, storage.clone(), MemoryIndexConfig::default(), 4)
                .unwrap();

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Guide\n\nRust programming tutorial.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        (db_path, storage)
    }

    fn setup_experience_backend(tmp: &TempDir) -> (MemoryBackendImpl, ExperienceStore) {
        let storage = MemoryStorage::new_flat(tmp.path(), &tmp.path().join("workspace-memory"));
        let database_path = storage.workspace_dir().join("index.sqlite");
        let store = ExperienceStore::open(&database_path).unwrap();
        let backend = MemoryBackendImpl::new(database_path, storage);
        (backend, store)
    }

    fn experience_fixture(
        backend: &MemoryBackendImpl,
        id: &str,
        source_run_id: &str,
        successful: bool,
    ) -> ExperienceMemory {
        let category = if successful {
            ExperienceCategory::SuccessfulPattern
        } else {
            ExperienceCategory::FailureAntiPattern
        };
        let now = chrono::Utc::now().timestamp();
        let mut experience = ExperienceMemory::new(
            category,
            "The parser visitor strategy produced an independently verified result",
            source_run_id,
            now,
        );
        experience.id = id.to_owned();
        experience.repository_id = backend
            .storage
            .workspace_dir()
            .to_string_lossy()
            .into_owned();
        experience.environment = super::super::experience::execution_environment();
        experience.scope = ExperienceScope::Repository;
        experience.task_type = "parser_change".to_owned();
        experience.task_summary = "Fix the parser visitor regression".to_owned();
        experience.strategy = "Extend the existing parser visitor".to_owned();
        experience.success = Some(successful);
        experience.tests_run = vec!["cargo test parser".to_owned()];
        experience.what_worked = if successful {
            vec!["Reused the existing parser visitor".to_owned()]
        } else {
            Vec::new()
        };
        experience.what_failed = if successful {
            Vec::new()
        } else {
            vec!["The original parser visitor skipped nested expressions".to_owned()]
        };
        experience.failure_reason = (!successful)
            .then(|| "Parser visitor regression still reproduced nested expressions".to_owned());
        experience.evidence = vec![EvidenceSignal {
            kind: EvidenceKind::Test,
            verdict: if successful {
                EvidenceVerdict::Passed
            } else {
                EvidenceVerdict::Failed
            },
            command: Some("cargo test parser".to_owned()),
            summary: if successful {
                "Parser visitor regression tests passed".to_owned()
            } else {
                "Parser visitor regression tests failed".to_owned()
            },
            score: Some(if successful { 1.0 } else { 0.0 }),
            observed_at: now,
            source_run_id: Some(source_run_id.to_owned()),
        }];
        experience.test_results = experience.evidence.clone();
        experience.refresh_confidence();
        experience
    }

    #[test]
    fn experience_search_returns_outcomes_details_and_resolved_source_sessions() {
        let temporary = TempDir::new().unwrap();
        let (backend, store) = setup_experience_backend(&temporary);
        store
            .upsert(&experience_fixture(
                &backend,
                "success-1",
                "run-success",
                true,
            ))
            .unwrap();
        store
            .upsert(&experience_fixture(
                &backend,
                "failure-1",
                "run-failure",
                false,
            ))
            .unwrap();
        store
            .record_source_session("run-success", "session-success")
            .unwrap();
        store
            .record_source_session("run-failure", "session-failure")
            .unwrap();

        let results = backend
            .search_experiences("parser visitor regression", 10, None)
            .unwrap();
        assert_eq!(results.len(), 2);

        let success = results
            .iter()
            .find(|result| result.id == "success-1")
            .unwrap();
        assert!(success.outcome);
        assert_eq!(success.category, "successful_pattern");
        assert_eq!(
            success.what_worked,
            vec!["Reused the existing parser visitor"]
        );
        assert_eq!(success.tests_run, vec!["cargo test parser"]);
        assert_eq!(success.source_run_ids, vec!["run-success"]);
        assert_eq!(success.source_session_ids, vec!["session-success"]);
        assert_eq!(success.evidence.len(), 1, "duplicate test signals collapse");
        assert_eq!(success.evidence[0].kind, "test");
        assert_eq!(success.evidence[0].verdict, "passed");
        assert_eq!(
            success.evidence[0].source_session_id.as_deref(),
            Some("session-success")
        );

        let failure = results
            .iter()
            .find(|result| result.id == "failure-1")
            .unwrap();
        assert!(!failure.outcome);
        assert_eq!(failure.category, "failure_anti_pattern");
        assert!(
            failure
                .failure_reason
                .as_deref()
                .unwrap()
                .contains("nested")
        );
        assert_eq!(failure.what_failed.len(), 1);
        assert_eq!(failure.evidence[0].verdict, "failed");
        assert_eq!(failure.source_session_ids, vec!["session-failure"]);
    }

    #[test]
    fn experience_search_keeps_legacy_run_references_without_inventing_sessions() {
        let temporary = TempDir::new().unwrap();
        let (backend, store) = setup_experience_backend(&temporary);
        store
            .upsert(&experience_fixture(
                &backend,
                "legacy-1",
                "legacy-run",
                true,
            ))
            .unwrap();

        let results = backend
            .search_experiences("parser visitor", 1, Some(true))
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_run_ids, vec!["legacy-run"]);
        assert!(results[0].source_session_ids.is_empty());
        assert_eq!(results[0].evidence[0].source_session_id, None);
        assert_eq!(
            backend
                .search_experiences("run:legacy-run", 1, None)
                .unwrap()[0]
                .id,
            "legacy-1",
            "legacy activation references remain resolvable without a session mapping"
        );
    }

    #[test]
    fn experience_search_resolves_exact_experience_run_and_session_references() {
        let temporary = TempDir::new().unwrap();
        let (backend, store) = setup_experience_backend(&temporary);
        store
            .upsert(&experience_fixture(
                &backend,
                "success-1",
                "shared-run",
                true,
            ))
            .unwrap();
        store
            .upsert(&experience_fixture(
                &backend,
                "failure-1",
                "shared-run",
                false,
            ))
            .unwrap();
        store
            .record_source_session("shared-run", "stable-session")
            .unwrap();

        let exact = backend
            .search_experiences("experience:success-1", 10, None)
            .unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].id, "success-1");
        assert!(
            backend
                .search_experiences("experience:success-1", 10, Some(false))
                .unwrap()
                .is_empty(),
            "exact reference lookup must still respect the outcome filter"
        );

        let from_run = backend
            .search_experiences("run:shared-run", 10, None)
            .unwrap();
        assert_eq!(from_run.len(), 2);

        let from_session = backend
            .search_experiences("session:stable-session", 10, None)
            .unwrap();
        assert_eq!(from_session.len(), 2);

        for invalid_reference in [
            "experience:",
            "experience:../stolen",
            "run:.",
            "run:nested/escape",
            "session:../stolen",
            "session:unmapped-session",
        ] {
            assert!(
                backend
                    .search_experiences(invalid_reference, 10, None)
                    .unwrap()
                    .is_empty(),
                "unknown or unsafe references must not expose experience details"
            );
        }
    }

    #[test]
    fn experience_search_rejects_declared_run_and_session_without_objective_evidence() {
        let temporary = TempDir::new().unwrap();
        let (backend, store) = setup_experience_backend(&temporary);
        let mut experience = experience_fixture(&backend, "backed-1", "verified-run", true);
        experience.source_run_ids.push("unsupported-run".to_owned());
        store.upsert(&experience).unwrap();
        store
            .record_source_session("verified-run", "verified-session")
            .unwrap();
        store
            .record_source_session("unsupported-run", "unsupported-session")
            .unwrap();

        let result = backend
            .search_experiences("experience:backed-1", 1, None)
            .unwrap()
            .remove(0);
        assert_eq!(result.source_run_ids, vec!["verified-run"]);
        assert_eq!(result.source_session_ids, vec!["verified-session"]);

        for unsupported_reference in ["run:unsupported-run", "session:unsupported-session"] {
            assert!(
                backend
                    .search_experiences(unsupported_reference, 10, None)
                    .unwrap()
                    .is_empty(),
                "an unsupported provenance declaration must not authorize reference lookup"
            );
        }
        assert_eq!(
            backend
                .search_experiences("run:verified-run", 1, None)
                .unwrap()[0]
                .id,
            "backed-1"
        );
    }

    #[test]
    fn experience_search_prioritizes_referenced_sources_beyond_the_visible_reference_limit() {
        let temporary = TempDir::new().unwrap();
        let (backend, store) = setup_experience_backend(&temporary);
        let mut experience = experience_fixture(&backend, "many-sources", "source-00", true);

        for index in 1..20 {
            let source_run = format!("source-{index:02}");
            experience.source_run_ids.push(source_run.clone());
            let mut evidence = experience.evidence[0].clone();
            evidence.source_run_id = Some(source_run);
            experience.evidence.push(evidence);
        }
        store.upsert(&experience).unwrap();

        for index in 0..20 {
            store
                .record_source_session(
                    &format!("source-{index:02}"),
                    &format!("session-{index:02}"),
                )
                .unwrap();
        }

        for (query, expected_run, expected_session) in [
            ("run:source-19", "source-19", "session-19"),
            ("session:session-18", "source-18", "session-18"),
        ] {
            let results = backend.search_experiences(query, 1, None).unwrap();
            assert_eq!(results.len(), 1, "late valid references must resolve");
            assert_eq!(
                results[0].source_run_ids.len(),
                MAX_EXPERIENCE_SEARCH_DETAILS
            );
            assert_eq!(results[0].source_run_ids[0], expected_run);
            assert_eq!(results[0].source_session_ids[0], expected_session);
            assert!(
                results[0]
                    .evidence
                    .iter()
                    .any(|signal| signal.source_run_id.as_deref() == Some(expected_run)),
                "the prioritized reference must retain supporting objective evidence"
            );
        }
    }

    #[test]
    fn experience_search_filters_outcomes_unknowns_foreign_records_and_unbacked_claims() {
        let temporary = TempDir::new().unwrap();
        let (backend, store) = setup_experience_backend(&temporary);
        store
            .upsert(&experience_fixture(
                &backend,
                "success-1",
                "run-success",
                true,
            ))
            .unwrap();
        store
            .upsert(&experience_fixture(
                &backend,
                "failure-1",
                "run-failure",
                false,
            ))
            .unwrap();

        let mut unknown = experience_fixture(&backend, "unknown-1", "run-unknown", true);
        unknown.category = ExperienceCategory::UncertainHypothesis;
        unknown.success = None;
        store.upsert(&unknown).unwrap();

        let mut foreign = experience_fixture(&backend, "foreign-1", "run-foreign", true);
        foreign.repository_id = "foreign-workspace".to_owned();
        foreign.scope = ExperienceScope::Global;
        foreign.generalizability = 0.97;
        for run in ["foreign-run-2", "foreign-run-3"] {
            foreign.source_run_ids.push(run.to_owned());
            let mut supporting = foreign.evidence[0].clone();
            supporting.source_run_id = Some(run.to_owned());
            foreign.evidence.push(supporting);
        }
        store.upsert(&foreign).unwrap();
        store
            .record_source_session("run-foreign", "foreign-session")
            .unwrap();

        let mut unsupported =
            experience_fixture(&backend, "unsupported-1", "run-unsupported", true);
        unsupported.task_type = "unsupported_claim".to_owned();
        unsupported.evidence[0].verdict = EvidenceVerdict::Failed;
        unsupported.test_results = unsupported.evidence.clone();
        store.upsert(&unsupported).unwrap();

        let successes = backend
            .search_experiences("parser visitor regression", 20, Some(true))
            .unwrap();
        assert_eq!(successes.len(), 1);
        assert_eq!(successes[0].id, "success-1");

        let failures = backend
            .search_experiences("parser visitor regression", 20, Some(false))
            .unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].id, "failure-1");

        for foreign_reference in [
            "experience:foreign-1",
            "run:run-foreign",
            "session:foreign-session",
        ] {
            assert!(
                backend
                    .search_experiences(foreign_reference, 20, None)
                    .unwrap()
                    .is_empty(),
                "direct references must never bypass workspace isolation"
            );
        }
    }

    #[test]
    fn experience_search_redacts_compromised_legacy_text_and_rejects_unsafe_references() {
        let temporary = TempDir::new().unwrap();
        let (backend, store) = setup_experience_backend(&temporary);
        let experience = experience_fixture(&backend, "secure-1", "safe-run", true);
        store.upsert(&experience).unwrap();

        let leaked_credential = "QWxhZGRpbjpPcGVuU2VzYW1l";
        let mut compromised = serde_json::to_value(&experience).unwrap();
        compromised["strategy"] =
            serde_json::Value::String(format!("Authorization: Basic {leaked_credential}"));
        compromised["tests_run"] = serde_json::json!([format!(
            "curl --header 'Authorization: Basic {leaked_credential}'"
        )]);
        compromised["evidence"][0]["summary"] =
            serde_json::Value::String(format!("Authorization: Basic {leaked_credential}"));
        compromised["test_results"] = serde_json::json!([]);
        compromised["source_run_ids"] = serde_json::json!(["../stolen-session", "safe-run"]);

        let connection = rusqlite::Connection::open(&backend.db_path).unwrap();
        connection
            .execute(
                "UPDATE experiences SET record_json = ?1 WHERE id = ?2",
                rusqlite::params![serde_json::to_string(&compromised).unwrap(), "secure-1"],
            )
            .unwrap();

        let results = backend
            .search_experiences("parser visitor", 1, Some(true))
            .unwrap();
        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert!(!result.strategy.contains(leaked_credential));
        assert!(!result.tests_run[0].contains(leaked_credential));
        assert!(!result.evidence[0].summary.contains(leaked_credential));
        assert_eq!(result.source_run_ids, vec!["safe-run"]);
        assert!(result.source_session_ids.is_empty());
    }

    #[test]
    fn experience_search_bounds_detail_lists_and_redacted_text() {
        let temporary = TempDir::new().unwrap();
        let (backend, store) = setup_experience_backend(&temporary);
        let mut experience = experience_fixture(&backend, "bounded-1", "bounded-run", true);
        experience.strategy = "s".repeat(MAX_EXPERIENCE_SEARCH_FIELD_CHARS + 100);
        experience.what_worked = (0..(MAX_EXPERIENCE_SEARCH_DETAILS + 5))
            .map(|index| format!("Successful parser visitor strategy {index}"))
            .collect();
        experience.tests_run = (0..(MAX_EXPERIENCE_SEARCH_DETAILS + 5))
            .map(|index| format!("cargo test parser_{index}"))
            .collect();
        store.upsert(&experience).unwrap();

        let results = backend
            .search_experiences("parser visitor", usize::MAX, Some(true))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].strategy.chars().count(),
            MAX_EXPERIENCE_SEARCH_FIELD_CHARS
        );
        assert_eq!(results[0].what_worked.len(), MAX_EXPERIENCE_SEARCH_DETAILS);
        assert_eq!(results[0].tests_run.len(), MAX_EXPERIENCE_SEARCH_DETAILS);
        assert!(
            backend
                .search_experiences("parser visitor", 0, None)
                .unwrap()
                .is_empty()
        );
        assert!(
            backend
                .search_experiences("  ", 10, None)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_backend_search() {
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        let results = backend.search("rust programming", 10, 0.0).await.unwrap();
        assert!(!results.is_empty(), "should find indexed content");
        assert!(results[0].snippet.contains("Rust"));
    }

    #[test]
    fn test_backend_total_chunks() {
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        let count = backend.total_chunks().unwrap();
        assert!(count >= 1, "should have at least 1 chunk");
    }

    #[test]
    fn test_backend_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MemoryBackendImpl>();
    }

    /// If credentials approved for one endpoint are used to build against a
    /// different URL (a wiring bug), they are dropped at build time rather than
    /// sent to the wrong endpoint. The session provider would panic if resolved.
    #[tokio::test]
    async fn test_build_drops_credentials_when_request_url_differs() {
        let session: xai_grok_tools::types::SharedApiKeyProvider = Arc::new(PanicKey);

        let scoped = EndpointScopedCredentials::for_endpoint(
            "https://api.x.ai/v1",
            |_| true,
            None,
            Some(session),
        );
        assert!(!scoped.is_empty(), "trusted endpoint keeps the credential");

        let config = xai_grok_config_types::MemoryEmbeddingConfig {
            model: Some("test-embedding-model".to_string()),
            ..Default::default()
        };
        let provider = build_embedding_provider(
            Some(&config),
            &scoped,
            Some("byok-static-key"),
            "https://other.example/v1",
        )
        .await;
        assert!(
            provider.is_some(),
            "mismatched request URL must fall back to the static key, not the scoped credential"
        );
    }

    /// A trusted, URL-matching endpoint builds the provider from the
    /// refresh-capable session credential and never consults the per-call
    /// api-key provider. The api-key provider panics if resolved.
    #[tokio::test]
    async fn test_trusted_endpoint_prefers_session_credential() {
        struct StubAuth;
        impl xai_grok_auth::HttpAuth for StubAuth {
            fn apply(
                &self,
                builder: reqwest::RequestBuilder,
                _base_url: &str,
            ) -> reqwest::RequestBuilder {
                builder
            }
        }
        #[async_trait::async_trait]
        impl xai_grok_auth::AuthCredentialProvider for StubAuth {
            fn snapshot(&self) -> xai_grok_auth::CredentialSnapshot {
                xai_grok_auth::CredentialSnapshot::default()
            }
            async fn refresh_after_unauthorized(&self) -> bool {
                false
            }
        }

        let auth: Arc<dyn xai_grok_auth::AuthCredentialProvider> = Arc::new(StubAuth);
        let api_key: xai_grok_tools::types::SharedApiKeyProvider = Arc::new(PanicKey);
        let scoped = EndpointScopedCredentials::for_endpoint(
            "https://api.x.ai/v1",
            |_| true,
            Some(auth),
            Some(api_key),
        );
        assert!(!scoped.is_empty(), "trusted endpoint keeps the credential");

        let config = xai_grok_config_types::MemoryEmbeddingConfig {
            model: Some("test-embedding-model".to_string()),
            ..Default::default()
        };
        let provider =
            build_embedding_provider(Some(&config), &scoped, None, "https://api.x.ai/v1").await;
        assert!(
            provider.is_some(),
            "trusted endpoint must build a provider from the session credential"
        );
    }

    #[test]
    fn endpoint_scoped_credentials_trust_gate_and_url_match() {
        struct AnyKey;
        impl xai_grok_tools::types::ApiKeyProvider for AnyKey {
            fn current_api_key(&self) -> Option<String> {
                None
            }
        }
        let key = || Arc::new(AnyKey) as xai_grok_tools::types::SharedApiKeyProvider;

        let denied = EndpointScopedCredentials::for_endpoint(
            "https://byok.example/v1",
            |_| false,
            None,
            Some(key()),
        );
        assert!(denied.is_empty(), "untrusted endpoint drops the credential");

        let scoped = EndpointScopedCredentials::for_endpoint(
            "https://api.x.ai/v1",
            |_| true,
            None,
            Some(key()),
        );
        assert!(!scoped.is_empty(), "trusted endpoint keeps the credential");
        assert!(
            scoped.approved_for("https://API.x.ai/v1"),
            "host casing normalizes"
        );
        assert!(
            !scoped.approved_for("https://api.x.ai/v2"),
            "different path rejected"
        );
        assert!(
            !scoped.approved_for("https://other.example/v1"),
            "different host rejected"
        );
        assert!(!scoped.approved_for("not-a-url"), "unparsable fails closed");
    }

    #[tokio::test]
    async fn test_search_with_punctuation_in_query() {
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        // Raw user message with punctuation — should not crash FTS5
        let results = backend
            .search("what is rust? how to use it!", 10, 0.0)
            .await
            .unwrap();
        assert!(
            !results.is_empty(),
            "should match 'rust' despite punctuation in query"
        );
    }

    #[tokio::test]
    async fn test_search_with_special_chars_only() {
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        // Query with only special chars — should return empty, not error
        let results = backend.search("???!!!", 10, 0.0).await.unwrap();
        assert!(
            results.is_empty(),
            "special-chars-only query should return empty"
        );
    }

    #[tokio::test]
    async fn test_search_hybrid_fts_only_fallback() {
        // Without embedding config, hybrid search should degrade to FTS-only
        let tmp = TempDir::new().unwrap();
        let (db_path, storage) = setup_index(&tmp);
        let backend = MemoryBackendImpl::new(db_path, storage);

        // Even with high min_score, hybrid search normalizes scores to [0,1]
        // so results above the threshold should be returned
        let results = backend.search("rust programming", 10, 0.0).await.unwrap();
        assert!(
            !results.is_empty(),
            "FTS-only fallback should still return results"
        );
        // Scores should be normalized (0,1] range from hybrid scoring
        assert!(results[0].score > 0.0, "hybrid scores should be positive");
    }

    /// The supplemental evergreen query in `search()` adds global/workspace
    /// candidates that the base `search_fts` missed due to candidate_limit.
    ///
    /// Tests the mechanism directly at the index level: verifies that with
    /// a tight FTS limit, global/workspace chunks are absent from the base
    /// results but present in the supplemental source-filtered query. Then
    /// confirms the full backend search pipeline surfaces them.
    #[tokio::test]
    async fn test_search_returns_global_and_workspace_memory() {
        let tmp = TempDir::new().unwrap();
        init_sqlite_vec();
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        let storage = MemoryStorage::with_paths(global, workspace);
        let db_path = storage.workspace_dir().join("index.sqlite");

        let mut idx =
            MemoryIndex::open_or_create(&db_path, storage.clone(), MemoryIndexConfig::default(), 4)
                .unwrap();

        // Index global + workspace with matching content.
        let global_file = tmp.path().join("global_mem.md");
        std::fs::write(
            &global_file,
            "# Preferences\n\nAlways use graphite for PRs. Prefer Rust over Python.",
        )
        .unwrap();
        idx.reindex_file(&global_file, "global").unwrap();

        let ws_file = tmp.path().join("ws_mem.md");
        std::fs::write(
            &ws_file,
            "# Project Decisions\n\nWe chose graphite for PRs in this project.",
        )
        .unwrap();
        idx.reindex_file(&ws_file, "workspace").unwrap();

        // Index session files that also match the query.
        for i in 0..5 {
            let f = tmp.path().join(format!("session_{i}.md"));
            std::fs::write(
                &f,
                format!("# Session {i}\n\nDiscussed graphite for PRs and item {i}."),
            )
            .unwrap();
            idx.reindex_file(&f, "session").unwrap();
        }

        // Verify the supplemental query mechanism: with a tight limit the
        // base FTS returns a mix, but `search_fts_by_sources` for
        // "global"/"workspace" always finds the evergreen chunks.
        let evergreen = idx
            .search_fts_by_sources("graphite PRs", 10, &["global", "workspace"])
            .unwrap();
        assert!(
            evergreen.len() >= 2,
            "supplemental evergreen query must find both global and workspace chunks"
        );
        let evergreen_sources: Vec<String> = evergreen
            .iter()
            .filter_map(|r| idx.get_chunk(&r.chunk_id).ok().flatten())
            .map(|c| c.source)
            .collect();
        assert!(
            evergreen_sources.contains(&"global".to_string()),
            "evergreen query must find global chunk"
        );
        assert!(
            evergreen_sources.contains(&"workspace".to_string()),
            "evergreen query must find workspace chunk"
        );
        drop(idx);

        // Full backend search: global/workspace must appear in results.
        let backend = MemoryBackendImpl::new(db_path, storage);
        let results = backend.search("graphite PRs", 10, 0.0).await.unwrap();

        let has_global = results.iter().any(|r| r.source == "global");
        let has_workspace = results.iter().any(|r| r.source == "workspace");
        assert!(
            has_global,
            "global MEMORY.md chunks must appear in search results"
        );
        assert!(
            has_workspace,
            "workspace MEMORY.md chunks must appear in search results"
        );
    }
}

#[cfg(test)]
mod index_embedding_tests {
    use crate::index::MemoryIndex;
    use crate::storage::MemoryStorage;

    #[test]
    fn test_chunks_without_embeddings() {
        let tmp = tempfile::TempDir::new().unwrap();
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        let storage = MemoryStorage::with_paths(global, workspace);
        let db_path = tmp.path().join("test.sqlite");

        let mut idx = MemoryIndex::open_or_create(
            &db_path,
            storage,
            xai_grok_config_types::MemoryIndexConfig::default(),
            4,
        )
        .unwrap();

        if !idx.vec_available() {
            // sqlite-vec not available — chunks_without_embeddings returns empty
            let missing = idx.chunks_without_embeddings().unwrap();
            assert!(missing.is_empty(), "no-vec: should return empty");
            return;
        }

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Title\n\nSome content here.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        // After reindex, chunks should exist but have no embeddings
        let missing = idx.chunks_without_embeddings().unwrap();
        assert!(
            !missing.is_empty(),
            "newly indexed chunks should be missing embeddings"
        );

        // After upserting an embedding, the chunk should disappear from missing
        let (chunk_id, _) = &missing[0];
        let dummy_embedding = vec![0.0f32; 4];
        idx.upsert_embedding(chunk_id, &dummy_embedding).unwrap();

        let missing_after = idx.chunks_without_embeddings().unwrap();
        assert_eq!(
            missing_after.len(),
            missing.len() - 1,
            "one fewer chunk should be missing after embedding"
        );
    }
}
