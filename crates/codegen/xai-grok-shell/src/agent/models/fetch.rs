use super::*;

// ── Fetch ───────────────────────────────────────────────────────────────────

/// Build the prefetched model map from a flat list of entries.
///
/// Each entry is keyed by its `id` field (falling back to the `model` slug
/// when `id` is absent). This lets A/B experiments that share the same
/// routing slug (e.g. "Auto" and "Grok Build" both route to `grok-build`)
/// coexist in the catalog without collision.
pub(crate) fn build_prefetched_map(
    models: Vec<config::ModelEntryConfig>,
    api_base_url_override: Option<String>,
) -> IndexMap<String, ModelEntry> {
    let mut map: IndexMap<String, ModelEntry> = IndexMap::with_capacity(models.len());
    for m in models {
        let key = m.id.clone().unwrap_or_else(|| m.model.clone());
        let info = config::ModelInfo::from_config(&m);
        let entry = ModelEntry {
            info,
            api_key: None,
            env_key: None,
            auth_provider: None,
            api_base_url: m.api_base_url.clone().or(api_base_url_override.clone()),
        };
        map.insert(key, entry);
    }
    map
}

/// Fetch remote models. Checks disk cache first; persists after fetch.
pub(crate) fn prefetch_models_blocking(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
) -> Option<IndexMap<String, ModelEntry>> {
    prefetch_models_blocking_gated(
        endpoints,
        auth,
        fetch_auth,
        crate::util::config::resolve_remote_fetch_enabled(),
    )
}

/// Blocking models + `/v1/settings` prefetch pair, shared by the early
/// prefetch thread and the leader's startup phase so the settings gate lives
/// once. The remote_fetch knob is resolved a single time so the two fetch
/// decisions cannot disagree mid-startup.
pub(crate) fn prefetch_models_and_settings_blocking(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
) -> (
    Option<IndexMap<String, ModelEntry>>,
    Option<crate::util::config::RemoteSettings>,
) {
    let remote_fetch_enabled = crate::util::config::resolve_remote_fetch_enabled();
    let models = prefetch_models_blocking_gated(endpoints, auth, fetch_auth, remote_fetch_enabled);
    // Settings need a grok.com session; skip for BYOK.
    let settings = match auth {
        Some(auth) if remote_fetch_enabled => {
            let _timer = crate::instrumentation_timer!("startup.early_settings_fetch");
            crate::remote::fetch_settings_blocking(
                &endpoints.proxy_url(),
                auth,
                endpoints.alpha_test_key.as_deref(),
            )
            .into_option()
        }
        _ => None,
    };
    (models, settings)
}

/// `remote_fetch_enabled` is a parameter so the pair helper above resolves the
/// knob once for both halves.
pub(crate) fn prefetch_models_blocking_gated(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
    remote_fetch_enabled: bool,
) -> Option<IndexMap<String, ModelEntry>> {
    fetch_models_uncommitted(endpoints, auth, fetch_auth, remote_fetch_enabled).commit()
}

pub(super) enum ModelsPrefetch {
    Cached(IndexMap<String, ModelEntry>),
    Fetched(ModelsCacheWrite),
    Unavailable,
}

impl ModelsPrefetch {
    pub(super) fn commit(self) -> Option<IndexMap<String, ModelEntry>> {
        match self {
            Self::Cached(models) => Some(models),
            Self::Fetched(write) => Some(write.commit()),
            Self::Unavailable => None,
        }
    }

    pub(super) fn models(&self) -> Option<&IndexMap<String, ModelEntry>> {
        match self {
            Self::Cached(models) => Some(models),
            Self::Fetched(write) => Some(&write.models),
            Self::Unavailable => None,
        }
    }
}

pub(super) struct ModelsCacheWrite {
    models: IndexMap<String, ModelEntry>,
    etag: Option<String>,
    auth_method: CacheAuthMethod,
    origin: String,
}

impl ModelsCacheWrite {
    fn commit(self) -> IndexMap<String, ModelEntry> {
        ModelsCacheManager::new().persist(
            &self.models,
            self.etag.as_deref(),
            self.auth_method,
            &self.origin,
        );
        self.models
    }
}

fn fetch_models_uncommitted(
    endpoints: &config::EndpointsConfig,
    auth: Option<&GrokAuth>,
    fetch_auth: ModelFetchAuth,
    remote_fetch_enabled: bool,
) -> ModelsPrefetch {
    let cache_auth = fetch_auth.cache_auth_method();
    // Same URL the fetch below will hit — the cache is only valid for it.
    let cache_origin = crate::remote::models_list_url(endpoints, fetch_auth);
    let cache = ModelsCacheManager::new();
    if let Some(cached) = cache.load_fresh(&cache_auth, &cache_origin) {
        return ModelsPrefetch::Cached(cached.models);
    }

    // Every catalog fetch in the product funnels through here, so this single
    // gate also covers callers that don't go through the prefetch-env check
    // (leader, headless, stdio, server). Cache above is local and stays usable.
    if !remote_fetch_enabled {
        tracing::info!("models fetch skipped: remote_fetch disabled");
        return ModelsPrefetch::Unavailable;
    }

    let _timer = crate::instrumentation_timer!("startup.fetch_models_blocking");
    match fetch_models_blocking(endpoints, auth, fetch_auth) {
        Ok(FetchModelsResult { models, etag }) if !models.is_empty() => {
            let api_base_url_override = match fetch_auth {
                ModelFetchAuth::ApiKey => Some(endpoints.xai_api_base_url.clone()),
                _ => None,
            };
            let map = build_prefetched_map(models, api_base_url_override);

            // NOTE: inheriting context_window / agent_type / api_backend
            // from hardcoded defaults is handled centrally in
            // `resolve_model_list` (config.rs), not here. Don't re-add it.

            tracing::info!(count = map.len(), etag = ?etag, "Prefetched models");
            ModelsPrefetch::Fetched(ModelsCacheWrite {
                models: map,
                etag,
                auth_method: cache_auth,
                origin: cache_origin,
            })
        }
        Ok(FetchModelsResult { .. }) => {
            tracing::warn!("Models endpoint returned empty list");
            ModelsPrefetch::Unavailable
        }
        Err(e) => {
            tracing::warn!("Failed to fetch models: {:?}", e);
            ModelsPrefetch::Unavailable
        }
    }
}

/// Startup prefetch result: models + remote settings.
pub struct EarlyPrefetchResult {
    pub models: Option<IndexMap<String, ModelEntry>>,
    pub settings: Option<crate::util::config::RemoteSettings>,
}

/// Handle for a startup prefetch thread.
pub type EarlyPrefetchHandle = std::thread::JoinHandle<EarlyPrefetchResult>;

pub(crate) struct PrefetchEnv {
    pub(crate) auth: Option<GrokAuth>,
    pub(crate) endpoints: config::EndpointsConfig,
    pub(crate) model_fetch_auth: ModelFetchAuth,
}

pub(crate) fn resolve_prefetch_env_with_auth(auth: Option<GrokAuth>) -> Option<PrefetchEnv> {
    let _timer = crate::instrumentation_timer!("startup.early_prefetch_launch");
    // Config-aware (not env-only) so the prefetch can't leak the bearer to api.x.ai.
    let mut endpoints = config::EndpointsConfig::from_effective_config();

    if endpoints.deployment_key.is_none() {
        endpoints.deployment_key = crate::managed_config::resolve_deployment_key();
    }

    resolve_prefetch_env_from_parts(
        auth,
        endpoints,
        crate::util::config::resolve_remote_fetch_enabled(),
    )
}

/// Decision core of [`resolve_prefetch_env_with_auth`], split from the config
/// loading so the gate is unit-testable.
///
/// `remote_fetch_enabled = false` wins over every credential shape AND over
/// `has_custom_endpoint()` (which otherwise forces the prefetch to run): the
/// explicit off switch must hold even when a stray login, `XAI_API_KEY`, or
/// `deployment_key` would re-arm the prefetch — and with it the `/v1/settings`
/// fetch and the deployment-config sync on the prefetch thread.
pub(crate) fn resolve_prefetch_env_from_parts(
    auth: Option<GrokAuth>,
    endpoints: config::EndpointsConfig,
    remote_fetch_enabled: bool,
) -> Option<PrefetchEnv> {
    if !remote_fetch_enabled {
        tracing::info!("startup model/settings prefetch skipped: remote_fetch disabled");
        return None;
    }

    let model_fetch_auth = ModelFetchAuth::resolve(&endpoints, auth.is_some());

    if auth.is_none()
        && !endpoints.has_custom_endpoint()
        && model_fetch_auth == ModelFetchAuth::Session
    {
        return None;
    }

    Some(PrefetchEnv {
        auth,
        endpoints,
        model_fetch_auth,
    })
}

pub(crate) fn resolve_prefetch_env(grok_com_config: Option<GrokComConfig>) -> Option<PrefetchEnv> {
    let grok_home = crate::util::grok_home::grok_home();
    let auth_manager = AuthManager::new(&grok_home, grok_com_config.unwrap_or_default());
    let auth = auth_manager.current();
    resolve_prefetch_env_with_auth(auth)
}

/// Start model + settings prefetch on a background thread using pre-resolved auth.
///
/// When the caller has already obtained valid credentials (e.g. via
/// `try_ensure_fresh_auth`), pass them here to avoid re-reading stale cached
/// credentials from disk.
pub fn start_early_prefetch_with_auth(auth: Option<GrokAuth>) -> Option<EarlyPrefetchHandle> {
    startup_prefetch::begin_with_auth(auth)
        .then(|| std::thread::spawn(startup_prefetch::wait_early_result))
}

/// Start model + settings prefetch on a background thread.
///
/// Convenience wrapper that reads cached auth from disk. Prefer
/// `start_early_prefetch_with_auth` when you have pre-resolved credentials.
/// Joins the shared startup fetch without synchronizing managed policy.
pub fn start_early_prefetch(grok_com_config: Option<GrokComConfig>) -> Option<EarlyPrefetchHandle> {
    startup_prefetch::begin(grok_com_config)
        .then(|| std::thread::spawn(startup_prefetch::wait_early_result))
}

/// Prefetch models + remote settings only — **no** managed-config sync.
///
/// Cache writes stay deferred until bootstrap accepts the shared fetch after
/// the policy gate. Open Grok still drops remote product kill-switches.
pub fn start_early_prefetch_settings_only(
    grok_com_config: Option<GrokComConfig>,
) -> Option<EarlyPrefetchHandle> {
    start_early_prefetch(grok_com_config)
}

pub(super) fn run_prefetch(
    env: PrefetchEnv,
) -> (
    ModelsPrefetch,
    Option<crate::util::config::RemoteSettings>,
    Option<SettingsCacheWrite>,
) {
    let mut timer = crate::instrumentation_timer!("startup.early_prefetch");
    let proxy_endpoint = env.endpoints.proxy_url();
    timer.with_field("endpoint", proxy_endpoint.as_str());
    let remote_fetch_enabled = crate::util::config::resolve_remote_fetch_enabled();
    let models = fetch_models_uncommitted(
        &env.endpoints,
        env.auth.as_ref(),
        env.model_fetch_auth,
        remote_fetch_enabled,
    );
    let (settings, settings_write) = match env.auth.as_ref() {
        Some(auth) if remote_fetch_enabled && env.model_fetch_auth == ModelFetchAuth::Session => {
            SettingsCacheManager::new().load_or_fetch(
                auth,
                &proxy_endpoint,
                env.endpoints.alpha_test_key.as_deref(),
                || {
                    let _timer = crate::instrumentation_timer!("startup.early_settings_fetch");
                    crate::remote::fetch_settings_blocking(
                        &proxy_endpoint,
                        auth,
                        env.endpoints.alpha_test_key.as_deref(),
                    )
                    .into_option()
                },
            )
        }
        _ => (None, None),
    };
    (models, settings, settings_write)
}
