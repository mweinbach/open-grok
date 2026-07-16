//! Provider-isolated Kimi model discovery.
//!
//! Kimi's public API is OpenAI-compatible Chat Completions, but its model
//! catalog and credentials must never pass through xAI's `/v1/models` client.

use crate::agent::config::{EnvKeys, ModelEntry, ModelInfo};
use anyhow::{Context, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::num::NonZeroU64;
use std::time::Duration;
use url::Url;
use xai_grok_sampling_types::{ApiBackend, ModelProvider, ToolMode};

pub(crate) const KIMI_API_BASE_URL: &str = "https://api.moonshot.ai/v1";
pub(crate) const KIMI_API_BASE_URL_ENV: &str = "OPENGROK_KIMI_API_BASE_URL";
pub(crate) const KIMI_API_KEY_ENV: &str = "MOONSHOT_API_KEY";
const KIMI_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Only provider-owned hosts may receive the application-stored Kimi key.
/// User-configured proxy endpoints remain possible with an explicit per-model
/// `api_key`/`env_key`, whose disclosure is then an intentional BYOK choice.
pub(crate) fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("api.moonshot.ai" | "api.moonshot.cn" | "api.kimi.com" | "api.kimi.ai")
        )
}

pub(crate) fn api_base_url() -> String {
    std::env::var(KIMI_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| KIMI_API_BASE_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    std::env::var(KIMI_API_KEY_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn stored_api_key() -> Option<String> {
    crate::auth::read_provider_api_key(&crate::util::grok_home::grok_home(), ModelProvider::Kimi)
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

/// Resolve the credential that may be sent to `base_url`.
///
/// An environment key is an explicit process-level BYOK choice and may be used
/// with the process's explicit endpoint override. The key saved through the UI
/// is provider-owned, so it fails closed unless the destination is an official
/// Kimi/Moonshot host.
fn api_key_for_base_url(base_url: &str) -> Option<String> {
    select_api_key(base_url, environment_api_key(), stored_api_key())
}

fn credential_fingerprint(api_key: &str) -> String {
    blake3::hash(api_key.as_bytes()).to_hex().to_string()
}

#[derive(Clone, Debug)]
pub(crate) struct KimiModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    credential_fingerprint: String,
}

impl KimiModelsCatalog {
    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn is_authoritative(&self) -> bool {
        !self.entries.is_empty()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct KimiModelsClient {
    http: reqwest::Client,
    base_url: String,
}

impl KimiModelsClient {
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

    pub(crate) async fn query(&self) -> anyhow::Result<Option<KimiModelsCatalog>> {
        let Some(api_key) = api_key_for_base_url(&self.base_url) else {
            return Ok(None);
        };
        self.query_with_key(&api_key).await.map(Some)
    }

    pub(crate) fn has_usable_api_key(&self) -> bool {
        api_key_for_base_url(&self.base_url).is_some()
    }

    pub(crate) fn catalog_matches_current_credential(&self, catalog: &KimiModelsCatalog) -> bool {
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|fingerprint| fingerprint == catalog.credential_fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<KimiModelsCatalog> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .timeout(KIMI_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .send()
            .await
            .with_context(|| format!("Kimi models request to {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Kimi models request returned {status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }
        let wire: KimiModelsResponse = response
            .json()
            .await
            .context("Kimi models response was invalid")?;
        let entries = wire
            .data
            .into_iter()
            .filter_map(|model| self.convert_model(model))
            .collect();
        Ok(KimiModelsCatalog {
            entries,
            credential_fingerprint: credential_fingerprint(api_key),
        })
    }

    fn convert_model(&self, wire: KimiWireModel) -> Option<(String, ModelEntry)> {
        let id = wire.id.trim();
        if id.is_empty() {
            tracing::warn!("Kimi models response contained an empty id; skipping entry");
            return None;
        }
        let mut info = ModelInfo::fallback(id);
        info.id = Some(id.to_owned());
        info.model = id.to_owned();
        info.base_url = self.base_url.trim_end_matches('/').to_owned();
        info.name = Some(id.to_owned());
        info.api_backend = ApiBackend::ChatCompletions;
        info.provider = ModelProvider::Kimi;
        info.tool_mode = Some(ToolMode::Direct);
        info.context_window = wire
            .context_length
            .and_then(NonZeroU64::new)
            .unwrap_or_else(|| NonZeroU64::new(256_000).expect("non-zero Kimi fallback"));
        // `supports_reasoning` means the model can reason, not that it accepts
        // the repo's adjustable `reasoning_effort` field. Embedded donor
        // metadata supplies the one known K3 `max` option during catalog merge.
        info.supports_reasoning_effort = false;
        info.reasoning_efforts.clear();
        info.reasoning_effort = None;
        info.supported_in_api = true;
        if wire.supports_reasoning == Some(true) {
            info.description = Some("Kimi reasoning model".to_owned());
        }
        Some((
            id.to_owned(),
            ModelEntry {
                info,
                api_key: None,
                env_key: Some(EnvKeys::single(KIMI_API_KEY_ENV)),
                api_base_url: None,
            },
        ))
    }
}

fn safe_error_excerpt(body: &str, api_key: &str) -> String {
    let sanitized = body
        .replace(api_key, "[REDACTED]")
        .replace(['\r', '\n'], " ");
    sanitized.chars().take(512).collect()
}

#[derive(Debug, Deserialize)]
struct KimiModelsResponse {
    #[serde(default)]
    data: Vec<KimiWireModel>,
}

#[derive(Debug, Deserialize)]
struct KimiWireModel {
    id: String,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    supports_reasoning: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn trusted_hosts_are_provider_scoped() {
        assert!(is_trusted_api_base_url("https://api.moonshot.ai/v1"));
        assert!(is_trusted_api_base_url("https://api.moonshot.cn/v1"));
        assert!(is_trusted_api_base_url("https://api.kimi.com/coding/v1"));
        assert!(is_trusted_api_base_url("https://api.kimi.ai/coding/v1"));
        assert!(!is_trusted_api_base_url("https://api.x.ai/v1"));
        assert!(!is_trusted_api_base_url("https://moonshot.example/v1"));
        assert!(!is_trusted_api_base_url("http://api.moonshot.ai/v1"));
    }

    #[test]
    fn model_conversion_is_chat_only_and_does_not_invent_effort_support() {
        let client = KimiModelsClient::with_base_url(KIMI_API_BASE_URL);
        let (id, entry) = client
            .convert_model(KimiWireModel {
                id: "kimi-k3".to_owned(),
                context_length: Some(1_048_576),
                supports_reasoning: Some(true),
            })
            .unwrap();
        assert_eq!(id, "kimi-k3");
        assert_eq!(entry.info.provider, ModelProvider::Kimi);
        assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
        assert_eq!(entry.info.tool_mode, Some(ToolMode::Direct));
        assert_eq!(entry.info.context_window.get(), 1_048_576);
        assert!(!entry.info.supports_reasoning_effort);
        assert!(entry.info.reasoning_efforts.is_empty());
        assert_eq!(entry.env_key.unwrap().primary(), Some(KIMI_API_KEY_ENV));
    }

    #[test]
    fn stored_key_never_leaves_provider_owned_hosts() {
        let stored = Some("stored-secret".to_owned());
        assert_eq!(
            select_api_key(KIMI_API_BASE_URL, None, stored.clone()).as_deref(),
            Some("stored-secret")
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
                    "id": "kimi-k3",
                    "context_length": 1_048_576,
                    "supports_reasoning": true
                }]
            }))
        }

        let capture = RequestCapture::default();
        let app = Router::new()
            .route("/v1/models", get(models))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = KimiModelsClient::with_base_url(format!("http://{address}/v1"));
        let catalog = client.query_with_key("model-query-canary").await.unwrap();
        let entries = catalog.entries();
        assert_eq!(entries["kimi-k3"].info.context_window.get(), 1_048_576);
        assert_eq!(entries["kimi-k3"].info.provider, ModelProvider::Kimi);
        assert_eq!(
            capture.0.lock().unwrap().as_deref(),
            Some("Bearer model-query-canary")
        );
    }
}
