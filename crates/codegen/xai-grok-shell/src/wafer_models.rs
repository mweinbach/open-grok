//! Provider-isolated Wafer AI model discovery.
//!
//! Wafer exposes an OpenAI-compatible Chat Completions API. Its `/models`
//! response is authoritative, so the catalog contains only model ids returned
//! by Wafer and never invents a static model list.

use crate::agent::config::{EnvKeys, ModelEntry, ModelInfo};
use anyhow::{Context, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::num::NonZeroU64;
use std::time::Duration;
use url::Url;
use xai_grok_sampling_types::{ApiBackend, ModelProvider, ToolMode};

pub const WAFER_API_BASE_URL: &str = "https://pass.wafer.ai/v1";
pub const WAFER_API_BASE_URL_ENV: &str = "OPENGROK_WAFER_API_BASE_URL";
pub const WAFER_API_KEY_ENV: &str = "WAFER_API_KEY";
const WAFER_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https" && url.host_str() == Some("pass.wafer.ai")
}

pub fn api_base_url() -> String {
    std::env::var(WAFER_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| WAFER_API_BASE_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    std::env::var(WAFER_API_KEY_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn environment_api_key_is_configured() -> bool {
    environment_api_key().is_some()
}

fn stored_api_key() -> Option<String> {
    crate::auth::read_provider_api_key(&crate::util::grok_home::grok_home(), ModelProvider::Wafer)
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

/// Wafer `/models` returns ids only. Assign published windows by normalized
/// id (case and punctuation ignored), most-specific families first.
fn curated_context_window(model_id: &str) -> Option<u64> {
    let id: String = model_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    if id.contains("kimik3") || id.contains("deepseekv4flash") || id.contains("glm52") {
        return Some(1_000_000);
    }
    if id.contains("kimik26") {
        return Some(262_144);
    }
    if id.contains("glm51") {
        return Some(203_000);
    }
    None
}

fn model_entry(model_id: &str, base_url: &str) -> ModelEntry {
    let key = format!("wafer:{model_id}");
    let mut info = ModelInfo::fallback(&key);
    info.id = Some(key);
    info.model = model_id.to_owned();
    info.base_url = base_url.trim_end_matches('/').to_owned();
    info.name = Some(model_id.to_owned());
    info.api_backend = ApiBackend::ChatCompletions;
    info.provider = ModelProvider::Wafer;
    info.tool_mode = Some(ToolMode::Direct);
    info.supports_reasoning_effort = false;
    info.reasoning_efforts.clear();
    info.reasoning_effort = None;
    info.supports_backend_search = false;
    info.supports_standalone_web_search = Some(false);
    info.supported_in_api = true;
    if let Some(window) = curated_context_window(model_id) {
        info.context_window = NonZeroU64::new(window).expect("non-zero Wafer context window");
    }
    ModelEntry {
        info,
        api_key: None,
        env_key: Some(EnvKeys::single(WAFER_API_KEY_ENV)),
        auth_provider: None,
        api_base_url: None,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WaferModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    credential_fingerprint: String,
}

impl WaferModelsCatalog {
    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn is_authoritative(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WaferModelsClient {
    http: reqwest::Client,
    base_url: String,
}

impl WaferModelsClient {
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

    pub(crate) async fn query(&self) -> anyhow::Result<Option<WaferModelsCatalog>> {
        let Some(api_key) = api_key_for_base_url(&self.base_url) else {
            return Ok(None);
        };
        self.query_with_key(&api_key).await.map(Some)
    }

    pub(crate) fn has_usable_api_key(&self) -> bool {
        api_key_for_base_url(&self.base_url).is_some()
    }

    pub(crate) fn catalog_matches_current_credential(&self, catalog: &WaferModelsCatalog) -> bool {
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|fingerprint| fingerprint == catalog.credential_fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<WaferModelsCatalog> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .timeout(WAFER_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .send()
            .await
            .with_context(|| format!("Wafer models request to {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Wafer models request returned {status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }
        let wire: WaferModelsResponse = response
            .json()
            .await
            .context("Wafer models response was invalid")?;
        Ok(self.catalog_from_wire(wire, api_key))
    }

    fn catalog_from_wire(&self, wire: WaferModelsResponse, api_key: &str) -> WaferModelsCatalog {
        let entries = wire
            .data
            .into_iter()
            .map(|model| model.id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .map(|id| {
                let key = format!("wafer:{id}");
                (key, model_entry(&id, &self.base_url))
            })
            .collect();
        WaferModelsCatalog {
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
struct WaferModelsResponse {
    #[serde(default)]
    data: Vec<WaferWireModel>,
}

#[derive(Debug, Deserialize)]
struct WaferWireModel {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn trusted_hosts_are_provider_scoped() {
        assert!(is_trusted_api_base_url(WAFER_API_BASE_URL));
        assert!(is_trusted_api_base_url("https://pass.wafer.ai/v1/models"));
        assert!(!is_trusted_api_base_url("http://pass.wafer.ai/v1"));
        assert!(!is_trusted_api_base_url("https://pass.wafer.ai.example/v1"));
        assert!(!is_trusted_api_base_url("https://api.x.ai/v1"));
    }

    #[test]
    fn stored_keys_never_leave_owned_hosts() {
        let stored = Some("wafer-stored-secret".to_owned());
        assert_eq!(
            select_api_key(WAFER_API_BASE_URL, None, stored.clone()).as_deref(),
            Some("wafer-stored-secret")
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
    fn wire_catalog_uses_only_returned_ids_and_chat_completions() {
        let client = WaferModelsClient::with_base_url(WAFER_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            WaferModelsResponse {
                data: vec![
                    WaferWireModel {
                        id: " wafer-model ".to_owned(),
                    },
                    WaferWireModel { id: "".to_owned() },
                    WaferWireModel {
                        id: "future-model".to_owned(),
                    },
                ],
            },
            "catalog-key",
        );
        let entries = catalog.entries();
        assert_eq!(entries.len(), 2);
        for (key, entry) in entries {
            assert!(key.starts_with("wafer:"));
            assert_eq!(entry.info.provider, ModelProvider::Wafer);
            assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
            assert_eq!(entry.info.tool_mode, Some(ToolMode::Direct));
            assert!(!entry.info.supports_backend_search);
            assert_eq!(entry.info.supports_standalone_web_search, Some(false));
            assert_eq!(entry.info.context_window.get(), 200_000);
            assert_eq!(
                entry.env_key.as_ref().and_then(EnvKeys::primary),
                Some(WAFER_API_KEY_ENV)
            );
        }
    }

    #[test]
    fn curated_windows_match_published_wafer_lineup() {
        let cases = [
            ("Kimi-K3", 1_000_000),
            ("kimi-k3-fast", 1_000_000),
            ("Kimi-K3-Fast", 1_000_000),
            ("DeepSeek-V4-Flash-0731-Fast", 1_000_000),
            ("deepseek-v4-flash", 1_000_000),
            ("GLM-5.2", 1_000_000),
            ("glm-5.2-flash", 1_000_000),
            ("glm5.2-fast", 1_000_000),
            ("GLM5.2-Fast", 1_000_000),
            ("Kimi-K2.6", 262_144),
            ("kimi-k2.6", 262_144),
            ("GLM-5.1", 203_000),
            ("glm-5.1", 203_000),
            ("MiniMax-M3", 200_000),
            ("future-model", 200_000),
        ];
        for (id, window) in cases {
            assert_eq!(
                model_entry(id, WAFER_API_BASE_URL)
                    .info
                    .context_window
                    .get(),
                window,
                "{id}"
            );
        }
        assert_eq!(curated_context_window("kimi-k2.6"), Some(262_144));
        assert_ne!(curated_context_window("kimi-k2.6"), Some(1_000_000));
    }

    #[test]
    fn wire_catalog_applies_curated_windows() {
        let client = WaferModelsClient::with_base_url(WAFER_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            WaferModelsResponse {
                data: vec![
                    WaferWireModel {
                        id: "Kimi-K3".to_owned(),
                    },
                    WaferWireModel {
                        id: "Kimi-K2.6".to_owned(),
                    },
                    WaferWireModel {
                        id: "GLM-5.1".to_owned(),
                    },
                ],
            },
            "catalog-key",
        );
        let entries = catalog.entries();
        assert_eq!(
            entries["wafer:Kimi-K3"].info.context_window.get(),
            1_000_000
        );
        assert_eq!(
            entries["wafer:Kimi-K2.6"].info.context_window.get(),
            262_144
        );
        assert_eq!(entries["wafer:GLM-5.1"].info.context_window.get(), 203_000);
    }

    #[test]
    fn errors_redact_credentials() {
        assert_eq!(
            safe_error_excerpt("invalid wafer-secret\nretry", "wafer-secret"),
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
                "data": [{"id": "wafer-model"}]
            }))
        }

        let capture = RequestCapture::default();
        let app = Router::new()
            .route("/v1/models", get(models))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = WaferModelsClient::with_base_url(format!("http://{address}/v1"));
        let catalog = client
            .query_with_key("test-wafer-key")
            .await
            .expect("model query");
        assert_eq!(
            capture.0.lock().expect("capture lock").as_deref(),
            Some("Bearer test-wafer-key")
        );
        assert!(catalog.entries().contains_key("wafer:wafer-model"));
    }
}
