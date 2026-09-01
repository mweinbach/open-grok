use super::*;

// ── Disk cache ──────────────────────────────────────────────────────────────

pub(crate) const MODELS_CACHE_FILE: &str = "models_cache.json";
pub(crate) const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(300);

pub(crate) fn is_fresh(fetched_at: DateTime<Utc>, ttl: std::time::Duration) -> bool {
    let Ok(ttl) = ChronoDuration::from_std(ttl) else {
        return false;
    };
    let age = Utc::now().signed_duration_since(fetched_at);
    age >= ChronoDuration::zero() && age < ttl
}

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
        is_fresh(self.fetched_at, ttl)
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
        if let Ok(json) = serde_json::to_vec_pretty(cache) {
            write_private_atomic(&self.path, self.ttl, &json);
        }
    }

    pub(crate) async fn atomic_write_async(&self, cache: &ModelsCache) {
        let Ok(json) = serde_json::to_vec_pretty(cache) else {
            return;
        };
        let path = self.path.clone();
        let ttl = self.ttl;
        let _ = tokio::task::spawn_blocking(move || write_private_atomic(&path, ttl, &json)).await;
    }
}

pub(crate) fn read_capped(path: &std::path::Path, maximum: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buffer = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(maximum.saturating_add(1))
        .read_to_end(&mut buffer)
        .ok()?;
    if buffer.len() as u64 > maximum {
        return None;
    }
    Some(buffer)
}

fn unique_tmp_path(path: &std::path::Path) -> std::path::PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    path.with_extension(format!("json.tmp.{}.{sequence}", std::process::id()))
}

fn sweep_stale_tmp(path: &std::path::Path, ttl: std::time::Duration) {
    let (Some(parent), Some(stem)) = (
        path.parent(),
        path.file_name().and_then(|name| name.to_str()),
    ) else {
        return;
    };
    let prefix = format!("{stem}.tmp.");
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with(&prefix)
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > ttl)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn write_private_atomic(path: &std::path::Path, ttl: std::time::Duration, bytes: &[u8]) {
    use std::io::Write;
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    sweep_stale_tmp(path, ttl);
    let temporary = unique_tmp_path(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options
        .open(&temporary)
        .and_then(|mut file| file.write_all(bytes))
        .is_ok();
    if written && std::fs::rename(&temporary, path).is_ok() {
        return;
    }
    let _ = std::fs::remove_file(&temporary);
}
