//! Provider-isolated RunInfra model discovery.
//!
//! RunInfra exposes an OpenAI-compatible Chat Completions API at
//! `https://api.runinfra.ai/v1`. The `/models` response is authoritative when
//! available and includes served `context_window` / `max_output_tokens`. When
//! `/models` is unreachable or returns nothing, a curated static fallback
//! list keeps the model picker populated with the published hosted lineup.

use crate::agent::config::{EnvKeys, ModelEntry, ModelInfo};
use anyhow::{Context, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::num::NonZeroU64;
use std::time::Duration;
use url::Url;
use xai_grok_sampling_types::{
    ApiBackend, ModelProvider, ReasoningEffort, ReasoningEffortOption, ToolMode,
};

/// Default hosted Model APIs base URL.
pub const RUNINFRA_API_BASE_URL: &str = "https://api.runinfra.ai/v1";
pub const RUNINFRA_API_BASE_URL_ENV: &str = "OPENGROK_RUNINFRA_API_BASE_URL";
/// Official RunInfra environment variable used by their docs and SDKs.
pub const RUNINFRA_GATEWAY_KEY_ENV: &str = "RUNINFRA_GATEWAY_KEY";
/// Alias accepted for consistency with other Open Grok providers.
pub const RUNINFRA_API_KEY_ENV: &str = "RUNINFRA_API_KEY";
const RUNINFRA_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CONTEXT_WINDOW: u64 = 262_144;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 32_768;

/// Published hosted Model APIs lineup. Live `/models` can also include the
/// caller's own verified deployments; those appear only in the dynamic
/// catalog.
const FALLBACK_MODEL_IDS: &[&str] = &[
    "deepseek-v4-flash",
    "nemotron-3-5-lightning-30b",
    "qwen3-8-2-4t-a95b",
    "qwen3-8-27b",
];

/// Hosted models that reason by default. Unknown live ids (workspace
/// deployments) stay fail-closed until we know they honor `reasoning_effort`.
const KNOWN_REASONING_MODEL_IDS: &[&str] = FALLBACK_MODEL_IDS;

/// The one listed hosted model that refuses `reasoning_effort: "none"`.
const ALWAYS_REASONING_MODEL_IDS: &[&str] = &["qwen3-8-2-4t-a95b"];

pub fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https" && url.host_str() == Some("api.runinfra.ai")
}

pub fn api_base_url() -> String {
    std::env::var(RUNINFRA_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| RUNINFRA_API_BASE_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    for name in [RUNINFRA_GATEWAY_KEY_ENV, RUNINFRA_API_KEY_ENV] {
        if let Some(value) = std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
        {
            return Some(value);
        }
    }
    None
}

pub fn environment_api_key_is_configured() -> bool {
    environment_api_key().is_some()
}

pub fn env_keys() -> EnvKeys {
    EnvKeys::new([RUNINFRA_GATEWAY_KEY_ENV, RUNINFRA_API_KEY_ENV])
}

fn stored_api_key() -> Option<String> {
    crate::auth::read_provider_api_key(
        &crate::util::grok_home::grok_home(),
        ModelProvider::Runinfra,
    )
}

fn select_api_key(
    base_url: &str,
    environment_key: Option<String>,
    stored_key: Option<String>,
) -> Option<String> {
    environment_key.or_else(|| {
        is_trusted_api_base_url(base_url)
            .then_some(stored_key)
            .flatten()
    })
}

fn api_key_for_base_url(base_url: &str) -> Option<String> {
    select_api_key(base_url, environment_api_key(), stored_api_key())
}

fn credential_fingerprint(api_key: &str) -> String {
    blake3::hash(api_key.as_bytes()).to_hex().to_string()
}

fn is_known_reasoning_model(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    KNOWN_REASONING_MODEL_IDS.iter().any(|id| lower == *id)
}

fn disables_reasoning_unsupported(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    ALWAYS_REASONING_MODEL_IDS.iter().any(|id| lower == *id)
}

fn curated_limits(model_id: &str) -> (u64, u32) {
    let lower = model_id.to_ascii_lowercase();
    if lower == "deepseek-v4-flash" {
        return (1_048_576, DEFAULT_MAX_OUTPUT_TOKENS);
    }
    (DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_OUTPUT_TOKENS)
}

fn display_name(model_id: &str) -> String {
    match model_id {
        "deepseek-v4-flash" => "DeepSeek V4 Flash".to_owned(),
        "nemotron-3-5-lightning-30b" => "Nemotron 3.5 Lightning 30B".to_owned(),
        "qwen3-8-2-4t-a95b" => "Qwen3.8 2.4T A95B".to_owned(),
        "qwen3-8-27b" => "Qwen3.8 27B".to_owned(),
        other => other.to_owned(),
    }
}

fn assigned_context_window(model_id: &str, wire_context: Option<u64>) -> NonZeroU64 {
    let (curated, _) = curated_limits(model_id);
    NonZeroU64::new(wire_context.filter(|&value| value > 0).unwrap_or(curated)).unwrap_or_else(
        || NonZeroU64::new(DEFAULT_CONTEXT_WINDOW).expect("non-zero RunInfra fallback"),
    )
}

fn assigned_max_output(model_id: &str, wire_max_output: Option<u32>) -> u32 {
    let (_, curated) = curated_limits(model_id);
    wire_max_output
        .filter(|&value| value > 0)
        .unwrap_or(curated)
}

fn log_catalog_refreshed(
    entries: &IndexMap<String, ModelEntry>,
    authoritative: bool,
    wire_context_present: bool,
) {
    let models: Vec<serde_json::Value> = entries
        .values()
        .map(|entry| {
            serde_json::json!({
                "id": entry.info.model,
                "context_window": entry.info.context_window.get(),
                "max_completion_tokens": entry.info.max_completion_tokens,
            })
        })
        .collect();
    crate::unified_log::info(
        "RunInfra model catalog refreshed",
        None,
        Some(serde_json::json!({
            "count": entries.len(),
            "authoritative": authoritative,
            "wire_context_present": wire_context_present,
            "models": models,
        })),
    );
}

fn effort_option(
    value: ReasoningEffort,
    description: &str,
    default: bool,
) -> ReasoningEffortOption {
    ReasoningEffortOption {
        id: value.as_str().to_owned(),
        value,
        label: match value {
            ReasoningEffort::None => "None",
            ReasoningEffort::Low => "Low",
            ReasoningEffort::Medium => "Medium",
            ReasoningEffort::High => "High",
            ReasoningEffort::Max => "Max",
            _ => unreachable!("RunInfra exposes only none/low/medium/high/max"),
        }
        .to_owned(),
        description: Some(description.to_owned()),
        default,
    }
}

/// Effort menu for hosted RunInfra reasoning models.
///
/// Every public hosted model reasons by default. `none` turns thinking off
/// except on `qwen3-8-2-4t-a95b`, which refuses that value. Intermediate
/// values are accepted but do not change measured reasoning length; the
/// gateway default for `deepseek-v4-flash` is `max`.
fn runinfra_reasoning_efforts(model_id: &str) -> Vec<ReasoningEffortOption> {
    let default = if model_id.eq_ignore_ascii_case("deepseek-v4-flash") {
        ReasoningEffort::Max
    } else {
        ReasoningEffort::High
    };
    let mut options = Vec::new();
    if !disables_reasoning_unsupported(model_id) {
        options.push(effort_option(
            ReasoningEffort::None,
            "Answer only; skip the billed reasoning stream",
            false,
        ));
    }
    options.extend([
        effort_option(
            ReasoningEffort::Low,
            "Accepted by the gateway; no measured change vs default reasoning",
            false,
        ),
        effort_option(
            ReasoningEffort::Medium,
            "Accepted by the gateway; no measured change vs default reasoning",
            false,
        ),
        effort_option(
            ReasoningEffort::High,
            "Default reasoning for models that do not inject max",
            default == ReasoningEffort::High,
        ),
        effort_option(
            ReasoningEffort::Max,
            "DeepSeek V4 Flash gateway default; maximum thinking depth",
            default == ReasoningEffort::Max,
        ),
    ]);
    options
}

fn model_entry(model_id: &str, base_url: &str) -> ModelEntry {
    model_entry_with_limits(model_id, base_url, None, None)
}

fn model_entry_with_limits(
    model_id: &str,
    base_url: &str,
    wire_context: Option<u64>,
    wire_max_output: Option<u32>,
) -> ModelEntry {
    let key = format!("runinfra:{model_id}");
    let mut info = ModelInfo::fallback(&key);
    info.id = Some(key);
    info.model = model_id.to_owned();
    info.base_url = base_url.trim_end_matches('/').to_owned();
    info.name = Some(display_name(model_id));
    info.api_backend = ApiBackend::ChatCompletions;
    info.provider = ModelProvider::Runinfra;
    info.tool_mode = Some(ToolMode::Direct);
    info.context_window = assigned_context_window(model_id, wire_context);
    info.max_completion_tokens = Some(assigned_max_output(model_id, wire_max_output));
    if is_known_reasoning_model(model_id) {
        let efforts = runinfra_reasoning_efforts(model_id);
        info.reasoning_effort = efforts
            .iter()
            .find(|option| option.default)
            .map(|option| option.value);
        info.reasoning_efforts = efforts;
        info.supports_reasoning_effort = true;
    } else {
        info.supports_reasoning_effort = false;
        info.reasoning_effort = None;
        info.reasoning_efforts.clear();
    }
    info.supports_backend_search = false;
    info.supports_standalone_web_search = Some(false);
    info.supported_in_api = true;
    ModelEntry {
        info,
        api_key: None,
        env_key: Some(env_keys()),
        auth_provider: None,
        api_base_url: None,
    }
}

fn fallback_entries(base_url: &str) -> IndexMap<String, ModelEntry> {
    FALLBACK_MODEL_IDS
        .iter()
        .map(|id| {
            let key = format!("runinfra:{id}");
            (key, model_entry(id, base_url))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct RuninfraModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    credential_fingerprint: Option<String>,
    wire_context_present: bool,
}

impl RuninfraModelsCatalog {
    fn dynamic(entries: IndexMap<String, ModelEntry>, api_key: &str) -> Self {
        Self {
            entries,
            credential_fingerprint: Some(credential_fingerprint(api_key)),
            wire_context_present: false,
        }
    }

    fn fallback(base_url: &str) -> Self {
        Self {
            entries: fallback_entries(base_url),
            credential_fingerprint: None,
            wire_context_present: false,
        }
    }

    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn is_authoritative(&self) -> bool {
        self.credential_fingerprint.is_some()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuninfraModelsClient {
    http: reqwest::Client,
    base_url: String,
}

impl RuninfraModelsClient {
    pub(crate) fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: api_base_url(),
        }
    }

    #[cfg(test)]
    fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    pub(crate) async fn query(&self) -> anyhow::Result<Option<RuninfraModelsCatalog>> {
        let Some(api_key) = api_key_for_base_url(&self.base_url) else {
            return Ok(None);
        };
        Ok(Some(self.query_with_fallback(&api_key).await))
    }

    async fn query_with_fallback(&self, api_key: &str) -> RuninfraModelsCatalog {
        match self.query_with_key(api_key).await {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "RunInfra models request failed; using curated fallback catalog"
                );
                crate::unified_log::warn(
                    "RunInfra models request failed; using curated fallback catalog",
                    None,
                    Some(serde_json::json!({
                        "reason": "request_failed",
                        "error": error.to_string(),
                    })),
                );
                let catalog = RuninfraModelsCatalog::fallback(&self.base_url);
                log_catalog_refreshed(&catalog.entries, false, false);
                catalog
            }
        }
    }

    pub(crate) fn has_usable_api_key(&self) -> bool {
        api_key_for_base_url(&self.base_url).is_some()
    }

    pub(crate) fn catalog_matches_current_credential(
        &self,
        catalog: &RuninfraModelsCatalog,
    ) -> bool {
        let Some(fingerprint) = catalog.credential_fingerprint.as_ref() else {
            return false;
        };
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|current| &current == fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<RuninfraModelsCatalog> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .timeout(RUNINFRA_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .send()
            .await
            .with_context(|| format!("RunInfra models request to {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "RunInfra models request returned {status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }
        let wire: RuninfraModelsResponse = response
            .json()
            .await
            .context("RunInfra models response was invalid")?;
        let catalog = self.catalog_from_wire(wire, api_key);
        if catalog.is_authoritative() {
            log_catalog_refreshed(&catalog.entries, true, catalog.wire_context_present);
        } else {
            crate::unified_log::info(
                "RunInfra model catalog using curated fallback",
                None,
                Some(serde_json::json!({
                    "reason": "empty_wire_response",
                })),
            );
            log_catalog_refreshed(&catalog.entries, false, false);
        }
        Ok(catalog)
    }

    fn catalog_from_wire(
        &self,
        wire: RuninfraModelsResponse,
        api_key: &str,
    ) -> RuninfraModelsCatalog {
        let mut wire_context_present = false;
        let entries: IndexMap<String, ModelEntry> = wire
            .data
            .into_iter()
            .filter_map(|model| {
                let id = model.id.trim().to_owned();
                if id.is_empty() {
                    return None;
                }
                let wire_context = model.context_window.filter(|&value| value > 0);
                if wire_context.is_some() {
                    wire_context_present = true;
                }
                let key = format!("runinfra:{id}");
                Some((
                    key,
                    model_entry_with_limits(
                        &id,
                        &self.base_url,
                        wire_context,
                        model.max_output_tokens,
                    ),
                ))
            })
            .collect();
        if entries.is_empty() {
            RuninfraModelsCatalog::fallback(&self.base_url)
        } else {
            let mut catalog = RuninfraModelsCatalog::dynamic(entries, api_key);
            catalog.wire_context_present = wire_context_present;
            catalog
        }
    }
}

fn safe_error_excerpt(body: &str, api_key: &str) -> String {
    let sanitized = body
        .replace(api_key, "[REDACTED]")
        .replace(['\r', '\n'], " ");
    sanitized.chars().take(512).collect()
}

#[derive(Debug, Deserialize)]
struct RuninfraModelsResponse {
    #[serde(default)]
    data: Vec<RuninfraWireModel>,
}

#[derive(Debug, Deserialize)]
struct RuninfraWireModel {
    id: String,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
}

impl RuninfraWireModel {
    #[cfg(test)]
    fn from_id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            context_window: None,
            max_output_tokens: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn trusted_hosts_are_provider_scoped() {
        assert!(is_trusted_api_base_url(RUNINFRA_API_BASE_URL));
        assert!(is_trusted_api_base_url("https://api.runinfra.ai/v1/models"));
        assert!(!is_trusted_api_base_url("http://api.runinfra.ai/v1"));
        assert!(!is_trusted_api_base_url(
            "https://api.runinfra.ai.example/v1"
        ));
        assert!(!is_trusted_api_base_url("https://api.x.ai/v1"));
        assert!(!is_trusted_api_base_url("https://proxy.example/v1"));
    }

    #[test]
    fn stored_keys_never_leave_owned_hosts() {
        let stored = Some("runinfra-stored-secret".to_owned());
        assert_eq!(
            select_api_key(RUNINFRA_API_BASE_URL, None, stored.clone()).as_deref(),
            Some("runinfra-stored-secret")
        );
        assert_eq!(
            select_api_key("https://proxy.example/v1", None, stored).as_deref(),
            None
        );
        assert_eq!(
            select_api_key(
                "https://proxy.example/v1",
                Some("explicit-environment-secret".to_owned()),
                None,
            )
            .as_deref(),
            Some("explicit-environment-secret")
        );
    }

    #[test]
    fn wire_catalog_uses_only_returned_ids_and_chat_completions() {
        let client = RuninfraModelsClient::with_base_url(RUNINFRA_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            RuninfraModelsResponse {
                data: vec![
                    RuninfraWireModel::from_id(" deepseek-v4-flash "),
                    RuninfraWireModel::from_id(""),
                    RuninfraWireModel::from_id("workspace-deploy"),
                ],
            },
            "catalog-key",
        );
        let entries = catalog.entries();
        assert_eq!(entries.len(), 2);
        assert!(catalog.is_authoritative());
        for (key, entry) in &entries {
            assert!(key.starts_with("runinfra:"));
            assert_eq!(entry.info.provider, ModelProvider::Runinfra);
            assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
            assert_eq!(entry.info.tool_mode, Some(ToolMode::Direct));
            assert!(!entry.info.supports_backend_search);
            assert_eq!(entry.info.supports_standalone_web_search, Some(false));
            assert_eq!(
                entry.env_key.as_ref().and_then(EnvKeys::primary),
                Some(RUNINFRA_GATEWAY_KEY_ENV)
            );
        }
        assert!(
            entries["runinfra:deepseek-v4-flash"]
                .info
                .supports_reasoning_effort
        );
        let deploy = &entries["runinfra:workspace-deploy"];
        assert!(!deploy.info.supports_reasoning_effort);
        assert!(deploy.info.reasoning_efforts.is_empty());
    }

    #[test]
    fn fallback_catalog_marks_hosted_reasoning_models() {
        let catalog = RuninfraModelsCatalog::fallback(RUNINFRA_API_BASE_URL);
        assert!(!catalog.is_authoritative());
        let entries = catalog.entries();
        assert_eq!(entries.len(), 4);
        assert!(entries.contains_key("runinfra:deepseek-v4-flash"));
        assert!(entries.contains_key("runinfra:qwen3-8-2-4t-a95b"));
        assert!(is_known_reasoning_model("deepseek-v4-flash"));
        assert!(!is_known_reasoning_model("workspace-deploy"));
    }

    #[test]
    fn deepseek_v4_flash_defaults_to_max_and_offers_none() {
        let catalog = RuninfraModelsCatalog::fallback(RUNINFRA_API_BASE_URL);
        let flash = &catalog.entries()["runinfra:deepseek-v4-flash"];
        let efforts: Vec<ReasoningEffort> = flash
            .info
            .reasoning_efforts
            .iter()
            .map(|option| option.value)
            .collect();
        assert_eq!(
            efforts,
            [
                ReasoningEffort::None,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ]
        );
        assert_eq!(flash.info.reasoning_effort, Some(ReasoningEffort::Max));
        assert_eq!(flash.info.context_window.get(), 1_048_576);
        assert_eq!(flash.info.max_completion_tokens, Some(32_768));
        assert_eq!(flash.info.name.as_deref(), Some("DeepSeek V4 Flash"));
    }

    #[test]
    fn qwen_2_4t_cannot_disable_reasoning() {
        let catalog = RuninfraModelsCatalog::fallback(RUNINFRA_API_BASE_URL);
        let qwen = &catalog.entries()["runinfra:qwen3-8-2-4t-a95b"];
        let efforts: Vec<ReasoningEffort> = qwen
            .info
            .reasoning_efforts
            .iter()
            .map(|option| option.value)
            .collect();
        assert_eq!(
            efforts,
            [
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ]
        );
        assert_eq!(qwen.info.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(qwen.info.context_window.get(), 262_144);
    }

    #[test]
    fn empty_wire_response_falls_back_to_curated_catalog() {
        let client = RuninfraModelsClient::with_base_url(RUNINFRA_API_BASE_URL);
        let catalog =
            client.catalog_from_wire(RuninfraModelsResponse { data: Vec::new() }, "catalog-key");
        assert!(!catalog.is_authoritative());
        assert!(catalog.entries().contains_key("runinfra:deepseek-v4-flash"));
    }

    #[test]
    fn wire_context_and_max_output_win_over_curated() {
        let client = RuninfraModelsClient::with_base_url(RUNINFRA_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            RuninfraModelsResponse {
                data: vec![RuninfraWireModel {
                    id: "deepseek-v4-flash".to_owned(),
                    context_window: Some(500_000),
                    max_output_tokens: Some(16_384),
                }],
            },
            "catalog-key",
        );
        let flash = &catalog.entries()["runinfra:deepseek-v4-flash"];
        assert_eq!(flash.info.context_window.get(), 500_000);
        assert_eq!(flash.info.max_completion_tokens, Some(16_384));
    }

    #[test]
    fn live_models_payload_uses_published_shape() {
        let wire: RuninfraModelsResponse = serde_json::from_value(serde_json::json!({
            "object": "list",
            "data": [{
                "id": "deepseek-v4-flash",
                "object": "model",
                "owned_by": "runinfra",
                "created": 1785679270,
                "availability": "available",
                "max_request_bytes": 3670016,
                "max_output_tokens": 32768,
                "context_window": 1048576,
                "max_concurrent_requests_per_api_key": 16,
                "max_tokens_per_minute_per_workspace": 4000000
            }]
        }))
        .expect("live RunInfra /models shape");
        let client = RuninfraModelsClient::with_base_url(RUNINFRA_API_BASE_URL);
        let catalog = client.catalog_from_wire(wire, "catalog-key");
        let flash = &catalog.entries()["runinfra:deepseek-v4-flash"];
        assert_eq!(flash.info.context_window.get(), 1_048_576);
        assert_eq!(flash.info.max_completion_tokens, Some(32_768));
    }

    #[test]
    fn errors_redact_credentials() {
        assert_eq!(
            safe_error_excerpt("invalid runinfra-secret\nretry", "runinfra-secret"),
            "invalid [REDACTED] retry"
        );
    }

    #[tokio::test]
    async fn model_query_uses_bearer_auth() {
        #[derive(Clone, Default)]
        struct RequestCapture(Arc<Mutex<Option<String>>>);

        async fn models(
            State(capture): State<RequestCapture>,
            headers: HeaderMap,
        ) -> Json<serde_json::Value> {
            *capture.0.lock().expect("capture lock") = headers
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            Json(serde_json::json!({
                "object": "list",
                "data": [{"id": "deepseek-v4-flash", "context_window": 1048576, "max_output_tokens": 32768}]
            }))
        }

        let capture = RequestCapture::default();
        let app = Router::new()
            .route("/v1/models", get(models))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = RuninfraModelsClient::with_base_url(format!("http://{address}/v1"));
        let catalog = client
            .query_with_key("test-runinfra-key")
            .await
            .expect("model query");
        assert_eq!(
            capture.0.lock().expect("capture lock").as_deref(),
            Some("Bearer test-runinfra-key")
        );
        assert!(catalog.entries().contains_key("runinfra:deepseek-v4-flash"));
        assert!(catalog.is_authoritative());
    }

    #[tokio::test]
    async fn query_falls_back_when_models_endpoint_unavailable() {
        let client = RuninfraModelsClient::with_base_url("http://127.0.0.1:1/v1");
        let catalog = client.query_with_fallback("test-key").await;
        assert!(!catalog.is_authoritative());
        assert!(catalog.entries().contains_key("runinfra:deepseek-v4-flash"));
    }
}
