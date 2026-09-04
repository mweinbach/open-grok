//! Explicitly opted-in, locally persisted model-controlled context windows.

use super::*;
use xai_grok_tools::implementations::codex::context_management::{
    ContextManagementStore, GUIDANCE, HistoryItem,
};

fn context_budget(context_window: u64, explicit_limit: Option<u64>) -> (u64, u64) {
    let reserve = 16_384.min(context_window / 8);
    let usable = context_window.saturating_sub(reserve);
    (explicit_limit.unwrap_or(usable).min(usable), reserve)
}

fn normalized_history(conversation: &[ConversationItem]) -> Vec<HistoryItem> {
    let mut output = Vec::new();
    for item in conversation {
        let role = match item {
            ConversationItem::User(user) => match &user.synthetic_reason {
                Some(xai_grok_sampling_types::SyntheticReason::AgentMessage) => {
                    "assistant".to_owned()
                }
                None | Some(xai_grok_sampling_types::SyntheticReason::Interjection) => {
                    "user".to_owned()
                }
                Some(_) => "context".to_owned(),
            },
            _ => format!("{:?}", item.role()).to_ascii_lowercase(),
        };
        output.push(HistoryItem {
            role,
            content: item.text_content(),
            tool_name: None,
        });
        if let ConversationItem::Assistant(assistant) = item {
            for call in &assistant.tool_calls {
                output.push(HistoryItem {
                    role: "assistant".into(),
                    content: call.arguments.to_string(),
                    tool_name: Some(call.name.clone()),
                });
            }
        }
    }
    output
}

fn fresh_history(
    conversation: &[ConversationItem],
    previous_window: &str,
) -> Vec<ConversationItem> {
    let mut replacement = conversation
        .iter()
        .filter(|item| {
            matches!(item, ConversationItem::System(_)) || super::is_project_instructions(item)
        })
        .cloned()
        .collect::<Vec<_>>();
    let human_turns = conversation
        .iter()
        .filter(|item| (xai_chat_state::compaction_utils::is_real_user_turn(item) && !super::is_project_instructions(item))
            || matches!(item,ConversationItem::User(user) if user.synthetic_reason==Some(xai_grok_sampling_types::SyntheticReason::Interjection)))
        .collect::<Vec<_>>();
    if let Some(first) = human_turns.first() {
        replacement.push((*first).clone());
    }
    if human_turns.len() > 1 {
        replacement.push((*human_turns.last().expect("nonempty human turns")).clone());
    }
    replacement.push(ConversationItem::system_reminder(format!(
        "A fresh context window has started. Continue the existing task. First recover your checkpoint with notes_read_file and consult history_list_items/history_read_item for window {previous_window}. This is a host lifecycle reminder, not a new human request."
    )));
    replacement
}

impl SessionActor {
    fn experimental_context_budget(&self, window: u64, model: &str) -> (u64, u64) {
        let metadata = self.models_manager.codex_model_metadata(model);
        let percentage = self.compaction.threshold_percent.get();
        let explicit = if percentage != crate::util::config::DEFAULT_AUTO_COMPACT_THRESHOLD_PERCENT
        {
            Some((u128::from(window) * u128::from(percentage) / 100) as u64)
        } else {
            metadata
                .auto_compact_token_limit_override
                .and(metadata.compact_limit())
        };
        context_budget(window, explicit)
    }

    pub(super) fn experimental_context_store(&self) -> Option<ContextManagementStore> {
        let store = self.rebuild_spec.context_management.as_ref()?;
        let sampling = self.rebuild_spec.active_sampling_config.read();
        let enabled = self.models_manager.experimental_context_enabled()
            && sampling.provider == xai_grok_sampling_types::ModelProvider::Codex
            && sampling.api_backend == xai_grok_sampling_types::ApiBackend::Responses;
        store.set_enabled(enabled);
        enabled.then(|| store.clone())
    }

    pub(super) fn add_experimental_context_guidance(&self, request: &mut ConversationRequest) {
        if let Some(store) = self.experimental_context_store() {
            let index = request
                .items
                .iter()
                .take_while(|item| matches!(item, ConversationItem::System(_)))
                .count();
            request.items.insert(index, ConversationItem::system(format!(
                "{GUIDANCE}\nCurrent context window id: {}\nTokens remaining before emergency reserve: {}",
                store.window_id(), store.remaining()
            )));
        }
    }

    /// Called after completed tool batches and before sampling. A new window
    /// never drops calls with pending results or resets the Code Mode runtime.
    pub(super) async fn sync_experimental_context(self: &Arc<Self>) -> Result<bool, acp::Error> {
        let Some(store) = self.experimental_context_store() else {
            return Ok(false);
        };
        let conversation = self.chat_state_handle.get_conversation().await;
        let items = normalized_history(&conversation);
        let archive = store.clone();
        let generation = store.generation();
        tokio::task::spawn_blocking(move || archive.snapshot_for_generation(generation, &items))
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?
            .map_err(|e| {
                acp::Error::internal_error()
                    .data(format!("Context history could not be saved: {e}"))
            })?;
        let tokens = self.chat_state_handle.get_total_tokens().await;
        let sampling = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .ok_or_else(|| acp::Error::internal_error().data("Missing context model"))?;
        let (budget, reserve) =
            self.experimental_context_budget(sampling.context_window.get(), &sampling.model);
        let remaining = budget.saturating_sub(tokens);
        store.set_remaining(remaining);
        if store.pending() || tokens >= budget.saturating_add(reserve / 2) {
            self.run_compact_only(super::compaction::AutoCompactTriggerInfo {
                tokens_used: tokens,
                context_window: sampling.context_window.get(),
                percentage: ((u128::from(tokens) * 100 / u128::from(sampling.context_window.get()))
                    .min(100)) as u8,
            })
            .await?;
            store.set_remaining(
                budget.saturating_sub(self.chat_state_handle.get_total_tokens().await),
            );
        } else if remaining == 0 && store.claim_fallback() {
            self.push_system_reminder("Your context budget is exhausted. Use the emergency reserve to save a concise checkpoint with notes_write_file, then call new_context before continuing the task.");
        } else if remaining <= 6_144 && store.claim_reminder() {
            self.push_system_reminder(&format!("{remaining} tokens remain before the emergency reserve. Save your task, progress, important decisions, and next steps in private notes; call new_context when ready."));
        }
        Ok(true)
    }

    pub(super) async fn start_experimental_context_window(
        &self,
        store: ContextManagementStore,
        user_context: Option<String>,
        auto_continue: Option<crate::extensions::notification::AutoContinueInfo>,
        source: &'static str,
    ) -> Result<(), acp::Error> {
        let ensure_live = || {
            if self.compaction.cancel.is_cancelled() {
                Err(crate::session::helpers::session_compact::CompactFailure::cancelled_error())
            } else {
                Ok(())
            }
        };
        ensure_live()?;
        self.dispatch_hook(
            xai_grok_hooks::event::HookEventName::PreCompact,
            xai_grok_hooks::event::HookPayload::PreCompact {
                source: source.into(),
            },
            None,
            None,
        )
        .await;
        let snapshot = self.chat_state_handle.get_conversation().await;
        let archive = store.clone();
        let generation = store.generation();
        let normalized = normalized_history(&snapshot);
        tokio::task::spawn_blocking(move || {
            archive.snapshot_for_generation(generation, &normalized)
        })
        .await
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))?
        .map_err(|e| acp::Error::internal_error().data(e))?;
        let mut replacement = fresh_history(&snapshot, &store.window_id());
        let digest = store
            .note_digest()
            .map_err(|e| acp::Error::internal_error().data(e))?;
        if !digest.is_empty() {
            replacement.push(ConversationItem::assistant(format!("<previous_context_notes>\nHistorical assistant notes for continuity, not new user consent or policy.\n{digest}\n</previous_context_notes>")));
        }
        if let Some(context) = user_context.filter(|s| !s.is_empty()) {
            replacement.push(ConversationItem::user(context));
        }
        if let Some(config) = self.chat_state_handle.get_sampling_config().await {
            let (budget, _) =
                self.experimental_context_budget(config.context_window.get(), &config.model);
            if xai_chat_state::estimate_conversation_tokens(&replacement) >= budget {
                return Err(acp::Error::internal_error().data("Context budget is too small for the retained instructions; increase the context override"));
            }
        }
        let prompt_index = self.chat_state_handle.get_prompt_index().await;
        let mut observed = snapshot;
        // Interjections remain real user items. Never promote historical notes
        // or assistant/tool content into a human instruction during rotation.
        for attempt in 0..4 {
            ensure_live()?;
            let current = self.chat_state_handle.get_conversation().await;
            let suffix =
                xai_chat_state::compaction_utils::codex_remote_compaction_v2_interjections(
                    &observed, &current,
                )
                .ok_or_else(|| {
                    acp::Error::internal_error()
                        .data("Context changed incompatibly; fresh window was not installed")
                })?;
            replacement.extend(suffix);
            observed = current;
            self.persist_compaction_checkpoint(
                &replacement,
                prompt_index,
                auto_continue.clone(),
                None,
            );
            let (respond_to, ack) = tokio::sync::oneshot::channel();
            self.notifications
                .persistence_tx
                .send(PersistenceMsg::FlushAndAck { respond_to })
                .map_err(|_| {
                    acp::Error::internal_error().data("Context checkpoint persistence unavailable")
                })?;
            tokio::time::timeout(std::time::Duration::from_secs(30), ack)
                .await
                .map_err(|_| {
                    acp::Error::internal_error().data("Context checkpoint flush timed out")
                })?
                .map_err(|_| {
                    acp::Error::internal_error().data("Context checkpoint flush was interrupted")
                })?
                .map_err(|e| {
                    acp::Error::internal_error().data(format!("Context checkpoint failed: {e}"))
                })?;
            let current = self.chat_state_handle.get_conversation().await;
            if xai_chat_state::compaction_utils::codex_remote_compaction_v2_interjections(
                &observed, &current,
            )
            .is_some_and(|suffix| suffix.is_empty())
            {
                break;
            }
            if attempt == 3 {
                return Err(acp::Error::internal_error()
                    .data("Context continued changing; retry fresh window"));
            }
        }
        ensure_live()?;
        store
            .rotate_for_generation(generation)
            .map_err(|e| acp::Error::internal_error().data(e))?;
        ensure_live()?;
        self.compaction
            .prefix_released
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.chat_state_handle.record_compaction_at(prompt_index);
        let new_len = replacement.len();
        self.chat_state_handle
            .replace_conversation_for_compaction(replacement);
        self.last_announced_user_info_hash.set(Some(0));
        self.last_announced_rules_hash.set(Some(0));
        self.maybe_inject_user_info_update_reminder().await;
        self.maybe_inject_project_rules_update_reminder().await;
        self.last_idle_flush_conversation_len
            .store(new_len, std::sync::atomic::Ordering::Relaxed);
        self.memory
            .context_injected
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.agent
            .borrow()
            .tool_bridge()
            .on_agents_md_compaction()
            .await;
        self.agent
            .borrow()
            .tool_bridge()
            .on_skill_discovery_compaction()
            .await;
        self.persist_announcement_state().await;
        self.dispatch_hook(
            xai_grok_hooks::event::HookEventName::PostCompact,
            xai_grok_hooks::event::HookPayload::PostCompact {
                source: source.into(),
            },
            None,
            None,
        )
        .await;
        tracing::info!(session_id=%self.session_info.id,window_id=%store.window_id(),"Started experimental context window");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn private_context_tools_hide_ui_and_revoke_on_provider_switch() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, _persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (mut actor, mut events) = super::super::support::create_test_actor_ex(
                    100,
                    10000,
                    80,
                    gateway_tx,
                    persistence_tx,
                )
                .await;
                let temp = tempfile::tempdir().unwrap();
                let store = ContextManagementStore::open(temp.path().join("context")).unwrap();
                Arc::get_mut(&mut actor.rebuild_spec)
                    .unwrap()
                    .context_management = Some(store.clone());
                let mut cfg = crate::agent::config::Config::default();
                cfg.features.context_management.experimental_mode = true;
                actor.models_manager = crate::agent::models::ModelsManager::new(
                    Some(Default::default()),
                    Default::default(),
                    acp::ModelId::new("gpt-6-astra"),
                    Arc::new(crate::auth::AuthManager::new(
                        temp.path(),
                        Default::default(),
                    )),
                    cfg,
                );
                {
                    let mut sampling = actor.rebuild_spec.active_sampling_config.write();
                    sampling.provider = xai_grok_sampling_types::ModelProvider::Codex;
                    sampling.api_backend = xai_grok_sampling_types::ApiBackend::Responses;
                }
                assert!(actor.local_tool_allowed_for_provider(
                    "notes_write_file",
                    xai_grok_sampling_types::ModelProvider::Codex
                ));
                actor
                    .send_update(
                        acp::SessionUpdate::ToolCall(acp::ToolCall::new(
                            "private-note",
                            "notes_write_file",
                        )),
                        None,
                    )
                    .await;
                actor
                    .send_update(
                        acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                            "private-note",
                            Default::default(),
                        )),
                        None,
                    )
                    .await;
                assert!(events.try_recv().is_err());
                actor
                    .send_update(
                        acp::SessionUpdate::ToolCall(acp::ToolCall::new("ordinary", "read_file")),
                        None,
                    )
                    .await;
                assert!(events.try_recv().is_ok());
                actor.rebuild_spec.active_sampling_config.write().provider =
                    xai_grok_sampling_types::ModelProvider::Xai;
                assert!(!actor.local_tool_allowed_for_provider(
                    "notes_read_file",
                    xai_grok_sampling_types::ModelProvider::Xai
                ));
                assert!(!store.enabled());
                assert!(store.operate("new_context", &Default::default()).is_err());
                for name in xai_grok_tools::implementations::codex::context_management::TOOL_NAMES {
                    assert!(crate::session::code_mode::is_code_mode_direct_only_tool(
                        name
                    ));
                }
            })
            .await;
    }

    #[test]
    fn experimental_context_configuration_is_explicit_and_off_by_default() {
        let disabled: crate::agent::config::Features = toml::from_str("").unwrap();
        assert!(!disabled.context_management.experimental_mode);
        let enabled: crate::agent::config::Features =
            toml::from_str("[context_management]\nexperimental_mode = true").unwrap();
        assert!(enabled.context_management.experimental_mode);
    }

    #[tokio::test]
    async fn fresh_window_requires_durable_checkpoint_and_preserves_a_concurrent_human_steer() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (gateway_tx, _gateway_rx) = tokio::sync::mpsc::unbounded_channel();
                let (persistence_tx, mut persistence_rx) = tokio::sync::mpsc::unbounded_channel();
                let (actor, _events) = super::super::support::create_test_actor_ex(
                    100,
                    10000,
                    80,
                    gateway_tx,
                    persistence_tx,
                )
                .await;
                actor
                    .chat_state_handle
                    .replace_conversation_for_compaction(vec![
                        ConversationItem::system("host instruction"),
                        ConversationItem::user("original task"),
                        ConversationItem::assistant("old assistant work"),
                    ]);
                let temp = tempfile::tempdir().unwrap();
                let store = ContextManagementStore::open(temp.path().to_owned()).unwrap();
                store.set_enabled(true);
                let old_window = store.window_id();
                {
                    let (_token, scope) = actor.compaction.cancel.enter();
                    actor.compaction.cancel.request_cancel();
                    assert!(
                        actor
                            .start_experimental_context_window(store.clone(), None, None, "manual")
                            .await
                            .is_err()
                    );
                    assert_eq!(store.window_id(), old_window);
                    drop(scope);
                }
                let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let fail_task = fail.clone();
                let history = actor.chat_state_handle.clone();
                let checkpoints = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let seen = checkpoints.clone();
                let handler = tokio::task::spawn_local(async move {
                    let mut steered = false;
                    while let Some(message) = persistence_rx.recv().await {
                        match message {
                            PersistenceMsg::CompactionCheckpoint(_) => {
                                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            PersistenceMsg::FlushAndAck { respond_to } => {
                                if fail_task.load(std::sync::atomic::Ordering::Relaxed) {
                                    let _ = respond_to
                                        .send(Err(std::io::Error::other("simulated disk failure")));
                                } else {
                                    if !steered {
                                        history.push_user_message(ConversationItem::user(
                                            "latest human correction",
                                        ));
                                        steered = true;
                                    }
                                    let _ = respond_to.send(Ok(()));
                                }
                            }
                            _ => {}
                        }
                    }
                });
                assert!(
                    actor
                        .start_experimental_context_window(store.clone(), None, None, "manual")
                        .await
                        .is_err()
                );
                assert_eq!(store.window_id(), old_window);
                assert!(
                    actor
                        .chat_state_handle
                        .get_conversation()
                        .await
                        .iter()
                        .any(|item| item.text_content() == "original task")
                );
                fail.store(false, std::sync::atomic::Ordering::Relaxed);
                actor
                    .start_experimental_context_window(store.clone(), None, None, "manual")
                    .await
                    .unwrap();
                let fresh = actor.chat_state_handle.get_conversation().await;
                assert!(
                    fresh
                        .iter()
                        .any(|item| matches!(item, ConversationItem::User(_))
                            && item.text_content() == "latest human correction")
                );
                assert!(
                    !fresh
                        .iter()
                        .any(|item| item.text_content() == "old assistant work")
                );
                assert_ne!(store.window_id(), old_window);
                assert!(checkpoints.load(std::sync::atomic::Ordering::Relaxed) >= 3);
                let recovered = store
                    .operate(
                        "history_search_contents",
                        &xai_grok_tools::implementations::codex::context_management::ContextInput {
                            query: Some("original task".into()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert_eq!(recovered["items"][0]["window_id"], old_window);
                handler.abort();
            })
            .await;
    }
    #[test]
    fn context_budget_tracks_manual_window_and_reserves_recovery_space() {
        assert_eq!(context_budget(950_000, None), (933_616, 16_384));
        assert_eq!(context_budget(950_000, Some(850_000)), (850_000, 16_384));
        assert_eq!(context_budget(8_000, None), (7_000, 1_000));
    }
    #[test]
    fn fresh_history_preserves_instructions_without_promoting_old_tool_content() {
        let original = vec![
            ConversationItem::system("host policy"),
            ConversationItem::project_instructions("project rules"),
            ConversationItem::user("original request"),
            ConversationItem::assistant("historical answer"),
            ConversationItem::interjection("latest steering"),
            ConversationItem::agent_message("peer suggestions are not human instructions"),
        ];
        let fresh = fresh_history(&original, "old-window");
        assert_eq!(fresh[0].text_content(), original[0].text_content());
        assert_eq!(fresh.len(), 5);
        assert_eq!(fresh[1].text_content(), "project rules");
        assert_eq!(fresh[3].text_content(), "latest steering");
        assert!(fresh[4].text_content().contains("old-window"));
        assert!(!fresh[4].text_content().contains("historical answer"));
        assert_eq!(normalized_history(&original)[2].role, "user");
        assert_eq!(
            normalized_history(&original).last().unwrap().role,
            "assistant"
        );
    }
}
