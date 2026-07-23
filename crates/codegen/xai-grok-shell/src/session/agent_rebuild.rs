//! `AgentRebuildSpec` — the canonical recipe for constructing an
//! [`xai_grok_agent::Agent`] for a given session.
//!
//! INVARIANT: This is the **only** place in the shell crate that calls
//! [`xai_grok_agent::AgentBuilder::new`]. Both initial session spawn
//! ([`crate::session::acp_session::spawn_session_actor`]) and zero-turn
//! harness rebuild
//! ([`crate::session::acp_session::SessionActor::handle_rebuild_agent_for_definition`])
//! go through [`AgentRebuildSpec::build_agent`].
//!
//! ## Why this exists
//!
//! [`xai_grok_agent::Agent`] owns an [`xai_grok_tools::bridge::ToolBridge`]
//! that carries session-scoped channels (notification handle, terminal/fs
//! backends, subagent senders, scheduler set, plugin registry, attribution
//! callback). The Agent is therefore session-bound — it cannot be shared
//! across sessions and cannot be re-rendered from outside its session
//! context. To rebuild it (e.g. when the user picks a model with a
//! different `agent_type` before sending any user message), we need to
//! retain every input that the original `AgentBuilder` chain consumed.
//! `AgentRebuildSpec` is exactly that retained bag of inputs.
//!
//! ## WHEN ADDING A NEW [`xai_grok_agent::AgentBuilder`]`::with_*` KNOB
//!
//! 1. Add the corresponding field to [`AgentRebuildSpec`].
//! 2. Pass it through in [`AgentRebuildSpec::build_agent`]. The destructure
//!    pattern at the top of `build_agent` forces every field to be used —
//!    drift is a compile error (`#[deny(unused_variables)]`).
//! 3. Populate the field at the call site in `spawn_session_actor`.
//!
//! ## Why some fields are channel senders
//!
//! Several `ToolBridge` resources (e.g. `UserQuestionSender`,
//! `SubagentBackendResource`) are backed by the `tx` half of channels
//! whose `rx` halves are owned by long-lived coordinator tasks spawned
//! in `spawn_session_actor`. The subagent channels are wrapped in a
//! `ChannelBackend` behind `SubagentBackendResource`. On rebuild, we
//! must reuse the **same** senders so the existing coordinator keeps
//! receiving requests; we cannot mint a fresh channel without orphaning
//! the running coordinator.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;
use xai_grok_agent::config::AgentDefinition;
use xai_grok_agent::error::AgentBuildError;
use xai_grok_agent::prompt::context::PromptAudience;
use xai_grok_agent::prompt::skills::SkillsConfig;
use xai_grok_agent::{Agent, AgentBuilder, CompactionPolicy, ReminderPolicy};
use xai_grok_tools::computer::types::{AsyncFileSystem, TerminalBackend};
use xai_grok_tools::implementations::grok_build::ask_user_question::types::UserQuestionRequest;
use xai_grok_tools::implementations::grok_build::deploy_app::AppBuilderDeployerConfig;
use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
use xai_grok_tools::implementations::grok_build::task::types::{
    MonitorEventBuffer, SubagentEvent, TaskModelValidator,
};
use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;
use xai_grok_tools::implementations::grok_build::web_fetch::WebFetchConfig;
use xai_grok_tools::implementations::lsp::LspBackend;
use xai_grok_tools::implementations::web_search::WebSearchConfig;
use xai_grok_tools::notification::ToolNotificationHandle;
use xai_grok_tools::types::SharedApiKeyProvider;
use xai_grok_tools::types::compat::CompatConfig;
use xai_grok_tools::types::memory_backend::MemoryBackend;
/// Shell-resolved per-tool `ToolConfig.params` JSON maps, bundled into one
/// named struct so the spawn telescopes carry a single argument instead of
/// adjacent identically-typed positionals that a caller could transpose.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResolvedToolParamsJson {
    /// `[toolset.bash]` overrides for the bash tool(s).
    pub bash: Option<serde_json::Map<String, serde_json::Value>>,
    /// `[toolset.ask_user_question]` timeout policy for the ask tool.
    pub ask_user_question: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Clone)]
pub struct ResolvedWebSearchState {
    /// The client `web_search` backend registered for the session's CURRENT
    /// provider. Re-resolved via [`Self::resolve_active`] at the seams where
    /// the provider is known: session spawn, model switch, toolset reload.
    pub config: WebSearchConfig,
    /// Candidate backends plus the persisted per-provider source selection,
    /// so a provider change can re-resolve locally.
    pub candidates: crate::tools::config::WebSearchCandidates,
}

impl ResolvedWebSearchState {
    /// State whose active config is resolved for `provider`.
    pub(crate) fn resolved_for(
        candidates: crate::tools::config::WebSearchCandidates,
        provider: xai_grok_sampling_types::ModelProvider,
    ) -> Self {
        let config = candidates.resolved_config_for(provider);
        Self { config, candidates }
    }

    /// Re-resolve the active config for `provider` in place.
    pub(crate) fn resolve_active(&mut self, provider: xai_grok_sampling_types::ModelProvider) {
        self.config = self.candidates.resolved_config_for(provider);
    }

    /// Whether the client `web_search` tool is model-visible for `provider`.
    /// Resolved from the candidates so a mid-transition stale `config` can
    /// never leak a backend across a provider boundary.
    pub(crate) fn allowed_for_provider(
        &self,
        provider: xai_grok_sampling_types::ModelProvider,
    ) -> bool {
        !matches!(
            self.candidates.resolved_config_for(provider),
            WebSearchConfig::Disabled
        )
    }

    /// Whether the local search replaces the provider's native hosted
    /// declaration (Codex with a non-native source that resolved).
    pub(crate) fn native_hosted_web_search_suppressed(
        &self,
        provider: xai_grok_sampling_types::ModelProvider,
    ) -> bool {
        self.candidates
            .native_hosted_web_search_suppressed(provider)
    }
}
/// Cached recipe for building a session-scoped [`Agent`].
///
/// See module docs for the invariant: this is the only construction
/// site for `Agent` in the shell crate. Cloning is intentionally not
/// derived — the spec lives behind an [`Arc`] and is shared by clone of
/// that `Arc`.
pub(crate) struct AgentRebuildSpec {
    pub working_directory: PathBuf,
    pub terminal_backend: Arc<dyn TerminalBackend>,
    pub fs_backend: Arc<dyn AsyncFileSystem>,
    pub tools_notification_handle: ToolNotificationHandle,
    pub bridge_state_path: PathBuf,
    pub session_env: Arc<HashMap<String, String>>,
    pub models_manager: crate::agent::models::ModelsManager,
    pub compaction_policy: CompactionPolicy,
    pub reminder_policy: ReminderPolicy,
    /// Restart-scoped Settings preference. The active model's provider,
    /// backend, and metadata are applied by the spawn/model-switch paths.
    pub tool_mode_preference: Option<crate::agent::config::ToolModePreference>,
    /// Session-owned embedded runtime. It lives on the rebuild spec so Agent
    /// harness rebuilds and model switches cannot discard persistent JS state.
    pub code_mode_runtime: Arc<crate::session::code_mode::CodeModeRuntimeSlot>,
    pub memory_enabled: bool,
    pub memory_global_path: Option<String>,
    pub memory_workspace_path: Option<String>,
    pub memory_backend: Option<Arc<dyn MemoryBackend>>,
    pub web_search: parking_lot::RwLock<ResolvedWebSearchState>,
    /// `[toolset.x_search].enabled` at spawn. The client x_search tool is
    /// registered when this is set AND the xAI candidate resolved (signed
    /// in); per-turn provider filtering keeps it off xAI requests, which use
    /// the hosted declaration instead.
    pub x_search_enabled: bool,
    pub backend_search: bool,
    pub web_fetch_config: WebFetchConfig,
    pub image_gen_config: ImageGenConfig,
    pub video_gen_config: VideoGenConfig,
    pub app_builder_deployer_config: AppBuilderDeployerConfig,
    pub write_file_enabled: bool,
    pub subagents_enabled: bool,
    pub subagent_toggle: HashMap<String, bool>,
    pub background_workflows_enabled: bool,
    pub ask_user_question_enabled: bool,
    pub persona_summaries: Vec<String>,
    pub prompt_audience: PromptAudience,
    pub role_instructions: Option<String>,
    pub persona_instructions: Option<String>,
    pub skills_config: SkillsConfig,
    /// Resolved vendor-compat config (from `Config::compat_resolved`), threaded
    /// into skills / rules / AGENTS.md discovery via the builder.
    pub compat: CompatConfig,
    pub context_window_tokens: u64,
    pub prompt_working_directory: Option<String>,
    pub lsp: Option<Arc<dyn LspBackend>>,
    pub plugin_registry: Option<Arc<xai_grok_agent::plugins::PluginRegistry>>,
    pub api_key_provider: Option<SharedApiKeyProvider>,
    pub attribution_callback: Option<xai_grok_tools::SharedAttributionCallback>,
    pub tool_params_json: ResolvedToolParamsJson,
    pub subagent_event_tx: Option<UnboundedSender<SubagentEvent>>,
    pub monitor_event_buffer: Option<MonitorEventBuffer>,
    pub user_question_tx: UnboundedSender<UserQuestionRequest>,
    pub subagent_depth: u32,
    pub session_id_str: String,
    pub blocking_wait_depth: Arc<crate::tools::tool_context::BlockingWaitState>,
    pub respect_gitignore: bool,
    pub path_not_found_hints: bool,
    pub scheduler_background_loops: bool,
    pub mcp_state: Arc<tokio::sync::Mutex<crate::session::mcp_servers::McpState>>,
    pub managed_gateway_tool_client:
        Option<xai_grok_tools::types::resources::ManagedGatewayToolClient>,
    pub is_non_interactive: bool,
    pub system_prompt_label: String,
    pub owner_session_id: Option<String>,
    pub parent_scheduler_handle:
        Option<xai_grok_tools::implementations::grok_build::scheduler::types::SchedulerHandle>,
}
impl AgentRebuildSpec {
    pub(crate) fn web_search_state(&self) -> ResolvedWebSearchState {
        self.web_search.read().clone()
    }

    pub(crate) fn replace_web_search_state(
        &self,
        next: ResolvedWebSearchState,
    ) -> ResolvedWebSearchState {
        std::mem::replace(&mut *self.web_search.write(), next)
    }

    /// Build a fresh [`Agent`] from this spec and an [`AgentDefinition`].
    ///
    /// This is the canonical construction path; see module docs for the
    /// invariant. The destructure pattern below is intentional —
    /// `#[deny(unused_variables)]` ensures any newly added spec field is
    /// used here, otherwise compilation fails.
    #[deny(unused_variables)]
    pub async fn build_agent(
        self: &Arc<Self>,
        definition: AgentDefinition,
    ) -> Result<Agent, AgentBuildError> {
        self.build_agent_inner(definition, None, None).await
    }
    /// Build an agent with optional one-shot overrides for initial spawn.
    ///
    /// `persisted_skill_names`: restored into the `SkillManager` before
    /// `seed()` to prevent duplicate system-reminder injection on resume.
    ///
    /// `preloaded_skills`: parent-discovered skills passed to
    /// `AgentBuilder::with_preloaded_skills()` to bypass filesystem
    /// discovery in subagents.
    ///
    /// Both are consumed once — the rebuild path (`build_agent`) passes
    /// `None` for both so between-turn harness switches get fresh discovery.
    pub async fn build_agent_with_initial_overrides(
        self: &Arc<Self>,
        definition: AgentDefinition,
        persisted_skill_names: Option<std::collections::HashSet<String>>,
        preloaded_skills: Option<Vec<xai_grok_tools::implementations::skills::types::SkillInfo>>,
    ) -> Result<Agent, AgentBuildError> {
        self.build_agent_inner(definition, persisted_skill_names, preloaded_skills)
            .await
    }
    #[deny(unused_variables)]
    async fn build_agent_inner(
        self: &Arc<Self>,
        definition: AgentDefinition,
        persisted_skill_names: Option<std::collections::HashSet<String>>,
        preloaded_skills: Option<Vec<xai_grok_tools::implementations::skills::types::SkillInfo>>,
    ) -> Result<Agent, AgentBuildError> {
        let Self {
            working_directory,
            terminal_backend,
            fs_backend,
            tools_notification_handle,
            bridge_state_path,
            session_env,
            models_manager,
            compaction_policy,
            reminder_policy,
            tool_mode_preference,
            code_mode_runtime,
            memory_enabled,
            memory_global_path,
            memory_workspace_path,
            memory_backend,
            web_search,
            x_search_enabled,
            backend_search,
            web_fetch_config,
            image_gen_config,
            video_gen_config,
            app_builder_deployer_config,
            write_file_enabled,
            subagents_enabled,
            subagent_toggle,
            background_workflows_enabled,
            ask_user_question_enabled,
            persona_summaries,
            prompt_audience,
            role_instructions,
            persona_instructions,
            skills_config,
            compat,
            context_window_tokens,
            prompt_working_directory,
            lsp,
            plugin_registry,
            api_key_provider,
            attribution_callback,
            tool_params_json,
            subagent_event_tx,
            monitor_event_buffer,
            user_question_tx,
            subagent_depth,
            session_id_str,
            blocking_wait_depth,
            respect_gitignore,
            path_not_found_hints,
            scheduler_background_loops,
            mcp_state,
            managed_gateway_tool_client,
            is_non_interactive,
            system_prompt_label,
            owner_session_id,
            parent_scheduler_handle,
        } = self.as_ref();
        let _ = mcp_state;
        let _ = code_mode_runtime;
        let web_search_config = web_search.read().config.clone();
        #[allow(unused_variables)]
        let is_cursor_template =
            crate::session::is_cursor_system_template(&definition.system_prompt);
        let mut builder = AgentBuilder::new(
            working_directory.clone(),
            terminal_backend.clone(),
            tools_notification_handle.clone(),
        )
        .from_definition(definition)
        .with_compaction_policy(compaction_policy.clone())
        .with_reminder_policy(reminder_policy.clone())
        .with_memory_enabled(*memory_enabled)
        .with_memory_paths(memory_global_path.clone(), memory_workspace_path.clone())
        .with_is_non_interactive(*is_non_interactive)
        .with_system_prompt_label(system_prompt_label.clone())
        .with_session_env(session_env.clone())
        .with_state_path(bridge_state_path.clone())
        .with_web_search_config(web_search_config)
        .with_x_search_config(if *x_search_enabled {
            web_search.read().candidates.xai.clone()
        } else {
            xai_grok_tools::implementations::web_search::WebSearchConfig::Disabled
        })
        .with_backend_search(*backend_search)
        .with_image_gen_config(image_gen_config.clone())
        .with_video_gen_config(video_gen_config.clone())
        .with_app_builder_deployer_config(app_builder_deployer_config.clone())
        .with_web_fetch_config(web_fetch_config.clone())
        .with_write_file_enabled(*write_file_enabled)
        .with_fs(fs_backend.clone())
        .with_subagents_enabled(*subagents_enabled)
        .with_subagent_toggle(subagent_toggle.clone())
        .with_background_workflows_enabled(*background_workflows_enabled)
        .with_task_model_slugs({
            let mut slugs = models_manager
                .available()
                .keys()
                .map(|model_id| model_id.0.to_string())
                .collect::<Vec<_>>();
            // `antigravity:*` slugs advertise the Antigravity CLI roster when
            // the feature is enabled + signed in. Cache-backed and
            // non-blocking: the first rebuild after startup may miss them
            // while the probe runs, but spawn-side validation is independent
            // of this advertisement.
            slugs.extend(crate::agent::antigravity::advertised_slugs_nonblocking());
            slugs
        })
        .with_ask_user_question_enabled(*ask_user_question_enabled)
        .with_persona_summaries(persona_summaries.clone())
        .with_prompt_audience(*prompt_audience)
        .with_role_instructions(role_instructions.clone())
        .with_persona_instructions(persona_instructions.clone())
        .with_skills_config(skills_config.clone())
        .with_compat_config(*compat)
        .with_context_window(*context_window_tokens)
        .with_mcp_max_output_bytes(
            crate::util::config::resolve_max_mcp_output_bytes_for_cwd(working_directory),
        );
        if let Some(owner_session_id) = owner_session_id.clone() {
            builder = builder.with_owner_session_id(owner_session_id);
        }
        if let Some(handle) = parent_scheduler_handle.clone() {
            builder = builder.with_parent_scheduler_handle(handle);
        }
        if let Some(memory_backend) = memory_backend.clone() {
            builder = builder.with_memory_backend(memory_backend);
        }
        if let Some(lsp) = lsp.clone() {
            builder = builder.with_lsp(lsp);
        }
        if let Some(plugin_registry) = plugin_registry.clone() {
            builder = builder.with_plugin_registry(plugin_registry);
        }
        if let Some(api_key_provider) = api_key_provider.clone() {
            builder = builder.with_api_key_provider(api_key_provider);
        }
        if let Some(attribution_callback) = attribution_callback.clone() {
            builder = builder.with_attribution_callback(attribution_callback);
        }
        if let Some(bash_params_json) = tool_params_json.bash.clone() {
            builder = builder.with_bash_params(bash_params_json);
        }
        if let Some(ask_user_question_params_json) = tool_params_json.ask_user_question.clone() {
            builder = builder.with_ask_user_question_params(ask_user_question_params_json);
        }
        if let Some(prompt_working_directory) = prompt_working_directory.clone() {
            builder = builder.with_prompt_working_directory(prompt_working_directory);
        }
        if let Some(names) = persisted_skill_names {
            builder = builder.with_persisted_announced_skill_names(names);
        }
        if let Some(skills) = preloaded_skills {
            builder = builder.with_preloaded_skills(skills);
        }
        let mut agent = builder.build().await?;
        agent.set_tool_mode(
            crate::agent::config::effective_tool_mode(
                xai_grok_sampling_types::ModelProvider::Xai,
                &crate::sampling::ApiBackend::Responses,
                None,
                *tool_mode_preference,
            )
            .expect("xAI Responses with no model requirement is always compatible")
            .mode,
        );
        let model_validator = models_manager.clone();
        agent
            .tool_bridge()
            .update_resource(TaskModelValidator::new(move |requested| {
                if crate::agent::antigravity::is_antigravity_slug(requested) {
                    // Cheap cache-backed gate; the coordinator re-checks
                    // authoritatively (async) at spawn time.
                    return crate::agent::antigravity::task_slug_error_nonblocking(requested);
                }
                model_validator.task_model_error(requested)
            }))
            .await;
        if let Some(event_tx) = subagent_event_tx.clone() {
            use xai_grok_tools::implementations::grok_build::task::backend::{
                ChannelBackend, SubagentBackendResource,
            };
            use xai_grok_tools::implementations::grok_build::task::types::{
                SessionIdResource, SubagentDepthCounter, SubagentEventSender,
            };
            let backend = SubagentBackendResource(Arc::new(ChannelBackend::for_session(
                event_tx.clone(),
                session_id_str.clone(),
            )));
            agent.tool_bridge().update_resource(backend).await;
            agent
                .tool_bridge()
                .update_resource(SubagentDepthCounter(*subagent_depth))
                .await;
            agent
                .tool_bridge()
                .update_resource(SessionIdResource(session_id_str.clone()))
                .await;
            agent
                .tool_bridge()
                .update_resource(SubagentEventSender(event_tx))
                .await;
            agent
                .tool_bridge()
                .update_resource(crate::tools::tool_context::subagent_foreground_wait(
                    Arc::clone(blocking_wait_depth),
                ))
                .await;
            if let Some(buffer) = monitor_event_buffer.clone() {
                agent.tool_bridge().update_resource(buffer).await;
            }
        }
        agent
            .tool_bridge()
            .update_resource(xai_grok_tools::types::resources::RespectGitignore(
                *respect_gitignore,
            ))
            .await;
        agent
            .tool_bridge()
            .update_resource(xai_grok_tools::types::resources::SchedulerBackgroundLoops(
                *scheduler_background_loops,
            ))
            .await;
        agent
            .tool_bridge()
            .update_resource(xai_grok_tools::types::resources::PathNotFoundHints(
                *path_not_found_hints,
            ))
            .await;
        if let Some(client) = managed_gateway_tool_client.clone() {
            agent.tool_bridge().update_resource(client).await;
        }
        {
            use xai_grok_tools::implementations::grok_build::ask_user_question::UserQuestionSender;
            agent
                .tool_bridge()
                .update_resource(UserQuestionSender(user_question_tx.clone()))
                .await;
        }
        Ok(agent)
    }
}
/// Build a stub [`AgentRebuildSpec`] for unit tests.
///
/// Every field is set to a minimal default suitable for test `SessionActor`
/// literals and focused `build_agent` tests.
#[cfg(test)]
pub(crate) fn test_rebuild_spec_default() -> Arc<AgentRebuildSpec> {
    let (uq_tx, _uq_rx) = tokio::sync::mpsc::unbounded_channel();
    Arc::new(AgentRebuildSpec {
        working_directory: std::env::temp_dir(),
        terminal_backend: Arc::new(
            xai_grok_tools::computer::local::LocalTerminalBackend::new_local(
                xai_grok_tools::computer::local::SearchShadowConfig::default(),
            ),
        ),
        fs_backend: Arc::new(xai_grok_tools::computer::local::LocalFs),
        tools_notification_handle: ToolNotificationHandle::noop(),
        bridge_state_path: std::env::temp_dir().join("test_tool_state.json"),
        session_env: Arc::new(HashMap::new()),
        models_manager: crate::agent::models::ModelsManager::default(),
        compaction_policy: CompactionPolicy::default(),
        reminder_policy: ReminderPolicy::default(),
        tool_mode_preference: None,
        code_mode_runtime: crate::session::code_mode::CodeModeRuntimeSlot::new(),
        memory_enabled: false,
        memory_global_path: None,
        memory_workspace_path: None,
        memory_backend: None,
        web_search: parking_lot::RwLock::new(ResolvedWebSearchState::resolved_for(
            crate::tools::config::WebSearchCandidates::disabled(),
            xai_grok_sampling_types::ModelProvider::default(),
        )),
        x_search_enabled: false,
        backend_search: false,
        web_fetch_config: WebFetchConfig::Disabled,
        image_gen_config: ImageGenConfig::default(),
        video_gen_config: VideoGenConfig::default(),
        app_builder_deployer_config: AppBuilderDeployerConfig::default(),
        write_file_enabled: true,
        subagents_enabled: false,
        subagent_toggle: HashMap::new(),
        background_workflows_enabled: false,
        ask_user_question_enabled: true,
        persona_summaries: vec![],
        prompt_audience: PromptAudience::Primary,
        role_instructions: None,
        persona_instructions: None,
        skills_config: SkillsConfig::default(),
        compat: CompatConfig::default(),
        context_window_tokens: 256_000,
        prompt_working_directory: None,
        lsp: None,
        plugin_registry: None,
        api_key_provider: None,
        attribution_callback: None,
        tool_params_json: ResolvedToolParamsJson::default(),
        subagent_event_tx: None,
        monitor_event_buffer: None,
        user_question_tx: uq_tx,
        subagent_depth: 0,
        session_id_str: "test-session".to_string(),
        blocking_wait_depth: Arc::new(crate::tools::tool_context::BlockingWaitState::new()),
        respect_gitignore: false,
        scheduler_background_loops: true,
        path_not_found_hints: false,
        mcp_state: Arc::new(tokio::sync::Mutex::new(
            crate::session::mcp_servers::McpState::new(vec![]),
        )),
        managed_gateway_tool_client: None,
        is_non_interactive: false,
        system_prompt_label: xai_grok_agent::DEFAULT_SYSTEM_PROMPT_LABEL.to_string(),
        owner_session_id: Some("test-session".to_string()),
        parent_scheduler_handle: None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    use crate::tools::config::{WebSearchCandidates, WebSearchSource, WebSearchSourceConfig};
    use xai_grok_sampling_types::ModelProvider;

    fn xai_enabled_config() -> WebSearchConfig {
        WebSearchConfig::Enabled {
            api_key: "xai-key".to_owned(),
            base_url: "https://api.x.ai/v1".to_owned(),
            model: "grok".to_owned(),
            extra_headers: Default::default(),
            alpha_test_key: None,
        }
    }

    fn perplexity_config() -> WebSearchConfig {
        WebSearchConfig::Perplexity {
            api_key: "secret".to_owned(),
            base_url: "https://api.perplexity.ai".to_owned(),
        }
    }

    fn candidates(
        xai: WebSearchConfig,
        perplexity: Option<WebSearchConfig>,
        source: WebSearchSourceConfig,
        legacy_perplexity_enabled: bool,
        implicit_xai_default: bool,
    ) -> WebSearchCandidates {
        WebSearchCandidates {
            xai,
            perplexity,
            source,
            legacy_perplexity_enabled,
            kimi_endpoint: crate::kimi_models::KimiApiEndpoint::Platform,
            implicit_xai_default,
        }
    }

    fn state(candidates: WebSearchCandidates) -> ResolvedWebSearchState {
        ResolvedWebSearchState::resolved_for(candidates, ModelProvider::default())
    }

    /// Defaults with xAI signed in: xAI and Kimi ride the xAI client search,
    /// Codex keeps its native declaration (no client tool).
    #[test]
    fn default_sources_route_xai_search_to_xai_and_kimi_only() {
        let s = state(candidates(
            xai_enabled_config(),
            None,
            WebSearchSourceConfig::default(),
            false,
            true,
        ));
        assert!(s.allowed_for_provider(ModelProvider::Xai));
        assert!(!s.allowed_for_provider(ModelProvider::Codex));
        assert!(s.allowed_for_provider(ModelProvider::Kimi));
        assert!(!s.native_hosted_web_search_suppressed(ModelProvider::Codex));
    }

    /// Neither xAI nor Perplexity available: every provider falls back to
    /// no client search (Kimi gets "none" per the sign-in fallback).
    #[test]
    fn no_credentials_resolves_to_none_everywhere() {
        let s = state(candidates(
            WebSearchConfig::Disabled,
            None,
            WebSearchSourceConfig::default(),
            false,
            true,
        ));
        for provider in [
            ModelProvider::Xai,
            ModelProvider::Codex,
            ModelProvider::Kimi,
        ] {
            assert!(!s.allowed_for_provider(provider), "{provider:?}");
        }
    }

    /// The legacy `[toolset.perplexity_web_search]` toggle keeps acting as
    /// the Kimi default; xAI stays on its own search.
    #[test]
    fn legacy_perplexity_toggle_defaults_kimi_to_perplexity() {
        let s = state(candidates(
            xai_enabled_config(),
            Some(perplexity_config()),
            WebSearchSourceConfig::default(),
            true,
            true,
        ));
        assert!(matches!(
            s.candidates.resolved_config_for(ModelProvider::Kimi),
            WebSearchConfig::Perplexity { .. }
        ));
        assert!(matches!(
            s.candidates.resolved_config_for(ModelProvider::Xai),
            WebSearchConfig::Enabled { .. }
        ));
        assert!(!s.allowed_for_provider(ModelProvider::Codex));
    }

    /// Explicit Codex sources: a resolvable non-native source suppresses the
    /// native hosted declaration; an unresolvable one leaves it in place.
    #[test]
    fn explicit_codex_source_controls_native_suppression() {
        let xai_selected = state(candidates(
            xai_enabled_config(),
            None,
            WebSearchSourceConfig {
                codex: Some(WebSearchSource::Xai),
                ..Default::default()
            },
            false,
            true,
        ));
        assert!(xai_selected.allowed_for_provider(ModelProvider::Codex));
        assert!(xai_selected.native_hosted_web_search_suppressed(ModelProvider::Codex));

        let perplexity_without_key = state(candidates(
            xai_enabled_config(),
            None,
            WebSearchSourceConfig {
                codex: Some(WebSearchSource::Perplexity),
                ..Default::default()
            },
            false,
            true,
        ));
        assert!(!perplexity_without_key.allowed_for_provider(ModelProvider::Codex));
        assert!(!perplexity_without_key.native_hosted_web_search_suppressed(ModelProvider::Codex));
    }

    /// Legacy rule preserved: an explicitly configured web-search model
    /// (non-implicit) opts Codex into the xAI client search by default.
    #[test]
    fn explicit_web_search_model_still_reaches_codex_by_default() {
        let s = state(candidates(
            xai_enabled_config(),
            None,
            WebSearchSourceConfig::default(),
            false,
            false,
        ));
        assert!(s.allowed_for_provider(ModelProvider::Codex));
        assert!(s.native_hosted_web_search_suppressed(ModelProvider::Codex));
    }

    /// Kimi Platform and Kimi Code resolve independently.
    #[test]
    fn kimi_endpoints_resolve_independently() {
        let mut c = candidates(
            xai_enabled_config(),
            Some(perplexity_config()),
            WebSearchSourceConfig {
                kimi_code: Some(WebSearchSource::Perplexity),
                ..Default::default()
            },
            false,
            true,
        );
        // Platform endpoint: kimi_platform default (xAI).
        assert!(matches!(
            c.resolved_config_for(ModelProvider::Kimi),
            WebSearchConfig::Enabled { .. }
        ));
        // Code endpoint: explicit Perplexity selection.
        c.kimi_endpoint = crate::kimi_models::KimiApiEndpoint::Code;
        assert!(matches!(
            c.resolved_config_for(ModelProvider::Kimi),
            WebSearchConfig::Perplexity { .. }
        ));
    }
    use crate::agent::config::{EndpointsConfig, ModelEntry};
    fn model_entry(internal_id: &str) -> ModelEntry {
        ModelEntry::fallback(internal_id, &EndpointsConfig::default())
    }
    fn task_description(agent: &Agent) -> String {
        let toolset = agent.tool_bridge().toolset();
        let task_name = toolset
            .tool_name_for_kind(xai_grok_tools::types::tool::ToolKind::Task)
            .expect("GrokBuild Task tool should be present");
        toolset
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.function.name == task_name)
            .and_then(|definition| definition.function.description)
            .expect("GrokBuild Task description should be present")
    }
    #[tokio::test(flavor = "current_thread")]
    async fn rebuild_uses_mixed_code_mode_for_settings_fallback() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut spec = test_rebuild_spec_default();
                Arc::get_mut(&mut spec)
                    .expect("test rebuild spec should be uniquely owned")
                    .tool_mode_preference =
                    Some(crate::agent::config::ToolModePreference::CodeMode);
                let agent = spec
                    .build_agent(AgentDefinition::default_grok_build())
                    .await
                    .expect("agent build should succeed");
                assert_eq!(
                    agent.tool_mode(),
                    xai_grok_sampling_types::ToolMode::CodeMode
                );
            })
            .await;
    }
    #[tokio::test(flavor = "current_thread")]
    async fn rebuild_projects_fresh_public_model_keys_into_task_description() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut spec = test_rebuild_spec_default();
                Arc::get_mut(&mut spec)
                    .expect("test rebuild spec should be uniquely owned")
                    .subagents_enabled = true;
                let models_manager = spec.models_manager.clone();
                models_manager
                    .insert_test_entry("zeta-public", model_entry("internal-zeta"));
                models_manager
                    .insert_test_entry("alpha-public", model_entry("internal-alpha"));
                let mut hidden = model_entry("internal-hidden");
                hidden.info.hidden = true;
                models_manager.insert_test_entry("private-hidden-model", hidden);
                let mut unselectable = model_entry("internal-unselectable");
                unselectable.info.user_selectable = false;
                models_manager
                    .insert_test_entry("private-unselectable-model", unselectable);
                let first = spec
                    .build_agent(AgentDefinition::default_grok_build())
                    .await
                    .expect("first agent build should succeed");
                let first_description = task_description(&first);
                assert!(
                    first_description
                    .contains("You may choose a different model or provider for a subagent when it materially fits the delegated task better (for example, speed, cost, depth, or provider capabilities). You MUST use only model slugs from this list:\n\
                         - alpha-public\n\
                         - zeta-public")
                );
                assert!(!first_description.contains("private-hidden-model"));
                assert!(!first_description.contains("private-unselectable-model"));
                assert!(!first_description.contains("internal-alpha"));
                let validator = first
                    .tool_bridge()
                    .toolset()
                    .get_resource_cloned::<TaskModelValidator>()
                    .await
                    .expect("Task model validator should be registered");
                assert!(validator.error_for("alpha-public").is_none());
                assert!(validator.error_for("private-hidden-model").is_some());
                models_manager
                    .insert_test_entry("beta-public", model_entry("internal-beta"));
                assert!(validator.error_for("beta-public").is_none());
                let rebuilt = spec
                    .build_agent(AgentDefinition::default_grok_build())
                    .await
                    .expect("rebuilt agent should succeed");
                let rebuilt_description = task_description(&rebuilt);
                assert!(
                    rebuilt_description
                    .contains("You may choose a different model or provider for a subagent when it materially fits the delegated task better (for example, speed, cost, depth, or provider capabilities). You MUST use only model slugs from this list:\n\
                         - alpha-public\n\
                         - beta-public\n\
                         - zeta-public")
                );
            })
            .await;
    }
}
