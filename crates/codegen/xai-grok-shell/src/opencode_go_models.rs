//! Provider-isolated OpenCode Go model discovery.
//!
//! OpenCode Go's `/models` endpoint is authoritative for availability but
//! returns model ids only. Canonical models.dev metadata supplies the per-model
//! wire protocol and limits. The intersection fails closed when metadata is
//! missing or names an unsupported SDK.

use crate::agent::config::{EnvKeys, ModelEntry, ModelInfo};
use anyhow::{Context, anyhow};
use indexmap::IndexMap;
use serde::Deserialize;
use std::num::NonZeroU64;
use std::time::Duration;
use url::Url;
use xai_grok_sampler::AuthScheme;
use xai_grok_sampling_types::{
    ApiBackend, ModelProvider, ReasoningEffort, ReasoningEffortOption, ToolMode,
};

pub const OPENCODE_GO_API_BASE_URL: &str = "https://opencode.ai/zen/go/v1";
pub const OPENCODE_GO_API_BASE_URL_ENV: &str = "OPENGROK_OPENCODE_GO_API_BASE_URL";
pub const OPENCODE_GO_MODELS_DEV_URL: &str = "https://models.dev/api.json";
pub const OPENCODE_GO_MODELS_DEV_URL_ENV: &str = "OPENGROK_OPENCODE_GO_MODELS_DEV_URL";
pub const OPENCODE_GO_API_KEY_ENV: &str = "OPENCODE_API_KEY";
const OPENCODE_GO_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const OPENCODE_GO_MODELS_DEV_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpenCodeGoModelDescriptor {
    pub key: String,
    pub id: String,
    pub name: String,
    pub api_backend: ApiBackend,
}

pub fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https" && url.host_str() == Some("opencode.ai")
}

pub fn api_base_url() -> String {
    std::env::var(OPENCODE_GO_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OPENCODE_GO_API_BASE_URL.to_owned())
}

fn models_dev_url() -> String {
    std::env::var(OPENCODE_GO_MODELS_DEV_URL_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OPENCODE_GO_MODELS_DEV_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    std::env::var(OPENCODE_GO_API_KEY_ENV)
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
        ModelProvider::OpenCodeGo,
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

#[derive(Clone, Debug)]
pub(crate) struct OpenCodeGoModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    descriptors: Vec<OpenCodeGoModelDescriptor>,
    credential_fingerprint: String,
    warnings: Vec<String>,
}

impl OpenCodeGoModelsCatalog {
    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn descriptors(&self) -> Vec<OpenCodeGoModelDescriptor> {
        self.descriptors.clone()
    }

    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub(crate) fn is_authoritative(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OpenCodeGoModelsClient {
    http: crate::lazy_http::LazyHttpClient,
    base_url: String,
    models_dev_url: String,
}

impl OpenCodeGoModelsClient {
    pub(crate) fn new() -> Self {
        Self {
            http: crate::lazy_http::LazyHttpClient::new(),
            base_url: api_base_url(),
            models_dev_url: models_dev_url(),
        }
    }

    #[cfg(test)]
    fn with_urls(base_url: impl Into<String>, models_dev_url: impl Into<String>) -> Self {
        Self {
            http: crate::lazy_http::LazyHttpClient::new(),
            base_url: base_url.into(),
            models_dev_url: models_dev_url.into(),
        }
    }

    pub(crate) async fn query(&self) -> anyhow::Result<Option<OpenCodeGoModelsCatalog>> {
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
        catalog: &OpenCodeGoModelsCatalog,
    ) -> bool {
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|fingerprint| fingerprint == catalog.credential_fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<OpenCodeGoModelsCatalog> {
        let models_url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let models_request = self
            .http
            .client()
            .get(&models_url)
            .timeout(OPENCODE_GO_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .send();
        let metadata_request = self
            .http
            .client()
            .get(&self.models_dev_url)
            .timeout(OPENCODE_GO_MODELS_DEV_REQUEST_TIMEOUT)
            .send();
        let (models_response, metadata_response) =
            tokio::try_join!(models_request, metadata_request)
                .with_context(|| "OpenCode Go model discovery request failed")?;

        let models_status = models_response.status();
        if !models_status.is_success() {
            let body = models_response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OpenCode Go models request returned {models_status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }
        let metadata_status = metadata_response.status();
        if !metadata_status.is_success() {
            return Err(anyhow!(
                "OpenCode Go metadata request returned {metadata_status}"
            ));
        }

        let available: OpenCodeGoModelsResponse = models_response
            .json()
            .await
            .context("OpenCode Go models response was invalid")?;
        // Isolate to the `opencode-go` object. models.dev is a global catalog;
        // sibling providers can carry schema the rest of this file does not
        // model (null effort tokens, unexpected types) and must not fail
        // OpenCode Go discovery.
        let metadata: ModelsDevCatalog = metadata_response
            .json()
            .await
            .context("OpenCode Go models.dev response was invalid")?;
        let provider = metadata
            .opencode_go
            .ok_or_else(|| anyhow!("models.dev did not contain the opencode-go provider"))?;
        Ok(self.catalog_from_wire(available, &provider, api_key))
    }

    fn catalog_from_wire(
        &self,
        available: OpenCodeGoModelsResponse,
        provider: &ModelsDevProvider,
        api_key: &str,
    ) -> OpenCodeGoModelsCatalog {
        let mut entries = IndexMap::new();
        let mut descriptors = Vec::new();
        let mut warnings = Vec::new();
        for wire in available.data {
            let id = wire.id.trim();
            if id.is_empty() {
                continue;
            }
            let Some(metadata) = provider.models.get(id) else {
                warnings.push(format!(
                    "OpenCode Go model `{id}` has no models.dev metadata"
                ));
                continue;
            };
            let sdk = metadata
                .provider
                .as_ref()
                .and_then(|provider| provider.npm.as_deref())
                .unwrap_or(provider.npm.as_str());
            let Some((api_backend, auth_scheme)) = protocol_for_sdk(sdk) else {
                warnings.push(format!(
                    "OpenCode Go model `{id}` uses unsupported SDK metadata `{sdk}`"
                ));
                continue;
            };
            let key = format!("opencode-go:{id}");
            let mut info = ModelInfo::fallback(&key);
            info.id = Some(key.clone());
            info.model = id.to_owned();
            info.base_url = self.base_url.trim_end_matches('/').to_owned();
            info.name = Some(metadata.name.clone().unwrap_or_else(|| id.to_owned()));
            info.description = metadata.description.clone();
            info.api_backend = api_backend;
            info.auth_scheme = auth_scheme;
            info.provider = ModelProvider::OpenCodeGo;
            info.tool_mode = Some(ToolMode::Direct);
            info.context_window = metadata
                .limit
                .as_ref()
                .and_then(|limit| limit.context)
                .and_then(NonZeroU64::new)
                .unwrap_or_else(|| NonZeroU64::new(200_000).expect("non-zero fallback"));
            info.supported_in_api = true;
            let efforts = reasoning_efforts_from_metadata(metadata);
            info.supports_reasoning_effort = !efforts.is_empty();
            info.reasoning_effort = efforts
                .iter()
                .find(|option| option.default)
                .map(|option| option.value);
            info.reasoning_efforts = efforts;
            let name = info.name.clone().unwrap_or_else(|| id.to_owned());
            entries.insert(
                key.clone(),
                ModelEntry {
                    info,
                    api_key: None,
                    env_key: Some(EnvKeys::single(OPENCODE_GO_API_KEY_ENV)),
                    auth_provider: None,
                    api_base_url: None,
                },
            );
            descriptors.push(OpenCodeGoModelDescriptor {
                key,
                id: id.to_owned(),
                name,
                api_backend,
            });
        }
        descriptors.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        OpenCodeGoModelsCatalog {
            entries,
            descriptors,
            credential_fingerprint: credential_fingerprint(api_key),
            warnings,
        }
    }
}

fn protocol_for_sdk(sdk: &str) -> Option<(ApiBackend, AuthScheme)> {
    match sdk {
        "@ai-sdk/anthropic" => Some((ApiBackend::Messages, AuthScheme::XApiKey)),
        "@ai-sdk/openai-compatible" | "@ai-sdk/openai" => {
            Some((ApiBackend::ChatCompletions, AuthScheme::Bearer))
        }
        _ => None,
    }
}

/// Effort menu from models.dev `reasoning_options`.
///
/// Only `type: "effort"` entries map onto Open Grok's `reasoning_effort`
/// control. `toggle` / `budget_tokens` entries describe a different wire
/// shape Open Grok does not send, so they are ignored. An absent, null, or
/// empty effort list stays fail-closed (no menu), matching OpenRouter's
/// `supported_efforts` contract. models.dev carries no default, so no option
/// is marked default and `reasoning_effort` stays unset (gateway default).
fn reasoning_efforts_from_metadata(metadata: &ModelsDevModel) -> Vec<ReasoningEffortOption> {
    if metadata.reasoning == Some(false) {
        return Vec::new();
    }
    let mut seen = Vec::new();
    for option in metadata.reasoning_options.as_deref().unwrap_or(&[]) {
        let is_effort = option
            .option_type
            .as_deref()
            .is_some_and(|kind| kind.eq_ignore_ascii_case("effort"));
        if !is_effort {
            continue;
        }
        for token in option.values.as_deref().unwrap_or(&[]) {
            let Some(token) = token.as_deref() else {
                continue;
            };
            let Ok(effort) = token.parse::<ReasoningEffort>() else {
                continue;
            };
            if !seen.contains(&effort) {
                seen.push(effort);
            }
        }
    }
    seen.into_iter()
        .map(|value| ReasoningEffortOption {
            id: value.as_str().to_owned(),
            value,
            label: effort_label(value).to_owned(),
            description: None,
            default: false,
        })
        .collect()
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

fn safe_error_excerpt(body: &str, api_key: &str) -> String {
    let sanitized = body
        .replace(api_key, "[REDACTED]")
        .replace(['\r', '\n'], " ");
    sanitized.chars().take(512).collect()
}

#[derive(Debug, Deserialize)]
struct OpenCodeGoModelsResponse {
    #[serde(default)]
    data: Vec<OpenCodeGoWireModel>,
}

#[derive(Debug, Deserialize)]
struct OpenCodeGoWireModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ModelsDevCatalog {
    #[serde(rename = "opencode-go", default)]
    opencode_go: Option<ModelsDevProvider>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevProvider {
    npm: String,
    #[serde(default)]
    models: IndexMap<String, ModelsDevModel>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModel {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    provider: Option<ModelsDevModelProvider>,
    #[serde(default)]
    limit: Option<ModelsDevLimit>,
    #[serde(default)]
    reasoning: Option<bool>,
    #[serde(default)]
    reasoning_options: Option<Vec<ModelsDevReasoningOption>>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevReasoningOption {
    #[serde(default, rename = "type")]
    option_type: Option<String>,
    /// models.dev has used `null` entries in effort lists (a sibling-provider
    /// shape that previously failed the whole catalog parse). Skip them.
    #[serde(default)]
    values: Option<Vec<Option<String>>>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevModelProvider {
    #[serde(default)]
    npm: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelsDevLimit {
    #[serde(default)]
    context: Option<u64>,
    #[serde(default, rename = "output")]
    _output: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_metadata_maps_per_model() {
        assert_eq!(
            protocol_for_sdk("@ai-sdk/anthropic"),
            Some((ApiBackend::Messages, AuthScheme::XApiKey))
        );
        assert_eq!(
            protocol_for_sdk("@ai-sdk/openai-compatible"),
            Some((ApiBackend::ChatCompletions, AuthScheme::Bearer))
        );
        assert_eq!(protocol_for_sdk("@ai-sdk/unknown"), None);
    }

    #[test]
    fn catalog_intersects_availability_and_fails_closed() {
        let client = OpenCodeGoModelsClient::with_urls(OPENCODE_GO_API_BASE_URL, "unused");
        let provider = ModelsDevProvider {
            npm: "@ai-sdk/openai-compatible".to_owned(),
            models: IndexMap::from([
                (
                    "chat-model".to_owned(),
                    ModelsDevModel {
                        name: Some("Chat Model".to_owned()),
                        description: None,
                        provider: None,
                        limit: Some(ModelsDevLimit {
                            context: Some(256_000),
                            _output: Some(1_000_000),
                        }),
                        reasoning: None,
                        reasoning_options: None,
                    },
                ),
                (
                    "messages-model".to_owned(),
                    ModelsDevModel {
                        name: Some("Messages Model".to_owned()),
                        description: None,
                        provider: Some(ModelsDevModelProvider {
                            npm: Some("@ai-sdk/anthropic".to_owned()),
                        }),
                        limit: Some(ModelsDevLimit {
                            context: Some(200_000),
                            _output: Some(500_000),
                        }),
                        reasoning: None,
                        reasoning_options: None,
                    },
                ),
            ]),
        };
        let catalog = client.catalog_from_wire(
            OpenCodeGoModelsResponse {
                data: vec![
                    OpenCodeGoWireModel {
                        id: "chat-model".to_owned(),
                    },
                    OpenCodeGoWireModel {
                        id: "messages-model".to_owned(),
                    },
                    OpenCodeGoWireModel {
                        id: "unknown".to_owned(),
                    },
                ],
            },
            &provider,
            "secret",
        );
        let entries = catalog.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries["opencode-go:chat-model"].info.api_backend,
            ApiBackend::ChatCompletions
        );
        assert_eq!(
            entries["opencode-go:messages-model"].info.api_backend,
            ApiBackend::Messages
        );
        assert_eq!(
            entries["opencode-go:chat-model"].info.max_completion_tokens,
            None
        );
        assert_eq!(
            entries["opencode-go:messages-model"]
                .info
                .max_completion_tokens,
            None
        );
        assert_eq!(catalog.warnings().len(), 1);
        assert!(
            !entries["opencode-go:chat-model"]
                .info
                .supports_reasoning_effort
        );
        assert!(
            entries["opencode-go:chat-model"]
                .info
                .reasoning_efforts
                .is_empty()
        );

        let mut cfg = crate::agent::config::Config::default();
        let disabled = crate::agent::models::resolve_model_catalog_with_provider_catalogs(
            &cfg,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&catalog),
        );
        assert!(
            disabled
                .values()
                .all(|entry| entry.info.provider != ModelProvider::OpenCodeGo),
            "OpenCode Go must default to no enabled models",
        );

        cfg.models.opencode_go_enabled_models = vec!["messages-model".to_owned()];
        let enabled = crate::agent::models::resolve_model_catalog_with_provider_catalogs(
            &cfg,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&catalog),
        );
        assert!(enabled.contains_key("opencode-go:messages-model"));
        assert!(!enabled.contains_key("opencode-go:chat-model"));
    }

    fn models_dev_model(
        reasoning: Option<bool>,
        reasoning_options: Option<Vec<ModelsDevReasoningOption>>,
    ) -> ModelsDevModel {
        ModelsDevModel {
            name: None,
            description: None,
            provider: None,
            limit: None,
            reasoning,
            reasoning_options,
        }
    }

    fn effort_option(option_type: &str, values: &[&str]) -> ModelsDevReasoningOption {
        ModelsDevReasoningOption {
            option_type: Some(option_type.to_owned()),
            values: Some(
                values
                    .iter()
                    .map(|value| Some((*value).to_owned()))
                    .collect(),
            ),
        }
    }

    fn effort_values(options: &[ReasoningEffortOption]) -> Vec<ReasoningEffort> {
        options.iter().map(|option| option.value).collect()
    }

    #[test]
    fn reasoning_efforts_parse_effort_values_in_order() {
        let metadata = models_dev_model(
            Some(true),
            Some(vec![effort_option("effort", &["low", "high", "max"])]),
        );
        let efforts = reasoning_efforts_from_metadata(&metadata);
        assert_eq!(
            effort_values(&efforts),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::High,
                ReasoningEffort::Max
            ]
        );
        assert!(efforts.iter().all(|option| !option.default));
    }

    #[test]
    fn reasoning_efforts_ignore_toggle_and_budget_tokens() {
        let metadata = models_dev_model(
            Some(true),
            Some(vec![
                effort_option("toggle", &[]),
                effort_option("effort", &["low", "medium", "xhigh"]),
                ModelsDevReasoningOption {
                    option_type: Some("budget_tokens".to_owned()),
                    values: None,
                },
            ]),
        );
        let efforts = reasoning_efforts_from_metadata(&metadata);
        assert_eq!(
            effort_values(&efforts),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::Xhigh
            ]
        );
    }

    #[test]
    fn reasoning_efforts_stay_fail_closed_without_effort_list() {
        for metadata in [
            models_dev_model(None, None),
            models_dev_model(Some(true), None),
            models_dev_model(Some(true), Some(vec![])),
            models_dev_model(Some(true), Some(vec![effort_option("toggle", &[])])),
            models_dev_model(
                Some(true),
                Some(vec![ModelsDevReasoningOption {
                    option_type: None,
                    values: Some(vec![Some("high".to_owned())]),
                }]),
            ),
            models_dev_model(Some(false), Some(vec![effort_option("effort", &["high"])])),
        ] {
            assert!(
                reasoning_efforts_from_metadata(&metadata).is_empty(),
                "{metadata:?}"
            );
        }
    }

    #[test]
    fn reasoning_efforts_skip_invalid_tokens_and_dedupe() {
        let metadata = models_dev_model(
            Some(true),
            Some(vec![
                effort_option("effort", &["high", "HIGH", "future", "max"]),
                effort_option("effort", &["max", "low"]),
            ]),
        );
        let efforts = reasoning_efforts_from_metadata(&metadata);
        assert_eq!(
            effort_values(&efforts),
            vec![
                ReasoningEffort::High,
                ReasoningEffort::Max,
                ReasoningEffort::Low
            ]
        );
    }

    #[test]
    fn reasoning_efforts_deserialize_live_models_dev_shape() {
        let metadata: ModelsDevModel = serde_json::from_value(serde_json::json!({
            "id": "glm-5.2",
            "name": "GLM-5.2",
            "reasoning": true,
            "reasoning_options": [{"type": "effort", "values": ["high", "max"]}],
            "limit": {"context": 1000000, "output": 131072}
        }))
        .expect("live models.dev model shape");
        let efforts = reasoning_efforts_from_metadata(&metadata);
        assert_eq!(
            effort_values(&efforts),
            vec![ReasoningEffort::High, ReasoningEffort::Max]
        );

        let toggle_only: ModelsDevModel = serde_json::from_value(serde_json::json!({
            "reasoning": true,
            "reasoning_options": [{"type": "toggle"}, {"type": "budget_tokens", "max": 262144}]
        }))
        .expect("toggle-only models.dev shape");
        assert!(reasoning_efforts_from_metadata(&toggle_only).is_empty());

        let null_tokens: ModelsDevModel = serde_json::from_value(serde_json::json!({
            "reasoning": true,
            "reasoning_options": [{"type": "effort", "values": [null, "low", "medium", "high"]}]
        }))
        .expect("models.dev null effort tokens");
        assert_eq!(
            effort_values(&reasoning_efforts_from_metadata(&null_tokens)),
            vec![
                ReasoningEffort::Low,
                ReasoningEffort::Medium,
                ReasoningEffort::High
            ]
        );
    }

    #[test]
    fn models_dev_catalog_ignores_sibling_provider_schema_drift() {
        let json = serde_json::json!({
            "sarvam": {
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "sarvam-105b": {
                        "reasoning": true,
                        "reasoning_options": [{
                            "type": "effort",
                            "values": [null, "low", "medium", "high"]
                        }]
                    }
                }
            },
            "opencode-go": {
                "npm": "@ai-sdk/openai-compatible",
                "models": {
                    "glm-5.2": {
                        "name": "GLM-5.2",
                        "reasoning": true,
                        "reasoning_options": [{"type": "effort", "values": ["high", "max"]}]
                    }
                }
            }
        });
        let catalog: ModelsDevCatalog =
            serde_json::from_value(json).expect("sibling drift must not fail opencode-go parse");
        let provider = catalog
            .opencode_go
            .expect("opencode-go provider")
            .models
            .shift_remove("glm-5.2")
            .expect("glm-5.2");
        assert_eq!(provider.name.as_deref(), Some("GLM-5.2"));
        assert_eq!(
            effort_values(&reasoning_efforts_from_metadata(&provider)),
            vec![ReasoningEffort::High, ReasoningEffort::Max]
        );
    }

    #[test]
    fn catalog_exposes_models_dev_effort_menu() {
        let client = OpenCodeGoModelsClient::with_urls(OPENCODE_GO_API_BASE_URL, "unused");
        let provider = ModelsDevProvider {
            npm: "@ai-sdk/openai-compatible".to_owned(),
            models: IndexMap::from([(
                "reasoning-model".to_owned(),
                ModelsDevModel {
                    name: Some("Reasoning Model".to_owned()),
                    description: None,
                    provider: None,
                    limit: None,
                    reasoning: Some(true),
                    reasoning_options: Some(vec![effort_option("effort", &["high", "max"])]),
                },
            )]),
        };
        let catalog = client.catalog_from_wire(
            OpenCodeGoModelsResponse {
                data: vec![OpenCodeGoWireModel {
                    id: "reasoning-model".to_owned(),
                }],
            },
            &provider,
            "secret",
        );
        let info = &catalog.entries()["opencode-go:reasoning-model"].info;
        assert!(info.supports_reasoning_effort);
        assert_eq!(
            effort_values(&info.reasoning_efforts),
            vec![ReasoningEffort::High, ReasoningEffort::Max]
        );
        assert_eq!(info.reasoning_effort, None);
    }
}
