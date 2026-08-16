//! Provider-isolated Google Gemini API (AI Studio) model discovery.
//!
//! Gemini's OpenAI-compatible Chat Completions surface lives at
//! `https://generativelanguage.googleapis.com/v1beta/openai/`. Live `GET /models`
//! returns the entire Gemini catalog (imagen, TTS, live, embeddings), so Open
//! Grok treats a curated four-model list as authoritative. A successful
//! `/models` response may enrich curated context / max-output values but must
//! never add or remove picker entries.

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

/// Official Gemini OpenAI-compatible base URL.
pub const GEMINI_API_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
pub const GEMINI_API_BASE_URL_ENV: &str = "OPENGROK_GEMINI_API_BASE_URL";
/// Official Gemini API / AI Studio environment variable.
pub const GEMINI_API_KEY_ENV: &str = "GEMINI_API_KEY";
/// Common Google-client alias accepted as a process-level BYOK choice.
pub const GOOGLE_API_KEY_ENV: &str = "GOOGLE_API_KEY";
const GEMINI_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CONTEXT_WINDOW: u64 = 1_048_576;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 65_536;

#[derive(Clone, Copy, Debug)]
pub struct CuratedGeminiModel {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

/// The only Gemini models Open Grok exposes in the picker. A `/models`
/// response may enrich these entries but can neither add nor remove them.
pub const CURATED_GEMINI_MODELS: [CuratedGeminiModel; 4] = [
    CuratedGeminiModel {
        id: "gemini-3.7-flash",
        name: "Gemini 3.7 Flash",
        description: "Gemini 3.7 Flash on Google AI Studio (low/medium/high thinking)",
    },
    CuratedGeminiModel {
        id: "gemini-3.6-flash",
        name: "Gemini 3.6 Flash",
        description: "Gemini 3.6 Flash on Google AI Studio",
    },
    CuratedGeminiModel {
        id: "gemini-3.5-flash-lite",
        name: "Gemini 3.5 Flash-Lite",
        description: "Gemini 3.5 Flash-Lite on Google AI Studio (minimal thinking default)",
    },
    CuratedGeminiModel {
        id: "gemini-3.1-pro-preview",
        name: "Gemini 3.1 Pro Preview",
        description: "Gemini 3.1 Pro Preview on Google AI Studio (low/medium/high thinking)",
    },
];

pub fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https" && url.host_str() == Some("generativelanguage.googleapis.com")
}

pub fn api_base_url() -> String {
    std::env::var(GEMINI_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| GEMINI_API_BASE_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    for name in [GEMINI_API_KEY_ENV, GOOGLE_API_KEY_ENV] {
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
    EnvKeys::new([GEMINI_API_KEY_ENV, GOOGLE_API_KEY_ENV])
}

fn stored_api_key() -> Option<String> {
    crate::auth::read_provider_api_key(&crate::util::grok_home::grok_home(), ModelProvider::Gemini)
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

pub fn catalog_key(model_id: &str) -> String {
    format!("gemini:{model_id}")
}

pub fn supports_minimal_reasoning(model_id: &str) -> bool {
    !matches!(model_id, "gemini-3.7-flash" | "gemini-3.1-pro-preview")
}

pub fn default_reasoning_effort(model_id: &str) -> ReasoningEffort {
    match model_id {
        "gemini-3.5-flash-lite" => ReasoningEffort::Minimal,
        "gemini-3.1-pro-preview" => ReasoningEffort::High,
        _ => ReasoningEffort::Medium,
    }
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
            ReasoningEffort::Minimal => "Minimal",
            ReasoningEffort::Low => "Low",
            ReasoningEffort::Medium => "Medium",
            ReasoningEffort::High => "High",
            _ => unreachable!("Gemini 3 menus expose only minimal/low/medium/high"),
        }
        .to_owned(),
        description: Some(description.to_owned()),
        default,
    }
}

fn gemini_reasoning_efforts(model_id: &str) -> Vec<ReasoningEffortOption> {
    let default = default_reasoning_effort(model_id);
    let mut options = Vec::new();
    if supports_minimal_reasoning(model_id) {
        options.push(effort_option(
            ReasoningEffort::Minimal,
            "Use as few thinking tokens as possible; Gemini 3 cannot fully disable thinking",
            default == ReasoningEffort::Minimal,
        ));
    }
    options.extend([
        effort_option(
            ReasoningEffort::Low,
            "Minimize latency and cost",
            default == ReasoningEffort::Low,
        ),
        effort_option(
            ReasoningEffort::Medium,
            "Balanced thinking for most tasks",
            default == ReasoningEffort::Medium,
        ),
        effort_option(
            ReasoningEffort::High,
            "Maximum thinking depth",
            default == ReasoningEffort::High,
        ),
    ]);
    options
}

fn assigned_context_window(wire_context: Option<u64>) -> NonZeroU64 {
    NonZeroU64::new(
        wire_context
            .filter(|&value| value > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW),
    )
    .unwrap_or_else(|| NonZeroU64::new(DEFAULT_CONTEXT_WINDOW).expect("non-zero Gemini fallback"))
}

fn assigned_max_output(wire_max_output: Option<u32>) -> u32 {
    wire_max_output
        .filter(|&value| value > 0)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
}

fn log_catalog_refreshed(entries: &IndexMap<String, ModelEntry>, enriched: bool) {
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
        "Gemini model catalog refreshed",
        None,
        Some(serde_json::json!({
            "count": entries.len(),
            "enriched": enriched,
            "models": models,
        })),
    );
}

fn curated_model_entry(
    curated: &CuratedGeminiModel,
    base_url: &str,
    wire_context: Option<u64>,
    wire_max_output: Option<u32>,
) -> ModelEntry {
    let key = catalog_key(curated.id);
    let mut info = ModelInfo::fallback(&key);
    info.id = Some(key);
    info.model = curated.id.to_owned();
    info.base_url = base_url.trim_end_matches('/').to_owned();
    info.name = Some(curated.name.to_owned());
    info.description = Some(curated.description.to_owned());
    info.api_backend = ApiBackend::ChatCompletions;
    info.provider = ModelProvider::Gemini;
    info.tool_mode = Some(ToolMode::Direct);
    info.context_window = assigned_context_window(wire_context);
    info.max_completion_tokens = Some(assigned_max_output(wire_max_output));
    let efforts = gemini_reasoning_efforts(curated.id);
    info.reasoning_effort = efforts
        .iter()
        .find(|option| option.default)
        .map(|option| option.value);
    info.reasoning_efforts = efforts;
    info.supports_reasoning_effort = true;
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

fn curated_entries(
    base_url: &str,
    limits: &IndexMap<String, (Option<u64>, Option<u32>)>,
) -> IndexMap<String, ModelEntry> {
    CURATED_GEMINI_MODELS
        .iter()
        .map(|curated| {
            let (context, max_output) = limits.get(curated.id).copied().unwrap_or((None, None));
            (
                catalog_key(curated.id),
                curated_model_entry(curated, base_url, context, max_output),
            )
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct GeminiModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    credential_fingerprint: String,
    enriched: bool,
}

impl GeminiModelsCatalog {
    fn curated(base_url: &str, api_key: &str) -> Self {
        Self {
            entries: curated_entries(base_url, &IndexMap::new()),
            credential_fingerprint: credential_fingerprint(api_key),
            enriched: false,
        }
    }

    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn is_authoritative(&self) -> bool {
        !self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GeminiModelsClient {
    http: reqwest::Client,
    base_url: String,
}

impl GeminiModelsClient {
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

    pub(crate) async fn query(&self) -> anyhow::Result<Option<GeminiModelsCatalog>> {
        let Some(api_key) = api_key_for_base_url(&self.base_url) else {
            return Ok(None);
        };
        Ok(Some(self.query_with_fallback(&api_key).await))
    }

    async fn query_with_fallback(&self, api_key: &str) -> GeminiModelsCatalog {
        match self.query_with_key(api_key).await {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "Gemini models request failed; using curated catalog"
                );
                crate::unified_log::warn(
                    "Gemini models request failed; using curated catalog",
                    None,
                    Some(serde_json::json!({
                        "reason": "request_failed",
                        "error": error.to_string(),
                    })),
                );
                let catalog = GeminiModelsCatalog::curated(&self.base_url, api_key);
                log_catalog_refreshed(&catalog.entries, false);
                catalog
            }
        }
    }

    pub(crate) fn has_usable_api_key(&self) -> bool {
        api_key_for_base_url(&self.base_url).is_some()
    }

    pub(crate) fn catalog_matches_current_credential(&self, catalog: &GeminiModelsCatalog) -> bool {
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|current| current == catalog.credential_fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<GeminiModelsCatalog> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .timeout(GEMINI_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .send()
            .await
            .with_context(|| format!("Gemini models request to {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Gemini models request returned {status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }
        let wire: GeminiModelsResponse = response
            .json()
            .await
            .context("Gemini models response was invalid")?;
        let catalog = self.catalog_from_wire(wire, api_key);
        log_catalog_refreshed(&catalog.entries, catalog.enriched);
        Ok(catalog)
    }

    /// Project the wire response onto the curated list. Uncurated ids
    /// (imagen, TTS, live, embeddings, other Gemini slugs) are ignored.
    fn catalog_from_wire(&self, wire: GeminiModelsResponse, api_key: &str) -> GeminiModelsCatalog {
        let mut limits = IndexMap::new();
        for model in wire.data {
            let id = model.id.trim();
            let id = id.strip_prefix("models/").unwrap_or(id);
            if id.is_empty() {
                continue;
            }
            let context = model
                .context_window
                .or(model.context_length)
                .or(model.input_token_limit)
                .filter(|&value| value > 0);
            let max_output = model
                .max_output_tokens
                .or(model.output_token_limit)
                .filter(|&value| value > 0);
            if context.is_some() || max_output.is_some() {
                limits.insert(id.to_owned(), (context, max_output));
            }
        }
        let entries = curated_entries(&self.base_url, &limits);
        let enriched = CURATED_GEMINI_MODELS.iter().any(|curated| {
            limits
                .get(curated.id)
                .is_some_and(|(context, max_output)| context.is_some() || max_output.is_some())
        });
        GeminiModelsCatalog {
            entries,
            credential_fingerprint: credential_fingerprint(api_key),
            enriched,
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
struct GeminiModelsResponse {
    #[serde(default)]
    data: Vec<GeminiWireModel>,
}

#[derive(Debug, Deserialize)]
struct GeminiWireModel {
    id: String,
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    input_token_limit: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    output_token_limit: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn trusted_hosts_are_provider_scoped() {
        assert!(is_trusted_api_base_url(GEMINI_API_BASE_URL));
        assert!(is_trusted_api_base_url(
            "https://generativelanguage.googleapis.com/v1beta/openai/models"
        ));
        assert!(!is_trusted_api_base_url(
            "http://generativelanguage.googleapis.com/v1beta/openai"
        ));
        assert!(!is_trusted_api_base_url(
            "https://generativelanguage.googleapis.com.example/v1"
        ));
        assert!(!is_trusted_api_base_url("https://api.x.ai/v1"));
        assert!(!is_trusted_api_base_url("https://proxy.example/v1"));
    }

    #[test]
    fn stored_keys_never_leave_owned_hosts() {
        let stored = Some("gemini-stored-secret".to_owned());
        assert_eq!(
            select_api_key(GEMINI_API_BASE_URL, None, stored.clone()).as_deref(),
            Some("gemini-stored-secret")
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
    fn curated_entries_are_chat_only_with_model_specific_reasoning_menus() {
        let catalog = GeminiModelsCatalog::curated(GEMINI_API_BASE_URL, "catalog-key");
        assert!(catalog.is_authoritative());
        let entries = catalog.entries();
        assert_eq!(entries.len(), CURATED_GEMINI_MODELS.len());
        for curated in &CURATED_GEMINI_MODELS {
            let entry = entries
                .get(&catalog_key(curated.id))
                .expect("curated Gemini entry");
            assert_eq!(entry.info.provider, ModelProvider::Gemini);
            assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
            assert_eq!(entry.info.tool_mode, Some(ToolMode::Direct));
            assert_eq!(entry.info.model, curated.id);
            assert_eq!(entry.info.name.as_deref(), Some(curated.name));
            assert_eq!(entry.info.context_window.get(), DEFAULT_CONTEXT_WINDOW);
            assert_eq!(
                entry.info.max_completion_tokens,
                Some(DEFAULT_MAX_OUTPUT_TOKENS)
            );
            assert!(!entry.info.supports_backend_search);
            assert_eq!(entry.info.supports_standalone_web_search, Some(false));
            assert!(entry.info.supports_reasoning_effort);
            assert_eq!(
                entry.info.reasoning_effort,
                Some(default_reasoning_effort(curated.id))
            );
            assert_eq!(
                entry.env_key.as_ref().and_then(EnvKeys::primary),
                Some(GEMINI_API_KEY_ENV)
            );
        }

        let flash_37 = &entries["gemini:gemini-3.7-flash"];
        assert_eq!(
            flash_37
                .info
                .reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ]
        );
        assert_eq!(
            flash_37.info.reasoning_effort,
            Some(ReasoningEffort::Medium)
        );

        let flash_36 = &entries["gemini:gemini-3.6-flash"];
        assert_eq!(
            flash_36
                .info
                .reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Minimal,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ]
        );
        assert_eq!(
            flash_36.info.reasoning_effort,
            Some(ReasoningEffort::Medium)
        );

        let lite = &entries["gemini:gemini-3.5-flash-lite"];
        assert_eq!(
            lite.info
                .reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Minimal,
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ]
        );
        assert_eq!(lite.info.reasoning_effort, Some(ReasoningEffort::Minimal));

        let pro = &entries["gemini:gemini-3.1-pro-preview"];
        assert_eq!(
            pro.info
                .reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High,
            ]
        );
        assert_eq!(pro.info.reasoning_effort, Some(ReasoningEffort::High));
    }

    #[test]
    fn wire_metadata_enriches_but_cannot_add_models() {
        let client = GeminiModelsClient::with_base_url(GEMINI_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            GeminiModelsResponse {
                data: vec![
                    GeminiWireModel {
                        id: "models/gemini-3.7-flash".to_owned(),
                        context_window: Some(2_097_152),
                        context_length: None,
                        input_token_limit: None,
                        max_output_tokens: Some(8_192),
                        output_token_limit: None,
                    },
                    GeminiWireModel {
                        id: "imagen-4.0-generate-001".to_owned(),
                        context_window: Some(8_192),
                        context_length: None,
                        input_token_limit: None,
                        max_output_tokens: None,
                        output_token_limit: None,
                    },
                    GeminiWireModel {
                        id: "gemini-2.5-flash".to_owned(),
                        context_window: Some(1_000_000),
                        context_length: None,
                        input_token_limit: None,
                        max_output_tokens: None,
                        output_token_limit: None,
                    },
                ],
            },
            "catalog-key",
        );
        let entries = catalog.entries();
        assert_eq!(entries.len(), CURATED_GEMINI_MODELS.len());
        assert_eq!(
            entries["gemini:gemini-3.7-flash"].info.context_window.get(),
            2_097_152
        );
        assert_eq!(
            entries["gemini:gemini-3.7-flash"]
                .info
                .max_completion_tokens,
            Some(8_192)
        );
        assert!(!entries.keys().any(|key| key.contains("imagen")));
        assert!(!entries.contains_key("gemini:gemini-2.5-flash"));
        assert_eq!(
            entries["gemini:gemini-3.6-flash"].info.context_window.get(),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn error_excerpt_redacts_a_reflected_credential() {
        let excerpt = safe_error_excerpt(
            "request rejected for model-query-canary\ntry again",
            "model-query-canary",
        );
        assert_eq!(excerpt, "request rejected for [REDACTED] try again");
    }

    #[tokio::test]
    async fn model_query_uses_bearer_auth_and_preserves_curated_ids() {
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
                "data": [
                    {"id": "gemini-3.6-flash", "context_length": 1_048_576},
                    {"id": "imagen-4.0-generate-001"}
                ]
            }))
        }

        let capture = RequestCapture::default();
        let app = Router::new()
            .route("/v1beta/openai/models", get(models))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = GeminiModelsClient::with_base_url(format!("http://{address}/v1beta/openai"));
        let catalog = client.query_with_key("model-query-canary").await.unwrap();
        let entries = catalog.entries();
        assert_eq!(entries.len(), CURATED_GEMINI_MODELS.len());
        assert_eq!(
            entries["gemini:gemini-3.6-flash"].info.provider,
            ModelProvider::Gemini
        );
        assert_eq!(
            capture.0.lock().unwrap().as_deref(),
            Some("Bearer model-query-canary")
        );
    }
}
