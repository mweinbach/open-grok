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
    ApiBackend, ChatCompletionRequest, ChatReasoningConfig, ChatThinkingMode, ModelProvider,
    ProviderProfile, ReasoningEffort, ReasoningSummary, RequestMetadataPolicy, ResponsesDialect,
    SamplingError,
};

use crate::config::{CodexApprovalPolicy, CodexPermissions, SamplerConfig};

/// Process-level fallback for the `x-grok-client-identifier` header.
const DEFAULT_CLIENT_IDENTIFIER: &str = "grok-shell";
/// Current stable Anthropic Messages wire version. A custom endpoint on the
/// Messages backend gets this when its model entry does not pin a version.
const ANTHROPIC_VERSION: &str = "2023-06-01";
pub(crate) const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
/// Codex session-affinity headers (codex-rs `build_session_headers`).
///
/// The ChatGPT Codex backend activates per-conversation prompt caching only
/// when a session identity header accompanies the body's `prompt_cache_key`;
/// with the body key alone, every request only hits the globally shared
/// instruction-prefix cache (verified empirically against
/// `/backend-api/codex/responses`: identical back-to-back requests report
/// `cached_tokens: 0` without these headers and a full prefix hit with them).
pub(crate) const CODEX_SESSION_ID_HEADER: &str = "session-id";
pub(crate) const CODEX_THREAD_ID_HEADER: &str = "thread-id";
pub(crate) const CODEX_CLIENT_REQUEST_ID_HEADER: &str = "x-client-request-id";
pub(crate) const X_CODEX_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";
pub(crate) const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

pub(crate) const MULTI_AGENT_MODE_OPEN_TAG: &str = "<multi_agent_mode>";
pub(crate) const MULTI_AGENT_MODE_CLOSE_TAG: &str = "</multi_agent_mode>";
pub(crate) const EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT: &str = "Any earlier instruction enabling proactive multi-agent delegation no longer applies. Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
pub(crate) const PROACTIVE_MULTI_AGENT_MODE_TEXT: &str = "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode developer message changes it.";

/// Provider-neutral input to the Responses request patching seam.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponsesRequestPolicy {
    pub multi_agent_v2: bool,
    pub use_responses_lite: bool,
    pub local_effort: Option<ReasoningEffort>,
    pub reasoning_summary: Option<ReasoningSummary>,
    pub codex_permissions: Option<CodexPermissions>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
}

/// Request metadata available to providers that use the xAI proxy contract.
#[derive(Clone, Copy, Debug)]
pub struct ProviderRequestHeaders<'a> {
    pub conv_id: &'a str,
    pub req_id: &'a str,
    pub model_id: &'a str,
    pub session_id: &'a str,
    pub turn_idx: Option<&'a str>,
    pub transient_retry: Option<&'a str>,
    pub agent_id: &'a str,
    pub deployment_id: Option<&'a str>,
    pub user_id: Option<&'a str>,
    /// Prompt-cache identity override (verbatim same-model forks inherit the
    /// parent's). Drives session-affinity headers only; the `x-grok-*`
    /// telemetry headers keep `session_id`.
    pub cache_affinity_id: Option<&'a str>,
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
        if let Some(attempt) = self.transient_retry {
            builder = builder.header("x-grok-transient-retry", attempt);
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
            .unwrap_or(xai_grok_version::version());
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
        let affinity_id = headers.cache_affinity_id.unwrap_or(headers.session_id);
        let builder = self.apply_session_affinity_headers(builder, Some(affinity_id));
        if self.profile().request_metadata == RequestMetadataPolicy::XGrokHeaders {
            headers.apply_x_grok(builder)
        } else {
            builder
        }
    }

    fn apply_responses_lite_header(&self, headers: &mut HeaderMap, enabled: bool) {
        headers.remove(RESPONSES_LITE_HEADER);
        if self.provider() == ModelProvider::Codex && enabled {
            headers.insert(RESPONSES_LITE_HEADER, HeaderValue::from_static("true"));
        }
    }

    /// Apply the provider's session identity headers for prompt-cache
    /// affinity. No-op for providers whose backends do not key caching on a
    /// session identity (xAI uses `x-grok-session-id` via the request
    /// metadata policy instead).
    fn apply_session_affinity_headers(
        &self,
        builder: RequestBuilder,
        session_id: Option<&str>,
    ) -> RequestBuilder {
        let _ = session_id;
        builder
    }

    /// Apply provider-owned request constraints after shared defaults. Most
    /// OpenAI-compatible providers need no rewrite.
    fn sanitize_chat_request(&self, _request: &mut ChatCompletionRequest) {}

    fn patch_responses_request(&self, request_body: &mut Value, policy: ResponsesRequestPolicy) {
        match self.profile().responses_dialect() {
            None | Some(ResponsesDialect::Xai) => {}
            Some(ResponsesDialect::Codex) => patch_codex_responses_request(request_body, policy),
            Some(ResponsesDialect::DeepSeek) => {
                patch_deepseek_responses_request(request_body, policy)
            }
            Some(ResponsesDialect::Meta) => patch_meta_responses_request(request_body),
            Some(ResponsesDialect::OpenAi) => patch_openai_responses_request(request_body, policy),
        }
    }

    /// Return the provider-owned cache key derived from stable request state.
    fn prompt_cache_key(&self, session_id: Option<&str>) -> Option<String> {
        match self.profile().responses_dialect() {
            None
            | Some(
                ResponsesDialect::Xai
                | ResponsesDialect::DeepSeek
                | ResponsesDialect::Meta
                | ResponsesDialect::OpenAi,
            ) => None,
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
            None
            | Some(
                ResponsesDialect::Xai
                | ResponsesDialect::DeepSeek
                | ResponsesDialect::Meta
                | ResponsesDialect::OpenAi,
            ) => {}
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
            Some(
                ResponsesDialect::Xai
                | ResponsesDialect::Codex
                | ResponsesDialect::DeepSeek
                | ResponsesDialect::Meta
                | ResponsesDialect::OpenAi,
            ) => true,
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

    /// codex-rs sends `session-id`, `thread-id`, and `x-client-request-id`
    /// (all derived from the stable thread id) on every Responses request.
    /// The backend requires one of the session identity headers before it
    /// serves the per-conversation prompt cache keyed by `prompt_cache_key`,
    /// so Open Grok maps its stable session id onto all three.
    fn apply_session_affinity_headers(
        &self,
        builder: RequestBuilder,
        session_id: Option<&str>,
    ) -> RequestBuilder {
        let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
            return builder;
        };
        builder
            .header(CODEX_SESSION_ID_HEADER, session_id)
            .header(CODEX_THREAD_ID_HEADER, session_id)
            .header(CODEX_CLIENT_REQUEST_ID_HEADER, session_id)
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
        request.service_tier = None;
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
        request.reasoning_effort = None;
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
        if let Some(tools) = &mut request.tools {
            for tool in tools {
                normalize_fireworks_schema(&mut tool.function.parameters);
            }
        }
    }
}

#[derive(Debug)]
pub struct DeepSeekProvider;

impl ProviderAdapter for DeepSeekProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::DeepSeek
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        for message in &mut request.messages {
            message.model_id = None;
        }
        request.service_tier = None;
    }
}

#[derive(Debug)]
pub struct MetaProvider;

impl ProviderAdapter for MetaProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Meta
    }
}

#[derive(Debug)]
pub struct OpenCodeGoProvider;

impl ProviderAdapter for OpenCodeGoProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::OpenCodeGo
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        for message in &mut request.messages {
            message.model_id = None;
        }
        request.service_tier = None;
    }
}

/// Wafer AI is an ordinary OpenAI-compatible Chat Completions provider.
#[derive(Debug)]
pub struct WaferProvider;

impl ProviderAdapter for WaferProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Wafer
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        request.reasoning_effort = None;
        request.service_tier = None;
        for message in &mut request.messages {
            message.model_id = None;
        }
    }
}

/// Z AI is an OpenAI-compatible Chat Completions provider (GLM models).
/// Unlike Wafer, Z AI keeps `reasoning_effort` so callers can drive its
/// "thinking mode", and a requested effort turns on the explicit `thinking`
/// object Z AI expects alongside it; it still drops Grok-internal
/// `service_tier` and per-message `model_id` metadata.
#[derive(Debug)]
pub struct ZaiProvider;

impl ProviderAdapter for ZaiProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Zai
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        request.service_tier = None;
        for message in &mut request.messages {
            message.model_id = None;
        }
        // Z AI gates GLM thinking with an explicit `thinking` object; a
        // requested reasoning_effort implies thinking enabled.
        request.thinking = request
            .reasoning_effort
            .is_some()
            .then(ChatThinkingMode::enabled);
    }
}

/// RunInfra is an OpenAI-compatible Chat Completions provider. It keeps
/// `reasoning_effort` (hosted models reason by default) and remaps DeepSeek
/// V4 Flash's high/xhigh/max values to the `max` token the gateway honors.
/// It still drops Grok-internal `service_tier` and per-message `model_id`.
#[derive(Debug)]
pub struct RuninfraProvider;

impl ProviderAdapter for RuninfraProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Runinfra
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        request.service_tier = None;
        for message in &mut request.messages {
            message.model_id = None;
        }
        if request.model.as_deref() == Some("deepseek-v4-flash")
            && let Some(effort) = request.reasoning_effort
        {
            request.reasoning_effort = Some(normalize_runinfra_deepseek_v4_flash_effort(effort));
        }
    }
}

fn normalize_runinfra_deepseek_v4_flash_effort(effort: ReasoningEffort) -> ReasoningEffort {
    match effort {
        ReasoningEffort::High
        | ReasoningEffort::Xhigh
        | ReasoningEffort::Max
        | ReasoningEffort::Ultra => ReasoningEffort::Max,
        other => other,
    }
}

/// Google Gemini API / AI Studio OpenAI-compatible Chat Completions.
/// Keeps `reasoning_effort` (the official thinking control) and remaps
/// values Gemini 3 rejects. Does not send Z AI `thinking` or Gemini-native
/// `thinking_level` — those overlap `reasoning_effort` and 400.
#[derive(Debug)]
pub struct GeminiProvider;

/// A user-supplied server address (custom-provider wizard, `[model.*]` with
/// `provider = "custom"`). The endpoint is untrusted third-party infrastructure
/// and its wire support is unknown, so this adapter sends only the plain
/// protocol it was configured for: OpenAI Chat Completions, vanilla OpenAI
/// Responses, or Anthropic Messages.
///
/// Nothing provider-private leaves the process on this route: no `x-grok-*`
/// headers (standard metadata policy), no session-id cache keys, no hosted
/// tools, no Code Mode transport, and no built-in credentials. Chat-only
/// extensions that OpenAI-compatible servers commonly reject (`thinking`,
/// per-message `model_id`) and Grok-internal `service_tier` are dropped.
#[derive(Debug)]
pub struct CustomProvider;

/// OpenRouter is an OpenAI-compatible Chat Completions gateway. It maps
/// `reasoning_effort` onto the nested `reasoning` object OpenRouter
/// documents, copies replayed thinking onto `messages[].reasoning`, and
/// drops Grok-internal `service_tier` plus per-message `model_id`.
#[derive(Debug)]
pub struct OpenRouterProvider;

impl ProviderAdapter for GeminiProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Gemini
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        request.service_tier = None;
        request.thinking = None;
        for message in &mut request.messages {
            message.model_id = None;
        }
        request.reasoning_effort = request
            .reasoning_effort
            .and_then(|effort| normalize_gemini_reasoning_effort(request.model.as_deref(), effort));
    }
}

impl ProviderAdapter for OpenRouterProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::OpenRouter
    }

    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        request.service_tier = None;
        request.thinking = None;
        let effort = request.reasoning_effort.take().or_else(|| {
            request
                .reasoning
                .as_ref()
                .and_then(|reasoning| reasoning.effort)
        });
        if let Some(effort) = effort {
            request.reasoning = Some(ChatReasoningConfig::effort(
                normalize_openrouter_reasoning_effort(effort),
            ));
        }
        for message in &mut request.messages {
            message.model_id = None;
            if message.reasoning.is_none() {
                message.reasoning = message.reasoning_content.take();
            } else {
                message.reasoning_content = None;
            }
        }
    }
}

impl ProviderAdapter for CustomProvider {
    fn provider(&self) -> ModelProvider {
        ModelProvider::Custom
    }

    /// Keep the Chat Completions body to the portable OpenAI surface. The
    /// endpoint's actual grammar is unknown, so a strict server must never see
    /// Grok-internal routing (`service_tier`), a per-message model override, or
    /// provider-specific `thinking` / nested `reasoning` extensions.
    fn sanitize_chat_request(&self, request: &mut ChatCompletionRequest) {
        request.service_tier = None;
        request.thinking = None;
        request.reasoning = None;
        for message in &mut request.messages {
            message.model_id = None;
            message.reasoning = None;
        }
    }

    /// Anthropic-compatible servers require `anthropic-version`; it is a wire
    /// requirement, not a preference. Supply the current stable version on the
    /// Messages backend only, and only when the model entry did not already
    /// pin one through `extra_headers` — the user's value must keep winning.
    ///
    /// This runs after `extra_headers` are applied, so it must never
    /// overwrite: `entry().or_insert()` is load-bearing here.
    fn apply_default_headers(&self, headers: &mut HeaderMap, config: &SamplerConfig) {
        self.sanitize_headers(headers);
        if config.api_backend != ApiBackend::Messages {
            return;
        }
        headers
            .entry("anthropic-version")
            .or_insert_with(|| HeaderValue::from_static(ANTHROPIC_VERSION));
    }
}

fn normalize_openrouter_reasoning_effort(effort: ReasoningEffort) -> ReasoningEffort {
    match effort {
        ReasoningEffort::Ultra => ReasoningEffort::Max,
        other => other,
    }
}

fn gemini_rejects_minimal(model: Option<&str>) -> bool {
    matches!(model, Some("gemini-3.7-flash" | "gemini-3.1-pro-preview"))
}

fn normalize_gemini_reasoning_effort(
    model: Option<&str>,
    effort: ReasoningEffort,
) -> Option<ReasoningEffort> {
    match effort {
        // Gemini 3 cannot turn thinking fully off; omit so the model default applies.
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal if gemini_rejects_minimal(model) => Some(ReasoningEffort::Low),
        ReasoningEffort::Xhigh | ReasoningEffort::Max | ReasoningEffort::Ultra => {
            Some(ReasoningEffort::High)
        }
        other => Some(other),
    }
}

fn normalize_fireworks_schema(schema: &mut Value) {
    match schema {
        Value::Bool(true) => {
            *schema = fireworks_unconstrained_schema();
        }
        Value::Array(items) => {
            for item in items {
                normalize_fireworks_schema(item);
            }
        }
        Value::Object(object) => {
            for keyword in [
                "additionalProperties",
                "contains",
                "else",
                "if",
                "items",
                "not",
                "propertyNames",
                "then",
                "unevaluatedItems",
                "unevaluatedProperties",
            ] {
                if let Some(child) = object.get_mut(keyword) {
                    normalize_fireworks_schema(child);
                }
            }
            for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
                if let Some(Value::Array(children)) = object.get_mut(keyword) {
                    for child in children {
                        normalize_fireworks_schema(child);
                    }
                }
            }
            for keyword in [
                "$defs",
                "definitions",
                "dependentSchemas",
                "patternProperties",
                "properties",
            ] {
                if let Some(Value::Object(children)) = object.get_mut(keyword) {
                    for child in children.values_mut() {
                        normalize_fireworks_schema(child);
                    }
                }
            }

            if object.keys().all(|key| is_schema_annotation(key)) {
                object.insert(
                    "anyOf".to_owned(),
                    fireworks_unconstrained_schema()["anyOf"].clone(),
                );
            }
        }
        _ => {}
    }
}

fn is_schema_annotation(keyword: &str) -> bool {
    matches!(
        keyword,
        "$anchor"
            | "$comment"
            | "$id"
            | "$schema"
            | "default"
            | "deprecated"
            | "description"
            | "examples"
            | "readOnly"
            | "title"
            | "writeOnly"
    )
}

fn fireworks_unconstrained_schema() -> Value {
    serde_json::json!({
        "anyOf": [
            {"type": "null"},
            {"type": "boolean"},
            {"type": "integer"},
            {"type": "number"},
            {"type": "string"},
            {"type": "array"},
            {"type": "object"}
        ]
    })
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
static DEEPSEEK_PROVIDER: DeepSeekProvider = DeepSeekProvider;
static META_PROVIDER: MetaProvider = MetaProvider;
static OPEN_CODE_GO_PROVIDER: OpenCodeGoProvider = OpenCodeGoProvider;
static WAFER_PROVIDER: WaferProvider = WaferProvider;
static ZAI_PROVIDER: ZaiProvider = ZaiProvider;
static RUNINFRA_PROVIDER: RuninfraProvider = RuninfraProvider;
static GEMINI_PROVIDER: GeminiProvider = GeminiProvider;
static OPENROUTER_PROVIDER: OpenRouterProvider = OpenRouterProvider;
static CUSTOM_PROVIDER: CustomProvider = CustomProvider;

/// Complete registry for the built-in providers.
pub static PROVIDER_REGISTRY: [ProviderRegistration; 13] = [
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
    ProviderRegistration {
        provider: ModelProvider::DeepSeek,
        adapter: &DEEPSEEK_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Meta,
        adapter: &META_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::OpenCodeGo,
        adapter: &OPEN_CODE_GO_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Wafer,
        adapter: &WAFER_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Zai,
        adapter: &ZAI_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Runinfra,
        adapter: &RUNINFRA_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Gemini,
        adapter: &GEMINI_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::OpenRouter,
        adapter: &OPENROUTER_PROVIDER,
    },
    ProviderRegistration {
        provider: ModelProvider::Custom,
        adapter: &CUSTOM_PROVIDER,
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
        ModelProvider::DeepSeek => PROVIDER_REGISTRY[4].adapter,
        ModelProvider::Meta => PROVIDER_REGISTRY[5].adapter,
        ModelProvider::OpenCodeGo => PROVIDER_REGISTRY[6].adapter,
        ModelProvider::Wafer => PROVIDER_REGISTRY[7].adapter,
        ModelProvider::Zai => PROVIDER_REGISTRY[8].adapter,
        ModelProvider::Runinfra => PROVIDER_REGISTRY[9].adapter,
        ModelProvider::Gemini => PROVIDER_REGISTRY[10].adapter,
        ModelProvider::OpenRouter => PROVIDER_REGISTRY[11].adapter,
        ModelProvider::Custom => PROVIDER_REGISTRY[12].adapter,
    }
}

fn patch_codex_responses_request(request_body: &mut Value, policy: ResponsesRequestPolicy) {
    patch_codex_agent_message_ids(request_body);
    patch_codex_instruction_roles(request_body);

    if let Some(permissions) = policy.codex_permissions.as_ref() {
        patch_codex_permissions(request_body, permissions, &policy);
    }

    if policy.use_responses_lite {
        patch_codex_responses_lite(request_body);
    }

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

fn patch_codex_agent_message_ids(request_body: &mut Value) {
    let Some(input) = request_body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    for item in input {
        // Older native mailboxes persisted local UUIDs as Responses item IDs.
        // Repair only that shape so resume works without changing opaque history.
        if item.get("type").and_then(Value::as_str) == Some("agent_message")
            && let Some(id) = item.get("id").and_then(Value::as_str)
            && let Ok(id) = uuid::Uuid::parse_str(id)
        {
            item["id"] = Value::String(format!("amsg_{id}"));
        }
    }
}

fn patch_codex_responses_lite(request_body: &mut Value) {
    let Some(body) = request_body.as_object_mut() else {
        return;
    };
    let already_prepared = !body.contains_key("tools")
        && !body.contains_key("instructions")
        && body
            .get("input")
            .and_then(Value::as_array)
            .and_then(|input| input.first())
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("additional_tools");
    let tools = body
        .remove("tools")
        .and_then(|tools| tools.as_array().cloned())
        .unwrap_or_default();
    let instructions = body.remove("instructions");
    body.insert("parallel_tool_calls".into(), Value::Bool(false));
    let input = body
        .entry("input")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::String(text) = input {
        *input = serde_json::json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": std::mem::take(text)}],
        }]);
    }
    let Some(input) = input.as_array_mut() else {
        return;
    };
    for item in input.iter_mut() {
        let item_type = item.get("type").and_then(Value::as_str);
        let content_key = match item_type {
            Some("function_call" | "custom_tool_call") => {
                if let Some(item) = item.as_object_mut() {
                    item.entry("namespace")
                        .or_insert_with(|| Value::String("functions".into()));
                }
                continue;
            }
            Some("function_call_output" | "custom_tool_call_output") => "output",
            Some("message") | None => "content",
            _ => continue,
        };
        if let Some(content) = item.get_mut(content_key).and_then(Value::as_array_mut) {
            for part in content {
                if part.get("type").and_then(Value::as_str) == Some("input_image")
                    && let Some(part) = part.as_object_mut()
                {
                    part.remove("detail");
                }
            }
        }
    }
    let mut prefix = vec![serde_json::json!({
        "type": "additional_tools",
        "role": "developer",
        "tools": responses_lite_tools(tools),
    })];
    if let Some(instructions) = instructions.and_then(|value| value.as_str().map(str::to_owned))
        && !instructions.is_empty()
    {
        prefix.push(serde_json::json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": instructions}],
            "internal_chat_message_metadata_passthrough": {
                "content_item_kinds": ["model.base_instructions"],
            },
        }));
    }
    if !already_prepared {
        input.splice(0..0, prefix);
    }
    ensure_reasoning_object(request_body);
    request_body["reasoning"]["context"] = Value::String("all_turns".into());
}

fn responses_lite_tools(tools: Vec<Value>) -> Vec<Value> {
    let mut functions = Vec::new();
    let mut namespaces = Vec::new();
    let mut functions_index = None;
    let mut description = Value::String(String::new());
    for mut tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("function" | "custom") => functions.push(tool),
            Some("namespace") if tool.get("name").and_then(Value::as_str) == Some("functions") => {
                if let Some(value) = tool.get("description")
                    && value.as_str().is_some_and(|text| !text.trim().is_empty())
                {
                    description = value.clone();
                }
                if let Some(tools) = tool.get_mut("tools").and_then(Value::as_array_mut) {
                    functions.append(tools);
                }
            }
            Some("namespace") => {
                namespaces.push(tool);
                continue;
            }
            _ => continue,
        }
        functions_index.get_or_insert(namespaces.len());
    }
    if let Some(index) = functions_index
        && !functions.is_empty()
    {
        namespaces.insert(
            index,
            serde_json::json!({
                "type": "namespace",
                "name": "functions",
                "description": description,
                "tools": functions,
            }),
        );
    }
    namespaces
}

fn patch_codex_permissions(
    request_body: &mut Value,
    permissions: &CodexPermissions,
    policy: &ResponsesRequestPolicy,
) {
    let mut metadata = serde_json::json!({
        "sandbox": permissions.sandbox,
        "sandbox_mode": permissions.sandbox_mode,
        "auto_review_enabled": permissions.auto_review_enabled,
    });
    if let Some(session_id) = policy.session_id.as_ref() {
        metadata["session_id"] = Value::String(session_id.clone());
        metadata["thread_id"] = Value::String(session_id.clone());
    }
    if let Some(turn_id) = policy.turn_id.as_ref() {
        metadata["turn_id"] = Value::String(turn_id.clone());
    }
    if let Some(profile) = permissions.sandbox_profile.as_ref() {
        metadata["open_grok_sandbox_profile"] = Value::String(profile.clone());
    }
    let client_metadata = request_body
        .as_object_mut()
        .expect("Responses request must be an object")
        .entry("client_metadata")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(client_metadata) = client_metadata.as_object_mut() {
        client_metadata.insert(
            X_CODEX_TURN_METADATA_HEADER.to_owned(),
            Value::String(metadata.to_string()),
        );
    }

    let network = if permissions.network_access {
        "enabled"
    } else {
        "restricted"
    };
    let sandbox = match permissions.sandbox_mode.as_str() {
        "read-only" => {
            "The filesystem sandbox permits reading files; workspace files cannot be modified."
        }
        "workspace-write" => {
            "The filesystem sandbox permits reading files and writing only within its allowed roots."
        }
        _ => {
            "No filesystem sandbox is active; commands have unrestricted filesystem access subject to local permission rules."
        }
    };
    let approval = match permissions.approval_policy {
        CodexApprovalPolicy::Never => {
            "Approval policy is `never`: Open Grok automatically permits tool executions unless a hard deny rule or plan-mode restriction applies. Do not claim that approval is required."
        }
        CodexApprovalPolicy::OnRequest if permissions.auto_review_enabled => {
            "Approval policy is `on-request` and `approvals_reviewer` is `auto_review`: Open Grok's permission classifier reviews tool actions automatically; hard deny rules and the actual sandbox remain enforced."
        }
        CodexApprovalPolicy::OnRequest => {
            "Approval policy is `on-request`: Open Grok requests user approval when its local permission policy requires it. Permission prompts are managed by the tool runtime."
        }
    };
    let roots = if permissions.writable_roots.is_empty() {
        String::new()
    } else {
        format!(
            "\nThe writable roots are {}.",
            permissions
                .writable_roots
                .iter()
                .map(|root| format!("`{root}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let rendered = format!(
        "<permissions instructions>\nFilesystem sandboxing: `sandbox_mode` is `{}`. {sandbox} Network access is {network}.\n{approval}{roots}\n</permissions instructions>",
        permissions.sandbox_mode,
    );
    let Some(input) = request_body.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };
    input.retain(|item| {
        !(item.get("role").and_then(Value::as_str) == Some("developer")
            && responses_message_text(item)
                .is_some_and(|text| text.contains("<permissions instructions>")))
    });
    let insert_at = input
        .iter()
        .position(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .unwrap_or(input.len());
    input.insert(
        insert_at,
        serde_json::json!({
            "type": "message",
            "role": "developer",
            "content": [{ "type": "input_text", "text": rendered }],
        }),
    );
}

fn patch_deepseek_responses_request(request_body: &mut Value, policy: ResponsesRequestPolicy) {
    let Some(body) = request_body.as_object_mut() else {
        return;
    };

    // DeepSeek's Responses endpoint is stateless. These OpenAI fields are
    // unsupported (and silently ignored), so omit them rather than implying
    // continuity, storage, or provider-side cache controls that do not exist.
    for field in [
        "background",
        "conversation",
        "context_management",
        "include",
        "metadata",
        "previous_response_id",
        "prompt",
        "prompt_cache_key",
        "prompt_cache_retention",
        "safety_identifier",
        "service_tier",
        "store",
        "stream_options",
        "truncation",
    ] {
        body.remove(field);
    }

    // DeepSeek accepts `reasoning.summary` for compatibility but does not
    // generate one. Its documented Responses effort set is
    // none/low/high/max, so normalize Open Grok's broader menu explicitly.
    let effort = policy.local_effort.map(|effort| match effort {
        ReasoningEffort::None => "none",
        ReasoningEffort::Minimal | ReasoningEffort::Low => "low",
        ReasoningEffort::Medium | ReasoningEffort::High | ReasoningEffort::Xhigh => "high",
        ReasoningEffort::Max | ReasoningEffort::Ultra => "max",
    });
    if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
        reasoning.remove("summary");
        if let Some(effort) = effort {
            reasoning.insert("effort".to_owned(), Value::String(effort.to_owned()));
        }
    } else if let Some(effort) = effort {
        body.insert(
            "reasoning".to_owned(),
            serde_json::json!({ "effort": effort }),
        );
    }
    if body
        .get("reasoning")
        .and_then(Value::as_object)
        .is_some_and(serde_json::Map::is_empty)
    {
        body.remove("reasoning");
    }
}

/// Clamp one reasoning-effort level to the set OpenAI documents for
/// `reasoning.effort` (`minimal` / `low` / `medium` / `high`). `None` means the
/// request should not carry an effort at all, and Open Grok's broader menu
/// (`xhigh` / `max` / `ultra`) collapses onto `high` so a strict gateway cannot
/// 400 on a level it has never heard of.
fn openai_responses_effort(effort: ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::None => None,
        ReasoningEffort::Minimal => Some("minimal"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High
        | ReasoningEffort::Xhigh
        | ReasoningEffort::Max
        | ReasoningEffort::Ultra => Some("high"),
    }
}

/// Shape a Responses request for a user-supplied endpoint that speaks the
/// vanilla OpenAI Responses protocol.
///
/// The endpoint is third-party infrastructure whose exact support is unknown,
/// so the body keeps only the portable stateless surface: Open Grok replays the
/// full input every turn, `store: false` stays (the fork's zero-data-retention
/// default), and encrypted reasoning round-trips through the `include` the
/// shared builder already sets. Everything that would imply provider-side
/// continuity, account routing, hosted prompt templates, background execution,
/// or a session-derived cache key is removed rather than silently ignored — a
/// BYO address must never receive first-party routing metadata or a session id.
fn patch_openai_responses_request(request_body: &mut Value, policy: ResponsesRequestPolicy) {
    let Some(body) = request_body.as_object_mut() else {
        return;
    };
    for field in [
        "background",
        "client_metadata",
        "conversation",
        "metadata",
        "previous_response_id",
        "prompt",
        "prompt_cache_key",
        "prompt_cache_retention",
        "safety_identifier",
        "service_tier",
        "stream_options",
        "truncation",
    ] {
        body.remove(field);
    }
    // `store: false` deliberately survives: it is the fork's
    // zero-data-retention default, and dropping it would let a vanilla OpenAI
    // endpoint persist the conversation server-side.

    if let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) {
        // `summary` is a Codex-side preference, and `service_tier`-style
        // routing has no meaning on a BYO endpoint.
        reasoning.remove("summary");
        match policy.local_effort.and_then(openai_responses_effort) {
            Some(effort) => {
                reasoning.insert("effort".to_owned(), Value::String(effort.to_owned()));
            }
            None => {
                reasoning.remove("effort");
            }
        }
    }
    if body
        .get("reasoning")
        .and_then(Value::as_object)
        .is_some_and(serde_json::Map::is_empty)
    {
        body.remove("reasoning");
    }
    if body
        .get("include")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        body.remove("include");
    }
}

fn patch_meta_responses_request(request_body: &mut Value) {
    if let Some(body) = request_body.as_object_mut() {
        body.remove("include");
        body.remove("prompt_cache_key");
        body.remove("prompt_cache_retention");
        body.remove("store");
        if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
            input.retain(|item| item.get("type").and_then(Value::as_str) != Some("reasoning"));
        }
    }
    if let Some(reasoning) = request_body
        .get_mut("reasoning")
        .and_then(Value::as_object_mut)
    {
        reasoning.remove("summary");
    }
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

    #[test]
    fn responses_lite_moves_tools_and_instructions_into_the_input_prefix() {
        let mut request = base_request();
        request["tools"] = serde_json::json!([
            {"type": "function", "name": "lookup", "parameters": {"type": "object"}},
            {"type": "custom", "name": "exec", "format": {"type": "text"}},
            {"type": "web_search"},
            {"type": "image_generation"},
        ]);
        request["parallel_tool_calls"] = Value::Bool(true);
        let policy = ResponsesRequestPolicy {
            use_responses_lite: true,
            reasoning_summary: Some(ReasoningSummary::Detailed),
            ..Default::default()
        };
        provider_adapter(ModelProvider::Codex)
            .patch_responses_request(&mut request, policy.clone());
        assert!(request.get("instructions").is_none());
        assert!(request.get("tools").is_none());
        assert_eq!(request["parallel_tool_calls"], false);
        assert_eq!(request["reasoning"]["context"], "all_turns");
        assert_eq!(request["reasoning"]["summary"], "detailed");
        assert_eq!(request["input"][0]["type"], "additional_tools");
        assert_eq!(request["input"][0]["role"], "developer");
        let tools = &request["input"][0]["tools"];
        assert_eq!(tools.as_array().unwrap().len(), 1);
        assert_eq!(tools[0]["type"], "namespace");
        assert_eq!(tools[0]["name"], "functions");
        assert_eq!(tools[0]["tools"].as_array().unwrap().len(), 2);
        assert_eq!(request["input"][1]["role"], "developer");
        assert_eq!(request["input"][1]["content"][0]["text"], "base prompt");
        assert_eq!(
            request["input"][1]["internal_chat_message_metadata_passthrough"]["content_item_kinds"],
            serde_json::json!(["model.base_instructions"])
        );
        assert_eq!(request["input"][2]["role"], "user");
        let prepared = request.clone();
        provider_adapter(ModelProvider::Codex).patch_responses_request(&mut request, policy);
        assert_eq!(request, prepared);
    }

    #[test]
    fn responses_lite_preserves_namespaces_and_removes_only_image_detail_fields() {
        let mut request = serde_json::json!({
            "tools": [
                {"type": "namespace", "name": "remote", "tools": []},
                {"type": "function", "name": "lookup"},
                {"type": "namespace", "name": "functions", "description": "Local tools", "tools": [{"type": "custom", "name": "exec"}]},
            ],
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "original"},
                    {"type": "input_text", "text": "Describe this", "detail": "preserve"},
                ]},
                {"type": "function_call_output", "call_id": "lookup-call", "output": [{"type": "input_image", "image_url": "image", "detail": "high"}]},
                {"type": "custom_tool_call_output", "call_id": "exec-call", "output": [{"type": "input_image", "image_url": "image", "detail": "low"}]},
                {"type": "reasoning", "encrypted_content": "opaque"},
                {"type": "function_call", "name": "lookup", "call_id": "lookup-call", "arguments": "{}"},
                {"type": "custom_tool_call", "name": "exec", "call_id": "exec-call", "input": "text(1)"},
            ],
        });
        provider_adapter(ModelProvider::Codex).patch_responses_request(
            &mut request,
            ResponsesRequestPolicy {
                use_responses_lite: true,
                ..Default::default()
            },
        );
        assert_eq!(request["input"][0]["tools"][0]["name"], "remote");
        assert_eq!(
            request["input"][0]["tools"][1]["description"],
            "Local tools"
        );
        assert_eq!(
            request["input"][0]["tools"][1]["tools"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(request["input"][1]["content"][0].get("detail").is_none());
        assert_eq!(request["input"][1]["content"][1]["detail"], "preserve");
        assert!(request["input"][2]["output"][0].get("detail").is_none());
        assert!(request["input"][3]["output"][0].get("detail").is_none());
        assert_eq!(request["input"][4]["encrypted_content"], "opaque");
        assert_eq!(request["input"][5]["namespace"], "functions");
        assert_eq!(request["input"][6]["namespace"], "functions");
    }

    #[test]
    fn responses_lite_body_and_headers_require_codex_model_opt_in() {
        for (provider, enabled) in [
            (ModelProvider::Codex, false),
            (ModelProvider::Codex, true),
            (ModelProvider::Xai, true),
            (ModelProvider::DeepSeek, true),
            (ModelProvider::Meta, true),
        ] {
            let opted_in = provider == ModelProvider::Codex && enabled;
            let mut request = base_request();
            request["tools"] = serde_json::json!([{"type": "function", "name": "lookup"}]);
            provider_adapter(provider).patch_responses_request(
                &mut request,
                ResponsesRequestPolicy {
                    use_responses_lite: enabled,
                    ..Default::default()
                },
            );
            assert_eq!(request["input"][0]["type"] == "additional_tools", opted_in);
            assert_eq!(request.get("tools").is_none(), opted_in);
            assert_eq!(request["reasoning"]["context"] == "all_turns", opted_in);
            let mut headers = HeaderMap::new();
            headers.insert(RESPONSES_LITE_HEADER, HeaderValue::from_static("forged"));
            provider_adapter(provider).apply_responses_lite_header(&mut headers, enabled);
            assert_eq!(headers.contains_key(RESPONSES_LITE_HEADER), opted_in);
            if opted_in {
                assert_eq!(headers[RESPONSES_LITE_HEADER], "true");
            }
        }
    }

    #[test]
    fn codex_repairs_legacy_agent_message_ids_without_changing_opaque_history() {
        let legacy_id = "00000000-0000-7000-8000-000000000001";
        let legacy_message = serde_json::json!({
            "type": "agent_message", "id": legacy_id,
            "author": "/root", "recipient": "/root/worker",
            "content": [{"type": "encrypted_content", "encrypted_content": "opaque-test-content"}],
            "internal_chat_message_metadata_passthrough": {"test_marker": "retained"},
        });
        let mut repaired_message = legacy_message.clone();
        repaired_message["id"] = format!("amsg_{legacy_id}").into();
        let mut public_message = legacy_message.clone();
        public_message["content"] =
            serde_json::json!([{"type": "input_text", "text": "  task text\n"}]);
        let mut repaired_public_message = public_message.clone();
        repaired_public_message["id"] = repaired_message["id"].clone();
        let mut idless_message = legacy_message.clone();
        idless_message.as_object_mut().unwrap().remove("id");
        let mut opaque_id_message = legacy_message.clone();
        opaque_id_message["id"] = "amsg_provider-owned-id".into();
        let mut unknown_id_message = legacy_message.clone();
        unknown_id_message["id"] = "unknown-provider-id".into();
        let other_item = serde_json::json!({
            "type": "compaction", "id": legacy_id, "encrypted_content": "opaque-test-summary",
        });
        let input = serde_json::json!([
            legacy_message,
            public_message,
            repaired_message,
            idless_message,
            opaque_id_message,
            unknown_id_message,
            other_item,
        ]);

        for provider in [ModelProvider::Codex, ModelProvider::Xai] {
            for use_responses_lite in [false, true] {
                let mut request = serde_json::json!({"input": input});
                let policy = ResponsesRequestPolicy {
                    use_responses_lite,
                    ..Default::default()
                };
                provider_adapter(provider).patch_responses_request(&mut request, policy.clone());
                let offset = usize::from(provider == ModelProvider::Codex && use_responses_lite);
                let mut expected = input.as_array().unwrap().clone();
                if provider == ModelProvider::Codex {
                    expected[0] = repaired_message.clone();
                    expected[1] = repaired_public_message.clone();
                }
                assert_eq!(&request["input"].as_array().unwrap()[offset..], expected);
                let prepared = request.clone();
                provider_adapter(provider).patch_responses_request(&mut request, policy);
                assert_eq!(
                    request, prepared,
                    "retry preparation must preserve stable IDs"
                );
            }
        }
    }

    #[test]
    fn native_agent_and_freeform_patch_wire_survive_responses_lite() {
        let agent_message = serde_json::json!({
            "type":"agent_message","id":"amsg_test","author":"/root","recipient":"/root/worker",
            "content":[{"type":"encrypted_content","encrypted_content":"opaque-test-content"}],
        });
        let function_call = serde_json::json!({
            "type":"function_call","name":"send_message","namespace":"collaboration","call_id":"message-call",
            "arguments":"{\"message\":\"opaque-test-content\"}","encrypted_function_args":["message"],
        });
        let mut request = serde_json::json!({
            "tools":[
                {"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"send_message"}]},
                {"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: /patch/"}},
            ],
            "input":[agent_message.clone(), function_call.clone(),
                {"type":"custom_tool_call","name":"apply_patch","call_id":"patch-call","input":"raw patch"},
                {"type":"custom_tool_call_output","call_id":"patch-call","output":"success"}],
        });
        provider_adapter(ModelProvider::Codex).patch_responses_request(
            &mut request,
            ResponsesRequestPolicy {
                use_responses_lite: true,
                ..Default::default()
            },
        );
        assert_eq!(request["input"][1], agent_message);
        assert_eq!(request["input"][2], function_call);
        assert_eq!(request["input"][3]["namespace"], "functions");
        assert_eq!(request["input"][4]["type"], "custom_tool_call_output");
        assert_eq!(
            request["input"][0]["tools"][1]["tools"][0]["name"],
            "apply_patch"
        );
        assert_eq!(
            request["input"][0]["tools"][1]["tools"][0]["format"]["syntax"],
            "lark"
        );
    }

    #[test]
    fn responses_lite_normalizes_string_input_without_losing_instructions() {
        let mut request =
            serde_json::json!({"input": "Hello", "instructions": "Base instructions"});
        provider_adapter(ModelProvider::Codex).patch_responses_request(
            &mut request,
            ResponsesRequestPolicy {
                use_responses_lite: true,
                ..Default::default()
            },
        );
        assert_eq!(request["input"][0]["type"], "additional_tools");
        assert_eq!(
            request["input"][1]["content"][0]["text"],
            "Base instructions"
        );
        assert_eq!(request["input"][2]["content"][0]["text"], "Hello");
    }

    fn base_request() -> Value {
        serde_json::json!({
            "input": [
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "base prompt"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}
            ],
            "reasoning": {"effort": "xhigh", "summary": "concise"}
        })
    }

    fn sandboxed_permissions(auto_review_enabled: bool) -> CodexPermissions {
        CodexPermissions {
            sandbox: "seatbelt".to_owned(),
            sandbox_mode: "workspace-write".to_owned(),
            sandbox_profile: Some("workspace".to_owned()),
            network_access: true,
            writable_roots: vec!["/tmp/project".to_owned()],
            approval_policy: CodexApprovalPolicy::OnRequest,
            auto_review_enabled,
        }
    }

    #[test]
    fn codex_permissions_report_applied_sandbox_and_auto_review() {
        let mut request = base_request();
        let policy = ResponsesRequestPolicy {
            codex_permissions: Some(sandboxed_permissions(true)),
            session_id: Some("session-123".to_owned()),
            turn_id: Some("7".to_owned()),
            ..Default::default()
        };
        provider_adapter(ModelProvider::Codex)
            .patch_responses_request(&mut request, policy.clone());
        provider_adapter(ModelProvider::Codex).patch_responses_request(&mut request, policy);

        let metadata: Value = serde_json::from_str(
            request["client_metadata"][X_CODEX_TURN_METADATA_HEADER]
                .as_str()
                .expect("Codex metadata must be JSON-encoded"),
        )
        .unwrap();
        assert_eq!(metadata["sandbox"], "seatbelt");
        assert_eq!(metadata["sandbox_mode"], "workspace-write");
        assert_eq!(metadata["auto_review_enabled"], true);
        assert_eq!(metadata["session_id"], "session-123");
        assert_eq!(metadata["turn_id"], "7");
        let permission_items = request["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| {
                item["role"] == "developer"
                    && responses_message_text(item)
                        .is_some_and(|text| text.contains("<permissions instructions>"))
            })
            .collect::<Vec<_>>();
        assert_eq!(permission_items.len(), 1);
        let text = responses_message_text(permission_items[0]).unwrap();
        assert!(text.contains("`workspace-write`"));
        assert!(text.contains("`auto_review`"));
        assert!(text.contains("`/tmp/project`"));
    }

    #[test]
    fn codex_permissions_never_claim_an_unsandboxed_yolo_session_is_confined() {
        let mut request = base_request();
        provider_adapter(ModelProvider::Codex).patch_responses_request(
            &mut request,
            ResponsesRequestPolicy {
                codex_permissions: Some(CodexPermissions {
                    sandbox: "none".to_owned(),
                    sandbox_mode: "danger-full-access".to_owned(),
                    sandbox_profile: None,
                    network_access: true,
                    writable_roots: vec![],
                    approval_policy: CodexApprovalPolicy::Never,
                    auto_review_enabled: false,
                }),
                ..Default::default()
            },
        );
        let metadata: Value = serde_json::from_str(
            request["client_metadata"][X_CODEX_TURN_METADATA_HEADER]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["sandbox"], "none");
        assert_eq!(metadata["sandbox_mode"], "danger-full-access");
        let text = responses_message_text(&request["input"][0]).unwrap();
        assert!(text.contains("No filesystem sandbox is active"));
        assert!(text.contains("Approval policy is `never`"));
    }

    #[test]
    fn codex_permissions_report_read_only_network_restricted_manual_approval() {
        let mut request = base_request();
        provider_adapter(ModelProvider::Codex).patch_responses_request(
            &mut request,
            ResponsesRequestPolicy {
                codex_permissions: Some(CodexPermissions {
                    sandbox: "seatbelt".to_owned(),
                    sandbox_mode: "read-only".to_owned(),
                    sandbox_profile: Some("read-only".to_owned()),
                    network_access: false,
                    writable_roots: vec![],
                    approval_policy: CodexApprovalPolicy::OnRequest,
                    auto_review_enabled: false,
                }),
                ..Default::default()
            },
        );
        let metadata: Value = serde_json::from_str(
            request["client_metadata"][X_CODEX_TURN_METADATA_HEADER]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["sandbox_mode"], "read-only");
        assert_eq!(metadata["auto_review_enabled"], false);
        let text = responses_message_text(&request["input"][0]).unwrap();
        assert!(text.contains("workspace files cannot be modified"));
        assert!(text.contains("Network access is restricted"));
        assert!(text.contains("Approval policy is `on-request`"));
        assert!(!text.contains("`auto_review`"));
    }

    #[test]
    fn execution_permissions_never_cross_to_non_codex_providers() {
        let mut request = base_request();
        let original = request.clone();
        provider_adapter(ModelProvider::Xai).patch_responses_request(
            &mut request,
            ResponsesRequestPolicy {
                codex_permissions: Some(sandboxed_permissions(true)),
                ..Default::default()
            },
        );
        assert_eq!(request, original);
    }

    #[test]
    fn registry_is_complete_and_profiles_match_keys() {
        let expected = [
            ModelProvider::Xai,
            ModelProvider::Codex,
            ModelProvider::Kimi,
            ModelProvider::Fireworks,
            ModelProvider::DeepSeek,
            ModelProvider::Meta,
            ModelProvider::OpenCodeGo,
            ModelProvider::Wafer,
            ModelProvider::Zai,
            ModelProvider::Runinfra,
            ModelProvider::Gemini,
            ModelProvider::OpenRouter,
            ModelProvider::Custom,
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
            ModelProvider::DeepSeek,
            ModelProvider::Meta,
            ModelProvider::OpenCodeGo,
            ModelProvider::Wafer,
            ModelProvider::Zai,
            ModelProvider::Runinfra,
            ModelProvider::Gemini,
            ModelProvider::OpenRouter,
            ModelProvider::Custom,
        ] {
            let mut request = base_request();
            let original = request.clone();
            provider_adapter(provider).patch_responses_request(
                &mut request,
                ResponsesRequestPolicy {
                    multi_agent_v2: false,
                    local_effort: Some(ReasoningEffort::Max),
                    reasoning_summary: None,
                    ..Default::default()
                },
            );

            match provider {
                ModelProvider::Codex => {
                    assert_eq!(request["instructions"], "base prompt");
                    assert_eq!(request["input"].as_array().unwrap().len(), 1);
                    assert_eq!(request["reasoning"]["effort"], "max");
                    assert!(request["reasoning"].get("summary").is_none());
                }
                ModelProvider::DeepSeek => {
                    assert_eq!(request["input"], original["input"]);
                    assert_eq!(request["reasoning"]["effort"], "max");
                    assert!(request["reasoning"].get("summary").is_none());
                }
                ModelProvider::Meta => {
                    assert_eq!(request["input"], original["input"]);
                    assert_eq!(request["reasoning"]["effort"], "xhigh");
                    assert!(request["reasoning"].get("summary").is_none());
                }
                // The BYO Responses route clamps to OpenAI's documented effort
                // set and drops the Codex-only summary preference.
                ModelProvider::Custom => {
                    assert_eq!(request["input"], original["input"]);
                    assert_eq!(request["reasoning"]["effort"], "high");
                    assert!(request["reasoning"].get("summary").is_none());
                }
                _ => assert_eq!(request, original),
            }
        }
    }

    #[test]
    fn deepseek_strips_internal_message_model_ids() {
        use xai_grok_sampling_types::types::ChatRequestMessage;

        let assistant = ChatRequestMessage::assistant("previous turn", "deepseek-v4-pro", None);
        assert!(assistant.model_id.is_some(), "constructor stamps model_id");
        let mut request = ChatCompletionRequest::new("deepseek-v4-pro", vec![assistant]);

        provider_adapter(ModelProvider::DeepSeek).sanitize_chat_request(&mut request);

        assert!(request.messages[0].model_id.is_none());
    }

    #[test]
    fn prompt_cache_and_event_policy_follow_provider_profile() {
        let xai = provider_adapter(ModelProvider::Xai);
        let codex = provider_adapter(ModelProvider::Codex);
        let kimi = provider_adapter(ModelProvider::Kimi);
        let fireworks = provider_adapter(ModelProvider::Fireworks);
        let deepseek = provider_adapter(ModelProvider::DeepSeek);
        let meta = provider_adapter(ModelProvider::Meta);
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
        assert_eq!(deepseek.prompt_cache_key(Some("session")), None);
        assert!(!deepseek.supports_turn_state(&ApiBackend::Responses));
        assert!(!deepseek.sends_doom_loop_opt_in());
        assert!(deepseek.normalizes_response_events());
        assert!(
            deepseek
                .validate_backend(&ApiBackend::ChatCompletions)
                .is_ok()
        );
        assert!(deepseek.validate_backend(&ApiBackend::Responses).is_ok());
        assert!(deepseek.validate_backend(&ApiBackend::Messages).is_err());
        assert_eq!(meta.prompt_cache_key(Some("session")), None);
        assert!(!meta.supports_turn_state(&ApiBackend::Responses));
        assert!(!meta.sends_doom_loop_opt_in());
        assert!(meta.normalizes_response_events());
        assert!(meta.validate_backend(&ApiBackend::ChatCompletions).is_err());
        assert!(meta.validate_backend(&ApiBackend::Responses).is_ok());
        assert!(meta.validate_backend(&ApiBackend::Messages).is_err());

        let wafer = provider_adapter(ModelProvider::Wafer);
        assert_eq!(wafer.prompt_cache_key(Some("session")), None);
        assert!(!wafer.supports_turn_state(&ApiBackend::Responses));
        assert!(!wafer.sends_doom_loop_opt_in());
        assert!(!wafer.normalizes_response_events());
        assert!(wafer.validate_backend(&ApiBackend::ChatCompletions).is_ok());
        assert!(wafer.validate_backend(&ApiBackend::Responses).is_err());
        assert!(wafer.validate_backend(&ApiBackend::Messages).is_err());

        let zai = provider_adapter(ModelProvider::Zai);
        assert_eq!(zai.prompt_cache_key(Some("session")), None);
        assert!(!zai.supports_turn_state(&ApiBackend::Responses));
        assert!(!zai.sends_doom_loop_opt_in());
        assert!(!zai.normalizes_response_events());
        assert!(zai.validate_backend(&ApiBackend::ChatCompletions).is_ok());
        assert!(zai.validate_backend(&ApiBackend::Responses).is_err());
        assert!(zai.validate_backend(&ApiBackend::Messages).is_err());

        let runinfra = provider_adapter(ModelProvider::Runinfra);
        assert_eq!(runinfra.prompt_cache_key(Some("session")), None);
        assert!(!runinfra.supports_turn_state(&ApiBackend::Responses));
        assert!(!runinfra.sends_doom_loop_opt_in());
        assert!(!runinfra.normalizes_response_events());
        assert!(
            runinfra
                .validate_backend(&ApiBackend::ChatCompletions)
                .is_ok()
        );
        assert!(runinfra.validate_backend(&ApiBackend::Responses).is_err());
        assert!(runinfra.validate_backend(&ApiBackend::Messages).is_err());
    }

    #[test]
    fn deepseek_responses_normalizes_effort_and_drops_unsupported_state() {
        for (effort, expected) in [
            (ReasoningEffort::None, "none"),
            (ReasoningEffort::Minimal, "low"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "high"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::Xhigh, "high"),
            (ReasoningEffort::Max, "max"),
            (ReasoningEffort::Ultra, "max"),
        ] {
            let mut request = serde_json::json!({
                "input": [],
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "must-not-send",
                "reasoning": {"effort": "xhigh", "summary": "concise"},
                "store": true,
            });
            provider_adapter(ModelProvider::DeepSeek).patch_responses_request(
                &mut request,
                ResponsesRequestPolicy {
                    local_effort: Some(effort),
                    ..Default::default()
                },
            );
            assert_eq!(request["reasoning"]["effort"], expected);
            assert!(request["reasoning"].get("summary").is_none());
            assert!(request.get("include").is_none());
            assert!(request.get("prompt_cache_key").is_none());
            assert!(request.get("store").is_none());
        }
    }

    #[test]
    fn meta_responses_preserves_supported_effort_and_drops_unsupported_state() {
        for effort in ["low", "medium", "high", "xhigh"] {
            let mut request = serde_json::json!({
                "input": [
                    {"type": "message", "role": "user", "content": "first question"},
                    {"type": "reasoning", "id": "reasoning_1", "summary": [{"type": "summary_text", "text": "transient"}]},
                    {"type": "message", "role": "assistant", "content": "first answer"},
                    {"type": "function_call", "call_id": "call_1", "name": "lookup", "arguments": "{\"key\":\"value\"}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": "result"},
                    {"type": "message", "role": "user", "content": "follow-up"}
                ],
                "include": ["reasoning.encrypted_content"],
                "prompt_cache_key": "must-not-send",
                "prompt_cache_retention": "24h",
                "reasoning": {"effort": effort, "summary": "concise"},
                "store": true,
            });
            provider_adapter(ModelProvider::Meta)
                .patch_responses_request(&mut request, ResponsesRequestPolicy::default());
            assert_eq!(request["reasoning"]["effort"], effort);
            assert!(request["reasoning"].get("summary").is_none());
            assert!(request.get("include").is_none());
            assert!(request.get("prompt_cache_key").is_none());
            assert!(request.get("prompt_cache_retention").is_none());
            assert!(request.get("store").is_none());
            let input = request["input"].as_array().expect("input array");
            assert_eq!(input.len(), 5);
            assert!(
                input
                    .iter()
                    .all(|item| item.get("type").and_then(Value::as_str) != Some("reasoning"))
            );
            assert_eq!(input[0]["role"], "user");
            assert_eq!(input[1]["role"], "assistant");
            assert_eq!(input[2]["type"], "function_call");
            assert_eq!(input[3]["type"], "function_call_output");
            assert_eq!(input[4]["role"], "user");
        }
    }

    #[test]
    fn custom_provider_policy_is_fail_closed_for_untrusted_endpoints() {
        let custom = provider_adapter(ModelProvider::Custom);
        // Every wizard format is reachable, and none of them carries Codex
        // turn state into a third-party session.
        for backend in [
            ApiBackend::ChatCompletions,
            ApiBackend::Responses,
            ApiBackend::Messages,
        ] {
            assert!(
                custom.validate_backend(&backend).is_ok(),
                "a custom endpoint must accept {backend:?}"
            );
            assert!(
                !custom.supports_turn_state(&backend),
                "a custom endpoint cannot carry Codex turn state on {backend:?}"
            );
        }
        // A user-operated address gets no first-party identity or capabilities:
        // no session-derived cache key, no xAI request metadata, no hosted
        // tools, no Code Mode transport, and no xAI service access.
        assert_eq!(custom.prompt_cache_key(Some("session")), None);
        assert!(!custom.sends_doom_loop_opt_in());
        assert!(custom.normalizes_response_events());
        let profile = custom.profile();
        assert_eq!(
            profile.code_mode_transport,
            xai_grok_sampling_types::CodeModeTransport::Unsupported
        );
        assert_eq!(profile.hosted_tool_dialect, None);
        assert!(!profile.has_native_web_search());
        assert!(!profile.allows_xai_services());
        assert!(profile.session_auth.is_api_key_only());
        assert_eq!(profile.responses_dialect(), Some(ResponsesDialect::OpenAi));
    }

    #[test]
    fn custom_chat_request_keeps_only_the_portable_openai_surface() {
        use xai_grok_sampling_types::types::{ChatRequestMessage, ToolDefinition};

        let mut assistant = ChatRequestMessage::assistant("previous turn", "byo-model", None);
        assistant.reasoning = Some("gateway thoughts".to_owned());
        let mut request = ChatCompletionRequest::new("byo-model", vec![assistant]);
        request.temperature = Some(0.7);
        request.reasoning_effort = Some(ReasoningEffort::High);
        request.service_tier = Some("priority".to_owned());
        request.thinking = Some(ChatThinkingMode::enabled());
        request.reasoning = Some(ChatReasoningConfig::effort(ReasoningEffort::High));
        request.tools = Some(vec![ToolDefinition::function(
            "lookup",
            Some("Look up a value"),
            serde_json::json!({"type": "object", "properties": {"key": {"type": "string"}}}),
        )]);

        provider_adapter(ModelProvider::Custom).sanitize_chat_request(&mut request);

        assert_eq!(request.service_tier, None);
        assert!(
            request.thinking.is_none(),
            "a provider-specific thinking extension must not reach an unknown server"
        );
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(request.temperature, Some(0.7));
        assert!(
            request
                .messages
                .iter()
                .all(|message| message.model_id.is_none())
        );
        let wire = serde_json::to_value(&request).expect("serializes");
        assert!(wire.get("reasoning").is_none());
        assert!(wire["messages"][0].get("reasoning").is_none());
        assert_eq!(wire["tools"][0]["type"], "function");
    }

    #[test]
    fn custom_messages_pins_anthropic_version_without_overriding_an_explicit_one() {
        fn headers_for(backend: ApiBackend, pinned: Option<&str>) -> HeaderMap {
            let mut headers = HeaderMap::new();
            // Stand-ins for whatever `extra_headers` or a header injector put
            // on the request before provider defaults are applied.
            headers.insert("x-grok-session-id", HeaderValue::from_static("private"));
            if let Some(pinned) = pinned {
                headers.insert(
                    "anthropic-version",
                    HeaderValue::from_str(pinned).expect("valid header"),
                );
            }
            provider_adapter(ModelProvider::Custom).apply_default_headers(
                &mut headers,
                &SamplerConfig {
                    api_key: Some("test-key".to_owned()),
                    base_url: "https://byo.example/v1".to_owned(),
                    model: "byo-model".to_owned(),
                    max_completion_tokens: None,
                    temperature: None,
                    top_p: None,
                    api_backend: backend,
                    provider: ModelProvider::Custom,
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
                    service_tier: None,
                    reasoning_summary: None,
                    origin_client: None,
                    client_identifier: None,
                    deployment_id: None,
                    user_id: None,
                    client_version: None,
                    attribution_callback: None,
                    bearer_resolver: None,
                    supports_backend_search: false,
                    supports_standalone_web_search: false,
                    codex_multi_agent_v2: false,
                    use_responses_lite: false,
                    experimental_supported_tools: Vec::new(),
                    codex_permissions: None,
                    compactions_remaining: None,
                    compaction_at_tokens: None,
                    doom_loop_recovery: None,
                    header_injector: None,
                },
            );
            headers
        }

        let version = |headers: &HeaderMap| {
            headers
                .get("anthropic-version")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };

        let messages = headers_for(ApiBackend::Messages, None);
        assert_eq!(version(&messages).as_deref(), Some(ANTHROPIC_VERSION));
        assert!(
            messages.get("x-grok-session-id").is_none(),
            "private x-grok headers must never reach a user-operated address"
        );
        assert_eq!(
            version(&headers_for(ApiBackend::Messages, Some("2024-10-22"))).as_deref(),
            Some("2024-10-22"),
            "an explicitly pinned version on the model entry must win"
        );
        for backend in [ApiBackend::ChatCompletions, ApiBackend::Responses] {
            assert!(
                version(&headers_for(backend, None)).is_none(),
                "{backend:?} must not carry an Anthropic header"
            );
        }
    }

    #[test]
    fn custom_responses_sends_only_the_vanilla_openai_surface() {
        let mut request = serde_json::json!({
            "model": "byo-model",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "hello"}
                ]},
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "opaque"}
            ],
            "include": ["reasoning.encrypted_content"],
            "store": false,
            "reasoning": {"effort": "ultra", "summary": "concise"},
            "service_tier": "priority",
            "prompt_cache_key": "session-identity",
            "prompt_cache_retention": "24h",
            "previous_response_id": "resp_1",
            "conversation": "conv_1",
            "background": true,
            "truncation": "auto",
            "safety_identifier": "user-1",
            "client_metadata": {"x-codex-turn-metadata": "opaque"},
        });
        provider_adapter(ModelProvider::Custom).patch_responses_request(
            &mut request,
            ResponsesRequestPolicy {
                local_effort: Some(ReasoningEffort::Ultra),
                ..Default::default()
            },
        );

        // Stateless full-input replay: the endpoint's own encrypted reasoning
        // round-trips, and `store: false` survives so a vanilla OpenAI host
        // does not start persisting the conversation.
        assert_eq!(request["input"].as_array().expect("input").len(), 2);
        assert_eq!(request["store"], false);
        assert_eq!(request["include"][0], "reasoning.encrypted_content");
        assert_eq!(request["reasoning"]["effort"], "high");
        assert!(request["reasoning"].get("summary").is_none());
        for field in [
            "background",
            "client_metadata",
            "conversation",
            "previous_response_id",
            "prompt_cache_key",
            "prompt_cache_retention",
            "safety_identifier",
            "service_tier",
            "truncation",
        ] {
            assert!(
                request.get(field).is_none(),
                "{field} must not reach a user-operated endpoint"
            );
        }

        // No effort selected means no `reasoning` block at all, and an empty
        // `include` array is dropped rather than sent as `[]`.
        let mut bare = serde_json::json!({
            "input": [],
            "include": [],
            "reasoning": {"summary": "concise"},
        });
        provider_adapter(ModelProvider::Custom).patch_responses_request(
            &mut bare,
            ResponsesRequestPolicy {
                local_effort: Some(ReasoningEffort::None),
                ..Default::default()
            },
        );
        assert!(bare.get("reasoning").is_none());
        assert!(bare.get("include").is_none());
    }

    #[test]
    fn fireworks_forwards_standard_sampling_parameters_unchanged() {
        let mut request =
            ChatCompletionRequest::new("accounts/fireworks/models/glm-5p2", Vec::new());
        request.temperature = Some(0.7);
        request.top_p = Some(0.95);
        request.reasoning_effort = Some(ReasoningEffort::High);
        request.service_tier = Some("priority".to_owned());
        provider_adapter(ModelProvider::Fireworks).sanitize_chat_request(&mut request);
        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.top_p, Some(0.95));
        assert_eq!(request.reasoning_effort, None);
        assert_eq!(request.service_tier.as_deref(), Some("priority"));
    }

    #[test]
    fn wafer_sanitizes_internal_message_metadata_and_keeps_function_tools() {
        use xai_grok_sampling_types::types::{ChatRequestMessage, ToolDefinition};

        let assistant = ChatRequestMessage::assistant("previous turn", "wafer-model", None);
        let mut request = ChatCompletionRequest::new("wafer-model", vec![assistant]);
        request.temperature = Some(0.7);
        request.top_p = Some(0.95);
        request.reasoning_effort = Some(ReasoningEffort::High);
        request.service_tier = Some("priority".to_owned());
        request.tools = Some(vec![ToolDefinition::function(
            "lookup",
            Some("Look up a value"),
            serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"]
            }),
        )]);

        provider_adapter(ModelProvider::Wafer).sanitize_chat_request(&mut request);

        assert!(
            request
                .messages
                .iter()
                .all(|message| message.model_id.is_none())
        );
        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.top_p, Some(0.95));
        assert_eq!(request.reasoning_effort, None);
        assert_eq!(request.service_tier, None);
        let wire = serde_json::to_value(&request).expect("serializes");
        assert!(wire["messages"][0].get("model_id").is_none());
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["function"]["name"], "lookup");
    }

    #[test]
    fn zai_sanitizes_internal_metadata_but_keeps_reasoning_and_function_tools() {
        use xai_grok_sampling_types::types::{ChatRequestMessage, ToolDefinition};

        let assistant = ChatRequestMessage::assistant("previous turn", "glm-5.2", None);
        let mut request = ChatCompletionRequest::new("glm-5.2", vec![assistant]);
        request.temperature = Some(0.7);
        request.top_p = Some(0.95);
        request.reasoning_effort = Some(ReasoningEffort::High);
        request.service_tier = Some("priority".to_owned());
        request.tools = Some(vec![ToolDefinition::function(
            "lookup",
            Some("Look up a value"),
            serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"]
            }),
        )]);

        provider_adapter(ModelProvider::Zai).sanitize_chat_request(&mut request);

        // Internal Grok metadata is stripped...
        assert!(
            request
                .messages
                .iter()
                .all(|message| message.model_id.is_none())
        );
        assert_eq!(request.service_tier, None);
        // ...but reasoning_effort is preserved so callers can drive thinking
        // mode, and a requested effort turns on Z AI's `thinking` object.
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(
            request.thinking,
            Some(xai_grok_sampling_types::ChatThinkingMode::enabled())
        );
        assert_eq!(request.temperature, Some(0.7));
        assert_eq!(request.top_p, Some(0.95));
        let wire = serde_json::to_value(&request).expect("serializes");
        assert!(wire["messages"][0].get("model_id").is_none());
        assert_eq!(wire["reasoning_effort"], "high");
        assert_eq!(
            wire["thinking"],
            serde_json::json!({"type": "enabled", "clear_thinking": false})
        );
        assert_eq!(wire["tools"][0]["type"], "function");
        assert_eq!(wire["tools"][0]["function"]["name"], "lookup");
    }

    #[test]
    fn zai_thinking_follows_reasoning_effort() {
        // Without reasoning_effort, no `thinking` object is sent.
        let mut request = ChatCompletionRequest::new(
            "glm-5.2",
            vec![xai_grok_sampling_types::types::ChatRequestMessage::user(
                "hi",
            )],
        );
        provider_adapter(ModelProvider::Zai).sanitize_chat_request(&mut request);
        let wire = serde_json::to_value(&request).expect("serializes");
        assert!(wire.get("thinking").is_none());
        assert!(wire.get("reasoning_effort").is_none());

        // Z AI's documented top effort serializes as "max" and enables thinking.
        request.reasoning_effort = Some(ReasoningEffort::Max);
        provider_adapter(ModelProvider::Zai).sanitize_chat_request(&mut request);
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["reasoning_effort"], "max");
        assert_eq!(
            wire["thinking"],
            serde_json::json!({"type": "enabled", "clear_thinking": false})
        );
    }

    #[test]
    fn runinfra_keeps_reasoning_and_rewrites_deepseek_v4_flash_effort() {
        use xai_grok_sampling_types::types::{ChatRequestMessage, ToolDefinition};

        let assistant = ChatRequestMessage::assistant("previous turn", "deepseek-v4-flash", None);
        let mut request = ChatCompletionRequest::new("deepseek-v4-flash", vec![assistant]);
        request.temperature = Some(0.7);
        request.reasoning_effort = Some(ReasoningEffort::High);
        request.service_tier = Some("priority".to_owned());
        request.tools = Some(vec![ToolDefinition::function(
            "lookup",
            Some("Look up a value"),
            serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"]
            }),
        )]);

        provider_adapter(ModelProvider::Runinfra).sanitize_chat_request(&mut request);

        assert!(
            request
                .messages
                .iter()
                .all(|message| message.model_id.is_none())
        );
        assert_eq!(request.service_tier, None);
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::Max));
        assert!(request.thinking.is_none());
        assert_eq!(request.temperature, Some(0.7));
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["reasoning_effort"], "max");
        assert!(wire.get("thinking").is_none());
        assert_eq!(wire["tools"][0]["function"]["name"], "lookup");

        let mut other =
            ChatCompletionRequest::new("qwen3-8-27b", vec![ChatRequestMessage::user("hi")]);
        other.reasoning_effort = Some(ReasoningEffort::High);
        provider_adapter(ModelProvider::Runinfra).sanitize_chat_request(&mut other);
        assert_eq!(other.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(
            normalize_runinfra_deepseek_v4_flash_effort(ReasoningEffort::None),
            ReasoningEffort::None
        );
        assert_eq!(
            normalize_runinfra_deepseek_v4_flash_effort(ReasoningEffort::Xhigh),
            ReasoningEffort::Max
        );
    }

    #[test]
    fn gemini_keeps_reasoning_and_rewrites_unsupported_efforts() {
        use xai_grok_sampling_types::types::{ChatRequestMessage, ToolDefinition};

        let assistant = ChatRequestMessage::assistant("previous turn", "gemini-3.7-flash", None);
        let mut request = ChatCompletionRequest::new("gemini-3.7-flash", vec![assistant]);
        request.temperature = Some(0.7);
        request.reasoning_effort = Some(ReasoningEffort::Minimal);
        request.service_tier = Some("priority".to_owned());
        request.thinking = Some(ChatThinkingMode::enabled());
        request.tools = Some(vec![ToolDefinition::function(
            "lookup",
            Some("Look up a value"),
            serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"]
            }),
        )]);

        provider_adapter(ModelProvider::Gemini).sanitize_chat_request(&mut request);

        assert!(
            request
                .messages
                .iter()
                .all(|message| message.model_id.is_none())
        );
        assert_eq!(request.service_tier, None);
        assert!(request.thinking.is_none());
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(request.temperature, Some(0.7));
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["reasoning_effort"], "low");
        assert!(wire.get("thinking").is_none());
        assert_eq!(wire["tools"][0]["function"]["name"], "lookup");

        let mut none_off =
            ChatCompletionRequest::new("gemini-3.6-flash", vec![ChatRequestMessage::user("hi")]);
        none_off.reasoning_effort = Some(ReasoningEffort::None);
        provider_adapter(ModelProvider::Gemini).sanitize_chat_request(&mut none_off);
        assert_eq!(none_off.reasoning_effort, None);

        let mut lite = ChatCompletionRequest::new(
            "gemini-3.5-flash-lite",
            vec![ChatRequestMessage::user("hi")],
        );
        lite.reasoning_effort = Some(ReasoningEffort::Minimal);
        provider_adapter(ModelProvider::Gemini).sanitize_chat_request(&mut lite);
        assert_eq!(lite.reasoning_effort, Some(ReasoningEffort::Minimal));

        let mut pro = ChatCompletionRequest::new(
            "gemini-3.1-pro-preview",
            vec![ChatRequestMessage::user("hi")],
        );
        pro.reasoning_effort = Some(ReasoningEffort::Ultra);
        provider_adapter(ModelProvider::Gemini).sanitize_chat_request(&mut pro);
        assert_eq!(pro.reasoning_effort, Some(ReasoningEffort::High));

        assert_eq!(
            normalize_gemini_reasoning_effort(Some("gemini-3.7-flash"), ReasoningEffort::Minimal),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            normalize_gemini_reasoning_effort(Some("gemini-3.6-flash"), ReasoningEffort::Minimal),
            Some(ReasoningEffort::Minimal)
        );
    }

    #[test]
    fn openrouter_uses_nested_reasoning_and_message_field() {
        use xai_grok_sampling_types::types::{ChatRequestMessage, ToolDefinition};

        let assistant = ChatRequestMessage::assistant(
            "previous turn",
            "anthropic/claude-sonnet-4",
            Some("prior thoughts".to_owned()),
        );
        let mut request = ChatCompletionRequest::new("anthropic/claude-sonnet-4", vec![assistant]);
        request.temperature = Some(0.2);
        request.reasoning_effort = Some(ReasoningEffort::Ultra);
        request.service_tier = Some("priority".to_owned());
        request.thinking = Some(ChatThinkingMode::enabled());
        request.tools = Some(vec![ToolDefinition::function(
            "lookup",
            Some("Look up a value"),
            serde_json::json!({
                "type": "object",
                "properties": {"key": {"type": "string"}},
                "required": ["key"]
            }),
        )]);

        provider_adapter(ModelProvider::OpenRouter).sanitize_chat_request(&mut request);

        assert!(
            request
                .messages
                .iter()
                .all(|message| message.model_id.is_none())
        );
        assert_eq!(request.service_tier, None);
        assert!(request.thinking.is_none());
        assert_eq!(request.reasoning_effort, None);
        assert_eq!(
            request.reasoning,
            Some(ChatReasoningConfig::effort(ReasoningEffort::Max))
        );
        assert_eq!(
            request.messages[0].reasoning.as_deref(),
            Some("prior thoughts")
        );
        assert!(request.messages[0].reasoning_content.is_none());
        assert_eq!(request.temperature, Some(0.2));

        let wire = serde_json::to_value(&request).expect("serializes");
        assert!(wire.get("reasoning_effort").is_none());
        assert_eq!(wire["reasoning"]["effort"], "max");
        assert!(wire.get("thinking").is_none());
        assert_eq!(wire["messages"][0]["reasoning"], "prior thoughts");
        assert!(wire["messages"][0].get("reasoning_content").is_none());
        assert_eq!(wire["tools"][0]["function"]["name"], "lookup");

        let mut none_off = ChatCompletionRequest::new(
            "anthropic/claude-sonnet-4",
            vec![ChatRequestMessage::user("hi")],
        );
        none_off.reasoning_effort = Some(ReasoningEffort::None);
        provider_adapter(ModelProvider::OpenRouter).sanitize_chat_request(&mut none_off);
        assert_eq!(none_off.reasoning_effort, None);
        assert_eq!(
            none_off.reasoning,
            Some(ChatReasoningConfig::effort(ReasoningEffort::None))
        );

        let mut unset =
            ChatCompletionRequest::new("openai/gpt-4o", vec![ChatRequestMessage::user("hi")]);
        provider_adapter(ModelProvider::OpenRouter).sanitize_chat_request(&mut unset);
        assert!(unset.reasoning.is_none());
        assert!(unset.reasoning_effort.is_none());

        // Both the shared shorthand and explicitly supplied nested controls
        // must use the gateway's effort vocabulary. The shorthand is
        // authoritative when both forms are supplied.
        for effort in [
            ReasoningEffort::None,
            ReasoningEffort::Minimal,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::Xhigh,
            ReasoningEffort::Max,
            ReasoningEffort::Ultra,
        ] {
            let expected = if effort == ReasoningEffort::Ultra {
                ReasoningEffort::Max
            } else {
                effort
            };
            for nested in [false, true] {
                let mut request = ChatCompletionRequest::new("model", Vec::new());
                request.reasoning = Some(ChatReasoningConfig::effort(if nested {
                    effort
                } else {
                    ReasoningEffort::Medium
                }));
                if !nested {
                    request.reasoning_effort = Some(effort);
                }
                provider_adapter(ModelProvider::OpenRouter).sanitize_chat_request(&mut request);
                let wire = serde_json::to_value(request).expect("serializes");
                assert!(wire.get("reasoning_effort").is_none());
                assert_eq!(wire["reasoning"]["effort"], expected.as_str());
            }
        }
    }

    #[test]
    fn fireworks_expands_annotation_only_tool_schemas() {
        use xai_grok_sampling_types::types::ToolDefinition;

        let mut request =
            ChatCompletionRequest::new("accounts/fireworks/models/glm-5p2", Vec::new()).with_tools(
                vec![ToolDefinition::function(
                    "workflow",
                    Some("Launch a workflow"),
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "args": {
                                "description": "JSON value bound to the script's args global.",
                                "default": null
                            },
                            "name": {"type": "string"}
                        }
                    }),
                )],
            );

        provider_adapter(ModelProvider::Fireworks).sanitize_chat_request(&mut request);

        let parameters = &request.tools.as_ref().unwrap()[0].function.parameters;
        let args = &parameters["properties"]["args"];
        assert_eq!(args["default"], Value::Null);
        assert_eq!(
            args["description"],
            "JSON value bound to the script's args global."
        );
        assert_eq!(args["anyOf"].as_array().unwrap().len(), 7);
        assert_eq!(parameters["properties"]["name"]["type"], "string");
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
                service_tier: None,
                reasoning_summary: None,
                origin_client: None,
                client_identifier: None,
                deployment_id: None,
                user_id: None,
                client_version: client_version.map(str::to_string),
                attribution_callback: None,
                bearer_resolver: None,
                supports_backend_search: false,
                supports_standalone_web_search: false,
                codex_multi_agent_v2: false,
                use_responses_lite: false,
                experimental_supported_tools: Vec::new(),
                codex_permissions: None,
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
