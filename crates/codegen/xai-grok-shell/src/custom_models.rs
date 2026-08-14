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
    pub api_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    /// Persist only when the user typed one. Prefer [`Self::env_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
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
    pub api_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
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

    validate_model_key(&record.key)?;
    validate_model_id(&record.model)?;
    if record.context_window == Some(0) {
        bail!("context_window must be greater than 0");
    }

    if let Some(raw) = record.provider.as_deref() {
        record.provider = Some(parse_provider(raw)?.as_str().to_owned());
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
            api_backend: self.api_backend.clone(),
            env_key: self.env_key.clone(),
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
        if let Some(api_backend) = &self.api_backend {
            table.insert("api_backend".into(), TomlValue::String(api_backend.clone()));
        }
        if let Some(env_key) = &self.env_key {
            table.insert("env_key".into(), TomlValue::String(env_key.clone()));
        }
        if let Some(api_key) = &self.api_key {
            table.insert("api_key".into(), TomlValue::String(api_key.clone()));
        }
        table
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
            api_backend: self
                .api_backend
                .as_deref()
                .and_then(|raw| parse_api_backend(raw).ok()),
            env_key: self.env_key.clone().map(EnvKeys::single),
            api_key: self.api_key.clone(),
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
        api_backend: model.api_backend.map(api_backend_as_str).map(str::to_owned),
        env_key: model
            .env_key
            .as_ref()
            .and_then(EnvKeys::primary)
            .map(str::to_owned),
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
        other => bail!(
            "invalid provider `{other}`; expected xai, codex, kimi, fireworks, \
             deepseek, meta, wafer, zai, or opencode_go"
        ),
    }
}

fn parse_api_backend(raw: &str) -> Result<ApiBackend> {
    match raw.trim() {
        "chat_completions" => Ok(ApiBackend::ChatCompletions),
        "responses" => Ok(ApiBackend::Responses),
        "messages" => Ok(ApiBackend::Messages),
        other => bail!(
            "invalid api_backend `{other}`; expected chat_completions, responses, or messages"
        ),
    }
}

fn api_backend_as_str(backend: ApiBackend) -> &'static str {
    match backend {
        ApiBackend::ChatCompletions => "chat_completions",
        ApiBackend::Responses => "responses",
        ApiBackend::Messages => "messages",
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
}
