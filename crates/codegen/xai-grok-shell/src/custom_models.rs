//! Custom `[model.<key>]` records used by Settings and ACP RPCs.
//!
//! Persistence lives in `util/config`; this module owns validation and the
//! public record shape. `api_key` is accepted on upsert only and is never
//! serialized on list/mutation responses.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use toml::Value as TomlValue;
use toml::map::Map as TomlMap;
use xai_grok_sampling_types::{ApiBackend, ModelProvider};

use crate::agent::config::{ConfigModelOverride, EnvKeys};

/// Wire record for `open-grok/custom-models/upsert`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomModelRecord {
    pub key: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// Persist only when the user typed one. Prefer [`Self::env_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Credential header: `bearer` or `x_api_key`.
    ///
    /// Written by the custom-provider wizard from the chosen wire format, and by
    /// any hand-written `[model.*]` table. Left unset for built-in providers,
    /// whose identity already fixes the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scheme: Option<String>,
}

/// List/mutation record. Never includes `api_key`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomModelPublicRecord {
    pub key: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_token_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// Resolved credential header for this row, never the credential itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scheme: Option<String>,
    pub has_api_key: bool,
}

/// Trim, validate, and apply provider endpoint defaults.
///
/// `api_key` is dropped when `env_key` is set. Missing Z AI / Wafer
/// `base_url` is filled so [`ConfigModelOverride::apply`] does not clear
/// the endpoint for api-key-only providers.
pub fn normalize_custom_model(
    mut record: CustomModelRecord,
) -> Result<(CustomModelRecord, Option<String>)> {
    record.key = record.key.trim().to_owned();
    record.model = record.model.trim().to_owned();
    record.name = trim_opt(record.name);
    record.provider = trim_opt(record.provider);
    record.base_url = trim_opt(record.base_url);
    record.api_backend = trim_opt(record.api_backend);
    record.env_key = trim_opt(record.env_key);
    record.api_key = trim_opt(record.api_key);
    record.auth_scheme = trim_opt(record.auth_scheme);
    if let Some(raw) = record.auth_scheme.as_deref() {
        // Reject a typo here rather than letting it silently fall back to a
        // bearer token against a server that wants `x-api-key`.
        parse_auth_scheme(raw)?;
    }

    validate_model_key(&record.key)?;
    validate_model_id(&record.model)?;
    if record.max_context_window == Some(0) || record.auto_compact_token_limit == Some(0) {
        bail!("max_context_window and auto_compact_token_limit must be greater than 0");
    }
    if record.context_window == Some(0) {
        bail!("context_window must be greater than 0");
    }

    if let Some(raw) = record.provider.as_deref() {
        record.provider = Some(parse_provider(raw)?.as_str().to_owned());
    }
    if record.max_context_window.is_some()
        && record
            .provider
            .as_deref()
            .is_some_and(|provider| provider != "codex")
    {
        bail!(
            "max_context_window is a Codex raw-context override; use context_window for this provider"
        );
    }
    if let Some(raw) = record.api_backend.as_deref() {
        record.api_backend = Some(api_backend_as_str(parse_api_backend(raw)?).to_owned());
    }

    let mut warning = None;
    if record.env_key.is_some() && record.api_key.is_some() {
        record.api_key = None;
        warning = Some("api_key was omitted because env_key is set".to_owned());
    }

    apply_provider_endpoint_defaults(&mut record);
    Ok((record, warning))
}

pub fn validate_model_key(key: &str) -> Result<()> {
    if key.is_empty() {
        bail!("custom model key must be non-empty");
    }
    if key.contains('\n') || key.contains('\r') {
        bail!("custom model key must not contain newlines");
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_'))
    {
        bail!(
            "custom model key `{key}` is not a valid TOML table suffix \
             (letters, digits, `:`, `.`, `-`, `_`)"
        );
    }
    Ok(())
}

pub fn validate_model_id(model: &str) -> Result<()> {
    if model.is_empty() {
        bail!("custom model id must be non-empty");
    }
    if model.contains('\n') || model.contains('\r') {
        bail!("custom model id must not contain newlines");
    }
    Ok(())
}

impl CustomModelRecord {
    pub fn to_public(&self) -> CustomModelPublicRecord {
        CustomModelPublicRecord {
            key: self.key.clone(),
            model: self.model.clone(),
            name: self.name.clone(),
            provider: self.provider.clone(),
            base_url: self.base_url.clone(),
            context_window: self.context_window,

            max_context_window: self.max_context_window,

            auto_compact_token_limit: self.auto_compact_token_limit,
            api_backend: self.api_backend.clone(),
            env_key: self.env_key.clone(),
            auth_scheme: self
                .resolved_auth_scheme()
                .map(auth_scheme_as_str)
                .map(str::to_owned),
            has_api_key: has_secret(&self.api_key),
        }
    }

    pub fn to_toml_table(&self) -> TomlMap<String, TomlValue> {
        let mut table = TomlMap::new();
        table.insert("model".into(), TomlValue::String(self.model.clone()));
        if let Some(name) = &self.name {
            table.insert("name".into(), TomlValue::String(name.clone()));
        }
        if let Some(provider) = &self.provider {
            table.insert("provider".into(), TomlValue::String(provider.clone()));
        }
        if let Some(base_url) = &self.base_url {
            table.insert("base_url".into(), TomlValue::String(base_url.clone()));
        }
        if let Some(context_window) = self.context_window {
            table.insert(
                "context_window".into(),
                TomlValue::Integer(i64::try_from(context_window).unwrap_or(i64::MAX)),
            );
        }
        for (key, value) in [
            ("max_context_window", self.max_context_window),
            ("auto_compact_token_limit", self.auto_compact_token_limit),
        ] {
            if let Some(value) = value {
                table.insert(
                    key.into(),
                    TomlValue::Integer(value.min(i64::MAX as u64) as i64),
                );
            }
        }
        if let Some(api_backend) = &self.api_backend {
            table.insert("api_backend".into(), TomlValue::String(api_backend.clone()));
        }
        if let Some(env_key) = &self.env_key {
            table.insert("env_key".into(), TomlValue::String(env_key.clone()));
        }
        if let Some(api_key) = &self.api_key {
            table.insert("api_key".into(), TomlValue::String(api_key.clone()));
        }
        if let Some(scheme) = self.resolved_auth_scheme() {
            table.insert(
                "auth_scheme".into(),
                TomlValue::String(auth_scheme_as_str(scheme).to_owned()),
            );
        }
        table
    }

    /// The credential header this row should carry.
    ///
    /// An explicit `auth_scheme` always wins. Otherwise only a user endpoint
    /// speaking Anthropic Messages is assumed to be a native Anthropic server
    /// (`x-api-key`); an OpenAI-compatible server keeps the historical bearer
    /// default, which is also what a gateway or proxy expects.
    pub fn resolved_auth_scheme(&self) -> Option<xai_grok_sampler::AuthScheme> {
        if let Some(raw) = self.auth_scheme.as_deref() {
            return parse_auth_scheme(raw).ok();
        }
        // Only a row that explicitly declares `provider = "custom"` is a user
        // endpoint. A row without `provider` keeps the built-in default (xAI),
        // so deriving a header for it would silently change first-party auth.
        let is_user_endpoint = self
            .provider
            .as_deref()
            .and_then(|raw| parse_provider(raw).ok())
            == Some(ModelProvider::Custom);
        if is_user_endpoint {
            match self.api_backend.as_deref() {
                Some("messages") => return Some(xai_grok_sampler::AuthScheme::XApiKey),
                Some("google_ai_studio" | "ai_studio" | "gemini" | "google") => {
                    return Some(xai_grok_sampler::AuthScheme::XGoogApiKey);
                }
                _ => {}
            }
        }
        None
    }

    pub fn to_override(&self) -> ConfigModelOverride {
        ConfigModelOverride {
            model: Some(self.model.clone()),
            name: self.name.clone(),
            provider: self
                .provider
                .as_deref()
                .and_then(|raw| parse_provider(raw).ok()),
            base_url: self.base_url.clone(),
            context_window: self.context_window,

            max_context_window: self.max_context_window,

            auto_compact_token_limit: self.auto_compact_token_limit,
            api_backend: self
                .api_backend
                .as_deref()
                .and_then(|raw| parse_api_backend(raw).ok()),
            env_key: self.env_key.clone().map(EnvKeys::single),
            api_key: self.api_key.clone(),
            auth_scheme: self.resolved_auth_scheme(),
            ..ConfigModelOverride::default()
        }
    }
}

pub fn override_to_public(key: &str, model: &ConfigModelOverride) -> CustomModelPublicRecord {
    CustomModelPublicRecord {
        key: key.to_owned(),
        model: model.model.clone().unwrap_or_else(|| key.to_owned()),
        name: model.name.clone(),
        provider: model.provider.map(ModelProvider::as_str).map(str::to_owned),
        base_url: model.base_url.clone(),
        context_window: model.context_window,

        max_context_window: model.max_context_window,

        auto_compact_token_limit: model.auto_compact_token_limit,
        api_backend: model.api_backend.map(api_backend_as_str).map(str::to_owned),
        env_key: model
            .env_key
            .as_ref()
            .and_then(EnvKeys::primary)
            .map(str::to_owned),
        auth_scheme: model.auth_scheme.map(auth_scheme_as_str).map(str::to_owned),
        has_api_key: has_secret(&model.api_key),
    }
}

fn apply_provider_endpoint_defaults(record: &mut CustomModelRecord) {
    match record.provider.as_deref() {
        Some("zai") if record.base_url.is_none() => {
            record.base_url = Some(crate::zai_models::api_base_url());
            if record.env_key.is_none() {
                record.env_key = Some(crate::zai_models::ZAI_API_KEY_ENV.to_owned());
            }
        }
        Some("wafer") if record.base_url.is_none() => {
            record.base_url = Some(crate::wafer_models::api_base_url());
            if record.env_key.is_none() {
                record.env_key = Some(crate::wafer_models::WAFER_API_KEY_ENV.to_owned());
            }
        }
        Some("runinfra" | "run_infra" | "run-infra") if record.base_url.is_none() => {
            record.base_url = Some(crate::runinfra_models::api_base_url());
            if record.env_key.is_none() {
                record.env_key = Some(crate::runinfra_models::RUNINFRA_GATEWAY_KEY_ENV.to_owned());
            }
        }
        Some("gemini" | "google" | "google_gemini" | "ai_studio" | "aistudio" | "gemini_api")
            if record.base_url.is_none() =>
        {
            record.base_url = Some(crate::gemini_models::api_base_url());
            if record.env_key.is_none() {
                record.env_key = Some(crate::gemini_models::GEMINI_API_KEY_ENV.to_owned());
            }
        }
        Some("openrouter" | "open_router" | "open-router") if record.base_url.is_none() => {
            record.base_url = Some(crate::openrouter_models::api_base_url());
            if record.env_key.is_none() {
                record.env_key = Some(crate::openrouter_models::OPENROUTER_API_KEY_ENV.to_owned());
            }
        }
        _ => {}
    }
}

fn parse_provider(raw: &str) -> Result<ModelProvider> {
    match raw.trim() {
        "xai" => Ok(ModelProvider::Xai),
        "codex" | "openai" | "openai_codex" => Ok(ModelProvider::Codex),
        "kimi" | "moonshot" | "moonshot_ai" => Ok(ModelProvider::Kimi),
        "fireworks" | "fireworks_ai" => Ok(ModelProvider::Fireworks),
        "deepseek" | "deep_seek" | "deepseek_api" => Ok(ModelProvider::DeepSeek),
        "meta" | "meta_ai" | "meta_api" => Ok(ModelProvider::Meta),
        "opencode_go" | "opencode-go" | "open_code_go" => Ok(ModelProvider::OpenCodeGo),
        "wafer" | "wafer_ai" => Ok(ModelProvider::Wafer),
        "zai" | "z_ai" | "z-ai" => Ok(ModelProvider::Zai),
        "runinfra" | "run_infra" | "run-infra" => Ok(ModelProvider::Runinfra),
        "gemini" | "google" | "google_gemini" | "ai_studio" | "aistudio" | "gemini_api" => {
            Ok(ModelProvider::Gemini)
        }
        "openrouter" | "open_router" | "open-router" => Ok(ModelProvider::OpenRouter),
        "custom" | "custom_endpoint" | "custom endpoint" | "byo" | "byok" => {
            Ok(ModelProvider::Custom)
        }
        other => bail!(
            "invalid provider `{other}`; expected xai, codex, kimi, fireworks, \
             deepseek, meta, wafer, zai, runinfra, gemini, opencode_go, openrouter, \
             or custom"
        ),
    }
}

/// Parse the `auth_scheme` config value (`bearer`, `x_api_key`, or `x_goog_api_key`).
fn parse_auth_scheme(raw: &str) -> Result<xai_grok_sampler::AuthScheme> {
    serde_json::from_value::<xai_grok_sampler::AuthScheme>(serde_json::Value::String(
        raw.trim().to_owned(),
    ))
    .map_err(|_| {
        anyhow::anyhow!(
            "invalid auth_scheme `{raw}`; expected bearer, x_api_key, or x_goog_api_key"
        )
    })
}

fn auth_scheme_as_str(scheme: xai_grok_sampler::AuthScheme) -> &'static str {
    match scheme {
        xai_grok_sampler::AuthScheme::Bearer => "bearer",
        xai_grok_sampler::AuthScheme::XApiKey => "x_api_key",
        xai_grok_sampler::AuthScheme::XGoogApiKey => "x_goog_api_key",
    }
}

fn parse_api_backend(raw: &str) -> Result<ApiBackend> {
    match raw.trim() {
        "chat_completions" => Ok(ApiBackend::ChatCompletions),
        "responses" => Ok(ApiBackend::Responses),
        "messages" => Ok(ApiBackend::Messages),
        "google_ai_studio" | "ai_studio" | "gemini" | "google" => Ok(ApiBackend::GoogleAiStudio),
        other => bail!(
            "invalid api_backend `{other}`; expected chat_completions, responses, messages, or google_ai_studio"
        ),
    }
}

fn api_backend_as_str(backend: ApiBackend) -> &'static str {
    match backend {
        ApiBackend::ChatCompletions => "chat_completions",
        ApiBackend::Responses => "responses",
        ApiBackend::Messages => "messages",
        ApiBackend::GoogleAiStudio => "google_ai_studio",
    }
}

fn trim_opt(value: Option<String>) -> Option<String> {
    value.and_then(|raw| {
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn has_secret(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|key| !key.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(key: &str, model: &str) -> CustomModelRecord {
        CustomModelRecord {
            key: key.to_owned(),
            model: model.to_owned(),
            ..CustomModelRecord::default()
        }
    }

    #[test]
    fn rejects_empty_key_and_model() {
        let err = normalize_custom_model(record("  ", "glm-5.2")).unwrap_err();
        assert!(err.to_string().contains("key must be non-empty"), "{err}");
        let err = normalize_custom_model(record("my-ollama", " \n")).unwrap_err();
        assert!(err.to_string().contains("id must be non-empty"), "{err}");
    }

    #[test]
    fn rejects_newlines_and_invalid_key_chars() {
        let err = normalize_custom_model(record("zai:\nextra", "glm")).unwrap_err();
        assert!(err.to_string().contains("newlines"), "{err}");
        let err = normalize_custom_model(record("zai extra", "glm")).unwrap_err();
        assert!(err.to_string().contains("TOML table suffix"), "{err}");
        let err = normalize_custom_model(record("zai/extra", "glm")).unwrap_err();
        assert!(err.to_string().contains("TOML table suffix"), "{err}");
        let err = normalize_custom_model(CustomModelRecord {
            key: "ok".into(),
            model: "glm\n5".into(),
            ..CustomModelRecord::default()
        })
        .unwrap_err();
        assert!(err.to_string().contains("newlines"), "{err}");
    }

    #[test]
    fn accepts_colon_dot_hyphen_underscore_keys() {
        let (got, warning) = normalize_custom_model(record("zai:glm-special.v1_x", "glm-special"))
            .expect("valid key");
        assert!(warning.is_none());
        assert_eq!(got.key, "zai:glm-special.v1_x");
        assert_eq!(got.model, "glm-special");
    }

    #[test]
    fn rejects_zero_context_window() {
        let err = normalize_custom_model(CustomModelRecord {
            context_window: Some(0),
            ..record("my-ollama", "llama")
        })
        .unwrap_err();
        assert!(err.to_string().contains("greater than 0"), "{err}");
    }

    #[test]
    fn rejects_unknown_provider_and_backend() {
        let err = normalize_custom_model(CustomModelRecord {
            provider: Some("anthropic".into()),
            ..record("k", "m")
        })
        .unwrap_err();
        assert!(err.to_string().contains("invalid provider"), "{err}");
        let err = normalize_custom_model(CustomModelRecord {
            api_backend: Some("grpc".into()),
            ..record("k", "m")
        })
        .unwrap_err();
        assert!(err.to_string().contains("invalid api_backend"), "{err}");
    }

    #[test]
    fn zai_and_wafer_fill_default_endpoint_and_env_key() {
        let (zai, _) = normalize_custom_model(CustomModelRecord {
            provider: Some("zai".into()),
            ..record("zai:extra", "glm-extra")
        })
        .unwrap();
        assert_eq!(
            zai.base_url.as_deref(),
            Some(crate::zai_models::api_base_url().as_str())
        );
        assert_eq!(
            zai.env_key.as_deref(),
            Some(crate::zai_models::ZAI_API_KEY_ENV)
        );

        let (wafer, _) = normalize_custom_model(CustomModelRecord {
            provider: Some("wafer".into()),
            ..record("wafer:extra", "wafer-extra")
        })
        .unwrap();
        assert_eq!(
            wafer.base_url.as_deref(),
            Some(crate::wafer_models::api_base_url().as_str())
        );
        assert_eq!(
            wafer.env_key.as_deref(),
            Some(crate::wafer_models::WAFER_API_KEY_ENV)
        );

        let (runinfra, _) = normalize_custom_model(CustomModelRecord {
            provider: Some("runinfra".into()),
            ..record("runinfra:extra", "workspace-deploy")
        })
        .unwrap();
        assert_eq!(
            runinfra.base_url.as_deref(),
            Some(crate::runinfra_models::api_base_url().as_str())
        );
        assert_eq!(
            runinfra.env_key.as_deref(),
            Some(crate::runinfra_models::RUNINFRA_GATEWAY_KEY_ENV)
        );

        let (gemini, _) = normalize_custom_model(CustomModelRecord {
            provider: Some("gemini".into()),
            ..record("gemini:extra", "gemini-extra")
        })
        .unwrap();
        assert_eq!(
            gemini.base_url.as_deref(),
            Some(crate::gemini_models::api_base_url().as_str())
        );
        assert_eq!(
            gemini.env_key.as_deref(),
            Some(crate::gemini_models::GEMINI_API_KEY_ENV)
        );
    }

    #[test]
    fn prefers_env_key_over_typed_api_key() {
        let (got, warning) = normalize_custom_model(CustomModelRecord {
            env_key: Some("MY_KEY".into()),
            api_key: Some("sk-secret".into()),
            ..record("my-ollama", "llama")
        })
        .unwrap();
        assert_eq!(got.env_key.as_deref(), Some("MY_KEY"));
        assert!(got.api_key.is_none());
        assert_eq!(
            warning.as_deref(),
            Some("api_key was omitted because env_key is set")
        );
        let public = got.to_public();
        assert!(!public.has_api_key);
        let json = serde_json::to_value(&public).unwrap();
        assert!(json.get("api_key").is_none());
    }

    #[test]
    fn custom_provider_is_accepted_and_canonicalized() {
        for raw in [
            "custom",
            "custom_endpoint",
            "custom endpoint",
            "byo",
            "byok",
        ] {
            let (got, _) = normalize_custom_model(CustomModelRecord {
                provider: Some(raw.to_owned()),
                ..record("localhost-11434:qwen3", "qwen3:latest")
            })
            .unwrap();
            assert_eq!(got.provider.as_deref(), Some("custom"), "for `{raw}`");
        }
        let err = normalize_custom_model(CustomModelRecord {
            provider: Some("teleprompter".into()),
            ..record("k", "m")
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("or custom"), "{err}");
    }

    /// A user endpoint speaking Anthropic Messages is assumed native and gets
    /// `x-api-key`; OpenAI-compatible endpoints and every built-in provider keep
    /// the bearer default, and an explicit value always wins.
    #[test]
    fn auth_scheme_is_derived_only_for_a_custom_messages_endpoint() {
        let cases = [
            (Some("custom"), Some("messages"), Some("x_api_key")),
            (Some("custom"), Some("responses"), None),
            (Some("custom"), Some("chat_completions"), None),
            (Some("custom"), None, None),
            (Some("zai"), Some("messages"), None),
            (None, Some("messages"), None),
        ];
        for (provider, api_backend, expected) in cases {
            let (got, _) = normalize_custom_model(CustomModelRecord {
                provider: provider.map(str::to_owned),
                api_backend: api_backend.map(str::to_owned),
                ..record("k-m", "model-id")
            })
            .unwrap();
            assert_eq!(
                got.to_public().auth_scheme.as_deref(),
                expected,
                "provider={provider:?} api_backend={api_backend:?}"
            );
        }
        let explicit = CustomModelRecord {
            provider: Some("custom".into()),
            api_backend: Some("messages".into()),
            auth_scheme: Some("bearer".into()),
            ..record("proxy-gateway:claude", "claude-x")
        };
        assert_eq!(
            explicit.resolved_auth_scheme(),
            Some(xai_grok_sampler::AuthScheme::Bearer)
        );
        let json = serde_json::to_value(explicit.to_public()).unwrap();
        assert_eq!(json.get("auth_scheme"), Some(&serde_json::json!("bearer")));
    }

    #[test]
    fn auth_scheme_lands_in_toml_the_override_and_the_public_record() {
        let (record, _) = normalize_custom_model(CustomModelRecord {
            provider: Some("custom".into()),
            api_backend: Some("messages".into()),
            base_url: Some("https://gateway.example.com/anthropic".into()),
            api_key: Some("sk-ant-user".into()),
            ..record("gateway.example.com:claude-x", "claude-x")
        })
        .unwrap();
        let table = record.to_toml_table();
        assert_eq!(
            table.get("auth_scheme").and_then(TomlValue::as_str),
            Some("x_api_key")
        );
        assert_eq!(
            record.to_override().auth_scheme,
            Some(xai_grok_sampler::AuthScheme::XApiKey)
        );
        let public = override_to_public("gateway.example.com:claude-x", &record.to_override());
        assert_eq!(public.auth_scheme.as_deref(), Some("x_api_key"));
        assert!(public.has_api_key);
    }

    #[test]
    fn a_misspelled_auth_scheme_is_rejected_rather_than_ignored() {
        let err = normalize_custom_model(CustomModelRecord {
            auth_scheme: Some("x-api-key".into()),
            ..record("k", "m")
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("invalid auth_scheme"), "{err}");
        assert!(err.contains("x_api_key"), "{err}");
    }

    #[test]
    fn public_record_flags_stored_api_key_without_echoing_it() {
        let record = CustomModelRecord {
            api_key: Some("sk-secret".into()),
            ..record("my-ollama", "llama")
        };
        let public = record.to_public();
        assert!(public.has_api_key);
        let json = serde_json::to_value(&public).unwrap();
        assert!(json.get("api_key").is_none());
        assert_eq!(json.get("has_api_key"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn google_ai_studio_api_backend_and_auth_scheme_in_custom_models() {
        let (record, _) = normalize_custom_model(CustomModelRecord {
            provider: Some("custom".into()),
            api_backend: Some("google_ai_studio".into()),
            base_url: Some("https://generativelanguage.googleapis.com/v1beta".into()),
            api_key: Some("AIzaSyTestKey".into()),
            ..record("gemini-custom", "gemini-2.5-flash")
        })
        .unwrap();
        assert_eq!(
            record.to_override().api_backend,
            Some(ApiBackend::GoogleAiStudio)
        );
        assert_eq!(
            record.to_override().auth_scheme,
            Some(xai_grok_sampler::AuthScheme::XGoogApiKey)
        );
        let public = override_to_public("gemini-custom", &record.to_override());
        assert_eq!(public.api_backend.as_deref(), Some("google_ai_studio"));
        assert_eq!(public.auth_scheme.as_deref(), Some("x_goog_api_key"));
    }
}
