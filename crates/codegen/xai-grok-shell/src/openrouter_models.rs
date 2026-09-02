//! Provider-isolated OpenRouter model discovery.
//!
//! OpenRouter's `/models` endpoint is authoritative for availability,
//! metadata, and per-model reasoning efforts. Tool-capable text models stay
//! in Settings until explicitly enabled; an empty enabled list enables none.

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
const OPENROUTER_MODELS_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const OPENROUTER_GATEWAY_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Max,
    ReasoningEffort::Xhigh,
    ReasoningEffort::High,
    ReasoningEffort::Medium,
    ReasoningEffort::Low,
    ReasoningEffort::Minimal,
    ReasoningEffort::None,
];
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
        // `/models` defaults to text output. Keep the local capability checks
        // as well, including for explicitly configured compatible endpoints.
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
            let reasoning_efforts = reasoning_efforts_from_wire(&wire);
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
    if !has_text_input(wire) {
        return Some("no text input");
    }
    if !has_text_output(wire) {
        return Some("no text output");
    }
    if !supports_tools(wire) {
        return Some("does not advertise tool calling");
    }
    None
}

fn has_text_input(wire: &OpenRouterWireModel) -> bool {
    let Some(architecture) = wire.architecture.as_ref() else {
        return true;
    };
    has_text_modality(
        &architecture.input_modalities,
        architecture.modality.as_deref().map(|modality| {
            modality
                .split_once("->")
                .map_or(modality, |(input, _)| input)
        }),
    )
}

fn has_text_output(wire: &OpenRouterWireModel) -> bool {
    let Some(architecture) = wire.architecture.as_ref() else {
        return true;
    };
    has_text_modality(
        &architecture.output_modalities,
        architecture.modality.as_deref().map(|modality| {
            modality
                .split_once("->")
                .map_or(modality, |(_, output)| output)
        }),
    )
}

fn has_text_modality(modalities: &[String], legacy_modality: Option<&str>) -> bool {
    if !modalities.is_empty() {
        return modalities
            .iter()
            .any(|modality| modality.eq_ignore_ascii_case("text"));
    }
    // Older compatible catalogs can omit architecture metadata. When a
    // legacy modality is supplied, inspect the correct side of the arrow:
    // audio->text cannot accept agent prompts, while text->text+image can.
    legacy_modality.is_none_or(|modalities| {
        modalities
            .split('+')
            .any(|modality| modality.trim().eq_ignore_ascii_case("text"))
    })
}

fn supports_tools(wire: &OpenRouterWireModel) -> bool {
    if wire.supported_parameters.is_empty() {
        return true;
    }
    wire.supported_parameters
        .iter()
        .any(|parameter| parameter.eq_ignore_ascii_case("tools"))
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

fn reasoning_efforts_from_wire(wire: &OpenRouterWireModel) -> Vec<ReasoningEffortOption> {
    // OpenRouter: `supported_efforts` array is the allowlist. `null` means the
    // gateway accepts every effort. Omitted means the model does not expose
    // effort selection (thinking may still happen, including `max_tokens`).
    let Some(reasoning) = wire.reasoning.as_ref() else {
        return Vec::new();
    };
    let mandatory = reasoning.mandatory.unwrap_or(false);
    let mut values = match &reasoning.supported_efforts {
        SupportedEfforts::Omitted => return Vec::new(),
        SupportedEfforts::All => OPENROUTER_GATEWAY_EFFORTS.to_vec(),
        SupportedEfforts::List(tokens) => tokens
            .iter()
            .filter_map(|token| token.parse().ok())
            .filter(|effort| *effort != ReasoningEffort::Ultra)
            .collect(),
    };
    if mandatory {
        values.retain(|effort| *effort != ReasoningEffort::None);
    }
    if values.is_empty() {
        return Vec::new();
    }
    effort_options(values, default_effort(reasoning, mandatory))
}

fn default_effort(
    reasoning: &OpenRouterModelReasoning,
    mandatory: bool,
) -> Option<ReasoningEffort> {
    let parsed_default = reasoning
        .default_effort
        .as_deref()
        .and_then(|token| token.parse().ok())
        .filter(|effort| *effort != ReasoningEffort::Ultra);
    if mandatory {
        return parsed_default.filter(|effort| *effort != ReasoningEffort::None);
    }
    match reasoning.default_enabled {
        Some(false) => Some(ReasoningEffort::None),
        Some(true) => parsed_default.filter(|effort| *effort != ReasoningEffort::None),
        // A default effort describes how to enable reasoning; without an
        // advertised on/off default, do not enable it on the user's behalf.
        None => None,
    }
}

fn effort_options(
    values: impl IntoIterator<Item = ReasoningEffort>,
    default: Option<ReasoningEffort>,
) -> Vec<ReasoningEffortOption> {
    let mut seen = Vec::new();
    for value in values {
        if !seen.contains(&value) {
            seen.push(value);
        }
    }
    // An unsupported or absent default is not permission to select the
    // highest advertised effort. Omitting effort preserves gateway defaults.
    let marked = default.filter(|effort| seen.contains(effort));
    seen.into_iter()
        .map(|value| ReasoningEffortOption {
            id: value.as_str().to_owned(),
            value,
            label: effort_label(value).to_owned(),
            description: None,
            default: marked == Some(value),
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
    #[serde(default)]
    reasoning: Option<OpenRouterModelReasoning>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModelReasoning {
    #[serde(default, deserialize_with = "deserialize_supported_efforts")]
    supported_efforts: SupportedEfforts,
    #[serde(default)]
    default_effort: Option<String>,
    #[serde(default)]
    default_enabled: Option<bool>,
    #[serde(default)]
    mandatory: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum SupportedEfforts {
    #[default]
    Omitted,
    All,
    List(Vec<String>),
}

fn deserialize_supported_efforts<'de, D>(deserializer: D) -> Result<SupportedEfforts, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<Vec<String>>::deserialize(deserializer)? {
        None => Ok(SupportedEfforts::All),
        Some(list) => Ok(SupportedEfforts::List(list)),
    }
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    #[serde(default)]
    modality: Option<String>,
    #[serde(default)]
    input_modalities: Vec<String>,
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
                input_modalities: vec!["text".to_owned()],
                output_modalities: outputs.iter().map(|value| (*value).to_owned()).collect(),
            }),
            top_provider: Some(OpenRouterTopProvider {
                context_length: context,
                max_completion_tokens: Some(8_192),
            }),
            supported_parameters: params.iter().map(|value| (*value).to_owned()).collect(),
            reasoning: None,
        }
    }

    fn with_reasoning(
        mut model: OpenRouterWireModel,
        reasoning: OpenRouterModelReasoning,
    ) -> OpenRouterWireModel {
        model.reasoning = Some(reasoning);
        model
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

    fn catalog_from(models: Vec<OpenRouterWireModel>) -> OpenRouterModelsCatalog {
        OpenRouterModelsClient::with_url(OPENROUTER_API_BASE_URL)
            .catalog_from_wire(OpenRouterModelsResponse { data: models }, "secret")
    }

    fn resolve_openrouter(
        cfg: &crate::agent::config::Config,
        catalog: &OpenRouterModelsCatalog,
    ) -> indexmap::IndexMap<String, crate::agent::config::ModelEntry> {
        crate::agent::models::resolve_model_catalog_with_provider_catalogs_and_wafer(
            cfg,
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
            Some(catalog),
        )
    }

    #[test]
    fn catalog_keeps_tool_capable_text_models_and_fails_closed_until_enabled() {
        let catalog = catalog_from(vec![
            with_reasoning(
                wire(
                    "google/gemini-3.5-flash",
                    "Google: Gemini 3.5 Flash",
                    &["text"],
                    &["tools", "reasoning"],
                    Some(200_000),
                ),
                OpenRouterModelReasoning {
                    supported_efforts: SupportedEfforts::List(vec![
                        "high".to_owned(),
                        "medium".to_owned(),
                        "low".to_owned(),
                        "minimal".to_owned(),
                    ]),
                    default_effort: Some("medium".to_owned()),
                    default_enabled: Some(true),
                    mandatory: Some(true),
                },
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
        ]);
        let entries = catalog.entries();
        assert_eq!(entries.len(), 2);
        assert!(entries.contains_key("openrouter:google/gemini-3.5-flash"));
        assert!(entries.contains_key("openrouter:openai/gpt-4o"));
        assert!(!entries.contains_key("openrouter:meta-llama/llama-3.1-8b-instruct"));
        assert_eq!(
            entries["openrouter:google/gemini-3.5-flash"]
                .info
                .api_backend,
            ApiBackend::ChatCompletions
        );
        assert_eq!(
            entries["openrouter:google/gemini-3.5-flash"]
                .info
                .context_window
                .get(),
            200_000
        );
        assert_eq!(
            entries["openrouter:google/gemini-3.5-flash"]
                .info
                .max_completion_tokens,
            Some(8_192)
        );
        let gemini_efforts = &entries["openrouter:google/gemini-3.5-flash"]
            .info
            .reasoning_efforts;
        assert_eq!(
            gemini_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::High,
                ReasoningEffort::Medium,
                ReasoningEffort::Low,
                ReasoningEffort::Minimal,
            ]
        );
        assert!(
            !gemini_efforts
                .iter()
                .any(|option| option.value == ReasoningEffort::None)
        );
        assert_eq!(
            entries["openrouter:google/gemini-3.5-flash"]
                .info
                .reasoning_effort,
            Some(ReasoningEffort::Medium)
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

        let cfg = crate::agent::config::Config::default();
        let disabled = resolve_openrouter(&cfg, &catalog);
        assert!(
            disabled
                .values()
                .all(|entry| entry.info.provider != ModelProvider::OpenRouter),
            "OpenRouter must default to no enabled models",
        );

        let mut filtered = crate::agent::config::Config::default();
        filtered.models.openrouter_enabled_models = vec!["openai/gpt-4o".to_owned()];
        let enabled = resolve_openrouter(&filtered, &catalog);
        assert!(enabled.contains_key("openrouter:openai/gpt-4o"));
        assert!(!enabled.contains_key("openrouter:google/gemini-3.5-flash"));
        assert!(!enabled.contains_key("openrouter:meta-llama/llama-3.1-8b-instruct"));
    }

    #[test]
    fn catalog_checks_text_input_and_output_independently() {
        let cases = [
            (
                serde_json::json!({"input_modalities": ["audio"], "output_modalities": ["text"]}),
                Some("no text input"),
            ),
            (
                serde_json::json!({"input_modalities": ["text"], "output_modalities": ["image"]}),
                Some("no text output"),
            ),
            (
                serde_json::json!({"input_modalities": ["image", "TEXT"], "output_modalities": ["text", "image"]}),
                None,
            ),
            (
                serde_json::json!({"modality": "audio->text"}),
                Some("no text input"),
            ),
            (
                serde_json::json!({"modality": "text->embeddings"}),
                Some("no text output"),
            ),
            (
                serde_json::json!({"modality": "text+image->image+text"}),
                None,
            ),
            // Explicit modality arrays take precedence over legacy metadata.
            (
                serde_json::json!({"input_modalities": ["audio"], "modality": "text->text"}),
                Some("no text input"),
            ),
            (
                serde_json::json!({"output_modalities": ["text"], "modality": "text->image"}),
                None,
            ),
            // Preserve compatibility with catalogs that omit architecture.
            (serde_json::json!({}), None),
            (serde_json::Value::Null, None),
        ];
        for (architecture, expected) in cases {
            let wire: OpenRouterWireModel = serde_json::from_value(serde_json::json!({
                "id": "test/model",
                "architecture": architecture,
                "supported_parameters": ["tools"],
            }))
            .expect("architecture fixture");
            assert_eq!(skip_reason(&wire), expected, "{architecture}");
        }
    }

    #[test]
    fn catalog_reasoning_metadata_never_invents_efforts_or_defaults() {
        use ReasoningEffort::{High, Low, Medium, None as Off};
        let cases = [
            (serde_json::Value::Null, vec![], None),
            (serde_json::json!({"mandatory": true}), vec![], None),
            (serde_json::json!({"supported_efforts": []}), vec![], None),
            (
                serde_json::json!({"supported_efforts": ["ultra", "future"]}),
                vec![],
                None,
            ),
            (
                serde_json::json!({"supported_efforts": ["high", "HIGH", "medium", "ultra", "future"], "default_enabled": true, "default_effort": "medium"}),
                vec![High, Medium],
                Some(Medium),
            ),
            (
                serde_json::json!({"supported_efforts": ["none", "low"], "default_enabled": false}),
                vec![Off, Low],
                Some(Off),
            ),
            (
                serde_json::json!({"supported_efforts": ["high", "low"], "default_enabled": false}),
                vec![High, Low],
                None,
            ),
            (
                serde_json::json!({"supported_efforts": ["high", "none"], "default_enabled": true, "default_effort": "none"}),
                vec![High, Off],
                None,
            ),
            (
                serde_json::json!({"supported_efforts": ["high", "low"], "default_effort": "high"}),
                vec![High, Low],
                None,
            ),
            (
                serde_json::json!({"supported_efforts": ["high", "low"], "default_enabled": true, "default_effort": "future"}),
                vec![High, Low],
                None,
            ),
            (
                serde_json::json!({"supported_efforts": ["high", "low"], "default_enabled": true, "default_effort": "medium"}),
                vec![High, Low],
                None,
            ),
            (
                serde_json::json!({"supported_efforts": ["none", "low"], "mandatory": true, "default_effort": "none"}),
                vec![Low],
                None,
            ),
            (
                serde_json::json!({"supported_efforts": ["none", "low"], "mandatory": true, "default_effort": "low"}),
                vec![Low],
                Some(Low),
            ),
            (
                serde_json::json!({"supported_efforts": ["none"], "mandatory": true}),
                vec![],
                None,
            ),
            (
                serde_json::json!({"supported_efforts": null}),
                OPENROUTER_GATEWAY_EFFORTS.to_vec(),
                None,
            ),
        ];
        let mut cfg = crate::agent::config::Config::default();
        cfg.models.openrouter_enabled_models = vec!["test/model".to_owned()];
        for (reasoning, expected_values, expected_default) in cases {
            let wire: OpenRouterWireModel = serde_json::from_value(serde_json::json!({
                "id": "test/model",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "supported_parameters": ["tools", "reasoning"],
                "reasoning": reasoning,
            }))
            .expect("reasoning fixture");
            let catalog = catalog_from(vec![wire]);
            let resolved = resolve_openrouter(&cfg, &catalog);
            let info = &resolved["openrouter:test/model"].info;
            assert_eq!(
                info.reasoning_efforts
                    .iter()
                    .map(|option| option.value)
                    .collect::<Vec<_>>(),
                expected_values,
                "{reasoning}",
            );
            assert_eq!(info.reasoning_effort, expected_default, "{reasoning}");
            assert_eq!(
                info.supports_reasoning_effort,
                !expected_values.is_empty(),
                "{reasoning}"
            );
            assert_eq!(
                info.reasoning_efforts
                    .iter()
                    .filter(|option| option.default)
                    .count(),
                usize::from(expected_default.is_some()),
                "{reasoning}",
            );
        }
    }

    #[test]
    fn catalog_omits_effort_menu_when_supported_efforts_is_absent() {
        let catalog = catalog_from(vec![
            with_reasoning(
                wire(
                    "moonshotai/kimi-k2.7-code",
                    "Kimi K2.7 Code",
                    &["text"],
                    &["tools", "reasoning"],
                    Some(200_000),
                ),
                OpenRouterModelReasoning {
                    supported_efforts: SupportedEfforts::Omitted,
                    default_effort: None,
                    default_enabled: Some(true),
                    mandatory: Some(true),
                },
            ),
            wire(
                "openai/gpt-4o",
                "OpenAI: GPT-4o",
                &["text"],
                &["tools", "reasoning"],
                Some(128_000),
            ),
        ]);
        let kimi = &catalog.entries()["openrouter:moonshotai/kimi-k2.7-code"].info;
        assert!(!kimi.supports_reasoning_effort);
        assert!(kimi.reasoning_efforts.is_empty());
        let gpt = &catalog.entries()["openrouter:openai/gpt-4o"].info;
        assert!(!gpt.supports_reasoning_effort);
        assert!(gpt.reasoning_efforts.is_empty());
    }

    #[test]
    fn catalog_pulls_gateway_efforts_when_supported_efforts_is_null() {
        let catalog = catalog_from(vec![with_reasoning(
            wire(
                "openai/gpt-5",
                "OpenAI: GPT-5",
                &["text"],
                &["tools", "reasoning"],
                Some(400_000),
            ),
            OpenRouterModelReasoning {
                supported_efforts: SupportedEfforts::All,
                default_effort: Some("medium".to_owned()),
                default_enabled: Some(true),
                mandatory: Some(false),
            },
        )]);
        let efforts = &catalog.entries()["openrouter:openai/gpt-5"]
            .info
            .reasoning_efforts;
        assert_eq!(
            efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            OPENROUTER_GATEWAY_EFFORTS.to_vec()
        );
        assert_eq!(
            catalog.entries()["openrouter:openai/gpt-5"]
                .info
                .reasoning_effort,
            Some(ReasoningEffort::Medium)
        );
    }

    #[test]
    fn catalog_deserializes_live_reasoning_object() {
        let parsed: OpenRouterModelsResponse = serde_json::from_str(
            r#"{
                "data": [{
                    "id": "google/gemini-3.5-flash",
                    "name": "Gemini",
                    "supported_parameters": ["tools", "reasoning"],
                    "architecture": {"output_modalities": ["text"]},
                    "reasoning": {
                        "supported_efforts": ["high", "medium", "low", "minimal"],
                        "default_effort": "medium",
                        "default_enabled": true,
                        "mandatory": true
                    }
                }, {
                    "id": "openai/o3",
                    "name": "o3",
                    "supported_parameters": ["tools", "reasoning"],
                    "architecture": {"output_modalities": ["text"]},
                    "reasoning": {
                        "supported_efforts": null,
                        "default_effort": "medium",
                        "default_enabled": true,
                        "mandatory": false
                    }
                }]
            }"#,
        )
        .expect("live OpenRouter reasoning payload");
        let catalog = OpenRouterModelsClient::with_url(OPENROUTER_API_BASE_URL)
            .catalog_from_wire(parsed, "secret");
        let gemini = &catalog.entries()["openrouter:google/gemini-3.5-flash"].info;
        assert_eq!(
            gemini
                .reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            vec![
                ReasoningEffort::High,
                ReasoningEffort::Medium,
                ReasoningEffort::Low,
                ReasoningEffort::Minimal,
            ]
        );
        assert_eq!(gemini.reasoning_effort, Some(ReasoningEffort::Medium));
        let o3 = &catalog.entries()["openrouter:openai/o3"].info;
        assert_eq!(
            o3.reasoning_efforts
                .iter()
                .map(|option| option.value)
                .collect::<Vec<_>>(),
            OPENROUTER_GATEWAY_EFFORTS.to_vec()
        );
    }
}
