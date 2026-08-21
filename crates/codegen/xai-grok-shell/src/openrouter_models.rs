//! Provider-isolated OpenRouter model discovery.
//!
//! OpenRouter's `/models` endpoint is authoritative for availability and
//! metadata. The catalog is opt-in: discovered models stay in Settings until
//! the user enables them, matching OpenCode Go.

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

pub const OPENROUTER_API_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const OPENROUTER_API_BASE_URL_ENV: &str = "OPENGROK_OPENROUTER_API_BASE_URL";
pub const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
const OPENROUTER_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const OPENROUTER_HTTP_REFERER: &str = "https://github.com/mweinbach/open-grok";
const OPENROUTER_APP_TITLE: &str = "Open Grok";

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OpenRouterModelDescriptor {
    pub key: String,
    pub id: String,
    pub name: String,
    pub api_backend: ApiBackend,
}

pub fn is_trusted_api_base_url(base_url: &str) -> bool {
    let Ok(url) = Url::parse(base_url) else {
        return false;
    };
    url.scheme() == "https" && url.host_str() == Some("openrouter.ai")
}

pub fn api_base_url() -> String {
    std::env::var(OPENROUTER_API_BASE_URL_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OPENROUTER_API_BASE_URL.to_owned())
}

fn environment_api_key() -> Option<String> {
    std::env::var(OPENROUTER_API_KEY_ENV)
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
        ModelProvider::OpenRouter,
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

fn attribution_headers() -> IndexMap<String, String> {
    IndexMap::from([
        (
            "HTTP-Referer".to_owned(),
            OPENROUTER_HTTP_REFERER.to_owned(),
        ),
        ("X-Title".to_owned(), OPENROUTER_APP_TITLE.to_owned()),
    ])
}

#[derive(Clone, Debug)]
pub(crate) struct OpenRouterModelsCatalog {
    entries: IndexMap<String, ModelEntry>,
    descriptors: Vec<OpenRouterModelDescriptor>,
    credential_fingerprint: String,
    warnings: Vec<String>,
}

impl OpenRouterModelsCatalog {
    pub(crate) fn entries(&self) -> IndexMap<String, ModelEntry> {
        self.entries.clone()
    }

    pub(crate) fn descriptors(&self) -> Vec<OpenRouterModelDescriptor> {
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
pub(crate) struct OpenRouterModelsClient {
    http: reqwest::Client,
    base_url: String,
}

impl OpenRouterModelsClient {
    pub(crate) fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: api_base_url(),
        }
    }

    #[cfg(test)]
    fn with_url(base_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
        }
    }

    pub(crate) async fn query(&self) -> anyhow::Result<Option<OpenRouterModelsCatalog>> {
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
        catalog: &OpenRouterModelsCatalog,
    ) -> bool {
        api_key_for_base_url(&self.base_url)
            .map(|key| credential_fingerprint(&key))
            .is_some_and(|fingerprint| fingerprint == catalog.credential_fingerprint)
    }

    async fn query_with_key(&self, api_key: &str) -> anyhow::Result<OpenRouterModelsCatalog> {
        let models_url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let models_response = self
            .http
            .get(&models_url)
            .timeout(OPENROUTER_MODELS_REQUEST_TIMEOUT)
            .bearer_auth(api_key)
            .header("HTTP-Referer", OPENROUTER_HTTP_REFERER)
            .header("X-Title", OPENROUTER_APP_TITLE)
            .send()
            .await
            .with_context(|| "OpenRouter model discovery request failed")?;

        let models_status = models_response.status();
        if !models_status.is_success() {
            let body = models_response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "OpenRouter models request returned {models_status}: {}",
                safe_error_excerpt(&body, api_key)
            ));
        }

        let available: OpenRouterModelsResponse = models_response
            .json()
            .await
            .context("OpenRouter models response was invalid")?;
        Ok(self.catalog_from_wire(available, api_key))
    }

    fn catalog_from_wire(
        &self,
        available: OpenRouterModelsResponse,
        api_key: &str,
    ) -> OpenRouterModelsCatalog {
        let mut entries = IndexMap::new();
        let mut descriptors = Vec::new();
        let mut warnings = Vec::new();
        for wire in available.data {
            let id = wire.id.trim();
            if id.is_empty() {
                continue;
            }
            if let Some(reason) = skip_reason(&wire) {
                warnings.push(format!("OpenRouter model `{id}` omitted: {reason}"));
                continue;
            }
            let key = format!("openrouter:{id}");
            let mut info = ModelInfo::fallback(&key);
            info.id = Some(key.clone());
            info.model = id.to_owned();
            info.base_url = self.base_url.trim_end_matches('/').to_owned();
            info.name = Some(wire.name.clone().unwrap_or_else(|| id.to_owned()));
            info.description = wire.description.clone();
            info.api_backend = ApiBackend::ChatCompletions;
            info.auth_scheme = AuthScheme::Bearer;
            info.provider = ModelProvider::OpenRouter;
            info.tool_mode = Some(ToolMode::Direct);
            info.extra_headers = attribution_headers();
            info.context_window = context_window(&wire);
            info.max_completion_tokens = max_completion_tokens(&wire);
            info.supported_in_api = true;
            let reasoning_efforts = if supports_reasoning(&wire) {
                openrouter_reasoning_efforts()
            } else {
                Vec::new()
            };
            info.supports_reasoning_effort = !reasoning_efforts.is_empty();
            info.reasoning_efforts = reasoning_efforts;
            info.reasoning_effort = info
                .reasoning_efforts
                .iter()
                .find(|option| option.default)
                .map(|option| option.value);
            let name = info.name.clone().unwrap_or_else(|| id.to_owned());
            entries.insert(
                key.clone(),
                ModelEntry {
                    info,
                    api_key: None,
                    env_key: Some(EnvKeys::single(OPENROUTER_API_KEY_ENV)),
                    auth_provider: None,
                    api_base_url: None,
                },
            );
            descriptors.push(OpenRouterModelDescriptor {
                key,
                id: id.to_owned(),
                name,
                api_backend: ApiBackend::ChatCompletions,
            });
        }
        descriptors.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        OpenRouterModelsCatalog {
            entries,
            descriptors,
            credential_fingerprint: credential_fingerprint(api_key),
            warnings,
        }
    }
}

fn skip_reason(wire: &OpenRouterWireModel) -> Option<&'static str> {
    if !has_text_output(wire) {
        return Some("no text output");
    }
    if !supports_tools(wire) {
        return Some("does not advertise tool calling");
    }
    None
}

fn has_text_output(wire: &OpenRouterWireModel) -> bool {
    let Some(architecture) = wire.architecture.as_ref() else {
        return true;
    };
    if !architecture.output_modalities.is_empty() {
        return architecture
            .output_modalities
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("text"));
    }
    match architecture.modality.as_deref() {
        Some(modality) => {
            let lower = modality.to_ascii_lowercase();
            !lower.contains("embedding")
                && !lower.contains("->image")
                && !lower.contains("->audio")
                && !lower.contains("->video")
        }
        None => true,
    }
}

fn supports_tools(wire: &OpenRouterWireModel) -> bool {
    if wire.supported_parameters.is_empty() {
        return true;
    }
    wire.supported_parameters
        .iter()
        .any(|parameter| parameter.eq_ignore_ascii_case("tools"))
}

fn supports_reasoning(wire: &OpenRouterWireModel) -> bool {
    wire.supported_parameters.iter().any(|parameter| {
        parameter.eq_ignore_ascii_case("reasoning")
            || parameter.eq_ignore_ascii_case("reasoning_effort")
    })
}

fn context_window(wire: &OpenRouterWireModel) -> NonZeroU64 {
    wire.context_length
        .or_else(|| {
            wire.top_provider
                .as_ref()
                .and_then(|provider| provider.context_length)
        })
        .and_then(NonZeroU64::new)
        .unwrap_or_else(|| NonZeroU64::new(200_000).expect("non-zero fallback"))
}

fn max_completion_tokens(wire: &OpenRouterWireModel) -> Option<u32> {
    wire.top_provider
        .as_ref()
        .and_then(|provider| provider.max_completion_tokens)
        .filter(|tokens| *tokens > 0)
        .and_then(|tokens| u32::try_from(tokens).ok())
}

fn openrouter_reasoning_efforts() -> Vec<ReasoningEffortOption> {
    [
        (ReasoningEffort::None, "None", false),
        (ReasoningEffort::Low, "Low", false),
        (ReasoningEffort::Medium, "Medium", true),
        (ReasoningEffort::High, "High", false),
        (ReasoningEffort::Xhigh, "Xhigh", false),
    ]
    .into_iter()
    .map(|(value, label, default)| ReasoningEffortOption {
        id: value.as_str().to_owned(),
        value,
        label: label.to_owned(),
        description: None,
        default,
    })
    .collect()
}

fn safe_error_excerpt(body: &str, api_key: &str) -> String {
    let sanitized = body
        .replace(api_key, "[REDACTED]")
        .replace(['\r', '\n'], " ");
    sanitized.chars().take(512).collect()
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelsResponse {
    #[serde(default)]
    data: Vec<OpenRouterWireModel>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterWireModel {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    architecture: Option<OpenRouterArchitecture>,
    #[serde(default)]
    top_provider: Option<OpenRouterTopProvider>,
    #[serde(default)]
    supported_parameters: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    #[serde(default)]
    modality: Option<String>,
    #[serde(default)]
    output_modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterTopProvider {
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(
        id: &str,
        name: &str,
        outputs: &[&str],
        params: &[&str],
        context: Option<u64>,
    ) -> OpenRouterWireModel {
        OpenRouterWireModel {
            id: id.to_owned(),
            name: Some(name.to_owned()),
            description: None,
            context_length: context,
            architecture: Some(OpenRouterArchitecture {
                modality: None,
                output_modalities: outputs.iter().map(|value| (*value).to_owned()).collect(),
            }),
            top_provider: Some(OpenRouterTopProvider {
                context_length: context,
                max_completion_tokens: Some(8_192),
            }),
            supported_parameters: params.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    #[test]
    fn trusted_host_is_https_openrouter() {
        assert!(is_trusted_api_base_url(OPENROUTER_API_BASE_URL));
        assert!(is_trusted_api_base_url(
            "https://openrouter.ai/api/v1/chat/completions"
        ));
        assert!(!is_trusted_api_base_url("https://example.com/v1"));
        assert!(!is_trusted_api_base_url("http://openrouter.ai/api/v1"));
    }

    #[test]
    fn catalog_keeps_tool_capable_text_models_and_fails_closed_until_enabled() {
        let client = OpenRouterModelsClient::with_url(OPENROUTER_API_BASE_URL);
        let catalog = client.catalog_from_wire(
            OpenRouterModelsResponse {
                data: vec![
                    wire(
                        "anthropic/claude-sonnet-4",
                        "Anthropic: Claude Sonnet 4",
                        &["text"],
                        &["tools", "reasoning"],
                        Some(200_000),
                    ),
                    wire(
                        "openai/gpt-4o",
                        "OpenAI: GPT-4o",
                        &["text"],
                        &["tools", "temperature"],
                        Some(128_000),
                    ),
                    wire(
                        "black-forest-labs/flux",
                        "Flux",
                        &["image"],
                        &["tools"],
                        Some(4_096),
                    ),
                    wire(
                        "openai/text-embedding-3-large",
                        "Embeddings",
                        &["embeddings"],
                        &[],
                        Some(8_191),
                    ),
                    wire(
                        "meta-llama/llama-3.1-8b-instruct",
                        "Llama 3.1 8B",
                        &["text"],
                        &["temperature"],
                        Some(131_072),
                    ),
                ],
            },
            "secret",
        );
        let entries = catalog.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains_key("openrouter:anthropic/claude-sonnet-4"));
        assert!(entries.contains_key("openrouter:openai/gpt-4o"));
        assert_eq!(
            entries["openrouter:anthropic/claude-sonnet-4"]
                .info
                .api_backend,
            ApiBackend::ChatCompletions
        );
        assert_eq!(
            entries["openrouter:anthropic/claude-sonnet-4"]
                .info
                .context_window
                .get(),
            200_000
        );
        assert_eq!(
            entries["openrouter:anthropic/claude-sonnet-4"]
                .info
                .max_completion_tokens,
            Some(8_192)
        );
        assert!(
            entries["openrouter:anthropic/claude-sonnet-4"]
                .info
                .supports_reasoning_effort
        );
        assert!(
            !entries["openrouter:openai/gpt-4o"]
                .info
                .supports_reasoning_effort
        );
        assert_eq!(
            entries["openrouter:openai/gpt-4o"]
                .info
                .extra_headers
                .get("HTTP-Referer")
                .map(String::as_str),
            Some(OPENROUTER_HTTP_REFERER)
        );
        assert_eq!(catalog.warnings().len(), 3);

        let mut cfg = crate::agent::config::Config::default();
        let disabled = crate::agent::models::resolve_model_catalog_with_provider_catalogs_and_wafer(
            &cfg,
            None,
            None,
            None,
            None,
            None,
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
                .all(|entry| entry.info.provider != ModelProvider::OpenRouter),
            "OpenRouter must default to no enabled models",
        );

        cfg.models.openrouter_enabled_models = vec!["openai/gpt-4o".to_owned()];
        let enabled = crate::agent::models::resolve_model_catalog_with_provider_catalogs_and_wafer(
            &cfg,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(&catalog),
        );
        assert!(enabled.contains_key("openrouter:openai/gpt-4o"));
        assert!(!enabled.contains_key("openrouter:anthropic/claude-sonnet-4"));
    }
}
