//! Prompt-task plumbing for `SessionActor` (`AgentTask`, `TaskSlot`,
//! `run_task`, turn guards) and the cancel paths.

use super::*;

/// Whether the post-cancel notification drain stays suppressed: rewind and non-stop cancels
/// clear it, a stop gesture arms it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WakeBarrier {
    Armed,
    Clear,
}

/// Outcome of `cancel_running_task`.
#[must_use = "the notification drain and the idle ping both depend on this"]
pub(super) struct CancelOutcome {
    pub(super) barrier: WakeBarrier,
    /// A user cancel with a classified reason tore down a turn, a rewind included. Whether that
    /// leaves the session idle is the loop's call.
    pub(super) turn_stopped: bool,
    pub(super) settled: bool,
}

impl CancelOutcome {
    fn noop() -> Self {
        Self {
            barrier: WakeBarrier::Clear,
            turn_stopped: false,
            settled: false,
        }
    }
}

enum CancelFinalization {
    Rewind,
    Keep(FinalizationLease),
}

fn front_is_rewind_poppable(front: Option<&InputItem>) -> bool {
    front.is_some_and(|input| {
        matches!(input.origin, crate::session::PromptOrigin::User)
            && !super::interjection::is_interject_fallback(&input.prompt_id)
    })
}

/// What a cancel needs to report the turn it tore down. `epoch` is captured with the task under
/// the state lock, so the report names that turn rather than whichever is current when it files.
pub(super) struct CancelledTurn<'a> {
    pub(super) prompt_id: &'a str,
    pub(super) epoch: TurnEpoch,
    pub(super) reason: xai_grok_hooks::event::StopCancelledReason,
    pub(super) trigger: Option<String>,
    pub(super) last_assistant_message: Option<String>,
}

impl SessionActor {
    /// Turn-scoped: soft cancel / max-turns only (not user Stop).
    /// `parent_prompt_id` is the authoritative turn id from the turn runner.
    pub(super) fn cancel_running_turn_subagents(&self, parent_prompt_id: &str) {
        self.cancel_subagents_for_prompt_id(parent_prompt_id);
    }

    /// User Stop with cancel_subagents: all non-workflow session children.
    /// Uses the session-bound backend API so cancel never wildcards other sessions.
    pub(super) fn cancel_all_session_subagents(&self) {
        if let Some(event_tx) = self.tool_context.subagent_event_tx.clone() {
            use xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend;
            let backend = ChannelBackend::for_session(event_tx, self.session_id_string());
            let _ = backend.request_cancel_parent_session(tokio::sync::oneshot::channel().0);
        }
    }

    /// Re-open Task spawns for this session after a prior user Stop.
    pub(super) fn open_subagent_spawn_admission(&self) {
        if let Some(event_tx) = self.tool_context.subagent_event_tx.clone() {
            use xai_grok_tools::implementations::grok_build::task::backend::ChannelBackend;
            let backend = ChannelBackend::for_session(event_tx, self.session_id_string());
            let _ = backend.open_spawn_admission();
        }
    }

    fn cancel_subagents_for_prompt_id(&self, parent_prompt_id: &str) {
        if let Some(event_tx) = self.tool_context.subagent_event_tx.clone() {
            use xai_grok_tools::implementations::grok_build::task::types::{
                SubagentCancelRequest, SubagentCancelTarget, SubagentEvent,
            };
            let _ = event_tx.send(SubagentEvent::Cancel(SubagentCancelRequest {
                parent_session_id: Some(self.session_id_string()),
                target: SubagentCancelTarget::ParentPromptId(parent_prompt_id.to_string()),
                respond_to: tokio::sync::oneshot::channel().0,
            }));
        }
    }

    /// Takes the state lock with `try_lock`, so it must run before its caller's first await: a
    /// task suspended across one still holds the guard.
    fn arm_wake_barrier(&self, trigger: Option<&crate::session::CancelTrigger>) {
        if let Some(gate) = &self.tool_context.task_wake_suppressed {
            gate.set(true);
        }
        let mut state = self.state.try_lock().expect("session state is actor-owned");
        state.notifications_suppressed = true;
        xai_grok_telemetry::unified_log::info(
            "shell.task_wake.cancel_barrier",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "trigger": trigger.map(crate::session::CancelTrigger::as_str),
                "gate": self
                    .tool_context
                    .task_wake_suppressed
                    .as_ref()
                    .is_some_and(|gate| gate.get()),
                "state": state.notifications_suppressed,
            })),
        );
        drop(state);
        if let Some(is_turn_active) = &self.tool_context.is_turn_active {
            is_turn_active.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Releases the gate's claim here rather than at the aborted task's drop, which runs too late
    /// for the cancel to report. Callers read `epoch` under the lock they take the task from.
    fn abort_turn_task(&self, task: &AgentTask, epoch: super::turn_report_slot::TurnEpoch) {
        task.abort();
        self.turn_report.release_aborted(epoch);
    }

    /// Cancel the running turn for a send-now prompt: Ctrl+C parity (kills the
    /// turn's foreground command; background tasks, subagents, and the queue
    /// survive). Flushes the replay buffer before teardown so streamed chunks
    /// persist; the caller's `maybe_start_running_task` then promotes the new
    /// front (a flushed interjection fallback runs ahead of the send-now prompt).
    pub(super) async fn cancel_turn_for_send_now(
        &self,
        replay_buffer: &mut crate::agent::update_chunk_merge::ReplayBuffer,
    ) -> CancelOutcome {
        if let Some(notification) = replay_buffer.flush() {
            self.emit_buffered(notification).await;
        }
        // Flush, don't clear: the pager already painted the text and said "Interjection sent".
        let flushed = self.flush_stranded_interjections().await;
        if flushed > 0 {
            xai_grok_telemetry::unified_log::info(
                "shell.prompt.send_now_flushed_interjections",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({ "count": flushed })),
            );
        }
        // Cancel-and-send replaces the turn rather than ending it, so it reports nothing.
        let outcome = self
            .cancel_running_task(crate::session::CancelOptions {
                trigger: Some(crate::session::CancelTrigger::SendNow),
                ..Default::default()
            })
            .await;
        if !outcome.settled {
            return outcome;
        }
        // Re-enable notification drains: unlike Ctrl+C, a send-now means the user is re-engaged.
        if let Some(gate) = &self.tool_context.task_wake_suppressed {
            gate.set(false);
        }
        {
            let mut state = self.state.lock().await;
            state.notifications_suppressed = false;
            state.take_hook_block_hold();
        }
        xai_grok_telemetry::unified_log::info(
            "shell.task_wake.gate_cleared",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({ "reason": "send_now" })),
        );
        outcome
    }

    /// Names the turn this cancel tore down, so a turn promoted in between is refused rather
    /// than reported over.
    fn report_cancelled_turn(&self, cancelled: CancelledTurn<'_>) {
        self.claim_and_queue(
            cancelled.prompt_id,
            cancelled.epoch,
            TurnEnd::Cancelled {
                reason: cancelled.reason,
                trigger: cancelled.trigger,
                reason_details: None,
                last_assistant_message: cancelled.last_assistant_message,
            },
        );
    }

    fn log_rewind_decision(
        &self,
        requested_prompt_id: Option<&str>,
        front_prompt_id: Option<&str>,
        rewind_disposition: &str,
    ) {
        xai_grok_telemetry::unified_log::info(
            "shell.cancel.rewind_decision",
            Some(self.session_info.id.0.as_ref()),
            Some(serde_json::json!({
                "requested_prompt_id": requested_prompt_id,
                "front_prompt_id": front_prompt_id,
                "rewind_disposition": rewind_disposition,
            })),
        );
    }

    async fn finish_rewound_cancel(
        &self,
        input: InputItem,
        target_prompt_index: Option<usize>,
        total_tokens: u64,
        turn_stopped: bool,
    ) -> CancelOutcome {
        let _ = self.rebuild_spec.code_mode_runtime.reset().await;
        if let Some(mut snapshot) = self.chat_state_handle.snapshot().await {
            let target_prompt_index =
                target_prompt_index.unwrap_or_else(|| snapshot.prompt_index.saturating_sub(1));
            snapshot.prompt_index = target_prompt_index;
            snapshot.prompt_texts.truncate(target_prompt_index);
            let keep_count =
                conversation_truncate_for_prompt(&snapshot.conversation, target_prompt_index);
            snapshot.conversation.truncate(keep_count);
            self.chat_state_handle.restore_snapshot(snapshot);
            self.file_state_tracker
                .truncate_from(target_prompt_index)
                .await;
        }
        let _ = input.respond_to.send(Ok(PromptTurnOk {
            stop_reason: acp::StopReason::Cancelled,
            total_tokens,
            turn_snapshot: None,
            completion_kind: PromptCompletionKind::Rewound,
            structured_output: None,
            usage: None,
            tool_overrides: self.effective_tool_overrides(),
        }));
        self.maybe_apply_pending_web_search_reload().await;
        CancelOutcome {
            barrier: WakeBarrier::Clear,
            turn_stopped,
            settled: true,
        }
    }

    pub(super) async fn cancel_running_task(
        &self,
        options: crate::session::CancelOptions,
    ) -> CancelOutcome {
        let kind = options
            .trigger
            .as_ref()
            .map(crate::session::CancelTrigger::kind);
        let cancel_reason = super::turn_end_hooks::cancel_reason_for_options(&options);
        let crate::session::CancelOptions {
            cancel_subagents,
            kill_background_tasks,
            history,
            trigger,
            user_initiated,
        } = options;
        let rewind_requested = matches!(
            history,
            crate::session::CancelHistoryDisposition::RewindIfNoOutput { .. }
        );
        let requested_prompt_id = match &history {
            crate::session::CancelHistoryDisposition::RewindIfNoOutput { prompt_id } => {
                prompt_id.clone()
            }
            crate::session::CancelHistoryDisposition::Keep => None,
        };
        let mut claimed_rewound: Option<(InputItem, TurnEpoch, usize)> = None;
        if let Some(requested) = requested_prompt_id.as_deref() {
            let mut state = self.state.lock().await;
            let front_prompt_id = state
                .pending_inputs
                .front()
                .map(|input| input.prompt_id.clone());
            if front_prompt_id.as_deref() != Some(requested) {
                drop(state);
                self.log_rewind_decision(
                    Some(requested),
                    front_prompt_id.as_deref(),
                    "stale_prompt_id",
                );
                return CancelOutcome::noop();
            }
            let poppable = front_is_rewind_poppable(state.pending_inputs.front());
            let rewind_disposition = if state.rewindable && poppable {
                self.sweep_monitor_buffer_into_pending(&mut state, "monitor-cancel-drain");
                let turn_epoch = self.turn_report.epoch();
                if let Some(task) = state
                    .running_task
                    .take_if(|task| task.prompt_id == requested)
                {
                    self.abort_turn_task(&task, turn_epoch);
                    self.cancel_active_sampling_requests();
                }
                if let Some(gate) = &self.tool_context.task_wake_suppressed {
                    gate.set(false);
                }
                state.notifications_suppressed = false;
                xai_grok_telemetry::unified_log::info(
                    "shell.task_wake.gate_cleared",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({ "reason": "rewind" })),
                );
                state.rewindable = false;
                let target_prompt_index = self
                    .chat_state_handle
                    .get_prompt_index()
                    .await
                    .saturating_sub(1);
                claimed_rewound = state
                    .pending_inputs
                    .pop_front()
                    .map(|input| (input, turn_epoch, target_prompt_index));
                "rewound"
            } else if !state.rewindable {
                "window_closed"
            } else {
                "non_user_front"
            };
            drop(state);
            self.log_rewind_decision(
                Some(requested),
                front_prompt_id.as_deref(),
                rewind_disposition,
            );
        }
        if let Some((input, epoch, target_prompt_index)) = claimed_rewound {
            let _strip_guard = self.prepare_image_strips_for_rewind().await;
            self.cancel_active_sampling_requests();
            self.cancel_pending_image_strips_for_rewind();
            self.notify_turn_abort(epoch, xai_agent_lifecycle::TurnAbortReason::Interrupted)
                .await;
            let total_tokens = self.chat_state_handle.get_total_tokens().await;
            return self
                .finish_rewound_cancel(
                    input,
                    Some(target_prompt_index),
                    total_tokens,
                    cancel_reason.is_some(),
                )
                .await;
        }
        let mut finalization = if rewind_requested {
            CancelFinalization::Rewind
        } else {
            let Some(lease) = self.state.lock().await.claim_cancel_finalization() else {
                return CancelOutcome::noop();
            };
            CancelFinalization::Keep(lease)
        };
        let claimed_turn_epoch = match &finalization {
            CancelFinalization::Rewind => None,
            CancelFinalization::Keep(lease) => lease.binding.epoch(),
        };
        let suppress_task_wakes = kind == Some(crate::session::CancelKind::StopGesture);
        // Abort in-flight `/compact` or auto-compact generation (stream select +
        // pre-replace guard). Safe when no compact is running.
        self.compaction.cancel.request_cancel();
        if suppress_task_wakes {
            self.arm_wake_barrier(trigger.as_ref());
        }

        // Unified-log processing marker (counterpart of `shell.cancel.received`
        // in `MvpAgent::cancel`): records which prompt the cancel lands on so
        // a stuck "Cancelling…" can be attributed to delivery vs. processing.
        // A pin snapshot only — the authoritative cancel identity is captured
        // from `running_task.prompt_id` under the state lock below, because
        // `current_prompt_id` is cleared early (turn scope guard drop /
        // `handle_completion`) while the finished front and its task slot are
        // still queued; keying the durable `TurnCompleted` on the pin alone
        // loses the terminal (and its `cancelTrigger`) in that window.
        let pinned_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        {
            xai_grok_telemetry::unified_log::info(
                "shell.cancel.processing",
                Some(self.session_info.id.0.as_ref()),
                Some(serde_json::json!({
                    "prompt_id": &pinned_prompt_id,
                    "cancel_subagents": cancel_subagents,
                    "kill_background_tasks": kill_background_tasks,
                    "rewind_if_no_output": rewind_requested,
                    "trigger": trigger.as_ref().map(crate::session::CancelTrigger::as_str),
                })),
            );
        }

        let cancel_elapsed_ms = self
            .state
            .lock()
            .await
            .running_task
            .as_ref()
            .map(|task| elapsed_ms_saturating(task.started_at, std::time::Instant::now()));
        if cancel_subagents {
            // Abort the producer first so it cannot enqueue more TaskTool work
            // after the ParentSession sweep. Keep the task slot for prompt-id
            // attribution below; cleanup will take+abort again (idempotent).
            {
                let state = self.state.lock().await;
                if let Some(task) = state.running_task.as_ref()
                    && let Some(epoch) = claimed_turn_epoch
                {
                    self.abort_turn_task(task, epoch);
                    self.cancel_active_sampling_requests();
                }
            }
            // Then cancel every non-workflow session child (incl. prior turns)
            // and close spawn admission until the next turn opens it.
            self.cancel_all_session_subagents();
        }

        // After the abort, so this chat-state round trip cannot keep the turn task alive, and
        // before the teardown edits the text.
        let last_assistant_message = if cancel_reason.is_some() {
            self.last_assistant_message_for_cancel().await
        } else {
            None
        };

        // A rewind is not a cancel either: the turn is being replaced, not stopped.
        if user_initiated && !rewind_requested {
            self.signals_handle().record_cancellation();
        }

        // Kill all running foreground terminal processes before aborting the task.
        // Each TerminalBackend implementation knows how to kill its own processes.
        // Background tasks are left alive for interactive sessions but killed
        // during subagent teardown (kill_background_tasks = true).
        //
        // Note: a narrow TOCTOU window exists — the running task could spawn a
        // new terminal between this call and the abort() below. In practice this
        // is negligible; abort() drops the future and any child handle it owns.
        let send_now = matches!(trigger, Some(crate::session::CancelTrigger::SendNow));
        if !send_now {
            self.kill_foreground_commands_for_cancel().await;
        }

        if kill_background_tasks {
            if self.startup_hints.is_subagent {
                // Subagent teardown: only kill tasks owned by this session,
                // not the parent's or sibling's tasks on the shared backend.
                self.agent
                    .borrow()
                    .tool_bridge()
                    .kill_all_background_tasks_by_owner(&self.session_info.id.0)
                    .await;
            } else {
                self.agent
                    .borrow()
                    .tool_bridge()
                    .kill_all_background_tasks()
                    .await;
            }
        }

        let total_tokens = self.chat_state_handle.get_total_tokens().await;
        let (
            cancelled_prompt_id,
            pending_inputs,
            rewound_input,
            had_queued_user_prompt,
            turn_epoch,
        ) = {
            let mut state = self.state.lock().await;
            debug_assert!(
                pinned_prompt_id.is_none()
                    || state.running_prompt_id().is_none()
                    || state.running_prompt_id() == pinned_prompt_id.as_deref(),
                "current_prompt_id pin disagrees with running_task identity"
            );

            if let CancelFinalization::Keep(lease) = &finalization
                && !state.finalization_binding_is_current(lease)
            {
                tracing::error!(
                    binding = ?lease.binding,
                    "claimed cancel lost its exact finalization binding"
                );
                if suppress_task_wakes {
                    if let Some(gate) = &self.tool_context.task_wake_suppressed {
                        gate.set(false);
                    }
                    state.notifications_suppressed = false;
                }
                return CancelOutcome::noop();
            }

            self.sweep_monitor_buffer_into_pending(&mut state, "monitor-cancel-drain");

            if kill_background_tasks {
                state.clear_pending_notifications();
            }

            let front_is_user_row = front_is_rewind_poppable(state.pending_inputs.front());
            let front_prompt_id = state
                .pending_inputs
                .front()
                .map(|input| input.prompt_id.clone());
            let identity_matches = requested_prompt_id
                .as_deref()
                .is_none_or(|id| front_prompt_id.as_deref() == Some(id));
            let was_rewindable = state.rewindable;
            let turn_epoch =
                claimed_turn_epoch.or_else(|| rewind_requested.then(|| self.turn_report.epoch()));
            let mut rewound_task_prompt_id = None;
            let rewound_input = if rewind_requested
                && requested_prompt_id.is_none()
                && state.rewindable
                && front_is_user_row
            {
                if let Some(task) = state.running_task.take()
                    && let Some(epoch) = turn_epoch
                {
                    self.abort_turn_task(&task, epoch);
                    rewound_task_prompt_id = Some(task.prompt_id);
                }
                if let Some(gate) = &self.tool_context.task_wake_suppressed {
                    gate.set(false);
                }
                state.notifications_suppressed = false;
                xai_grok_telemetry::unified_log::info(
                    "shell.task_wake.gate_cleared",
                    Some(self.session_info.id.0.as_ref()),
                    Some(serde_json::json!({ "reason": "rewind" })),
                );
                state.rewindable = false;
                state.pending_inputs.pop_front()
            } else {
                None
            };
            if rewind_requested && requested_prompt_id.is_none() {
                self.log_rewind_decision(
                    None,
                    front_prompt_id.as_deref(),
                    if rewound_input.is_some() {
                        "legacy_rewound"
                    } else if !was_rewindable {
                        "window_closed"
                    } else {
                        "non_user_front"
                    },
                );
            }
            let cancelled_prompt_id = if !identity_matches {
                None
            } else if rewound_input.is_some() {
                rewound_task_prompt_id
            } else if let CancelFinalization::Keep(_) = &finalization {
                state.running_task.as_ref().map(|task| {
                    if let Some(epoch) = turn_epoch {
                        self.abort_turn_task(task, epoch);
                    }
                    task.prompt_id.clone()
                })
            } else {
                state.running_task.take().map(|task| {
                    if let Some(epoch) = turn_epoch {
                        self.abort_turn_task(&task, epoch);
                    }
                    task.prompt_id
                })
            };
            if !identity_matches {
                self.log_rewind_decision(
                    requested_prompt_id.as_deref(),
                    front_prompt_id.as_deref(),
                    "stale_prompt_id",
                );
                if suppress_task_wakes {
                    if let Some(gate) = &self.tool_context.task_wake_suppressed {
                        gate.set(false);
                    }
                    state.notifications_suppressed = false;
                }
                return CancelOutcome::noop();
            }

            // Decide which queued inputs get resolved with `Cancelled` now vs.
            // preserved for the post-cancel drain:
            //
            // * rewind: the front was already popped above; respond to nothing.
            // * hard teardown (`kill_background_tasks`, the subagent-shutdown
            //   path that sends `Shutdown` next): drain the WHOLE queue — there
            //   is no point starting the next prompt and draining resolves every
            //   queued input's `respond_to` cleanly.
            // * normal cancel: remove the running turn; only an interactive stop
            //   (Ctrl+C / Esc / [stop]) also removes queued task/workflow
            //   completion wakes. Preserve real user prompts
            //   and unrelated synthetic entries so `maybe_start_running_task` can
            //   promote the next genuine user turn.
            //   The cancelling client does not pull any prompt back into its
            //   input — the server queue is the single source of truth for what
            //   runs next. Previously every cancel did `std::mem::take`,
            //   discarding the whole queue server-side; because no broadcast
            //   followed, clients kept a stale mirror and the queue only visibly
            //   vanished on the next prompt's (now-empty) broadcast.
            //
            // The in-flight turn is always `pending_inputs.front()`
            // (`maybe_start_running_task` promotes the front WITHOUT popping it;
            // `handle_completion` pops it at turn end) — so index 0 is the slot
            // whose `respond_to` the client is awaiting and whose spinner is
            // showing. We ALWAYS resolve it with `Cancelled`, regardless of
            // whether `running_task` is currently `Some`: in the narrow windows
            // where the front has no live task (e.g. a completion was just
            // dequeued and the next prompt not yet promoted, or a cancel races
            // ahead of `maybe_start_running_task`), gating on `running_task`
            // would drop the front's `respond_to`, hanging the client's
            // `session/prompt` forever (the TUI spinner never returns to idle).
            let pending_inputs = if rewound_input.is_some() {
                VecDeque::new()
            } else if kill_background_tasks {
                std::mem::take(&mut state.pending_inputs)
            } else {
                let mut kept = VecDeque::with_capacity(state.pending_inputs.len());
                let mut cancelled = VecDeque::new();
                for (idx, item) in std::mem::take(&mut state.pending_inputs)
                    .into_iter()
                    .enumerate()
                {
                    let is_running_turn = idx == 0;
                    if is_running_turn {
                        cancelled.push_back(item);
                    } else if suppress_task_wakes
                        && matches!(
                            &item.origin,
                            super::PromptOrigin::TaskCompleted { .. }
                                | super::PromptOrigin::WorkflowCompleted { .. }
                        )
                    {
                        if let Some(fallback) = item.task_wake_fallback {
                            Self::push_task_wake_fallback(&mut state, fallback);
                        }
                        Self::respond_removed_prompt(item.respond_to);
                    } else {
                        kept.push_back(item);
                    }
                }
                state.pending_inputs = kept;
                cancelled
            };
            // Whether a user prompt remains queued behind the just-cancelled
            // turn. It distinguishes the next turn's redirect kind for
            // telemetry: `queued_after_cancel` (a queued prompt is promoted)
            // vs `cancel_then_send` (the user types a fresh prompt). Synthetic
            // inputs (auto-wake / nudges) are not user redirects.
            let had_queued_user_prompt = state
                .pending_inputs
                .iter()
                .any(|i| !i.origin.is_synthetic());
            // NOTE: `current_prompt_id` is deliberately NOT cleared here —
            // cancel usage attribution must snapshot the ledger against the
            // live pin first; it is cleared below, right after the
            // `finalize_usage_from_outcome` / `snapshot_prompt_usage` call.
            (
                cancelled_prompt_id,
                pending_inputs,
                rewound_input,
                had_queued_user_prompt,
                turn_epoch,
            )
        };
        // Authoritative cancel identity: the task actually torn down. The pin
        // is only a fallback for windows where a cancel raced ahead of the
        // promote (no task yet) — see the capture comment above.
        let tore_down_task = cancelled_prompt_id.is_some();
        if tore_down_task {
            self.cancel_active_sampling_requests();
            if rewound_input.is_none() {
                self.chat_state_handle
                    .repair_dangling_after_harness_halt("user_cancel");
            }
        }

        if rewound_input.is_none()
            && let Some(prompt_id) = cancelled_prompt_id.as_deref()
            && let Some(epoch) = turn_epoch
            && let Some(reason) = cancel_reason
        {
            self.report_cancelled_turn(CancelledTurn {
                prompt_id,
                epoch,
                reason,
                trigger: trigger.as_ref().map(|t| t.as_str().to_string()),
                last_assistant_message,
            });
        }

        if tore_down_task {
            self.events.cancel_active_tool();
        }
        // No prompt id means no turn ran, and closing a fresh session would
        // otherwise write a `TurnEnded` with no `turn_started`.
        if rewound_input.is_none() && cancelled_prompt_id.is_some() {
            // The trigger (esc / ctrl_c / …) rides in the events.jsonl
            // `cancellation_context`; the category stays `MidTurnAbort` so the
            // existing dashboards/dataset keep working.
            let cancellation_context = trigger
                .as_ref()
                .map(|t| serde_json::json!({ "trigger": t.as_str() }));
            self.emit_turn_ended(
                crate::session::events::TurnOutcomeLabel::Cancelled,
                Some(crate::session::events::CancellationCategory::MidTurnAbort),
                cancellation_context,
            );
            // Mark the next real user prompt as following a mid-turn abort so
            // replay/analytics/the model can see the user stopped this turn.
            // Send-now is a silent cancel-and-send — the user is continuing,
            // not aborting — so it must not arm the interrupt category or the
            // "[Request interrupted]" reminder for its own continuation turn
            // (mirrors the cancel-rate skip above).
            let send_now = matches!(trigger, Some(crate::session::CancelTrigger::SendNow));
            if !send_now {
                self.events.set_prior_interrupt_category(
                    crate::session::events::CancellationCategory::MidTurnAbort,
                );
            }
            // Arm a one-shot `<system-reminder>` for the next real user turn,
            // but only when the abort leaves the model with NO other signal:
            // the partial assistant text is discarded out-of-band, so the only
            // remaining cue is the dangling-tool-call repair. If a tool call is
            // committed but unanswered — a tool mid-execution, OR a turn parked
            // on a permission prompt (where no tool is marked active yet) — the
            // next-turn repair already emits a "cancelled" tool-result, so we
            // skip the reminder to avoid a duplicate signal. Gating on the
            // actual dangling state (not `had_active_tool`) covers the
            // permission-prompt and partial-parallel-call cases too.
            if !send_now
                && !self.chat_state_handle.has_dangling_tool_calls().await
                && self
                    .chat_state_handle
                    .get_last_assistant_text_in_turn()
                    .await
                    .is_some()
            {
                self.events.set_pending_interrupt_reminder();
            }
            // Shared `redirect_kind` for the data pipeline: the next user turn's
            // `turn_started` records HOW the user redirected after this abort.
            self.events
                .set_prior_redirect_kind(if had_queued_user_prompt {
                    crate::session::events::RedirectKind::QueuedAfterCancel
                } else {
                    crate::session::events::RedirectKind::CancelThenSend
                });
        }

        if send_now {
            self.background_foreground_commands_for_send_now().await;
        }
        if tore_down_task {
            if let Some(is_turn_active) = &self.tool_context.is_turn_active {
                is_turn_active.store(false, std::sync::atomic::Ordering::Relaxed);
            }
            self.tool_context.blocking_wait_depth.reset();
            self.flush_pending_skill_reminders().await;
        }

        // No multi-second drain here (actor loop would block RecordSubagentUsage).
        // Same UsageDrainOutcome policy as freeze via finalize_usage_from_outcome.
        let cancelled_usage = if rewound_input.is_none() {
            if let Some(ref prompt_id) = cancelled_prompt_id {
                let reply = self.outstanding_reply_for_prompt(prompt_id).await;
                let outcome =
                    super::turn::UsageDrainOutcome::from_outstanding_reply(reply.as_ref());
                self.finalize_usage_from_outcome(prompt_id, outcome).await
            } else {
                None
            }
        } else {
            None
        };
        if tore_down_task {
            self.cancel_active_sampling_requests();
            *self.doom_loop_turn_tally.lock() = Default::default();
        }
        let turn_stopped = cancelled_prompt_id.is_some() && cancel_reason.is_some();
        // Announced for every cancel that stopped a turn, including a rewind and a
        // cancel-and-send. A no-op if the turn task announced first.
        if cancelled_prompt_id.is_some()
            && let Some(epoch) = turn_epoch
        {
            self.notify_turn_abort(epoch, xai_agent_lifecycle::TurnAbortReason::Interrupted)
                .await;
        }
        if matches!(finalization, CancelFinalization::Rewind)
            && let Some(prompt_id) = cancelled_prompt_id.as_deref()
        {
            self.clear_exact_turn_resources(prompt_id).await;
        }
        let no_task_pinned_prompt_id = match &finalization {
            CancelFinalization::Keep(lease) => match &lease.binding {
                FinalizationBinding::NoTask(Some(id)) if pinned_prompt_id.as_ref() == Some(id) => {
                    Some(id.clone())
                }
                _ => None,
            },
            CancelFinalization::Rewind => None,
        };
        if rewound_input.is_none()
            && let Some(prompt_id) = cancelled_prompt_id.or(no_task_pinned_prompt_id)
        {
            // Stamp `cancelTrigger` on the terminal `_meta` so clients can tell
            // a send-now cancel from an interactive Ctrl+C/Esc.
            self.emit_turn_completed(
                prompt_id,
                &Ok(acp::StopReason::Cancelled),
                cancelled_usage.clone(),
                trigger.as_ref().map(crate::session::CancelTrigger::as_str),
                tore_down_task.then(|| {
                    crate::session::commands::meta_category_str(
                        crate::session::events::CancellationCategory::MidTurnAbort,
                    )
                }),
                None,
                cancel_elapsed_ms,
            )
            .await;
        }

        if let Some(input) = rewound_input {
            let _strip_guard = self.prepare_image_strips_for_rewind().await;
            self.cancel_active_sampling_requests();
            self.cancel_pending_image_strips_for_rewind();
            return self
                .finish_rewound_cancel(input, None, total_tokens, turn_stopped)
                .await;
        }

        for (idx, input) in pending_inputs.into_iter().enumerate() {
            // Running turn is idx 0; queued prompts never spent tokens.
            let is_running_turn = tore_down_task && idx == 0;
            if let Some(task_id) = input.origin.completion_id()
                && let Some(reservations) = &self.tool_context.task_completion_reservations
            {
                reservations.release(task_id);
            }
            let _ = input
                .respond_to
                .send(Ok(PromptTurnOk {
                    stop_reason: acp::StopReason::Cancelled,
                    total_tokens,
                    turn_snapshot: None,
                    completion_kind: PromptCompletionKind::Cancelled {
                        // Previously hard-coded `None`, which dropped the
                        // category on the abort path so `streaming_partial.json`
                        // recorded a bare `"cancelled"`. Carry `MidTurnAbort`
                        // so the partial's `reason` and any downstream consumer
                        // match what `emit_turn_ended` wrote to `events.jsonl`.
                        category: is_running_turn
                            .then_some(crate::session::events::CancellationCategory::MidTurnAbort),
                        // Thread the trigger on the running turn only (idx 0);
                        // MvpAgent stamps it on the `PromptResponse` `_meta`.
                        context: if is_running_turn {
                            trigger.as_ref().map(|t| {
                                crate::session::commands::CancellationContext {
                                    trigger: Some(t.as_str().to_string()),
                                    ..Default::default()
                                }
                            })
                        } else {
                            None
                        },
                    },
                    structured_output: None,
                    usage: if is_running_turn {
                        cancelled_usage.clone()
                    } else {
                        None
                    },
                    // Only the running turn (idx 0) ran, so only it echoes a bound; a queued prompt
                    // that never promoted attests nothing (like respond_removed_prompt).
                    tool_overrides: if is_running_turn {
                        self.effective_tool_overrides()
                    } else {
                        None
                    },
                }))
                .ok();
        }
        let settled = match &mut finalization {
            CancelFinalization::Keep(lease) => self.finish_finalization_lease(lease).await,
            CancelFinalization::Rewind => false,
        };
        if settled {
            self.maybe_apply_pending_web_search_reload().await;
        }
        CancelOutcome {
            barrier: if suppress_task_wakes {
                WakeBarrier::Armed
            } else {
                WakeBarrier::Clear
            },
            turn_stopped,
            settled,
        }
    }
}

impl SessionActor {
    async fn background_foreground_commands_for_send_now(&self) {
        let owner = Some(self.session_info.id.0.as_ref());
        let backgrounded = {
            self.agent
                .borrow()
                .tool_bridge()
                .background_foreground_commands(owner)
                .await
        };

        if backgrounded.is_empty() {
            self.agent
                .borrow()
                .tool_bridge()
                .kill_foreground_commands_by_owner(&self.session_info.id.0)
                .await;
        }

        for backgrounded_command in backgrounded {
            let message = format!(
                "Command was moved to the background because the user sent a new message. \
                 The process is still running, not cancelled. Retrieve its output later \
                 with {} (task_id: {}).",
                self.tool_context.task_output_tool_name, backgrounded_command.tool_call_id
            );
            self.chat_state_handle
                .push_tool_result(ConversationItem::tool_result(
                    backgrounded_command.tool_call_id,
                    message,
                ));
        }
    }

    #[tracing::instrument(name = "cancel.kill_foreground", skip_all)]
    async fn kill_foreground_commands_for_cancel(&self) {
        if self.startup_hints.is_subagent {
            self.agent
                .borrow()
                .tool_bridge()
                .kill_foreground_commands_by_owner(&self.session_info.id.0)
                .await;
        } else {
            self.agent
                .borrow()
                .tool_bridge()
                .kill_foreground_commands()
                .await;
        }
    }
}

#[cfg(test)]
mod task_slot_tests {
    // Exercises the shared `TaskSlot<T>` primitive that backs both the deferred
    // prefix and the idle-notification debounce: arm / take / cancel / re-arm.
    use super::TaskSlot;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Spawn a 60s task that bumps `fired` by `by`, armed into `slot`.
    fn arm_counter(slot: &TaskSlot<()>, fired: &Arc<AtomicUsize>, by: usize) {
        let f = Arc::clone(fired);
        slot.arm(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            f.fetch_add(by, Ordering::SeqCst);
        }));
    }

    /// A task left armed runs to completion once its delay elapses.
    #[tokio::test(start_paused = true)]
    async fn armed_task_fires_after_delay() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;

        assert_eq!(fired.load(Ordering::SeqCst), 1, "armed task must fire");
    }

    /// `cancel()` aborts a still-pending task (the new-user-prompt reset path).
    #[tokio::test(start_paused = true)]
    async fn cancel_aborts_pending_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);

        tokio::time::advance(Duration::from_secs(30)).await;
        slot.cancel();
        assert!(slot.take().is_none(), "cancel must clear the slot");
        tokio::time::advance(Duration::from_secs(120)).await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            0,
            "cancelled task must not fire"
        );
    }

    /// Arming a new task aborts the previous one so only the latest fires.
    #[tokio::test(start_paused = true)]
    async fn rearm_aborts_previous_task() {
        let fired = Arc::new(AtomicUsize::new(0));
        let slot: TaskSlot<()> = TaskSlot::new();
        arm_counter(&slot, &fired, 1);
        arm_counter(&slot, &fired, 10);

        let task = slot.take().expect("task armed");
        tokio::time::advance(Duration::from_secs(61)).await;
        let _ = task.await;
        tokio::task::yield_now().await;

        assert_eq!(
            fired.load(Ordering::SeqCst),
            10,
            "only the re-armed task fires"
        );
    }
}
