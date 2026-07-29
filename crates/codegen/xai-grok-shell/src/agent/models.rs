//! Model fetching, resolution, and management.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::RwLock;

use agent_client_protocol as acp;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use indexmap::IndexMap;

use crate::agent::config::{self, ModelEntry, resolve_credentials, sampling_config_for_model};
use crate::auth::{AuthManager, GrokAuth, GrokComConfig};
use crate::codex_models::{CodexCompactionMetadata, CodexModelsCatalog, CodexModelsClient};
use crate::deepseek_models::{DeepSeekModelsCatalog, DeepSeekModelsClient};
use crate::fireworks_models::{FireworksModelsCatalog, FireworksModelsClient};
use crate::kimi_models::{KimiApiEndpoint, KimiModelsCatalog, KimiModelsClient};
use crate::opencode_go_models::{
    OpenCodeGoModelDescriptor, OpenCodeGoModelsCatalog, OpenCodeGoModelsClient,
};
use crate::remote::{FetchModelsResult, fetch_models_blocking};
use crate::sampling::SamplerConfig as SamplingConfig;
use globset::{Glob, GlobSet, GlobSetBuilder};
use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption, ToolMode};

// ── Auth method for model fetching ──────────────────────────────────────────

/// Credential for `/v1/models` fetching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelFetchAuth {
    Session,
    ApiKey,
    Deployment,
    CustomEndpoint,
}

impl ModelFetchAuth {
    /// custom_endpoint > session > deployment > API key.
    ///
    /// A `deployment_key` outranks an ambient `XAI_API_KEY` so a stray env key
    /// can't redirect model fetching from the deployment's entitlement-gated
    /// proxy to a raw `/v1/models` endpoint that lists the full model registry.
    pub(crate) fn resolve(endpoints: &config::EndpointsConfig, has_cached_session: bool) -> Self {
        if endpoints.has_custom_endpoint() {
            Self::CustomEndpoint
        } else if has_cached_session {
            Self::Session
        } else if endpoints.deployment_key.is_some() {
            Self::Deployment
        } else if crate::agent::auth_method::has_xai_api_key_env() {
            Self::ApiKey
        } else {
            Self::Session
        }
    }

    pub(crate) fn cache_auth_method(&self) -> CacheAuthMethod {
        match self {
            Self::CustomEndpoint | Self::ApiKey => CacheAuthMethod::ApiKey,
            Self::Session => CacheAuthMethod::Session,
            Self::Deployment => CacheAuthMethod::Deployment,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CacheAuthMethod {
    Session,
    ApiKey,
    Deployment,
}

pub(crate) fn task_model_error_for_catalog_with_provider_auth(
    requested: &str,
    available: &IndexMap<String, ModelEntry>,
    has_xai_session: bool,
    has_codex_session: bool,
) -> Option<String> {
    let is_available = |entry: &ModelEntry| {
        entry.info.user_selectable
            && entry
                .info
                .visible_for_provider_auth(has_xai_session, has_codex_session)
    };
    if let Some(entry) = config::find_model_by_id(available, requested)
        && is_available(entry)
    {
        return task_model_credential_error(requested, entry);
    }

    let mut slugs = available
        .iter()
        .filter(|(_, entry)| is_available(entry))
        .map(|(slug, _)| slug.as_str())
        .collect::<Vec<_>>();
    slugs.sort_unstable();
    let guidance = if slugs.is_empty() {
        "No valid model slugs are currently available. Omit `model` to inherit the parent model."
            .to_string()
    } else {
        format!(
            "Valid model slugs: {}. Omit `model` to inherit the parent model.",
            slugs.join(", ")
        )
    };
    Some(format!("Unknown Task.model slug '{requested}'. {guidance}"))
}

/// Credential gate for spawning a child on an API-key-only provider.
///
/// A Kimi / Fireworks AI child without a usable key is doomed: it dies during
/// spawn setup in `refresh_kimi_sampling_config_for_spawn`, AFTER a
/// `background: true` task tool has already answered "Subagent started in
/// background" — so the caller sees a silent 0-turn failure and tends to fall
/// back to another model without telling the user. Rejecting the slug here
/// makes the task tool fail the call eagerly with an actionable message
/// instead. Mirrors the key-emptiness test in
/// `refresh_kimi_sampling_config_for_spawn`; that later check remains as
/// defense-in-depth against settings changing mid-spawn.
fn task_model_credential_error(requested: &str, entry: &ModelEntry) -> Option<String> {
    use xai_grok_sampling_types::ModelProvider;
    let provider = entry.info.provider;
    if !matches!(
        provider,
        ModelProvider::Kimi
            | ModelProvider::Fireworks
            | ModelProvider::DeepSeek
            | ModelProvider::OpenCodeGo
    ) {
        return None;
    }
    let credentials = config::resolve_credentials(entry, None);
    if credentials
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        return None;
    }
    Some(format!(
        "Model '{requested}' needs a {} API key, and none is configured. \
         Ask the user to add one in settings, or omit `model` to inherit the \
         parent model.",
        provider.name()
    ))
}

/// Thread-safe model manager.
///
/// Owns the auth manager, config, and gateway needed to refresh models.
/// Uses `parking_lot::RwLock` for short clone-and-release access.
#[derive(Clone)]
pub struct ModelsManager {
    inner: Arc<Inner>,
}

struct Inner {
    prefetched: RwLock<Option<IndexMap<String, ModelEntry>>>,
    /// Provider-isolated ChatGPT Codex catalog. This must never be stored in
    /// `prefetched`, which is owned by xAI's `/v1/models` transport.
    codex_catalog: RwLock<Option<CodexModelsCatalog>>,
    /// Provider-isolated Kimi catalog queried with only the Kimi API key.
    kimi_catalog: RwLock<Option<KimiModelsCatalog>>,
    /// Provider-isolated Fireworks catalog queried with only the Fireworks
    /// API key. The queried catalog only enriches the curated model list.
    fireworks_catalog: RwLock<Option<FireworksModelsCatalog>>,
    /// Provider-isolated direct DeepSeek catalog queried only with the
    /// DeepSeek credential.
    deepseek_catalog: RwLock<Option<DeepSeekModelsCatalog>>,
    opencode_go_catalog: RwLock<Option<OpenCodeGoModelsCatalog>>,
    models: RwLock<IndexMap<String, ModelEntry>>,
    current_model_id: RwLock<acp::ModelId>,
    current_reasoning_effort: RwLock<Option<ReasoningEffort>>,
    etag: RwLock<Option<String>>,
    /// Set once a real catalog has been fetched; gates whether
    /// `apply_refresh_result` calls `reselect_default_model` (first
    /// time) or `reselect_current_model_if_missing` (subsequent).
    /// Reset in `clear()` for identity changes.
    has_fetched_real_catalog: RwLock<bool>,
    // ── Owned context for self-contained refresh ────────────────
    auth_manager: Arc<AuthManager>,
    cfg: RwLock<config::Config>,
    fetch_auth: RwLock<ModelFetchAuth>,
    gateway: RwLock<Option<xai_acp_lib::AcpAgentGatewaySender>>,
    cache: ModelsCacheManager,
    codex_client: CodexModelsClient,
    kimi_client: RwLock<KimiModelsClient>,
    fireworks_client: FireworksModelsClient,
    deepseek_client: DeepSeekModelsClient,
    opencode_go_client: OpenCodeGoModelsClient,
    /// Serialize Codex cache/network refreshes. Session startup waits for an
    /// already-running refresh so it resolves against the same catalog rather
    /// than racing past the initial `OnlineIfUncached` request.
    codex_refresh_lock: tokio::sync::Mutex<()>,
    kimi_refresh_lock: tokio::sync::Mutex<()>,
    fireworks_refresh_lock: tokio::sync::Mutex<()>,
    deepseek_refresh_lock: tokio::sync::Mutex<()>,
    opencode_go_refresh_lock: tokio::sync::Mutex<()>,
    /// Invalidates a refresh result when Codex logout races an in-flight
    /// `/models` request. Logout increments this before clearing the cache;
    /// only a refresh that started in the current generation may publish.
    codex_catalog_generation: AtomicU64,
    /// Invalidates a Kimi model query when a catalog clear races its response.
    kimi_catalog_generation: AtomicU64,
    /// Invalidates a Fireworks model query when a catalog clear races its
    /// response.
    fireworks_catalog_generation: AtomicU64,
    deepseek_catalog_generation: AtomicU64,
    opencode_go_catalog_generation: AtomicU64,
    /// Guard to prevent overlapping retry loops.
    retry_in_flight: AtomicBool,
    /// Single-flight for the etag-triggered background refresh (`spawn_fetch`).
    refresh_in_flight: AtomicBool,
    /// `allowed_models` matched nothing in the fetched catalog; the prompt path
    /// blocks rather than run on the bundled default. Set in `apply_refresh_result`.
    allowlist_excludes_all: AtomicBool,
    /// Set once the user explicitly picks a model (`/model`); guards the
    /// first-catalog reselect from clobbering that choice.
    user_selected_model: AtomicBool,
    /// Layer-3 LazinessDetector model-switch signal. Carries a
    /// monotonically-increasing generation counter (`u64`) that is
    /// bumped whenever the current model id actually changes via
    /// [`Self::set_current_model_id`].
    ///
    /// Two consumer patterns:
    /// 1. `subscribe_model_switch().changed().await` — used by the
    ///    `SessionActor` main loop to react to a switch (e.g. zero
    ///    the per-session nudge counter). Critically, `watch::Receiver`
    ///    only resolves `.changed()` on changes that happen **after**
    ///    subscription — there is no stored-permit hazard akin to
    ///    `tokio::sync::Notify::notify_one()`.
    /// 2. `model_switch_generation()` — cheap snapshot read used by
    ///    `maybe_fire_laziness_check`'s polling loop to detect a
    ///    switch that occurred during the idle wait or sampler call.
    ///
    /// `watch::Sender` natively fans out to every subscriber, so this
    /// replaces the previous `RwLock<Vec<Arc<Notify>>>` listener
    /// registry — no manual fan-out, no listener-leak risk, no
    /// `unregister` API to maintain.
    model_switch_watch: tokio::sync::watch::Sender<u64>,
}

/// Clears an in-flight flag on drop so a panicking task can't wedge future refreshes.
struct RetryInFlightGuard(Arc<Inner>);
impl Drop for RetryInFlightGuard {
    fn drop(&mut self) {
        self.0.retry_in_flight.store(false, Ordering::Release);
    }
}
struct RefreshInFlightGuard(Arc<Inner>);
impl Drop for RefreshInFlightGuard {
    fn drop(&mut self) {
        self.0.refresh_in_flight.store(false, Ordering::Release);
    }
}

impl Default for ModelsManager {
    fn default() -> Self {
        let grok_home = crate::util::grok_home::grok_home();
        let auth_manager = Arc::new(AuthManager::new(&grok_home, GrokComConfig::default()));
        Self::new(
            None,
            IndexMap::new(),
            acp::ModelId::new("default"),
            auth_manager,
            config::Config::default(),
        )
    }
}

impl ModelsManager {
    pub(crate) fn new(
        prefetched: Option<IndexMap<String, ModelEntry>>,
        models: IndexMap<String, ModelEntry>,
        current_model_id: acp::ModelId,
        auth_manager: Arc<AuthManager>,
        cfg: config::Config,
    ) -> Self {
        let kimi_endpoint = cfg.models.kimi_endpoint;
        Self::new_with_provider_catalogs(
            prefetched,
            None,
            None,
            models,
            current_model_id,
            auth_manager,
            cfg,
            CodexModelsClient::new(),
            KimiModelsClient::new(kimi_endpoint),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_provider_catalogs(
        prefetched: Option<IndexMap<String, ModelEntry>>,
        codex_catalog: Option<CodexModelsCatalog>,
        kimi_catalog: Option<KimiModelsCatalog>,
        models: IndexMap<String, ModelEntry>,
        current_model_id: acp::ModelId,
        auth_manager: Arc<AuthManager>,
        cfg: config::Config,
        codex_client: CodexModelsClient,
        kimi_client: KimiModelsClient,
    ) -> Self {
        let has_session = auth_manager.current_or_expired().is_some();
        let fetch_auth = ModelFetchAuth::resolve(&cfg.endpoints, has_session);
        let current_reasoning_effort = cfg.models.default_reasoning_effort;
        Self {
            inner: Arc::new(Inner {
                prefetched: RwLock::new(prefetched),
                codex_catalog: RwLock::new(codex_catalog),
                kimi_catalog: RwLock::new(kimi_catalog),
                fireworks_catalog: RwLock::new(None),
                deepseek_catalog: RwLock::new(None),
                opencode_go_catalog: RwLock::new(None),
                models: RwLock::new(models),
                current_model_id: RwLock::new(current_model_id),
                current_reasoning_effort: RwLock::new(current_reasoning_effort),
                etag: RwLock::new(None),
                has_fetched_real_catalog: RwLock::new(false),
                auth_manager,
                cfg: RwLock::new(cfg),
                fetch_auth: RwLock::new(fetch_auth),
                gateway: RwLock::new(None),
                cache: ModelsCacheManager::new(),
                codex_client,
                kimi_client: RwLock::new(kimi_client),
                fireworks_client: FireworksModelsClient::new(),
                deepseek_client: DeepSeekModelsClient::new(),
                opencode_go_client: OpenCodeGoModelsClient::new(),
                codex_refresh_lock: tokio::sync::Mutex::new(()),
                kimi_refresh_lock: tokio::sync::Mutex::new(()),
                fireworks_refresh_lock: tokio::sync::Mutex::new(()),
                deepseek_refresh_lock: tokio::sync::Mutex::new(()),
                opencode_go_refresh_lock: tokio::sync::Mutex::new(()),
                codex_catalog_generation: AtomicU64::new(0),
                kimi_catalog_generation: AtomicU64::new(0),
                fireworks_catalog_generation: AtomicU64::new(0),
                deepseek_catalog_generation: AtomicU64::new(0),
                opencode_go_catalog_generation: AtomicU64::new(0),
                retry_in_flight: AtomicBool::new(false),
                refresh_in_flight: AtomicBool::new(false),
                allowlist_excludes_all: AtomicBool::new(false),
                user_selected_model: AtomicBool::new(false),
                model_switch_watch: tokio::sync::watch::channel(0u64).0,
            }),
        }
    }

    /// Subscribe to model-switch events. Returns a `watch::Receiver`
    /// carrying the monotonic generation counter. `.changed()` only
    /// resolves on switches that occur **after** subscription, so
    /// there is no stored-permit hazard (the bug that motivated
    /// replacing the previous `Arc<Notify>` design).
    pub fn subscribe_model_switch(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.model_switch_watch.subscribe()
    }

    /// Cheap snapshot of the current model-switch generation. Used by
    /// `maybe_fire_laziness_check`'s polling loop to detect a switch
    /// that occurred during the idle wait or sampler call without
    /// having to allocate a fresh `Receiver` per fire.
    pub fn model_switch_generation(&self) -> u64 {
        *self.inner.model_switch_watch.borrow()
    }

    /// Build from a resolved config. Falls back to bundled default if no models available.
    ///
    /// When `prefetched_models` is `None`, the disk cache is consulted so that
    /// server-side models are available for default-model resolution even when
    /// the caller didn't do an explicit prefetch.
    pub fn from_config(
        cfg: &config::Config,
        prefetched_models: Option<IndexMap<String, ModelEntry>>,
        auth_manager: Arc<AuthManager>,
    ) -> Result<Self, String> {
        let has_session = auth_manager.current_or_expired().is_some();
        let is_session_auth = auth_manager
            .current_or_expired()
            .is_some_and(|auth| auth.is_session_auth());
        let is_codex_session_auth = crate::codex_auth::is_logged_in();
        let fetch_auth = ModelFetchAuth::resolve(&cfg.endpoints, has_session);
        let prefetched_models = prefetched_models.or_else(|| {
            let cache = ModelsCacheManager::new();
            cache
                .load_fresh(
                    &fetch_auth.cache_auth_method(),
                    &crate::remote::models_list_url(&cfg.endpoints, fetch_auth),
                )
                .map(|c| c.models)
        });
        let has_prefetched = prefetched_models.is_some();
        let codex_client = CodexModelsClient::new();
        let codex_catalog = codex_client.load_fresh_cache();
        let kimi_client = KimiModelsClient::new(cfg.models.kimi_endpoint);
        let kimi_catalog = None;
        let catalog = resolve_model_catalog_with_provider_catalogs(
            cfg,
            prefetched_models.clone(),
            codex_catalog.as_ref(),
            kimi_catalog.as_ref(),
            None,
            None,
            None,
        );

        // Validate only against a real catalog; a bundled-only first run defers
        // to the async fetch (`apply_refresh_result`).
        if has_prefetched {
            validate_selectable(cfg, &catalog)?;
        }

        let (current_model_key, current_model, model_source) =
            resolve_default_model_with_provider_auth(
                cfg,
                &catalog,
                is_session_auth,
                is_codex_session_auth,
            );

        tracing::info!(
            model_id = %current_model.model,
            source = %model_source,
            "default model resolved"
        );

        let current_model_id = acp::ModelId::new(Arc::from(current_model_key));

        let mgr = Self::new_with_provider_catalogs(
            prefetched_models,
            codex_catalog,
            kimi_catalog,
            catalog,
            current_model_id,
            auth_manager,
            cfg.clone(),
            codex_client,
            kimi_client,
        );
        if has_prefetched {
            *mgr.inner.has_fetched_real_catalog.write() = true;
        }
        Ok(mgr)
    }

    pub(crate) fn set_gateway(&self, gateway: xai_acp_lib::AcpAgentGatewaySender) {
        *self.inner.gateway.write() = Some(gateway);
        self.start_codex_models_refresh();
        self.start_kimi_models_query();
        self.start_fireworks_models_query();
        self.start_deepseek_models_query();
        self.start_opencode_go_models_query();
    }

    /// Refresh the authenticated ChatGPT Codex catalog after the agent has a
    /// runtime and gateway. Existing credentials use `OnlineIfUncached`, just
    /// like codex-rs startup, while an explicit post-login extension can force
    /// an online request through [`Self::refresh_codex_models`].
    fn start_codex_models_refresh(&self) {
        if !crate::codex_auth::is_logged_in() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("Codex model refresh deferred: no Tokio runtime");
            return;
        };
        let manager = self.clone();
        runtime.spawn(async move {
            if let Err(error) = manager.refresh_codex_models(false).await {
                tracing::warn!(%error, "Codex model catalog refresh failed; keeping cached/embedded models");
            }
        });
    }

    /// Load or fetch the provider-isolated ChatGPT Codex model catalog and
    /// publish the resulting combined model state. A failed fetch leaves the
    /// current live/cache/fallback catalog untouched.
    pub(crate) async fn refresh_codex_models(&self, force_online: bool) -> anyhow::Result<bool> {
        let _refresh_guard = self.inner.codex_refresh_lock.lock().await;
        let generation = self.inner.codex_catalog_generation.load(Ordering::Acquire);

        let refreshed = if force_online {
            self.inner.codex_client.fetch_and_cache().await?
        } else {
            self.inner.codex_client.load_fresh_or_fetch().await?
        };
        let Some(refreshed) = refreshed else {
            tracing::debug!("Codex model refresh skipped: no Codex credentials");
            return Ok(false);
        };
        if generation != self.inner.codex_catalog_generation.load(Ordering::Acquire)
            || !crate::codex_auth::is_logged_in()
        {
            // `fetch_and_cache` persists before returning. If logout won the
            // race, remove that just-written account-scoped cache as well as
            // refusing to republish its catalog in memory.
            self.inner.codex_client.invalidate_cache();
            tracing::debug!("discarded Codex model refresh completed after logout");
            return Ok(false);
        }
        if !self
            .inner
            .codex_client
            .catalog_matches_current_account(&refreshed)
        {
            // Login can remain continuously true while the selected ChatGPT
            // workspace changes. The catalog carries a non-secret principal
            // digest so account A's in-flight response cannot publish for B.
            tracing::debug!("discarded Codex model refresh completed after account switch");
            return Ok(false);
        }

        let count = refreshed.list_visible_entries().len();
        let authoritative = refreshed.is_authoritative();
        *self.inner.codex_catalog.write() = Some(refreshed);
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        tracing::info!(count, authoritative, "Codex model catalog refreshed");
        Ok(true)
    }

    /// Clear only ChatGPT Codex's live catalog/cache after Codex logout.
    /// The independently authenticated xAI catalog and cache remain intact;
    /// embedded OpenAI metadata immediately becomes the offline fallback.
    pub(crate) fn clear_codex_models(&self) -> bool {
        self.inner
            .codex_catalog_generation
            .fetch_add(1, Ordering::AcqRel);
        let had_catalog = self.inner.codex_catalog.write().take().is_some();
        let had_cache = self.inner.codex_client.cache_path().is_file();
        self.inner.codex_client.invalidate_cache();

        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        had_catalog || had_cache
    }

    fn start_kimi_models_query(&self) {
        let client = self.inner.kimi_client.read().clone();
        if !client.supports_models_query() || !client.has_usable_api_key() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("Kimi model query deferred: no Tokio runtime");
            return;
        };
        let manager = self.clone();
        runtime.spawn(async move {
            if let Err(error) = manager.refresh_kimi_models().await {
                tracing::warn!(%error, "Kimi model query failed; keeping embedded models");
            }
        });
    }

    /// Query Kimi's provider-owned `/models` endpoint and publish only its
    /// catalog partition. Failures preserve the last live/embedded catalog.
    pub(crate) async fn refresh_kimi_models(&self) -> anyhow::Result<bool> {
        let _refresh_guard = self.inner.kimi_refresh_lock.lock().await;
        let generation = self.inner.kimi_catalog_generation.load(Ordering::Acquire);
        let client = self.inner.kimi_client.read().clone();
        let Some(refreshed) = client.query().await? else {
            tracing::debug!("Kimi model query skipped: no Kimi API key");
            return Ok(false);
        };
        if generation != self.inner.kimi_catalog_generation.load(Ordering::Acquire)
            || !client.catalog_matches_current_credential(&refreshed)
            || self.inner.kimi_client.read().endpoint() != client.endpoint()
        {
            tracing::debug!(
                "discarded Kimi model query completed after catalog clear or credential change"
            );
            return Ok(false);
        }
        let count = refreshed.entries().len();
        let authoritative = refreshed.is_authoritative();
        *self.inner.kimi_catalog.write() = Some(refreshed);
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        tracing::info!(count, authoritative, "Kimi model catalog refreshed");
        Ok(true)
    }

    /// Drop queried Kimi entries after its key is cleared. The embedded K3
    /// entry remains in the catalog, but is hidden from model pickers until a
    /// replacement key is configured.
    pub(crate) fn clear_kimi_models(&self) -> bool {
        self.inner
            .kimi_catalog_generation
            .fetch_add(1, Ordering::AcqRel);
        let had_catalog = self.inner.kimi_catalog.write().take().is_some();
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        had_catalog
    }

    fn start_fireworks_models_query(&self) {
        if !self.inner.fireworks_client.has_usable_api_key() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("Fireworks model query deferred: no Tokio runtime");
            return;
        };
        let manager = self.clone();
        runtime.spawn(async move {
            if let Err(error) = manager.refresh_fireworks_models().await {
                tracing::warn!(%error, "Fireworks model query failed; keeping embedded models");
            }
        });
    }

    /// Query Fireworks' provider-owned `/models` endpoint and publish only its
    /// curated catalog partition. Failures preserve the embedded catalog.
    pub(crate) async fn refresh_fireworks_models(&self) -> anyhow::Result<bool> {
        let _refresh_guard = self.inner.fireworks_refresh_lock.lock().await;
        let generation = self
            .inner
            .fireworks_catalog_generation
            .load(Ordering::Acquire);
        let client = self.inner.fireworks_client.clone();
        let Some(refreshed) = client.query().await? else {
            tracing::debug!("Fireworks model query skipped: no Fireworks API key");
            return Ok(false);
        };
        if generation
            != self
                .inner
                .fireworks_catalog_generation
                .load(Ordering::Acquire)
            || !client.catalog_matches_current_credential(&refreshed)
        {
            tracing::debug!(
                "discarded Fireworks model query completed after catalog clear or credential change"
            );
            return Ok(false);
        }
        let count = refreshed.entries().len();
        let authoritative = refreshed.is_authoritative();
        *self.inner.fireworks_catalog.write() = Some(refreshed);
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        tracing::info!(count, authoritative, "Fireworks model catalog refreshed");
        Ok(true)
    }

    /// Drop queried Fireworks entries after its key is cleared. The embedded
    /// curated entries remain in the catalog, but are hidden from model pickers
    /// until a replacement key is configured.
    pub(crate) fn clear_fireworks_models(&self) -> bool {
        self.inner
            .fireworks_catalog_generation
            .fetch_add(1, Ordering::AcqRel);
        let had_catalog = self.inner.fireworks_catalog.write().take().is_some();
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        had_catalog
    }

    /// Apply a Fireworks credential change to the resident model manager:
    /// refresh the curated partition when a usable key exists, otherwise drop
    /// any credential-derived entries back to the embedded fallback.
    pub(crate) async fn apply_fireworks_credential_change(&self) -> anyhow::Result<bool> {
        if self.inner.fireworks_client.has_usable_api_key() {
            self.refresh_fireworks_models().await
        } else {
            self.clear_fireworks_models();
            Ok(false)
        }
    }

    fn start_deepseek_models_query(&self) {
        if !self.inner.deepseek_client.has_usable_api_key() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("DeepSeek model query deferred: no Tokio runtime");
            return;
        };
        let manager = self.clone();
        runtime.spawn(async move {
            if let Err(error) = manager.refresh_deepseek_models().await {
                tracing::warn!(%error, "DeepSeek model query failed; keeping embedded models");
            }
        });
    }

    pub(crate) async fn refresh_deepseek_models(&self) -> anyhow::Result<bool> {
        let _refresh_guard = self.inner.deepseek_refresh_lock.lock().await;
        let generation = self
            .inner
            .deepseek_catalog_generation
            .load(Ordering::Acquire);
        let client = self.inner.deepseek_client.clone();
        let Some(refreshed) = client.query().await? else {
            tracing::debug!("DeepSeek model query skipped: no DeepSeek API key");
            return Ok(false);
        };
        if generation
            != self
                .inner
                .deepseek_catalog_generation
                .load(Ordering::Acquire)
            || !client.catalog_matches_current_credential(&refreshed)
        {
            tracing::debug!(
                "discarded DeepSeek model query completed after catalog clear or credential change"
            );
            return Ok(false);
        }
        let count = refreshed.entries().len();
        let authoritative = refreshed.is_authoritative();
        *self.inner.deepseek_catalog.write() = Some(refreshed);
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        tracing::info!(count, authoritative, "DeepSeek model catalog refreshed");
        Ok(true)
    }

    pub(crate) fn clear_deepseek_models(&self) -> bool {
        self.inner
            .deepseek_catalog_generation
            .fetch_add(1, Ordering::AcqRel);
        let had_catalog = self.inner.deepseek_catalog.write().take().is_some();
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        had_catalog
    }

    pub(crate) async fn apply_deepseek_credential_change(&self) -> anyhow::Result<bool> {
        if self.inner.deepseek_client.has_usable_api_key() {
            self.refresh_deepseek_models().await
        } else {
            self.clear_deepseek_models();
            Ok(false)
        }
    }

    fn start_opencode_go_models_query(&self) {
        if !self.inner.opencode_go_client.has_usable_api_key() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::debug!("OpenCode Go model query deferred: no Tokio runtime");
            return;
        };
        let manager = self.clone();
        runtime.spawn(async move {
            if let Err(error) = manager.refresh_opencode_go_models().await {
                tracing::warn!(%error, "OpenCode Go model query failed; keeping current models");
            }
        });
    }

    /// Query OpenCode Go's catalog and publish only the explicitly enabled
    /// models. The unfiltered descriptors remain available to Settings.
    pub(crate) async fn refresh_opencode_go_models(&self) -> anyhow::Result<bool> {
        let _refresh_guard = self.inner.opencode_go_refresh_lock.lock().await;
        let generation = self
            .inner
            .opencode_go_catalog_generation
            .load(Ordering::Acquire);
        let client = self.inner.opencode_go_client.clone();
        let Some(refreshed) = client.query().await? else {
            tracing::debug!("OpenCode Go model query skipped: no API key");
            return Ok(false);
        };
        if generation
            != self
                .inner
                .opencode_go_catalog_generation
                .load(Ordering::Acquire)
            || !client.catalog_matches_current_credential(&refreshed)
        {
            tracing::debug!(
                "discarded OpenCode Go model query completed after catalog clear or credential change"
            );
            return Ok(false);
        }
        for warning in refreshed.warnings() {
            tracing::warn!(warning, "OpenCode Go model omitted from catalog");
        }
        let count = refreshed.entries().len();
        *self.inner.opencode_go_catalog.write() = Some(refreshed);
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        tracing::info!(count, "OpenCode Go model catalog refreshed");
        Ok(true)
    }

    pub(crate) fn clear_opencode_go_models(&self) -> bool {
        self.inner
            .opencode_go_catalog_generation
            .fetch_add(1, Ordering::AcqRel);
        let had_catalog = self.inner.opencode_go_catalog.write().take().is_some();
        let cfg = self.inner.cfg.read().clone();
        let prefetched = self.inner.prefetched.read().clone();
        self.rebuild(&cfg, prefetched);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
        had_catalog
    }

    pub(crate) async fn apply_opencode_go_credential_change(&self) -> anyhow::Result<bool> {
        if self.inner.opencode_go_client.has_usable_api_key() {
            self.refresh_opencode_go_models().await
        } else {
            self.clear_opencode_go_models();
            Ok(false)
        }
    }

    pub fn opencode_go_models(&self) -> Vec<OpenCodeGoModelDescriptor> {
        self.inner
            .opencode_go_catalog
            .read()
            .as_ref()
            .map(OpenCodeGoModelsCatalog::descriptors)
            .unwrap_or_default()
    }

    pub fn opencode_go_enabled_models(&self) -> Vec<String> {
        self.inner
            .cfg
            .read()
            .models
            .opencode_go_enabled_models
            .clone()
    }

    pub fn apply_opencode_go_enabled_models(&self, mut enabled_models: Vec<String>) {
        enabled_models.sort();
        enabled_models.dedup();
        let mut cfg = self.inner.cfg.read().clone();
        cfg.models.opencode_go_enabled_models = enabled_models;
        self.apply_config(cfg);
    }

    /// Apply a Kimi service selection to the resident model manager. The
    /// embedded partition is rebuilt synchronously, then both Platform and
    /// Code attempt a live `/models` refresh when a usable key is present.
    /// Failures keep the service-local embedded catalog.
    pub async fn apply_kimi_endpoint(&self, endpoint: KimiApiEndpoint) -> anyhow::Result<bool> {
        let mut cfg = self.inner.cfg.read().clone();
        cfg.models.kimi_endpoint = endpoint;
        self.apply_config(cfg);

        if self.kimi_endpoint() != endpoint {
            anyhow::bail!("Kimi endpoint change was rejected by model catalog validation");
        }

        if self.inner.kimi_client.read().supports_models_query() {
            self.refresh_kimi_models().await
        } else {
            Ok(false)
        }
    }

    /// Swap config, rebuild catalog, and reselect the model.
    ///
    /// Calls `reselect_default_model` when the preferred model changed
    /// (and is `Some`); otherwise `reselect_current_model_if_missing`.
    pub fn apply_config(&self, new_config: config::Config) {
        // Reject an invalid reload instead of mutating live state: bad globs or
        // (once a real catalog exists) an allowlist that excludes everything.
        if let Err(e) = new_config.validate_model_filters() {
            tracing::error!(error = %e, "ignoring config reload: invalid model filters");
            return;
        }
        let old_kimi_endpoint = self.inner.cfg.read().models.kimi_endpoint;
        let kimi_endpoint_changed = old_kimi_endpoint != new_config.models.kimi_endpoint;
        let prefetched = self.inner.prefetched.read().clone();
        let new_catalog = {
            let codex_catalog = self.inner.codex_catalog.read();
            let kimi_catalog = self.inner.kimi_catalog.read();
            let fireworks_catalog = self.inner.fireworks_catalog.read();
            let deepseek_catalog = self.inner.deepseek_catalog.read();
            let opencode_go_catalog = self.inner.opencode_go_catalog.read();
            resolve_model_catalog_with_provider_catalogs(
                &new_config,
                prefetched,
                codex_catalog.as_ref(),
                if kimi_endpoint_changed {
                    None
                } else {
                    kimi_catalog.as_ref()
                },
                fireworks_catalog.as_ref(),
                deepseek_catalog.as_ref(),
                opencode_go_catalog.as_ref(),
            )
        };
        let has_real_catalog = *self.inner.has_fetched_real_catalog.read();
        if has_real_catalog && let Err(e) = validate_selectable(&new_config, &new_catalog) {
            tracing::error!(error = %e, "ignoring config reload: allowed_models excludes all models");
            return;
        }

        let (old_preferred, old_default_is_campaign) = {
            let cfg = self.inner.cfg.read();
            (
                cfg.models.default.clone(),
                cfg.models.default_is_campaign_driven,
            )
        };
        let new_preferred = new_config.models.default.clone();
        let has_session = self.inner.auth_manager.current_or_expired().is_some();
        *self.inner.fetch_auth.write() =
            ModelFetchAuth::resolve(&new_config.endpoints, has_session);
        if kimi_endpoint_changed {
            self.inner
                .kimi_catalog_generation
                .fetch_add(1, Ordering::AcqRel);
            self.inner.kimi_catalog.write().take();
            *self.inner.kimi_client.write() =
                KimiModelsClient::new(new_config.models.kimi_endpoint);
        }
        *self.inner.cfg.write() = new_config.clone();
        // Recompute the prompt-block flag so a corrective reload unblocks.
        if has_real_catalog {
            let excludes_all = allowlist_matches_nothing(&new_config, &new_catalog);
            self.inner
                .allowlist_excludes_all
                .store(excludes_all, Ordering::Relaxed);
        }
        *self.inner.models.write() = new_catalog;

        // A preferred-model flip caused only by a campaign overlay appearing or
        // disappearing must not yank an in-flight session whose current model is
        // still usable — the campaign applies to /new sessions only.
        let preferred_changed = new_preferred != old_preferred && new_preferred.is_some();
        // Recognize an appearing OR withdrawing campaign from the
        // `default_is_campaign_driven` flag on each config (no disk I/O); correct
        // even when the user has no base default (where a value compare would miss).
        let mut campaign_defaults = std::collections::HashSet::new();
        if new_config.models.default_is_campaign_driven
            && let Some(d) = &new_preferred
        {
            campaign_defaults.insert(d.clone());
        }
        if old_default_is_campaign && let Some(d) = &old_preferred {
            campaign_defaults.insert(d.clone());
        }
        let campaign_only_flip =
            is_campaign_only_flip(&old_preferred, &new_preferred, &campaign_defaults);
        let current_still_ok = {
            let models = self.inner.models.read();
            let cur = self.inner.current_model_id.read();
            models
                .get(cur.0.as_ref())
                .is_some_and(|e| e.info.user_selectable)
        };
        if preferred_changed && !(campaign_only_flip && current_still_ok) {
            self.reselect_default_model(&new_config);
        } else {
            self.reselect_current_model_if_missing(&new_config);
        }

        // Push the new catalog to connected clients (`x.ai/models/update`).
        // Without this, a long-running agent (leader mode) correctly swaps
        // its in-memory catalog on a config.toml `[model.*]`/`[models]` edit,
        // but already-connected clients keep rendering the stale model list
        // until they reconnect. No-op when no gateway is attached (tests,
        // pre-init).
        self.notify_models_updated();
    }

    // ── Accessors ───────────────────────────────────────────────────

    pub fn models(&self) -> IndexMap<String, ModelEntry> {
        self.inner.models.read().clone()
    }

    pub fn endpoints(&self) -> config::EndpointsConfig {
        self.inner.cfg.read().endpoints.clone()
    }

    pub fn kimi_endpoint(&self) -> KimiApiEndpoint {
        self.inner.cfg.read().models.kimi_endpoint
    }

    pub fn effective_kimi_endpoint(&self) -> KimiApiEndpoint {
        self.inner.kimi_client.read().endpoint()
    }

    /// Explicit recap helper-model override. `None` means apply the
    /// provider-aware automatic policy at call time.
    pub fn recap_model(&self) -> Option<String> {
        self.inner.cfg.read().models.recap.clone()
    }

    /// Explicit memory helper-model override shared by flush, Dream, and note
    /// rewriting. `None` means apply the provider-aware automatic policy.
    pub fn memory_model(&self) -> Option<String> {
        self.inner.cfg.read().models.memory.clone()
    }

    /// Does the current credential grant access to OAuth-only models?
    fn is_session_auth(&self) -> bool {
        self.inner
            .auth_manager
            .current_or_expired()
            .is_some_and(|auth| auth.is_session_auth())
    }

    fn is_codex_session_auth(&self) -> bool {
        crate::codex_auth::is_logged_in()
    }

    /// ACP-visible (non-hidden) projection of the catalog.
    /// The catalog coming from `resolve_model_catalog` already has
    /// allowed_models + disabled_models + hidden_models applied.
    pub fn available(&self) -> IndexMap<acp::ModelId, acp::ModelInfo> {
        let snapshot = {
            let models = self.inner.models.read();
            models.clone()
        };

        let selectable: IndexMap<_, _> = snapshot
            .into_iter()
            .filter(|(_, e)| e.info.user_selectable)
            .collect();

        available_models_with_provider_auth(
            &selectable,
            self.is_session_auth(),
            self.is_codex_session_auth(),
        )
    }

    pub(crate) fn task_model_error(&self, requested: &str) -> Option<String> {
        let is_session_auth = self.is_session_auth();
        let models = self.inner.models.read();
        task_model_error_for_catalog_with_provider_auth(
            requested,
            &models,
            is_session_auth,
            self.is_codex_session_auth(),
        )
    }

    pub fn current_model_id(&self) -> acp::ModelId {
        self.inner.current_model_id.read().clone()
    }

    pub fn set_current_model_id(&self, id: acp::ModelId) {
        // Explicit `/model` pick: remember so first-catalog reselect won't
        // clobber it (background refresh after non-blocking boot).
        self.inner
            .user_selected_model
            .store(true, Ordering::Relaxed);
        self.set_current_model_id_internal(id);
    }

    /// Set current model without marking a user selection (catalog reselect /
    /// config-driven default resolution).
    fn set_current_model_id_internal(&self, id: acp::ModelId) {
        // Only bump the model-switch generation on a real change.
        // The pager's `/model` handler can call this with the
        // already-active id during re-resolution; bumping the counter
        // in that case would needlessly cancel a healthy in-flight
        // classifier call and zero the per-session nudge counter.
        let changed = {
            let mut cur = self.inner.current_model_id.write();
            let changed = *cur != id;
            *cur = id;
            changed
        };
        if changed {
            self.inner
                .model_switch_watch
                .send_modify(|generation| *generation += 1);
        }
    }

    /// Look up the per-model Layer-3 LazinessDetector config for the
    /// model identified by `model_id`. Returns the default (disabled)
    /// config when the id isn't in the catalog — same fallback
    /// semantics as the `auto_compact_threshold_percent` lookup.
    pub fn laziness_detector_for(&self, model_id: &str) -> config::LazinessDetectorPerModelConfig {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().laziness_detector.clone())
            .unwrap_or_default()
    }

    /// Test-only catalog poke: inserts a `ModelEntry` keyed by `id`,
    /// allowing integration tests to enable Layer-3 features per
    /// model without spinning up the full config-merge pipeline.
    #[cfg(test)]
    pub(crate) fn insert_test_entry(&self, id: impl Into<String>, entry: ModelEntry) {
        self.inner.models.write().insert(id.into(), entry);
    }

    pub fn current_reasoning_effort(&self) -> Option<ReasoningEffort> {
        *self.inner.current_reasoning_effort.read()
    }

    pub fn set_current_reasoning_effort(&self, effort: Option<ReasoningEffort>) {
        *self.inner.current_reasoning_effort.write() = effort;
    }

    /// Whether the given model supports reasoning effort according to the catalog.
    pub fn model_supports_reasoning_effort(&self, model_id: &str) -> bool {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().supports_reasoning_effort)
            .unwrap_or(false)
    }

    /// The catalog default reasoning effort for `model_id`, if the catalog
    /// pins one. Used as the final fallback when neither the session handle
    /// nor the global config sets an explicit effort, so surfaced config stays
    /// consistent with the effort sampling actually uses.
    pub fn model_default_reasoning_effort(&self, model_id: &str) -> Option<ReasoningEffort> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().reasoning_effort)
    }

    /// The raw catalog `reasoning_efforts` list for `model_id` with no fallback,
    /// empty when the catalog pins none (caller falls back to the built-in
    /// session modes). Distinct from the pager's gate-first, fallback-applied
    /// `ModelState::reasoning_effort_options`.
    pub fn model_reasoning_efforts(&self, model_id: &str) -> Vec<ReasoningEffortOption> {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().reasoning_efforts.clone())
            .unwrap_or_default()
    }

    /// Service tiers advertised for `model_id` (Codex Fast/Flex routing).
    pub fn model_service_tiers(
        &self,
        model_id: &str,
    ) -> Vec<xai_grok_sampling_types::ModelServiceTier> {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().service_tiers.clone())
            .unwrap_or_default()
    }

    /// Whether `model_id` advertises a concrete service-tier id.
    pub fn model_supports_service_tier(&self, model_id: &str, service_tier: &str) -> bool {
        self.model_service_tiers(model_id)
            .iter()
            .any(|tier| tier.id == service_tier)
    }

    /// Whether `model_id` advertises Fast / priority routing (`/fast`).
    pub fn model_supports_fast_service_tier(&self, model_id: &str) -> bool {
        self.model_service_tiers(model_id)
            .iter()
            .any(xai_grok_sampling_types::ModelServiceTier::is_fast)
    }

    /// Resolve the Fast service-tier id for `model_id`, if advertised.
    pub fn model_fast_service_tier_id(&self, model_id: &str) -> Option<String> {
        self.model_service_tiers(model_id)
            .into_iter()
            .find(|tier| tier.is_fast())
            .map(|tier| tier.id)
    }

    /// Whether a concrete effort is accepted by this model's live catalog
    /// entry. A non-empty server menu is authoritative; legacy entries fall
    /// back to the standard reasoning menu.
    pub fn model_accepts_reasoning_effort(&self, model_id: &str, effort: ReasoningEffort) -> bool {
        let models = self.inner.models.read();
        config::find_model_by_id(&models, model_id)
            .is_some_and(|entry| model_offers_reasoning_effort(&entry.info, effort))
    }

    pub fn model_supports_backend_search(&self, model_id: &str) -> bool {
        self.inner
            .models
            .read()
            .get(model_id)
            .map(|e| e.info().supports_backend_search)
            .unwrap_or(false)
    }

    /// Resolve the live Codex v2 multi-agent capability by catalog key or
    /// routing slug. The menu comes from the authenticated `/models` response,
    /// with the embedded catalog used only as its offline fallback.
    pub fn model_supports_codex_multi_agent_v2(&self, model_id: &str) -> bool {
        let models = self.inner.models.read();
        config::find_model_by_id(&models, model_id)
            .is_some_and(|entry| config::supports_codex_multi_agent_v2(entry.info()))
    }

    /// Resolve the effective Codex reasoning-summary mode by catalog key or
    /// routing slug. The authenticated models.json snapshot remains
    /// authoritative across client rebuilds and subagent inheritance.
    pub fn model_reasoning_summary(
        &self,
        model_id: &str,
    ) -> Option<xai_grok_sampling_types::ReasoningSummary> {
        let models = self.inner.models.read();
        config::find_model_by_id(&models, model_id)
            .and_then(|entry| config::model_reasoning_summary(entry.info()))
    }

    /// Live Codex compaction metadata for a catalog key or routing slug.
    /// Embedded/offline models deliberately return `None`, retaining the
    /// historical 90%-of-raw fallback and no hash-triggered compaction.
    pub(crate) fn codex_compaction_metadata(
        &self,
        model_id: &str,
    ) -> Option<CodexCompactionMetadata> {
        let routing_slug = {
            let models = self.inner.models.read();
            let entry = config::find_model_by_id(&models, model_id)?;
            (entry.info.provider == xai_grok_sampling_types::ModelProvider::Codex)
                .then(|| entry.info.model.clone())?
        };
        self.inner
            .codex_catalog
            .read()
            .as_ref()
            .and_then(|catalog| catalog.compaction_metadata(&routing_slug))
    }

    pub fn model_compactions_remaining(
        &self,
        model_id: &str,
    ) -> Option<xai_grok_sampling_types::CompactionsRemaining> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().compactions_remaining)
    }

    pub fn model_compaction_at_tokens(
        &self,
        model_id: &str,
    ) -> Option<xai_grok_sampling_types::CompactionAtTokens> {
        self.inner
            .models
            .read()
            .get(model_id)
            .and_then(|e| e.info().compaction_at_tokens)
    }

    /// Catalog opt-in to display the served-checkpoint fingerprint for this model.
    ///
    /// `model_id` may be a routing slug (`config.model`, e.g. `grok-4.5`)
    /// OR a catalog key; the catalog map is keyed by the config key, which can
    /// differ from the slug for custom/enterprise ids (e.g. key `enterprise-grok-build`
    /// → slug `grok-4.5`). Resolve to the catalog key first so a slug
    /// caller still finds the opted-in entry.
    pub fn model_show_model_fingerprint(&self, model_id: &str) -> bool {
        let models = self.inner.models.read();
        resolve_catalog_key(&models, &acp::ModelId::new(model_id))
            .and_then(|key| models.get(key.0.as_ref()))
            .map(|e| e.info().show_model_fingerprint)
            .unwrap_or(false)
    }

    /// Resolved next-prompt-suggestion model pin from the live config
    /// (`env > [models] prompt_suggestion > remote settings`); tracks config
    /// hot-reloads via [`Self::apply_config`]. Consumed catalog-guarded by
    /// `handle_suggest_prompt`.
    pub fn prompt_suggest_model_pin(&self) -> crate::config::PromptSuggestModelPin {
        self.inner.cfg.read().prompt_suggest_model_pin.clone()
    }

    /// Whether `model_id` resolves in the current catalog — as a config key
    /// or a routing slug (see [`resolve_catalog_key`]). Deliberately checks
    /// the full catalog rather than the user-selectable projection: auxiliary
    /// background calls need a *sampleable* model, and hidden or
    /// non-selectable entries are still sampleable.
    pub fn model_in_catalog(&self, model_id: &str) -> bool {
        let models = self.inner.models.read();
        resolve_catalog_key(&models, &acp::ModelId::new(model_id)).is_some()
    }

    #[cfg(test)]
    fn prefetched(&self) -> Option<IndexMap<String, ModelEntry>> {
        self.inner.prefetched.read().clone()
    }

    #[cfg(test)]
    fn has_fetched_real_catalog(&self) -> bool {
        *self.inner.has_fetched_real_catalog.read()
    }

    // ── Mutations ───────────────────────────────────────────────────

    fn rebuild(&self, cfg: &config::Config, prefetched: Option<IndexMap<String, ModelEntry>>) {
        let catalog = {
            let codex_catalog = self.inner.codex_catalog.read();
            let kimi_catalog = self.inner.kimi_catalog.read();
            let fireworks_catalog = self.inner.fireworks_catalog.read();
            let deepseek_catalog = self.inner.deepseek_catalog.read();
            let opencode_go_catalog = self.inner.opencode_go_catalog.read();
            resolve_model_catalog_with_provider_catalogs(
                cfg,
                prefetched,
                codex_catalog.as_ref(),
                kimi_catalog.as_ref(),
                fireworks_catalog.as_ref(),
                deepseek_catalog.as_ref(),
                opencode_go_catalog.as_ref(),
            )
        };
        *self.inner.models.write() = catalog;
    }

    /// Refresh models when the etag changes.
    ///
    /// Writes etag optimistically before spawning the fetch to coalesce
    /// concurrent callers seeing the same new etag.
    pub async fn refresh_if_new_etag(&self, etag: String) {
        let same_etag = {
            let current = self.inner.etag.read();
            current.as_deref() == Some(etag.as_str())
        };
        if same_etag {
            let fetch_auth = *self.inner.fetch_auth.read();
            self.inner
                .cache
                .renew_ttl(&fetch_auth.cache_auth_method(), &self.cache_origin())
                .await;
            return;
        }
        *self.inner.etag.write() = Some(etag.clone());
        tracing::info!(etag = %etag, "models etag changed, refreshing");
        self.do_refresh(Some(etag), RefreshStrategy::Online);
    }

    /// Auth identity changed: invalidate disk cache and refresh the catalog.
    ///
    /// Safe on OIDC token recovery after idle: we never drop a successfully-fetched
    /// catalog on transient failure. Only fall back to the bundled default when
    /// we have never had a real catalog (`!has_fetched_real_catalog`), or via
    /// the genuine no-auth path (`clear()`).
    ///
    /// Respects the auth snapshot / hot-swap discipline.
    pub async fn on_auth_changed(&self) {
        let config = self.inner.cfg.read().clone();
        crate::agent::init::update_telemetry_config(&config, &self.inner.auth_manager);
        self.inner.cache.invalidate();
        let has_session = self.inner.auth_manager.current_or_expired().is_some();
        let fetch_auth = ModelFetchAuth::resolve(&config.endpoints, has_session);
        *self.inner.fetch_auth.write() = fetch_auth;
        if self.inner.auth_manager.current_or_expired().is_none()
            && fetch_auth == ModelFetchAuth::Session
        {
            self.clear();
            return;
        }

        // Never eagerly drop prefetched on auth recovery. Only fall back to
        // bundled defaults when we have never had a real catalog. Resolved once
        // so the fetch and the failure-vs-disabled classification below agree.
        let remote_fetch_enabled = crate::util::config::resolve_remote_fetch_enabled();
        self.fetch_and_apply_inner(remote_fetch_enabled).await;

        if !*self.inner.has_fetched_real_catalog.read() && self.inner.prefetched.read().is_none() {
            if remote_fetch_enabled {
                xai_grok_telemetry::unified_log::warn(
                    "model catalog: falling back to bundled defaults only",
                    None,
                    Some(serde_json::json!({
                        "trigger": "on_auth_changed",
                        "had_real_catalog": false,
                    })),
                );
            } else {
                // Deliberate no-fetch state, not a failure: no warn-class log.
                tracing::debug!("model catalog: bundled defaults in use (remote_fetch disabled)");
            }
            self.rebuild(&config, None); // first-time only: no fetched catalog, use bundled defaults
            self.reselect_current_model_if_missing(&config);

            // Schedule background retries so we recover once the network is
            // back (e.g. after sleep/resume when the first fetch races DNS).
            // With remote_fetch disabled a retry can never succeed, so none is
            // scheduled.
            if remote_fetch_enabled {
                self.spawn_catalog_retry();
            }
        }

        self.notify_models_updated();
    }

    /// Notify clients about the current model catalog.
    fn notify_models_updated(&self) {
        let available = self.available();
        let current = self.current_model_id();
        let count = available.len();
        xai_grok_telemetry::unified_log::info(
            "model catalog: notifying clients",
            None,
            Some(serde_json::json!({
                "model_count": count,
                "current_model_id": current.0.as_ref(),
            })),
        );
        if let Some(ref gw) = *self.inner.gateway.read() {
            let model_state =
                acp::SessionModelState::new(current, available.values().cloned().collect());
            if let Ok(params) = serde_json::value::to_raw_value(&model_state) {
                gw.forward_fire_and_forget(acp::ExtNotification::new(
                    "x.ai/models/update",
                    params.into(),
                ));
            }
        }
    }

    /// Hot-reload the catalog from `~/.opengrok/models_cache.json` after an
    /// external write (detected by the config file watcher).
    ///
    /// A long-running leader otherwise only refreshes its catalog from its
    /// *own* fetch paths (startup prefetch, auth change, response-header etag).
    /// When another grok process sharing `~/.opengrok` (a `--no-leader` run, a
    /// newer client, grok-desktop) fetches a fresher `/v1/models` catalog and
    /// persists it, this picks it up without a network round-trip.
    ///
    /// Guards, in order:
    /// 1. `load_fresh` — rejects stale (TTL), version-mismatched,
    ///    auth-method-mismatched, or origin-mismatched cache files (another
    ///    process running with different credentials or pointed at a
    ///    different backend must not poison this catalog).
    /// 2. Content dedup — the leader itself rewrites the cache file
    ///    (`persist` after fetch, `renew_ttl` on same-etag responses), and the
    ///    watcher has no self-write suppression. If the cached models match
    ///    the in-memory prefetched catalog this is a no-op (the etag is still
    ///    adopted so `refresh_if_new_etag` doesn't refetch needlessly).
    ///
    /// On a real change: swaps the prefetched catalog, rebuilds, re-resolves
    /// the configured default when this is the first real catalog (otherwise
    /// reselects the current model if it disappeared), and notifies clients.
    pub fn reload_from_disk_cache(&self) {
        self.reload_from_cache_manager(&self.inner.cache);
    }

    /// Core of [`Self::reload_from_disk_cache`], parameterized over the cache
    /// manager so tests can point it at a temp file (the production
    /// `ModelsCacheManager` path is fixed to `grok_home()`, a process-wide
    /// `OnceLock`).
    fn reload_from_cache_manager(&self, cache: &ModelsCacheManager) {
        let fetch_auth = *self.inner.fetch_auth.read();
        let Some(cached) = cache.load_fresh(&fetch_auth.cache_auth_method(), &self.cache_origin())
        else {
            tracing::debug!("models cache changed on disk but is not loadable; ignoring");
            return;
        };

        // Self-write / no-change dedup by content. `ModelEntry` doesn't impl
        // `PartialEq` (nested config types), so compare the serialized form —
        // catalogs are small (tens of entries) and writes are debounced.
        let same_content = {
            let prefetched = self.inner.prefetched.read();
            prefetched.as_ref().is_some_and(|current| {
                serde_json::to_string(current).ok() == serde_json::to_string(&cached.models).ok()
            })
        };
        if same_content {
            // Adopt the (possibly newer) etag without a rebuild so the next
            // response-header comparison in `refresh_if_new_etag` is accurate.
            if cached.etag.is_some() {
                *self.inner.etag.write() = cached.etag;
            }
            tracing::debug!("models cache changed on disk but catalog is identical; skipping");
            return;
        }

        let cfg = self.inner.cfg.read().clone();
        let count = cached.models.len();
        // Capture whether this is the first real catalog (mirrors
        // `apply_refresh_result`): if the leader bootstrapped on bundled
        // defaults, the configured default must be re-resolved against the
        // real catalog rather than left on a placeholder.
        let first_real_catalog = {
            let mut flag = self.inner.has_fetched_real_catalog.write();
            let was_first = !*flag;
            *flag = true;
            was_first
        };
        *self.inner.prefetched.write() = Some(cached.models.clone());
        self.rebuild(&cfg, Some(cached.models));
        *self.inner.etag.write() = cached.etag;
        if first_real_catalog {
            self.reselect_default_model(&cfg);
        } else {
            self.reselect_current_model_if_missing(&cfg);
        }

        // Recompute the prompt-block flag (mirrors `apply_refresh_result`) so
        // a corrective external cache write unlatches a previously latched
        // "allowlist excludes everything" state instead of keeping prompts
        // blocked against a stale catalog.
        let excludes_all = allowlist_matches_nothing(&cfg, &self.inner.models.read());
        self.inner
            .allowlist_excludes_all
            .store(excludes_all, Ordering::Relaxed);
        if excludes_all {
            tracing::error!("allowed_models excludes all fetched models; prompts will be blocked");
        }

        tracing::info!(count, "model catalog hot-reloaded from disk cache");
        xai_grok_telemetry::unified_log::info(
            "model catalog: reloaded from external disk-cache write",
            None,
            Some(serde_json::json!({ "model_count": count })),
        );
        self.notify_models_updated();
    }

    /// Retry model catalog fetch in the background with exponential backoff.
    ///
    /// Spawned when `on_auth_changed` falls back to bundled defaults. Uses the
    /// crate-standard `execute_with_backoff` (5 attempts, 5s base, 60s cap) and
    /// notifies clients on success so the UI recovers after sleep/resume without
    /// requiring a manual restart.
    fn spawn_catalog_retry(&self) {
        self.spawn_catalog_retry_with_backoff(crate::tools::retry::BackoffConfig::new(
            5, 5_000, 60_000,
        ));
    }

    /// [`Self::spawn_catalog_retry`] with an injectable backoff (fast in tests).
    fn spawn_catalog_retry_with_backoff(&self, backoff: crate::tools::retry::BackoffConfig) {
        // Deliberate no-fetch state: a retry loop can never succeed, so don't
        // start one (defensive re-check; the spawn site already gates).
        if !crate::util::config::resolve_remote_fetch_enabled() {
            return;
        }
        // Prevent overlapping retry loops.
        if self
            .inner
            .retry_in_flight
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tracing::debug!("model catalog retry already in flight, skipping");
            return;
        }

        let mgr = self.clone();
        tokio::task::spawn(async move {
            let _retry_guard = RetryInFlightGuard(mgr.inner.clone());
            let result = crate::tools::retry::execute_with_backoff(
                &backoff,
                || {
                    let mgr = mgr.clone();
                    async move {
                        // Bail out early if another code path already loaded a real catalog.
                        if *mgr.inner.has_fetched_real_catalog.read() {
                            return Ok(());
                        }

                        mgr.fetch_and_apply().await;

                        if *mgr.inner.has_fetched_real_catalog.read() {
                            Ok(())
                        } else {
                            Err("model catalog fetch returned no models")
                        }
                    }
                },
                |attempt, max_retries, delay| async move {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: retry scheduled",
                        None,
                        Some(serde_json::json!({
                            "attempt": attempt,
                            "max_retries": max_retries,
                            "delay_ms": delay.as_millis() as u64,
                        })),
                    );
                },
            )
            .await;

            match result {
                Ok(()) => {
                    let count = mgr.available().len();
                    xai_grok_telemetry::unified_log::info(
                        "model catalog: retry succeeded",
                        None,
                        Some(serde_json::json!({ "model_count": count })),
                    );
                    mgr.notify_models_updated();
                }
                Err(e) => {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: all retries exhausted",
                        None,
                        Some(serde_json::json!({ "error": e })),
                    );
                }
            }
        });
    }

    /// One-shot background catalog refresh after readiness; no-op when a fresh
    /// disk cache already loaded a real catalog. Also kicks provider-isolated
    /// Codex/Kimi/Fireworks refreshes that were previously tied to gateway set.
    pub fn spawn_background_refresh(&self) {
        if *self.inner.has_fetched_real_catalog.read() {
            tracing::debug!(
                "skipping startup background model refresh: fresh cache already loaded"
            );
        } else {
            self.spawn_catalog_retry();
        }
        // Multi-provider catalogs also stream in after readiness (not on the
        // blocking boot path).
        self.start_codex_models_refresh();
        self.start_kimi_models_query();
        self.start_fireworks_models_query();
        self.start_opencode_go_models_query();
    }

    /// Refresh the model catalog on every auth token refresh.
    ///
    /// Listens for [`AuthManager::refresh_notifier`] signals directly,
    /// bypassing the FSEvents file watcher which can silently stop
    /// delivering events on macOS after resume from sleep. On each
    /// notification the catalog is re-fetched from the server; if the
    /// fetch succeeds and the catalog changed, clients are notified
    /// via `x.ai/models/update`.
    pub fn start_auth_refresh_watcher(&self, notify: Arc<tokio::sync::Notify>) {
        let mgr = self.clone();
        let had_catalog_at_start = *self.inner.has_fetched_real_catalog.read();
        xai_grok_telemetry::unified_log::info(
            "model catalog: auth refresh watcher started",
            None,
            Some(serde_json::json!({
                "had_real_catalog": had_catalog_at_start,
                "model_count": self.available().len(),
            })),
        );
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                // Deliberate no-fetch state: skip the refresh entirely so the
                // failure-classifying logs below keep meaning "actually failed".
                if !crate::util::config::resolve_remote_fetch_enabled() {
                    tracing::debug!(
                        "model catalog: auth refresh watcher skipped (remote_fetch disabled)"
                    );
                    continue;
                }
                let had_catalog = *mgr.inner.has_fetched_real_catalog.read();
                let old_count = mgr.available().len();
                xai_grok_telemetry::unified_log::info(
                    "model catalog: auth refresh watcher triggered",
                    None,
                    Some(serde_json::json!({
                        "had_real_catalog": had_catalog,
                        "model_count_before": old_count,
                    })),
                );
                mgr.fetch_and_apply().await;
                let has_catalog = *mgr.inner.has_fetched_real_catalog.read();
                let new_count = mgr.available().len();
                if has_catalog {
                    if !had_catalog || new_count != old_count {
                        xai_grok_telemetry::unified_log::info(
                            "model catalog: auth refresh watcher updated catalog",
                            None,
                            Some(serde_json::json!({
                                "model_count_before": old_count,
                                "model_count_after": new_count,
                                "was_recovery": !had_catalog,
                            })),
                        );
                    }
                    mgr.notify_models_updated();
                } else {
                    xai_grok_telemetry::unified_log::warn(
                        "model catalog: auth refresh watcher fetch failed",
                        None,
                        Some(serde_json::json!({
                            "model_count": old_count,
                        })),
                    );
                }
            }
        });
    }

    /// Wipe only xAI in-memory state so a previous xAI identity's catalog
    /// cannot leak. The independent Codex cache/catalog and embedded Codex
    /// fallback remain available for Codex-only sessions.
    fn clear(&self) {
        *self.inner.prefetched.write() = None;
        *self.inner.etag.write() = None;
        *self.inner.has_fetched_real_catalog.write() = false;
        self.inner
            .allowlist_excludes_all
            .store(false, Ordering::Relaxed);
        // A new identity starts fresh: drop the prior user's pick so its
        // first catalog reselects that identity's default.
        self.inner
            .user_selected_model
            .store(false, Ordering::Relaxed);
        let cfg = self.inner.cfg.read().clone();
        self.rebuild(&cfg, None);
        self.reselect_current_model_if_missing(&cfg);
        self.notify_models_updated();
    }

    /// Build a `SamplingConfig` from the current model + auth state.
    pub fn sampling_config(&self) -> SamplingConfig {
        let config = self.inner.cfg.read().clone();
        let auth_manager = self.inner.auth_manager.as_ref();
        let current_model_id = self.current_model_id();
        let all_models = self.models();
        let fallback;
        let current_model = match all_models
            .get(current_model_id.0.as_ref())
            .or_else(|| all_models.values().next())
        {
            Some(m) => m,
            None => {
                tracing::warn!("no models available in catalog; defaulting to bundled model");
                let default_id = crate::models::default_model().to_string();
                fallback = ModelEntry::fallback(&default_id, &config.endpoints);
                &fallback
            }
        };

        let session_auth = auth_manager.current_or_expired();
        let credentials =
            resolve_credentials(current_model, session_auth.as_ref().map(|a| a.key.as_str()));

        sampling_config_for_model(
            current_model,
            credentials,
            config.endpoints.alpha_test_key.clone(),
            config.client_version.clone(),
            crate::managed_config::resolve_deployment_id(
                config.endpoints.deployment_key.as_deref(),
            ),
            None,
        )
    }

    /// Disk-cache origin key for this manager's current endpoints/auth shape
    /// (see [`ModelsCache::origin`]).
    fn cache_origin(&self) -> String {
        let endpoints = self.inner.cfg.read().endpoints.clone();
        let fetch_auth = *self.inner.fetch_auth.read();
        crate::remote::models_list_url(&endpoints, fetch_auth)
    }

    fn try_load_cache(&self) -> bool {
        let fetch_auth = *self.inner.fetch_auth.read();
        let Some(cached) = self
            .inner
            .cache
            .load_fresh(&fetch_auth.cache_auth_method(), &self.cache_origin())
        else {
            return false;
        };
        let cfg = self.inner.cfg.read().clone();
        *self.inner.has_fetched_real_catalog.write() = true;
        *self.inner.prefetched.write() = Some(cached.models.clone());
        self.rebuild(&cfg, Some(cached.models));
        *self.inner.etag.write() = cached.etag;
        true
    }

    /// A catalog-fetch session refresh bounded by `STARTUP_AUTH_REFRESH_TIMEOUT`.
    /// A hung IdP on a cold cache degrades to a session-less fetch (the
    /// bundled/cache catalog stays and the next refresh retries) instead of
    /// stalling boot, mirroring the readiness path's no-mint auth bound.
    async fn bounded_startup_auth(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
        Self::bounded_auth_refresh(async { auth_manager.auth().await.ok() }).await
    }

    /// Bounds an auth-refresh future to `STARTUP_AUTH_REFRESH_TIMEOUT`, yielding
    /// `None` on timeout. Split out so the timeout contract is unit-testable
    /// without a live IdP.
    async fn bounded_auth_refresh<F>(fut: F) -> Option<GrokAuth>
    where
        F: std::future::Future<Output = Option<GrokAuth>>,
    {
        match tokio::time::timeout(crate::http::STARTUP_AUTH_REFRESH_TIMEOUT, fut).await {
            Ok(auth) => auth,
            Err(_) => {
                tracing::warn!(
                    timeout_secs = crate::http::STARTUP_AUTH_REFRESH_TIMEOUT.as_secs(),
                    "model catalog: auth refresh timed out; fetching without a fresh session"
                );
                None
            }
        }
    }

    fn spawn_fetch(&self, new_etag: Option<String>) {
        self.spawn_fetch_inner(
            new_etag,
            crate::util::config::resolve_remote_fetch_enabled(),
        );
    }

    /// `remote_fetch_enabled` is a parameter so tests can drive the gate without touching on-disk config.
    fn spawn_fetch_inner(&self, new_etag: Option<String>, remote_fetch_enabled: bool) {
        // Degrade to Offline: keep serving the current (cache/static) catalog.
        if !remote_fetch_enabled {
            tracing::info!("model catalog refresh skipped: remote_fetch disabled");
            return;
        }
        if self
            .inner
            .refresh_in_flight
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tracing::debug!("model catalog refresh already in flight, skipping");
            return;
        }
        let cfg = self.inner.cfg.read().clone();
        let endpoints = cfg.endpoints.clone();
        let fetch_auth = *self.inner.fetch_auth.read();
        let auth_manager = self.inner.auth_manager.clone();
        let mgr = self.clone();

        tokio::task::spawn(async move {
            let _refresh_guard = RefreshInFlightGuard(mgr.inner.clone());
            let auth = Self::bounded_startup_auth(&auth_manager).await;
            let new_prefetched = match tokio::time::timeout(
                crate::http::STARTUP_FETCH_TIMEOUT,
                fetch_models_async(endpoints, auth, fetch_auth),
            )
            .await
            {
                Ok(models) => models,
                Err(_) => {
                    tracing::warn!("etag-triggered model refresh timed out");
                    None
                }
            };
            if !mgr.apply_refresh_result(&cfg, new_prefetched, new_etag) {
                return;
            }
            tracing::info!("models manager refreshed");
            mgr.notify_models_updated();
        });
    }

    /// Fetch models, rebuild state, and notify clients.
    fn do_refresh(&self, new_etag: Option<String>, strategy: RefreshStrategy) {
        match strategy {
            RefreshStrategy::Offline => {
                if self.try_load_cache() {
                    tracing::info!("models manager refreshed from cache (offline)");
                }
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.try_load_cache() {
                    tracing::info!("models manager refreshed from cache (online_if_uncached)");
                    return;
                }
                self.spawn_fetch(new_etag);
            }
            RefreshStrategy::Online => {
                self.spawn_fetch(new_etag);
            }
        }
    }

    /// Resolve the model list: tries cache first, then fetches from the network.
    pub async fn list_models(&self, strategy: RefreshStrategy) {
        match strategy {
            RefreshStrategy::Offline => {
                self.try_load_cache();
            }
            RefreshStrategy::OnlineIfUncached => {
                if self.try_load_cache() {
                    return;
                }
                self.fetch_and_apply().await;
            }
            RefreshStrategy::Online => {
                self.fetch_and_apply().await;
            }
        }
    }

    async fn fetch_and_apply(&self) {
        self.fetch_and_apply_inner(crate::util::config::resolve_remote_fetch_enabled())
            .await
    }

    /// `remote_fetch_enabled` is a parameter so tests can drive the gate
    /// without touching on-disk config layers.
    async fn fetch_and_apply_inner(&self, remote_fetch_enabled: bool) {
        // Degrade to Offline: keep serving the current (cache/static) catalog.
        if !remote_fetch_enabled {
            tracing::info!("model catalog refresh skipped: remote_fetch disabled");
            return;
        }
        let auth = Self::bounded_startup_auth(&self.inner.auth_manager).await;
        let has_auth = auth.is_some();
        let fetch_auth = *self.inner.fetch_auth.read();
        let cfg = self.inner.cfg.read().clone();
        xai_grok_telemetry::unified_log::info(
            "model catalog: fetching",
            None,
            Some(serde_json::json!({
                "has_auth": has_auth,
                "fetch_auth": format!("{fetch_auth:?}"),
            })),
        );
        let new_prefetched = match tokio::time::timeout(
            crate::http::STARTUP_FETCH_TIMEOUT,
            fetch_models_async(cfg.endpoints.clone(), auth, fetch_auth),
        )
        .await
        {
            Ok(res) => res,
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = crate::http::STARTUP_FETCH_TIMEOUT.as_secs(),
                    "model catalog fetch timed out"
                );
                None
            }
        };
        let success = self.apply_refresh_result(&cfg, new_prefetched, None);
        if success {
            xai_grok_telemetry::unified_log::info(
                "model catalog: fetch succeeded",
                None,
                Some(serde_json::json!({
                    "model_count": self.available().len(),
                })),
            );
        }
    }

    fn apply_refresh_result(
        &self,
        config: &config::Config,
        new_prefetched: Option<IndexMap<String, ModelEntry>>,
        new_etag: Option<String>,
    ) -> bool {
        let Some(new_prefetched) = new_prefetched else {
            tracing::warn!("model refresh failed, leaving existing models unchanged");
            xai_grok_telemetry::unified_log::warn(
                "model catalog refresh failed",
                None,
                Some(serde_json::json!({
                    "had_real_catalog": *self.inner.has_fetched_real_catalog.read(),
                })),
            );
            return false;
        };

        let first_real_catalog = {
            let mut flag = self.inner.has_fetched_real_catalog.write();
            let was_first = !*flag;
            *flag = true;
            was_first
        };
        *self.inner.prefetched.write() = Some(new_prefetched.clone());
        self.rebuild(config, Some(new_prefetched));
        *self.inner.etag.write() = new_etag;

        // Can't exit a running app; flag it so the prompt path blocks instead.
        let excludes_all = allowlist_matches_nothing(config, &self.inner.models.read());
        self.inner
            .allowlist_excludes_all
            .store(excludes_all, Ordering::Relaxed);
        if excludes_all {
            tracing::error!("allowed_models excludes all fetched models; prompts will be blocked");
        }

        // Respect an explicit pre-catalog `/model` pick: auto-select the
        // default on the first catalog only when the user hasn't chosen.
        // Either way a now-invalid selection is replaced.
        if first_real_catalog && !self.inner.user_selected_model.load(Ordering::Relaxed) {
            self.reselect_default_model(config);
        } else {
            self.reselect_current_model_if_missing(config);
        }
        true
    }

    pub fn allowlist_excludes_all(&self) -> bool {
        self.inner.allowlist_excludes_all.load(Ordering::Relaxed)
    }

    /// Re-pick the default if `current_model_id` is gone from the catalog *or*
    /// is no longer `user_selectable` (e.g. a config reload narrowed
    /// `allowed_models`), so UI and sampling don't disagree on the active model.
    fn reselect_current_model_if_missing(&self, config: &config::Config) {
        let current = self.inner.current_model_id.read().clone();
        let has_xai_session = self.is_session_auth();
        let has_codex_session = self.is_codex_session_auth();
        let needs_reselection = {
            let models = self.inner.models.read();
            match models.get(current.0.as_ref()) {
                None => true,
                Some(entry) => {
                    !entry.info.user_selectable
                        || !model_available_for_provider_auth(
                            entry,
                            has_xai_session,
                            has_codex_session,
                        )
                }
            }
        };
        if !needs_reselection {
            return;
        }
        let (key, _, source) = {
            let models = self.inner.models.read();
            resolve_default_model_with_provider_auth(
                config,
                &models,
                self.is_session_auth(),
                self.is_codex_session_auth(),
            )
        };
        let new_id = acp::ModelId::new(Arc::from(key));
        tracing::info!(
            old = %current.0, new = %new_id.0, source = %source,
            "current model not in new catalog, reselecting default"
        );
        self.set_current_model_id_internal(new_id);
    }

    /// Re-resolve the default model against the current catalog.
    ///
    /// Called on first catalog fetch and when `apply_config` detects a
    /// preferred-model change.
    fn reselect_default_model(&self, config: &config::Config) {
        let (key, _, source) = {
            let models = self.inner.models.read();
            resolve_default_model_with_provider_auth(
                config,
                &models,
                self.is_session_auth(),
                self.is_codex_session_auth(),
            )
        };
        let new_id = acp::ModelId::new(Arc::from(key));
        let current = self.inner.current_model_id.read().clone();
        if current.0.as_ref() != new_id.0.as_ref() {
            tracing::info!(
                old = %current.0, new = %new_id.0, source = %source,
                "re-resolved default model after catalog populated"
            );
            self.set_current_model_id_internal(new_id);
        }
    }
}

// ── Refresh strategy ────────────────────────────────────────────────────────

/// How to resolve the model list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from network, ignore cache.
    Online,
    /// Only use cached data, never fetch.
    Offline,
    /// Use cache if fresh, otherwise fetch.
    OnlineIfUncached,
}

mod cache;
mod endpoint;
mod fetch;
mod resolution;

pub(crate) use cache::*;
pub(crate) use endpoint::*;
pub(crate) use fetch::*;
pub use fetch::{
    EarlyPrefetchHandle, EarlyPrefetchResult, start_early_prefetch,
    start_early_prefetch_settings_only, start_early_prefetch_with_auth,
};
pub(crate) use resolution::*;

#[cfg(test)]
mod tests;
