use std::time::Duration;

use chrono::Utc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::notification::types::ToolNotificationHandle;
use crate::notification::{ScheduledTaskCreated, ScheduledTaskFired, ScheduledTaskRemoved};
use crate::types::resources::{SharedResources, State};

use super::interval::interval_to_human;
use super::types::{ScheduledTask, SchedulerCommand, SchedulerError, SchedulerState};

const MAX_SCHEDULED_TASKS: usize = 50;

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
        self.handle_missed_tasks().await;
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

        // Advancing `last_fired_at` pushes `next_fire_at` forward by one
        // interval, so the task is not re-selected until it is due again.
        // Overlapping prompts are deduped downstream by stable queue-item-id.
        task.last_fired_at = Some(now);
        let next_fire_at = Some(task.next_fire_at().to_rfc3339());

        let should_remove = if !task.recurring {
            true
        } else {
            task.is_expired(now)
        };

        if should_remove {
            state.tasks.remove(idx);
        }

        // Drop the lock before sending the notification to avoid holding it
        // across potentially blocking operations.
        drop(res);

        tracing::info!(
            task_id = %task_id,
            schedule = %human_schedule,
            "Firing scheduled task"
        );

        let removed_task_id = should_remove.then(|| task_id.clone());

        self.notification_handle
            .send_scheduled_task_fired(ScheduledTaskFired {
                task_id,
                prompt,
                human_schedule,
                next_fire_at,
            });

        if let Some(task_id) = removed_task_id {
            self.notification_handle
                .send_scheduled_task_removed(ScheduledTaskRemoved { task_id });
        }
    }

    async fn handle_missed_tasks(&mut self) {
        let now = Utc::now();
        let mut res = self.resources.lock().await;
        let state = res.get_or_default::<State<SchedulerState>>();

        let missed: Vec<(String, String, u64)> = state
            .tasks
            .iter()
            .filter(|t| t.is_missed(now))
            .map(|t| (t.id.clone(), t.prompt.clone(), t.interval_secs))
            .collect();

        if missed.is_empty() {
            return;
        }

        drop(res);

        let mut fired_ids = Vec::new();
        for (task_id, prompt, interval_secs) in missed {
            tracing::info!(task_id = %task_id, "Firing missed one-shot task");
            self.notification_handle
                .send_scheduled_task_fired(ScheduledTaskFired {
                    task_id: task_id.clone(),
                    prompt,
                    human_schedule: interval_to_human(interval_secs),
                    next_fire_at: None,
                });
            fired_ids.push(task_id);
        }

        let mut res = self.resources.lock().await;
        let state = res.get_or_default::<State<SchedulerState>>();
        state.tasks.retain(|t| !fired_ids.contains(&t.id));
        drop(res);

        for task_id in fired_ids {
            self.notification_handle
                .send_scheduled_task_removed(ScheduledTaskRemoved { task_id });
        }
    }

    /// Re-emit `ScheduledTaskCreated` for every task currently in state.
    ///
    /// Runs after `handle_missed_tasks()` so missed one-shots that were just
    /// pruned are not announced as ghost entries. The notification bridge
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
            // Iteration order matters: the pager sorts the tasks pane by
            // created_at, but every re-announced task gets a synthetic
            // `Instant::now()` on the pager side, so the relative order is
            // determined by the order we send notifications here. Keep
            // state.tasks as a Vec so insertion order survives.
            state.tasks.iter().map(task_created_payload).collect()
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
    async fn missed_one_shots_all_fire_and_are_removed() {
        let mut resources = Resources::new();
        resources.register_state::<SchedulerState>();

        let state = resources.get_or_default::<State<SchedulerState>>();
        let past = chrono::Utc::now() - chrono::Duration::seconds(60);
        for i in 0..3 {
            let mut task = ScheduledTask::new(1, format!("missed-{i}"), false, false);
            task.id = format!("missed-{i}");
            task.created_at = past;
            state.tasks.push(task);
        }

        let shared = Arc::new(Mutex::new(resources));
        let (notif_handle, mut notif_rx) = ToolNotificationHandle::channel();
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let cancel_token = CancellationToken::new();

        let mut actor = SchedulerActor {
            resources: shared.clone(),
            notification_handle: notif_handle,
            cmd_rx,
            cancel_token: cancel_token.clone(),
        };

        actor.handle_missed_tasks().await;

        // All three missed one-shots fire (fires first, then removes).
        for _ in 0..3 {
            let notif = notif_rx.try_recv().expect("missed task should fire");
            assert!(matches!(notif, ToolNotification::ScheduledTaskFired(_)));
        }
        for _ in 0..3 {
            let notif = notif_rx.try_recv().expect("missed task should be removed");
            assert!(matches!(notif, ToolNotification::ScheduledTaskRemoved(_)));
        }
        assert!(notif_rx.try_recv().is_err());

        // All fired missed one-shots are pruned from state.
        let res = shared.lock().await;
        let remaining = res
            .get::<State<SchedulerState>>()
            .map(|s| s.tasks.len())
            .unwrap_or(0);
        assert_eq!(remaining, 0, "all fired missed one-shots should be removed");
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
