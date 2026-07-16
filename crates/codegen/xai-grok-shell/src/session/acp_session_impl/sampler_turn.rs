//! Sampler-turn pipeline for `SessionActor`: tool definitions, model auth
//! facts/gates and retry, sampler config reconstruction, sampling-failure
//! recovery, and per-response usage recording.
use super::*;

/// Independent model work that must not inherit the primary chat model's
/// provider, credentials, or reasoning budget by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AuxiliaryModelPurpose {
    Recap,
    Memory,
}

/// Fully-resolved auxiliary sampling route. Keeping the client and all of the
/// request metadata together prevents a model slug from being sent through a
/// different provider's endpoint (the old memory-flush behavior).
pub(super) struct PreparedAuxiliarySampling {
    pub client: xai_grok_sampler::SamplingClient,
    pub model: String,
    pub context_window: u64,
    pub reasoning_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
    pub provider: xai_grok_sampling_types::ModelProvider,
}

const AUTOMATIC_CODEX_AUX_MODEL: &str = "gpt-5.6-terra";

fn automatic_auxiliary_model(
    provider: xai_grok_sampling_types::ModelProvider,
) -> Option<&'static str> {
    (provider == xai_grok_sampling_types::ModelProvider::Codex).then_some(AUTOMATIC_CODEX_AUX_MODEL)
}

fn auxiliary_reasoning_effort(
    provider: xai_grok_sampling_types::ModelProvider,
    supported: bool,
) -> Option<xai_grok_sampling_types::ReasoningEffort> {
    supported.then_some(match provider {
        xai_grok_sampling_types::ModelProvider::Codex => {
            xai_grok_sampling_types::ReasoningEffort::Medium
        }
        xai_grok_sampling_types::ModelProvider::Xai => {
            xai_grok_sampling_types::ReasoningEffort::Low
        }
    })
}

struct AuthManagerBearerResolver(std::sync::Arc<crate::auth::AuthManager>);

impl std::fmt::Debug for AuthManagerBearerResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthManagerBearerResolver").finish()
    }
}

impl xai_grok_sampler::BearerResolver for AuthManagerBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        self.0.current_or_expired().map(|auth| auth.key)
    }

    fn fail_closed_on_missing(&self) -> bool {
        false
    }
}

/// Provider-owned resolver for an xAI auxiliary route selected from a Codex
/// chat. Unlike the active-session resolver above, this one must fail closed
/// rather than fall back to the static credential captured at construction.
struct XaiAuxAuthManagerBearerResolver(std::sync::Arc<crate::auth::AuthManager>);

impl std::fmt::Debug for XaiAuxAuthManagerBearerResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XaiAuxAuthManagerBearerResolver").finish()
    }
}

impl xai_grok_sampler::BearerResolver for XaiAuxAuthManagerBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        self.0.current_or_expired().map(|auth| auth.key)
    }

    fn fail_closed_on_missing(&self) -> bool {
        true
    }
}
/// Auth-failure detector for tool errors. Matches strictly on HTTP 401
/// when the error carries a structured status code, mirroring
/// `SamplingError::is_auth_error` in xai-grok-sampling-types: 403 is
/// deliberately excluded because it means "authenticated but forbidden"
/// (content-safety blocks, ZDR-gated requests, remote settings gates), where
/// a token refresh would be a no-op and would surface to the client as
/// a spurious auth_required teardown.
///
/// String fallbacks remain for tools that surface auth failures without
/// going through the structured `HttpFailure` path (e.g. JSON-only
/// `invalid_token` payloads, BYOK key-validation messages).
pub(super) fn is_auth_tool_error(err: &xai_tool_runtime::ToolError) -> bool {
    if let Some(details) = &err.details
        && let Some(status) = details
            .get(HTTP_STATUS_DETAILS_KEY)
            .and_then(|s| s.as_u64())
    {
        return status == 401;
    }
    let lower = err.to_string().to_ascii_lowercase();
    lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_token")
}
/// Gate inputs bundled with the composed decision so the 401-recovery log can
/// report the components.
#[derive(Clone, Copy)]
struct SessionTokenAuthGate {
    is_session_based: bool,
    model_byok: crate::agent::auth_method::ModelByok,
    /// Whether the request targets a first-party host. Lets an `Unknown`
    /// BYOK status still refresh against cli-chat-proxy / `*.x.ai` without
    /// risking a session-token leak to a third-party BYOK endpoint.
    endpoint_is_first_party: bool,
}
impl SessionTokenAuthGate {
    /// Single place `is_session_based` / `endpoint_is_first_party` are derived,
    /// so all call sites assemble the gate identically.
    fn new(
        auth_method_id: Option<&acp::AuthMethodId>,
        model_byok: crate::agent::auth_method::ModelByok,
        base_url: &str,
    ) -> Self {
        Self {
            is_session_based: auth_method_id
                .is_some_and(crate::agent::auth_method::is_session_based_method),
            model_byok,
            endpoint_is_first_party: crate::util::is_first_party_xai_url(base_url),
        }
    }
    fn active(self) -> bool {
        crate::agent::auth_method::session_token_auth_gate(
            self.is_session_based,
            self.model_byok,
            self.endpoint_is_first_party,
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TurnAuthRefreshRoute {
    CodexOAuth,
    XaiSession,
    ConfigApiKey,
}

pub(super) fn turn_auth_refresh_route(
    provider: xai_grok_sampling_types::ModelProvider,
    auth_type: xai_chat_state::AuthType,
) -> TurnAuthRefreshRoute {
    match (provider.profile().session_auth, auth_type) {
        (
            xai_grok_sampling_types::BuiltInSessionAuthKind::ApiKeyOnly,
            xai_chat_state::AuthType::SessionToken,
        ) => TurnAuthRefreshRoute::ConfigApiKey,
        (
            xai_grok_sampling_types::BuiltInSessionAuthKind::CodexOAuth,
            xai_chat_state::AuthType::SessionToken,
        ) => TurnAuthRefreshRoute::CodexOAuth,
        (
            xai_grok_sampling_types::BuiltInSessionAuthKind::XaiSession,
            xai_chat_state::AuthType::SessionToken,
        ) => TurnAuthRefreshRoute::XaiSession,
        (_, xai_chat_state::AuthType::ApiKey) => TurnAuthRefreshRoute::ConfigApiKey,
    }
}
/// Run a tool call; on an auth-shaped failure, attempt recovery via
/// `AuthManager` and one retry. When `shared_recovery` is `Some`, concurrent
/// 401s in the same batch deduplicate via `OnceCell::get_or_init`.
pub(super) async fn call_with_auth_retry<F, Fut>(
    auth_manager: Option<&std::sync::Arc<crate::auth::AuthManager>>,
    shared_recovery: Option<&tokio::sync::OnceCell<bool>>,
    tool_name: &str,
    mut call: F,
) -> Result<xai_grok_tools::types::output::ToolRunResult, xai_tool_runtime::ToolError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<
            Output = Result<
                xai_grok_tools::types::output::ToolRunResult,
                xai_tool_runtime::ToolError,
            >,
        >,
{
    let result = call().await;
    let Err(ref err) = result else { return result };
    if !is_auth_tool_error(err) {
        return result;
    }
    let Some(am) = auth_manager else {
        return result;
    };
    let src = crate::auth::recovery::RecoverySource::Background;
    let recovered = match shared_recovery {
        Some(cell) => *cell.get_or_init(|| am.try_recover_unauthorized(src)).await,
        None => am.try_recover_unauthorized(src).await,
    };
    if recovered {
        tracing::info!(
            tool = tool_name,
            "auth recovery: tool 401, recovered, retrying"
        );
        call().await
    } else {
        tracing::warn!(tool = tool_name, "auth recovery: tool 401, refresh failed");
        xai_grok_telemetry::unified_log::warn(
            "auth recovery: tool 401, refresh failed",
            None,
            Some(serde_json::json!({ "tool" : tool_name })),
        );
        result
    }
}
impl SessionActor {
    pub(super) async fn prepare_tool_definitions_timed(&self) -> (Vec<ToolDefinition>, u64) {
        let mcp_wait_start = std::time::Instant::now();
        match self.mcp_strategy {
            McpInitStrategy::Blocking => {
                if !self.mcp_state.lock().await.is_initialized() {
                    tracing::info!(
                        "Blocking strategy: waiting for MCP initialization before first prompt..."
                    );
                    self.wait_for_mcp_initialized().await;
                }
            }
            McpInitStrategy::Progressive => {}
        }
        let mcp_wait_ms = mcp_wait_start.elapsed().as_millis() as u64;
        let defs = self.prepare_tool_definitions_inner().await;
        (defs, mcp_wait_ms)
    }
    pub(super) async fn prepare_tool_definitions(&self) -> Vec<ToolDefinition> {
        self.prepare_tool_definitions_timed().await.0
    }
    /// The exact tool specs a turn sends, BEFORE the turn-specific
    /// structured-output append. Single source of truth shared by the turn
    /// (`acp_session_impl/turn.rs`) and the `SnapshotToolDefinitions` handler, so
    /// a verbatim-fork child's tool prefix can never silently drift from what the
    /// parent turn actually sends. `defs` is the already-resolved tool list
    /// (`prepare_tool_definitions_*`); this applies only the `web_search` drop
    /// under backend search and the `ToolSpec::from` mapping.
    pub(crate) fn turn_base_tool_specs(
        &self,
        defs: &[ToolDefinition],
        provider: xai_grok_sampling_types::ModelProvider,
    ) -> Vec<ToolSpec> {
        let use_backend_search =
            self.agent.borrow().backend_search_enabled() && self.supports_backend_search.get();
        defs.iter()
            .filter(|td| {
                self.local_tool_allowed_for_provider(&td.function.name, provider)
                    && (!use_backend_search || td.function.name != "web_search")
            })
            .cloned()
            .map(ToolSpec::from)
            .collect()
    }

    /// Keep provider-specific local tools out of Codex requests and Code Mode
    /// dispatch. Search may cross providers only through an explicit
    /// non-default model route; xAI media tools have no equivalent explicit
    /// provider-neutral opt-in and stay xAI-only.
    pub(crate) fn local_tool_allowed_for_provider(
        &self,
        tool_name: &str,
        provider: xai_grok_sampling_types::ModelProvider,
    ) -> bool {
        if provider.profile().allows_xai_services() {
            return true;
        }
        match tool_name {
            "web_search" => !self.rebuild_spec.implicit_local_web_search,
            "image_gen" | "image_edit" | "image_to_video" | "reference_to_video" => false,
            // Memory storage and FTS are local/provider-neutral. Semantic
            // search may independently use the user's connected xAI embedding
            // route while chat runs through Codex.
            "memory_search" | "memory_get" => self.memory.is_enabled(),
            _ => true,
        }
    }
    pub(super) async fn prepare_tool_definitions_inner(&self) -> Vec<ToolDefinition> {
        let bridge = self.agent.borrow().tool_bridge().clone();
        let defs = bridge.tool_definitions_builtins_only().await;
        let plan_active = self.plan_mode.lock().is_active();
        filter_cursor_tools_by_plan_mode(defs, plan_active)
    }
    /// Memoized per-model [`ModelAuthFacts`](crate::agent::config::ModelAuthFacts),
    /// keyed by `model_id`.
    ///
    /// A fresh `Unknown` (config currently unparseable) falls back to the last
    /// definite value for the same `model_id` rather than demoting a live session
    /// to non-refreshable api-key mode. Because a config edit can turn the
    /// currently-selected model into a per-model BYOK model without changing
    /// `model_id`, keying on `model_id` alone is insufficient — each
    /// model/credential chokepoint must clear this memo (`replace(None)`).
    pub(super) fn model_auth_facts(&self, model_id: &str) -> crate::agent::config::ModelAuthFacts {
        use crate::agent::auth_method::ModelByok;
        if let Some((cached_id, facts)) = self.model_auth_facts.borrow().as_ref()
            && cached_id == model_id
            && facts.byok != ModelByok::Unknown
        {
            return *facts;
        }
        let fresh = crate::agent::config::resolve_model_auth_facts(model_id);
        if fresh.byok == ModelByok::Unknown {
            if let Some((cached_id, facts)) = self.model_auth_facts.borrow().as_ref()
                && cached_id == model_id
            {
                return *facts;
            }
            return fresh;
        }
        *self.model_auth_facts.borrow_mut() = Some((model_id.to_string(), fresh));
        fresh
    }
    /// Gate inputs for `model_id` routed to `base_url`. See
    /// [`crate::agent::auth_method::session_token_auth_gate`] for the rationale
    /// (`base_url` keeps an `Unknown` BYOK status refreshable only
    /// against first-party xAI hosts).
    fn auth_gate(&self, model_id: &str, base_url: &str) -> SessionTokenAuthGate {
        let byok = self.model_auth_facts(model_id).byok;
        let auth_method = self.auth_method_id.load();
        SessionTokenAuthGate::new(auth_method.as_deref(), byok, base_url)
    }
    /// Emit a unified-log breadcrumb whenever the session-token refresh gate is
    /// evaluated with an **`Unknown`** per-model BYOK status on a session-based
    /// method — the condition that (pre-fix) silently demoted live sessions to
    /// stale-token 401s. The uploaded per-turn unified log then shows whether
    /// the first-party-endpoint fallback kept refresh active or withheld it, so
    /// we can confirm the fix works (or catch a residual demotion) per session
    /// even when server-side metrics only show the aggregate 401. No-op for a
    /// definite `Byok`/`NotByok`, so steady-state turns stay quiet — a burst of
    /// these is itself the signal that `Unknown` is being hit in the field.
    fn log_auth_gate_unknown(&self, site: &str, gate: SessionTokenAuthGate, base_url: &str) {
        use crate::agent::auth_method::ModelByok;
        if gate.model_byok != ModelByok::Unknown || !gate.is_session_based {
            return;
        }
        let refresh_active = gate.active();
        let ctx = serde_json::json!(
            { "site" : site, "model_byok" : gate.model_byok.as_str(), "is_session_based"
            : gate.is_session_based, "endpoint_is_first_party" : gate
            .endpoint_is_first_party, "refresh_active" : refresh_active, "base_url" :
            base_url, }
        );
        let sid = Some(self.session_info.id.0.as_ref());
        if refresh_active {
            xai_grok_telemetry::unified_log::info(
                "auth gate: Unknown BYOK on first-party endpoint — session-token refresh kept active",
                sid,
                Some(ctx),
            );
        } else {
            xai_grok_telemetry::unified_log::warn(
                "auth gate: Unknown BYOK on non-first-party endpoint — refresh withheld (may surface stale-token 401)",
                sid,
                Some(ctx),
            );
        }
    }
    /// Reconstruct a full `SamplerConfig` (with credentials) by combining
    /// the actor's `SamplingConfig` and `Credentials`. Folds in the
    /// URL-derived headers (cli-chat-proxy auth, the staging auth header)
    /// so the sampler crate stays URL-agnostic.
    pub(super) async fn reconstruct_full_config(&self) -> SamplingConfig {
        #[allow(clippy::items_after_statements)]
        #[derive(Debug)]
        struct TraceContextInjector;
        impl xai_grok_sampler::HeaderInjector for TraceContextInjector {
            fn inject(&self, headers: &mut reqwest::header::HeaderMap) {
                if let Some(tp) = xai_file_utils::trace_context::current_traceparent()
                    && let Ok(v) = reqwest::header::HeaderValue::from_str(&tp)
                {
                    headers.insert("traceparent", v);
                }
            }
        }
        let cfg = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .unwrap_or_else(|| xai_grok_sampling_types::SamplingConfig {
                base_url: String::new(),
                model: String::new(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: Default::default(),
                provider: Default::default(),
                extra_headers: Default::default(),
                context_window: std::num::NonZeroU64::new(256_000).unwrap(),
                reasoning_effort: None,
                stream_tool_calls: None,
            });
        let creds = self.chat_state_handle.get_credentials().await;
        let model_facts = self.model_auth_facts(cfg.model.as_str());
        let auth_method = self.auth_method_id.load();
        let gate =
            SessionTokenAuthGate::new(auth_method.as_deref(), model_facts.byok, &cfg.base_url);
        let use_bearer_resolver = gate.active();
        self.log_auth_gate_unknown("reconstruct_full_config", gate, &cfg.base_url);
        let auth_scheme =
            crate::agent::config::effective_auth_scheme(cfg.provider, model_facts.auth_scheme);
        let mut extra_headers = cfg.extra_headers;
        crate::agent::config::inject_url_derived_headers(
            &mut extra_headers,
            creds.alpha_test_key.as_deref(),
            &cfg.base_url,
        );
        let compaction_at_tokens = self.compaction_at_tokens.get();
        let compactions_remaining = self.compactions_remaining.get();
        if compactions_remaining.is_some() || compaction_at_tokens.is_some() {
            let has_compaction_summary = self
                .chat_state_handle
                .get_last_compaction_prompt_index()
                .await
                .is_some();
            if let Some(value) =
                compactions_remaining.and_then(|c| c.resolve(has_compaction_summary))
            {
                extra_headers.insert("x-compactions-remaining".to_string(), value.to_string());
            }
            if !has_compaction_summary
                && let Some(value) = compaction_at_tokens.and_then(|c| {
                    c.resolve(
                        cfg.context_window.get(),
                        self.compaction.threshold_percent.get(),
                    )
                })
            {
                extra_headers.insert("x-compaction-at".to_string(), value.to_string());
            }
        }
        // Chat state carries the provider resolved from the live model entry.
        // This is authoritative for prefetched remote-only models that the
        // disk-backed model-facts resolver cannot see.
        let provider = cfg.provider;
        let profile = provider.profile();
        let use_codex_bearer_resolver = profile.session_auth.is_codex()
            && creds.auth_type == xai_chat_state::AuthType::SessionToken;
        let codex_bearer_resolver = use_codex_bearer_resolver.then(|| {
            std::sync::Arc::new(crate::codex_auth::CodexBearerResolver::from_headers(
                &extra_headers,
            )) as xai_grok_sampler::SharedBearerResolver
        });
        let codex_multi_agent_v2 = self
            .models_manager
            .model_supports_codex_multi_agent_v2(&cfg.model);
        let reasoning_summary = self.models_manager.model_reasoning_summary(&cfg.model);
        SamplingConfig {
            api_key: creds.api_key,
            base_url: cfg.base_url,
            model: cfg.model,
            max_completion_tokens: cfg.max_completion_tokens,
            temperature: cfg.temperature,
            top_p: cfg.top_p,
            api_backend: cfg.api_backend,
            provider,
            auth_scheme,
            extra_headers,
            context_window: cfg.context_window.get(),
            client_version: creds.client_version,
            reasoning_effort: cfg.reasoning_effort,
            reasoning_summary,
            force_http1: false,
            max_retries: Some(self.max_retries),
            stream_tool_calls: cfg.stream_tool_calls.unwrap_or(false),
            idle_timeout_secs: None,
            client_identifier: profile
                .request_metadata
                .sends_x_grok_headers()
                .then(|| self.client_identifier.clone())
                .flatten(),
            deployment_id: profile
                .request_metadata
                .sends_x_grok_headers()
                .then(|| {
                    crate::managed_config::resolve_deployment_id(
                        crate::managed_config::resolve_deployment_key().as_deref(),
                    )
                })
                .flatten(),
            user_id: profile
                .request_metadata
                .sends_x_grok_headers()
                .then(|| {
                    self.auth_manager
                        .as_ref()
                        .and_then(|am| am.current_or_expired())
                        .filter(|a| a.is_xai_auth())
                        .map(|a| a.user_id)
                })
                .flatten(),
            origin_client: self.origin_client.clone(),
            attribution_callback: profile
                .session_auth
                .is_xai()
                .then(|| self.attribution_callback.clone())
                .flatten(),
            bearer_resolver: if use_codex_bearer_resolver {
                codex_bearer_resolver
            } else if profile.session_auth.is_xai() && use_bearer_resolver {
                self.auth_manager
                    .as_ref()
                    .map(|am| -> xai_grok_sampler::SharedBearerResolver {
                        std::sync::Arc::new(AuthManagerBearerResolver(am.clone()))
                    })
            } else {
                None
            },
            supports_backend_search: self.supports_backend_search.get(),
            codex_multi_agent_v2,
            compactions_remaining: self.compactions_remaining.get(),
            compaction_at_tokens: self.compaction_at_tokens.get(),
            doom_loop_recovery: self.doom_loop_recovery,
            header_injector: Some(std::sync::Arc::new(TraceContextInjector)),
        }
    }
    /// Install auto-mode permission classifier with a live LLM side-query
    /// (laziness-classifier pattern: `prepare_chat_completion` +
    /// `conversation_collect` on a LocalSet task; channel bridges the
    /// `Send` permission actor). Heuristic runs only when the side-query
    /// errors or returns unparseable text.
    pub(crate) async fn wire_permission_auto_llm_classifier(self: &Arc<Self>) {
        if !self.permissions.is_auto_mode() {
            return;
        }
        if self.permissions.has_llm_side_query() {
            return;
        }
        let auto_cfg = crate::util::config::resolve_auto_mode_config_from_disk();
        let session_model = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let aux_classifier_sampler = match auto_cfg.classifier_model.as_deref() {
            Some(slug) => self.resolve_auto_classifier_sampler(slug).await,
            None => None,
        };
        let models = self.models_manager.models();
        let effective_supports_re = crate::agent::config::effective_classifier_supports_re(
            aux_classifier_sampler
                .as_ref()
                .map(|(_, model)| model.as_str()),
            &session_model,
            &models,
        );
        let (prompt_type, classifier_reasoning_effort) =
            crate::util::config::auto_mode_classifier_defaults(&auto_cfg, effective_supports_re);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(
            Vec<xai_grok_workspace::permission::ClassifierMessage>,
            tokio::sync::oneshot::Sender<Result<String, String>>,
        )>();
        let session = Arc::clone(self);
        tokio::task::spawn_local(async move {
            const TIMEOUT_MS: u64 = 15_000;
            while let Some((messages, respond_to)) = rx.recv().await {
                let result = async {
                    let (sampling_client, model) = match &aux_classifier_sampler {
                        Some((client, model)) => (client.clone(), model.clone()),
                        None => {
                            let client = session
                                .prepare_chat_completion(false)
                                .await
                                .map_err(|e| e.to_string())?;
                            let model = session
                                .chat_state_handle
                                .get_sampling_config()
                                .await
                                .map(|c| c.model)
                                .unwrap_or_default();
                            (client, model)
                        }
                    };
                    let session_id = session.session_info.id.to_string();
                    let items = messages
                        .into_iter()
                        .map(|m| match m.role {
                            xai_grok_workspace::permission::ClassifierMessageRole::System => {
                                ConversationItem::system(m.text)
                            }
                            xai_grok_workspace::permission::ClassifierMessageRole::User => {
                                ConversationItem::user(m.text)
                            }
                        })
                        .collect::<Vec<_>>();
                    let request = ConversationRequest {
                        items,
                        tools: vec![],
                        hosted_tools: vec![],
                        tool_choice: None,
                        model: Some(model),
                        temperature: None,
                        max_output_tokens: None,
                        json_schema: Some(
                            xai_grok_workspace::permission::classifier_output_json_schema(),
                        ),
                        reasoning_effort: classifier_reasoning_effort,
                        x_grok_conv_id: Some(session_id.clone()),
                        x_grok_req_id: Some(format!("xai-perm-auto-{}", uuid::Uuid::new_v4())),
                        x_grok_session_id: Some(session_id),
                        x_grok_agent_id: Some(xai_grok_telemetry::id::agent_id()),
                        ..ConversationRequest::default()
                    };
                    let fut = sampling_client.conversation_collect(request);
                    let response =
                        tokio::time::timeout(std::time::Duration::from_millis(TIMEOUT_MS), fut)
                            .await
                            .map_err(|_| "permission auto classifier timed out".to_string())?
                            .map_err(|e| e.to_string())?;
                    Ok(response.assistant_text())
                }
                .await;
                let _ = respond_to.send(result);
            }
        });
        let clf =
            xai_grok_workspace::permission::LlmPermissionClassifier::with_channel(tx, prompt_type);
        debug_assert!(
            clf.has_side_query(),
            "channel-wired classifier must report has_side_query"
        );
        self.permissions.set_classifier_with_side_query(clf, true);
        tracing::info!(
            session_id = % self.session_info.id,
            "Wired live LLM permission auto-mode classifier (session sampling channel)"
        );
    }
    /// Resolve a standalone aux-model `SamplerConfig` for `slug` via the shared
    /// catalog routing (Tier-1 catalog creds / Tier-2 xAI-proxy via session token
    /// / `XAI_API_KEY` / deployment key), gathering the session-local auth context
    /// once. Shared by image-describe and the classifier so the gather can't
    /// drift. `None` ⇒ caller falls back to the session model.
    pub(super) async fn resolve_aux_sampler_config(
        &self,
        slug: &str,
    ) -> Option<xai_grok_sampler::SamplerConfig> {
        let creds = self.chat_state_handle.get_credentials().await;
        let models = self.models_manager.models();
        let target_provider = crate::agent::config::find_model_by_id(&models, slug)
            .map(|entry| entry.info().provider);
        // Resolve and refresh xAI auth independently of the active chat
        // provider. This keeps a long-lived Codex session's explicit xAI
        // recap/memory helper alive without ever borrowing the Codex bearer.
        // A known Codex helper skips xAI refresh entirely.
        let xai_auth = (target_provider != Some(xai_grok_sampling_types::ModelProvider::Codex))
            .then(|| self.auth_manager.as_ref())
            .flatten()
            .and_then(|manager| manager.current_or_expired().map(|auth| (manager, auth.key)));
        let session_key = if let Some((manager, fallback)) = xai_auth {
            manager.get_valid_token().await.ok().or(Some(fallback))
        } else {
            None
        };
        let endpoints = self.models_manager.endpoints();
        let disable_api_key_auth = self
            .auth_manager
            .as_ref()
            .map(|am| am.grok_com_config().api_key_auth_disabled())
            .unwrap_or(false);
        let mut resolved = crate::agent::config::resolve_aux_model_sampling_config(
            slug,
            &models,
            &endpoints,
            session_key.as_deref(),
            disable_api_key_auth,
            creds.alpha_test_key.clone(),
            creds.client_version.clone(),
        )?;
        let uses_live_xai_credential = resolved.provider
            == xai_grok_sampling_types::ModelProvider::Xai
            && crate::util::is_first_party_xai_url(&resolved.base_url)
            && session_key
                .as_ref()
                .zip(resolved.api_key.as_ref())
                .is_some_and(|(live, resolved)| live == resolved);
        if uses_live_xai_credential && let Some(manager) = self.auth_manager.as_ref() {
            resolved.bearer_resolver = Some(std::sync::Arc::new(XaiAuxAuthManagerBearerResolver(
                manager.clone(),
            )));
        }
        Some(resolved)
    }

    /// Resolve a recap or memory model as an independent, provider-correct
    /// sampling route.
    ///
    /// Automatic selection never crosses providers: Codex sessions prefer
    /// `gpt-5.6-terra`, while xAI/BYOK sessions keep their active model. An
    /// explicit Settings/config choice may cross providers, but only when the
    /// selected catalog entry has credentials for its own provider.
    pub(super) async fn prepare_auxiliary_sampling(
        &self,
        purpose: AuxiliaryModelPurpose,
        call_override: Option<&str>,
    ) -> Result<PreparedAuxiliarySampling, acp::Error> {
        let active = self.reconstruct_full_config().await;
        let configured = call_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| match purpose {
                AuxiliaryModelPurpose::Recap => self.models_manager.recap_model(),
                AuxiliaryModelPurpose::Memory => self.models_manager.memory_model(),
            });
        let models = self.models_manager.models();

        let desired = configured
            .clone()
            .or_else(|| automatic_auxiliary_model(active.provider).map(str::to_owned));

        let mut selected = None;
        if let Some(slug) = desired.as_deref() {
            let intended_provider = crate::agent::config::find_model_by_id(&models, slug)
                .map(|entry| entry.info().provider);
            if let Some(intended_provider) = intended_provider {
                match self.resolve_aux_sampler_config(slug).await {
                    Some(mut cfg) if cfg.provider == intended_provider => {
                        crate::agent::config::stamp_session_local_sampler_fields(
                            &mut cfg,
                            &active,
                            self.client_identifier.clone(),
                            Some(self.max_retries),
                        );
                        // An automatic helper choice is provider-local by
                        // contract. Explicit choices are the only opt-in to
                        // sending recap/memory content to another provider.
                        if configured.is_some() || cfg.provider == active.provider {
                            selected = Some(cfg);
                        }
                    }
                    Some(cfg) => tracing::warn!(
                        auxiliary_purpose = ?purpose,
                        auxiliary_model = %slug,
                        intended_provider = ?intended_provider,
                        resolved_provider = ?cfg.provider,
                        "auxiliary model resolved through the wrong provider; using active model"
                    ),
                    None => tracing::warn!(
                        auxiliary_purpose = ?purpose,
                        auxiliary_model = %slug,
                        "auxiliary model has no usable provider credentials; using active model"
                    ),
                }
            } else {
                tracing::warn!(
                    auxiliary_purpose = ?purpose,
                    auxiliary_model = %slug,
                    "auxiliary model is not in the authenticated catalog; using active model"
                );
            }
        }

        let mut config = selected.unwrap_or(active);
        let supports_reasoning_effort =
            crate::agent::config::find_model_by_id(&models, &config.model)
                .is_some_and(|entry| entry.info().supports_reasoning_effort);
        let reasoning_effort =
            auxiliary_reasoning_effort(config.provider, supports_reasoning_effort).filter(
                |effort| {
                    self.models_manager
                        .model_accepts_reasoning_effort(&config.model, *effort)
                },
            );
        config.reasoning_effort = reasoning_effort;

        let model = config.model.clone();
        let context_window = config.context_window;
        let provider = config.provider;
        let client = xai_grok_sampler::SamplingClient::new(config)
            .map_err(|error| self.to_acp_error(error))?;
        tracing::info!(
            auxiliary_purpose = ?purpose,
            auxiliary_model = %model,
            auxiliary_provider = ?provider,
            auxiliary_reasoning_effort = ?reasoning_effort,
            explicit = configured.is_some(),
            "prepared auxiliary sampling route"
        );
        Ok(PreparedAuxiliarySampling {
            client,
            model,
            context_window,
            reasoning_effort,
            provider,
        })
    }
    /// Resolve a dedicated sampler for the Auto-mode classifier model `slug`,
    /// stamping session-local auth/attribution like image-describe (which relies
    /// on the resolver, not a config override, for `base_url`/`api_backend` so
    /// credentials stay consistent). `None` ⇒ caller falls back to the session
    /// client + model.
    async fn resolve_auto_classifier_sampler(
        &self,
        slug: &str,
    ) -> Option<(xai_grok_sampler::SamplingClient, String)> {
        let active_session_config = self.reconstruct_full_config().await;
        let mut cfg = self.resolve_aux_sampler_config(slug).await?;
        crate::agent::config::stamp_session_local_sampler_fields(
            &mut cfg,
            &active_session_config,
            self.client_identifier.clone(),
            Some(self.max_retries),
        );
        let model = cfg.model.clone();
        let client = xai_grok_sampler::SamplingClient::new(cfg)
            .map_err(|e| {
                tracing::warn!(
                    error = % e,
                    "auto classifier aux sampler build failed; using session model"
                )
            })
            .ok()?;
        Some((client, model))
    }
    #[tracing::instrument(
        name = "session.prepare_chat_completion",
        skip_all,
        fields(force_http1)
    )]
    pub(super) async fn prepare_chat_completion(
        &self,
        force_http1: bool,
    ) -> Result<xai_grok_sampler::SamplingClient, acp::Error> {
        self.refresh_token_if_expired().await;
        let mut full_config = self.reconstruct_full_config().await;
        full_config.force_http1 = force_http1;
        let sampling_client =
            xai_grok_sampler::SamplingClient::new(full_config).map_err(|e| self.to_acp_error(e))?;
        Ok(sampling_client)
    }

    /// Build a direct client that participates in the active logical Codex
    /// turn. This is reserved for the remote compact endpoint; auxiliary
    /// sampling clients deliberately remain detached from main-turn sticky
    /// routing state.
    pub(super) async fn prepare_chat_completion_with_codex_turn_state(
        &self,
        force_http1: bool,
        codex_turn_state: std::sync::Arc<std::sync::OnceLock<String>>,
    ) -> Result<xai_grok_sampler::SamplingClient, acp::Error> {
        self.refresh_token_if_expired().await;
        let mut full_config = self.reconstruct_full_config().await;
        full_config.force_http1 = force_http1;
        xai_grok_sampler::SamplingClient::new_with_codex_turn_state(full_config, codex_turn_state)
            .map_err(|e| self.to_acp_error(e))
    }
    /// Push a fresh `SamplerConfig` into the per-session sampler actor
    /// before each turn. Mirrors `prepare_chat_completion`'s
    /// auth-refresh + config rebuild, but routes the result to the
    /// `xai-grok-sampler` instead of constructing a new
    /// `OaiCompatClient`.
    ///
    /// Behaviour parity: we run the same `refresh_token_if_expired()`
    /// and `reconstruct_full_config()` so the sampler picks up any
    /// newly issued session token. The previous client cache inside
    /// the sampler actor is invalidated automatically by
    /// `update_config`.
    pub(crate) async fn prepare_sampler_for_turn(&self) {
        self.refresh_token_if_expired().await;
        let mut sampler_config = self.reconstruct_full_config().await;
        sampler_config.idle_timeout_secs = Some(self.inference_idle_timeout.as_secs());
        self.sampler_handle.update_config(sampler_config);
    }
    fn log_terminal_failure(
        &self,
        error_type: &str,
        status_code: Option<u16>,
        message: &str,
        provider: xai_grok_sampling_types::ModelProvider,
    ) {
        let auth = provider
            .profile()
            .session_auth
            .is_xai()
            .then(|| {
                self.auth_manager
                    .as_ref()
                    .and_then(|am| am.current_or_expired())
            })
            .flatten();
        let reauthable = is_reauthable_failure(Some(error_type), message);
        xai_grok_telemetry::unified_log::warn(
            "turn.terminal_failure",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!(
                { "error_type" : error_type, "status_code" : status_code,
                "provider" : format!("{provider:?}"),
                "reauthable" : reauthable, "auth_mode" : auth.as_ref().map(| a |
                format!("{:?}", a.auth_mode)), "key_prefix" : auth.as_ref().map(| a |
                crate ::auth::token_suffix(& a.key).to_owned()), "expires_at" : auth
                .as_ref().and_then(| a | a.expires_at.map(| e | e.to_rfc3339())),
                "message" : crate ::util::truncate(message, 300), }
            )),
        );
    }
    pub(crate) async fn handle_sampling_failure(
        self: &Arc<Self>,
        error: xai_grok_sampler::SamplingErrorInfo,
    ) -> Result<SamplerFailureRecovery, acp::Error> {
        self.handle_sampling_failure_with_policy(error, true).await
    }

    pub(super) async fn handle_sampling_failure_with_policy(
        self: &Arc<Self>,
        error: xai_grok_sampler::SamplingErrorInfo,
        codex_auth_refresh_allowed: bool,
    ) -> Result<SamplerFailureRecovery, acp::Error> {
        use xai_grok_sampler::SamplingErrorKind;
        let request_config = self.chat_state_handle.get_sampling_config().await;
        let request_provider = request_config
            .as_ref()
            .map(|config| config.provider)
            .unwrap_or_default();
        let request_auth_type = self.chat_state_handle.get_credentials().await.auth_type;
        if self.should_compact_on_error(&error).await {
            let cw = error
                .model_metadata
                .as_ref()
                .and_then(|m| m.context_window)
                .expect("should_compact_on_error guarantees context_window");
            {
                let total_tokens = self.chat_state_handle.get_estimated_total_tokens().await;
                let percentage = xai_token_estimation::usage_percentage_u8(total_tokens, cw);
                if let Some(mut cfg) = self.chat_state_handle.get_sampling_config().await
                    && let Some(new_cw) = std::num::NonZeroU64::new(cw)
                    && self.compaction.context_window_override.is_none()
                {
                    cfg.context_window = new_cw;
                    self.chat_state_handle.update_sampling_config(cfg);
                }
                let trigger_info = compaction::AutoCompactTriggerInfo {
                    tokens_used: total_tokens,
                    context_window: cw,
                    percentage,
                };
                self.run_compact_only(trigger_info).await?;
                return Ok(SamplerFailureRecovery::CompactAndResubmit);
            }
        }
        let detailed_message = error.message.clone();
        if matches!(error.kind, SamplingErrorKind::Api)
            && error.status_code == Some(400)
            && error.message.contains("encrypted_content")
        {
            self.signals_handle()
                .record_error_typed("encrypted_content_mismatch");
            let friendly = "This session's conversation history is incompatible \
                            with the current model. Please start a new session."
                .to_string();
            self.log_terminal_failure(
                "encrypted_content_mismatch",
                error.status_code,
                &friendly,
                request_provider,
            );
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "encrypted_content_mismatch".to_string(),
                    message: friendly.clone(),
                },
            ))
            .await;
            return Err(acp::Error::invalid_params().data(friendly));
        }
        if matches!(error.kind, SamplingErrorKind::RateLimited) {
            self.log_terminal_failure(
                "rate_limited",
                error.status_code,
                &detailed_message,
                request_provider,
            );
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Exhausted {
                    attempts: 0,
                    reason: detailed_message.clone(),
                    is_rate_limited: true,
                },
            ))
            .await;
            let acp_err = acp::Error::new(
                crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                "Rate limited".to_string(),
            )
            .data(detailed_message);
            return Err(acp_err);
        }
        let auth_recovery_eligible = matches!(error.kind, SamplingErrorKind::Auth)
            && match request_provider {
                xai_grok_sampling_types::ModelProvider::Codex => {
                    let eligible = request_auth_type == xai_chat_state::AuthType::SessionToken
                        && codex_auth_refresh_allowed;
                    if !eligible {
                        tracing::warn!(
                            session_id = % self.session_info.id.0,
                            auth_type = ?request_auth_type,
                            codex_auth_refresh_allowed,
                            "Codex auth recovery unavailable; surfacing 401",
                        );
                    }
                    eligible
                }
                xai_grok_sampling_types::ModelProvider::Xai => {
                    let (model_id, base_url) = request_config
                        .as_ref()
                        .map(|config| (config.model.as_str(), config.base_url.as_str()))
                        .unwrap_or_default();
                    let gate = self.auth_gate(model_id, base_url);
                    let eligible = gate.active();
                    self.log_auth_gate_unknown("handle_sampling_failure", gate, base_url);
                    if !eligible {
                        tracing::warn!(
                            session_id = % self.session_info.id.0, is_session_based = gate
                            .is_session_based, model_byok = gate.model_byok.as_str(),
                            endpoint_is_first_party = gate.endpoint_is_first_party,
                            "auth recovery: sampler 401 not refreshable (api-key auth) — surfacing 401",
                        );
                        xai_grok_telemetry::unified_log::warn(
                            "auth recovery: sampler 401 not eligible (api-key auth)",
                            Some(self.session_info.id.0.as_ref()),
                            Some(serde_json::json!(
                                { "kind" : error.kind.as_str(), "status_code" : error
                                .status_code, "is_session_based" : gate.is_session_based,
                                "model_byok" : gate.model_byok.as_str(),
                                "endpoint_is_first_party" : gate.endpoint_is_first_party, }
                            )),
                        );
                    }
                    eligible
                }
            };
        if !matches!(error.kind, SamplingErrorKind::Auth) && error.status_code == Some(401) {
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401 not eligible (non-auth error kind)",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!(
                    { "kind" : error.kind.as_str(), "status_code" : error
                    .status_code, }
                )),
            );
        }
        if auth_recovery_eligible
            && request_provider == xai_grok_sampling_types::ModelProvider::Codex
        {
            match crate::codex_auth::force_refresh().await {
                Ok(Some(credentials)) => {
                    if self.update_codex_chat_credentials(Some(credentials)).await {
                        self.prepare_sampler_for_turn().await;
                        return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                            provider: request_provider,
                        });
                    }
                    tracing::warn!(
                        session_id = % self.session_info.id.0,
                        "Codex auth recovery rejected an account identity change"
                    );
                }
                Ok(None) => {
                    tracing::warn!(
                        session_id = % self.session_info.id.0,
                        "Codex auth recovery failed: no isolated Codex account"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        session_id = % self.session_info.id.0,
                        %error,
                        "Codex auth recovery failed"
                    );
                }
            }
        }
        if auth_recovery_eligible
            && request_provider == xai_grok_sampling_types::ModelProvider::Xai
            && crate::auth::devbox_login::is_devbox_environment()
            && let Some(ref am) = self.auth_manager
        {
            match am.try_devbox_recovery().await {
                Ok(auth) => {
                    tracing::info!(
                        session_id = % self.session_info.id.0, user_id = % auth.user_id,
                        "auth recovery: sampler 401, devbox re-mint, retrying"
                    );
                    self.prepare_sampler_for_turn().await;
                    return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                        provider: request_provider,
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = % self.session_info.id.0, error = % e,
                        "auth recovery: sampler 401, devbox re-mint failed"
                    );
                    xai_grok_telemetry::unified_log::warn(
                        "auth recovery: sampler 401, devbox re-mint failed",
                        Some(self.session_info.id.0.as_ref()),
                        Some(serde_json::json!({ "error" : format!("{e}") })),
                    );
                }
            }
        }
        if auth_recovery_eligible
            && request_provider == xai_grok_sampling_types::ModelProvider::Xai
            && let Some(ref am) = self.auth_manager
        {
            if am
                .try_recover_unauthorized(crate::auth::recovery::RecoverySource::Turn)
                .await
            {
                tracing::info!(
                    session_id = % self.session_info.id.0,
                    "auth recovery: sampler 401, recovered, retrying"
                );
                xai_grok_telemetry::unified_log::info(
                    "auth recovery: sampler 401, recovered, retrying",
                    Some(self.session_info.id.0.as_ref()),
                    None,
                );
                self.prepare_sampler_for_turn().await;
                return Ok(SamplerFailureRecovery::RefreshAuthAndResubmit {
                    provider: request_provider,
                });
            }
            tracing::warn!(
                session_id = % self.session_info.id.0,
                "auth recovery: sampler 401, refresh failed"
            );
            xai_grok_telemetry::unified_log::warn(
                "auth recovery: sampler 401, refresh failed",
                Some(self.session_info.id.0.as_ref()),
                None,
            );
        }
        if matches!(error.kind, SamplingErrorKind::IdleTimeout) {
            self.signals_handle().record_idle_timeout();
        }
        if matches!(error.kind, SamplingErrorKind::EmptyResponse) {
            if let Some(ref ctx) = error.empty_response_context {
                tracing::warn!(
                    empty_response = true, empty_reason = ctx.reason.as_str(),
                    had_reasoning = ctx.had_reasoning, content_len = ctx.content_len,
                    tool_call_count = ctx.tool_call_count, completion_tokens = ctx
                    .completion_tokens.unwrap_or(0), reasoning_tokens = ctx
                    .reasoning_tokens.unwrap_or(0), finish_reason = ctx
                    .finish_reason_str(), first_choice_seen = ctx.first_choice_seen,
                    model = % ctx.model,
                    "empty response after retries exhausted: {reason}", reason = ctx
                    .reason,
                );
                {
                    let mut cap = self.streaming_turn_capture.lock();
                    cap.reasoning_tokens = ctx.reasoning_tokens;
                    cap.completion_tokens = ctx.completion_tokens;
                    cap.finish_reason = ctx.finish_reason.clone();
                    cap.empty_reason = Some(ctx.reason.as_str().to_owned());
                }
            }
            self.signals_handle().record_error_typed("empty_response");
        }
        let auth_mode = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current())
            .map(|a| a.auth_mode)
            .unwrap_or(crate::auth::AuthMode::ApiKey);
        let auth_mode_str = if request_provider == xai_grok_sampling_types::ModelProvider::Codex {
            match request_auth_type {
                xai_chat_state::AuthType::SessionToken => "CodexOAuth".to_owned(),
                xai_chat_state::AuthType::ApiKey => "ApiKey".to_owned(),
            }
        } else {
            format!("{auth_mode:?}")
        };
        let client_version = xai_grok_version::VERSION;
        if request_provider == xai_grok_sampling_types::ModelProvider::Xai
            && auth_mode == crate::auth::AuthMode::WebLogin
        {
            let msg = format!(
                "{detailed_message}\n\n\
                 You are using a deprecated authentication method (WebLogin).\n\
                 This auth method is no longer supported and will cause errors.\n\n\
                 To fix: run `open-grok logout` then `open-grok login` to re-authenticate with OAuth2.\n\n\
                 Version: {client_version}"
            );
            self.log_terminal_failure("legacy_auth", error.status_code, &msg, request_provider);
            self.send_xai_notification(XaiSessionUpdate::RetryState(
                crate::extensions::notification::RetryState::Failed {
                    error_type: "legacy_auth".to_string(),
                    message: msg.clone(),
                },
            ))
            .await;
            return Err(acp::Error::internal_error().data(msg));
        }
        let is_model_404 =
            error.status_code == Some(404) && detailed_message.contains("does not exist");
        let is_auth_401 =
            error.status_code == Some(401) || matches!(error.kind, SamplingErrorKind::Auth);
        let detailed_message = if is_model_404 || is_auth_401 {
            let current_model = self
                .chat_state_handle
                .get_sampling_config()
                .await
                .map(|c| c.model)
                .unwrap_or_else(|| "unknown".to_string());
            let available: Vec<String> = self
                .models_manager
                .models()
                .values()
                .map(|m| m.model.clone())
                .collect();
            let mut msg = format!("{detailed_message}\n");
            msg.push_str(&format!("\n  Model:     {current_model}"));
            msg.push_str(&format!("\n  Auth:      {auth_mode_str}"));
            msg.push_str(&format!("\n  Version:   {client_version}"));
            if available.is_empty() {
                msg.push_str("\n  Available: (none)");
            } else {
                msg.push_str(&format!("\n  Available: {}", available.join(", ")));
            }
            if is_model_404 && !available.iter().any(|m| m == &current_model) {
                msg.push_str(&format!(
                    "\n\n  '{}' is not in your available models.",
                    current_model
                ));
                msg.push_str("\n  Switch models with /model or start a new session.");
            }
            if is_auth_401
                && request_provider == xai_grok_sampling_types::ModelProvider::Codex
                && request_auth_type == xai_chat_state::AuthType::SessionToken
            {
                msg.push_str("\n\n  Reconnect with `open-grok login --codex`.");
            }
            msg
        } else {
            detailed_message
        };
        let error_type = if xai_grok_sampling_types::is_context_length_error(&error.message) {
            "context_length"
        } else {
            error.kind.as_str()
        };
        self.log_terminal_failure(
            error_type,
            error.status_code,
            &detailed_message,
            request_provider,
        );
        self.send_xai_notification(XaiSessionUpdate::RetryState(
            crate::extensions::notification::RetryState::Failed {
                error_type: error_type.to_string(),
                message: detailed_message.clone(),
            },
        ))
        .await;
        Err(
            acp::Error::internal_error().data(crate::sampling::error::terminal_error_data(
                detailed_message,
                error.status_code,
                error.kind,
            )),
        )
    }
    /// Drive a single turn through the sampler-based path.
    ///
    /// Calls `prepare_sampler_for_turn` first (auth refresh + config
    /// push), then submits via `SamplerHandle::submit_and_collect` and
    /// returns:
    /// * `Ok(SamplerTurnOutcome::Response(_))` - model responded.
    /// * `Ok(SamplerTurnOutcome::CompactAndResubmit)` - compaction
    ///    ran, the outer turn loop should `continue`.
    /// * `Ok(SamplerTurnOutcome::RefreshAuthAndResubmit)` - auth 401
    ///    recovery succeeded, credentials refreshed, retry once.
    /// * `Err(acp::Error)` - terminal failure already reported via
    ///    `send_xai_notification(RetryState::Failed)`.
    pub(crate) async fn run_turn_via_sampler(
        self: &Arc<Self>,
        request: ConversationRequest,
        codex_auth_refresh_allowed: bool,
    ) -> Result<SamplerTurnOutcome, acp::Error> {
        self.prepare_sampler_for_turn().await;
        let stream_drained_rx = {
            let (tx, rx) = tokio::sync::oneshot::channel();
            *self.turn_stream_drained.lock() = Some(tx);
            rx
        };
        let request_id = xai_grok_sampler::RequestId::random();
        let request_id_str = request_id.as_str().to_string();
        match self
            .sampler_handle
            .submit_and_collect(request_id, request)
            .await
        {
            Ok((response, metrics)) => {
                let span = tracing::Span::current();
                span.record("request_id", request_id_str.as_str());
                if let Some(ttft) = metrics.time_to_first_token_ms {
                    span.record("ttft_ms", ttft as i64);
                }
                if metrics.attempts > 0 {
                    span.record("attempt", i64::from(metrics.attempts));
                }
                if tokio::time::timeout(std::time::Duration::from_secs(5), stream_drained_rx)
                    .await
                    .is_err()
                {
                    self.turn_stream_drained.lock().take();
                    tracing::warn!(
                        "stream-drain barrier timed out; proceeding to emit tool \
                         calls (eventId ordering may be imperfect this turn)"
                    );
                }
                Ok(SamplerTurnOutcome::Response(
                    Box::new(response),
                    Box::new(metrics),
                ))
            }
            Err(rich_err) => {
                self.turn_stream_drained.lock().take();
                let info = xai_grok_sampler::SamplingErrorInfo::from(&rich_err);
                match self
                    .handle_sampling_failure_with_policy(info, codex_auth_refresh_allowed)
                    .await?
                {
                    SamplerFailureRecovery::CompactAndResubmit => {
                        Ok(SamplerTurnOutcome::CompactAndResubmit)
                    }
                    SamplerFailureRecovery::RefreshAuthAndResubmit { provider } => {
                        Ok(SamplerTurnOutcome::RefreshAuthAndResubmit { provider })
                    }
                }
            }
        }
    }
    pub(super) async fn update_codex_chat_credentials(
        &self,
        credentials: Option<crate::codex_auth::CodexCredentials>,
    ) -> bool {
        let mut adopted_identity = false;
        if let Some(credentials) = credentials.as_ref() {
            let Some(mut sampling_config) = self.chat_state_handle.get_sampling_config().await
            else {
                return false;
            };
            match crate::codex_auth::credentials_match_identity_anchor(
                &sampling_config.extra_headers,
                credentials,
            ) {
                Some(true) => {}
                Some(false) => {
                    let mut chat_credentials = self.chat_state_handle.get_credentials().await;
                    chat_credentials.api_key = None;
                    chat_credentials.auth_type = xai_chat_state::AuthType::SessionToken;
                    self.chat_state_handle.update_credentials(chat_credentials);
                    return false;
                }
                None => {
                    // A session started before Codex login adopts the first
                    // authenticated identity exactly once. Later account or
                    // workspace changes fail closed against this anchor.
                    crate::codex_auth::set_oauth_identity_anchor(
                        &mut sampling_config.extra_headers,
                        Some(credentials),
                    );
                    self.chat_state_handle
                        .update_sampling_config(sampling_config);
                    adopted_identity = true;
                }
            }
        }
        let mut chat_credentials = self.chat_state_handle.get_credentials().await;
        let has_access_token = credentials.is_some();
        let access_token = credentials
            .as_ref()
            .map(|credentials| credentials.access_token.clone());
        if chat_credentials.api_key != access_token
            || chat_credentials.auth_type != xai_chat_state::AuthType::SessionToken
        {
            chat_credentials.api_key = access_token;
            chat_credentials.auth_type = xai_chat_state::AuthType::SessionToken;
            self.chat_state_handle.update_credentials(chat_credentials);
        }
        if has_access_token && adopted_identity {
            let bearer_resolver = std::sync::Arc::new(
                crate::codex_auth::CodexBearerResolver::from_credentials(credentials.as_ref()),
            ) as xai_grok_sampler::SharedBearerResolver;
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::RefreshCodexSummaryAuth { bearer_resolver });
        }
        has_access_token
    }
    /// Proactively refresh the auth token if near expiry.
    pub(super) async fn refresh_token_if_expired(&self) {
        let mut creds = self.chat_state_handle.get_credentials().await;
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let provider = sampling_config
            .as_ref()
            .map(|config| config.provider)
            .unwrap_or_default();

        match turn_auth_refresh_route(provider, creds.auth_type) {
            TurnAuthRefreshRoute::CodexOAuth => {
                let credentials = match crate::codex_auth::fresh_credentials().await {
                    Ok(credentials) => credentials,
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            "Codex OAuth preflight refresh failed; using isolated cached token"
                        );
                        crate::codex_auth::load_credentials().ok().flatten()
                    }
                };
                // `None` is intentional after Codex logout or an unreadable
                // auth store: it prevents the sampler's static Authorization
                // fallback from retaining any prior-provider token.
                self.update_codex_chat_credentials(credentials).await;
                return;
            }
            TurnAuthRefreshRoute::XaiSession => {
                let Some(ref am) = self.auth_manager else {
                    xai_grok_telemetry::unified_log::debug(
                        "xAI token refresh skipped: no auth manager",
                        Some(self.session_info.id.0.as_ref()),
                        None,
                    );
                    return;
                };
                let (model_id, base_url) = sampling_config
                    .as_ref()
                    .map(|config| (config.model.as_str(), config.base_url.as_str()))
                    .unwrap_or_default();
                if self.auth_gate(model_id, base_url).active()
                    && let Ok(key) = am.get_valid_token().await
                {
                    if creds.api_key.as_deref() != Some(&key) {
                        creds.api_key = Some(key);
                        self.chat_state_handle.update_credentials(creds);
                    }
                    return;
                }
            }
            TurnAuthRefreshRoute::ConfigApiKey => {}
        }
        use crate::auth::{is_jwt_expired_or_near, parse_jwt_expiration};
        const REFRESH_THRESHOLD: chrono::Duration = chrono::Duration::minutes(5);
        let creds = self.chat_state_handle.get_credentials().await;
        let current_key = creds.api_key;
        let current_model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();
        let Some(ref key) = current_key else { return };
        if !is_jwt_expired_or_near(key, REFRESH_THRESHOLD) {
            if let Some(exp) = parse_jwt_expiration(key) {
                let remaining_secs = (exp - chrono::Utc::now()).num_seconds();
                tracing::debug!(
                    model = % current_model_id, remaining_secs,
                    "JWT token valid, no refresh needed"
                );
            } else {
                tracing::debug!(
                    model = % current_model_id, key_len = key.len(),
                    "Token is not a JWT, expiry-based refresh not applicable"
                );
            }
            return;
        }
        let remaining_secs =
            parse_jwt_expiration(key).map_or(0, |exp| (exp - chrono::Utc::now()).num_seconds());
        tracing::info!(
            model = % current_model_id, remaining_secs,
            "JWT near expiry, refreshing from config.toml"
        );
        let Some(new_key) = self.reload_api_key_from_config(&current_model_id) else {
            return;
        };
        if key == &new_key {
            tracing::warn!(
                model = % current_model_id,
                "Config.toml returned same token (not yet rotated by external process?)"
            );
            return;
        }
        let new_remaining_secs = parse_jwt_expiration(&new_key)
            .map_or(0, |exp| (exp - chrono::Utc::now()).num_seconds());
        tracing::info!(
            model = % current_model_id, new_remaining_secs, key_len = new_key.len(),
            "Refreshed API token from config.toml"
        );
        let mut creds = self.chat_state_handle.get_credentials().await;
        creds.api_key = Some(new_key);
        self.chat_state_handle.update_credentials(creds);
    }
    fn reload_api_key_from_config(&self, current_model_id: &str) -> Option<String> {
        let raw_config = crate::config::load_effective_config()
            .map_err(|e| tracing::warn!(error = % e, "Failed to reload config"))
            .ok()?;
        let config = crate::agent::config::Config::new_from_toml_cfg(&raw_config)
            .map_err(|e| tracing::warn!(error = % e, "Failed to parse reloaded config.toml"))
            .ok()?;
        let config_model = config
            .config_models
            .iter()
            .find(|(k, v)| v.model.as_deref().unwrap_or(k.as_str()) == current_model_id)
            .map(|(_, v)| v);
        let Some(model) = config_model else {
            tracing::warn!(
                model = % current_model_id, available = ? config.config_models.keys()
                .collect::< Vec < _ >> (), "Model not found in config.toml [model.*]"
            );
            return None;
        };
        let key = crate::agent::config::first_own_credential(
            model.api_key.as_deref(),
            model.env_key.as_ref(),
        );
        if key.is_none() {
            tracing::warn!(
                model = % current_model_id, env_key = ? model.env_key,
                "No api_key or env_key resolved for model"
            );
        }
        key
    }
    /// Propagate the model-reported token usage from a turn response into
    /// chat state, the per-prompt usage ledger, and per-turn signals.
    ///
    /// This is the only place per-turn `total_tokens` is refreshed in the
    /// post-sampler-refactor path; without it `state.total_tokens` would
    /// stay frozen at the `estimate_conversation_tokens` seed from
    /// `ChatState::new`, freezing `/context` and corrupting the resume
    /// restore that reads `meta.totalTokens` from `updates.jsonl`.
    /// Resetting `estimated_tokens_since_model = 0` here also keeps the
    /// preflight-overflow guard accurate against the next turn's
    /// tool-result deltas.
    pub(crate) fn record_response_token_usage(
        &self,
        response: &ConversationResponse,
        api_duration_ms: Option<u64>,
    ) {
        if let Some(ref u) = response.usage {
            self.chat_state_handle
                .record_token_usage(u64::from(u.total_tokens));
            self.chat_state_handle.record_last_turn_usage(u.clone());
            self.chat_state_handle.record_model_call_usage(
                response.assistant().and_then(|a| a.model_id.clone()),
                u.clone(),
                api_duration_ms,
                response.cost_usd_ticks,
            );
            self.signals_handle()
                .record_token_usage(u.completion_tokens, u.reasoning_tokens);
        }
    }
    pub(super) async fn record_assistant_response(&self, assistant_item: ConversationItem) {
        self.signals_handle().record_assistant_message();
        if let ConversationItem::Assistant(ref a) = assistant_item {
            tracing::info!(
                model_id = ? a.model_id, "DEBUG record_assistant_response model_id"
            );
        }
        if let ConversationItem::Assistant(ref a) = assistant_item
            && let Some(first_call) = a.tool_calls.first()
        {
            tracing::info!("Assistant requested tool call: {}", first_call.id);
        }
        self.chat_state_handle
            .push_assistant_response(assistant_item);
    }
}

#[cfg(test)]
mod auxiliary_model_policy_tests {
    use super::*;
    use xai_grok_sampling_types::{ModelProvider, ReasoningEffort};

    #[test]
    fn automatic_helpers_stay_provider_local() {
        assert_eq!(
            automatic_auxiliary_model(ModelProvider::Codex),
            Some("gpt-5.6-terra")
        );
        assert_eq!(automatic_auxiliary_model(ModelProvider::Xai), None);
    }

    #[test]
    fn auxiliary_reasoning_is_cheap_and_capability_gated() {
        assert_eq!(
            auxiliary_reasoning_effort(ModelProvider::Codex, true),
            Some(ReasoningEffort::Medium)
        );
        assert_eq!(
            auxiliary_reasoning_effort(ModelProvider::Xai, true),
            Some(ReasoningEffort::Low)
        );
        assert_eq!(
            auxiliary_reasoning_effort(ModelProvider::Codex, false),
            None
        );
        assert_eq!(auxiliary_reasoning_effort(ModelProvider::Xai, false), None);
    }
}
