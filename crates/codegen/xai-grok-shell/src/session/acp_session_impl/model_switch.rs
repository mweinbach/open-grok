use super::*;
use crate::remote::DEFAULT_CONTEXT_WINDOW;
use xai_chat_state::conversation_util::replace_or_insert_system_head;

const MODEL_SWITCH_ACTIVE_TURN_ERROR: &str =
    "Cannot switch models while a turn is active; cancel it or wait for it to finish.";

fn code_mode_transport_enabled(mode: xai_grok_sampling_types::ToolMode) -> bool {
    matches!(
        mode,
        xai_grok_sampling_types::ToolMode::CodeMode
            | xai_grok_sampling_types::ToolMode::CodeModeOnly
    )
}

fn code_mode_runtime_reset_required(
    previous_provider: xai_grok_sampling_types::ModelProvider,
    next_provider: xai_grok_sampling_types::ModelProvider,
    previous_mode: xai_grok_sampling_types::ToolMode,
    next_mode: xai_grok_sampling_types::ToolMode,
) -> bool {
    previous_provider != next_provider
        || code_mode_transport_enabled(previous_mode) != code_mode_transport_enabled(next_mode)
}

impl SessionActor {
    async fn current_provider(&self) -> xai_grok_sampling_types::ModelProvider {
        self.chat_state_handle
            .get_sampling_config()
            .await
            .map(|config| config.provider)
            .unwrap_or_default()
    }

    async fn apply_web_search_toolset_state(
        &self,
        mut state: crate::session::agent_rebuild::ResolvedWebSearchState,
    ) -> Result<(), acp::Error> {
        // The reload broadcast carries provider-agnostic candidates; the
        // active backend is this session's per-provider resolution.
        state.resolve_active(self.current_provider().await);
        let previous = self.rebuild_spec.replace_web_search_state(state);
        let definition = self.agent.borrow().definition().clone();
        let tool_mode = self.agent.borrow().tool_mode();
        match self.build_agent_for_definition(definition, tool_mode).await {
            Ok(agent) => {
                self.install_rebuilt_agent(agent, true).await;
                Ok(())
            }
            Err(error) => {
                self.rebuild_spec.replace_web_search_state(previous);
                Err(error)
            }
        }
    }

    async fn apply_claimed_web_search_reload(
        &self,
        request: crate::session::PendingWebSearchReload,
    ) {
        let result = self.apply_web_search_toolset_state(request.state).await;
        if result.is_ok() {
            self.end_lifecycle_mutation(LifecycleMutationKind::ToolsetReload)
                .await;
        }
        let _ = request.responds_to.send(result);
    }

    pub(super) async fn handle_reload_web_search_toolset(
        self: &Arc<Self>,
        state: crate::session::agent_rebuild::ResolvedWebSearchState,
        responds_to: tokio::sync::oneshot::Sender<Result<(), acp::Error>>,
    ) {
        // Every provider is source-selectable now, so every reload goes
        // through the rebuild path (no native-search fast path).
        let request = crate::session::PendingWebSearchReload { state, responds_to };
        let request = {
            let mut actor_state = self.state.lock().await;
            let turn_active = actor_state.running_task.is_some()
                || self
                    .session_turn_active
                    .load(std::sync::atomic::Ordering::Acquire);
            match actor_state.lifecycle_mutation {
                Some(LifecycleMutationKind::ToolsetReload) if !turn_active => Some(request),
                Some(_) | None if turn_active => {
                    if actor_state.pending_web_search_reload.is_none() {
                        actor_state.pending_web_search_reload = Some(request);
                    } else {
                        let _ = request.responds_to.send(Err(acp::Error::invalid_request()
                            .data("a web search reload is already pending")));
                    }
                    None
                }
                Some(active) => {
                    if actor_state.pending_web_search_reload.is_none() {
                        actor_state.pending_web_search_reload = Some(request);
                    } else {
                        let _ = request
                            .responds_to
                            .send(Err(acp::Error::invalid_request().data(format!(
                                "a session {} is already in progress",
                                active.as_str()
                            ))));
                    }
                    None
                }
                None => {
                    actor_state.lifecycle_mutation = Some(LifecycleMutationKind::ToolsetReload);
                    Some(request)
                }
            }
        };
        if let Some(request) = request {
            self.apply_claimed_web_search_reload(request).await;
        }
    }

    pub(super) async fn maybe_apply_pending_web_search_reload(&self) {
        let request = {
            let mut state = self.state.lock().await;
            if state.running_task.is_some()
                || self
                    .session_turn_active
                    .load(std::sync::atomic::Ordering::Acquire)
                || state.lifecycle_mutation.is_some()
            {
                return;
            }
            let request = state.pending_web_search_reload.take();
            if request.is_some() {
                state.lifecycle_mutation = Some(LifecycleMutationKind::ToolsetReload);
            }
            request
        };
        if let Some(request) = request {
            self.apply_claimed_web_search_reload(request).await;
        }
    }

    pub(super) async fn handle_set_session_model(
        &self,
        selected_model_id: acp::ModelId,
        sampling_config: xai_grok_sampler::SamplerConfig,
        use_concise: bool,
        apply_prompt_override: bool,
        skip_prompt_rewrite: bool,
        auto_compact_threshold_percent: u8,
        agent_rebuild: Option<(Box<xai_grok_agent::AgentDefinition>, bool)>,
        resolved_tool_policy_override: Option<crate::session::tool_surface::ResolvedToolPolicy>,
    ) -> Result<acp::ModelId, acp::Error> {
        if let Err(blocked) = self
            .begin_lifecycle_mutation(LifecycleMutationKind::ModelSwitch)
            .await
        {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                reason = %blocked.message(),
                "handle_set_session_model: lifecycle gate unavailable"
            );
            let message = match blocked {
                LifecycleMutationBlock::ActiveTurn => MODEL_SWITCH_ACTIVE_TURN_ERROR.to_string(),
                LifecycleMutationBlock::MutationInProgress(_) => blocked.message(),
            };
            return Err(acp::Error::invalid_request().data(message));
        }

        let result = self
            .handle_set_session_model_while_gated(
                selected_model_id,
                sampling_config,
                use_concise,
                apply_prompt_override,
                skip_prompt_rewrite,
                auto_compact_threshold_percent,
                agent_rebuild,
                resolved_tool_policy_override,
            )
            .await;
        self.end_lifecycle_mutation(LifecycleMutationKind::ModelSwitch)
            .await;
        result
    }

    async fn handle_set_session_model_while_gated(
        &self,
        selected_model_id: acp::ModelId,
        sampling_config: xai_grok_sampler::SamplerConfig,
        use_concise: bool,
        apply_prompt_override: bool,
        skip_prompt_rewrite: bool,
        auto_compact_threshold_percent: u8,
        agent_rebuild: Option<(Box<xai_grok_agent::AgentDefinition>, bool)>,
        resolved_tool_policy_override: Option<crate::session::tool_surface::ResolvedToolPolicy>,
    ) -> Result<acp::ModelId, acp::Error> {
        let previous_provider = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|config| config.provider)
            .unwrap_or_default();
        let previous_tool_mode = self.agent.borrow().tool_mode();
        let model_tool_mode = crate::agent::models::resolve_model_tool_mode(
            &self.models_manager.models(),
            &selected_model_id,
        );
        let current_resolution = crate::agent::config::effective_tool_mode(
            sampling_config.provider,
            &sampling_config.api_backend,
            model_tool_mode,
            self.rebuild_spec.tool_mode_preference,
        )
        .map_err(|error| acp::Error::invalid_request().data(error.to_string()))?;
        let resolved_tool_policy =
            crate::session::tool_surface::ResolvedToolPolicy::select_for_route(
                current_resolution,
                resolved_tool_policy_override,
                sampling_config.provider,
                &sampling_config.api_backend,
            )
            .map_err(|error| acp::Error::invalid_request().data(error))?;
        let effective_tool_mode = resolved_tool_policy.resolved.mode;
        // Re-resolve the client web-search backend for the incoming provider
        // so a staged agent registers the source selected for it in
        // `[toolset.web_search_source]` (e.g. Kimi→Codex must not carry a
        // Perplexity backend across the boundary). Swapped back if the staged
        // build below fails and the session stays on the previous provider.
        let web_search_previous = (previous_provider != sampling_config.provider).then(|| {
            let mut state = self.rebuild_spec.web_search_state();
            state.resolve_active(sampling_config.provider);
            self.rebuild_spec.replace_web_search_state(state)
        });
        // Build a replacement harness to completion before invalidating the
        // live JavaScript timeline. Agent construction is the fallible part of
        // a harness switch; staging it here keeps a failed model switch from
        // resetting an otherwise unchanged Code Mode session.
        let prepared_agent_rebuild = match agent_rebuild {
            Some((definition, preserve_history)) => {
                match self
                    .build_agent_for_definition(*definition, effective_tool_mode)
                    .await
                {
                    Ok(agent) => Some((agent, preserve_history)),
                    Err(error) => {
                        if let Some(previous) = web_search_previous {
                            self.rebuild_spec.replace_web_search_state(previous);
                        }
                        return Err(error);
                    }
                }
            }
            None => None,
        };

        // Close the cumulative xAI export boundary synchronously before any
        // state mutation, await, or telemetry task can observe Codex content.
        self.feedback_manager
            .observe_provider(sampling_config.provider);
        if code_mode_runtime_reset_required(
            previous_provider,
            sampling_config.provider,
            previous_tool_mode,
            effective_tool_mode,
        ) {
            self.rebuild_spec
                .code_mode_runtime
                .reset()
                .await
                .map_err(|error| {
                    acp::Error::internal_error()
                        .data(format!("failed to reset Code Mode runtime: {error}"))
                })?;
        }
        if let Some((agent, preserve_history)) = prepared_agent_rebuild {
            self.install_rebuilt_agent(agent, preserve_history).await;
        }
        let model_id = acp::ModelId::new(sampling_config.model.clone());
        let new_context_window = self.compaction.context_window_override.unwrap_or_else(|| {
            std::num::NonZeroU64::new(sampling_config.context_window).unwrap_or_else(|| {
                std::num::NonZeroU64::new(DEFAULT_CONTEXT_WINDOW)
                    .expect("DEFAULT_CONTEXT_WINDOW is non-zero")
            })
        });
        let prev_threshold = self.compaction.threshold_percent.get();
        if prev_threshold != auto_compact_threshold_percent {
            tracing::info!(
                session_id = %self.session_info.id.0,
                new_model = %sampling_config.model,
                old_threshold = prev_threshold,
                new_threshold = auto_compact_threshold_percent,
                "auto_compact_threshold_percent updated for model switch"
            );
        }
        self.compaction
            .threshold_percent
            .set(auto_compact_threshold_percent);
        self.memory.active_provider.set(sampling_config.provider);
        self.supports_backend_search
            .set(sampling_config.supports_backend_search);
        self.compactions_remaining
            .set(sampling_config.compactions_remaining);
        self.compaction_at_tokens
            .set(sampling_config.compaction_at_tokens);
        self.agent.borrow_mut().set_tool_mode(effective_tool_mode);
        xai_grok_telemetry::unified_log::info(
            "backend_search: model switch",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!(
                { "new_model" : & sampling_config.model, "api_backend" :
                format!("{:?}", sampling_config.api_backend),
                "tool_mode" : format!("{:?}", effective_tool_mode),
                "supports_backend_search" : sampling_config.supports_backend_search,
                }
            )),
        );
        self.chat_state_handle
            .update_sampling_config(xai_grok_sampling_types::SamplingConfig {
                base_url: sampling_config.base_url.clone(),
                model: sampling_config.model.clone(),
                max_completion_tokens: sampling_config.max_completion_tokens,
                temperature: sampling_config.temperature,
                top_p: sampling_config.top_p,
                api_backend: sampling_config.api_backend,
                provider: sampling_config.provider,
                extra_headers: sampling_config.extra_headers.clone(),
                query_params: sampling_config.query_params.clone(),
                env_http_headers: sampling_config.env_http_headers.clone(),
                context_window: new_context_window,
                reasoning_effort: sampling_config.reasoning_effort,
                stream_tool_calls: Some(sampling_config.stream_tool_calls),
            });
        let existing = self.chat_state_handle.get_credentials().await;
        let session_key = self
            .auth_manager
            .as_ref()
            .and_then(|am| am.current_or_expired().map(|a| a.key));
        self.chat_state_handle
            .update_credentials(xai_chat_state::Credentials {
                api_key: sampling_config.api_key.clone(),
                auth_type: crate::agent::config::resolve_chat_state_auth_type_for_sampling_config(
                    &sampling_config,
                    session_key.as_deref(),
                    existing.auth_type,
                ),
                alpha_test_key: existing.alpha_test_key,
                client_version: sampling_config.client_version.clone(),
            });
        self.invalidate_model_auth_memo();
        self.signals_handle()
            .record_model_usage(&sampling_config.model);
        if apply_prompt_override && !skip_prompt_rewrite {
            let mut conversation = self.chat_state_handle.get_conversation().await;
            for item in conversation.iter_mut() {
                if let ConversationItem::System(sys) = item {
                    if use_concise {
                        sys.content = std::sync::Arc::<str>::from(
                            xai_grok_agent::prompt::template::COMPACT_SYSTEM_PROMPT,
                        );
                    } else {
                        sys.content =
                            std::sync::Arc::<str>::from(self.agent.borrow().system_prompt());
                    }
                    break;
                }
            }
            self.chat_state_handle.replace_conversation(conversation);
        } else if !apply_prompt_override {
            tracing::info!(
                session_id = %self.session_info.id.0,
                model_id = %model_id.0,
                "handle_set_session_model: skipping prompt override (apply_prompt_override=false)"
            );
        } else {
            tracing::info!(
                session_id = %self.session_info.id.0,
                model_id = %model_id.0,
                "handle_set_session_model: skipping prompt rewrite (just rebuilt harness)"
            );
        }
        let agent_name = self.agent.borrow().definition().name.clone();
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::CurrentModel {
                model_id: model_id.clone(),
                provider: sampling_config.provider,
                agent_name: Some(agent_name),
                reasoning_effort: Some(sampling_config.reasoning_effort),
                resolved_tool_policy: Some(resolved_tool_policy),
            });
        Ok(model_id)
    }
    /// Rebuild the active provider harness between turns.
    ///
    /// Builds a fresh [`xai_grok_agent::Agent`] from the cached
    /// [`crate::session::agent_rebuild::AgentRebuildSpec`] + the supplied
    /// [`xai_grok_agent::AgentDefinition`], replaces `self.agent`,
    /// rewrites the system message in the conversation, persists the
    /// new prompt artifacts, and updates `active_agent_type`.
    ///
    /// Triggered from `MvpAgent::set_session_model` when the new model's
    /// `agent_type` differs from the session's current `active_agent_type`.
    /// Defense-in-depth: rejects if a turn is in flight. When
    /// `preserve_history` is true, the existing first user/prefix item is not
    /// rewritten because it is no longer safe to infer its role by position.
    #[cfg(test)]
    pub(super) async fn handle_rebuild_agent_for_definition(
        &self,
        definition: xai_grok_agent::AgentDefinition,
        preserve_history: bool,
    ) -> Result<(), acp::Error> {
        let turn_slot_active = self.state.lock().await.running_task.is_some();
        let turn_future_active = self
            .session_turn_active
            .load(std::sync::atomic::Ordering::Acquire);
        if turn_slot_active || turn_future_active {
            tracing::warn!(
                session_id = % self.session_info.id.0, new_agent_type = % definition.name,
                turn_slot_active,
                turn_future_active,
                "handle_rebuild_agent_for_definition: turn in flight, rejecting rebuild"
            );
            return Err(acp::Error::invalid_request().data(MODEL_SWITCH_ACTIVE_TURN_ERROR));
        }
        let current_tool_mode = self.agent.borrow().tool_mode();
        let new_agent = self
            .build_agent_for_definition(definition, current_tool_mode)
            .await?;
        self.install_rebuilt_agent(new_agent, preserve_history)
            .await;
        Ok(())
    }

    async fn build_agent_for_definition(
        &self,
        definition: xai_grok_agent::AgentDefinition,
        tool_mode: xai_grok_sampling_types::ToolMode,
    ) -> Result<xai_grok_agent::Agent, acp::Error> {
        let new_agent_name = definition.name.clone();
        tracing::info!(
            session_id = % self.session_info.id.0, new_agent_type = % new_agent_name,
            "handle_rebuild_agent_for_definition: building replacement harness"
        );
        let mut new_agent = self
            .rebuild_spec
            .build_agent(definition)
            .await
            .map_err(|e| {
                tracing::error!(
                    session_id = %self.session_info.id.0,
                    new_agent_type = %new_agent_name,
                    error = %e,
                    "handle_rebuild_agent_for_definition: AgentBuilder::build failed"
                );
                acp::Error::internal_error().data(format!(
                    "rebuild_agent: build failed for agent_type={new_agent_name}: {e}"
                ))
            })?;
        new_agent.set_tool_mode(tool_mode);
        Ok(new_agent)
    }

    async fn install_rebuilt_agent(
        &self,
        new_agent: xai_grok_agent::Agent,
        preserve_history: bool,
    ) {
        let new_agent_name = new_agent.definition().name.clone();
        let new_system_prompt = new_agent.system_prompt().to_string();
        let mut new_prompt_context = new_agent.prompt_context().clone();
        new_prompt_context.normalize_for_persistence();
        if let Some(handle) = self.compaction.prefire.take_handle() {
            handle.abort();
            let _ = handle.await;
            self.compaction.prefire.finish();
        }
        self.compaction.prefire.clear();
        *self.agent.borrow_mut() = new_agent;
        *self.active_agent_type.lock() = Some(new_agent_name.clone());
        self.emit_resolved_tool_overrides();
        self.queue_exit_reminder_on_approved_exit.store(
            self.is_cursor_harness(),
            std::sync::atomic::Ordering::Relaxed,
        );
        if let Err(e) = self.workspace_ops.bind_local_session(
            &self.session_id_string(),
            self.tool_context.cwd.as_path().to_path_buf(),
            self.tool_context.hunk_tracker_handle.clone(),
            self.agent.borrow().tool_bridge().toolset(),
            None,
        ) {
            tracing::warn!(error = %e, "failed to rebind local session toolset after agent rebuild");
        }
        {
            let bridge = self.agent.borrow().tool_bridge().clone();
            let snapshot = self.tool_metadata_snapshot.clone();
            let tool_index = crate::session::tool_index::Bm25ToolSearchIndex::new(snapshot);
            bridge
                .update_resource(xai_grok_tools::types::tool_index::ToolIndex(
                    std::sync::Arc::new(tool_index),
                ))
                .await;
            if let Some(client) = self.rebuild_spec.managed_gateway_tool_client.clone() {
                bridge.update_resource(client).await;
            }
            let plan_path = self.plan_mode.lock().plan_file_path().to_path_buf();
            bridge
                .update_resource(xai_grok_tools::types::resources::PlanFilePath(plan_path))
                .await;
            if let Some(display_cwd) = self.display_cwd.get() {
                bridge
                    .set_display_cwd(std::path::PathBuf::from(display_cwd))
                    .await;
            }
            bridge
                .update_resource(
                    xai_grok_tools::implementations::grok_build::workflow::WorkflowLaunchHandle(
                        self.workflow_launch_tx.clone(),
                    ),
                )
                .await;
            if !self.goal_runs_on_workflow_engine() {
                bridge
                    .update_resource(
                        xai_grok_tools::implementations::grok_build::update_goal::GoalUpdateHandle(
                            self.goal_update_tx.clone(),
                        ),
                    )
                    .await;
            }
            if let Some(reservations) = self.tool_context.task_completion_reservations.clone() {
                bridge.update_resource(reservations).await;
            }
            if let Some(gate) = self.tool_context.task_wake_suppressed.clone() {
                bridge.update_resource(gate).await;
            }
            self.inject_deny_read_globs().await;
        }
        {
            let notified = self.mcp_handshakes_done.notified();
            tokio::pin!(notified);
            let needs_wait = {
                let s = self.mcp_state.lock().await;
                !s.configs.is_empty() && !s.is_initialized()
            };
            if needs_wait {
                const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
                tokio::select! {
                    () = &mut notified => {}
                    () = tokio::time::sleep(TIMEOUT) => {
                        tracing::warn!(
                            session_id = %self.session_info.id.0,
                            "handle_rebuild_agent_for_definition: timed out waiting for MCP handshakes"
                        );
                    }
                }
            }
        }
        self.re_register_mcp_tools_on_rebuilt_bridge().await;
        if let Some(old_handle) = self.deferred_prefix.take() {
            old_handle.abort();
        }
        let new_user_prefix = if preserve_history {
            None
        } else {
            Some(self.build_user_message_prefix().await)
        };
        {
            let mut conversation = self.chat_state_handle.get_conversation().await;
            let _ = replace_or_insert_system_head(&mut conversation, &new_system_prompt);
            if let Some(new_user_prefix) = new_user_prefix {
                let drop_startup_skill_reminder = false;
                Self::rewrite_zero_turn_prefix(
                    &mut conversation,
                    new_user_prefix,
                    drop_startup_skill_reminder,
                );
            }
            if !conversation_has_project_instructions(&conversation)
                && let Some(agents_md_reminder) = self.agent.borrow().agents_md_user_reminder()
            {
                let agents_md_at = conversation.len().min(2);
                conversation.insert(
                    agents_md_at,
                    ConversationItem::project_instructions(agents_md_reminder),
                );
            }
            self.inject_baseline_skill_reminder(&mut conversation).await;
            self.chat_state_handle.replace_conversation(conversation);
        }
        save_prompt_context(&self.session_info, &new_prompt_context);
        save_system_prompt(&self.session_info, &new_system_prompt);
        let snapshot = self.chat_state_handle.get_conversation().await;
        persist_chat_history_jsonl_sync(&self.session_info, &snapshot);
        self.mcp_reminder_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.send_available_commands_update().await;
        tracing::info!(
            session_id = %self.session_info.id.0,
            new_agent_type = %new_agent_name,
            "handle_rebuild_agent_for_definition: harness rebuild complete"
        );
    }
    /// Apply a client-supplied `systemPromptOverride` on session attach without
    /// wiping user/assistant history: swap only the leading `System` message,
    /// atomically inside the `ChatStateActor` (see
    /// `ChatStateCommand::ReplaceSystemHead` for the serialization guarantees).
    /// `system_prompt.txt` (not owned by the persistence actor) is saved
    /// directly, even on a head no-op, so a previously-diverged secondary
    /// artifact self-heals. Skipped entirely on a verbatim mirror-fork
    /// (`preserve_inherited_system`).
    pub(super) async fn handle_replace_system_prompt(&self, system_prompt: String) {
        if self.startup_hints.preserve_inherited_system {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: skipped (preserve_inherited_system)"
            );
            return;
        }
        let Some(changed) = self
            .chat_state_handle
            .replace_system_head(&system_prompt)
            .await
        else {
            tracing::error!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: chat-state actor unavailable; override not applied"
            );
            return;
        };
        save_system_prompt(&self.session_info, &system_prompt);
        if changed {
            tracing::info!(
                session_id = %self.session_info.id.0,
                prompt_len = system_prompt.len(),
                "handle_replace_system_prompt: client override applied"
            );
        } else {
            tracing::debug!(
                session_id = %self.session_info.id.0,
                "handle_replace_system_prompt: head already matches, no-op"
            );
        }
    }
}

#[cfg(test)]
mod runtime_reset_policy_tests {
    use super::code_mode_runtime_reset_required;
    use xai_grok_sampling_types::{ApiBackend, CodeModeTransport, ModelProvider, ToolMode};

    #[test]
    fn provider_or_transport_boundary_resets_but_presentation_only_change_does_not() {
        assert!(code_mode_runtime_reset_required(
            ModelProvider::Xai,
            ModelProvider::Codex,
            ToolMode::CodeMode,
            ToolMode::CodeMode,
        ));
        assert!(code_mode_runtime_reset_required(
            ModelProvider::Xai,
            ModelProvider::Xai,
            ToolMode::Direct,
            ToolMode::CodeMode,
        ));
        assert!(!code_mode_runtime_reset_required(
            ModelProvider::Xai,
            ModelProvider::Xai,
            ToolMode::CodeMode,
            ToolMode::CodeModeOnly,
        ));
        assert!(!code_mode_runtime_reset_required(
            ModelProvider::Xai,
            ModelProvider::Xai,
            ToolMode::Direct,
            ToolMode::Direct,
        ));
    }

    #[test]
    fn cold_resume_keeps_persisted_preference_but_not_over_model_requirement() {
        let persisted = crate::session::tool_surface::ResolvedToolPolicy {
            resolved: crate::agent::config::ResolvedToolMode {
                mode: ToolMode::CodeMode,
                source: crate::agent::config::ToolModeSource::UserPreference,
            },
            transport: Some(CodeModeTransport::NativeCustomGrammar),
            route_provider: Some(ModelProvider::Codex),
            route_backend: Some(ApiBackend::Responses),
        };
        let current_default = crate::agent::config::ResolvedToolMode {
            mode: ToolMode::Direct,
            source: crate::agent::config::ToolModeSource::Default,
        };
        assert_eq!(
            crate::session::tool_surface::ResolvedToolPolicy::select_for_route(
                current_default,
                Some(persisted),
                ModelProvider::Codex,
                &ApiBackend::Responses,
            )
            .unwrap(),
            persisted,
        );

        let required = crate::agent::config::ResolvedToolMode {
            mode: ToolMode::CodeModeOnly,
            source: crate::agent::config::ToolModeSource::ModelRequirement,
        };
        let selected = crate::session::tool_surface::ResolvedToolPolicy::select_for_route(
            required,
            Some(persisted),
            ModelProvider::Codex,
            &ApiBackend::Responses,
        )
        .unwrap();
        assert_eq!(selected.resolved, required);
        assert_eq!(
            selected.transport,
            Some(CodeModeTransport::NativeCustomGrammar)
        );
    }
}
