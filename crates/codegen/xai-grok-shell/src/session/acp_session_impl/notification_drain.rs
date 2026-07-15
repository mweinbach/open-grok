//! Idle-gated pending-notification buffering and drain for `SessionActor`,
//! plus auto-start of queued prompts (`maybe_start_running_task`).

use super::*;

/// Maximum number of pending notifications before oldest are dropped.
pub(super) const MAX_PENDING_NOTIFICATIONS: usize = 50;

/// A notification buffered for idle-gated drain (see `maybe_drain_notifications`).
pub(crate) struct PendingNotification {
    #[expect(
        dead_code,
        reason = "Retained for debugging / future per-notification tracing."
    )]
    pub(crate) prompt_id: String,
    pub(crate) prompt_blocks: Vec<acp::ContentBlock>,
    pub(crate) priority: NotificationPriority,
    pub(crate) source: NotificationSource,
}

impl SessionActor {
    pub(super) async fn maybe_start_running_task(
        self: Arc<Self>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        let mut state = self.state.lock().await;
        if state.running_task.is_some() {
            let queue_depth = state.pending_inputs.len();
            if queue_depth > 0 {
                xai_grok_telemetry::unified_log::debug(
                    "shell.prompt.start_blocked",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({
                        "reason": "task_already_running",
                        "queue_depth": queue_depth,
                    })),
                );
                tracing::debug!(
                    target: "qtrace",
                    pid = std::process::id(),
                    event = "server_start_blocked",
                    queue_depth,
                    front_prompt_id = state
                        .pending_inputs
                        .front()
                        .map(|i| i.prompt_id.as_str())
                        .unwrap_or(""),
                    session = self.session_info.id.0.as_ref(),
                    "maybe_start_running_task blocked: a turn is already running",
                );
            }
            return;
        }

        // Note: Auto-compact is now handled inline during process_conversation_turn,
        // so we no longer need to check for queued auto-compact here.

        // Start the next pending user prompt. Pull all needed fields from the
        // queue head in one `front_mut` scope so we can mutate `state` again
        // (e.g. `rewindable`) without overlapping borrows.
        let (
            persist_ack,
            parsed_prompt_tx,
            prompt_id,
            prompt_blocks,
            prompt_mode,
            trace_gcs_config,
            artifact_tracker,
            client_identifier,
            screen_mode,
            verbatim,
            json_schema,
        ) = {
            let Some(front) = state.pending_inputs.front_mut() else {
                return;
            };
            (
                front.persist_ack.take(),
                front.parsed_prompt_tx.take(),
                front.prompt_id.clone(),
                front.prompt_blocks.clone(),
                front.prompt_mode,
                front.trace_gcs_config.clone(),
                front.artifact_tracker.clone(),
                front.client_identifier.clone(),
                front.screen_mode.clone(),
                front.verbatim,
                front.json_schema.clone(),
            )
        };
        {
            let mut current_prompt_id = self
                .current_prompt_id
                .lock()
                .expect("current_prompt_id mutex poisoned");
            *current_prompt_id = Some(prompt_id.clone());
        }
        state.rewindable = true;
        self.agent
            .borrow()
            .tool_bridge()
            .update_resource(
                xai_grok_tools::implementations::grok_build::task::types::CurrentPromptIdResource(
                    prompt_id.clone(),
                ),
            )
            .await;

        tracing::debug!(
            target: "qtrace",
            pid = std::process::id(),
            event = "server_promote",
            prompt_id = %prompt_id,
            remaining_queued = state.pending_inputs.len().saturating_sub(1),
            session = self.session_info.id.0.as_ref(),
            "promoting front of pending_inputs to the running turn",
        );
        state.running_task = Some(AgentTask::new_prompt(
            self.clone(),
            prompt_id,
            prompt_blocks,
            prompt_mode,
            trace_gcs_config,
            artifact_tracker,
            client_identifier,
            screen_mode,
            verbatim,
            json_schema,
            completion_tx,
            persist_ack,
            parsed_prompt_tx,
        ));
        // The front prompt is now the in-flight turn; re-broadcast so the
        // shared queue drops it from the pending list.
        self.broadcast_queue_changed(&state);
    }

    /// Drain pending notifications into a single batched turn, if idle and not suppressed.
    ///
    /// Guards:
    /// - No turn is running (`running_task` is `None`)
    /// - No user prompts are pending (user prompts always take priority)
    /// - Notifications are NOT suppressed (cleared on next user prompt)
    ///
    /// All notifications are taken and merged into a single `InputItem` with
    /// `---` separators between content blocks. The take+push happens in a
    /// single lock acquisition to avoid interleaving.
    pub(super) async fn maybe_drain_notifications(
        self: Arc<Self>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        // Auto-wake notification turns are DROPPED both while the goal loop is
        // active (a bg-task / monitor "completed" turn would pull a weak model
        // off the goal continuation, e.g. relaunching a killed server) AND
        // after the goal completes (the autonomous run is over — late dev-
        // server completions should leave the session idle, not spawn fresh
        // post-goal turns). Independently, completions whose source task
        // originated during the goal turn are dropped regardless of status (see
        // `split_goal_suppressed`). Dropped notifications are still marked
        // reported below so nothing resurfaces later.
        let suppress_all = self.goal_harness_enabled()
            && matches!(
                self.goal_tracker.lock().status(),
                Some(
                    crate::session::goal_tracker::GoalStatus::Active
                        | crate::session::goal_tracker::GoalStatus::Complete
                )
            );

        let drained_task_ids: Vec<String>;

        let drained = {
            let mut state = self.state.lock().await;

            // Shared idle predicate — same conditions Layer 3 uses via
            // `is_session_idle_for_injection`. Inlined here so the
            // `mut state` borrow can survive into the take/push below.
            if !is_session_idle_for_injection(&state) {
                return;
            }

            // Backstop sweep for events that hit the buffer after the
            // turn-end drain (the is_turn_active flag can lag the actual
            // turn teardown). Normally a no-op.
            self.sweep_monitor_buffer_into_pending(&mut state, "monitor-idle-drain");

            // Nothing to drain
            if state.pending_notifications.is_empty() {
                return;
            }

            // Take all notifications and build merged blocks inside the lock
            let notifications = std::mem::take(&mut state.pending_notifications);

            drained_task_ids = notifications
                .iter()
                .map(|n| n.source.task_id().to_string())
                .collect();

            let (to_surface, dropped) = {
                let goal_turn_task_ids = self.goal_turn_task_ids.lock();
                Self::split_goal_suppressed(suppress_all, &goal_turn_task_ids, notifications)
            };
            if dropped > 0 {
                tracing::info!(
                    dropped,
                    suppress_all,
                    "dropping suppressed pending notifications (goal active/complete or goal-turn origin)"
                );
            }

            if to_surface.is_empty() {
                false
            } else {
                Self::drain_notifications_into_turn(
                    &mut state,
                    to_surface,
                    &self.tool_context.task_output_tool_name,
                )
            }
        };
        // Mark reported whether dropped or surfaced, so the per-tool-call
        // `TaskCompletionReminder` won't resurface the same completions.
        let ids: Vec<&str> = drained_task_ids.iter().map(String::as_str).collect();
        self.mark_completions_reported(&ids).await;

        if drained {
            SessionActor::maybe_start_running_task(self, completion_tx).await;
        }
    }

    /// Notifies extensions when the session settles idle (nothing running, nothing queued).
    /// The idle check stays host-side; extensions only get the event.
    pub(super) async fn emit_session_idle_if_idle(&self) {
        {
            let state = self.state.lock().await;
            if !is_session_idle_for_injection(&state) {
                return;
            }
        }
        for contributor in self.extension_registry.session_lifecycle_contributors() {
            contributor
                .on_session_idle(&xai_agent_lifecycle::SessionIdleInput)
                .await;
        }
    }

    /// Sweep this session's buffered monitor events (`drain_owned`) into
    /// `pending_notifications`. Used where the turn loop can no longer
    /// drain the buffer: turn end (`drain_monitor_buffer_to_pending`),
    /// turn cancel, and the idle drain (all three race the
    /// `is_turn_active`-gated buffer push in `InjectNotification`).
    pub(super) fn sweep_monitor_buffer_into_pending(
        &self,
        state: &mut State,
        prompt_id_prefix: &str,
    ) {
        let Some(buffer) = &self.tool_context.monitor_event_buffer else {
            return;
        };
        for event in xai_grok_tools::implementations::grok_build::task::types::drain_owned(
            buffer,
            Some(self.session_info.id.0.as_ref()),
        ) {
            state.pending_notifications.push(PendingNotification {
                prompt_id: format!("{prompt_id_prefix}-{}", uuid::Uuid::now_v7()),
                prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                    event.event_text,
                ))],
                priority: NotificationPriority::Next,
                source: NotificationSource::MonitorEvent {
                    task_id: event.task_id,
                },
            });
        }
    }

    /// Partition drained notifications into `(to_surface, dropped_count)`.
    ///
    /// `suppress_all` mirrors the goal Active/Complete blanket gate (drop
    /// everything); independently, notifications whose source task is in
    /// `goal_turn_task_ids` are always dropped (see that field).
    pub(super) fn split_goal_suppressed(
        suppress_all: bool,
        goal_turn_task_ids: &std::collections::HashSet<String>,
        notifications: Vec<PendingNotification>,
    ) -> (Vec<PendingNotification>, usize) {
        if suppress_all {
            let dropped = notifications.len();
            return (Vec::new(), dropped);
        }
        let mut dropped = 0usize;
        let to_surface = notifications
            .into_iter()
            .filter(|n| {
                let keep = !goal_turn_task_ids.contains(n.source.task_id());
                if !keep {
                    dropped += 1;
                }
                keep
            })
            .collect();
        (to_surface, dropped)
    }

    /// Build the merged `NotificationDrain` `InputItem` from `notifications`
    /// and push it onto `state.pending_inputs`. Always returns `true` so the
    /// caller starts the running task.
    ///
    /// Monitor-event notifications are collapsed into ONE
    /// `format_monitor_events` block (same batched/deduped shape as the
    /// mid-turn injection — `<monitor task_id=…>` groups with `[label N]`
    /// lines), placed at the position of the first monitor entry. Other
    /// notifications keep their raw blocks. Everything joins with `---`.
    pub(super) fn drain_notifications_into_turn(
        state: &mut State,
        notifications: Vec<PendingNotification>,
        task_output_tool_name: &str,
    ) -> bool {
        use xai_grok_tools::implementations::grok_build::task::types::MonitorEventNotification;

        // Collapse monitor entries: collect their text into events, remember
        // where the first one sat so the batch lands in arrival position.
        let mut monitor_events: Vec<MonitorEventNotification> = Vec::new();
        let mut sections: Vec<Vec<acp::ContentBlock>> = Vec::new();
        let mut monitor_section_idx: Option<usize> = None;
        for notif in &notifications {
            match &notif.source {
                NotificationSource::MonitorEvent { task_id } => {
                    let event_text = notif
                        .prompt_blocks
                        .iter()
                        .filter_map(|b| match b {
                            acp::ContentBlock::Text(t) => Some(t.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    monitor_events.push(MonitorEventNotification {
                        task_id: task_id.clone(),
                        event_text,
                        owner_session_id: None,
                    });
                    if monitor_section_idx.is_none() {
                        monitor_section_idx = Some(sections.len());
                        sections.push(Vec::new()); // placeholder, filled below
                    }
                }
                NotificationSource::BashTaskCompleted { .. } => {
                    sections.push(notif.prompt_blocks.clone());
                }
            }
        }
        if let (Some(idx), Some(batch)) = (
            monitor_section_idx,
            xai_grok_tools::reminders::task_completion::format_monitor_events(
                &monitor_events,
                Some(task_output_tool_name),
            ),
        ) {
            sections[idx] = vec![acp::ContentBlock::Text(acp::TextContent::new(batch))];
        }

        let mut merged_blocks: Vec<acp::ContentBlock> = Vec::new();
        for (i, section) in sections.iter().enumerate() {
            if i > 0 {
                merged_blocks.push(acp::ContentBlock::Text(acp::TextContent::new(
                    "---".to_string(),
                )));
            }
            merged_blocks.extend(section.iter().cloned());
        }

        let merged_prompt_id = format!("notifications-{}", uuid::Uuid::now_v7());

        // Receiver intentionally dropped — notification turns have no caller
        // awaiting the result. The send() in handle_completion returns Err,
        // which is harmless.
        let (respond_to, _) = tokio::sync::oneshot::channel();

        state.pending_inputs.push_back(InputItem {
            prompt_id: merged_prompt_id,
            prompt_blocks: merged_blocks,
            prompt_mode: crate::session::plan_mode::PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: true,
            json_schema: None,
            origin: super::PromptOrigin::NotificationDrain,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            queue_meta: None,
            send_now: false,
        });

        tracing::info!(
            count = notifications.len(),
            next_count = notifications.iter().filter(|n| n.priority == NotificationPriority::Next).count(),
            later_count = notifications.iter().filter(|n| n.priority == NotificationPriority::Later).count(),
            sources = %notifications.iter().map(|n| match &n.source {
                NotificationSource::MonitorEvent { task_id } => format!("monitor:{task_id}"),
                NotificationSource::BashTaskCompleted { task_id } => format!("bash:{task_id}"),
            }).collect::<Vec<_>>().join(","),
            "Drained pending notifications into single batched turn"
        );

        true
    }

    /// Turn-end straggler sweep: monitor events buffered during the turn's
    /// final sampling step (after the loop's last `inject_pending_monitor_events`
    /// pass) move to `pending_notifications`. Runs in the completion handler
    /// before `maybe_drain_notifications`, so it — not the idle sweep — is
    /// what normally catches them.
    pub(super) async fn drain_monitor_buffer_to_pending(&self) {
        let mut state = self.state.lock().await;
        self.sweep_monitor_buffer_into_pending(&mut state, "monitor-turn-end-drain");
    }
}
