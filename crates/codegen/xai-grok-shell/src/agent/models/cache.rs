use super::*;

// ── Disk cache ──────────────────────────────────────────────────────────────

pub(crate) const MODELS_CACHE_FILE: &str = "models_cache.json";
pub(crate) const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct ModelsCache {
    pub(crate) fetched_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) grok_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auth_method: Option<CacheAuthMethod>,
    /// Models-list URL this catalog was fetched from
    /// ([`crate::remote::models_list_url`]). Compared on load so a cache
    /// written against one backend is a miss for another: entries embed
    /// absolute `base_url`s, so adopting a foreign-origin cache silently
    /// re-points inference (the windows lifecycle e2e failed exactly this
    /// way — test 1's mock-server catalog, cached in the shared profile,
    /// sent test 2's prompts to a dead port). `None` (legacy files) never
    /// matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) etag: Option<String>,
    pub(crate) models: IndexMap<String, ModelEntry>,
}

impl ModelsCache {
    pub(crate) fn is_fresh(&self, ttl: std::time::Duration) -> bool {
        let Ok(ttl) = ChronoDuration::from_std(ttl) else {
            return false;
        };
        let age = Utc::now().signed_duration_since(self.fetched_at);
        age >= ChronoDuration::zero() && age < ttl
    }
}

pub(crate) struct CacheResult {
    pub(crate) models: IndexMap<String, ModelEntry>,
    pub(crate) etag: Option<String>,
}

pub(crate) struct ModelsCacheManager {
    pub(crate) path: std::path::PathBuf,
    pub(crate) ttl: std::time::Duration,
}

impl ModelsCacheManager {
    pub(crate) fn new() -> Self {
        Self {
            path: crate::util::grok_home::grok_home().join(MODELS_CACHE_FILE),
            ttl: CACHE_TTL,
        }
    }

    /// Sync; used by `prefetch_models_blocking`. Will be removed once startup
    /// prefetch is async.
    pub(crate) fn load_fresh(
        &self,
        expected_auth: &CacheAuthMethod,
        expected_origin: &str,
    ) -> Option<CacheResult> {
        let data = std::fs::read(&self.path).ok()?;
        let cache: ModelsCache = serde_json::from_slice(&data).ok()?;
        if cache.grok_version.as_deref() != Some(xai_grok_version::version()) {
            tracing::debug!("models cache version mismatch");
            return None;
        }
        if cache.auth_method.as_ref() != Some(expected_auth) {
            tracing::debug!("models cache auth method mismatch");
            return None;
        }
        if cache.origin.as_deref() != Some(expected_origin) {
            tracing::debug!(
                cached = ?cache.origin,
                expected = expected_origin,
                "models cache origin mismatch"
            );
            return None;
        }
        if !cache.is_fresh(self.ttl) {
            tracing::debug!("models cache is stale");
            return None;
        }
        tracing::debug!(count = cache.models.len(), "loaded models from disk cache");
        Some(CacheResult {
            models: cache.models,
            etag: cache.etag,
        })
    }

    /// Sync; see `load_fresh` note.
    pub(crate) fn persist(
        &self,
        models: &IndexMap<String, ModelEntry>,
        etag: Option<&str>,
        auth_method: CacheAuthMethod,
        origin: &str,
    ) {
        let cache = ModelsCache {
            fetched_at: Utc::now(),
            grok_version: Some(xai_grok_version::version().to_string()),
            auth_method: Some(auth_method),
            origin: Some(origin.to_string()),
            etag: etag.map(|s| s.to_string()),
            models: models.clone(),
        };
        self.atomic_write(&cache);
    }

    pub(crate) async fn renew_ttl(&self, expected_auth: &CacheAuthMethod, expected_origin: &str) {
        let data = match tokio::fs::read(&self.path).await {
            Ok(data) => data,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                tracing::warn!(error = %e, "models cache TTL renewal: read failed");
                return;
            }
        };
        let Ok(mut cache) = serde_json::from_slice::<ModelsCache>(&data) else {
            return;
        };
        if cache.auth_method.as_ref() != Some(expected_auth) {
            tracing::debug!("models cache TTL renewal skipped: auth method mismatch");
            return;
        }
        if cache.origin.as_deref() != Some(expected_origin) {
            tracing::debug!("models cache TTL renewal skipped: origin mismatch");
            return;
        }
        cache.fetched_at = Utc::now();
        self.atomic_write_async(&cache).await;
        tracing::debug!("models cache TTL renewed");
    }

    /// Sync; see `load_fresh` note.
    pub(crate) fn invalidate(&self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => tracing::info!("models disk cache invalidated"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, "failed to invalidate models disk cache"),
        }
    }

    /// Sync; see `load_fresh` note.
    pub(crate) fn atomic_write(&self, cache: &ModelsCache) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = self.path.with_extension("json.tmp");
        if let Ok(json) = serde_json::to_vec_pretty(cache)
            && std::fs::write(&tmp, &json).is_ok()
        {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub(crate) async fn atomic_write_async(&self, cache: &ModelsCache) {
        if let Some(parent) = self.path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let tmp = self.path.with_extension("json.tmp");
        let Ok(json) = serde_json::to_vec_pretty(cache) else {
            return;
        };
        if tokio::fs::write(&tmp, &json).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &self.path).await;
        }
    }
}
