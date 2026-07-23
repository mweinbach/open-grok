//! Provider-specific transport policy for the sampling client.
//!
//! Authentication is intentionally not part of this adapter. API-key and
//! bearer resolution remain owned by [`crate::config::AuthScheme`] and
//! [`crate::config::BearerResolver`]; this module only projects a provider's
//! request and Responses-wire behavior.

use reqwest::RequestBuilder;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use xai_grok_sampling_types::{
    ApiBackend, ChatCompletionRequest, ModelProvider, ProviderProfile, ReasoningEffort,
    ReasoningSummary, RequestMetadataPolicy, ResponsesDialect, SamplingError,
};

use crate::config::SamplerConfig;

/// Process-level fallback for the `x-grok-client-identifier` header.
const DEFAULT_CLIENT_IDENTIFIER: &str = "grok-shell";
pub(crate) const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";

pub(crate) const MULTI_AGENT_MODE_OPEN_TAG: &str = "<multi_agent_mode>";
pub(crate) const MULTI_AGENT_MODE_CLOSE_TAG: &str = "</multi_agent_mode>";
pub(crate) const EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT: &str = "Any earlier instruction enabling proactive multi-agent delegation no longer applies. Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
pub(crate) const PROACTIVE_MULTI_AGENT_MODE_TEXT: &str = "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode developer message changes it.";

/// Provider-neutral input to the Responses request patching seam.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResponsesRequestPolicy {
    pub multi_agent_v2: bool,
    pub local_effort: Option<ReasoningEffort>,
    pub reasoning_summary: Option<ReasoningSummary>,
}

/// Request metadata available to providers that use the xAI proxy contract.
#[derive(Clone, Copy, Debug)]
pub struct ProviderRequestHeaders<'a> {
    pub conv_id: &'a str,
    pub req_id: &'a str,
    pub model_id: &'a str,
    pub session_id: &'a str,
    pub turn_idx: Option<&'a str>,
    pub agent_id: &'a str,
    pub deployment_id: Option<&'a str>,
    pub user_id: Option<&'a str>,
}

impl ProviderRequestHeaders<'_> {
    fn apply_x_grok(self, builder: RequestBuilder) -> RequestBuilder {
        let mut builder = builder
            .header("x-grok-conv-id", self.conv_id)
            .header("x-grok-req-id", self.req_id)
            .header("x-grok-model-override", self.model_id)
            .header("x-grok-session-id", self.session_id)
            .header("x-grok-agent-id", self.agent_id);
        if let Some(turn_idx) = self.turn_idx {
            builder = builder.header("x-grok-turn-idx", turn_idx);
        }
        if let Some(deployment_id) = self.deployment_id.filter(|value| !value.is_empty()) {
            builder = builder.header("x-grok-deployment-id", deployment_id);
        }
        if let Some(user_id) = self.user_id.filter(|value| !value.is_empty()) {
            builder = builder.header("x-grok-user-id", user_id);
        }
        builder
    }

    #[cfg(test)]
    pub(crate) fn apply_for_provider(
        self,
        builder: RequestBuilder,
        provider: ModelProvider,
    ) -> RequestBuilder {
        provider_adapter(provider).apply_request_headers(builder, self)
    }
}

/// Provider policy consumed by [`crate::client::SamplingClient`].
///
/// Implementations are stateless and registered once for the process. Methods
/// deliberately do not receive credentials or an auth scheme.
pub trait ProviderAdapter: std::fmt::Debug + Send + Sync {
    fn provider(&self) -> ModelProvider;

    fn profile(&self) -> ProviderProfile {
        self.provider().profile()
    }

    fn validate_backend(&self, backend: &ApiBackend) -> Result<(), SamplingError> {
        if self.profile().supports_backend(backend) {
            Ok(())
        } else {
            Err(SamplingError::InvalidConfiguration(
                "API backend is not supported by the selected provider",
            ))
        }
    }

    /// Remove provider-private headers that are forbidden by this profile.
    /// This runs both at client construction and immediately before send so
    /// extra headers, live auth, and header injectors cannot bypass policy.
    fn sanitize_headers(&self, headers: &mut HeaderMap) {
        if self.profile().request_metadata == RequestMetadataPolicy::StandardHeadersOnly {
            remove_x_grok_headers(headers);
        }
    }

    fn apply_default_headers(&self, headers: &mut HeaderMap, config: &SamplerConfig) {
        self.sanitize_headers(headers);
        if self.profile().request_metadata != RequestMetadataPolicy::XGrokHeaders {
            return;
        }

        // xAI's API gates requests on a parseable client version and rejects
        // absent/unparseable ones with 426 ("version (none)"). The session's
        // configured client_version can legitimately be None on cross-provider
        // paths (e.g. a Codex-parented subagent overriding to a Grok model),
        // so fall back to this build's own version, normalized to its base
        // semver (the fork's `-open-grok.N` pre-release suffix is not part of
        // the upstream version grammar the gate parses).
        let client_version = config
            .client_version
            .as_deref()
            .unwrap_or(xai_grok_version::VERSION);
        let client_version = client_version
            .split(['-', '+'])
            .next()
            .filter(|base| !base.is_empty())
            .unwrap_or(client_version);
        insert_optional_header(headers, "x-grok-client-version", Some(client_version));
        insert_optional_header(
            headers,
            "x-grok-deployment-id",
            config.deployment_id.as_deref(),
        );
        insert_optional_header(headers, "x-grok-user-id", config.user_id.as_deref());

        let client_identifier = config
            .client_identifier
            .as_deref()
            .unwrap_or(DEFAULT_CLIENT_IDENTIFIER);
        insert_optional_header(headers, "x-grok-client-identifier", Some(client_identifier));
    }

    fn apply_request_headers(
        &self,
        builder: RequestBuilder,
        headers: ProviderRequestHeaders<'_>,
    ) -> RequestBuilder {
        if self.profile().request_metadata == RequestMetadataPolicy::XGrokHeaders {
            headers.apply_x_grok(builder)
        } else {
            builder
        }
    }

    /// Apply provider-owned request constraints after shared defaults. Most
    /// OpenAI-compatible providers need no rewrite.
    fn sanitize_chat_request(&self, _request: &mut ChatCompletionRequest) {}

    fn patch_responses_request(&self, request_body: &mut Value, policy: ResponsesRequestPolicy) {
        match self.profile().responses_dialect() {
            None | Some(ResponsesDialect::Xai) => {}
            Some(ResponsesDialect::Codex) => patch_codex_responses_request(request_body, policy),
        }
    }

    /// Return the provider-owned cache key derived from stable request state.
    fn prompt_cache_key(&self, session_id: Option<&str>) -> Option<String> {
        match self.profile().responses_dialect() {
            None | Some(ResponsesDialect::Xai) => None,
            Some(ResponsesDialect::Codex) => session_id
                .filter(|session_id| !session_id.is_empty())
                .map(str::to_owned),
        }
    }

    fn supports_turn_state(&self, backend: &ApiBackend) -> bool {
        self.profile().responses_dialect() == Some(ResponsesDialect::Codex)
            && *backend == ApiBackend::Responses
    }

    /// Remove any untrusted value and install only the first captured state.
    fn apply_turn_state_header(
        &self,
        headers: &mut HeaderMap,
        turn_state: Option<&Arc<OnceLock<String>>>,
    ) {
        headers.remove(X_CODEX_TURN_STATE_HEADER);
        if !self.supports_turn_state(&ApiBackend::Responses) {
            return;
        }
        if let Some(mut value) = turn_state
            .and_then(|state| state.get())
            .and_then(|state| HeaderValue::from_str(state).ok())
        {
            value.set_sensitive(true);
            headers.insert(X_CODEX_TURN_STATE_HEADER, value);
        }
    }

    fn capture_turn_state(&self, headers: &HeaderMap, turn_state: Option<&Arc<OnceLock<String>>>) {
        if !self.supports_turn_state(&ApiBackend::Responses) {
            return;
        }
        let Some(state) = turn_state else {
            return;
        };
        let Some(value) = headers
            .get(X_CODEX_TURN_STATE_HEADER)
            .and_then(|value| value.to_str().ok())
        else {
            return;
        };
        let _ = state.set(value.to_owned());
    }

    /// Absorb the forward-compatible Responses metadata side channel.
    ///
    /// xAI historically swallowed this event too, so all adapters preserve
    /// that compatibility. Only the Codex dialect captures routing state.
    fn absorb_response_metadata(
        &self,
        event_name: &str,
        data: &str,
        turn_state: Option<&Arc<OnceLock<String>>>,
    ) -> bool {
        let parsed = serde_json::from_str::<Value>(data).ok();
        let is_metadata = event_name == "response.metadata"
            || parsed
                .as_ref()
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str)
                == Some("response.metadata");
        if !is_metadata {
            return false;
        }

        if let Some(response_id) = parsed
            .as_ref()
            .and_then(|value| value.get("response_id"))
            .and_then(Value::as_str)
        {
            tracing::trace!(%response_id, provider = self.provider().as_str(), "received response metadata");
        }

        match self.profile().responses_dialect() {
            None | Some(ResponsesDialect::Xai) => {}
            Some(ResponsesDialect::Codex) => {
                let value = parsed
                    .as_ref()
                    .and_then(|value| value.get("headers"))
                    .and_then(Value::as_object)
                    .and_then(|headers| {
                        headers.iter().find_map(|(name, value)| {
                            name.eq_ignore_ascii_case(X_CODEX_TURN_STATE_HEADER)
                                .then(|| response_metadata_header_value(value))
                                .flatten()
                        })
                    });
                if let (Some(state), Some(value)) = (turn_state, value) {
                    let _ = state.set(value);
                }
            }
        }

        true
    }

    fn sends_doom_loop_opt_in(&self) -> bool {
        self.profile().request_metadata == RequestMetadataPolicy::XGrokHeaders
    }

    /// Both current dialects need the dependency-boundary compatibility pass.
    fn normalizes_response_events(&self) -> bool {
        match self.profile().responses_dialect() {
            None => false,
            Some(ResponsesDialect::Xai | ResponsesDialect::Codex) => true,
        }
    }

    fn ignores_unknown_response_event(&self, error: &SamplingError, data: &str) -> bool {
        self.profile().responses_dialect() == Some(ResponsesDialect::Codex)
            && is_unknown_top_level_response_event(error, data)
    }
}

#[derive(Debug)]
pub struct XaiProvider;

impl ProviderAdapter for XaiProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Xai
    }
}

#[derive(Debug)]
pub struct CodexProvider;

impl ProviderAdapter for CodexProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Codex
    }
}

#[derive(Debug)]
pub struct KimiProvider;

impl ProviderAdapter for KimiProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Kimi
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        // Kimi's coding models own their sampling policy. Moonshot documents
        // temperature/top_p as fixed for this lane, so do not forward either
        // user or global defaults. Penalty tuning is likewise provider-owned
        // for the coding models.
        request.temperature = None;
        request.top_p = None;
        request.frequency_penalty = None;
        request.presence_penalty = None;
    }
}

/// Fireworks AI is an ordinary OpenAI-compatible Chat Completions provider.
/// Unlike Kimi's coding lane, Fireworks accepts standard sampling fields, so
/// the shared defaults are forwarded unchanged.
#[derive(Debug)]
pub struct FireworksProvider;

impl ProviderAdapter for FireworksProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Fireworks
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        // Fireworks validates the chat schema strictly and rejects the whole
        // request with 400 "Extra inputs are not permitted" when a replayed
        // assistant message still carries Open Grok's internal per-message
        // `model_id` attribution (any multi-request turn hits this: the
        // second request replays the first assistant message). The request-
        // level `model` field is the real selector, so drop the bookkeeping
        // field for this provider. `reasoning_content` is left as-is: it is
        // part of the GLM/OpenAI-compatible reply contract, not our metadata.
        for message in &mut request.messages {
            message.model_id = None;
        }
    }
}

/// One entry in the built-in provider registry.
#[derive(Clone, Copy, Debug)]
pub struct ProviderRegistration {
    pub provider: ModelProvider,
    pub adapter: &'static dyn ProviderAdapter,
}

static XAI_PROVIDER: XaiProvider = XaiProvider;
static CODEX_PROVIDER: CodexProvider = CodexProvider;
static KIMI_PROVIDER: KimiProvider = KimiProvider;
static FIREWORKS_PROVIDER: FireworksProvider = FireworksProvider;

/// Complete registry for the built-in providers.
pub static PROVIDER_REGISTRY: [ProviderRegistration; 4] = [
    ProviderRegistration {
        provider: ModelProvider::Xai,
        adapter: &XAI_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Codex,
        adapter: &CODEX_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Kimi,
        adapter: &KIMI_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Fireworks,
        adapter: &FIREWORKS_PROVIDER,
    },
];

/// Look up the stateless transport adapter for a built-in provider.
pub fn provider_adapter(provider: ModelProvider) -> &'static dyn ProviderAdapter {
    // Keep the match exhaustive so adding a ModelProvider cannot silently use
    // another provider's wire policy. The table test verifies registry parity.
    match provider {
        ModelProvider::Xai => PROVIDER_REGISTRY[0].adapter,
        ModelProvider::Codex => PROVIDER_REGISTRY[1].adapter,
        ModelProvider::Kimi => PROVIDER_REGISTRY[2].adapter,
        ModelProvider::Fireworks => PROVIDER_REGISTRY[3].adapter,
    }
}

fn patch_codex_responses_request(request_body: &mut Value, policy: ResponsesRequestPolicy) {
    patch_codex_instruction_roles(request_body);

    // Codex sandboxes `web_search` unless the request opts into live access.
    // async-openai's native tool serializes the bare `{"type":"web_search"}`
    // shape, so grant live sources here — the fork's long-standing Codex
    // dialect behavior — while leaving any explicit override untouched.
    if let Some(tools) = request_body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut() {
            if tool.get("type").and_then(Value::as_str) == Some("web_search")
                && let Some(object) = tool.as_object_mut()
                && !object.contains_key("external_web_access")
            {
                object.insert("external_web_access".into(), true.into());
            }
        }
    }

    match policy
        .reasoning_summary
        .and_then(|summary| summary.wire_value())
    {
        Some(summary) => {
            ensure_reasoning_object(request_body);
            request_body["reasoning"]["summary"] = Value::String(summary.to_owned());
        }
        None => {
            if let Some(reasoning) = request_body
                .get_mut("reasoning")
                .and_then(Value::as_object_mut)
            {
                reasoning.remove("summary");
            }
        }
    }

    if matches!(
        policy.local_effort,
        Some(ReasoningEffort::Max | ReasoningEffort::Ultra)
    ) {
        ensure_reasoning_object(request_body);
        request_body["reasoning"]["effort"] = Value::String("max".to_owned());
    }

    if !policy.multi_agent_v2 {
        return;
    }
    let mode_text = if policy.local_effort == Some(ReasoningEffort::Ultra) {
        PROACTIVE_MULTI_AGENT_MODE_TEXT
    } else {
        EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT
    };
    let rendered = format!("{MULTI_AGENT_MODE_OPEN_TAG}{mode_text}{MULTI_AGENT_MODE_CLOSE_TAG}");
    let Some(input) = request_body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };

    input.retain(|item| !is_multi_agent_mode_item(item));
    let mode_item = serde_json::json!({
        "type": "message",
        "role": "developer",
        "content": [{ "type": "input_text", "text": rendered }],
    });
    let insert_at = input
        .last()
        .filter(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .map_or(input.len(), |_| input.len() - 1);
    input.insert(insert_at, mode_item);
}

fn remove_x_grok_headers(headers: &mut HeaderMap) {
    let private_headers = headers
        .keys()
        .filter(|name| name.as_str().starts_with("x-grok-"))
        .cloned()
        .collect::<Vec<_>>();
    for name in private_headers {
        headers.remove(name);
    }
}

fn insert_optional_header(headers: &mut HeaderMap, name: &'static str, value: Option<&str>) {
    let Some(value) = value else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(HeaderName::from_static(name), value);
    }
}

fn ensure_reasoning_object(request_body: &mut Value) {
    if !request_body.get("reasoning").is_some_and(Value::is_object) {
        request_body["reasoning"] = serde_json::json!({});
    }
}

fn patch_codex_instruction_roles(request_body: &mut Value) {
    let Some(input) = request_body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };

    let mut leading_instructions = Vec::new();
    let mut in_leading_prefix = true;
    let mut projected = Vec::with_capacity(input.len());
    for mut item in std::mem::take(input) {
        let is_system = item.get("role").and_then(Value::as_str) == Some("system");
        if !is_system {
            in_leading_prefix = false;
            projected.push(item);
            continue;
        }

        if in_leading_prefix
            && let Some(text) = responses_message_text(&item).filter(|text| !text.trim().is_empty())
        {
            leading_instructions.push(text);
            continue;
        }
        item["role"] = Value::String("developer".to_owned());
        projected.push(item);
    }
    *input = projected;

    if leading_instructions.is_empty() {
        return;
    }
    let leading = leading_instructions.join("\n\n");
    let instructions = request_body
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map_or(leading, str::to_owned);
    request_body["instructions"] = Value::String(instructions);
}

fn responses_message_text(item: &Value) -> Option<String> {
    match item.get("content")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn is_multi_agent_mode_item(item: &Value) -> bool {
    if item.get("role").and_then(Value::as_str) != Some("developer") {
        return false;
    }
    match item.get("content") {
        Some(Value::String(text)) => text.contains(MULTI_AGENT_MODE_OPEN_TAG),
        Some(Value::Array(content)) => content.iter().any(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| text.contains(MULTI_AGENT_MODE_OPEN_TAG))
        }),
        _ => false,
    }
}

fn response_metadata_header_value(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => values.first().and_then(response_metadata_header_value),
        _ => None,
    }
}

fn is_unknown_top_level_response_event(error: &SamplingError, data: &str) -> bool {
    let SamplingError::Serialization(error) = error else {
        return false;
    };
    let Some(event_type) = serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
    else {
        return false;
    };
    error
        .to_string()
        .contains(&format!("unknown variant `{event_type}`"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> Value {
        serde_json::json!({
            "input": [
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "base prompt"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}
            ],
            "reasoning": {"effort": "xhigh", "summary": "concise"}
        })
    }

    #[test]
    fn registry_is_complete_and_profiles_match_keys() {
        let expected = [
            ModelProvider::Xai,
            ModelProvider::Codex,
            ModelProvider::Kimi,
            ModelProvider::Fireworks,
        ];
        assert_eq!(PROVIDER_REGISTRY.len(), expected.len());
        for provider in expected {
            let entries = PROVIDER_REGISTRY
                .iter()
                .filter(|entry| entry.provider == provider)
                .collect::<Vec<_>>();
            assert_eq!(entries.len(), 1, "provider registry entry for {provider:?}");
            let adapter = provider_adapter(provider);
            assert_eq!(adapter.provider(), provider);
            assert_eq!(adapter.profile(), provider.profile());
            assert_eq!(entries[0].adapter.provider(), provider);
        }
    }

    #[test]
    fn request_patching_is_selected_only_by_provider_adapter() {
        for provider in [
            ModelProvider::Xai,
            ModelProvider::Codex,
            ModelProvider::Kimi,
            ModelProvider::Fireworks,
        ] {
            let mut request = base_request();
            let original = request.clone();
            provider_adapter(provider).patch_responses_request(
                &mut request,
                ResponsesRequestPolicy {
                    multi_agent_v2: false,
                    local_effort: Some(ReasoningEffort::Max),
                    reasoning_summary: None,
                },
            );

            if provider != ModelProvider::Codex {
                assert_eq!(request, original);
            } else {
                assert_eq!(request["instructions"], "base prompt");
                assert_eq!(request["input"].as_array().unwrap().len(), 1);
                assert_eq!(request["reasoning"]["effort"], "max");
                assert!(request["reasoning"].get("summary").is_none());
            }
        }
    }

    #[test]
    fn prompt_cache_and_event_policy_follow_provider_profile() {
        let xai = provider_adapter(ModelProvider::Xai);
        let codex = provider_adapter(ModelProvider::Codex);
        let kimi = provider_adapter(ModelProvider::Kimi);
        let fireworks = provider_adapter(ModelProvider::Fireworks);
        assert_eq!(xai.prompt_cache_key(Some("session")), None);
        assert_eq!(
            codex.prompt_cache_key(Some("session")),
            Some("session".to_owned())
        );
        assert!(!xai.supports_turn_state(&ApiBackend::Responses));
        assert!(codex.supports_turn_state(&ApiBackend::Responses));
        assert!(xai.sends_doom_loop_opt_in());
        assert!(!codex.sends_doom_loop_opt_in());
        assert!(!kimi.sends_doom_loop_opt_in());
        assert!(xai.normalizes_response_events());
        assert!(codex.normalizes_response_events());
        assert!(!kimi.normalizes_response_events());
        assert!(kimi.validate_backend(&ApiBackend::ChatCompletions).is_ok());
        assert!(kimi.validate_backend(&ApiBackend::Responses).is_err());
        assert_eq!(fireworks.prompt_cache_key(Some("session")), None);
        assert!(!fireworks.supports_turn_state(&ApiBackend::Responses));
        assert!(!fireworks.sends_doom_loop_opt_in());
        assert!(!fireworks.normalizes_response_events());
        assert!(
            fireworks
                .validate_backend(&ApiBackend::ChatCompletions)
                .is_ok()
        );
        assert!(fireworks.validate_backend(&ApiBackend::Responses).is_err());
        assert!(fireworks.validate_backend(&ApiBackend::Messages).is_err());
    }

    #[test]
    fn fireworks_forwards_standard_sampling_parameters_unchanged() {
        let mut request =
            ChatCompletionRequest::new("accounts/fireworks/models/glm-5p2", Vec::new());
        request.temperature = Some(0.7);
        request.top_p = Some(0.95);
        provider_adapter(ModelProvider::Fireworks).sanitize_chat_request(&mut request);
        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.top_p, Some(0.95));
    }

    #[test]
    fn xai_client_version_header_always_present_and_base_semver() {
        fn config_with_version(client_version: Option<&str>) -> SamplerConfig {
            SamplerConfig {
                api_key: Some("test-key".to_string()),
                base_url: "https://api.x.ai".to_string(),
                model: "grok-4.5".to_string(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: ApiBackend::ChatCompletions,
                provider: ModelProvider::Xai,
                auth_scheme: crate::config::AuthScheme::Bearer,
                extra_headers: indexmap::IndexMap::new(),
                query_params: indexmap::IndexMap::new(),
                env_http_headers: indexmap::IndexMap::new(),
                context_window: 8192,
                force_http1: false,
                max_retries: None,
                stream_tool_calls: false,
                idle_timeout_secs: None,
                reasoning_effort: None,
                reasoning_summary: None,
                origin_client: None,
                client_identifier: None,
                deployment_id: None,
                user_id: None,
                client_version: client_version.map(str::to_string),
                attribution_callback: None,
                bearer_resolver: None,
                supports_backend_search: false,
                codex_multi_agent_v2: false,
                compactions_remaining: None,
                compaction_at_tokens: None,
                doom_loop_recovery: None,
                header_injector: None,
            }
        }
        let header_for = |client_version: Option<&str>| {
            let mut headers = HeaderMap::new();
            provider_adapter(ModelProvider::Xai)
                .apply_default_headers(&mut headers, &config_with_version(client_version));
            headers
                .get("x-grok-client-version")
                .expect("xAI requests must always carry a client version (426 gate)")
                .to_str()
                .expect("ascii")
                .to_string()
        };

        // Cross-provider paths (Codex parent → Grok child) resolve no session
        // client_version; the build's own version must be sent, not nothing.
        let fallback = header_for(None);
        assert!(!fallback.is_empty());
        assert!(
            !fallback.contains('-'),
            "fork pre-release suffix must be stripped for the gate parser: {fallback}"
        );

        assert_eq!(header_for(Some("0.1.220-open-grok.23")), "0.1.220");
        assert_eq!(header_for(Some("0.1.230")), "0.1.230");
    }

    #[test]
    fn fireworks_strips_internal_model_id_from_replayed_messages() {
        use xai_grok_sampling_types::types::ChatRequestMessage;

        let assistant = ChatRequestMessage::assistant(
            "previous turn",
            "accounts/fireworks/models/glm-5p2",
            None,
        );
        assert!(assistant.model_id.is_some(), "constructor stamps model_id");
        let mut request = ChatCompletionRequest::new(
            "accounts/fireworks/models/glm-5p2",
            vec![
                ChatRequestMessage::system("s"),
                ChatRequestMessage::user("u"),
                assistant,
                ChatRequestMessage::user("follow-up"),
            ],
        );

        provider_adapter(ModelProvider::Fireworks).sanitize_chat_request(&mut request);

        assert!(request.messages.iter().all(|m| m.model_id.is_none()));
        let wire = serde_json::to_string(&request).expect("serializes");
        assert!(
            !wire.contains("model_id"),
            "Fireworks rejects extra per-message fields with 400: {wire}"
        );
    }

    #[test]
    fn kimi_omits_sampling_parameters_owned_by_the_model() {
        let mut request = ChatCompletionRequest::new("kimi-k3", Vec::new());
        request.temperature = Some(0.7);
        request.top_p = Some(0.95);
        request.frequency_penalty = Some(0.2);
        request.presence_penalty = Some(0.3);
        provider_adapter(ModelProvider::Kimi).sanitize_chat_request(&mut request);
        assert_eq!(request.temperature, None);
        assert_eq!(request.top_p, None);
        assert_eq!(request.frequency_penalty, None);
        assert_eq!(request.presence_penalty, None);
    }
}
