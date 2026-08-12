//! Provider-isolated Z AI model discovery.
//!
//! Z AI exposes an OpenAI-compatible Chat Completions API (GLM models). The
//! default base URL targets the GLM Coding Plan endpoint; a standard API
//! endpoint and any other host may be selected via the
//! `OPENGROK_ZAI_API_BASE_URL` environment variable.
//!
//! The `/models` response is authoritative when available, so a successful
//! query replaces the catalog with only the ids Z AI returned. When `/models`
//! is unreachable or returns nothing, a curated static fallback list keeps
//! the model picker populated.

use crate::agent::config::{EnvKeys, ModelEntry, ModelInfo};
use anyhow::{Context, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::time::Duration;
use url::Url;
use xai_grok_sampling_types::{ApiBackend, ModelProvider, ToolMode};

/// Default base URL: the GLM Coding Plan OpenAI-compatible endpoint.
pub const ZAI_API_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";
pub const ZAI_API_BASE_URL_ENV: &str = "OPENGROK_ZAI_API_BASE_URL";
pub const ZAI_API_KEY_ENV: &str = "ZAI_API_KEY";
const ZAI_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Models known to support Z AI's "thinking mode" / reasoning. The dynamic
/// `/models` response carries no capability flag, so reasoning exposure is
/// decided from this curated set by model id (case-insensitive prefix match).
const KNOWN_REASONING_MODEL_PREFIXES: &[&str] =
    &["glm-4.5", "glm-4.6", "glm-4.7", "glm-4-32b", "glm-5"];

/// Curated fallback model ids used when `/models` fails or returns nothing.
/// Kept current with Z AI's published GLM text model lineup; the dynamic
/// catalog overrides this once a query succeeds.
const FALLBACK_MODEL_IDS: &[&str] = &[
    "glm-5.2",
    "glm-5-turbo",
    "glm-5.1",
    "glm-5",
    "glm-4.7",
    "glm-4.6",
    "glm-4.5",
    "glm-4-32b-0414-128k",
];

pub fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    // The Coding Plan (`/api/coding/paas/v4`) and standard API
    // (`/api/paas/v4`) both live on api.z.ai; both path prefixes are trusted.
    url.scheme() == "https" && url.host_str() == Some("api.z.ai")
}

pub fn api_base_url() -> String {
    std::env::var(ZAI_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ZAI_API_BASE_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    std::env::var(ZAI_API_KEY_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn environment_api_key_is_configured() -> bool {
    environment_api_key().is_some()
}

fn stored_api_key() -> Option<String> {
    crate::auth::read_provider_api_key(&crate::util::grok_home::grok_home(), ModelProvider::Zai)
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

/// Whether a model id is known to support reasoning ("thinking mode").
fn is_known_reasoning_model(model_id: &str) -> bool {
    let lower = model_id.to_ascii_lowercase();
    KNOWN_REASONING_MODEL_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

fn model_entry(model_id: &str, base_url: &str) -> ModelEntry {
    let key = format!("zai:{model_id}");
    let mut info = ModelInfo::fallback(&key);
    info.id = Some(key);
    info.model = model_id.to_owned();
    info.base_url = base_url.trim_end_matches('/').to_owned();
    info.name = Some(model_id.to_owned());
    info.api_backend = ApiBackend::ChatCompletions;
    info.provider = ModelProvider::Zai;
    info.tool_mode = Some(ToolMode::Direct);
    info.supports_reasoning_effort = is_known_reasoning_model(model_id);
    info.reasoning_effort = None;
    info.reasoning_efforts.clear();
    info.supports_backend_search = false;
    info.supports_standalone_web_search = Some(false);
    info.supported_in_api = true;
    ModelEntry {
        info,
        api_key: None,
        env_key: Some(EnvKeys::single(ZAI_API_KEY_ENV)),
        auth_provider: None,
        api_base_url: None,
    }
}

/// Curated fallback catalog used when `/models` is unavailable. Keeps the
/// picker populated with the published GLM lineup so users can select a model
/// before the first successful (or any) dynamic query.
fn fallback_entries(base_url: &str) -> IndexMap<String, ModelEntry> {
    FALLBACK_MODEL_IDS
        .iter()
        .map(|id| {
            let key = format!("zai:{id}");
            (key, model_entry(id, base_url))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct ZaiModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    credential_fingerprint: Option<String>,
}

impl ZaiModelsCatalog {
    fn dynamic(entries: IndexMap<String, ModelEntry>, api_key: &str) -> Self {
        Self {
            entries,
            credential_fingerprint: Some(credential_fingerprint(api_key)),
        }
    }

    fn fallback(base_url: &str) -> Self {
        Self {
            entries: fallback_entries(base_url),
            credential_fingerprint: None,
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
pub(crate) struct ZaiModelsClient {
    http: reqwest::Client,
    base_url: String,
}

impl ZaiModelsClient {
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

    /// Query the dynamic `/models` endpoint. Returns `Ok(None)` when no API
    /// key is available. On a fetch/parse failure, falls back to the curated
    /// catalog so the picker is never empty.
    pub(crate) async fn query(&self) -> anyhow::Result<Option<ZaiModelsCatalog>> {
        let Some(api_key) = api_key_for_base_url(&self.base_url) else {
            return Ok(None);
        };
        Ok(Some(self.query_with_fallback(&api_key).await))
    }

    /// Fetch the dynamic catalog with `api_key`, falling back to the curated
    /// catalog on any error so the model picker is never empty.
    async fn query_with_fallback(&self, api_key: &str) -> ZaiModelsCatalog {
        match self.query_with_key(api_key).await {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::warn!(
                    %error,
                    "Z AI models request failed; using curated fallback catalog"
                );
                ZaiModelsCatalog::fallback(&self.base_url)
            }
        }
    }

    pub(crate) fn has_usable_api_key(&self) -> bool {
        api_key_for_base_url(&self.base_url).is_some()
    }

    pub(crate) fn catalog_matches_current_credential(&self, catalog: &ZaiModelsCatalog) -> bool {
        // The curated fallback has no fingerprint and is always considered
        // stale relative to a live credential so a successful dynamic query
        // can replace it.
        let Some(fingerprint) = catalog.credential_fingerprint.as_ref() else {
            return false;
        };
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|current| &current == fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<ZaiModelsCatalog> {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .get(&url)
            .timeout(ZAI_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .send()
            .await
            .with_context(|| format!("Z AI models request to {url} failed"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "Z AI models request returned {status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }
        let wire: ZaiModelsResponse = response
            .json()
            .await
            .context("Z AI models response was invalid")?;
        Ok(self.catalog_from_wire(wire, api_key))
    }

    fn catalog_from_wire(&self, wire: ZaiModelsResponse, api_key: &str) -> ZaiModelsCatalog {
        let entries: IndexMap<String, ModelEntry> = wire
            .data
            .into_iter()
            .map(|model| model.id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .map(|id| {
                let key = format!("zai:{id}");
                (key, model_entry(&id, &self.base_url))
            })
            .collect();
        if entries.is_empty() {
            // Treat an empty but well-formed response as "no listing"; use the
            // fallback so the picker stays usable.
            ZaiModelsCatalog::fallback(&self.base_url)
        } else {
            ZaiModelsCatalog::dynamic(entries, api_key)
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
struct ZaiModelsResponse {
    #[serde(default)]
    data: Vec<ZaiWireModel>,
}

#[derive(Debug, Deserialize)]
struct ZaiWireModel {
    id: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, extract::State, http::HeaderMap, routing::get};
    use std::sync::{Arc, Mutex};

    #[test]
    fn trusted_hosts_are_provider_scoped() {
        assert!(is_trusted_api_base_url(ZAI_API_BASE_URL));
        assert!(is_trusted_api_base_url(
            "https://api.z.ai/api/coding/paas/v4"
        ));
        assert!(is_trusted_api_base_url("https://api.z.ai/api/paas/v4"));
        assert!(is_trusted_api_base_url(
            "https://api.z.ai/api/paas/v4/models"
        ));
        assert!(!is_trusted_api_base_url("http://api.z.ai/api/paas/v4"));
        assert!(!is_trusted_api_base_url("https://api.z.ai.example/v1"));
        assert!(!is_trusted_api_base_url("https://api.x.ai/v1"));
        assert!(!is_trusted_api_base_url("https://proxy.example/v1"));
    }

    #[test]
    fn stored_keys_never_leave_owned_hosts() {
        let stored = Some("zai-stored-secret".to_owned());
        assert_eq!(
            select_api_key(ZAI_API_BASE_URL, None, stored.clone()).as_deref(),
            Some("zai-stored-secret")
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
        let client = ZaiModelsClient::with_base_url(ZAI_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            ZaiModelsResponse {
                data: vec![
                    ZaiWireModel {
                        id: " glm-5.2 ".to_owned(),
                    },
                    ZaiWireModel { id: "".to_owned() },
                    ZaiWireModel {
                        id: "glm-4.6".to_owned(),
                    },
                ],
            },
            "catalog-key",
        );
        let entries = catalog.entries();
        assert_eq!(entries.len(), 2);
        assert!(catalog.is_authoritative());
        for (key, entry) in entries {
            assert!(key.starts_with("zai:"));
            assert_eq!(entry.info.provider, ModelProvider::Zai);
            assert_eq!(entry.info.api_backend, ApiBackend::ChatCompletions);
            assert_eq!(entry.info.tool_mode, Some(ToolMode::Direct));
            assert!(!entry.info.supports_backend_search);
            assert_eq!(entry.info.supports_standalone_web_search, Some(false));
            assert_eq!(
                entry.env_key.as_ref().and_then(EnvKeys::primary),
                Some(ZAI_API_KEY_ENV)
            );
        }
    }

    #[test]
    fn fallback_catalog_marks_known_reasoning_models() {
        let catalog = ZaiModelsCatalog::fallback(ZAI_API_BASE_URL);
        assert!(!catalog.is_authoritative());
        let entries = catalog.entries();
        // Fallback list is populated.
        assert!(!entries.is_empty());
        assert!(entries.contains_key("zai:glm-5.2"));
        // Reasoning-capable models expose the effort control.
        let glm_5_2 = &entries["zai:glm-5.2"];
        assert!(glm_5_2.info.supports_reasoning_effort);
        // Non-reasoning models do not (none in the current fallback set, so
        // assert the predicate itself classifies an unknown id as non-reasoning).
        assert!(!is_known_reasoning_model("glm-ocr"));
    }

    #[test]
    fn wire_catalog_marks_reasoning_for_known_models() {
        let client = ZaiModelsClient::with_base_url(ZAI_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            ZaiModelsResponse {
                data: vec![
                    ZaiWireModel {
                        id: "glm-5.2".to_owned(),
                    },
                    ZaiWireModel {
                        id: "glm-4.6".to_owned(),
                    },
                    ZaiWireModel {
                        id: "glm-ocr".to_owned(),
                    },
                ],
            },
            "catalog-key",
        );
        let entries = catalog.entries();
        assert!(entries["zai:glm-5.2"].info.supports_reasoning_effort);
        assert!(entries["zai:glm-4.6"].info.supports_reasoning_effort);
        assert!(!entries["zai:glm-ocr"].info.supports_reasoning_effort);
    }

    #[test]
    fn empty_wire_response_falls_back_to_curated_catalog() {
        let client = ZaiModelsClient::with_base_url(ZAI_API_BASE_URL);
        let catalog =
            client.catalog_from_wire(ZaiModelsResponse { data: Vec::new() }, "catalog-key");
        // Fallback is non-authoritative but populated.
        assert!(!catalog.is_authoritative());
        assert!(!catalog.entries().is_empty());
    }

    #[test]
    fn errors_redact_credentials() {
        assert_eq!(
            safe_error_excerpt("invalid zai-secret\nretry", "zai-secret"),
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
                "data": [{"id": "glm-5.2"}]
            }))
        }

        let capture = RequestCapture::default();
        let app = Router::new()
            .route("/v4/models", get(models))
            .with_state(capture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = ZaiModelsClient::with_base_url(format!("http://{address}/v4"));
        let catalog = client
            .query_with_key("test-zai-key")
            .await
            .expect("model query");
        assert_eq!(
            capture.0.lock().expect("capture lock").as_deref(),
            Some("Bearer test-zai-key")
        );
        assert!(catalog.entries().contains_key("zai:glm-5.2"));
        assert!(catalog.is_authoritative());
    }

    #[tokio::test]
    async fn query_falls_back_when_models_endpoint_unavailable() {
        let client = ZaiModelsClient::with_base_url("http://127.0.0.1:1/v4");
        let catalog = client.query_with_fallback("test-key").await;
        // Unreachable endpoint → curated fallback, non-authoritative.
        assert!(!catalog.is_authoritative());
        assert!(catalog.entries().contains_key("zai:glm-5.2"));
    }
}
