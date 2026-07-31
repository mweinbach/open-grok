//! Provider-isolated DeepSeek direct model discovery.
//!
//! DeepSeek serves OpenAI-compatible Chat Completions and, for V4 Flash, a
//! native Responses API. The provider's `/models` endpoint is authoritative for
//! the curated direct-model partition; unknown future model ids fail closed
//! until their limits and capabilities are reviewed here.

use crate::agent::config::{EnvKeys, ModelEntry, ModelInfo};
use anyhow::{Context, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::num::NonZeroU64;
use std::time::Duration;
use url::Url;
use xai_grok_sampling_types::{ApiBackend, ModelProvider, ReasoningEffort, ToolMode};

pub const DEEPSEEK_API_BASE_URL: &str = "https://api.deepseek.com";
pub const DEEPSEEK_API_BASE_URL_ENV: &str = "OPENGROK_DEEPSEEK_API_BASE_URL";
pub const DEEPSEEK_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
const DEEPSEEK_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub struct CuratedDeepSeekModel {
    pub key: &'static str,
    pub slug: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub context_window: u64,
    pub api_backend: ApiBackend,
}

pub const CURATED_DEEPSEEK_MODELS: [CuratedDeepSeekModel; 2] = [
    CuratedDeepSeekModel {
        key: "deepseek:deepseek-v4-pro",
        slug: "deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        description: "DeepSeek V4 Pro through the direct DeepSeek Chat Completions API",
        context_window: 1_000_000,
        api_backend: ApiBackend::ChatCompletions,
    },
    CuratedDeepSeekModel {
        key: "deepseek:deepseek-v4-flash",
        slug: "deepseek-v4-flash",
        name: "DeepSeek V4 Flash",
        description: "DeepSeek V4 Flash (0731) through the direct DeepSeek Responses API",
        context_window: 1_000_000,
        api_backend: ApiBackend::Responses,
    },
];

pub fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https" && url.host_str() == Some("api.deepseek.com")
}

pub fn api_base_url() -> String {
    std::env::var(DEEPSEEK_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEEPSEEK_API_BASE_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    std::env::var(DEEPSEEK_API_KEY_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn environment_api_key_is_configured() -> bool {
    environment_api_key().is_some()
}

fn stored_api_key() -> Option<String> {
    crate::auth::read_provider_api_key(
        &crate::util::grok_home::grok_home(),
        ModelProvider::DeepSeek,
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

fn reasoning_effort_option(
    value: ReasoningEffort,
    description: &str,
    default: bool,
) -> xai_grok_sampling_types::ReasoningEffortOption {
    let id = value.as_str().to_owned();
    xai_grok_sampling_types::ReasoningEffortOption {
        label: match value {
            ReasoningEffort::None => "None".to_owned(),
            ReasoningEffort::Low => "Low".to_owned(),
            ReasoningEffort::High => "High".to_owned(),
            ReasoningEffort::Max => "Max".to_owned(),
            other => other.as_str().to_owned(),
        },
        id,
        value,
        description: Some(description.to_owned()),
        default,
    }
}

fn curated_reasoning_efforts(
    curated: &CuratedDeepSeekModel,
) -> Vec<xai_grok_sampling_types::ReasoningEffortOption> {
    match curated.api_backend {
        // Responses documents none/low/high/max for V4 Flash thinking control.
        ApiBackend::Responses => vec![
            reasoning_effort_option(ReasoningEffort::None, "Disable thinking mode", false),
            reasoning_effort_option(
                ReasoningEffort::Low,
                "Faster responses with lighter reasoning",
                false,
            ),
            reasoning_effort_option(
                ReasoningEffort::High,
                "Default DeepSeek reasoning depth for agentic work",
                true,
            ),
            reasoning_effort_option(
                ReasoningEffort::Max,
                "Maximum reasoning depth for the hardest problems",
                false,
            ),
        ],
        // Chat Completions uses reasoning_effort with thinking enabled by default.
        ApiBackend::ChatCompletions => vec![
            reasoning_effort_option(
                ReasoningEffort::Low,
                "Faster responses with lighter reasoning",
                false,
            ),
            reasoning_effort_option(
                ReasoningEffort::High,
                "Default DeepSeek reasoning depth for agentic work",
                true,
            ),
            reasoning_effort_option(
                ReasoningEffort::Max,
                "Maximum reasoning depth for the hardest problems",
                false,
            ),
        ],
        ApiBackend::Messages => Vec::new(),
    }
}

fn curated_model_entry(curated: &CuratedDeepSeekModel, base_url: &str) -> ModelEntry {
    let mut info = ModelInfo::fallback(curated.key);
    info.id = Some(curated.key.to_owned());
    info.model = curated.slug.to_owned();
    info.base_url = base_url.trim_end_matches('/').to_owned();
    info.name = Some(curated.name.to_owned());
    info.description = Some(curated.description.to_owned());
    info.api_backend = curated.api_backend;
    info.provider = ModelProvider::DeepSeek;
    info.tool_mode = Some(ToolMode::Direct);
    info.context_window =
        NonZeroU64::new(curated.context_window).expect("non-zero DeepSeek context window");
    info.reasoning_efforts = curated_reasoning_efforts(curated);
    info.supports_reasoning_effort = !info.reasoning_efforts.is_empty();
    info.reasoning_effort = info
        .reasoning_efforts
        .iter()
        .find(|opt| opt.default)
        .or_else(|| info.reasoning_efforts.first())
        .map(|opt| opt.value);
    info.supported_in_api = true;
    ModelEntry {
        info,
        api_key: None,
        env_key: Some(EnvKeys::single(DEEPSEEK_API_KEY_ENV)),
        auth_provider: None,
        api_base_url: None,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DeepSeekModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    credential_fingerprint: String,
}

impl DeepSeekModelsCatalog {
    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn is_authoritative(&self) -> bool {
        !self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DeepSeekModelsClient {
    http: reqwest::Client,
    base_url: String,
}

impl DeepSeekModelsClient {
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

    pub(crate) async fn query(&self) -> anyhow::Result<Option<DeepSeekModelsCatalog>> {
        let Some(api_key) = api_key_for_base_url(&self.base_url) else {
            return Ok(None);
        };
        self.query_with_key(&api_key).await.map(Some)
    }

    pub(crate) fn has_usable_api_key(&self) -> bool {
        api_key_for_base_url(&self.base_url).is_some()
    }

    pub(crate) fn catalog_matches_current_credential(
        &self,
        catalog: &DeepSeekModelsCatalog,
    ) -> bool {
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|fingerprint| fingerprint == catalog.credential_fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<DeepSeekModelsCatalog> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .timeout(DEEPSEEK_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .send()
            .await
            .with_context(|| format!("DeepSeek models request to {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "DeepSeek models request returned {status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }
        let wire: DeepSeekModelsResponse = response
            .json()
            .await
            .context("DeepSeek models response was invalid")?;
        Ok(self.catalog_from_wire(wire, api_key))
    }

    fn catalog_from_wire(
        &self,
        wire: DeepSeekModelsResponse,
        api_key: &str,
    ) -> DeepSeekModelsCatalog {
        let available = wire
            .data
            .into_iter()
            .map(|model| model.id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect::<std::collections::HashSet<_>>();
        let entries = CURATED_DEEPSEEK_MODELS
            .iter()
            .filter(|curated| available.contains(curated.slug))
            .map(|curated| {
                (
                    curated.key.to_owned(),
                    curated_model_entry(curated, &self.base_url),
                )
            })
            .collect();
        DeepSeekModelsCatalog {
            entries,
            credential_fingerprint: credential_fingerprint(api_key),
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
struct DeepSeekModelsResponse {
    #[serde(default)]
    data: Vec<DeepSeekWireModel>,
}

#[derive(Debug, Deserialize)]
struct DeepSeekWireModel {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn trusted_hosts_are_provider_scoped() {
        assert!(is_trusted_api_base_url(DEEPSEEK_API_BASE_URL));
        assert!(is_trusted_api_base_url("https://api.deepseek.com/v1"));
        assert!(!is_trusted_api_base_url("http://api.deepseek.com"));
        assert!(!is_trusted_api_base_url("https://api.x.ai/v1"));
        assert!(!is_trusted_api_base_url("https://deepseek.example/v1"));
    }

    #[test]
    fn stored_keys_never_leave_owned_hosts() {
        let stored = Some("deepseek-stored-secret".to_owned());
        assert_eq!(
            select_api_key(DEEPSEEK_API_BASE_URL, None, stored.clone()).as_deref(),
            Some("deepseek-stored-secret")
        );
        assert_eq!(
            select_api_key("https://proxy.example/v1", None, stored),
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
    fn wire_catalog_routes_flash_to_responses_and_pro_to_chat() {
        let client = DeepSeekModelsClient::with_base_url(DEEPSEEK_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            DeepSeekModelsResponse {
                data: vec![
                    DeepSeekWireModel {
                        id: "deepseek-v4-pro".to_owned(),
                    },
                    DeepSeekWireModel {
                        id: "deepseek-v4-flash".to_owned(),
                    },
                    DeepSeekWireModel {
                        id: "future-unknown".to_owned(),
                    },
                ],
            },
            "catalog-key",
        );
        let entries = catalog.entries();
        assert_eq!(entries.len(), 2);

        let pro = &entries["deepseek:deepseek-v4-pro"];
        assert_eq!(pro.info.model, "deepseek-v4-pro");
        assert_eq!(pro.info.provider, ModelProvider::DeepSeek);
        assert_eq!(pro.info.api_backend, ApiBackend::ChatCompletions);
        assert_eq!(pro.info.tool_mode, Some(ToolMode::Direct));
        assert_eq!(pro.info.context_window.get(), 1_000_000);
        assert!(pro.info.supports_reasoning_effort);
        assert_eq!(pro.info.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(
            pro.env_key.as_ref().and_then(EnvKeys::primary),
            Some(DEEPSEEK_API_KEY_ENV)
        );

        let flash = &entries["deepseek:deepseek-v4-flash"];
        assert_eq!(flash.info.model, "deepseek-v4-flash");
        assert_eq!(flash.info.provider, ModelProvider::DeepSeek);
        assert_eq!(flash.info.api_backend, ApiBackend::Responses);
        assert!(flash.info.supports_reasoning_effort);
        assert_eq!(flash.info.reasoning_effort, Some(ReasoningEffort::High));
        assert!(
            flash
                .info
                .reasoning_efforts
                .iter()
                .any(|opt| opt.value == ReasoningEffort::None)
        );
        assert!(
            flash
                .info
                .description
                .as_deref()
                .is_some_and(|text| text.contains("0731") && text.contains("Responses"))
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
                "data": [{"id": "deepseek-v4-flash"}]
            }))
        }

        let capture = RequestCapture::default();
        let app = Router::new()
            .route("/models", get(models))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = DeepSeekModelsClient::with_base_url(format!("http://{address}"));
        let catalog = client.query_with_key("model-query-canary").await.unwrap();
        let flash = &catalog.entries()["deepseek:deepseek-v4-flash"];
        assert_eq!(flash.info.api_backend, ApiBackend::Responses);
        assert_eq!(
            capture.0.lock().unwrap().as_deref(),
            Some("Bearer model-query-canary")
        );
    }
}
