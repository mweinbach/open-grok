use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use xai_grok_tools::implementations::grok_build::scheduler::types::{
    ScheduledTask, SchedulerCommand, SchedulerHandle,
};

use crate::session::{WorkspaceSession, WorkspaceShared};

const POLL_INTERVAL: Duration = Duration::from_secs(30);

const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq)]
enum SchedulerAnswer {
    Alive(Option<u64>),
    Dead,
    NoReply,
}

#[derive(Debug, PartialEq)]
enum PollUpdate {
    Record(Option<u64>),
    KeepLast,
}

pub(crate) fn spawn_scheduler_liveness_poll(shared: &Arc<WorkspaceShared>) {
    if shared.status_config.scheduled_task_keep_awake.is_zero() {
        return;
    }
    if shared.scheduler_poll_started.swap(true, Ordering::SeqCst) {
        return;
    }

    let weak = Arc::downgrade(shared);
    tokio::spawn(async move {
        loop {
            let Some(shared) = weak.upgrade() else { return };
            if let PollUpdate::Record(next_run) = poll_schedulers_once(&shared).await {
                shared.activity_tracker.record_scheduler_poll(next_run);
            }
            drop(shared);

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

async fn poll_schedulers_once(shared: &Arc<WorkspaceShared>) -> PollUpdate {
    let sessions: Vec<Arc<WorkspaceSession>> = shared.sessions.read().values().cloned().collect();

    let mut handles = Vec::with_capacity(sessions.len());
    for session in sessions {
        let toolset = session.toolset();
        let handle = {
            let res = toolset.resources.lock().await;
            res.get::<SchedulerHandle>().cloned()
        };
        if let Some(handle) = handle {
            handles.push(handle);
        }
    }

    let answers = futures::future::join_all(handles.iter().map(poll_scheduler_handle)).await;
    poll_update(answers)
}

fn poll_update(answers: impl IntoIterator<Item = SchedulerAnswer>) -> PollUpdate {
    let mut earliest: Option<u64> = None;
    let mut unanswered = false;
    for answer in answers {
        match answer {
            SchedulerAnswer::Alive(Some(run)) => {
                earliest = Some(earliest.map_or(run, |e| e.min(run)));
            }
            SchedulerAnswer::Alive(None) | SchedulerAnswer::Dead => {}
            SchedulerAnswer::NoReply => unanswered = true,
        }
    }

    if earliest.is_none() && unanswered {
        PollUpdate::KeepLast
    } else {
        PollUpdate::Record(earliest)
    }
}

async fn poll_scheduler_handle(handle: &SchedulerHandle) -> SchedulerAnswer {
    let (reply, rx) = tokio::sync::oneshot::channel();
    if handle.0.send(SchedulerCommand::List { reply }).is_err() {
        return SchedulerAnswer::Dead;
    }

    match tokio::time::timeout(REPLY_TIMEOUT, rx).await {
        Err(_elapsed) => SchedulerAnswer::NoReply,
        Ok(Err(_closed)) => SchedulerAnswer::Dead,
        Ok(Ok(snapshot)) => {
            SchedulerAnswer::Alive(earliest_pending_fire(&snapshot.tasks, chrono::Utc::now()))
        }
    }
}

fn earliest_pending_fire(
    tasks: &[ScheduledTask],
    now: chrono::DateTime<chrono::Utc>,
) -> Option<u64> {
    tasks
        .iter()
        .filter_map(|task| task.pending_fire_at(now))
        .map(|fire| fire.timestamp_millis().max(0) as u64)
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earliest_pending_fire_skips_expired_and_fired_one_shots() {
        let now = chrono::Utc::now();

        let hourly = ScheduledTask::new(3600, "p".into(), true, false);
        let minutely = ScheduledTask::new(60, "p".into(), true, false);
        let mut expired = ScheduledTask::new(60, "p".into(), true, false);
        expired.expires_at = Some(now - chrono::Duration::seconds(1));
        let mut fired_one_shot = ScheduledTask::new(60, "p".into(), false, false);
        fired_one_shot.last_fired_at = Some(now - chrono::Duration::minutes(9));
        let due_unfired_one_shot = ScheduledTask::new(0, "p".into(), false, false);

        assert_eq!(
            earliest_pending_fire(&[expired.clone(), fired_one_shot.clone()], now),
            None,
            "expired tasks and one-shots that already ran can never run again"
        );
        assert!(
            earliest_pending_fire(&[due_unfired_one_shot], now).is_some(),
            "a due one-shot that never ran still has a run coming"
        );

        let earliest = earliest_pending_fire(
            &[hourly.clone(), minutely.clone(), expired, fired_one_shot],
            now,
        )
        .expect("live recurring tasks have a run coming");
        assert_eq!(
            earliest,
            minutely.next_fire_at().timestamp_millis() as u64,
            "the earliest coming run wins"
        );
        assert!(earliest < hourly.next_fire_at().timestamp_millis() as u64);
    }

    #[tokio::test]
    async fn dead_scheduler_actor_answers_dead() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);

        assert_eq!(
            poll_scheduler_handle(&SchedulerHandle(tx)).await,
            SchedulerAnswer::Dead,
            "a closed command channel is a dead scheduler"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_scheduler_answers_no_reply_not_dead() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let holder = tokio::spawn(async move {
            let Some(SchedulerCommand::List { reply }) = rx.recv().await else {
                return;
            };
            tokio::time::sleep(Duration::from_secs(3600)).await;
            drop(reply);
        });

        assert_eq!(
            poll_scheduler_handle(&SchedulerHandle(tx)).await,
            SchedulerAnswer::NoReply,
            "no reply within the wait is not proof of death"
        );

        holder.abort();
    }

    #[test]
    fn poll_update_keeps_last_result_on_silence_and_records_otherwise() {
        assert_eq!(
            poll_update([SchedulerAnswer::NoReply]),
            PollUpdate::KeepLast,
            "silence with no found run must not clear the hold"
        );
        assert_eq!(
            poll_update([SchedulerAnswer::NoReply, SchedulerAnswer::Alive(Some(70))]),
            PollUpdate::Record(Some(70)),
            "a found run records even when another scheduler is silent"
        );
        assert_eq!(
            poll_update([
                SchedulerAnswer::Alive(Some(90)),
                SchedulerAnswer::Alive(Some(70))
            ]),
            PollUpdate::Record(Some(70)),
            "the earliest run across sessions wins"
        );
        assert_eq!(
            poll_update([SchedulerAnswer::Dead, SchedulerAnswer::Alive(None)]),
            PollUpdate::Record(None),
            "dead schedulers and empty task lists clear the hold"
        );
        assert_eq!(poll_update([]), PollUpdate::Record(None));
    }

    #[tokio::test]
    async fn poll_finds_a_live_scheduler_through_a_real_session() {
        let handle = crate::handle::WorkspaceHandle::for_test();
        handle
            .create_session_with_config(
                "main",
                None,
                None,
                crate::capability::CapabilityMode::All,
                None,
                false,
            )
            .expect("create session");

        assert_eq!(
            poll_schedulers_once(&handle.shared).await,
            PollUpdate::Record(None),
            "a live scheduler with nothing to run must clear the hold"
        );

        let scheduler = {
            let session = handle.session("main").expect("session exists");
            let toolset = session.toolset();
            let res = toolset.resources.lock().await;
            res.get::<SchedulerHandle>()
                .cloned()
                .expect("scheduler handle in resources")
        };
        let (reply, rx) = tokio::sync::oneshot::channel();
        scheduler
            .0
            .send(SchedulerCommand::Create {
                task: ScheduledTask::new(3600, "p".into(), true, false),
                reply,
            })
            .expect("scheduler alive");
        rx.await.expect("actor replies").expect("task created");

        match poll_schedulers_once(&handle.shared).await {
            PollUpdate::Record(Some(run)) => {
                assert!(
                    run > chrono::Utc::now().timestamp_millis() as u64,
                    "the recorded run must be in the future"
                );
            }
            other => panic!("a live loop must record its coming run, got {other:?}"),
        }
    }
}
