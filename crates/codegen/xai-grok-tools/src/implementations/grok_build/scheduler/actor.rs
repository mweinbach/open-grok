use std::time::Duration;

use chrono::Utc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::implementations::grok_build::task::types::{
    SessionIdResource, SubagentEvent, SubagentEventSender, SubagentQueryRequest, SubagentRequest,
    SubagentRuntimeOverrides, SubagentSnapshotStatus,
};
use crate::notification::types::ToolNotificationHandle;
use crate::notification::{ScheduledTaskCreated, ScheduledTaskFired, ScheduledTaskRemoved};
use crate::reminders::format_loop_iteration_prompt;
use crate::types::resources::{SharedResources, State};

use super::interval::interval_to_human;
use super::types::{
    LOOP_COMPLETION_OUTPUT_CAP, LOOP_FRESH_CHAIN_EVERY, ScheduledTask, SchedulerCommand,
    SchedulerError, SchedulerState,
};

const MAX_SCHEDULED_TASKS: usize = 50;

enum LoopFireOutcome {
    Spawned(String),
    Foreground,
    Skipped,
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}\u{2026}")
}

/// Build a `ScheduledTaskCreated` payload from a task. Shared between the
/// live `SchedulerCommand::Create` path and the post-restore re-announce so
/// the wire format stays in lockstep.
fn task_created_payload(task: &ScheduledTask) -> ScheduledTaskCreated {
    ScheduledTaskCreated {
        task_id: task.id.clone(),
        prompt: task.prompt.clone(),
        human_schedule: interval_to_human(task.interval_secs),
        next_fire_at: Some(task.next_fire_at().to_rfc3339()),
    }
}

pub struct SchedulerActor {
    pub(crate) resources: SharedResources,
    pub(crate) notification_handle: ToolNotificationHandle,
    pub(crate) cmd_rx: mpsc::UnboundedReceiver<SchedulerCommand>,
    pub(crate) cancel_token: CancellationToken,
}

impl SchedulerActor {
    pub async fn run(mut self) {
        self.await_subagent_wiring_for_due_tasks().await;
        self.announce_existing_tasks().await;

        loop {
            let next_fire = self.compute_next_fire_delay().await;

            tokio::select! {
                biased;

                _ = self.cancel_token.cancelled() => {
                    tracing::debug!("SchedulerActor shutting down (cancelled)");
                    break;
                }


                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd).await;
                }

                _ = tokio::time::sleep(next_fire) => {
                    self.fire_next_task().await;
                }
            }
        }

        // Notify the pager to remove UI chips for any remaining tasks on shutdown.
        let remaining: Vec<String> = {
            let mut res = self.resources.lock().await;
            let state = res.get_or_default::<State<SchedulerState>>();
            state.tasks.drain(..).map(|t| t.id).collect()
        };
        for task_id in remaining {
            self.notification_handle
                .send_scheduled_task_removed(ScheduledTaskRemoved { task_id });
        }
    }

    async fn await_subagent_wiring_for_due_tasks(&self) {
        const STARTUP_WIRING_GRACE: Duration = Duration::from_secs(10);
        const POLL_INTERVAL: Duration = Duration::from_millis(250);
        let deadline = tokio::time::Instant::now() + STARTUP_WIRING_GRACE;
        loop {
            let (needs_wiring, wired) = {
                let res = self.resources.lock().await;
                let enabled = res
                    .get::<crate::types::resources::SchedulerBackgroundLoops>()
                    .is_none_or(|v| v.0);
                let soon = Utc::now() + chrono::Duration::seconds(2);
                let needs_wiring = enabled
                    && res.get::<State<SchedulerState>>().is_some_and(|s| {
                        s.tasks
                            .iter()
                            .any(|t| t.recurring && !t.foreground && t.next_fire_at() <= soon)
                    });
                let wired = res.get::<SubagentEventSender>().is_some()
                    && res.get::<SessionIdResource>().is_some();
                (needs_wiring, wired)
            };
            if !needs_wiring || wired || tokio::time::Instant::now() >= deadline {
                return;
            }
            tokio::select! {
                _ = self.cancel_token.cancelled() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    }

    async fn compute_next_fire_delay(&self) -> Duration {
        let res = self.resources.lock().await;
        let scheduler_state = res.get::<State<SchedulerState>>();
        scheduler_state
            .map(|s| {
                s.tasks
                    .iter()
                    .map(|t| t.next_fire_at())
                    .min()
                    .map(|next| {
                        let now = Utc::now();
                        if next <= now {
                            Duration::ZERO
                        } else {
                            (next - now).to_std().unwrap_or(Duration::ZERO)
                        }
                    })
                    .unwrap_or(Duration::MAX)
            })
            .unwrap_or(Duration::MAX)
    }

    async fn fire_next_task(&mut self) {
        let now = Utc::now();
        let mut res = self.resources.lock().await;
        let state = res.get_or_default::<State<SchedulerState>>();
        let idx = state.tasks.iter().position(|t| t.next_fire_at() <= now);

        let Some(idx) = idx else {
            return;
        };

        let task = &mut state.tasks[idx];
        let task_id = task.id.clone();
        let prompt = task.prompt.clone();
        let human_schedule = interval_to_human(task.interval_secs);
        let foreground = task.foreground;
        let last_subagent_id = task.last_subagent_id.clone();
        let iterations_since_fresh = task.iterations_since_fresh;
        let chain_reset_pending = task.chain_reset_pending;

        // Advancing `last_fired_at` pushes `next_fire_at` forward by one
        // interval, so the task is not re-selected until it is due again.
        // Overlapping prompts are deduped downstream by stable queue-item-id.
        task.last_fired_at = Some(now);
        let next_fire_at = task.recurring.then(|| task.next_fire_at().to_rfc3339());

        if task.recurring && task.is_expired(now) {
            let expired_task_id = task_id;
            state.tasks.remove(idx);
            drop(res);
            tracing::info!(task_id = %expired_task_id, "Scheduled task expired; removing without firing");
            self.notification_handle
                .send_scheduled_task_removed(ScheduledTaskRemoved {
                    task_id: expired_task_id,
                });
            return;
        }

        let should_remove = !task.recurring;

        if should_remove {
            state.tasks.remove(idx);
        }

        let background_enabled = res
            .get::<crate::types::resources::SchedulerBackgroundLoops>()
            .is_none_or(|v| v.0);
        let spawn_deps = if foreground || should_remove || !background_enabled {
            None
        } else {
            let events = res.get::<SubagentEventSender>().cloned();
            let session = res.get::<SessionIdResource>().map(|s| s.0.clone());
            events.zip(session)
        };

        // Drop the lock before sending the notification to avoid holding it
        // across potentially blocking operations.
        drop(res);

        tracing::info!(
            task_id = %task_id,
            schedule = %human_schedule,
            background = spawn_deps.is_some(),
            "Firing scheduled task"
        );

        let removed_task_id = should_remove.then(|| task_id.clone());

        let outcome = match spawn_deps {
            Some((events, parent_session_id)) => {
                self.fire_as_loop_subagent(
                    &events,
                    parent_session_id,
                    &task_id,
                    &prompt,
                    &human_schedule,
                    last_subagent_id,
                    iterations_since_fresh,
                    chain_reset_pending,
                )
                .await
            }
            None => LoopFireOutcome::Foreground,
        };

        match outcome {
            LoopFireOutcome::Skipped => {
                // No fire happened, but last_fired_at advanced: re-announce so
                // the pager's countdown tracks the new next_fire_at instead of
                // showing the task as overdue until the next real fire.
                let payload = {
                    let res = self.resources.lock().await;
                    res.get::<State<SchedulerState>>()
                        .and_then(|s| s.tasks.iter().find(|t| t.id == task_id))
                        .map(task_created_payload)
                };
                if let Some(payload) = payload {
                    self.notification_handle
                        .send_scheduled_task_created(payload);
                }
            }
            LoopFireOutcome::Foreground => {
                self.notification_handle
                    .send_scheduled_task_fired(ScheduledTaskFired {
                        task_id,
                        prompt,
                        human_schedule,
                        next_fire_at,
                        subagent_id: None,
                    });
            }
            LoopFireOutcome::Spawned(id) => {
                self.notification_handle
                    .send_scheduled_task_fired(ScheduledTaskFired {
                        task_id,
                        prompt,
                        human_schedule,
                        next_fire_at,
                        subagent_id: Some(id),
                    });
            }
        }

        if let Some(task_id) = removed_task_id {
            self.notification_handle
                .send_scheduled_task_removed(ScheduledTaskRemoved { task_id });
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn fire_as_loop_subagent(
        &self,
        events: &SubagentEventSender,
        parent_session_id: String,
        task_id: &str,
        prompt: &str,
        human_schedule: &str,
        last_subagent_id: Option<String>,
        iterations_since_fresh: u32,
        chain_reset_pending: bool,
    ) -> LoopFireOutcome {
        let prev_snapshot = match &last_subagent_id {
            Some(prev_id) => {
                let (respond_to, rx) = tokio::sync::oneshot::channel();
                let query_sent = events
                    .0
                    .send(SubagentEvent::Query(SubagentQueryRequest {
                        subagent_id: prev_id.clone(),
                        block: false,
                        timeout_ms: None,
                        respond_to,
                    }))
                    .is_ok();
                if query_sent {
                    tokio::select! {
                        biased;
                        _ = self.cancel_token.cancelled() => return LoopFireOutcome::Skipped,
                        snapshot = rx => match snapshot {
                            Ok(s) => s,
                            Err(_) => return LoopFireOutcome::Skipped,
                        },
                        _ = tokio::time::sleep(Duration::from_secs(10)) => {
                            tracing::warn!(
                                task_id = %task_id,
                                previous_subagent = %prev_id,
                                "Loop in-flight query timed out; skipping this fire"
                            );
                            return LoopFireOutcome::Skipped;
                        }
                    }
                } else {
                    // Coordinator channel closed — cannot verify whether the
                    // previous iteration is still running. Skip rather than
                    // treating this as "no previous snapshot", which could
                    // fall through to Foreground inject and double-execute.
                    tracing::warn!(
                        task_id = %task_id,
                        previous_subagent = %prev_id,
                        "Loop in-flight query send failed (coordinator unavailable); skipping this fire"
                    );
                    return LoopFireOutcome::Skipped;
                }
            }
            None => None,
        };

        if let Some(snapshot) = &prev_snapshot
            && matches!(
                snapshot.status,
                SubagentSnapshotStatus::Initializing | SubagentSnapshotStatus::Running { .. }
            )
        {
            tracing::info!(
                task_id = %task_id,
                previous_subagent = %snapshot.subagent_id,
                "Skipping loop fire: previous iteration still running"
            );
            return LoopFireOutcome::Skipped;
        }

        let prev_completed_output = prev_snapshot.as_ref().and_then(|s| match &s.status {
            SubagentSnapshotStatus::Completed { output, .. } => Some(output.clone()),
            _ => None,
        });
        let chain_due_for_restart = iterations_since_fresh >= LOOP_FRESH_CHAIN_EVERY;
        let anchor_usable = prev_completed_output.is_some() && last_subagent_id.is_some();
        let (resume_from, prior_summary, next_iterations_since_fresh) = if chain_reset_pending {
            (None, None, 1)
        } else if chain_due_for_restart || !anchor_usable {
            let summary = prev_completed_output
                .as_deref()
                .map(|o| truncate_chars(o, 600));
            (None, summary, 1)
        } else {
            (last_subagent_id.clone(), None, iterations_since_fresh + 1)
        };

        let subagent_id = uuid::Uuid::now_v7().to_string();
        let framed_prompt =
            format_loop_iteration_prompt(prompt, task_id, human_schedule, prior_summary.as_deref());
        let description = format!(
            "loop: {} ({human_schedule})",
            truncate_chars(prompt.lines().next().unwrap_or(prompt), 60)
        );

        {
            let mut res = self.resources.lock().await;
            let state = res.get_or_default::<State<SchedulerState>>();
            let Some(task) = state.tasks.iter_mut().find(|t| t.id == task_id) else {
                tracing::info!(task_id = %task_id, "Loop task deleted mid-fire; not spawning");
                return LoopFireOutcome::Skipped;
            };
            task.last_subagent_id = Some(subagent_id.clone());
            task.iterations_since_fresh = next_iterations_since_fresh;
            task.chain_reset_pending = false;
        }

        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let request = SubagentRequest {
            id: subagent_id.clone(),
            prompt: framed_prompt,
            description,
            subagent_type: "general-purpose".to_string(),
            parent_session_id,
            parent_prompt_id: None,
            swarm: None,
            resume_from,
            cwd: None,
            runtime_overrides: SubagentRuntimeOverrides {
                completion_output_cap: Some(LOOP_COMPLETION_OUTPUT_CAP),
                spawn_depth: Some(0),
                ..Default::default()
            },
            run_in_background: true,
            surface_completion: true,
            fork_context: false,
            result_tx,
        };

        if events
            .0
            .send(SubagentEvent::Spawn(Box::new(request)))
            .is_err()
        {
            let mut res = self.resources.lock().await;
            let state = res.get_or_default::<State<SchedulerState>>();
            if let Some(task) = state.tasks.iter_mut().find(|t| t.id == task_id) {
                // Restore the full pre-fire snapshot, including a pending
                // chain reset the aborted spawn had consumed — losing it
                // would let a later fire resume the old chain under a new
                // prompt.
                task.last_subagent_id = last_subagent_id;
                task.iterations_since_fresh = iterations_since_fresh;
                task.chain_reset_pending = chain_reset_pending;
            }
            return LoopFireOutcome::Foreground;
        }

        let resources = self.resources.clone();
        let guard_task_id = task_id.to_string();
        let spawned_id = subagent_id.clone();
        tokio::spawn(async move {
            let Ok(result) = result_rx.await else {
                return;
            };
            if result.error.is_none() {
                return;
            }
            tracing::warn!(
                task_id = %guard_task_id,
                subagent_id = %spawned_id,
                error = ?result.error,
                "Loop iteration spawn failed; clearing chain anchor"
            );
            let mut res = resources.lock().await;
            let state = res.get_or_default::<State<SchedulerState>>();
            if let Some(task) = state
                .tasks
                .iter_mut()
                .find(|t| t.last_subagent_id.as_deref() == Some(spawned_id.as_str()))
            {
                task.last_subagent_id = None;
                task.iterations_since_fresh = 0;
            }
        });

        LoopFireOutcome::Spawned(subagent_id)
    }

    /// Re-emit `ScheduledTaskCreated` for every task currently in state.
    ///
    /// forwards each one to the client exactly like a fresh `Create`, which
    /// lets the pager rebuild its `scheduled_tasks` view after a session
    /// restore (only `session/update` payloads land in `updates.jsonl`, so the
    /// replay path cannot recover these on its own).
    async fn announce_existing_tasks(&self) {
        let snapshot: Vec<ScheduledTaskCreated> = {
            let res = self.resources.lock().await;
            let Some(state) = res.get::<State<SchedulerState>>() else {
                return;
            };
            if state.tasks.is_empty() {
                return;
            }
            let now = Utc::now();
            // Iteration order matters: the pager sorts the tasks pane by
            // created_at, but every re-announced task gets a synthetic
            // `Instant::now()` on the pager side, so the relative order is
            // determined by the order we send notifications here. Keep
            // state.tasks as a Vec so insertion order survives.
            state
                .tasks
                .iter()
                .filter(|t| t.recurring || t.next_fire_at() > now)
                .map(task_created_payload)
                .collect()
        };

        tracing::info!(
            count = snapshot.len(),
            "Re-announcing scheduled tasks after restore"
        );

        for created in snapshot {
            self.notification_handle
                .send_scheduled_task_created(created);
        }
    }

    async fn handle_command(&mut self, cmd: SchedulerCommand) {
        match cmd {
            SchedulerCommand::Create { task, reply } => {
                let mut res = self.resources.lock().await;
                let state = res.get_or_default::<State<SchedulerState>>();
                if state.tasks.len() >= MAX_SCHEDULED_TASKS {
                    let _ = reply.send(Err(SchedulerError::TaskLimitReached(MAX_SCHEDULED_TASKS)));
                    return;
                }
                self.notification_handle
                    .send_scheduled_task_created(task_created_payload(&task));
                state.tasks.push(task.clone());
                let _ = reply.send(Ok(task));
            }
            SchedulerCommand::Update {
                id,
                prompt,
                interval_secs,
                reply,
            } => {
                let mut res = self.resources.lock().await;
                let state = res.get_or_default::<State<SchedulerState>>();
                let Some(task) = state.tasks.iter_mut().find(|t| t.id == id) else {
                    let _ = reply.send(Err(SchedulerError::TaskNotFound(id)));
                    return;
                };
                if let Some(prompt) = prompt {
                    // A new prompt is a new job: the next fire starts a fresh
                    // transcript. The anchor is kept (not cleared) so the
                    // in-flight guard still sees a running old iteration —
                    // clearing it here would let the next fire double-spawn.
                    if prompt != task.prompt {
                        task.chain_reset_pending = true;
                        task.iterations_since_fresh = 0;
                    }
                    task.prompt = prompt;
                }
                if let Some(interval_secs) = interval_secs {
                    task.interval_secs = interval_secs;
                    if task.next_fire_at() <= Utc::now() {
                        task.last_fired_at = Some(Utc::now());
                    }
                }
                let updated = task.clone();
                drop(res);

                self.notification_handle
                    .send_scheduled_task_created(task_created_payload(&updated));
                let _ = reply.send(Ok(updated));
            }
            SchedulerCommand::Delete { id, reply } => {
                let mut res = self.resources.lock().await;
                let state = res.get_or_default::<State<SchedulerState>>();
                let before = state.tasks.len();
                state.tasks.retain(|t| t.id != id);
                let removed = before != state.tasks.len();
                drop(res);
                let _ = reply.send(removed);
                if removed {
                    self.notification_handle
                        .send_scheduled_task_removed(ScheduledTaskRemoved { task_id: id });
                }
            }
            SchedulerCommand::List { reply } => {
                let res = self.resources.lock().await;
                let tasks = res
                    .get::<State<SchedulerState>>()
                    .map(|s| s.tasks.clone())
                    .unwrap_or_default();
                let _ = reply.send(tasks);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::implementations::grok_build::scheduler::types::{ScheduledTask, SchedulerHandle};
    use crate::notification::ToolNotification;
    use crate::types::resources::Resources;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn make_test_actor() -> (
        SchedulerHandle,
        CancellationToken,
        mpsc::UnboundedReceiver<ToolNotification>,
    ) {
        let mut resources = Resources::new();
        resources.register_state::<SchedulerState>();
        let shared = Arc::new(Mutex::new(resources));

        let (notif_handle, notif_rx) = ToolNotificationHandle::channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        let actor = SchedulerActor {
            resources: shared,
            notification_handle: notif_handle,
            cmd_rx,
            cancel_token: cancel_token.clone(),
        };

        tokio::spawn(actor.run());

        (SchedulerHandle(cmd_tx), cancel_token, notif_rx)
    }

    #[tokio::test]
    async fn create_and_list_task() {
        let (handle, cancel, _notif_rx) = make_test_actor();

        let task = ScheduledTask::new(300, "check deploy".into(), true, false);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task: task.clone(),
                reply: reply_tx,
            })
            .unwrap();
        let result = reply_rx.await.unwrap();
        assert!(result.is_ok());

        let (list_tx, list_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::List { reply: list_tx })
            .unwrap();
        let tasks = list_rx.await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].prompt, "check deploy");

        cancel.cancel();
    }

    #[tokio::test]
    async fn delete_task() {
        let (handle, cancel, _notif_rx) = make_test_actor();

        let task = ScheduledTask::new(300, "test".into(), true, false);
        let task_id = task.id.clone();

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();

        let (del_tx, del_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Delete {
                id: task_id,
                reply: del_tx,
            })
            .unwrap();
        let removed = del_rx.await.unwrap();
        assert!(removed);

        let (list_tx, list_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::List { reply: list_tx })
            .unwrap();
        let tasks = list_rx.await.unwrap();
        assert!(tasks.is_empty());

        cancel.cancel();
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_false() {
        let (handle, cancel, _notif_rx) = make_test_actor();

        let (del_tx, del_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Delete {
                id: "nonexistent".into(),
                reply: del_tx,
            })
            .unwrap();
        let removed = del_rx.await.unwrap();
        assert!(!removed);

        cancel.cancel();
    }

    #[tokio::test]
    async fn max_task_limit_enforced() {
        let (handle, cancel, _notif_rx) = make_test_actor();

        for i in 0..MAX_SCHEDULED_TASKS {
            let task = ScheduledTask::new(300, format!("task {i}"), true, false);
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            handle
                .0
                .send(SchedulerCommand::Create {
                    task,
                    reply: reply_tx,
                })
                .unwrap();
            reply_rx.await.unwrap().unwrap();
        }

        let task = ScheduledTask::new(300, "one too many".into(), true, false);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task,
                reply: reply_tx,
            })
            .unwrap();
        let result = reply_rx.await.unwrap();
        assert!(result.is_err());

        cancel.cancel();
    }

    #[tokio::test]
    async fn cancel_token_shuts_down_actor() {
        let (handle, cancel, _notif_rx) = make_test_actor();
        cancel.cancel();
        // After cancellation, sends should eventually fail (receiver dropped).
        tokio::time::sleep(Duration::from_millis(50)).await;
        let task = ScheduledTask::new(300, "test".into(), true, false);
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        // The send may succeed (buffered) but the reply will never come
        // because the actor has stopped.
        let _ = handle.0.send(SchedulerCommand::Create {
            task,
            reply: reply_tx,
        });
    }

    #[tokio::test]
    async fn recurring_task_fires_repeatedly_without_external_clear() {
        let (handle, cancel, mut notif_rx) = make_test_actor();

        let mut task = ScheduledTask::new(1, "recurring".into(), true, false);
        task.created_at = chrono::Utc::now() - chrono::Duration::seconds(10);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();

        // Drain ScheduledTaskCreated.
        let notif = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
            .await
            .expect("created")
            .expect("channel open");
        assert!(matches!(notif, ToolNotification::ScheduledTaskCreated(_)));

        // First fire.
        let notif = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
            .await
            .expect("first fire")
            .expect("channel open");
        assert!(matches!(notif, ToolNotification::ScheduledTaskFired(_)));

        // The task re-fires purely from `next_fire_at` rescheduling, with no
        // in-flight guard to clear.
        let notif = tokio::time::timeout(Duration::from_secs(3), notif_rx.recv())
            .await
            .expect("second fire")
            .expect("channel open");
        assert!(matches!(notif, ToolNotification::ScheduledTaskFired(_)));

        cancel.cancel();
    }

    #[tokio::test]
    async fn announces_existing_tasks_on_startup() {
        let mut resources = Resources::new();
        resources.register_state::<SchedulerState>();

        // Simulate session restore: scheduler state already contains two
        // recurring tasks before the actor spawns. Both intervals are far
        // enough in the future that neither will fire during the test.
        let state = resources.get_or_default::<State<SchedulerState>>();
        let mut task_a = ScheduledTask::new(300, "task A".into(), true, false);
        task_a.id = "restored-A".to_string();
        state.tasks.push(task_a);
        let mut task_b = ScheduledTask::new(600, "task B".into(), true, false);
        task_b.id = "restored-B".to_string();
        state.tasks.push(task_b);

        let shared = Arc::new(Mutex::new(resources));
        let (notif_handle, mut notif_rx) = ToolNotificationHandle::channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        let actor = SchedulerActor {
            resources: shared,
            notification_handle: notif_handle,
            cmd_rx,
            cancel_token: cancel_token.clone(),
        };

        tokio::spawn(actor.run());

        // The first two events on the channel must be ScheduledTaskCreated
        // for the two restored tasks, in insertion order.
        let n1 = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
            .await
            .expect("first notification")
            .expect("channel open");
        let n2 = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
            .await
            .expect("second notification")
            .expect("channel open");

        let ToolNotification::ScheduledTaskCreated(c1) = n1 else {
            panic!("expected ScheduledTaskCreated, got {n1:?}");
        };
        let ToolNotification::ScheduledTaskCreated(c2) = n2 else {
            panic!("expected ScheduledTaskCreated, got {n2:?}");
        };

        assert_eq!(c1.task_id, "restored-A");
        assert_eq!(c1.prompt, "task A");
        assert_eq!(c1.human_schedule, "every 5 minutes");
        assert!(c1.next_fire_at.is_some());

        assert_eq!(c2.task_id, "restored-B");
        assert_eq!(c2.prompt, "task B");
        assert_eq!(c2.human_schedule, "every 10 minutes");
        assert!(c2.next_fire_at.is_some());

        // No further events should arrive within a short window: tasks are
        // not yet due and no commands are in flight.
        let extra = tokio::time::timeout(Duration::from_millis(150), notif_rx.recv()).await;
        assert!(
            extra.is_err(),
            "no further notifications expected, got {:?}",
            extra.ok()
        );

        cancel_token.cancel();
    }

    #[tokio::test]
    async fn announces_no_tasks_when_state_empty() {
        let (_handle, cancel, mut notif_rx) = make_test_actor();

        // Brand-new session: state.tasks is empty, the re-announce step must
        // be a silent no-op.
        let result = tokio::time::timeout(Duration::from_millis(150), notif_rx.recv()).await;
        assert!(
            result.is_err(),
            "expected no notifications, got {:?}",
            result.ok()
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn legacy_one_shot_fires_via_normal_loop_and_is_removed() {
        let mut resources = Resources::new();
        resources.register_state::<SchedulerState>();

        let state = resources.get_or_default::<State<SchedulerState>>();
        let mut task = ScheduledTask::new(1, "legacy one-shot".into(), false, false);
        task.id = "legacy-1".to_string();
        task.created_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        state.tasks.push(task);

        let shared = Arc::new(Mutex::new(resources));
        let (notif_handle, mut notif_rx) = ToolNotificationHandle::channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        let actor = SchedulerActor {
            resources: shared.clone(),
            notification_handle: notif_handle,
            cmd_rx,
            cancel_token: cancel_token.clone(),
        };
        tokio::spawn(actor.run());

        let mut kinds = Vec::new();
        for _ in 0..2 {
            let notif = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
                .await
                .expect("notification")
                .expect("channel open");
            kinds.push(match notif {
                ToolNotification::ScheduledTaskCreated(_) => "created",
                ToolNotification::ScheduledTaskFired(_) => "fired",
                ToolNotification::ScheduledTaskRemoved(_) => "removed",
                _ => "other",
            });
        }
        assert_eq!(kinds, vec!["fired", "removed"]);

        let res = shared.lock().await;
        let remaining = res
            .get::<State<SchedulerState>>()
            .map(|s| s.tasks.len())
            .unwrap_or(0);
        assert_eq!(remaining, 0, "legacy one-shot removed after firing");
        cancel_token.cancel();
    }

    #[tokio::test]
    async fn update_patches_in_place_preserving_identity_and_phase() {
        let (handle, cancel, _notif_rx) = make_test_actor();

        let task = ScheduledTask::new(300, "check deploy".into(), true, false);
        let task_id = task.id.clone();
        let created_at = task.created_at;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();

        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: task_id.clone(),
                prompt: None,
                interval_secs: Some(600),
                reply: up_tx,
            })
            .unwrap();
        let updated = up_rx.await.unwrap().unwrap();
        assert_eq!(updated.id, task_id, "identity preserved");
        assert_eq!(updated.interval_secs, 600);
        assert_eq!(updated.prompt, "check deploy");
        assert_eq!(updated.created_at, created_at, "phase anchor preserved");
        assert!(updated.last_fired_at.is_none());

        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: task_id.clone(),
                prompt: Some("check rollback".into()),
                interval_secs: None,
                reply: up_tx,
            })
            .unwrap();
        let updated = up_rx.await.unwrap().unwrap();
        assert_eq!(updated.interval_secs, 600);
        assert_eq!(updated.prompt, "check rollback");

        let (list_tx, list_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::List { reply: list_tx })
            .unwrap();
        assert_eq!(list_rx.await.unwrap().len(), 1);

        cancel.cancel();
    }

    #[tokio::test]
    async fn update_shrinking_interval_never_fires_immediately() {
        let (handle, cancel, mut notif_rx) = make_test_actor();

        let mut task = ScheduledTask::new(3600, "check deploy".into(), true, false);
        task.created_at = chrono::Utc::now() - chrono::Duration::seconds(600);
        let task_id = task.id.clone();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();

        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: task_id,
                prompt: None,
                interval_secs: Some(60),
                reply: up_tx,
            })
            .unwrap();
        let updated = up_rx.await.unwrap().unwrap();
        assert!(
            updated.next_fire_at() > chrono::Utc::now(),
            "an update must never leave the task immediately due"
        );

        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), notif_rx.recv()).await {
                Ok(Some(ToolNotification::ScheduledTaskFired(_))) => {
                    panic!("update must not trigger a fire");
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn update_unknown_id_errors_and_never_creates() {
        let (handle, cancel, _notif_rx) = make_test_actor();

        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: "nonexistent".into(),
                prompt: Some("new prompt".into()),
                interval_secs: Some(300),
                reply: up_tx,
            })
            .unwrap();
        let result = up_rx.await.unwrap();
        assert!(matches!(result, Err(SchedulerError::TaskNotFound(_))));

        let (list_tx, list_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::List { reply: list_tx })
            .unwrap();
        assert!(
            list_rx.await.unwrap().is_empty(),
            "a failed update must not fall back to creating a task"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn update_emits_created_notification_as_upsert() {
        let (handle, cancel, mut notif_rx) = make_test_actor();

        let task = ScheduledTask::new(300, "check deploy".into(), true, false);
        let task_id = task.id.clone();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv()).await;

        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: task_id.clone(),
                prompt: Some("check rollback".into()),
                interval_secs: None,
                reply: up_tx,
            })
            .unwrap();
        up_rx.await.unwrap().unwrap();

        let notif = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
            .await
            .expect("upsert notification")
            .expect("channel open");
        let ToolNotification::ScheduledTaskCreated(created) = notif else {
            panic!("expected ScheduledTaskCreated upsert, got {notif:?}");
        };
        assert_eq!(created.task_id, task_id, "same chip identity");
        assert_eq!(created.prompt, "check rollback");

        cancel.cancel();
    }

    #[tokio::test]
    async fn cancel_sends_removed_for_remaining_tasks() {
        let mut resources = Resources::new();
        resources.register_state::<SchedulerState>();

        let state = resources.get_or_default::<State<SchedulerState>>();
        let mut task_a = ScheduledTask::new(300, "task A".into(), true, false);
        task_a.id = "cancel-A".to_string();
        state.tasks.push(task_a);
        let mut task_b = ScheduledTask::new(600, "task B".into(), true, false);
        task_b.id = "cancel-B".to_string();
        state.tasks.push(task_b);

        let shared = Arc::new(Mutex::new(resources));
        let (notif_handle, mut notif_rx) = ToolNotificationHandle::channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        let actor = SchedulerActor {
            resources: shared,
            notification_handle: notif_handle,
            cmd_rx,
            cancel_token: cancel_token.clone(),
        };

        let handle = tokio::spawn(actor.run());

        // Drain the ScheduledTaskCreated announcements.
        let _ = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv()).await;
        let _ = tokio::time::timeout(Duration::from_secs(1), notif_rx.recv()).await;

        cancel_token.cancel();
        handle.await.expect("actor should complete");

        // Collect all ScheduledTaskRemoved notifications.
        let mut removed_ids = Vec::new();
        while let Ok(notif) = notif_rx.try_recv() {
            if let ToolNotification::ScheduledTaskRemoved(r) = notif {
                removed_ids.push(r.task_id);
            }
        }
        removed_ids.sort();
        assert_eq!(removed_ids, vec!["cancel-A", "cancel-B"]);
    }

    fn make_test_actor_with_subagents() -> (
        SchedulerHandle,
        CancellationToken,
        mpsc::UnboundedReceiver<ToolNotification>,
        mpsc::UnboundedReceiver<SubagentEvent>,
    ) {
        let mut resources = Resources::new();
        resources.register_state::<SchedulerState>();
        let (subagent_tx, subagent_rx) = mpsc::unbounded_channel();
        resources.insert(SubagentEventSender(subagent_tx));
        resources.insert(
            crate::implementations::grok_build::task::types::SessionIdResource(
                "parent-session".to_string(),
            ),
        );
        let shared = Arc::new(Mutex::new(resources));

        let (notif_handle, notif_rx) = ToolNotificationHandle::channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        let actor = SchedulerActor {
            resources: shared,
            notification_handle: notif_handle,
            cmd_rx,
            cancel_token: cancel_token.clone(),
        };
        tokio::spawn(actor.run());

        (SchedulerHandle(cmd_tx), cancel_token, notif_rx, subagent_rx)
    }

    async fn create_due_task(handle: &SchedulerHandle, prompt: &str, foreground: bool) -> String {
        let mut task = ScheduledTask::new(1, prompt.into(), true, false);
        task.foreground = foreground;
        task.created_at = chrono::Utc::now() - chrono::Duration::seconds(10);
        let task_id = task.id.clone();
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();
        task_id
    }

    async fn next_subagent_event(rx: &mut mpsc::UnboundedReceiver<SubagentEvent>) -> SubagentEvent {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("subagent event within timeout")
            .expect("subagent channel open")
    }

    #[tokio::test]
    async fn background_fire_spawns_loop_subagent() {
        let (handle, cancel, mut notif_rx, mut subagent_rx) = make_test_actor_with_subagents();
        let task_id = create_due_task(&handle, "check deploy status", false).await;

        let event = next_subagent_event(&mut subagent_rx).await;
        let SubagentEvent::Spawn(request) = event else {
            panic!("expected Spawn, got a different event");
        };
        assert!(request.run_in_background);
        assert!(request.surface_completion);
        assert!(request.parent_prompt_id.is_none());
        assert!(request.resume_from.is_none(), "first iteration is fresh");
        assert_eq!(request.subagent_type, "general-purpose");
        assert_eq!(request.parent_session_id, "parent-session");
        assert!(request.prompt.contains("check deploy status"));
        assert!(request.prompt.contains(&task_id));
        assert!(request.prompt.contains("short status"));
        assert_eq!(
            request.runtime_overrides.completion_output_cap,
            Some(LOOP_COMPLETION_OUTPUT_CAP)
        );
        assert_eq!(
            request.runtime_overrides.spawn_depth,
            Some(0),
            "iterations spawn at root depth so they can spawn subagents"
        );
        assert!(request.description.starts_with("loop: "));

        let mut fired_subagent_id = None;
        for _ in 0..4 {
            let notif = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
                .await
                .expect("notification")
                .expect("channel open");
            if let ToolNotification::ScheduledTaskFired(f) = notif {
                fired_subagent_id = f.subagent_id;
                break;
            }
        }
        assert_eq!(fired_subagent_id.as_deref(), Some(request.id.as_str()));

        let (list_tx, list_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::List { reply: list_tx })
            .unwrap();
        let tasks = list_rx.await.unwrap();
        assert_eq!(
            tasks[0].last_subagent_id.as_deref(),
            Some(request.id.as_str())
        );
        assert_eq!(tasks[0].iterations_since_fresh, 1);

        cancel.cancel();
    }

    #[tokio::test]
    async fn in_flight_iteration_skips_fire_then_resumes_chain() {
        let (handle, cancel, mut notif_rx, mut subagent_rx) = make_test_actor_with_subagents();
        create_due_task(&handle, "watch ci", false).await;

        let SubagentEvent::Spawn(first) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected first Spawn");
        };
        let first_id = first.id.clone();
        loop {
            let notif = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
                .await
                .expect("first-fire notification")
                .expect("channel open");
            if matches!(notif, ToolNotification::ScheduledTaskFired(_)) {
                break;
            }
        }

        let SubagentEvent::Query(query) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected Query before second spawn");
        };
        assert_eq!(query.subagent_id, first_id);
        assert!(!query.block);
        let _ = query.respond_to.send(Some(
            crate::implementations::grok_build::task::types::SubagentSnapshot {
                subagent_id: first_id.clone(),
                description: "loop: watch ci".into(),
                subagent_type: "general-purpose".into(),
                status: SubagentSnapshotStatus::Running {
                    turn_count: 1,
                    tool_call_count: 2,
                    tokens_used: 0,
                    context_window_tokens: 0,
                    context_usage_pct: 0,
                    tools_used: vec![],
                    error_count: 0,
                },
                started_at_epoch_ms: 0,
                duration_ms: 100,
                persona: None,
            },
        ));

        let SubagentEvent::Query(query2) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected the next tick's Query, not a Spawn");
        };
        match notif_rx.try_recv() {
            Ok(ToolNotification::ScheduledTaskCreated(c)) => {
                assert!(
                    c.next_fire_at.is_some(),
                    "skip re-announce refreshes the countdown"
                );
            }
            Ok(other) => panic!("skipped fire must only re-announce, got {other:?}"),
            Err(_) => panic!("skipped fire should re-announce the task"),
        }
        assert!(
            notif_rx.try_recv().is_err(),
            "skipped fire must not emit a fired notification"
        );
        assert_eq!(query2.subagent_id, first_id);
        let _ = query2.respond_to.send(Some(
            crate::implementations::grok_build::task::types::SubagentSnapshot {
                subagent_id: first_id.clone(),
                description: "loop: watch ci".into(),
                subagent_type: "general-purpose".into(),
                status: SubagentSnapshotStatus::Completed {
                    output: "ci green".into(),
                    tool_calls: 3,
                    turns: 1,
                    worktree_path: None,
                },
                started_at_epoch_ms: 0,
                duration_ms: 100,
                persona: None,
            },
        ));

        let SubagentEvent::Spawn(second) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected second Spawn after completion");
        };
        assert_eq!(second.resume_from.as_deref(), Some(first_id.as_str()));
        assert_ne!(second.id, first_id);

        cancel.cancel();
    }

    #[tokio::test]
    async fn prompt_update_resets_chain_interval_update_keeps_it() {
        let (handle, cancel, _notif_rx, mut subagent_rx) = make_test_actor_with_subagents();
        let task_id = create_due_task(&handle, "watch ci", false).await;

        let SubagentEvent::Spawn(first) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected first Spawn");
        };

        // Interval-only patch: the chain anchor survives.
        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: task_id.clone(),
                prompt: None,
                interval_secs: Some(3600),
                reply: up_tx,
            })
            .unwrap();
        let updated = up_rx.await.unwrap().unwrap();
        assert_eq!(updated.last_subagent_id.as_deref(), Some(first.id.as_str()));
        assert_eq!(updated.iterations_since_fresh, 1);

        // Prompt patch: new job, fresh chain pending — but the anchor is kept
        // so the in-flight guard still covers a running old iteration.
        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: task_id,
                prompt: Some("watch deploys instead".into()),
                interval_secs: None,
                reply: up_tx,
            })
            .unwrap();
        let updated = up_rx.await.unwrap().unwrap();
        assert_eq!(updated.last_subagent_id.as_deref(), Some(first.id.as_str()));
        assert!(updated.chain_reset_pending);
        assert_eq!(updated.iterations_since_fresh, 0);

        cancel.cancel();
    }

    #[tokio::test]
    async fn prompt_update_keeps_in_flight_guard_then_spawns_fresh() {
        let (handle, cancel, _notif_rx, mut subagent_rx) = make_test_actor_with_subagents();
        let task_id = create_due_task(&handle, "watch ci", false).await;

        let SubagentEvent::Spawn(first) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected first Spawn");
        };
        let first_id = first.id.clone();

        // Patch the prompt while iteration 1 is still running.
        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: task_id,
                prompt: Some("watch deploys instead".into()),
                interval_secs: None,
                reply: up_tx,
            })
            .unwrap();
        up_rx.await.unwrap().unwrap();

        // Next tick still queries the old iteration; Running must skip.
        let SubagentEvent::Query(q1) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected Query after prompt update");
        };
        assert_eq!(q1.subagent_id, first_id, "guard queries the kept anchor");
        let _ = q1.respond_to.send(Some(
            crate::implementations::grok_build::task::types::SubagentSnapshot {
                subagent_id: first_id.clone(),
                description: "loop: watch ci".into(),
                subagent_type: "general-purpose".into(),
                status: SubagentSnapshotStatus::Running {
                    turn_count: 1,
                    tool_call_count: 1,
                    tokens_used: 0,
                    context_window_tokens: 0,
                    context_usage_pct: 0,
                    tools_used: vec![],
                    error_count: 0,
                },
                started_at_epoch_ms: 0,
                duration_ms: 100,
                persona: None,
            },
        ));

        // Old iteration done: the following tick spawns the NEW job fresh —
        // no resume_from, and the framing carries no old-task output.
        let SubagentEvent::Query(q2) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected second Query, not a Spawn");
        };
        let _ = q2.respond_to.send(Some(
            crate::implementations::grok_build::task::types::SubagentSnapshot {
                subagent_id: first_id.clone(),
                description: "loop: watch ci".into(),
                subagent_type: "general-purpose".into(),
                status: SubagentSnapshotStatus::Completed {
                    output: "old task output".into(),
                    tool_calls: 1,
                    turns: 1,
                    worktree_path: None,
                },
                started_at_epoch_ms: 0,
                duration_ms: 100,
                persona: None,
            },
        ));
        let SubagentEvent::Spawn(second) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected fresh Spawn after old iteration completed");
        };
        assert!(second.resume_from.is_none(), "prompt change spawns fresh");
        assert!(second.prompt.contains("watch deploys instead"));
        assert!(
            !second.prompt.contains("old task output"),
            "old task's output must not leak into the new job"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn failed_spawn_resets_chain_state() {
        let (handle, cancel, _notif_rx, mut subagent_rx) = make_test_actor_with_subagents();
        // One-hour interval, backdated to fire exactly once: the assertion
        // window cannot be raced by a second fire re-pointing the anchor.
        let mut task = ScheduledTask::new(3600, "watch ci".into(), true, false);
        task.created_at = chrono::Utc::now() - chrono::Duration::seconds(3610);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Create {
                task,
                reply: reply_tx,
            })
            .unwrap();
        reply_rx.await.unwrap().unwrap();

        let SubagentEvent::Spawn(request) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected Spawn");
        };
        let spawned_id = request.id.clone();
        let _ = request.result_tx.send(
            crate::implementations::grok_build::task::types::SubagentResult {
                success: false,
                error: Some("worktree creation failed".into()),
                subagent_id: spawned_id.clone(),
                ..Default::default()
            },
        );

        // The watcher clears the anchor AND the fresh-chain counter so the
        // next fire starts clean.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let (list_tx, list_rx) = tokio::sync::oneshot::channel();
            handle
                .0
                .send(SchedulerCommand::List { reply: list_tx })
                .unwrap();
            let tasks = list_rx.await.unwrap();
            if tasks[0].last_subagent_id.is_none() {
                assert_eq!(tasks[0].iterations_since_fresh, 0);
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "watcher did not reset chain state"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn spawn_send_failure_restores_pending_chain_reset() {
        let (handle, cancel, _notif_rx, mut subagent_rx) = make_test_actor_with_subagents();
        let task_id = create_due_task(&handle, "watch ci", false).await;

        let SubagentEvent::Spawn(first) = next_subagent_event(&mut subagent_rx).await else {
            panic!("expected first Spawn");
        };
        let first_id = first.id.clone();

        let (up_tx, up_rx) = tokio::sync::oneshot::channel();
        handle
            .0
            .send(SchedulerCommand::Update {
                id: task_id.clone(),
                prompt: Some("watch deploys instead".into()),
                interval_secs: None,
                reply: up_tx,
            })
            .unwrap();
        up_rx.await.unwrap().unwrap();

        // Coordinator gone: the next fire's guard query and spawn send both
        // fail, taking the rollback path.
        drop(subagent_rx);

        // Wait for the failed fire's rollback to land in state.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let (list_tx, list_rx) = tokio::sync::oneshot::channel();
            handle
                .0
                .send(SchedulerCommand::List { reply: list_tx })
                .unwrap();
            let task = list_rx.await.unwrap().remove(0);
            // The rollback restores the pre-fire snapshot exactly: old
            // anchor, pre-fire counter, and the still-pending reset.
            if task.last_subagent_id.as_deref() == Some(first_id.as_str())
                && task.chain_reset_pending
                && task.iterations_since_fresh == 0
                && task.last_fired_at.is_some()
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "rollback did not restore the full snapshot: {task:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        cancel.cancel();
    }

    #[tokio::test]
    async fn background_loops_disabled_forces_legacy_path() {
        let mut resources = Resources::new();
        resources.register_state::<SchedulerState>();
        let (subagent_tx, mut subagent_rx) = mpsc::unbounded_channel();
        resources.insert(SubagentEventSender(subagent_tx));
        resources.insert(
            crate::implementations::grok_build::task::types::SessionIdResource(
                "parent-session".to_string(),
            ),
        );
        resources.insert(crate::types::resources::SchedulerBackgroundLoops(false));
        let shared = Arc::new(Mutex::new(resources));

        let (notif_handle, mut notif_rx) = ToolNotificationHandle::channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();
        tokio::spawn(
            SchedulerActor {
                resources: shared,
                notification_handle: notif_handle,
                cmd_rx,
                cancel_token: cancel_token.clone(),
            }
            .run(),
        );
        let handle = SchedulerHandle(cmd_tx);
        create_due_task(&handle, "watch ci", false).await;

        let mut fired = None;
        for _ in 0..4 {
            let notif = tokio::time::timeout(Duration::from_secs(3), notif_rx.recv())
                .await
                .expect("notification")
                .expect("channel open");
            if let ToolNotification::ScheduledTaskFired(f) = notif {
                fired = Some(f);
                break;
            }
        }
        assert!(
            fired.expect("fired notification").subagent_id.is_none(),
            "disabled config must take the legacy inject path"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), subagent_rx.recv())
                .await
                .is_err(),
            "disabled config must not touch the subagent coordinator"
        );

        cancel_token.cancel();
    }

    #[tokio::test]
    async fn foreground_task_fires_legacy_inject_path() {
        let (handle, cancel, mut notif_rx, mut subagent_rx) = make_test_actor_with_subagents();
        create_due_task(&handle, "needs main context", true).await;

        let mut fired = None;
        for _ in 0..4 {
            let notif = tokio::time::timeout(Duration::from_secs(2), notif_rx.recv())
                .await
                .expect("notification")
                .expect("channel open");
            if let ToolNotification::ScheduledTaskFired(f) = notif {
                fired = Some(f);
                break;
            }
        }
        let fired = fired.expect("fired notification");
        assert!(fired.subagent_id.is_none());

        assert!(
            tokio::time::timeout(Duration::from_millis(200), subagent_rx.recv())
                .await
                .is_err(),
            "foreground fire must not touch the subagent coordinator"
        );

        cancel.cancel();
    }

    #[tokio::test]
    async fn cancel_with_no_tasks_sends_no_removed() {
        let mut resources = Resources::new();
        resources.register_state::<SchedulerState>();

        let shared = Arc::new(Mutex::new(resources));
        let (notif_handle, mut notif_rx) = ToolNotificationHandle::channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        let actor = SchedulerActor {
            resources: shared,
            notification_handle: notif_handle,
            cmd_rx,
            cancel_token: cancel_token.clone(),
        };

        let handle = tokio::spawn(actor.run());
        cancel_token.cancel();
        handle.await.expect("actor should complete");

        assert!(
            notif_rx.try_recv().is_err(),
            "no notifications expected when state is empty"
        );
    }
}
