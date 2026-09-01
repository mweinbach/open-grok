use super::*;

use super::side_call::AuxCall;
use crate::session::helpers::{session_recap, session_summary};

#[cfg(test)]
mod tests {
    use super::super::support::{create_test_actor, run_on_session_sized_stack};
    use super::*;

    async fn title_actor(
        turns: usize,
        base_url: &str,
    ) -> (SessionActor, mpsc::UnboundedReceiver<PersistenceMsg>) {
        let (gateway_tx, _gateway_rx) = mpsc::unbounded_channel();
        let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
        let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
        actor.title_refresh_enabled = true;
        let mut config = actor.chat_state_handle.get_sampling_config().await.unwrap();
        config.base_url = base_url.to_owned();
        config.api_backend = crate::sampling::ApiBackend::ChatCompletions;
        actor.chat_state_handle.update_sampling_config(config);
        let mut conversation = vec![ConversationItem::system("You are a coding assistant.")];
        for turn_index in 0..turns {
            conversation.push(ConversationItem::user(format!("Question {turn_index}")));
            conversation.push(ConversationItem::assistant(format!("Answer {turn_index}")));
        }
        actor.chat_state_handle.replace_conversation(conversation);
        (actor, persistence_rx)
    }

    #[test]
    fn title_refresh_waits_for_checkpoint_and_preserves_conversation() {
        run_on_session_sized_stack(|| {
            Box::pin(async {
                let server = xai_grok_test_support::MockInferenceServer::start()
                    .await
                    .unwrap();
                server.set_response("Understand the project and implement requested changes");
                let (actor, mut persistence_rx) = title_actor(2, &server.url()).await;
                actor.refresh_title(0).await;
                assert_eq!(actor.next_title_refresh_idx.get(), 0);
                assert!(server.requests().is_empty());

                let mut conversation = actor.chat_state_handle.get_conversation().await;
                conversation.push(ConversationItem::user("Third question"));
                conversation.push(ConversationItem::assistant("Third answer"));
                actor
                    .chat_state_handle
                    .replace_conversation(conversation.clone());
                actor.refresh_title(0).await;
                assert_eq!(actor.next_title_refresh_idx.get(), 1);
                assert_eq!(
                    serde_json::to_value(actor.chat_state_handle.get_conversation().await).unwrap(),
                    serde_json::to_value(conversation).unwrap()
                );
                assert!(std::iter::from_fn(|| persistence_rx.try_recv().ok()).any(|message| {
                matches!(message, PersistenceMsg::RegenerateTitle(title) if title == "Understand the project and implement requested changes")
            }));
                let requests = server.requests();
                let body = requests
                    .iter()
                    .find_map(|request| request.body.as_ref())
                    .unwrap();
                assert!(
                    body.get("tools")
                        .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
                );
                let wire = body.to_string();
                assert!(wire.contains("Question 0") && wire.contains("Third question"));
                assert!(wire.contains("WHOLE conversation"));
                let request_count = requests.len();
                actor.refresh_title(0).await;
                assert_eq!(server.requests().len(), request_count);
            })
        });
    }

    #[test]
    fn title_refresh_failed_attempt_consumes_checkpoint_but_stale_attempt_does_not() {
        run_on_session_sized_stack(|| {
            Box::pin(async {
                let server = xai_grok_test_support::MockInferenceServer::start()
                    .await
                    .unwrap();
                server.set_response("");
                let (actor, mut persistence_rx) = title_actor(6, &server.url()).await;
                actor.refresh_title(0).await;
                assert_eq!(
                    actor.next_title_refresh_idx.get(),
                    session_summary::TITLE_REFRESH_TURNS.len()
                );
                assert!(
                    !std::iter::from_fn(|| persistence_rx.try_recv().ok())
                        .any(|message| { matches!(message, PersistenceMsg::RegenerateTitle(_)) })
                );
                actor.next_title_refresh_idx.set(0);
                actor.title_refresh_generation.set(1);
                server.set_response("A title from the stale generation");
                actor.refresh_title(0).await;
                assert_eq!(actor.next_title_refresh_idx.get(), 0);
                assert!(
                    !std::iter::from_fn(|| persistence_rx.try_recv().ok())
                        .any(|message| { matches!(message, PersistenceMsg::RegenerateTitle(_)) })
                );
            })
        });
    }

    #[test]
    fn title_refresh_has_one_inflight_task_and_manual_rename_wins() {
        run_on_session_sized_stack(|| {
            Box::pin(async {
                let (actor, _persistence_rx) = title_actor(3, "http://localhost").await;
                let actor = Arc::new(actor);
                let pending_task = tokio::task::spawn_local(std::future::pending::<()>());
                let pending_id = pending_task.id();
                *actor.title_refresh_task.borrow_mut() = Some(pending_task);
                actor.maybe_refresh_title();
                assert_eq!(
                    actor.title_refresh_task.borrow().as_ref().unwrap().id(),
                    pending_id
                );
                actor.on_title_renamed(true);
                assert!(actor.title_refresh_task.borrow().is_none());
                assert_eq!(
                    actor.next_title_refresh_idx.get(),
                    session_summary::TITLE_REFRESH_TURNS.len()
                );
                assert_eq!(actor.title_refresh_generation.get(), 1);
                actor.maybe_refresh_title();
                assert!(actor.title_refresh_task.borrow().is_none());
                actor.on_title_renamed(false);
                assert_eq!(actor.next_title_refresh_idx.get(), 0);
                assert_eq!(actor.title_refresh_generation.get(), 2);
            })
        });
    }
}

const TITLE_REFRESH_MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);

impl SessionActor {
    pub(crate) fn maybe_refresh_title(self: &Arc<Self>) {
        if !self.title_refresh_enabled || self.startup_hints.is_subagent {
            return;
        }
        if self.next_title_refresh_idx.get() >= session_summary::TITLE_REFRESH_TURNS.len() {
            return;
        }
        if self
            .title_refresh_task
            .borrow()
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return;
        }
        let generation = self.title_refresh_generation.get();
        let actor = self.clone();
        let task = tokio::task::spawn_local(async move {
            actor.refresh_title(generation).await;
            if actor.title_refresh_generation.get() == generation {
                *actor.title_refresh_task.borrow_mut() = None;
            }
        });
        *self.title_refresh_task.borrow_mut() = Some(task);
    }

    pub(crate) fn on_title_renamed(&self, manual: bool) {
        self.abort_title_refresh();
        let idx = if manual {
            session_summary::TITLE_REFRESH_TURNS.len()
        } else {
            0
        };
        self.next_title_refresh_idx.set(idx);
        session_summary::save_title_refresh_watermark(
            &crate::session::persistence::session_dir(&self.session_info),
            idx,
        );
    }

    pub(crate) fn abort_title_refresh(&self) {
        self.title_refresh_generation
            .set(self.title_refresh_generation.get().wrapping_add(1));
        if let Some(task) = self.title_refresh_task.borrow_mut().take() {
            task.abort();
        }
    }

    async fn refresh_title(&self, generation: u64) {
        let conversation = self.chat_state_handle.get_conversation().await;
        let turns = session_recap::main_turn_count(&conversation);

        let idx = self.next_title_refresh_idx.get();
        let target_idx = session_summary::checkpoints_reached(turns);
        if target_idx <= idx {
            return;
        }

        let title = self.generate_refreshed_title(conversation).await;

        if self.title_refresh_generation.get() != generation {
            return;
        }
        self.next_title_refresh_idx.set(target_idx);
        session_summary::save_title_refresh_watermark(
            &crate::session::persistence::session_dir(&self.session_info),
            target_idx,
        );
        if let Some(title) = title {
            tracing::info!(turns, chars = title.len(), "session title refreshed");
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::RegenerateTitle(title));
        }
    }

    async fn generate_refreshed_title(
        &self,
        conversation: Vec<ConversationItem>,
    ) -> Option<String> {
        let setup = match self
            .prepare_auxiliary_sampling(AuxiliaryModelPurpose::TitleRefresh, None)
            .await
        {
            Ok(setup) => setup,
            Err(error) => {
                tracing::warn!(error = %error, "title refresh: failed to prepare sampling client");
                return None;
            }
        };
        let instruction = session_summary::title_refresh_instruction(self.reminder_wrapper_tag());
        let items = session_recap::budget_instruction_items(
            conversation,
            instruction,
            setup.client.api_backend().requires_reasoning_strip(),
            setup.context_window,
        );
        let request = self.parent_cached_request(AuxCall {
            items,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            model: setup.model.clone(),
            reasoning_effort: setup.reasoning_effort,
            backend: setup.client.api_backend(),
            conv_id: format!("title-refresh-{}", uuid::Uuid::new_v4()),
            req_id: format!("xai-title-refresh-{}", uuid::Uuid::new_v4()),
        });

        let response = match tokio::time::timeout(
            TITLE_REFRESH_MODEL_TIMEOUT,
            setup.client.conversation_collect(request),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "title refresh: model call failed");
                return None;
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = TITLE_REFRESH_MODEL_TIMEOUT.as_secs(),
                    "title refresh: model call timed out"
                );
                return None;
            }
        };
        super::side_call::log_prompt_cache_hit(
            "title_refresh",
            setup.client.api_backend(),
            &response,
        );
        let title = session_summary::clean_title_text(&response.assistant_text());
        if title.is_empty() {
            tracing::debug!("title refresh: model returned empty title");
            return None;
        }
        Some(title)
    }
}
