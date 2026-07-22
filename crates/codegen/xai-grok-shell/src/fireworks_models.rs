//! Provider-isolated Fireworks AI model discovery.
//!
//! Fireworks serves an OpenAI-compatible Chat Completions API. Open Grok shows
//! a curated model list for the provider; the `/models` endpoint is queried
//! only to enrich curated entries (context window) and never adds models. The
//! Fireworks credential must never pass through xAI's `/v1/models` client.

use crate::agent::config::{EnvKeys, ModelEntry, ModelInfo};
use anyhow::{Context, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::num::NonZeroU64;
use std::time::Duration;
use url::Url;
use xai_grok_sampling_types::{ApiBackend, ModelProvider, ToolMode};

pub const FIREWORKS_API_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";
pub const FIREWORKS_API_BASE_URL_ENV: &str = "OPENGROK_FIREWORKS_API_BASE_URL";
pub const FIREWORKS_API_KEY_ENV: &str = "FIREWORKS_API_KEY";
const FIREWORKS_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One curated Fireworks catalog entry. The key is the stable catalog id shown
/// to users; the slug is the Fireworks routing path sent on the wire.
#[derive(Clone, Copy, Debug)]
pub struct CuratedFireworksModel {
    pub key: &'static str,
    pub slug: &'static str,
    pub name: &'static str,
    pub description: &'static str,
    pub fallback_context_window: u64,
}

/// The only Fireworks models Open Grok exposes. A `/models` response may
/// enrich these entries but can neither add nor remove them.
pub const CURATED_FIREWORKS_MODELS: [CuratedFireworksModel; 4] = [
    CuratedFireworksModel {
        key: "glm-5.2",
        slug: "accounts/fireworks/models/glm-5p2",
        name: "GLM 5.2",
        description: "Zhipu's GLM 5.2 frontier model on Fireworks AI",
        fallback_context_window: 1_040_000,
    },
    CuratedFireworksModel {
        key: "glm-5.2-fast",
        slug: "accounts/fireworks/routers/glm-5p2-fast",
        name: "GLM 5.2 Fast",
        description: "GLM 5.2 on Fireworks AI's low-latency router",
        fallback_context_window: 1_040_000,
    },
    CuratedFireworksModel {
        key: "deepseek-v4-pro",
        slug: "accounts/fireworks/models/deepseek-v4-pro",
        name: "DeepSeek V4 Pro",
        description: "DeepSeek V4 Pro on Fireworks AI",
        fallback_context_window: 1_040_000,
    },
    CuratedFireworksModel {
        key: "kimi-k2.7-code",
        slug: "accounts/fireworks/models/kimi-k2p7-code",
        name: "Kimi K2.7 Code",
        description: "Moonshot's Kimi K2.7 coding model on Fireworks AI",
        fallback_context_window: 262_144,
    },
];

/// Only provider-owned hosts may receive the application-stored Fireworks
/// key. User-configured proxy endpoints remain possible with an explicit
/// per-model `api_key`/`env_key`, whose disclosure is then an intentional
/// BYOK choice.
pub fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https" && url.host_str() == Some("api.fireworks.ai")
}

pub fn api_base_url() -> String {
    std::env::var(FIREWORKS_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| FIREWORKS_API_BASE_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    std::env::var(FIREWORKS_API_KEY_ENV)
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
        ModelProvider::Fireworks,
    )
}

/// Resolve the credential that may be sent to `base_url`.
///
/// An environment key is an explicit process-level BYOK choice and may be
/// used with the process's explicit endpoint override. The key saved through
/// the UI is provider-owned, so it fails closed unless the destination is an
/// official Fireworks host.
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

fn curated_model_entry(
    curated: &CuratedFireworksModel,
    base_url: &str,
    context_window: Option<u64>,
) -> ModelEntry {
    let mut info = ModelInfo::fallback(curated.key);
    info.id = Some(curated.key.to_owned());
    info.model = curated.slug.to_owned();
    info.base_url = base_url.trim_end_matches('/').to_owned();
    info.name = Some(curated.name.to_owned());
    info.description = Some(curated.description.to_owned());
    info.api_backend = ApiBackend::ChatCompletions;
    info.provider = ModelProvider::Fireworks;
    info.tool_mode = Some(ToolMode::Direct);
    info.context_window = context_window
        .and_then(NonZeroU64::new)
        .or_else(|| NonZeroU64::new(curated.fallback_context_window))
        .expect("non-zero Fireworks fallback context window");
    info.supports_reasoning_effort = false;
    info.reasoning_efforts.clear();
    info.reasoning_effort = None;
    info.supported_in_api = true;
    ModelEntry {
        info,
        api_key: None,
        env_key: Some(EnvKeys::single(FIREWORKS_API_KEY_ENV)),
        auth_provider: None,
        api_base_url: None,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FireworksModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    credential_fingerprint: String,
}

impl FireworksModelsCatalog {
    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn is_authoritative(&self) -> bool {
        !self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FireworksModelsClient {
    http: reqwest::Client,
    base_url: String,
}

impl FireworksModelsClient {
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

    pub(crate) async fn query(&self) -> anyhow::Result<Option<FireworksModelsCatalog>> {
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
        catalog: &FireworksModelsCatalog,
    ) -> bool {
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|fingerprint| fingerprint == catalog.credential_fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<FireworksModelsCatalog> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .timeout(FIREWORKS_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .send()
            .await
            .with_context(|| format!("Fireworks models request to {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Fireworks models request returned {status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }
        let wire: FireworksModelsResponse = response
            .json()
            .await
            .context("Fireworks models response was invalid")?;
        Ok(self.catalog_from_wire(wire, api_key))
    }

    /// Project the wire response onto the curated list: every curated model is
    /// always present; wire metadata only enriches its context window.
    fn catalog_from_wire(
        &self,
        wire: FireworksModelsResponse,
        api_key: &str,
    ) -> FireworksModelsCatalog {
        let context_lengths: IndexMap<String, u64> = wire
            .data
            .into_iter()
            .filter_map(|model| {
                let id = model.id.trim().to_owned();
                let context_length = model.context_length?;
                (!id.is_empty()).then_some((id, context_length))
            })
            .collect();
        let entries = CURATED_FIREWORKS_MODELS
            .iter()
            .map(|curated| {
                (
                    curated.key.to_owned(),
                    curated_model_entry(
                        curated,
                        &self.base_url,
                        context_lengths.get(curated.slug).copied(),
                    ),
                )
            })
            .collect();
        FireworksModelsCatalog {
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
struct FireworksModelsResponse {
    #[serde(default)]
    data: Vec<FireworksWireModel>,
}

#[derive(Debug, Deserialize)]
struct FireworksWireModel {
    id: String,
    #[serde(default)]
    context_length: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn trusted_hosts_are_provider_scoped() {
        assert!(is_trusted_api_base_url(FIREWORKS_API_BASE_URL));
        assert!(is_trusted_api_base_url(
            "https://api.fireworks.ai/inference/v1/models"
        ));
        assert!(!is_trusted_api_base_url(
            "http://api.fireworks.ai/inference/v1"
        ));
        assert!(!is_trusted_api_base_url("https://api.x.ai/v1"));
        assert!(!is_trusted_api_base_url(
            "https://fireworks.example/inference/v1"
        ));
        assert!(!is_trusted_api_base_url("https://api.moonshot.ai/v1"));
    }

    #[test]
    fn stored_keys_never_leave_owned_hosts() {
        let stored = Some("fireworks-stored-secret".to_owned());
        assert_eq!(
            select_api_key(FIREWORKS_API_BASE_URL, None, stored.clone()).as_deref(),
            Some("fireworks-stored-secret")
        );
        assert_eq!(
            select_api_key("https://proxy.example/v1", None, stored),
            None,
            "a UI-stored key must not authenticate an unrecognized proxy"
        );
        assert_eq!(
            select_api_key(
                "https://proxy.example/v1",
                Some("explicit-environment-secret".to_owned()),
                None,
            )
            .as_deref(),
            Some("explicit-environment-secret"),
            "an environment key is an explicit process-level BYOK choice"
        );
    }

    #[test]
    fn curated_entries_are_chat_only_with_provider_owned_credentials() {
        let client = FireworksModelsClient::with_base_url(FIREWORKS_API_BASE_URL);
        let catalog =
            client.catalog_from_wire(FireworksModelsResponse { data: Vec::new() }, "catalog-key");
        let entries = catalog.entries();
        assert_eq!(entries.len(), CURATED_FIREWORKS_MODELS.len());
        for curated in &CURATED_FIREWORKS_MODELS {
            let entry = entries.get(curated.key).expect("curated Fireworks entry");
            assert_eq!(entry.info.provider, ModelProvider::Fireworks);
            assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
            assert_eq!(entry.info.tool_mode, Some(ToolMode::Direct));
            assert_eq!(entry.info.model, curated.slug);
            assert_eq!(entry.info.name.as_deref(), Some(curated.name));
            assert_eq!(
                entry.info.context_window.get(),
                curated.fallback_context_window
            );
            assert!(!entry.info.supports_reasoning_effort);
            assert!(entry.info.reasoning_efforts.is_empty());
            assert_eq!(
                entry.env_key.as_ref().and_then(EnvKeys::primary),
                Some(FIREWORKS_API_KEY_ENV)
            );
        }
    }

    #[test]
    fn wire_metadata_enriches_but_cannot_add_models() {
        let client = FireworksModelsClient::with_base_url(FIREWORKS_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            FireworksModelsResponse {
                data: vec![
                    FireworksWireModel {
                        id: "accounts/fireworks/models/glm-5p2".to_owned(),
                        context_length: Some(262_144),
                    },
                    FireworksWireModel {
                        id: "accounts/fireworks/models/uncurated-model".to_owned(),
                        context_length: Some(8_192),
                    },
                ],
            },
            "catalog-key",
        );
        let entries = catalog.entries();
        assert_eq!(entries["glm-5.2"].info.context_window.get(), 262_144);
        assert_eq!(
            entries.len(),
            CURATED_FIREWORKS_MODELS.len(),
            "uncurated wire models must not appear in the catalog"
        );
        assert!(!entries.contains_key("accounts/fireworks/models/uncurated-model"));
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
    async fn model_query_uses_bearer_auth_and_preserves_context_length() {
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
                "data": [{
                    "id": "accounts/fireworks/models/deepseek-v4-pro",
                    "context_length": 1_048_576
                }]
            }))
        }

        let capture = RequestCapture::default();
        let app = Router::new()
            .route("/inference/v1/models", get(models))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = FireworksModelsClient::with_base_url(format!("http://{address}/inference/v1"));
        let catalog = client.query_with_key("model-query-canary").await.unwrap();
        let entries = catalog.entries();
        assert_eq!(
            entries["deepseek-v4-pro"].info.context_window.get(),
            1_048_576
        );
        assert_eq!(
            entries["deepseek-v4-pro"].info.provider,
            ModelProvider::Fireworks
        );
        assert_eq!(entries.len(), CURATED_FIREWORKS_MODELS.len());
        assert_eq!(
            capture.0.lock().unwrap().as_deref(),
            Some("Bearer model-query-canary")
        );
    }
}
