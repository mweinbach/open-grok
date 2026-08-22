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

pub(crate) const MULTI_AGENT_MODE_OPEN_TAG: &str = "<multi_agent_mode>";
pub(crate) const MULTI_AGENT_MODE_CLOSE_TAG: &str = "</multi_agent_mode>";
pub(crate) const EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT: &str = "Any earlier instruction enabling proactive multi-agent delegation no longer applies. Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
pub(crate) const PROACTIVE_MULTI_AGENT_MODE_TEXT: &str = "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode developer message changes it.";

/// Provider-neutral input to the Responses request patching seam.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResponsesRequestPolicy {
    pub multi_agent_v2: bool,
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
        let affinity_id = headers.cache_affinity_id.unwrap_or(headers.session_id);
        let builder = self.apply_session_affinity_headers(builder, Some(affinity_id));
        if self.profile().request_metadata == RequestMetadataPolicy::XGrokHeaders {
            headers.apply_x_grok(builder)
        } else {
            builder
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
        }
    }

    /// Return the provider-owned cache key derived from stable request state.
    fn prompt_cache_key(&self, session_id: Option<&str>) -> Option<String> {
        match self.profile().responses_dialect() {
            None
            | Some(ResponsesDialect::Xai | ResponsesDialect::DeepSeek | ResponsesDialect::Meta) => {
                None
            }
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
            | Some(ResponsesDialect::Xai | ResponsesDialect::DeepSeek | ResponsesDialect::Meta) => {
            }
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
                | ResponsesDialect::Meta,
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
        if let Some(effort) = request.reasoning_effort.take() {
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

/// Complete registry for the built-in providers.
pub static PROVIDER_REGISTRY: [ProviderRegistration; 12] = [
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
    }
}

fn patch_codex_responses_request(request_body: &mut Value, policy: ResponsesRequestPolicy) {
    patch_codex_instruction_roles(request_body);

    if let Some(permissions) = policy.codex_permissions.as_ref() {
        patch_codex_permissions(request_body, permissions, &policy);
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

        assert_eq!(
            normalize_openrouter_reasoning_effort(ReasoningEffort::Ultra),
            ReasoningEffort::Max
        );
        assert_eq!(
            normalize_openrouter_reasoning_effort(ReasoningEffort::Xhigh),
            ReasoningEffort::Xhigh
        );
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
