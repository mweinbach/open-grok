//! Live ChatGPT Codex model discovery and its provider-scoped disk cache.
//!
//! This module deliberately does not use the xAI model transport, auth manager,
//! or `models_cache.json`. Codex models are fetched with the independent OAuth
//! credentials in `$OPENGROK_HOME/codex-auth.json` and cached in
//! `$OPENGROK_HOME/codex_models_cache.json`.

use crate::agent::config::{ModelEntry, ModelInfo};
use crate::codex_auth::{self, CodexCredentials};
use anyhow::{Context, anyhow};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use indexmap::IndexMap;
use reqwest::StatusCode;
use reqwest::header::{ETAG, USER_AGENT};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use url::Url;
use xai_grok_sampling_types::{
    ApiBackend, ModelProvider, ReasoningEffort, ReasoningEffortOption, ReasoningSummary, ToolMode,
};

pub(crate) const CODEX_MODELS_CACHE_FILE: &str = "codex_models_cache.json";
pub(crate) const CODEX_CLIENT_VERSION_ENV: &str = "OPENGROK_CODEX_CLIENT_VERSION";
/// Compatibility version of the pinned official Codex snapshot whose model
/// catalog contract Open Grok implements. This is intentionally independent
/// from Open Grok's own package version.
pub(crate) const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.144.5";
const CODEX_MODELS_CACHE_TTL: Duration = Duration::from_secs(300);
const CODEX_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_EFFECTIVE_CONTEXT_WINDOW_PERCENT: i64 = 95;

const fn default_true() -> bool {
    true
}

/// Resolve the Codex compatibility version advertised to `/models`.
/// Prerelease/build suffixes are deliberately removed, matching codex-rs's
/// `client_version_to_whole` behavior.
pub(crate) fn codex_client_version() -> String {
    match std::env::var(CODEX_CLIENT_VERSION_ENV) {
        Ok(value) => match normalize_whole_semver(&value) {
            Some(version) => version,
            None => {
                tracing::warn!(
                    value = %value,
                    fallback = DEFAULT_CODEX_CLIENT_VERSION,
                    "invalid OPENGROK_CODEX_CLIENT_VERSION; using pinned compatibility version"
                );
                DEFAULT_CODEX_CLIENT_VERSION.to_owned()
            }
        },
        Err(_) => DEFAULT_CODEX_CLIENT_VERSION.to_owned(),
    }
}

fn normalize_whole_semver(value: &str) -> Option<String> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    let version = semver::Version::parse(value).ok()?;
    Some(format!(
        "{}.{}.{}",
        version.major, version.minor, version.patch
    ))
}

/// Codex backend visibility is distinct from xAI's auth-dependent picker flag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CodexModelVisibility {
    List,
    Hide,
    None,
}

impl Default for CodexModelVisibility {
    fn default() -> Self {
        Self::None
    }
}

impl CodexModelVisibility {
    pub(crate) fn is_list_visible(self) -> bool {
        self == Self::List
    }
}

/// One converted remote model plus metadata needed for Codex merge semantics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CodexCatalogModel {
    pub(crate) priority: i32,
    pub(crate) visibility: CodexModelVisibility,
    /// Raw provider override. Resolution clamps this to 90% of the raw
    /// context window, matching codex-rs `ModelInfo::auto_compact_token_limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_compact_token_limit: Option<i64>,
    /// Opaque compatibility identifier used to compact before a model's
    /// compaction contract changes between turns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) comp_hash: Option<String>,
    /// `context_window.or(max_context_window)` before the 95%-effective picker
    /// projection. Needed because upstream clamps against the raw window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_context_window: Option<i64>,
    pub(crate) entry: ModelEntry,
}

impl CodexCatalogModel {
    pub(crate) fn slug(&self) -> &str {
        &self.entry.info.model
    }

    pub(crate) fn resolved_auto_compact_token_limit(&self) -> Option<u64> {
        let context_limit = self
            .resolved_context_window
            .map(|context_window| (context_window * 9) / 10);
        let resolved = match (context_limit, self.auto_compact_token_limit) {
            (Some(context_limit), Some(config_limit)) => Some(config_limit.min(context_limit)),
            (Some(context_limit), None) => Some(context_limit),
            (None, config_limit) => config_limit,
        }?;
        // Provider metadata is expected to be positive. Treat a malformed
        // non-positive limit as zero, which preserves codex-rs's immediate
        // threshold behavior without wrapping into an enormous u64.
        Some(u64::try_from(resolved).unwrap_or(0))
    }
}

/// A provider-scoped snapshot returned by the live endpoint or disk cache.
#[derive(Clone, Debug)]
pub(crate) struct CodexModelsCatalog {
    pub(crate) models: Vec<CodexCatalogModel>,
    pub(crate) etag: Option<String>,
    /// Non-secret digest of the ChatGPT principal whose credentials produced
    /// this snapshot. The manager checks it again immediately before publish
    /// so an account switch cannot expose account A's in-flight catalog to B.
    account_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodexCompactionMetadata {
    pub(crate) auto_compact_token_limit: Option<u64>,
    pub(crate) comp_hash: Option<String>,
}

impl CodexModelsCatalog {
    /// ChatGPT treats a non-empty remote catalog containing at least one listed
    /// model as authoritative for the Codex partition of the combined picker.
    pub(crate) fn is_authoritative(&self) -> bool {
        self.models
            .iter()
            .any(|model| model.visibility.is_list_visible())
    }

    /// Return all remote entries, including hidden entries. Keeping hidden
    /// entries allows the live catalog to hide a same-slug embedded fallback.
    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.models
            .iter()
            .map(|model| (model.slug().to_owned(), model.entry.clone()))
            .collect()
    }

    /// Return only entries that the Codex backend marks as picker-visible.
    pub(crate) fn list_visible_entries(&self) -> IndexMap<String, ModelEntry> {
        self.models
            .iter()
            .filter(|model| model.visibility.is_list_visible())
            .map(|model| (model.slug().to_owned(), model.entry.clone()))
            .collect()
    }

    fn account_fingerprint(&self) -> &str {
        &self.account_fingerprint
    }

    pub(crate) fn compaction_metadata(&self, slug: &str) -> Option<CodexCompactionMetadata> {
        self.models
            .iter()
            .find(|model| model.slug() == slug)
            .map(|model| CodexCompactionMetadata {
                auto_compact_token_limit: model.resolved_auto_compact_token_limit(),
                comp_hash: model.comp_hash.clone(),
            })
    }
}

#[derive(Clone, Debug, Deserialize)]
struct CodexModelsResponse {
    models: Vec<CodexWireModel>,
}

/// Forward-compatible subset of codex-rs's `openai_models::ModelInfo`.
/// Unknown server fields are intentionally ignored.
#[derive(Clone, Debug, Deserialize)]
struct CodexWireModel {
    slug: String,
    display_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    default_reasoning_level: Option<String>,
    #[serde(default)]
    supported_reasoning_levels: Vec<CodexWireReasoningLevel>,
    /// Whether the model accepts the Responses `reasoning.summary` member.
    /// Older catalogs omitted this field and Codex defaults it to supported.
    #[serde(default = "default_true")]
    supports_reasoning_summary_parameter: bool,
    /// Model-selected summary detail. Missing values default to `detailed` so
    /// supported Codex models expose useful reasoning summaries by default.
    #[serde(default)]
    default_reasoning_summary: ReasoningSummary,
    #[serde(default)]
    visibility: CodexModelVisibility,
    #[serde(default)]
    supported_in_api: bool,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_context_window: Option<i64>,
    #[serde(default)]
    auto_compact_token_limit: Option<i64>,
    #[serde(default)]
    comp_hash: Option<String>,
    #[serde(default = "default_effective_context_window_percent")]
    effective_context_window_percent: i64,
    #[serde(default)]
    supports_search_tool: bool,
    #[serde(default)]
    tool_mode: Option<String>,
    #[serde(default)]
    multi_agent_version: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct CodexWireReasoningLevel {
    effort: String,
    #[serde(default)]
    description: String,
}

const fn default_effective_context_window_percent() -> i64 {
    DEFAULT_EFFECTIVE_CONTEXT_WINDOW_PERCENT
}

#[derive(Debug, Serialize, Deserialize)]
struct CodexModelsCache {
    fetched_at: DateTime<Utc>,
    open_grok_version: String,
    client_version: String,
    base_origin: String,
    base_url: String,
    account_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    models: Vec<CodexCatalogModel>,
}

impl CodexModelsCache {
    fn is_fresh(&self, ttl: Duration) -> bool {
        let Ok(ttl) = ChronoDuration::from_std(ttl) else {
            return false;
        };
        let age = Utc::now().signed_duration_since(self.fetched_at);
        age >= ChronoDuration::zero() && age < ttl
    }

    fn into_catalog(self) -> CodexModelsCatalog {
        CodexModelsCatalog {
            models: self.models,
            etag: self.etag,
            account_fingerprint: self.account_fingerprint,
        }
    }
}

#[async_trait]
trait CodexModelsAuthSource: fmt::Debug + Send + Sync {
    fn current_credentials(&self) -> anyhow::Result<Option<CodexCredentials>>;
    async fn fresh_credentials(&self) -> anyhow::Result<Option<CodexCredentials>>;
    async fn force_refresh(&self) -> anyhow::Result<Option<CodexCredentials>>;
}

#[derive(Debug)]
struct ProductionCodexModelsAuthSource;

#[async_trait]
impl CodexModelsAuthSource for ProductionCodexModelsAuthSource {
    fn current_credentials(&self) -> anyhow::Result<Option<CodexCredentials>> {
        codex_auth::load_credentials().map_err(Into::into)
    }

    async fn fresh_credentials(&self) -> anyhow::Result<Option<CodexCredentials>> {
        codex_auth::fresh_credentials().await
    }

    async fn force_refresh(&self) -> anyhow::Result<Option<CodexCredentials>> {
        codex_auth::force_refresh().await
    }
}

/// Provider-owned Codex `/models` transport and cache policy.
#[derive(Clone, Debug)]
pub(crate) struct CodexModelsClient {
    http: reqwest::Client,
    cache_path: PathBuf,
    base_url: String,
    open_grok_version: String,
    client_version: String,
    cache_ttl: Duration,
    auth: Arc<dyn CodexModelsAuthSource>,
}

impl CodexModelsClient {
    pub(crate) fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            cache_path: crate::util::grok_home::grok_home().join(CODEX_MODELS_CACHE_FILE),
            base_url: codex_auth::inference_base_url(),
            open_grok_version: xai_grok_version::VERSION.to_owned(),
            client_version: codex_client_version(),
            cache_ttl: CODEX_MODELS_CACHE_TTL,
            auth: Arc::new(ProductionCodexModelsAuthSource),
        }
    }

    /// Load a fresh cache only when it belongs to the current Codex account,
    /// compiled client version, and configured backend origin.
    pub(crate) fn load_fresh_cache(&self) -> Option<CodexModelsCatalog> {
        let credentials = match self.auth.current_credentials() {
            Ok(Some(credentials)) => credentials,
            Ok(None) => return None,
            Err(error) => {
                tracing::warn!(%error, "Codex models cache credentials could not be read");
                return None;
            }
        };
        self.load_fresh_cache_for(&credentials)
    }

    /// Fetch a fresh catalog and persist it. No Codex login is a normal empty
    /// result; transport/protocol failures are returned and leave any old cache
    /// untouched.
    pub(crate) async fn fetch_and_cache(&self) -> anyhow::Result<Option<CodexModelsCatalog>> {
        let Some(mut credentials) = self.auth.fresh_credentials().await? else {
            return Ok(None);
        };

        let fetched = match self.fetch_once(&credentials).await {
            Ok(fetched) => fetched,
            Err(CodexModelsRequestError::Unauthorized) => {
                credentials = self
                    .auth
                    .force_refresh()
                    .await?
                    .ok_or_else(|| anyhow!("OpenAI Codex login is no longer available"))?;
                self.fetch_once(&credentials)
                    .await
                    .map_err(CodexModelsRequestError::into_anyhow)?
            }
            Err(error) => return Err(error.into_anyhow()),
        };

        // Cache persistence is best-effort: a read-only or full home directory
        // must not make a successfully fetched catalog unavailable to the user.
        // Recheck the principal first so a request completed after an account
        // switch cannot overwrite the new account's cache with the old result.
        if !self.catalog_matches_current_account(&fetched) {
            tracing::debug!("skipping Codex models cache write: account changed during request");
        } else if let Err(error) = self.persist(&fetched, &credentials, Utc::now()) {
            tracing::warn!(%error, path = %self.cache_path.display(), "Codex models cache write failed");
        }
        Ok(Some(fetched))
    }

    /// Codex-rs `OnlineIfUncached`: use a fresh matching cache, otherwise fetch.
    pub(crate) async fn load_fresh_or_fetch(&self) -> anyhow::Result<Option<CodexModelsCatalog>> {
        if let Some(cached) = self.load_fresh_cache() {
            return Ok(Some(cached));
        }
        self.fetch_and_cache().await
    }

    pub(crate) fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    /// Verify that a fetched or cached catalog still belongs to the currently
    /// selected ChatGPT principal. This intentionally compares only a digest of
    /// stable identity claims, never bearer or refresh-token material.
    pub(crate) fn catalog_matches_current_account(&self, catalog: &CodexModelsCatalog) -> bool {
        let credentials = match self.auth.current_credentials() {
            Ok(Some(credentials)) => credentials,
            Ok(None) => return false,
            Err(error) => {
                tracing::warn!(%error, "Codex catalog publish credentials could not be read");
                return false;
            }
        };
        account_fingerprint(&credentials)
            .as_deref()
            .is_some_and(|current| current == catalog.account_fingerprint())
    }

    /// Remove only the provider-scoped Codex catalog cache. Used after Codex
    /// logout so the prior ChatGPT account's picker metadata cannot survive;
    /// xAI's independent `models_cache.json` is never touched.
    pub(crate) fn invalidate_cache(&self) {
        match std::fs::remove_file(&self.cache_path) {
            Ok(()) => tracing::debug!(
                path = %self.cache_path.display(),
                "Codex models cache invalidated"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                %error,
                path = %self.cache_path.display(),
                "Codex models cache could not be invalidated"
            ),
        }
    }

    fn load_fresh_cache_for(&self, credentials: &CodexCredentials) -> Option<CodexModelsCatalog> {
        let data = std::fs::read(&self.cache_path).ok()?;
        let cache: CodexModelsCache = serde_json::from_slice(&data).ok()?;
        let (base_origin, base_url) = self.cache_endpoint_identity().ok()?;

        if cache.open_grok_version != self.open_grok_version {
            tracing::debug!(
                cached = %cache.open_grok_version,
                expected = %self.open_grok_version,
                "Codex models cache Open Grok build mismatch"
            );
            return None;
        }
        if cache.client_version != self.client_version {
            tracing::debug!(
                cached = %cache.client_version,
                expected = %self.client_version,
                "Codex models cache version mismatch"
            );
            return None;
        }
        if cache.base_origin != base_origin || cache.base_url != base_url {
            tracing::debug!(
                cached_origin = %cache.base_origin,
                expected_origin = %base_origin,
                "Codex models cache endpoint mismatch"
            );
            return None;
        }
        let expected_account = account_fingerprint(credentials)?;
        if cache.account_fingerprint != expected_account {
            tracing::debug!("Codex models cache account mismatch");
            return None;
        }
        if !cache.is_fresh(self.cache_ttl) {
            tracing::debug!("Codex models cache is stale");
            return None;
        }

        Some(cache.into_catalog())
    }

    async fn fetch_once(
        &self,
        credentials: &CodexCredentials,
    ) -> Result<CodexModelsCatalog, CodexModelsRequestError> {
        let request_account = account_fingerprint(credentials).ok_or_else(|| {
            CodexModelsRequestError::Other(anyhow!(
                "Codex credentials have no stable account identity"
            ))
        })?;
        let url = self.models_url().map_err(CodexModelsRequestError::Other)?;
        let mut request = self
            .http
            .get(url)
            .timeout(CODEX_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(&credentials.access_token)
            // codex-rs's default HTTP client applies both of these headers to
            // the `/models` request before provider auth is layered on top.
            .header("originator", codex_auth::CODEX_ORIGINATOR)
            .header(USER_AGENT, self.codex_user_agent())
            // Matches codex-rs's first-party OpenAI provider header. This must
            // advertise the compatible Codex client, not Open Grok's package.
            .header("version", &self.client_version);
        if let Some(account_id) = credentials.account_id.as_deref() {
            request = request.header("ChatGPT-Account-ID", account_id);
        }
        if credentials.account_is_fedramp {
            request = request.header("X-OpenAI-Fedramp", "true");
        }

        let response = request.send().await.map_err(|error| {
            CodexModelsRequestError::Other(anyhow!(error).context("Codex models request failed"))
        })?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(CodexModelsRequestError::Unauthorized);
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CodexModelsRequestError::Other(anyhow!(
                "Codex models request returned {status}: {}",
                safe_error_excerpt(&body)
            )));
        }

        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await.map_err(|error| {
            CodexModelsRequestError::Other(
                anyhow!(error).context("Codex models response could not be read"),
            )
        })?;
        let wire: CodexModelsResponse = serde_json::from_slice(&body).map_err(|error| {
            CodexModelsRequestError::Other(
                anyhow!(error).context("Codex models response was invalid"),
            )
        })?;

        let mut models: Vec<CodexCatalogModel> = wire
            .models
            .into_iter()
            .filter_map(|model| self.convert_model(model))
            .collect();
        models.sort_by_key(|model| model.priority);

        Ok(CodexModelsCatalog {
            models,
            etag,
            account_fingerprint: request_account,
        })
    }

    fn convert_model(&self, wire: CodexWireModel) -> Option<CodexCatalogModel> {
        let slug = wire.slug.trim();
        if slug.is_empty() {
            tracing::warn!("Codex models response contained an empty slug; skipping entry");
            return None;
        }

        let mut info = ModelInfo::fallback(slug);
        info.id = Some(slug.to_owned());
        info.model = slug.to_owned();
        info.base_url = self.normalized_base_url();
        info.name = Some(
            (!wire.display_name.trim().is_empty())
                .then(|| wire.display_name.trim().to_owned())
                .unwrap_or_else(|| slug.to_owned()),
        );
        info.description = wire
            .description
            .map(|description| description.trim().to_owned())
            .filter(|description| !description.is_empty());
        info.api_backend = ApiBackend::Responses;
        info.provider = ModelProvider::Codex;
        info.tool_mode = match parse_tool_mode(wire.tool_mode.as_deref()) {
            Ok(tool_mode) => tool_mode,
            Err(value) => {
                tracing::warn!(
                    model = slug,
                    tool_mode = value,
                    "skipping Codex model with unknown tool mode"
                );
                return None;
            }
        };
        info.codex_multi_agent_v2 = wire.multi_agent_version.as_deref() == Some("v2");
        info.agent_type = "codex".to_owned();
        info.hidden = !wire.visibility.is_list_visible();
        // This transport is backed exclusively by ChatGPT OAuth credentials.
        // The upstream field describes codex-rs API-key availability, which
        // Open Grok does not implement for this provider, so never let it make
        // a live Codex model visible without a Codex session.
        if wire.supported_in_api {
            tracing::debug!(
                model = slug,
                "Codex model advertises API-key support; keeping OAuth-only locally"
            );
        }
        info.supported_in_api = false;
        info.supports_backend_search = wire.supports_search_tool;
        info.supports_reasoning_summary_parameter = wire.supports_reasoning_summary_parameter;
        info.default_reasoning_summary = wire.default_reasoning_summary;

        let resolved_context_window = wire.context_window.or(wire.max_context_window);
        if let Some(context_window) = effective_context_window(
            resolved_context_window,
            wire.effective_context_window_percent,
        ) {
            info.context_window = context_window;
        }

        let default_effort = wire
            .default_reasoning_level
            .as_deref()
            .and_then(parse_supported_reasoning_effort);
        let mut reasoning_efforts = Vec::new();
        for level in wire.supported_reasoning_levels {
            let Some(value) = parse_supported_reasoning_effort(&level.effort) else {
                tracing::debug!(
                    model = slug,
                    effort = %level.effort,
                    "Codex model advertised an unsupported reasoning effort; skipping option"
                );
                continue;
            };
            if reasoning_efforts
                .iter()
                .any(|option: &ReasoningEffortOption| option.value == value)
            {
                continue;
            }
            reasoning_efforts.push(ReasoningEffortOption {
                id: value.as_str().to_owned(),
                value,
                label: effort_label(value).to_owned(),
                description: (!level.description.trim().is_empty())
                    .then(|| level.description.trim().to_owned()),
                default: Some(value) == default_effort,
            });
        }
        if !reasoning_efforts.is_empty() {
            if !reasoning_efforts.iter().any(|option| option.default) {
                reasoning_efforts[0].default = true;
            }
            info.reasoning_effort = default_effort
                .filter(|effort| {
                    reasoning_efforts
                        .iter()
                        .any(|option| option.value == *effort)
                })
                .or_else(|| reasoning_efforts.first().map(|option| option.value));
            info.supports_reasoning_effort = true;
            info.reasoning_efforts = reasoning_efforts;
        }

        Some(CodexCatalogModel {
            priority: wire.priority,
            visibility: wire.visibility,
            auto_compact_token_limit: wire.auto_compact_token_limit,
            comp_hash: wire.comp_hash,
            resolved_context_window,
            entry: ModelEntry {
                info,
                api_key: None,
                env_key: None,
                auth_provider: None,
                api_base_url: None,
            },
        })
    }

    fn models_url(&self) -> anyhow::Result<Url> {
        let mut url = Url::parse(&self.base_url).context("Codex models base URL is invalid")?;
        let path = format!("{}/models", url.path().trim_end_matches('/'));
        url.set_path(&path);
        url.query_pairs_mut()
            .append_pair("client_version", &self.client_version);
        Ok(url)
    }

    fn cache_endpoint_identity(&self) -> anyhow::Result<(String, String)> {
        let url = Url::parse(&self.base_url).context("Codex models base URL is invalid")?;
        Ok((
            url.origin().ascii_serialization(),
            self.normalized_base_url(),
        ))
    }

    fn normalized_base_url(&self) -> String {
        self.base_url.trim_end_matches('/').to_owned()
    }

    fn codex_user_agent(&self) -> String {
        // Match codex-rs's stable `{originator}/{package-version}` prefix. The
        // compatibility version is the Codex build contract advertised by this
        // client; Open Grok's own package version is deliberately separate.
        format!("{}/{}", codex_auth::CODEX_ORIGINATOR, self.client_version)
    }

    fn persist(
        &self,
        catalog: &CodexModelsCatalog,
        credentials: &CodexCredentials,
        fetched_at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let (base_origin, base_url) = self.cache_endpoint_identity()?;
        let cache = CodexModelsCache {
            fetched_at,
            open_grok_version: self.open_grok_version.clone(),
            client_version: self.client_version.clone(),
            base_origin,
            base_url,
            account_fingerprint: account_fingerprint(credentials)
                .ok_or_else(|| anyhow!("Codex credentials have no stable account identity"))?,
            etag: catalog.etag.clone(),
            models: catalog.models.clone(),
        };
        let data = serde_json::to_vec_pretty(&cache).context("serialize Codex models cache")?;
        let parent = self
            .cache_path
            .parent()
            .ok_or_else(|| anyhow!("Codex models cache path has no parent"))?;
        std::fs::create_dir_all(parent).context("create Codex models cache directory")?;

        // `NamedTempFile::persist` replaces atomically on supported platforms.
        // A failed fetch never reaches this function, so it cannot delete or
        // truncate the last known-good cache.
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .context("create Codex models cache temp file")?;
        temporary
            .write_all(&data)
            .context("write Codex models cache temp file")?;
        temporary
            .as_file_mut()
            .sync_all()
            .context("sync Codex models cache temp file")?;
        temporary
            .persist(&self.cache_path)
            .map_err(|error| error.error)
            .context("replace Codex models cache")?;
        Ok(())
    }

    #[cfg(test)]
    fn for_test(
        cache_path: PathBuf,
        base_url: String,
        client_version: String,
        cache_ttl: Duration,
        auth: Arc<dyn CodexModelsAuthSource>,
    ) -> Self {
        Self {
            http: reqwest::Client::new(),
            cache_path,
            base_url,
            open_grok_version: "test-open-grok".to_owned(),
            client_version,
            cache_ttl,
            auth,
        }
    }
}

impl Default for CodexModelsClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
enum CodexModelsRequestError {
    Unauthorized,
    Other(anyhow::Error),
}

impl CodexModelsRequestError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Unauthorized => anyhow!("OpenAI Codex rejected the OAuth token"),
            Self::Other(error) => error,
        }
    }
}

fn parse_supported_reasoning_effort(value: &str) -> Option<ReasoningEffort> {
    // Keep this allowlist explicit so future server values do not silently
    // become selectable before the local transport knows how to encode them.
    match value {
        "none" => Some(ReasoningEffort::None),
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::Xhigh),
        "max" => Some(ReasoningEffort::Max),
        "ultra" => Some(ReasoningEffort::Ultra),
        _ => None,
    }
}

fn effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "None",
        ReasoningEffort::Minimal => "Minimal",
        ReasoningEffort::Low => "Low",
        ReasoningEffort::Medium => "Medium",
        ReasoningEffort::High => "High",
        ReasoningEffort::Xhigh => "Xhigh",
        ReasoningEffort::Max => "Max",
        ReasoningEffort::Ultra => "Ultra",
    }
}

fn parse_tool_mode(value: Option<&str>) -> Result<Option<ToolMode>, String> {
    match value {
        Some("direct") => Ok(Some(ToolMode::Direct)),
        Some("code_mode") => Ok(Some(ToolMode::CodeMode)),
        Some("code_mode_only") => Ok(Some(ToolMode::CodeModeOnly)),
        Some(value) => Err(value.to_owned()),
        None => Ok(None),
    }
}

fn effective_context_window(raw: Option<i64>, percent: i64) -> Option<std::num::NonZeroU64> {
    let raw = raw?;
    if raw <= 0 || percent <= 0 {
        return None;
    }
    let effective = raw.saturating_mul(percent) / 100;
    u64::try_from(effective)
        .ok()
        .and_then(std::num::NonZeroU64::new)
}

fn account_fingerprint(credentials: &CodexCredentials) -> Option<String> {
    // Never key a cache only by a rotating bearer. If the ID token does not
    // expose any stable principal, fail closed and skip disk caching.
    if credentials.account_id.is_none()
        && credentials.chatgpt_user_id.is_none()
        && credentials.email.is_none()
    {
        return None;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"open-grok-codex-model-cache-account-v1\0");
    hash_identity_component(&mut hasher, credentials.account_id.as_deref());
    hash_identity_component(&mut hasher, credentials.chatgpt_user_id.as_deref());
    hash_identity_component(&mut hasher, credentials.email.as_deref());
    hasher.update(&[u8::from(credentials.is_workspace_account)]);
    Some(hasher.finalize().to_hex().to_string())
}

fn hash_identity_component(hasher: &mut blake3::Hasher, value: Option<&str>) {
    let value = value.unwrap_or_default().as_bytes();
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn safe_error_excerpt(body: &str) -> String {
    const LIMIT: usize = 512;
    let mut excerpt: String = body.chars().take(LIMIT).collect();
    if body.chars().count() > LIMIT {
        excerpt.push('…');
    }
    excerpt.replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, HeaderValue};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    #[derive(Debug)]
    struct TestAuthSource {
        current: Mutex<Option<CodexCredentials>>,
        fresh: Option<CodexCredentials>,
        refreshed: Option<CodexCredentials>,
        force_calls: AtomicUsize,
    }

    #[async_trait]
    impl CodexModelsAuthSource for TestAuthSource {
        fn current_credentials(&self) -> anyhow::Result<Option<CodexCredentials>> {
            Ok(self.current.lock().unwrap().clone())
        }

        async fn fresh_credentials(&self) -> anyhow::Result<Option<CodexCredentials>> {
            Ok(self.fresh.clone())
        }

        async fn force_refresh(&self) -> anyhow::Result<Option<CodexCredentials>> {
            self.force_calls.fetch_add(1, Ordering::SeqCst);
            let refreshed = self.refreshed.clone();
            *self.current.lock().unwrap() = refreshed.clone();
            Ok(refreshed)
        }
    }

    fn credentials(token: &str, account: &str, fedramp: bool) -> CodexCredentials {
        CodexCredentials {
            access_token: token.to_owned(),
            account_id: Some(account.to_owned()),
            chatgpt_user_id: Some("user-1".to_owned()),
            email: Some("person@example.com".to_owned()),
            plan_type: Some("pro".to_owned()),
            is_workspace_account: true,
            account_is_fedramp: fedramp,
        }
    }

    fn auth_source(credentials: CodexCredentials) -> Arc<TestAuthSource> {
        Arc::new(TestAuthSource {
            current: Mutex::new(Some(credentials.clone())),
            fresh: Some(credentials.clone()),
            refreshed: Some(credentials),
            force_calls: AtomicUsize::new(0),
        })
    }

    fn model_response() -> serde_json::Value {
        json!({
            "models": [
                {
                    "slug": "gpt-5.6-sol",
                    "display_name": "GPT-5.6 Sol Live",
                    "description": "Live catalog description",
                    "default_reasoning_level": "medium",
                    "supports_reasoning_summary_parameter": true,
                    "default_reasoning_summary": "detailed",
                    "supported_reasoning_levels": [
                        {"effort": "low", "description": "Fast"},
                        {"effort": "medium", "description": "Balanced"},
                        {"effort": "xhigh", "description": "Deep"},
                        {"effort": "max", "description": "Maximum"},
                        {"effort": "ultra", "description": "Automatic delegation"}
                    ],
                    "visibility": "list",
                    "supported_in_api": true,
                    "priority": 1,
                    "context_window": 372000,
                    "max_context_window": 400000,
                    "auto_compact_token_limit": 300123,
                    "comp_hash": "comp-v3",
                    "effective_context_window_percent": 95,
                    "supports_search_tool": true,
                    "tool_mode": "code_mode_only",
                    "multi_agent_version": "v2"
                },
                {
                    "slug": "hidden-model",
                    "display_name": "Hidden",
                    "visibility": "hide",
                    "supported_in_api": false,
                    "priority": 2,
                    "context_window": 200000
                },
                {
                    "slug": "unknown-tool-mode",
                    "display_name": "Unknown Tool Mode",
                    "visibility": "list",
                    "supported_in_api": false,
                    "priority": 3,
                    "context_window": 200000,
                    "tool_mode": "automatic"
                }
            ]
        })
    }

    #[derive(Clone, Debug)]
    struct ObservedRequest {
        query: HashMap<String, String>,
        authorization: Option<String>,
        account_id: Option<String>,
        fedramp: Option<String>,
        version: Option<String>,
        originator: Option<String>,
        user_agent: Option<String>,
        xai_token_auth: Option<String>,
        x_api_key: Option<String>,
    }

    #[derive(Clone)]
    struct ServerState {
        observed: Arc<Mutex<Vec<ObservedRequest>>>,
        statuses: Arc<Mutex<VecDeque<StatusCode>>>,
        body: serde_json::Value,
        etag: Option<&'static str>,
        request_started: Option<Arc<Notify>>,
        response_release: Option<Arc<Notify>>,
    }

    async fn models_handler(
        State(state): State<ServerState>,
        Query(query): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> Response {
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        state.observed.lock().unwrap().push(ObservedRequest {
            query,
            authorization: header("authorization"),
            account_id: header("chatgpt-account-id"),
            fedramp: header("x-openai-fedramp"),
            version: header("version"),
            originator: header("originator"),
            user_agent: header("user-agent"),
            xai_token_auth: header("x-xai-token-auth"),
            x_api_key: header("x-api-key"),
        });
        if let Some(request_started) = &state.request_started {
            request_started.notify_one();
        }
        if let Some(response_release) = &state.response_release {
            response_release.notified().await;
        }
        let status = state
            .statuses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(StatusCode::OK);
        let mut response = (status, axum::Json(state.body.clone())).into_response();
        if status.is_success()
            && let Some(etag) = state.etag
        {
            response
                .headers_mut()
                .insert(ETAG, HeaderValue::from_static(etag));
        }
        response
    }

    async fn spawn_models_server(
        statuses: impl IntoIterator<Item = StatusCode>,
        body: serde_json::Value,
        etag: Option<&'static str>,
    ) -> (
        String,
        Arc<Mutex<Vec<ObservedRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_models_server_with_gate(statuses, body, etag, None).await
    }

    async fn spawn_models_server_with_gate(
        statuses: impl IntoIterator<Item = StatusCode>,
        body: serde_json::Value,
        etag: Option<&'static str>,
        gate: Option<(Arc<Notify>, Arc<Notify>)>,
    ) -> (
        String,
        Arc<Mutex<Vec<ObservedRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (request_started, response_release) = gate
            .map(|(request_started, response_release)| {
                (Some(request_started), Some(response_release))
            })
            .unwrap_or_default();
        let state = ServerState {
            observed: observed.clone(),
            statuses: Arc::new(Mutex::new(statuses.into_iter().collect())),
            body,
            etag,
            request_started,
            response_release,
        };
        let app = Router::new()
            .route("/codex/models", get(models_handler))
            .with_state(state);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}/codex"), observed, server)
    }

    fn test_client(
        temp: &tempfile::TempDir,
        base_url: String,
        version: &str,
        ttl: Duration,
        auth: Arc<dyn CodexModelsAuthSource>,
    ) -> CodexModelsClient {
        CodexModelsClient::for_test(
            temp.path().join(CODEX_MODELS_CACHE_FILE),
            base_url,
            version.to_owned(),
            ttl,
            auth,
        )
    }

    #[tokio::test]
    async fn fetch_uses_codex_url_auth_headers_and_converts_live_metadata() {
        let (base_url, observed, server) =
            spawn_models_server([StatusCode::OK], model_response(), Some("live-etag")).await;
        let temp = tempfile::tempdir().unwrap();
        let auth = auth_source(credentials("codex-token", "workspace-1", true));
        let client = test_client(&temp, base_url, "9.8.7", Duration::from_secs(300), auth);

        let catalog = client.fetch_and_cache().await.unwrap().unwrap();
        server.abort();

        let requests = observed.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].query.get("client_version").map(String::as_str),
            Some("9.8.7")
        );
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer codex-token")
        );
        assert_eq!(requests[0].account_id.as_deref(), Some("workspace-1"));
        assert_eq!(requests[0].fedramp.as_deref(), Some("true"));
        assert_eq!(requests[0].version.as_deref(), Some("9.8.7"));
        assert_eq!(
            requests[0].originator.as_deref(),
            Some(codex_auth::CODEX_ORIGINATOR)
        );
        assert_eq!(
            requests[0].user_agent.as_deref(),
            Some("codex_cli_rs/9.8.7")
        );
        assert_eq!(requests[0].xai_token_auth, None);
        assert_eq!(requests[0].x_api_key, None);

        assert_eq!(catalog.etag.as_deref(), Some("live-etag"));
        assert!(catalog.is_authoritative());
        assert_eq!(catalog.models.len(), 2);
        assert!(
            catalog
                .models
                .iter()
                .all(|model| model.slug() != "unknown-tool-mode"),
            "unknown explicit tool modes must fail closed",
        );
        let live = &catalog.models[0];
        assert_eq!(live.slug(), "gpt-5.6-sol");
        assert_eq!(live.entry.info.name.as_deref(), Some("GPT-5.6 Sol Live"));
        assert_eq!(live.entry.info.context_window.get(), 353_400);
        assert_eq!(live.auto_compact_token_limit, Some(300_123));
        assert_eq!(live.resolved_auto_compact_token_limit(), Some(300_123));
        assert_eq!(live.comp_hash.as_deref(), Some("comp-v3"));
        assert_eq!(live.entry.info.provider, ModelProvider::Codex);
        assert_eq!(live.entry.info.api_backend, ApiBackend::Responses);
        assert_eq!(live.entry.info.tool_mode, Some(ToolMode::CodeModeOnly));
        assert!(live.entry.info.codex_multi_agent_v2);
        assert!(live.entry.info.supports_backend_search);
        assert!(live.entry.info.supports_reasoning_summary_parameter);
        assert_eq!(
            live.entry.info.default_reasoning_summary,
            ReasoningSummary::Detailed
        );
        assert_eq!(live.entry.info.agent_type, "codex");
        assert!(
            !live.entry.info.supported_in_api,
            "live Codex models remain OAuth-only even when upstream advertises API support"
        );
        assert_eq!(
            live.entry
                .info
                .reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::Xhigh,
                ReasoningEffort::Max,
                ReasoningEffort::Ultra,
            ]
        );
        assert_eq!(
            live.entry.info.reasoning_efforts[3].description.as_deref(),
            Some("Maximum")
        );
        assert_eq!(
            live.entry.info.reasoning_efforts[4].description.as_deref(),
            Some("Automatic delegation")
        );
        assert_eq!(
            live.entry.info.reasoning_effort,
            Some(ReasoningEffort::Medium)
        );
        assert!(!catalog.models[1].visibility.is_list_visible());
        assert!(catalog.models[1].entry.info.hidden);
    }

    #[tokio::test]
    async fn unauthorized_forces_one_refresh_and_retries_with_rotated_bearer() {
        let (base_url, observed, server) = spawn_models_server(
            [StatusCode::UNAUTHORIZED, StatusCode::OK],
            model_response(),
            None,
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let old = credentials("old-token", "workspace-1", false);
        let new = credentials("new-token", "workspace-1", false);
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(old.clone())),
            fresh: Some(old),
            refreshed: Some(new),
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(
            &temp,
            base_url,
            "1.2.3",
            Duration::from_secs(300),
            auth.clone(),
        );

        assert!(client.fetch_and_cache().await.unwrap().is_some());
        server.abort();

        assert_eq!(auth.force_calls.load(Ordering::SeqCst), 1);
        let requests = observed.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer old-token")
        );
        assert_eq!(
            requests[1].authorization.as_deref(),
            Some("Bearer new-token")
        );
    }

    #[tokio::test]
    async fn account_switch_discards_in_flight_catalog_at_publish_boundary() {
        let request_started = Arc::new(Notify::new());
        let response_release = Arc::new(Notify::new());
        let (base_url, _observed, server) = spawn_models_server_with_gate(
            [StatusCode::OK],
            model_response(),
            None,
            Some((request_started.clone(), response_release.clone())),
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let account_a = credentials("token-a", "workspace-a", false);
        let account_b = credentials("token-b", "workspace-b", false);
        let auth = Arc::new(TestAuthSource {
            current: Mutex::new(Some(account_a.clone())),
            fresh: Some(account_a.clone()),
            refreshed: Some(account_a),
            force_calls: AtomicUsize::new(0),
        });
        let client = test_client(
            &temp,
            base_url,
            "1.2.3",
            Duration::from_secs(300),
            auth.clone(),
        );

        let fetching = {
            let client = client.clone();
            tokio::spawn(async move { client.fetch_and_cache().await })
        };
        request_started.notified().await;
        *auth.current.lock().unwrap() = Some(account_b);
        response_release.notify_one();
        let catalog = fetching.await.unwrap().unwrap().unwrap();
        server.abort();

        assert!(
            !client.catalog_matches_current_account(&catalog),
            "account A's result must not pass account B's publish-time check"
        );
        assert!(
            !client.cache_path().exists(),
            "a stale-account result must not replace the current account's cache"
        );
    }

    #[tokio::test]
    async fn fresh_matching_cache_avoids_network_and_retains_etag() {
        let (base_url, observed, server) =
            spawn_models_server([StatusCode::OK], model_response(), Some("cached-etag")).await;
        let temp = tempfile::tempdir().unwrap();
        let creds = credentials("token", "workspace-1", false);
        let client = test_client(
            &temp,
            base_url,
            "1.2.3",
            Duration::from_secs(300),
            auth_source(creds),
        );

        client.fetch_and_cache().await.unwrap().unwrap();
        let cached = client.load_fresh_or_fetch().await.unwrap().unwrap();
        server.abort();

        assert_eq!(observed.lock().unwrap().len(), 1);
        assert_eq!(cached.etag.as_deref(), Some("cached-etag"));
        assert_eq!(cached.models[0].entry.info.context_window.get(), 353_400);
        assert!(
            cached.models[0]
                .entry
                .info
                .supports_reasoning_summary_parameter
        );
        assert_eq!(
            cached.models[0].entry.info.default_reasoning_summary,
            ReasoningSummary::Detailed,
        );
    }

    #[tokio::test]
    async fn cache_misses_for_staleness_version_endpoint_and_account() {
        let (base_url, _observed, server) =
            spawn_models_server([StatusCode::OK], model_response(), None).await;
        let temp = tempfile::tempdir().unwrap();
        let creds = credentials("token", "workspace-1", false);
        let client = test_client(
            &temp,
            base_url.clone(),
            "1.2.3",
            Duration::from_secs(300),
            auth_source(creds.clone()),
        );
        let catalog = client.fetch_and_cache().await.unwrap().unwrap();
        server.abort();
        assert!(client.load_fresh_cache().is_some());

        let version_mismatch = test_client(
            &temp,
            base_url.clone(),
            "1.2.4",
            Duration::from_secs(300),
            auth_source(creds.clone()),
        );
        assert!(version_mismatch.load_fresh_cache().is_none());

        let endpoint_mismatch = test_client(
            &temp,
            format!("{base_url}/different"),
            "1.2.3",
            Duration::from_secs(300),
            auth_source(creds.clone()),
        );
        assert!(endpoint_mismatch.load_fresh_cache().is_none());

        let other_account = test_client(
            &temp,
            base_url,
            "1.2.3",
            Duration::from_secs(300),
            auth_source(credentials("other-token", "workspace-2", false)),
        );
        assert!(other_account.load_fresh_cache().is_none());

        client
            .persist(&catalog, &creds, Utc::now() - ChronoDuration::hours(1))
            .unwrap();
        assert!(client.load_fresh_cache().is_none());
    }

    #[tokio::test]
    async fn failed_fetch_does_not_replace_last_good_cache() {
        let (base_url, _observed, server) =
            spawn_models_server([StatusCode::OK], model_response(), Some("good")).await;
        let temp = tempfile::tempdir().unwrap();
        let creds = credentials("token", "workspace-1", false);
        let client = test_client(
            &temp,
            base_url,
            "1.2.3",
            Duration::from_secs(300),
            auth_source(creds.clone()),
        );
        client.fetch_and_cache().await.unwrap().unwrap();
        server.abort();
        let before = std::fs::read(client.cache_path()).unwrap();

        let (failing_base, _observed, failing_server) = spawn_models_server(
            [StatusCode::INTERNAL_SERVER_ERROR],
            json!({"error": "boom"}),
            None,
        )
        .await;
        let failing = test_client(
            &temp,
            failing_base,
            "1.2.3",
            Duration::from_secs(300),
            auth_source(creds),
        );
        assert!(failing.fetch_and_cache().await.is_err());
        failing_server.abort();

        assert_eq!(std::fs::read(client.cache_path()).unwrap(), before);
    }

    #[tokio::test]
    async fn invalidation_removes_only_the_codex_models_cache() {
        let (base_url, _observed, server) =
            spawn_models_server([StatusCode::OK], model_response(), None).await;
        let temp = tempfile::tempdir().unwrap();
        let xai_cache = temp.path().join("models_cache.json");
        std::fs::write(&xai_cache, b"xai-cache").unwrap();
        let client = test_client(
            &temp,
            base_url,
            "1.2.3",
            Duration::from_secs(300),
            auth_source(credentials("token", "workspace-1", false)),
        );
        client.fetch_and_cache().await.unwrap().unwrap();
        server.abort();
        assert!(client.cache_path().exists());

        client.invalidate_cache();
        client.invalidate_cache(); // NotFound is intentionally a no-op.

        assert!(!client.cache_path().exists());
        assert_eq!(std::fs::read(xai_cache).unwrap(), b"xai-cache");
    }

    #[test]
    fn hidden_only_catalog_is_not_authoritative_but_keeps_hidden_entry() {
        let temp = tempfile::tempdir().unwrap();
        let client = test_client(
            &temp,
            "https://chatgpt.example/codex".to_owned(),
            "1.2.3",
            Duration::from_secs(300),
            auth_source(credentials("token", "workspace-1", false)),
        );
        let wire: CodexModelsResponse = serde_json::from_value(json!({
            "models": [{
                "slug": "hidden",
                "display_name": "Hidden",
                "visibility": "hide",
                "supported_in_api": false,
                "priority": 1,
                "context_window": 100000
            }]
        }))
        .unwrap();
        let catalog = CodexModelsCatalog {
            models: wire
                .models
                .into_iter()
                .filter_map(|model| client.convert_model(model))
                .collect(),
            etag: None,
            account_fingerprint: account_fingerprint(&credentials("token", "workspace-1", false))
                .unwrap(),
        };

        assert!(!catalog.is_authoritative());
        assert!(catalog.list_visible_entries().is_empty());
        assert_eq!(catalog.entries().len(), 1);
        assert!(catalog.entries()["hidden"].info.hidden);
    }

    #[test]
    fn multi_agent_version_is_independent_from_ultra_effort() {
        let temp = tempfile::tempdir().unwrap();
        let client = test_client(
            &temp,
            "https://chatgpt.example/codex".to_owned(),
            "1.2.3",
            Duration::from_secs(300),
            auth_source(credentials("token", "workspace-1", false)),
        );
        let convert = |value| {
            let wire: CodexWireModel = serde_json::from_value(value).unwrap();
            client.convert_model(wire).unwrap().entry
        };

        let v2_without_ultra = convert(json!({
            "slug": "future-v2",
            "display_name": "Future v2",
            "visibility": "list",
            "context_window": 100000,
            "multi_agent_version": "v2",
            "supported_reasoning_levels": [{"effort": "high"}]
        }));
        assert!(v2_without_ultra.info.codex_multi_agent_v2);
        assert!(
            v2_without_ultra
                .info
                .reasoning_efforts
                .iter()
                .all(|option| option.value != ReasoningEffort::Ultra)
        );

        let v1_with_ultra = convert(json!({
            "slug": "future-v1",
            "display_name": "Future v1",
            "visibility": "list",
            "context_window": 100000,
            "multi_agent_version": "v1",
            "supported_reasoning_levels": [{"effort": "ultra"}]
        }));
        assert!(!v1_with_ultra.info.codex_multi_agent_v2);
        assert_eq!(
            v1_with_ultra.info.reasoning_efforts[0].value,
            ReasoningEffort::Ultra
        );
    }

    #[test]
    fn reasoning_summary_metadata_defaults_and_explicit_modes_match_codex() {
        let temp = tempfile::tempdir().unwrap();
        let client = test_client(
            &temp,
            "https://chatgpt.example/codex".to_owned(),
            "1.2.3",
            Duration::from_secs(300),
            auth_source(credentials("token", "workspace-1", false)),
        );

        let convert = |supports: Option<bool>, summary: Option<ReasoningSummary>| {
            let mut value = json!({
                "slug": "summary-model",
                "display_name": "Summary Model",
                "visibility": "list",
                "context_window": 100000
            });
            if let Some(supports) = supports {
                value["supports_reasoning_summary_parameter"] = json!(supports);
            }
            if let Some(summary) = summary {
                value["default_reasoning_summary"] = serde_json::to_value(summary).unwrap();
            }
            let wire: CodexWireModel = serde_json::from_value(value).unwrap();
            client.convert_model(wire).unwrap().entry.info
        };

        let defaults = convert(None, None);
        assert!(defaults.supports_reasoning_summary_parameter);
        assert_eq!(
            defaults.default_reasoning_summary,
            ReasoningSummary::Detailed
        );

        for summary in [
            ReasoningSummary::Auto,
            ReasoningSummary::Concise,
            ReasoningSummary::Detailed,
        ] {
            let info = convert(Some(true), Some(summary));
            assert_eq!(info.default_reasoning_summary, summary);
            assert_eq!(
                crate::agent::config::model_reasoning_summary(&info),
                Some(summary),
            );
        }

        // Catalog `none` must still request a summary for the interactive TUI;
        // otherwise encrypted-only reasoning leaves thinking blocks empty.
        let catalog_none = convert(Some(true), Some(ReasoningSummary::None));
        assert_eq!(
            catalog_none.default_reasoning_summary,
            ReasoningSummary::None
        );
        assert_eq!(
            crate::agent::config::model_reasoning_summary(&catalog_none),
            Some(ReasoningSummary::Auto),
        );

        let unsupported = convert(Some(false), Some(ReasoningSummary::Detailed));
        assert_eq!(
            crate::agent::config::model_reasoning_summary(&unsupported),
            None
        );
    }

    #[test]
    fn auto_compact_limit_matches_upstream_raw_context_clamp() {
        let temp = tempfile::tempdir().unwrap();
        let client = test_client(
            &temp,
            "https://chatgpt.example/codex".to_owned(),
            "1.2.3",
            Duration::from_secs(300),
            auth_source(credentials("token", "workspace-1", false)),
        );
        let convert = |limit: Option<i64>| {
            let wire: CodexWireModel = serde_json::from_value(json!({
                "slug": "clamped",
                "display_name": "Clamped",
                "visibility": "list",
                "context_window": 100000,
                "auto_compact_token_limit": limit,
            }))
            .unwrap();
            client.convert_model(wire).unwrap()
        };

        assert_eq!(
            convert(None).resolved_auto_compact_token_limit(),
            Some(90_000)
        );
        assert_eq!(
            convert(Some(70_123)).resolved_auto_compact_token_limit(),
            Some(70_123)
        );
        assert_eq!(
            convert(Some(95_000)).resolved_auto_compact_token_limit(),
            Some(90_000)
        );
    }

    #[test]
    fn codex_compatibility_version_normalizes_to_whole_semver() {
        assert_eq!(
            normalize_whole_semver("0.144.5-alpha.1+build"),
            Some("0.144.5".to_owned())
        );
        assert_eq!(
            normalize_whole_semver(" v0.145.0 "),
            Some("0.145.0".to_owned())
        );
        assert_eq!(normalize_whole_semver("not-a-version"), None);
        assert_eq!(DEFAULT_CODEX_CLIENT_VERSION, "0.144.5");
    }
}
