use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

#[derive(thiserror::Error, Debug)]
pub enum SchedulerError {
    #[error("invalid interval: {0}")]
    InvalidInterval(String),

    #[error("maximum of {0} scheduled tasks reached")]
    TaskLimitReached(usize),

    #[error("no scheduled task with id {0}; call scheduler_list to see active task ids")]
    TaskNotFound(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTask {
    pub id: String,
    pub interval_secs: u64,
    pub prompt: String,
    #[serde(default = "default_recurring")]
    pub recurring: bool,
    pub durable: bool,
    #[serde(default)]
    pub foreground: bool,
    pub created_at: DateTime<Utc>,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_subagent_id: Option<String>,
    #[serde(default)]
    pub iterations_since_fresh: u32,
    /// Set when the prompt is patched: the next fire starts a fresh
    /// transcript instead of resuming the old task's. The anchor itself is
    /// kept until then so the in-flight guard can still see a running
    /// iteration.
    #[serde(default)]
    pub chain_reset_pending: bool,
}

pub const LOOP_FRESH_CHAIN_EVERY: u32 = 10;

pub const LOOP_COMPLETION_OUTPUT_CAP: usize = 4_000;

fn default_recurring() -> bool {
    true
}

impl ScheduledTask {
    pub fn new(interval_secs: u64, prompt: String, recurring: bool, durable: bool) -> Self {
        Self::with_fire_immediately(interval_secs, prompt, recurring, durable, false)
    }

    pub fn with_fire_immediately(
        interval_secs: u64,
        prompt: String,
        recurring: bool,
        durable: bool,
        fire_immediately: bool,
    ) -> Self {
        let now = Utc::now();
        // When fire_immediately is true, anchor created_at in the past so that
        // next_fire_at() = created_at + interval = now, firing on the first tick.
        let created_at = if fire_immediately {
            now - chrono::Duration::seconds(interval_secs as i64)
        } else {
            now
        };
        Self {
            id: uuid::Uuid::now_v7().to_string().replace('-', "")[..12].to_string(),
            interval_secs,
            prompt,
            recurring,
            durable,
            foreground: false,
            created_at,
            last_fired_at: None,
            expires_at: if recurring {
                Some(now + chrono::Duration::days(7))
            } else {
                None
            },
            last_subagent_id: None,
            iterations_since_fresh: 0,
            chain_reset_pending: false,
        }
    }

    /// Next fire time, computed from `last_fired_at` (or `created_at` if never fired).
    pub fn next_fire_at(&self) -> DateTime<Utc> {
        let anchor = self.last_fired_at.unwrap_or(self.created_at);
        anchor + chrono::Duration::seconds(self.interval_secs as i64)
    }

    /// Whether this task has expired (recurring tasks only).
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|exp| now >= exp)
    }
}

/// Persisted state for the scheduler, stored via Resources + ResourcesPersistence.
/// Only durable tasks are serialized; non-durable tasks are filtered out before save.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SchedulerState {
    pub tasks: Vec<ScheduledTask>,
}

crate::register_resource!("grok_build", "Scheduler", SchedulerState);

/// Handle for tools to communicate with the SchedulerActor.
/// Ephemeral -- not serialized, not persisted. Inserted via `resources.insert()`.
#[derive(Clone)]
pub struct SchedulerHandle(pub mpsc::UnboundedSender<SchedulerCommand>);

pub enum SchedulerCommand {
    Create {
        task: ScheduledTask,
        reply: oneshot::Sender<Result<ScheduledTask, SchedulerError>>,
    },
    Update {
        id: String,
        prompt: Option<String>,
        interval_secs: Option<u64>,
        reply: oneshot::Sender<Result<ScheduledTask, SchedulerError>>,
    },
    Delete {
        id: String,
        reply: oneshot::Sender<bool>,
    },
    List {
        reply: oneshot::Sender<Vec<ScheduledTask>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_recurring_task_has_7_day_expiry() {
        let task = ScheduledTask::new(300, "check deploy".into(), true, false);
        assert!(task.expires_at.is_some());
        let expiry = task.expires_at.unwrap();
        let diff = expiry - task.created_at;
        assert_eq!(diff.num_days(), 7);
    }

    #[test]
    fn new_one_shot_task_has_no_expiry() {
        let task = ScheduledTask::new(300, "check deploy".into(), false, false);
        assert!(task.expires_at.is_none());
    }

    #[test]
    fn next_fire_at_uses_created_at_when_never_fired() {
        let task = ScheduledTask::new(300, "test".into(), true, false);
        let expected = task.created_at + chrono::Duration::seconds(300);
        assert_eq!(task.next_fire_at(), expected);
    }

    #[test]
    fn next_fire_at_uses_last_fired_at_when_present() {
        let mut task = ScheduledTask::new(300, "test".into(), true, false);
        let fired = Utc::now();
        task.last_fired_at = Some(fired);
        let expected = fired + chrono::Duration::seconds(300);
        assert_eq!(task.next_fire_at(), expected);
    }

    #[test]
    fn is_expired_returns_true_when_past_expiry() {
        let mut task = ScheduledTask::new(300, "test".into(), true, false);
        task.expires_at = Some(Utc::now() - chrono::Duration::hours(1));
        assert!(task.is_expired(Utc::now()));
    }

    #[test]
    fn is_expired_returns_false_when_before_expiry() {
        let task = ScheduledTask::new(300, "test".into(), true, false);
        assert!(!task.is_expired(Utc::now()));
    }

    #[test]
    fn is_expired_returns_false_for_one_shot() {
        let task = ScheduledTask::new(300, "test".into(), false, false);
        assert!(!task.is_expired(Utc::now()));
    }

    #[test]
    fn legacy_state_without_recurring_field_deserializes_as_recurring() {
        let json = r#"{"id":"abc123","intervalSecs":300,"prompt":"check",
                       "durable":true,"createdAt":"2026-01-01T00:00:00Z",
                       "lastFiredAt":null,"expiresAt":null}"#;
        let task: ScheduledTask = serde_json::from_str(json).unwrap();
        assert!(task.recurring);
    }

    #[test]
    fn legacy_one_shot_state_still_deserializes() {
        let json = r#"{"id":"abc123","intervalSecs":300,"prompt":"check",
                       "recurring":false,"durable":true,
                       "createdAt":"2026-01-01T00:00:00Z",
                       "lastFiredAt":null,"expiresAt":null}"#;
        let task: ScheduledTask = serde_json::from_str(json).unwrap();
        assert!(!task.recurring);
    }

    #[test]
    fn task_id_is_12_chars() {
        let task = ScheduledTask::new(300, "test".into(), true, false);
        assert_eq!(task.id.len(), 12);
    }

    #[test]
    fn scheduler_state_default_is_empty() {
        let state = SchedulerState::default();
        assert!(state.tasks.is_empty());
    }
}
