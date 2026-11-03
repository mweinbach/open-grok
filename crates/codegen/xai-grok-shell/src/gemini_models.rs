//! Provider-isolated Google Gemini API (AI Studio) model discovery.
//!
//! Discover chat models from Google's paginated native catalog. Inference and
//! explicitly configured OpenAI-compatible catalog overrides keep their routes.

use crate::agent::config::{EnvKeys, ModelEntry, ModelInfo};
use anyhow::{Context, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::HashSet;
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
const GEMINI_NATIVE_MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const GEMINI_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_CONTEXT_WINDOW: u64 = 1_048_576;
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 65_536;

#[derive(Clone, Copy, Debug)]
pub struct CuratedGeminiModel {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

/// Offline defaults with known reasoning controls.
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

fn log_catalog_refreshed(entries: &IndexMap<String, ModelEntry>, live: bool) {
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
            "source": if live { "live" } else { "fallback" },
            "models": models,
        })),
    );
}

fn model_entry(id: &str, base_url: &str, wire: Option<&GeminiWireModel>) -> ModelEntry {
    let key = catalog_key(id);
    let mut info = ModelInfo::fallback(&key);
    let curated = CURATED_GEMINI_MODELS.iter().find(|model| model.id == id);
    info.id = Some(key);
    info.model = id.to_owned();
    info.base_url = base_url.trim_end_matches('/').to_owned();
    info.name = Some(
        wire.and_then(|m| m.display_name.as_deref())
            .filter(|name| !name.trim().is_empty())
            .or_else(|| curated.map(|m| m.name))
            .unwrap_or(id)
            .to_owned(),
    );
    info.description = wire
        .and_then(|m| m.description.clone())
        .or_else(|| curated.map(|m| m.description.to_owned()));
    info.api_backend = ApiBackend::ChatCompletions;
    info.provider = ModelProvider::Gemini;
    info.tool_mode = Some(ToolMode::Direct);
    let context = wire.and_then(GeminiWireModel::context_window);
    let max_output = wire.and_then(GeminiWireModel::max_output);
    if curated.is_some() {
        info.context_window = assigned_context_window(context);
        info.max_completion_tokens = Some(assigned_max_output(max_output));
        info.reasoning_efforts = gemini_reasoning_efforts(id);
        info.reasoning_effort = Some(default_reasoning_effort(id));
        info.supports_reasoning_effort = true;
    } else {
        if let Some(limit) = context.and_then(NonZeroU64::new) {
            info.context_window = limit;
        }
        info.max_completion_tokens = max_output;
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

#[derive(Clone, Debug)]
pub(crate) struct GeminiModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    credential_fingerprint: String,
    live: bool,
}

impl GeminiModelsCatalog {
    fn curated(base_url: &str, api_key: &str) -> Self {
        Self {
            entries: CURATED_GEMINI_MODELS
                .iter()
                .map(|m| (catalog_key(m.id), model_entry(m.id, base_url, None)))
                .collect(),
            credential_fingerprint: credential_fingerprint(api_key),
            live: false,
        }
    }

    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn is_live(&self) -> bool {
        self.live
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GeminiModelsClient {
    base_url: String,
}

impl GeminiModelsClient {
    pub(crate) fn new() -> Self {
        Self {
            base_url: api_base_url(),
        }
    }

    #[cfg(test)]
    fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
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
        let base = self.base_url.trim_end_matches('/');
        let native = base == GEMINI_API_BASE_URL;
        let url = if native {
            GEMINI_NATIVE_MODELS_URL.to_owned()
        } else {
            format!("{base}/models")
        };
        tokio::time::timeout(
            GEMINI_MODELS_REQUEST_TIMEOUT,
            self.query_pages(&url, native, api_key),
        )
        .await
        .context("Gemini model discovery timed out")?
    }

    async fn query_pages(
        &self,
        url: &str,
        native: bool,
        api_key: &str,
    ) -> anyhow::Result<GeminiModelsCatalog> {
        // A redirect must never forward x-goog-api-key to another host.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let mut entries = IndexMap::new();
        let mut next_page = None;
        let mut seen_tokens = HashSet::new();
        loop {
            let request = http.get(url);
            let mut request = if native {
                request
                    .header("x-goog-api-key", api_key)
                    .query(&[("pageSize", "1000")])
            } else {
                request.bearer_auth(api_key)
            };
            if let Some(token) = next_page.take() {
                request = request.query(&[("pageToken", token)]);
            }
            let response = request
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
            let page: GeminiModelsResponse = response
                .json()
                .await
                .context("Gemini models response was invalid")?;
            for model in page.models {
                if let Some(id) = model.chat_model_id(native) {
                    entries.insert(
                        catalog_key(id),
                        model_entry(id, &self.base_url, Some(&model)),
                    );
                }
            }
            let Some(token) = page.next_page_token.filter(|token| !token.is_empty()) else {
                break;
            };
            anyhow::ensure!(
                seen_tokens.insert(token.clone()),
                "Gemini model discovery returned a repeated page token"
            );
            next_page = Some(token);
        }
        log_catalog_refreshed(&entries, true);
        Ok(GeminiModelsCatalog {
            entries,
            credential_fingerprint: credential_fingerprint(api_key),
            live: true,
        })
    }
}

fn safe_error_excerpt(body: &str, api_key: &str) -> String {
    let sanitized = body
        .replace(api_key, "[REDACTED]")
        .replace(['\r', '\n'], " ");
    sanitized.chars().take(512).collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiModelsResponse {
    #[serde(alias = "data")]
    models: Vec<GeminiWireModel>,
    #[serde(alias = "next_page_token")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiWireModel {
    // Google returns `name: "models/<id>"`; compatible endpoints return `id`.
    id: Option<String>,
    name: Option<String>,
    display_name: Option<String>,
    description: Option<String>,
    supported_generation_methods: Option<Vec<String>>,
    #[serde(alias = "supportedOutputModalities")]
    output_modalities: Option<Vec<String>>,
    #[serde(rename = "context_window")]
    context: Option<u64>,
    #[serde(rename = "context_length")]
    context_length: Option<u64>,
    #[serde(alias = "input_token_limit")]
    input_token_limit: Option<u64>,
    #[serde(rename = "max_output_tokens")]
    max_output_tokens: Option<u32>,
    #[serde(alias = "output_token_limit")]
    output_token_limit: Option<u32>,
}

impl GeminiWireModel {
    fn chat_model_id(&self, native: bool) -> Option<&str> {
        let name = self.id.as_deref().or(self.name.as_deref())?.trim();
        let id = name.strip_prefix("models/").unwrap_or(name);
        // OpenAI-compatible lists can contain just IDs. Google's native list
        // identifies generation methods, even on specialized models (e.g. TTS).
        let can_generate = self
            .supported_generation_methods
            .as_ref()
            .map_or(!native, |methods| {
                methods.iter().any(|m| m == "generateContent")
            });
        let produces_text = self
            .output_modalities
            .as_ref()
            .is_none_or(|modes| modes.iter().any(|m| m.eq_ignore_ascii_case("text")));
        if !id.starts_with("gemini-")
            || !can_generate
            || !produces_text
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_'))
            || id.split(['-', '_']).any(|part| {
                matches!(
                    part,
                    "tts"
                        | "audio"
                        | "live"
                        | "image"
                        | "embedding"
                        | "robotics"
                        | "computer"
                        | "transcribe"
                )
            })
            || id.starts_with("gemini-nano-banana-")
            || id.starts_with("gemini-omni-")
            || id.contains("deep-research")
        {
            return None;
        }
        Some(id)
    }

    fn context_window(&self) -> Option<u64> {
        [self.input_token_limit, self.context, self.context_length]
            .into_iter()
            .flatten()
            .find(|&n| n > 0)
    }

    fn max_output(&self) -> Option<u32> {
        [self.output_token_limit, self.max_output_tokens]
            .into_iter()
            .flatten()
            .find(|&n| n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::HeaderMap, routing::get};

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
        assert!(!catalog.is_live());
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
    fn error_excerpt_redacts_a_reflected_credential() {
        let excerpt = safe_error_excerpt(
            "request rejected for model-query-canary\ntry again",
            "model-query-canary",
        );
        assert_eq!(excerpt, "request rejected for [REDACTED] try again");
    }

    #[tokio::test]
    async fn native_catalog_pages_only_return_text_generation_models() {
        use axum::extract::Query;
        use std::collections::HashMap;

        async fn page(
            headers: HeaderMap,
            Query(query): Query<HashMap<String, String>>,
        ) -> Json<serde_json::Value> {
            assert_eq!(headers["x-goog-api-key"], "test-key");
            assert!(!headers.contains_key("authorization"));
            assert_eq!(query["pageSize"], "1000");
            if let Some(token) = query.get("pageToken") {
                assert_eq!(token, "next+/=");
                return Json(serde_json::json!({"models": [
                    {"name":"models/gemini-4-test-preview","displayName":"Gemini test preview",
                     "inputTokenLimit":2000000,"outputTokenLimit":40000,
                     "supportedGenerationMethods":["generateContent"]},
                    {"name":"models/gemini-4-test-preview-tts","supportedGenerationMethods":["generateContent"]},
                    {"name":"models/gemini-nano-banana-fixture","supportedGenerationMethods":["generateContent"]},
                    {"name":"models/gemini-omni-fixture","supportedGenerationMethods":["generateContent"]},
                    {"name":"models/gemini-test-transcribe","supportedGenerationMethods":["generateContent"]},
                    {"name":"models/gemini-other","supportedGenerationMethods":["embedContent"]},
                    {"name":"models/gemini-music","supportedGenerationMethods":["generateContent"],"outputModalities":["AUDIO"]}
                ]}));
            }
            Json(serde_json::json!({"nextPageToken":"next+/=","models":[
                {"name":"models/gemini-3.7-flash","inputTokenLimit":12345,"supportedGenerationMethods":["generateContent"]},
                {"name":"models/imagen-model","supportedGenerationMethods":["generateContent"]},
                {"name":"models/gemini-no-methods"}
            ]}))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1beta/models", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/v1beta/models", get(page)))
                .await
                .unwrap()
        });
        let client = GeminiModelsClient::with_base_url(GEMINI_API_BASE_URL);
        let catalog = client.query_pages(&url, true, "test-key").await.unwrap();
        assert_eq!(catalog.entries.len(), 2);
        let model = &catalog.entries["gemini:gemini-4-test-preview"].info;
        assert_eq!(model.model, "gemini-4-test-preview");
        assert_eq!(model.name.as_deref(), Some("Gemini test preview"));
        assert_eq!(model.provider, ModelProvider::Gemini);
        assert_eq!(model.api_backend, ApiBackend::ChatCompletions);
        assert_eq!(model.base_url, GEMINI_API_BASE_URL);
        assert_eq!(model.context_window.get(), 2_000_000);
        assert_eq!(model.max_completion_tokens, Some(40_000));
        assert!(!model.supports_reasoning_effort);
        let known = &catalog.entries["gemini:gemini-3.7-flash"].info;
        assert!(known.supports_reasoning_effort);
        assert_eq!(known.context_window.get(), 12345);
    }

    #[tokio::test]
    async fn compatible_endpoint_discovers_ids_without_capability_metadata() {
        async fn models(headers: HeaderMap) -> Json<serde_json::Value> {
            assert_eq!(headers["authorization"], "Bearer test-key");
            assert!(!headers.contains_key("x-goog-api-key"));
            Json(serde_json::json!({"object": "list","data":[
                {"id":"gemini-4-test"}, {"id":"gemini-3.6-flash","context_length":262144},
                {"id":"gemini-live-test"},{"id":"imagen-4.0-generate-001"}
            ]}))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1beta/openai", listener.local_addr().unwrap());
        let app = Router::new().route("/v1beta/openai/models", get(models));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = GeminiModelsClient::with_base_url(&base);
        let catalog = client.query_with_key("test-key").await.unwrap();
        assert_eq!(catalog.entries.len(), 2);
        assert_eq!(catalog.entries["gemini:gemini-4-test"].info.base_url, base);
        assert_eq!(
            catalog.entries["gemini:gemini-3.6-flash"]
                .info
                .context_window
                .get(),
            262144
        );
    }

    #[tokio::test]
    async fn native_discovery_does_not_forward_the_key_on_redirect() {
        let app = Router::new()
            .route(
                "/models",
                get(|| async { axum::response::Redirect::temporary("/elsewhere") }),
            )
            .route(
                "/elsewhere",
                get(|| async { Json(serde_json::json!({"models": []})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/models", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = GeminiModelsClient::with_base_url(GEMINI_API_BASE_URL);
        assert!(
            client
                .query_pages(&url, true, "test-key")
                .await
                .unwrap_err()
                .to_string()
                .contains("307")
        );
    }

    #[tokio::test]
    async fn repeated_page_token_does_not_publish_a_partial_catalog() {
        let page =
            Json(serde_json::json!({"models":[{"id":"gemini-4-test"}],"nextPageToken":"same"}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/models", get(move || async move { page }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = GeminiModelsClient::with_base_url(base);
        let catalog = client.query_with_fallback("test-key").await;
        assert!(!catalog.live);
        assert_eq!(catalog.entries.len(), CURATED_GEMINI_MODELS.len());
        assert!(!catalog.entries.contains_key("gemini:gemini-4-test"));
    }
}
