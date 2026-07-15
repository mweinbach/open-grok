//! Notification bridge: translates `xai-grok-tools` `ToolNotification` events
//! into `xai-grok-shell`'s native systems (ACP gateway, hunk tracker, file state tracker).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::{self as acp, Client as _};
use tokio::sync::{Mutex as TokioMutex, mpsc};
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_tools::notification::types::{ToolNotification, ToolNotificationHandle};
use xai_grok_tools::types::output::{BashOutput, ToolOutput};
use xai_hunk_tracker::HunkTrackerHandle;

use crate::session::commands::SessionCommand;
use crate::session::commands::{NotificationPriority, NotificationSource};
use crate::session::persistence::PersistenceMsg;
use xai_grok_workspace::session::file_state::FileStateTracker;

/// Configuration for the notification bridge.
pub struct NotificationBridgeConfig {
    /// ACP gateway for sending streaming updates to TUI
    pub gateway: GatewaySender,
    /// ACP session ID
    pub session_id: acp::SessionId,
    /// Hunk tracker for recording agent writes
    pub hunk_tracker_handle: HunkTrackerHandle,
    /// File state tracker for rewind functionality
    pub file_state_tracker: Arc<FileStateTracker>,
    /// Current prompt index (shared with session state)
    pub prompt_index: Arc<TokioMutex<usize>>,
    /// Working directory for path relativization
    pub cwd: PathBuf,
    /// Shared gate: when false, suppress gateway forwarding.
    /// Events are still processed for hunk tracking and file state.
    pub gateway_enabled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Persistence channel for durably storing notifications.
    /// Used to persist bash output even when the gateway gate is closed.
    pub persistence_tx: mpsc::UnboundedSender<PersistenceMsg>,
    /// When true, send incremental `output_delta` instead of full `output`
    /// in bash streaming updates. The client must opt in via the
    /// `x.ai/incrementalBashOutput` capability.
    pub incremental_bash_output: bool,
    /// Plan mode tracker shared with the session actor.
    /// Used to transition state on `PlanModeEntered` / `PlanModeExited`
    /// tool notifications.
    pub plan_mode: Arc<parking_lot::Mutex<crate::session::plan_mode::PlanModeTracker>>,
    /// Session-level prompt mode shared with the session actor.
    /// Updated on `PlanModeEntered` / `PlanModeExited` and `session/set_mode`
    /// so the next turn starts in the correct mode.
    pub current_prompt_mode: Arc<parking_lot::Mutex<crate::session::plan_mode::PromptMode>>,
    /// Turn-level prompt mode. Set at turn start, then updated only by
    /// agent tool calls (`EnterPlanMode` / `ExitPlanMode`). NOT affected
    /// by `session/set_mode`. Read at turn end for `end_prompt_mode`.
    pub turn_prompt_mode: Arc<parking_lot::Mutex<crate::session::plan_mode::PromptMode>>,
    /// Session command channel for monitor events and task-completed injections.
    pub session_cmd_tx: mpsc::UnboundedSender<SessionCommand>,
    /// Shared set of IDs delivered via auto-wake, used to suppress duplicate
    /// `TaskCompletionReminder` entries for the same task/subagent.
    pub auto_wake_delivered: xai_grok_tools::reminders::task_completion::AutoWakeDeliveredIds,
    /// Channel for requesting trace uploads for synthetic auto-wake turns.
    /// Wrapped in `Arc<Mutex<..>>` because the coordinator creates the channel
    /// after the notification bridge is spawned — the bridge reads the latest
    /// value on each notification.
    pub(crate) synthetic_trace_tx: Arc<
        std::sync::Mutex<
            Option<
                tokio::sync::mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>,
            >,
        >,
    >,
    /// Resolved name of the `BackgroundTaskAction` tool. Written exactly
    /// once after the agent's toolset is finalized; read many times
    /// thereafter from the notification bridge and the session actor's
    /// between-turn drain. `None` means no such tool is registered in this
    /// toolset, which is a valid resolved state.
    pub task_output_tool_name: Arc<std::sync::OnceLock<Option<String>>>,
    /// Resolved name of the `Read` tool, used by `format_bash_completion`'s
    /// disk-pointer footer so the model can recover full bash output from
    /// `task.output_file` even when no polling tool is available. Same
    /// write-once-read-many lifecycle as `task_output_tool_name`.
    pub read_tool_name: Arc<std::sync::OnceLock<Option<String>>>,
    /// When `false`, bash task completions fall back to the idle-gated
    /// `InjectNotification` path instead of immediate synthetic prompts.
    pub auto_wake_enabled: bool,
    /// When `true`, an approved `PlanModeExited` also arms the tracker's
    /// next-turn exit reminder. Grok-build leaves this `false` — its
    /// exit-plan tool result already informs the model, and a deferred
    /// reminder would arrive stale. Shared with the session actor (the
    /// `gateway_enabled` pattern) and refreshed on zero-turn rebuilds so the
    /// bridge always agrees with the live session gate.
    pub queue_exit_reminder_on_approved_exit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// When `true`, suppress the bash auto-wake synthetic prompt. Shared `Arc`
    /// written at one chokepoint — see
    /// `SessionActor::set_goal_loop_active_resource` for the rationale.
    pub goal_loop_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Snapshot a shared `OnceLock` tool-name slot as a borrowed `&str`.
/// Returns `None` if the slot is still unset (toolset not yet finalized)
/// or if the resolved value is `None` (no such tool registered in this
/// toolset.
pub(crate) fn resolved_tool_name(slot: &std::sync::OnceLock<Option<String>>) -> Option<&str> {
    slot.get().and_then(|v| v.as_deref())
}

/// Stamp a bridge-emitted notification's meta before it forks into
/// persistence + broadcast — see `util::event_id::ensure_event_id_meta`.
fn stamp_event_id(config: &NotificationBridgeConfig, meta: &mut Option<acp::Meta>) {
    crate::util::event_id::ensure_event_id_meta(&config.session_id.0, meta);
}

/// Create a `ToolNotificationHandle` and spawn a bridge task that
/// translates notifications into shell-native systems.
pub fn spawn_notification_bridge(config: NotificationBridgeConfig) -> ToolNotificationHandle {
    let (handle, mut rx) = ToolNotificationHandle::channel();

    tokio::task::spawn_local(async move {
        // Per-tool-call byte offset for incremental delta computation.
        // Only used when `config.incremental_bash_output` is true.
        let mut offsets: HashMap<String, usize> = HashMap::new();

        while let Some(notification) = rx.recv().await {
            handle_notification(&config, notification, &mut offsets).await;
        }
        tracing::debug!("Notification bridge task exiting (sender dropped)");
    });

    handle
}

/// Emit a `CurrentModeUpdate` for the given [`SessionMode`] — persisted to
/// `updates.jsonl` so session replay re-applies the mode, and forwarded to
/// the gateway so the pager updates live.
async fn emit_current_mode_update(
    config: &NotificationBridgeConfig,
    mode: xai_grok_tools::types::SessionMode,
) {
    let mut notification = acp::SessionNotification::new(
        config.session_id.clone(),
        acp::SessionUpdate::CurrentModeUpdate(acp::CurrentModeUpdate::new(
            acp::SessionModeId::new(mode.as_id()),
        )),
    );
    stamp_event_id(config, &mut notification.meta);

    let _ = config.persistence_tx.send(PersistenceMsg::Update(
        crate::session::storage::SessionUpdate::Acp(Box::new(notification.clone())),
    ));

    config.gateway.forward_fire_and_forget(notification);
}

/// Handle a single notification by forwarding it to the appropriate shell system.
async fn handle_notification(
    config: &NotificationBridgeConfig,
    notification: ToolNotification,
    offsets: &mut HashMap<String, usize>,
) {
    match notification {
        ToolNotification::BashOutputChunk(chunk) => {
            // Compute output and output_delta based on incremental mode.
            let (output, output_delta) = if config.incremental_bash_output {
                let prev_offset = offsets.get(&chunk.base.tool_call_id).copied().unwrap_or(0);
                let full = &chunk.base.output;
                let delta = if prev_offset <= full.len() {
                    full[prev_offset..].to_vec()
                } else {
                    // Buffer shrank (e.g. terminal clear / reset).
                    // Send the full buffer and reset offset.
                    full.clone()
                };
                offsets.insert(chunk.base.tool_call_id.clone(), full.len());
                // In incremental mode: output is empty, delta carries the bytes.
                (Vec::new(), Some(delta))
            } else {
                (chunk.base.output.clone(), None)
            };

            // Build a ToolOutput::Bash from the chunk for the TUI to parse
            let bash_output = ToolOutput::Bash(BashOutput {
                output_for_prompt: BashOutput::make_output_for_prompt(&String::from_utf8_lossy(
                    &chunk.base.output,
                )),
                output,
                exit_code: 0,
                command: chunk.base.command.clone(),
                truncated: chunk.base.truncated,
                signal: None,
                timed_out: false,
                description: None,
                current_dir: chunk.base.cwd.to_string_lossy().to_string(),
                output_file: String::new(),
                total_bytes: chunk.base.total_bytes,
                output_delta,
                was_bare_echo: false,
            });

            // Send ACP ToolCallUpdate with InProgress status for TUI streaming
            let update = acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(chunk.base.tool_call_id.clone()),
                acp::ToolCallUpdateFields::new()
                    .status(Some(acp::ToolCallStatus::InProgress))
                    .content(Some(vec![acp::ToolCallContent::from(
                        acp::ContentBlock::Text(acp::TextContent::new(
                            String::from_utf8_lossy(&chunk.base.output).into_owned(),
                        )),
                    )]))
                    .raw_output(serde_json::to_value(&bash_output).ok()),
            ));
            let mut notification = acp::SessionNotification::new(config.session_id.clone(), update);
            stamp_event_id(config, &mut notification.meta);
            // Always persist — even when the gateway gate is closed, so bash
            // output survives replay when the client later calls loadSession.
            let _ = config.persistence_tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Acp(Box::new(notification.clone())),
            ));
            // Only forward to the client if the gateway gate is open.
            if config
                .gateway_enabled
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let _ = config.gateway.session_notification(notification).await;
            }
        }

        ToolNotification::BashExecutionComplete(complete) => {
            // Clean up offset tracking for this tool call.
            offsets.remove(&complete.base.tool_call_id);
            tracing::debug!(
                tool_call_id = %complete.base.tool_call_id,
                exit_code = ?complete.exit_code,
                "Bash execution complete notification received"
            );
        }

        ToolNotification::BashExecutionTimeout(timeout) => {
            tracing::debug!(
                tool_call_id = %timeout.base.tool_call_id,
                elapsed = ?timeout.elapsed,
                "Bash execution timeout notification received"
            );
        }

        ToolNotification::BashExecutionFailed(failed) => {
            tracing::warn!(
                tool_call_id = %failed.tool_call_id,
                error = %failed.error,
                "Bash execution failed notification received"
            );
        }

        ToolNotification::BashExecutionBackgrounded(bg) => {
            tracing::debug!(
                tool_call_id = %bg.base.tool_call_id,
                task_id = %bg.task_id,
                command = %bg.base.command,
                output_file = %bg.output_file.display(),
                "Bash execution backgrounded notification received — forwarding to TUI"
            );

            // Forward as x.ai/task_backgrounded ExtNotification so the TUI can
            // correlate tool_call_id with task_id and populate the tasks panel.
            let mut notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::TaskBackgrounded {
                    tool_call_id: bg.base.tool_call_id.clone(),
                    task_id: bg.task_id.clone(),
                    command: bg.base.command.clone(),
                    cwd: bg.base.cwd.to_string_lossy().to_string(),
                    output_file: bg.output_file.to_string_lossy().to_string(),
                    monitor_description: bg.monitor_description.clone(),
                    description: bg.description.clone(),
                },
                meta: None,
            };
            {
                let mut meta_map = None;
                stamp_event_id(config, &mut meta_map);
                notification.meta = meta_map.map(serde_json::Value::Object);
            }

            // Persist so task correlation survives reconnect/replay.
            let _ = config.persistence_tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone())),
            ));

            let params = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
                .ok();
            if let Some(params) = params {
                let ext_notification =
                    acp::ExtNotification::new("x.ai/task_backgrounded", params.into());
                config.gateway.forward_fire_and_forget(ext_notification);
            }
        }

        ToolNotification::FileWritten(written) => {
            let prompt_index = *config.prompt_index.lock().await;
            config.hunk_tracker_handle.record_agent_write(
                written.absolute_path.clone(),
                written.content.clone(),
                prompt_index,
                written.previous_content.clone(),
            );

            if written.previous_content.is_some() || written.is_new_file {
                config
                    .file_state_tracker
                    .add_before_snapshot_for_prompt(
                        prompt_index,
                        &written.absolute_path,
                        &config.cwd,
                        written.previous_content,
                    )
                    .await;
            }

            tracing::debug!(
                path = %written.absolute_path.display(),
                is_new_file = written.is_new_file,
                "FileWritten notification forwarded to hunk tracker"
            );
        }

        ToolNotification::TaskCompleted(task_snapshot) => {
            let is_monitor =
                task_snapshot.kind == xai_grok_tools::computer::types::TaskKind::Monitor;
            let task_id = task_snapshot.task_id.clone();
            let goal_loop_active = config
                .goal_loop_active
                .load(std::sync::atomic::Ordering::Relaxed);

            // Block-waited / explicitly-killed: the model already has the result
            // (blocking wait return or kill_task tool response). Skip auto-wake
            // for both bash and monitors — a redundant synthetic prompt is noise.
            //
            // Natural monitor exit (including exit code 0) MUST auto-wake the
            // same way bash does. Relying only on the pipeline's terminal
            // `MonitorEvent` + idle-gated `InjectNotification` was easy to miss
            // when the agent was idle and the monitor produced no further
            // stdout. The pager still receives x.ai/task_completed below for UI.
            // Stamped on the completion notification below so the pager knows
            // whether a wake response follows the chip.
            let mut will_wake = false;
            if task_snapshot.block_waited || task_snapshot.explicitly_killed {
                // no auto-wake
            } else if goal_loop_active {
                // Goal loop active: suppress the wake (synthetic prompt + the
                // idle-gated fallback); surfaces 2/3 drain it. See
                // `set_goal_loop_active_resource`.
                tracing::info!(
                    task_id = %task_id,
                    is_monitor,
                    "auto-wake: suppressed completion (goal loop active)"
                );
            } else if config.auto_wake_enabled {
                // Mark delivered so `TaskCompletionReminder` suppresses the
                // duplicate on the next tool call (bash and monitor alike).
                config.auto_wake_delivered.insert(task_id.clone());

                // Monitor exit: the TaskCompleted Prompt is the sole model-facing
                // wake. Drop any already-queued pipeline MonitorEvents for this
                // task (stdout lines + terminal ended) so they do not start a
                // second NotificationDrain turn after the wake.
                if is_monitor {
                    let _ = config
                        .session_cmd_tx
                        .send(SessionCommand::DropMonitorNotifications {
                            task_id: task_id.clone(),
                        });
                }

                let tool_name = resolved_tool_name(&config.task_output_tool_name);
                let read_name = resolved_tool_name(&config.read_tool_name);
                let body = if is_monitor {
                    xai_grok_tools::reminders::task_completion::format_monitor_completion(
                        &task_snapshot,
                        tool_name,
                    )
                } else {
                    xai_grok_tools::reminders::task_completion::format_bash_completion(
                        &task_snapshot,
                        tool_name,
                        read_name,
                    )
                };
                let message = xai_grok_tools::reminders::wrap_reminder(&body);
                let prompt_id = format!("task-completed-{task_id}");
                let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(message))];

                // Capture a pre-prompt session snapshot for the trace upload path.
                let (before_copy_tx, before_copy_rx) = tokio::sync::oneshot::channel();
                let _ = config.session_cmd_tx.send(SessionCommand::CopyFile {
                    respond_to: before_copy_tx,
                });

                let (respond_to, completion_rx) = tokio::sync::oneshot::channel();
                tracing::info!(
                    task_id = %task_id,
                    prompt_id = %prompt_id,
                    is_monitor,
                    "auto-wake: injecting synthetic prompt for completed background task"
                );
                // Stamp from the actual enqueue: `will_wake` on the completion
                // notification must never promise a wake this send didn't queue
                // (mirrors `parent_channel_open` in `should_auto_wake_subagent`).
                // The channel is unbounded, so this only fails when the session
                // actor is already gone.
                will_wake = config
                    .session_cmd_tx
                    .send(SessionCommand::Prompt {
                        prompt_id: prompt_id.clone(),
                        prompt_blocks,
                        prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                        artifact_upload_ctx: None,
                        client_identifier: None,
                        screen_mode: None,
                        verbatim: true,
                        traceparent: xai_file_utils::trace_context::current_traceparent(),
                        json_schema: None,
                        send_now: false,
                        respond_to,
                        persist_ack: None,
                        parsed_prompt_tx: None,
                    })
                    .is_ok();

                if let Some(ref trace_tx) = *config
                    .synthetic_trace_tx
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                {
                    tracing::info!(
                        task_id = %task_id,
                        "auto-wake: sending synthetic turn trace request"
                    );
                    let _ = trace_tx.send(crate::upload::turn::SyntheticTurnTraceRequest {
                        session_id: config.session_id.clone(),
                        prompt_id,
                        completion_rx,
                        before_session_copy_rx: before_copy_rx,
                    });
                } else {
                    tracing::debug!(
                        task_id = %task_id,
                        "auto-wake: no synthetic_trace_tx, skipping trace request"
                    );
                }
            } else {
                // Auto-wake disabled — fall back to idle-gated notification drain.
                let tool_name = resolved_tool_name(&config.task_output_tool_name);
                let read_name = resolved_tool_name(&config.read_tool_name);
                let message = if is_monitor {
                    xai_grok_tools::reminders::task_completion::format_monitor_completion(
                        &task_snapshot,
                        tool_name,
                    )
                } else {
                    xai_grok_tools::reminders::task_completion::format_bash_completion(
                        &task_snapshot,
                        tool_name,
                        read_name,
                    )
                };
                let prompt_id = format!("bash-completed-{task_id}");
                let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(message))];
                let _ = config
                    .session_cmd_tx
                    .send(SessionCommand::InjectNotification {
                        prompt_id,
                        prompt_blocks,
                        priority: NotificationPriority::Later,
                        source: NotificationSource::BashTaskCompleted {
                            task_id: task_id.clone(),
                        },
                    });
            }

            // When a task is complete send notifications to the client so it can act on it
            let mut notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::TaskCompleted {
                    task_snapshot,
                    will_wake,
                },
                meta: None,
            };
            {
                let mut meta_map = None;
                stamp_event_id(config, &mut meta_map);
                notification.meta = meta_map.map(serde_json::Value::Object);
            }

            // Persist so task completion history survives reconnect/replay.
            let _ = config.persistence_tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone())),
            ));

            let params = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
                .ok();
            if let Some(params) = params {
                let notification: acp::ExtNotification =
                    acp::ExtNotification::new("x.ai/task_completed", params.into());
                config.gateway.forward_fire_and_forget(notification);
            }

            let _ = config
                .session_cmd_tx
                .send(SessionCommand::DispatchNotificationHook {
                    notification_type: "task_complete".into(),
                    message: Some(format!("Background task completed: {task_id}")),
                    title: None,
                    level: Some("info".into()),
                });
        }

        ToolNotification::PlanModeEntered(entered) => {
            let activated = config.plan_mode.lock().activate_from_tool();
            if activated {
                *config.current_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Plan;
                *config.turn_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Plan;

                let snapshot = config.plan_mode.lock().snapshot();
                let _ = config
                    .persistence_tx
                    .send(PersistenceMsg::PlanModeState(snapshot));

                // Notify the frontend immediately so the plan-mode chip appears in the UI
                // (currentModeId = 'plan'). Without this the agent can silently enter plan
                // mode via the EnterPlanMode tool and the UI would never update.
                emit_current_mode_update(config, xai_grok_tools::types::SessionMode::Plan).await;
            }
            tracing::info!(
                tool_call_id = %entered.tool_call_id,
                activated,
                "Plan mode entered via EnterPlanMode tool"
            );
        }

        ToolNotification::PlanModeExited(exited) => {
            // v1: auto-approve. A full implementation would present an
            // approval UI with reject/feedback options.
            let deactivated = {
                let mut tracker = config.plan_mode.lock();
                let deactivated = tracker.deactivate_approved();
                if deactivated
                    && config
                        .queue_exit_reminder_on_approved_exit
                        .load(std::sync::atomic::Ordering::Relaxed)
                {
                    tracker.queue_exit_reminder();
                }
                deactivated
            };
            if deactivated {
                *config.current_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Agent;
                *config.turn_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Agent;

                let snapshot = config.plan_mode.lock().snapshot();
                let _ = config
                    .persistence_tx
                    .send(PersistenceMsg::PlanModeState(snapshot));

                // Mirror the entry path: emit a `CurrentModeUpdate("default")`
                // so the pager flips out of plan mode without having to
                // string-match tool titles. Persist + forward so the next
                // session replay also sees the exit.
                emit_current_mode_update(config, xai_grok_tools::types::SessionMode::Default).await;
            }
            tracing::info!(
                tool_call_id = %exited.tool_call_id,
                deactivated,
                has_plan = exited.plan_content.is_some(),
                "Plan mode exited via ExitPlanMode tool"
            );
        }

        ToolNotification::UserQuestionAsked(asked) => {
            tracing::info!(
                tool_call_id = %asked.tool_call_id,
                "User question asked"
            );
        }

        ToolNotification::LspServerStarting(s) => {
            tracing::debug!(server = %s.server_name, command = %s.command, "LSP server starting");
        }
        ToolNotification::LspServerReady(s) => {
            tracing::info!(server = %s.server_name, "LSP server ready");
        }
        ToolNotification::LspServerCrashed(s) => {
            tracing::warn!(server = %s.server_name, "LSP server crashed");
        }
        ToolNotification::LspServerRetrying(s) => {
            tracing::warn!(
                server = %s.server_name,
                attempt = s.attempt,
                max_restarts = s.max_restarts,
                backoff_ms = s.backoff_ms,
                "LSP server retrying"
            );
        }
        ToolNotification::LspServerFailed(s) => {
            tracing::error!(server = %s.server_name, error = %s.error, "LSP server failed");
        }

        ToolNotification::ScheduledTaskFired(fired) => {
            tracing::info!(
                task_id = %fired.task_id,
                schedule = %fired.human_schedule,
                "Scheduled task fired, injecting prompt into session"
            );

            let inject_payload = serde_json::json!({
                "sessionId": config.session_id,
                "taskId": &fired.task_id,
                "prompt": &fired.prompt,
                "humanSchedule": &fired.human_schedule,
                "nextFireAt": &fired.next_fire_at,
            });
            if let Ok(params) = serde_json::value::to_raw_value(&inject_payload) {
                config
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/scheduled_task_inject_prompt",
                        params.into(),
                    ));
            }

            let fired_notif = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::ScheduledTaskFired {
                    task_id: fired.task_id,
                    prompt: fired.prompt,
                    human_schedule: fired.human_schedule,
                    next_fire_at: fired.next_fire_at,
                },
                meta: None,
            };
            if let Ok(params) =
                serde_json::to_value(&fired_notif).and_then(|v| serde_json::value::to_raw_value(&v))
            {
                config
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/scheduled_task_fired",
                        params.into(),
                    ));
            }
        }

        ToolNotification::MonitorEvent(event) => {
            // Cross-session guard: in leader mode many sessions share one agent
            // process, so drop events whose owner isn't this bridge's session
            // (else session A's monitor injects reminders into session B).
            // `None` owners (legacy backends) pass through.
            let my_session = config.session_id.0.as_ref();
            if let Some(owner) = event.owner_session_id.as_deref()
                && owner != my_session
            {
                // WARN (not debug) to surface the leader-mode mis-route in logs.
                tracing::warn!(
                    task_id = %event.task_id,
                    description = %event.description,
                    monitor_owner = %owner,
                    bridge_session = %my_session,
                    "Dropped cross-session monitor event: owner does not match this bridge's session"
                );
                return;
            }

            tracing::debug!(
                task_id = %event.task_id,
                description = %event.description,
                "Monitor event received, injecting into session"
            );

            // Forward to pager -- raw text for the bg task stdout buffer.
            let notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::MonitorEvent {
                    task_id: event.task_id.clone(),
                    description: event.description.clone(),
                    event_text: event.raw_text.clone(),
                },
                meta: None,
            };
            let params = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
                .ok();
            if let Some(params) = params {
                config
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/monitor_event",
                        params.into(),
                    ));
            }

            // If this monitor already auto-woke via TaskCompleted, do not inject
            // model-facing notifications (avoids a second NotificationDrain turn
            // with the same ended signal). Pager UI still got the event above.
            if config.auto_wake_delivered.contains(&event.task_id) {
                tracing::debug!(
                    task_id = %event.task_id,
                    "skipping model inject for monitor event: task already auto-woke via TaskCompleted"
                );
                return;
            }

            // Inject the event into the notification queue for idle-gated drain.
            let prompt_id = format!("monitor-{}-{}", event.task_id, uuid::Uuid::now_v7());
            let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
                event.event_text,
            ))];
            let _ = config
                .session_cmd_tx
                .send(SessionCommand::InjectNotification {
                    prompt_id,
                    prompt_blocks,
                    priority: NotificationPriority::Next,
                    source: NotificationSource::MonitorEvent {
                        task_id: event.task_id.clone(),
                    },
                });
        }

        ToolNotification::ScheduledTaskRemoved(removed) => {
            tracing::info!(task_id = %removed.task_id, "Scheduled task removed");

            let mut notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::ScheduledTaskDeleted {
                    task_id: removed.task_id,
                },
                meta: None,
            };
            {
                let mut meta_map = None;
                stamp_event_id(config, &mut meta_map);
                notification.meta = meta_map.map(serde_json::Value::Object);
            }
            // Persist the deletion too, so replay on resume nets out a removed
            // loop instead of resurrecting it from a persisted `created` line.
            let _ = config.persistence_tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone())),
            ));
            if let Ok(params) = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
            {
                config
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/scheduled_task_deleted",
                        params.into(),
                    ));
            }
        }

        ToolNotification::ScheduledTaskCreated(created) => {
            tracing::info!(task_id = %created.task_id, "Scheduled task created");

            let mut notification = crate::extensions::notification::SessionNotification {
                session_id: config.session_id.clone(),
                update: crate::extensions::notification::SessionUpdate::ScheduledTaskCreated {
                    task_id: created.task_id,
                    prompt: created.prompt,
                    human_schedule: created.human_schedule,
                    next_fire_at: created.next_fire_at,
                },
                meta: None,
            };
            {
                let mut meta_map = None;
                stamp_event_id(config, &mut meta_map);
                notification.meta = meta_map.map(serde_json::Value::Object);
            }
            // Persist so the loop survives reconnect/replay, mirroring
            // TaskBackgrounded/TaskCompleted. Without this, a second terminal
            // that resumes the session restores monitors and subagents (whose
            // notifications are persisted) but NOT loops, so the "watching" cue
            // and Tasks pane undercount until the loop next fires. Create/delete
            // are infrequent (bounded by the number of live loops); the
            // recurring `_fired` notification is deliberately NOT persisted to
            // avoid unbounded log growth.
            let _ = config.persistence_tx.send(PersistenceMsg::Update(
                crate::session::storage::SessionUpdate::Xai(Box::new(notification.clone())),
            ));
            if let Ok(params) = serde_json::to_value(&notification)
                .and_then(|v| serde_json::value::to_raw_value(&v))
            {
                config
                    .gateway
                    .forward_fire_and_forget(acp::ExtNotification::new(
                        "x.ai/scheduled_task_created",
                        params.into(),
                    ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_tools::computer::types::TaskKind;
    use xai_grok_tools::types::TaskSnapshot;

    fn make_test_config() -> (
        NotificationBridgeConfig,
        mpsc::UnboundedReceiver<SessionCommand>,
    ) {
        let (config, _gateway_rx, _persistence_rx, session_cmd_rx) = make_test_config_full();
        (config, session_cmd_rx)
    }

    /// Same as [`make_test_config`] but also returns the gateway and
    /// persistence receivers so a test can inspect the notifications
    /// emitted by `handle_notification`. Use this for plan-mode tests.
    #[allow(clippy::type_complexity)]
    fn make_test_config_full() -> (
        NotificationBridgeConfig,
        mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
        mpsc::UnboundedReceiver<PersistenceMsg>,
        mpsc::UnboundedReceiver<SessionCommand>,
    ) {
        let (gateway_tx, gateway_rx) = mpsc::unbounded_channel();
        let gateway = xai_acp_lib::AcpAgentGatewaySender::new(gateway_tx);
        let (session_cmd_tx, session_cmd_rx) = mpsc::unbounded_channel();
        let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
        let config = NotificationBridgeConfig {
            gateway,
            session_id: acp::SessionId::new("test-session"),
            hunk_tracker_handle: HunkTrackerHandle::noop(),
            file_state_tracker: Arc::new(FileStateTracker::new()),
            prompt_index: Arc::new(TokioMutex::new(0)),
            cwd: PathBuf::from("/tmp"),
            gateway_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            persistence_tx,
            incremental_bash_output: false,
            plan_mode: Arc::new(parking_lot::Mutex::new(
                crate::session::plan_mode::PlanModeTracker::new(PathBuf::from("/tmp/test-session")),
            )),
            current_prompt_mode: Arc::new(parking_lot::Mutex::new(
                crate::session::plan_mode::PromptMode::Agent,
            )),
            turn_prompt_mode: Arc::new(parking_lot::Mutex::new(
                crate::session::plan_mode::PromptMode::Agent,
            )),
            session_cmd_tx,
            auto_wake_delivered:
                xai_grok_tools::reminders::task_completion::AutoWakeDeliveredIds::default(),
            synthetic_trace_tx: Arc::new(std::sync::Mutex::new(None)),
            task_output_tool_name: Arc::new(std::sync::OnceLock::new()),
            read_tool_name: Arc::new(std::sync::OnceLock::new()),
            auto_wake_enabled: true,
            queue_exit_reminder_on_approved_exit: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
            goal_loop_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        (config, gateway_rx, persistence_rx, session_cmd_rx)
    }

    fn make_task_snapshot(task_id: &str, kind: TaskKind) -> TaskSnapshot {
        TaskSnapshot {
            task_id: task_id.into(),
            command: "echo test".into(),
            display_command: None,
            cwd: String::new(),
            start_time: std::time::SystemTime::now(),
            end_time: Some(std::time::SystemTime::now()),
            output: String::new(),
            output_file: PathBuf::new(),
            truncated: false,
            exit_code: Some(0),
            signal: None,
            completed: true,
            kind,
            block_waited: false,
            explicitly_killed: false,
            owner_session_id: None,
        }
    }

    #[tokio::test]
    async fn bash_task_completed_injects_bash_task_completed_source() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let snapshot = make_task_snapshot("bg-123", TaskKind::Bash);
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        // Auto-wake sends CopyFile first, then Prompt (not InjectNotification).
        let cmd1 = cmd_rx.try_recv().expect("expected CopyFile");
        assert!(matches!(cmd1, SessionCommand::CopyFile { .. }));

        let cmd2 = cmd_rx.try_recv().expect("expected Prompt");
        match cmd2 {
            SessionCommand::Prompt {
                prompt_id,
                prompt_blocks,
                verbatim,
                ..
            } => {
                assert!(prompt_id.starts_with("task-completed-"));
                assert!(verbatim);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    _ => panic!("expected text block"),
                };
                assert!(text.contains("bg-123"));
                assert!(text.contains("exit code: 0"));
                assert!(text.contains(r#"get_command_or_subagent_output("bg-123")"#));
                assert!(!text.contains(r#"get_task_output("bg-123")"#));
            }
            _ => panic!("expected Prompt"),
        }

        let cmd3 = cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete");
        match cmd3 {
            SessionCommand::DispatchNotificationHook {
                notification_type,
                message,
                ..
            } => {
                assert_eq!(notification_type, "task_complete");
                assert_eq!(
                    message.as_deref(),
                    Some("Background task completed: bg-123")
                );
            }
            _ => panic!("expected DispatchNotificationHook"),
        }
    }

    /// Gap 1: while a goal loop is active, a completed background bash task
    /// must NOT fire the synthetic auto-wake prompt — an async "task completed"
    /// wake mid-goal derails a weak model. It must also NOT be marked
    /// auto-wake-delivered (so surface 2's `TaskCompletionReminder` is free to
    /// drain it). The pager's `x.ai/task_completed` notification still fires.
    #[tokio::test]
    async fn bash_task_completed_suppresses_auto_wake_during_goal_loop() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        config
            .goal_loop_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = make_task_snapshot("bg-goal", TaskKind::Bash);
        let mut offsets = HashMap::new();

        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;

        // No synthetic prompt / CopyFile / InjectNotification while the goal
        // loop drives the turn — only the Notification hook dispatch.
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => assert_eq!(notification_type, "task_complete"),
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "goal-loop-active bash completion must not inject auto-wake commands"
        );
        // Not marked auto-wake-delivered: surface 2 must be free to drain it.
        assert!(
            config.auto_wake_delivered.snapshot().is_empty(),
            "goal-loop-active completion must not be marked auto-wake-delivered"
        );
        // The pager UI notification must still be emitted.
        let mut found_ext = false;
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                && args.request.method.as_ref() == "x.ai/task_completed"
            {
                found_ext = true;
            }
        }
        assert!(
            found_ext,
            "x.ai/task_completed ExtNotification must still be sent for UI"
        );
    }

    /// Gap 1 (preserve non-goal behavior): with the goal loop inactive — the
    /// default for a normal session — a completed bash task DOES fire the
    /// synthetic auto-wake prompt AND is marked auto-wake-delivered so surface
    /// 2 suppresses the duplicate reminder.
    #[tokio::test]
    async fn bash_task_completed_auto_wakes_and_marks_delivered_without_goal_loop() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        // goal_loop_active defaults to false (normal session).
        let snapshot = make_task_snapshot("bg-normal", TaskKind::Bash);
        let mut offsets = HashMap::new();

        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;

        // Auto-wake sends CopyFile, then the synthetic Prompt.
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::CopyFile { .. })
        ));
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::Prompt { .. })
        ));
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::DispatchNotificationHook { .. })
        ));
        // And the task IS marked auto-wake-delivered.
        assert_eq!(
            config.auto_wake_delivered.snapshot(),
            vec!["bg-normal".to_string()],
        );
    }

    /// `will_wake` off the emitted `x.ai/task_completed` params.
    fn task_completed_will_wake(
        gateway_rx: &mut mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
    ) -> Option<bool> {
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                && args.request.method.as_ref() == "x.ai/task_completed"
            {
                let v: serde_json::Value = serde_json::from_str(args.request.params.get()).ok()?;
                return v["update"]["will_wake"].as_bool();
            }
        }
        None
    }

    /// The completion notification carries the wake verdict — the pager keys
    /// its between-turns status line on it (skip when a wake response
    /// follows, emit when nothing else will mark the moment).
    #[tokio::test]
    async fn task_completed_notification_stamps_will_wake() {
        // Wake leg: auto-wake enabled, no suppression.
        let (config, mut gateway_rx, _persistence_rx, _cmd_rx) = make_test_config_full();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("bg-wake", TaskKind::Bash)),
            &mut offsets,
        )
        .await;
        assert_eq!(
            task_completed_will_wake(&mut gateway_rx),
            Some(true),
            "an auto-woken completion must stamp will_wake: true"
        );

        // Suppressed leg: goal loop active — no wake follows the chip.
        let (config, mut gateway_rx, _persistence_rx, _cmd_rx) = make_test_config_full();
        config
            .goal_loop_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("bg-goal", TaskKind::Bash)),
            &mut offsets,
        )
        .await;
        assert_eq!(
            task_completed_will_wake(&mut gateway_rx),
            Some(false),
            "a suppressed completion must stamp will_wake: false"
        );
    }

    /// Dead session actor: the synthetic Prompt enqueue fails, so no wake will
    /// ever run — the notification must stamp `will_wake: false`, not promise a
    /// wake the send didn't queue (the pager would suppress its between-turns
    /// status line for a wake that never comes).
    #[tokio::test]
    async fn task_completed_stamps_will_wake_false_when_session_channel_closed() {
        let (config, mut gateway_rx, _persistence_rx, cmd_rx) = make_test_config_full();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        drop(cmd_rx);
        let mut offsets = HashMap::new();
        handle_notification(
            &config,
            ToolNotification::TaskCompleted(make_task_snapshot("bg-dead", TaskKind::Bash)),
            &mut offsets,
        )
        .await;
        assert_eq!(
            task_completed_will_wake(&mut gateway_rx),
            Some(false),
            "a completion whose wake prompt could not be enqueued must stamp will_wake: false"
        );
    }

    /// Gap 1 (adjacent branch): the goal-loop arm sits BEFORE the
    /// `auto_wake_enabled == false` `InjectNotification` fallback, so an
    /// auto-wake-DISABLED completion mid-goal must also be suppressed — it must
    /// NOT fall through to the idle-gated `InjectNotification`. Guards against a
    /// future reorder that would leak a mid-goal notification.
    #[tokio::test]
    async fn bash_task_completed_auto_wake_disabled_still_suppressed_during_goal_loop() {
        let (mut config, mut cmd_rx) = make_test_config();
        config.auto_wake_enabled = false;
        config
            .goal_loop_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = make_task_snapshot("bg-disabled-goal", TaskKind::Bash);
        let mut offsets = HashMap::new();

        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;

        // No InjectNotification during the goal loop, even with auto-wake disabled.
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => assert_eq!(notification_type, "task_complete"),
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "goal-loop-active completion must not InjectNotification with auto-wake disabled"
        );
        // And not marked auto-wake-delivered.
        assert!(config.auto_wake_delivered.snapshot().is_empty());
    }

    /// Natural monitor exit (including exit code 0) must immediate-auto-wake
    /// the same way bash does — not only via the idle-gated MonitorEvent path.
    /// Also drops queued MonitorEvents so a second NotificationDrain turn is
    /// not started for the same completion.
    #[tokio::test]
    async fn monitor_task_completed_auto_wakes_with_monitor_ended_message() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let mut snapshot = make_task_snapshot("mon-456", TaskKind::Monitor);
        snapshot.display_command = Some("[monitor] watch deploy".into());
        snapshot.command = "tail -f deploy.log".into();
        snapshot.exit_code = Some(0);
        let mut offsets = HashMap::new();

        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;

        // Drop queued pipeline events first (sole-wake guarantee).
        match cmd_rx
            .try_recv()
            .expect("expected DropMonitorNotifications")
        {
            SessionCommand::DropMonitorNotifications { task_id } => {
                assert_eq!(task_id, "mon-456");
            }
            _ => panic!("expected DropMonitorNotifications before auto-wake Prompt"),
        }
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::CopyFile { .. })
        ));
        let cmd = cmd_rx.try_recv().expect("expected Prompt auto-wake");
        match cmd {
            SessionCommand::Prompt {
                prompt_id,
                prompt_blocks,
                verbatim,
                ..
            } => {
                assert_eq!(prompt_id, "task-completed-mon-456");
                assert!(verbatim);
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => t.text.as_str(),
                    _ => panic!("expected text block"),
                };
                assert!(
                    text.contains("[monitor ended: exited (code 0)]"),
                    "auto-wake must carry the terminal ended wording: {text}"
                );
                assert!(
                    text.contains("watch deploy"),
                    "auto-wake should include the monitor description: {text}"
                );
                assert!(
                    text.contains("get_command_or_subagent_output(\"mon-456\")"),
                    "auto-wake should point at the poll tool: {text}"
                );
            }
            _ => panic!("expected Prompt auto-wake for natural monitor exit"),
        }
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SessionCommand::DispatchNotificationHook { .. })
        ));
        assert_eq!(
            config.auto_wake_delivered.snapshot(),
            vec!["mon-456".to_string()],
        );
    }

    /// After TaskCompleted auto-wake marked the task delivered, late pipeline
    /// MonitorEvents must not inject another model-facing notification.
    #[tokio::test]
    async fn monitor_event_skipped_after_task_completed_auto_wake() {
        let (config, mut cmd_rx) = make_test_config();
        config.auto_wake_delivered.insert("mon-done".into());
        let mut offsets = HashMap::new();

        handle_notification(
            &config,
            ToolNotification::MonitorEvent(xai_grok_tools::notification::types::MonitorEvent {
                task_id: "mon-done".into(),
                description: "short exit".into(),
                event_text: "<monitor-event>done</monitor-event>".into(),
                raw_text: "done".into(),
                owner_session_id: Some("test-session".into()),
            }),
            &mut offsets,
        )
        .await;

        // No InjectNotification — only the TaskCompleted wake should talk to the model.
        assert!(
            cmd_rx.try_recv().is_err(),
            "post-auto-wake MonitorEvent must not InjectNotification"
        );
    }

    /// Explicit kill of a monitor still skips auto-wake — the model already
    /// got the kill_task tool result.
    #[tokio::test]
    async fn monitor_explicitly_killed_skips_auto_wake() {
        let (config, mut cmd_rx) = make_test_config();
        let mut snapshot = make_task_snapshot("mon-killed", TaskKind::Monitor);
        snapshot.explicitly_killed = true;
        let mut offsets = HashMap::new();

        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;

        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => assert_eq!(notification_type, "task_complete"),
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "explicitly-killed monitor must not auto-wake"
        );
        assert!(config.auto_wake_delivered.snapshot().is_empty());
    }

    /// Goal-loop suppression applies to monitor completions too.
    #[tokio::test]
    async fn monitor_task_completed_suppressed_during_goal_loop() {
        let (config, mut cmd_rx) = make_test_config();
        config
            .goal_loop_active
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let snapshot = make_task_snapshot("mon-goal", TaskKind::Monitor);
        let mut offsets = HashMap::new();

        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;

        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => assert_eq!(notification_type, "task_complete"),
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "goal-loop-active monitor completion must not auto-wake"
        );
        assert!(config.auto_wake_delivered.snapshot().is_empty());
    }

    #[tokio::test]
    async fn scheduled_task_created_is_persisted() {
        // A `/loop` create must be persisted (like TaskBackgrounded) so a
        // second terminal that resumes the session restores the loop from
        // replay — otherwise it stays invisible until the loop next fires.
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::ScheduledTaskCreated(
            xai_grok_tools::notification::types::ScheduledTaskCreated {
                task_id: "loop-1".into(),
                prompt: "check deploy".into(),
                human_schedule: "every 5 minutes".into(),
                next_fire_at: Some("2026-01-01T00:00:00Z".into()),
            },
        );
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        let msg = persistence_rx
            .try_recv()
            .expect("scheduled_task_created must be persisted");
        match msg {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                assert!(matches!(
                    &notif.update,
                    crate::extensions::notification::SessionUpdate::ScheduledTaskCreated { .. }
                ));
                assert!(
                    notif
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("eventId"))
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| id.starts_with("test-session-")),
                    "persisted xAI bridge lines must carry an eventId"
                );
            }
            _ => panic!("expected PersistenceMsg::Update(Xai(ScheduledTaskCreated))"),
        }
    }

    /// Persisted⇒stamped contract at the bridge's highest-frequency emitter:
    /// the persisted bash-output line carries an `eventId`, and the live
    /// broadcast carries the SAME id (the meta is minted before the
    /// persist/broadcast fork — divergent ids would re-deliver the line on a
    /// cursor reconnect).
    #[tokio::test]
    async fn bash_output_chunk_persists_and_broadcasts_one_event_id() {
        let (config, mut gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::BashOutputChunk(
            xai_grok_tools::notification::types::BashOutputChunk {
                base: xai_grok_tools::notification::types::BashNotificationBase {
                    tool_call_id: "call-1".into(),
                    command: "echo hi".into(),
                    output: b"hi\n".to_vec(),
                    total_bytes: 3,
                    truncated: false,
                    cwd: PathBuf::from("/tmp"),
                },
            },
        );
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        let persisted_id = match persistence_rx.try_recv().expect("chunk must be persisted") {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) => notif
                .meta
                .as_ref()
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str())
                .expect("persisted ACP bridge lines must carry an eventId")
                .to_string(),
            other => panic!("expected PersistenceMsg::Update(Acp(..)), got {other:?}"),
        };

        let broadcast_id = match gateway_rx.try_recv().expect("chunk must be broadcast") {
            xai_acp_lib::AcpClientMessage::SessionNotification(args) => args
                .request
                .meta
                .as_ref()
                .and_then(|m| m.get("eventId"))
                .and_then(|v| v.as_str())
                .expect("broadcast must carry the eventId")
                .to_string(),
            other => panic!("expected SessionNotification, got {other:?}"),
        };
        assert_eq!(persisted_id, broadcast_id);
    }

    #[tokio::test]
    async fn scheduled_task_removed_is_persisted() {
        // The deletion must also persist so replay nets out a removed loop
        // instead of resurrecting it from the persisted `created` line.
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::ScheduledTaskRemoved(
            xai_grok_tools::notification::types::ScheduledTaskRemoved {
                task_id: "loop-1".into(),
            },
        );
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        let msg = persistence_rx
            .try_recv()
            .expect("scheduled_task_removed must be persisted");
        match msg {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                assert!(matches!(
                    &notif.update,
                    crate::extensions::notification::SessionUpdate::ScheduledTaskDeleted { .. }
                ));
                assert!(
                    xai_persisted_event_id(&notif).is_some(),
                    "the persisted deletion line must be stamped"
                );
            }
            _ => panic!("expected PersistenceMsg::Update(Xai(ScheduledTaskDeleted))"),
        }
    }

    fn xai_persisted_event_id(
        notif: &crate::extensions::notification::SessionNotification,
    ) -> Option<String> {
        notif
            .meta
            .as_ref()
            .and_then(|m| m.get("eventId"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// Per-site stamp pins for the bridge emitters not covered by the
    /// representative chokepoint tests: deleting any one `stamp_event_id`
    /// call must fail a test (an id-less persisted line silently disables
    /// incremental reconnect for the session).
    #[tokio::test]
    async fn task_backgrounded_persisted_line_is_stamped() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::BashExecutionBackgrounded(
            xai_grok_tools::notification::types::BashExecutionBackgrounded {
                base: xai_grok_tools::notification::types::BashNotificationBase {
                    tool_call_id: "call-bg".into(),
                    command: "sleep 100".into(),
                    output: Vec::new(),
                    total_bytes: 0,
                    truncated: false,
                    cwd: PathBuf::from("/tmp"),
                },
                output_file: PathBuf::from("/tmp/out.log"),
                task_id: "task-bg".into(),
                monitor_description: None,
                description: None,
            },
        );
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        match persistence_rx.try_recv().expect("must persist") {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                assert!(xai_persisted_event_id(&notif).is_some());
            }
            _ => panic!("expected Xai update"),
        }
    }

    #[tokio::test]
    async fn task_completed_persisted_line_is_stamped() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        // Monitor kind: persists without the bash auto-wake side effects.
        let snapshot = make_task_snapshot("mon-1", TaskKind::Monitor);
        let mut offsets = HashMap::new();

        handle_notification(
            &config,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;

        match persistence_rx.try_recv().expect("must persist") {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(notif)) => {
                assert!(xai_persisted_event_id(&notif).is_some());
            }
            _ => panic!("expected Xai update"),
        }
    }

    #[tokio::test]
    async fn current_mode_update_persisted_line_is_stamped() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();

        emit_current_mode_update(&config, xai_grok_tools::types::SessionMode::Plan).await;

        match persistence_rx.try_recv().expect("must persist") {
            PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) => {
                assert!(matches!(
                    notif.update,
                    acp::SessionUpdate::CurrentModeUpdate(_)
                ));
                assert!(
                    notif
                        .meta
                        .as_ref()
                        .and_then(|m| m.get("eventId"))
                        .and_then(|v| v.as_str())
                        .is_some(),
                    "the persisted mode line must be stamped"
                );
            }
            _ => panic!("expected Acp update"),
        }
    }

    #[tokio::test]
    async fn scheduled_task_fired_is_not_persisted() {
        // `_fired` recurs on every interval; persisting it would grow the
        // updates log without bound. Loops are restored from create/delete, so
        // the fire stays gateway-only (the pager self-heals the entry on a live
        // fire if needed).
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        let notification = ToolNotification::ScheduledTaskFired(
            xai_grok_tools::notification::types::ScheduledTaskFired {
                task_id: "loop-1".into(),
                prompt: "check deploy".into(),
                human_schedule: "every 5 minutes".into(),
                next_fire_at: Some("2026-01-01T00:00:00Z".into()),
            },
        );
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        assert!(
            persistence_rx.try_recv().is_err(),
            "scheduled_task_fired must NOT be persisted (recurring \u{2192} unbounded log growth)"
        );
    }

    fn make_monitor_event_notification(task_id: &str, owner: Option<&str>) -> ToolNotification {
        ToolNotification::MonitorEvent(xai_grok_tools::notification::types::MonitorEvent {
            task_id: task_id.into(),
            description: "errors in deploy.log".into(),
            event_text: format!("<monitor-event task_id=\"{task_id}\">boom</monitor-event>"),
            raw_text: "boom".into(),
            owner_session_id: owner.map(str::to_string),
        })
    }

    #[tokio::test]
    async fn cross_session_monitor_event_is_dropped() {
        // The bridge belongs to "test-session"; the event is owned by a
        // different session. In leader mode (one agent process, many sessions)
        // this is the cross-session leak: without the owner guard the foreign
        // monitor would inject a `<monitor-event>` reminder into this session's
        // conversation. Assert it is fully dropped — no conversation injection
        // and no pager forward.
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        let notification = make_monitor_event_notification("mon-foreign", Some("other-session"));
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        assert!(
            cmd_rx.try_recv().is_err(),
            "cross-session monitor event must not be injected into this session"
        );
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg {
                assert_ne!(
                    args.request.method.as_ref(),
                    "x.ai/monitor_event",
                    "cross-session monitor event must not be forwarded to the pager"
                );
            }
        }
    }

    #[tokio::test]
    async fn same_session_monitor_event_is_injected() {
        // Owner matches the bridge's own session id ("test-session") -> deliver.
        let (config, mut cmd_rx) = make_test_config();
        let notification = make_monitor_event_notification("mon-own", Some("test-session"));
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        match cmd_rx
            .try_recv()
            .expect("own-session monitor event must be injected")
        {
            SessionCommand::InjectNotification { source, .. } => match source {
                NotificationSource::MonitorEvent { task_id } => assert_eq!(task_id, "mon-own"),
                _ => panic!("expected MonitorEvent notification source"),
            },
            _ => panic!("expected InjectNotification"),
        }
    }

    #[tokio::test]
    async fn legacy_monitor_event_without_owner_is_injected() {
        // Legacy / non-grok-build backends record no owner; such events must
        // pass through unchanged for backwards compatibility.
        let (config, mut cmd_rx) = make_test_config();
        let notification = make_monitor_event_notification("mon-legacy", None);
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        assert!(
            matches!(
                cmd_rx
                    .try_recv()
                    .expect("legacy (no-owner) monitor event must be injected"),
                SessionCommand::InjectNotification {
                    source: NotificationSource::MonitorEvent { .. },
                    ..
                }
            ),
            "legacy monitor event should be injected as a MonitorEvent notification"
        );
    }

    #[tokio::test]
    async fn block_waited_task_skips_auto_wake_prompt() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        let mut snapshot = make_task_snapshot("bg-waited", TaskKind::Bash);
        snapshot.block_waited = true;
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        // block_waited tasks must NOT inject a synthetic prompt — the
        // blocking caller already received the result directly.
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => assert_eq!(notification_type, "task_complete"),
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "block_waited completion should not send Prompt or InjectNotification"
        );

        // The x.ai/task_completed ExtNotification for UI updates must still be sent.
        let mut found_ext = false;
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                && args.request.method.as_ref() == "x.ai/task_completed"
            {
                found_ext = true;
            }
        }
        assert!(
            found_ext,
            "x.ai/task_completed ExtNotification must still be sent for UI"
        );
    }

    #[tokio::test]
    async fn explicitly_killed_task_skips_auto_wake_prompt() {
        let (config, mut gateway_rx, _persistence_rx, mut cmd_rx) = make_test_config_full();
        let mut snapshot = make_task_snapshot("bg-killed", TaskKind::Bash);
        snapshot.explicitly_killed = true;
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        // explicitly_killed tasks must NOT inject a synthetic prompt — the
        // model already received the KillTaskResult from the kill tool.
        match cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete")
        {
            SessionCommand::DispatchNotificationHook {
                notification_type, ..
            } => assert_eq!(notification_type, "task_complete"),
            _ => panic!("unexpected session command"),
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "explicitly_killed completion should not send Prompt or InjectNotification"
        );

        // The x.ai/task_completed ExtNotification for UI updates must still be sent.
        let mut found_ext = false;
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                && args.request.method.as_ref() == "x.ai/task_completed"
            {
                found_ext = true;
            }
        }
        assert!(
            found_ext,
            "x.ai/task_completed ExtNotification must still be sent for UI"
        );
    }

    #[tokio::test]
    async fn bash_task_completed_falls_back_when_auto_wake_disabled() {
        let (mut config, mut cmd_rx) = make_test_config();
        config.auto_wake_enabled = false;
        config
            .task_output_tool_name
            .set(Some("get_command_or_subagent_output".to_string()))
            .expect("slot is fresh in this test fixture");
        let snapshot = make_task_snapshot("bg-disabled", TaskKind::Bash);
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        // With auto-wake disabled, should use InjectNotification (not Prompt).
        let cmd = cmd_rx.try_recv().expect("expected InjectNotification");
        match cmd {
            SessionCommand::InjectNotification {
                prompt_id,
                prompt_blocks,
                priority,
                source,
                ..
            } => {
                assert!(prompt_id.starts_with("bash-completed-"));
                assert_eq!(priority, NotificationPriority::Later);
                assert!(matches!(
                    source,
                    NotificationSource::BashTaskCompleted { ref task_id } if task_id == "bg-disabled"
                ));
                let text = match &prompt_blocks[0] {
                    acp::ContentBlock::Text(t) => &t.text,
                    _ => panic!("expected text block"),
                };
                assert!(text.contains(r#"get_command_or_subagent_output("bg-disabled")"#));
                assert!(!text.contains(r#"get_task_output("bg-disabled")"#));
                assert!(!text.contains("response:"));
            }
            _ => panic!("expected InjectNotification"),
        }

        let hook_cmd = cmd_rx
            .try_recv()
            .expect("expected DispatchNotificationHook for task_complete");
        match hook_cmd {
            SessionCommand::DispatchNotificationHook {
                notification_type,
                message,
                ..
            } => {
                assert_eq!(notification_type, "task_complete");
                assert_eq!(
                    message.as_deref(),
                    Some("Background task completed: bg-disabled")
                );
            }
            _ => panic!("expected DispatchNotificationHook"),
        }
    }

    #[tokio::test]
    async fn bash_completion_uses_single_task_id_clone() {
        // Verify the task_id appears in the prompt_id (auto-wake path).
        let (config, mut cmd_rx) = make_test_config();
        let snapshot = make_task_snapshot("unique-id-789", TaskKind::Bash);
        let notification = ToolNotification::TaskCompleted(snapshot);
        let mut offsets = HashMap::new();

        handle_notification(&config, notification, &mut offsets).await;

        // Skip CopyFile
        let _ = cmd_rx.try_recv().unwrap();
        let cmd = cmd_rx.try_recv().unwrap();
        if let SessionCommand::Prompt { prompt_id, .. } = cmd {
            assert_eq!(prompt_id, "task-completed-unique-id-789");
        } else {
            panic!("expected Prompt");
        }
    }

    fn extract_current_mode_id(notification: &acp::SessionNotification) -> Option<&str> {
        match &notification.update {
            acp::SessionUpdate::CurrentModeUpdate(cmu) => Some(cmu.current_mode_id.0.as_ref()),
            _ => None,
        }
    }

    /// Regression: `PlanModeExited` must emit `CurrentModeUpdate("default")`
    /// onto both the gateway and the persistence stream. Without this,
    /// agent-driven plan approvals leave the TUI stuck in plan mode.
    #[tokio::test]
    async fn plan_mode_exited_emits_current_mode_update_default() {
        let (config, mut gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();

        // Pre-condition: agent path requires plan mode to be Active first
        // so `deactivate_approved` actually flips state and triggers the emit.
        {
            let mut tracker = config.plan_mode.lock();
            assert!(tracker.activate_from_tool());
        }
        *config.current_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Plan;
        *config.turn_prompt_mode.lock() = crate::session::plan_mode::PromptMode::Plan;

        let notification =
            ToolNotification::PlanModeExited(xai_grok_tools::notification::types::PlanModeExited {
                tool_call_id: "tc-exit-1".into(),
                plan_content: Some("- step 1".into()),
                plan_file_path: "/tmp/test-session/plan.md".into(),
            });

        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;

        // Gateway: one CurrentModeUpdate("default").
        let mut gateway_modes = Vec::new();
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg
                && let Some(id) = extract_current_mode_id(&args.request)
            {
                gateway_modes.push(id.to_string());
            }
        }
        assert_eq!(
            gateway_modes,
            vec!["default".to_string()],
            "PlanModeExited should emit exactly one CurrentModeUpdate(default) to the gateway"
        );

        // Persistence: same notification persisted so replay re-applies the exit.
        let mut persisted_modes = Vec::new();
        while let Ok(msg) = persistence_rx.try_recv() {
            if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) = msg
                && let Some(id) = extract_current_mode_id(&notif)
            {
                persisted_modes.push(id.to_string());
            }
        }
        assert_eq!(
            persisted_modes,
            vec!["default".to_string()],
            "PlanModeExited should persist exactly one CurrentModeUpdate(default)"
        );

        // Session-level prompt mode was reset.
        assert!(matches!(
            *config.current_prompt_mode.lock(),
            crate::session::plan_mode::PromptMode::Agent
        ));
    }

    /// Default (grok) polarity: the exit_plan_mode tool result is the model's
    /// only exit signal, so an approved `PlanModeExited` must NOT arm the
    /// deferred exit reminder — in memory or in the persisted snapshot.
    /// Sibling of `plan_mode_exited_arms_exit_reminder_when_gated`.
    #[tokio::test]
    async fn plan_mode_exited_does_not_arm_exit_reminder_by_default() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();

        {
            let mut tracker = config.plan_mode.lock();
            assert!(tracker.activate_from_tool());
        }

        let notification =
            ToolNotification::PlanModeExited(xai_grok_tools::notification::types::PlanModeExited {
                tool_call_id: "tc-exit-grok".into(),
                plan_content: Some("- step 1".into()),
                plan_file_path: "/tmp/test-session/plan.md".into(),
            });

        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;

        assert!(
            !config.plan_mode.lock().has_pending_exit_reminder(),
            "approved exit must not arm the deferred exit reminder"
        );
        let mut persisted_plan_snapshots = Vec::new();
        while let Ok(msg) = persistence_rx.try_recv() {
            if let PersistenceMsg::PlanModeState(snapshot) = msg {
                persisted_plan_snapshots.push(snapshot);
            }
        }
        assert!(
            !persisted_plan_snapshots.is_empty()
                && persisted_plan_snapshots
                    .iter()
                    .all(|s| !s.pending_exit_reminder),
            "persisted plan-mode snapshot must not carry the exit reminder"
        );
    }

    /// Gated counterpart: when `queue_exit_reminder_on_approved_exit` is
    /// set, an approved `PlanModeExited` must arm the next-turn exit
    /// reminder and persist it.
    #[tokio::test]
    async fn plan_mode_exited_arms_exit_reminder_when_gated() {
        let (config, _gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();
        config
            .queue_exit_reminder_on_approved_exit
            .store(true, std::sync::atomic::Ordering::Relaxed);

        {
            let mut tracker = config.plan_mode.lock();
            assert!(tracker.activate_from_tool());
        }

        let notification =
            ToolNotification::PlanModeExited(xai_grok_tools::notification::types::PlanModeExited {
                tool_call_id: "tc-exit-gated".into(),
                plan_content: Some("- step 1".into()),
                plan_file_path: "/tmp/test-session/plan.md".into(),
            });

        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;

        assert!(
            config.plan_mode.lock().has_pending_exit_reminder(),
            "gated approved exit must arm the next-turn exit reminder"
        );
        let mut persisted_plan_snapshots = Vec::new();
        while let Ok(msg) = persistence_rx.try_recv() {
            if let PersistenceMsg::PlanModeState(snapshot) = msg {
                persisted_plan_snapshots.push(snapshot);
            }
        }
        assert!(
            !persisted_plan_snapshots.is_empty()
                && persisted_plan_snapshots
                    .iter()
                    .all(|s| s.pending_exit_reminder),
            "persisted plan-mode snapshot must carry the armed exit reminder"
        );
    }

    /// Symmetric to the exit test: `PlanModeEntered` emits
    /// `CurrentModeUpdate("plan")`.
    #[tokio::test]
    async fn plan_mode_entered_emits_current_mode_update_plan() {
        let (config, mut gateway_rx, mut persistence_rx, _cmd_rx) = make_test_config_full();

        let notification = ToolNotification::PlanModeEntered(
            xai_grok_tools::notification::types::PlanModeEntered {
                tool_call_id: "tc-enter-1".into(),
            },
        );

        let mut offsets = HashMap::new();
        handle_notification(&config, notification, &mut offsets).await;

        let mut gateway_modes = Vec::new();
        while let Ok(msg) = gateway_rx.try_recv() {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg
                && let Some(id) = extract_current_mode_id(&args.request)
            {
                gateway_modes.push(id.to_string());
            }
        }
        assert_eq!(gateway_modes, vec!["plan".to_string()]);

        let mut persisted_modes = Vec::new();
        while let Ok(msg) = persistence_rx.try_recv() {
            if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(notif)) = msg
                && let Some(id) = extract_current_mode_id(&notif)
            {
                persisted_modes.push(id.to_string());
            }
        }
        assert_eq!(persisted_modes, vec!["plan".to_string()]);
    }

    /// Build a completed-bash `TaskSnapshot` whose `output` is large enough
    /// to trip the inline-completion truncation cap, with a concrete
    /// `output_file` path so the disk-pointer footer is exercised end-to-end.
    fn make_large_bash_snapshot(task_id: &str, output_file: PathBuf) -> TaskSnapshot {
        TaskSnapshot {
            task_id: task_id.into(),
            command: "yes hello | head -c 20000".into(),
            display_command: None,
            cwd: String::new(),
            start_time: std::time::SystemTime::now(),
            end_time: Some(std::time::SystemTime::now()),
            output: "h".repeat(20_000),
            output_file,
            truncated: true,
            exit_code: Some(0),
            signal: None,
            completed: true,
            kind: TaskKind::Bash,
            block_waited: false,
            explicitly_killed: false,
            owner_session_id: None,
        }
    }

    /// Extract the auto-wake prompt text emitted on the session command channel.
    fn auto_wake_prompt_text(cmd_rx: &mut mpsc::UnboundedReceiver<SessionCommand>) -> String {
        let _ = cmd_rx.try_recv().expect("expected CopyFile");
        let cmd = cmd_rx.try_recv().expect("expected Prompt");
        match cmd {
            SessionCommand::Prompt { prompt_blocks, .. } => match &prompt_blocks[0] {
                acp::ContentBlock::Text(t) => t.text.clone(),
                _ => panic!("expected text block"),
            },
            _ => panic!("expected Prompt"),
        }
    }

    /// Extract the InjectNotification prompt text emitted on the session
    /// command channel (auto-wake-disabled fallback path).
    fn inject_notification_prompt_text(
        cmd_rx: &mut mpsc::UnboundedReceiver<SessionCommand>,
    ) -> String {
        let cmd = cmd_rx.try_recv().expect("expected InjectNotification");
        match cmd {
            SessionCommand::InjectNotification { prompt_blocks, .. } => match &prompt_blocks[0] {
                acp::ContentBlock::Text(t) => t.text.clone(),
                _ => panic!("expected text block"),
            },
            _ => panic!("expected InjectNotification"),
        }
    }

    /// Bash completion with a large output and no polling tool (compat-harness
    /// toolset) renders the truncation marker AND the disk-pointer footer
    /// pointing the model at `output_file` via the resolved Read tool name.
    /// Covers BOTH the auto-wake branch and the auto-wake-disabled fallback
    /// so the truncation + footer behaviour stays consistent across both
    /// completion-injection paths.
    #[tokio::test]
    async fn bash_completion_renders_disk_pointer_footer_in_both_branches() {
        let output_file = PathBuf::from("/tmp/bg-disk-pointer.log");

        let (config_auto, mut cmd_rx_auto) = make_test_config();
        config_auto
            .read_tool_name
            .set(Some("read_file".to_string()))
            .expect("fresh slot");
        let snapshot = make_large_bash_snapshot("bg-disk-1", output_file.clone());
        let mut offsets = HashMap::new();
        handle_notification(
            &config_auto,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;
        let prompt = auto_wake_prompt_text(&mut cmd_rx_auto);
        assert!(
            prompt.contains("[Output truncated"),
            "auto-wake: expected truncation marker, got: {prompt}"
        );
        let expected_footer = format!(
            "Use read_file on {} for full content",
            output_file.display()
        );
        assert!(
            prompt.contains(&expected_footer),
            "auto-wake: expected disk-pointer footer `{expected_footer}`, got: {prompt}"
        );
        assert!(
            prompt.contains("bg-disk-1"),
            "auto-wake: prompt must reference task id"
        );

        let (mut config_no_wake, mut cmd_rx_no_wake) = make_test_config();
        config_no_wake.auto_wake_enabled = false;
        config_no_wake
            .read_tool_name
            .set(Some("read_file".to_string()))
            .expect("fresh slot");
        let snapshot = make_large_bash_snapshot("bg-disk-2", output_file.clone());
        let mut offsets = HashMap::new();
        handle_notification(
            &config_no_wake,
            ToolNotification::TaskCompleted(snapshot),
            &mut offsets,
        )
        .await;
        let prompt = inject_notification_prompt_text(&mut cmd_rx_no_wake);
        assert!(
            prompt.contains("[Output truncated"),
            "auto-wake-disabled: expected truncation marker, got: {prompt}"
        );
        let expected_footer = format!(
            "Use read_file on {} for full content",
            output_file.display()
        );
        assert!(
            prompt.contains(&expected_footer),
            "auto-wake-disabled: expected disk-pointer footer `{expected_footer}`, got: {prompt}"
        );
        assert!(
            prompt.contains("bg-disk-2"),
            "auto-wake-disabled: prompt must reference task id"
        );
    }
}
