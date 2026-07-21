//! Virtual (non-process) background tasks overlaid on a real terminal
//! backend.
//!
//! [`VirtualTaskTerminalBackend`] decorates the session's `dyn
//! TerminalBackend` so in-process long-running work (today: background
//! `workflow` runs) resolves through the exact same surfaces as
//! process-backed tasks: `get_task_output` polling, blocking
//! `wait_for_completion`, the model's `kill_task`, the TUI kill button
//! (which routes terminal-only), the `list_tasks` completion reminder, and
//! session compaction summaries. Everything that is not a virtual-task hit
//! delegates to the wrapped backend unchanged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::types::{
    BackgroundHandle, ComputerError, KillOutcome, TaskSnapshot, TerminalBackend,
    TerminalRunRequest, TerminalRunResult, VirtualTaskHandle,
};

struct VirtualTask {
    snapshot: TaskSnapshot,
    cancellation: CancellationToken,
    done: Arc<tokio::sync::Notify>,
}

/// Registry of virtual tasks, shared by the decorator's clones.
#[derive(Default)]
struct VirtualTaskRegistry {
    tasks: parking_lot::Mutex<HashMap<String, VirtualTask>>,
}

impl VirtualTaskRegistry {
    fn register(&self, snapshot: TaskSnapshot) -> VirtualTaskHandle {
        let cancellation = CancellationToken::new();
        let handle = VirtualTaskHandle {
            cancellation: cancellation.clone(),
        };
        self.tasks.lock().insert(
            snapshot.task_id.clone(),
            VirtualTask {
                snapshot,
                cancellation,
                done: Arc::new(tokio::sync::Notify::new()),
            },
        );
        handle
    }

    fn get(&self, task_id: &str) -> Option<TaskSnapshot> {
        self.tasks
            .lock()
            .get(task_id)
            .map(|task| task.snapshot.clone())
    }

    fn list(&self) -> Vec<TaskSnapshot> {
        self.tasks
            .lock()
            .values()
            .map(|task| task.snapshot.clone())
            .collect()
    }

    fn kill(&self, task_id: &str) -> Option<KillOutcome> {
        let mut tasks = self.tasks.lock();
        let task = tasks.get_mut(task_id)?;
        if task.snapshot.completed {
            return Some(KillOutcome::AlreadyExited);
        }
        task.snapshot.explicitly_killed = true;
        task.cancellation.cancel();
        Some(KillOutcome::Killed)
    }

    fn complete(
        &self,
        task_id: &str,
        output: String,
        exit_code: Option<i32>,
    ) -> Option<TaskSnapshot> {
        let (snapshot, done) = {
            let mut tasks = self.tasks.lock();
            let task = tasks.get_mut(task_id)?;
            task.snapshot.completed = true;
            task.snapshot.end_time = Some(std::time::SystemTime::now());
            task.snapshot.exit_code = exit_code;
            task.snapshot.output = output;
            (task.snapshot.clone(), task.done.clone())
        };
        done.notify_waiters();
        Some(snapshot)
    }

    async fn wait(&self, task_id: &str, timeout: Option<Duration>) -> Option<TaskSnapshot> {
        // Snapshot the notifier before checking state so a completion that
        // lands between the check and the wait still wakes us.
        let (done, completed) = {
            let tasks = self.tasks.lock();
            let task = tasks.get(task_id)?;
            (task.done.clone(), task.snapshot.completed)
        };
        if completed {
            return self.mark_block_waited(task_id);
        }
        let notified = done.notified();
        match timeout {
            Some(timeout) => {
                if tokio::time::timeout(timeout, notified).await.is_err() {
                    // Timed out: report current (still running) state.
                    return self.get(task_id);
                }
            }
            None => notified.await,
        }
        self.mark_block_waited(task_id)
    }

    /// A blocking waiter consumed the terminal state: suppress the duplicate
    /// completion auto-wake, mirroring the process-task backend.
    fn mark_block_waited(&self, task_id: &str) -> Option<TaskSnapshot> {
        let mut tasks = self.tasks.lock();
        let task = tasks.get_mut(task_id)?;
        if task.snapshot.completed {
            task.snapshot.block_waited = true;
        }
        Some(task.snapshot.clone())
    }
}

/// Decorator adding virtual-task support to any [`TerminalBackend`].
pub struct VirtualTaskTerminalBackend {
    inner: Arc<dyn TerminalBackend>,
    registry: VirtualTaskRegistry,
}

impl VirtualTaskTerminalBackend {
    pub fn new(inner: Arc<dyn TerminalBackend>) -> Self {
        Self {
            inner,
            registry: VirtualTaskRegistry::default(),
        }
    }
}

#[async_trait::async_trait]
impl TerminalBackend for VirtualTaskTerminalBackend {
    async fn run(&self, request: TerminalRunRequest) -> Result<TerminalRunResult, ComputerError> {
        self.inner.run(request).await
    }

    async fn run_background(
        &self,
        request: TerminalRunRequest,
    ) -> Result<BackgroundHandle, ComputerError> {
        self.inner.run_background(request).await
    }

    async fn get_task(&self, task_id: &str) -> Option<TaskSnapshot> {
        match self.registry.get(task_id) {
            Some(snapshot) => Some(snapshot),
            None => self.inner.get_task(task_id).await,
        }
    }

    async fn kill_task(&self, task_id: &str) -> KillOutcome {
        match self.registry.kill(task_id) {
            Some(outcome) => outcome,
            None => self.inner.kill_task(task_id).await,
        }
    }

    async fn kill_foreground_commands(&self) {
        self.inner.kill_foreground_commands().await;
    }

    async fn kill_foreground_commands_by_owner(&self, owner_session_id: &str) {
        self.inner
            .kill_foreground_commands_by_owner(owner_session_id)
            .await;
    }

    async fn kill_all_background_tasks(&self) {
        for snapshot in self.registry.list() {
            let _ = self.registry.kill(&snapshot.task_id);
        }
        self.inner.kill_all_background_tasks().await;
    }

    async fn kill_all_background_tasks_by_owner(&self, owner_session_id: &str) {
        for snapshot in self.registry.list() {
            if snapshot.owner_session_id.as_deref() == Some(owner_session_id) {
                let _ = self.registry.kill(&snapshot.task_id);
            }
        }
        self.inner
            .kill_all_background_tasks_by_owner(owner_session_id)
            .await;
    }

    async fn warm_shell(&self, cwd: &std::path::Path) {
        self.inner.warm_shell(cwd).await;
    }

    async fn reparent_notifications(
        &self,
        old_owner_session_id: &str,
        new_owner_session_id: &str,
        new_handle: crate::notification::types::ToolNotificationHandle,
        backend_weak: std::sync::Weak<dyn TerminalBackend>,
    ) {
        self.inner
            .reparent_notifications(
                old_owner_session_id,
                new_owner_session_id,
                new_handle,
                backend_weak,
            )
            .await;
    }

    async fn background_foreground_command(&self, tool_call_id: &str) -> bool {
        self.inner.background_foreground_command(tool_call_id).await
    }

    async fn wait_for_completion(
        &self,
        task_id: &str,
        timeout: Option<Duration>,
    ) -> Option<TaskSnapshot> {
        match self.registry.wait(task_id, timeout).await {
            Some(snapshot) => Some(snapshot),
            None => self.inner.wait_for_completion(task_id, timeout).await,
        }
    }

    async fn list_tasks(&self) -> Vec<TaskSnapshot> {
        let mut tasks = self.inner.list_tasks().await;
        tasks.extend(self.registry.list());
        tasks
    }

    async fn get_shell_cwd(&self) -> Option<std::path::PathBuf> {
        self.inner.get_shell_cwd().await
    }

    async fn register_virtual_task(&self, snapshot: TaskSnapshot) -> Option<VirtualTaskHandle> {
        Some(self.registry.register(snapshot))
    }

    async fn complete_virtual_task(
        &self,
        task_id: &str,
        output: String,
        exit_code: Option<i32>,
    ) -> Option<TaskSnapshot> {
        self.registry.complete(task_id, output, exit_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computer::types::TaskKind;

    /// Inner backend that panics if any task lookup reaches it — proves the
    /// overlay answers virtual ids without delegating.
    struct RejectingInner;

    #[async_trait::async_trait]
    impl TerminalBackend for RejectingInner {
        async fn run(
            &self,
            _request: TerminalRunRequest,
        ) -> Result<TerminalRunResult, ComputerError> {
            Err(ComputerError::io("test stub"))
        }
        async fn run_background(
            &self,
            _request: TerminalRunRequest,
        ) -> Result<BackgroundHandle, ComputerError> {
            Err(ComputerError::io("test stub"))
        }
        async fn get_task(&self, _task_id: &str) -> Option<TaskSnapshot> {
            None
        }
        async fn kill_task(&self, _task_id: &str) -> KillOutcome {
            KillOutcome::NotFound
        }
        async fn wait_for_completion(
            &self,
            _task_id: &str,
            _timeout: Option<Duration>,
        ) -> Option<TaskSnapshot> {
            None
        }
        async fn list_tasks(&self) -> Vec<TaskSnapshot> {
            Vec::new()
        }
    }

    fn snapshot(task_id: &str) -> TaskSnapshot {
        TaskSnapshot {
            task_id: task_id.to_string(),
            command: "workflow test".to_string(),
            display_command: None,
            cwd: "/tmp".to_string(),
            start_time: std::time::SystemTime::now(),
            end_time: None,
            output: String::new(),
            output_file: std::path::PathBuf::from("/tmp/progress.log"),
            truncated: false,
            exit_code: None,
            signal: None,
            completed: false,
            kind: TaskKind::Bash,
            block_waited: false,
            explicitly_killed: false,
            owner_session_id: Some("session-1".to_string()),
        }
    }

    fn backend() -> VirtualTaskTerminalBackend {
        VirtualTaskTerminalBackend::new(Arc::new(RejectingInner))
    }

    #[tokio::test]
    async fn virtual_task_round_trip_get_complete_wait() {
        let backend = backend();
        let handle = backend
            .register_virtual_task(snapshot("run-1"))
            .await
            .expect("decorator supports virtual tasks");
        assert!(!handle.cancellation.is_cancelled());

        let running = backend.get_task("run-1").await.expect("visible");
        assert!(!running.completed);
        assert_eq!(backend.list_tasks().await.len(), 1);

        let done = backend
            .complete_virtual_task("run-1", "final output".into(), Some(0))
            .await
            .expect("completes");
        assert!(done.completed);
        assert_eq!(done.output, "final output");
        assert_eq!(done.exit_code, Some(0));
        assert!(!done.block_waited, "no waiter consumed it");

        // A wait after completion returns immediately and marks block_waited.
        let waited = backend
            .wait_for_completion("run-1", Some(Duration::from_millis(10)))
            .await
            .expect("waits");
        assert!(waited.completed);
        assert!(waited.block_waited);
    }

    #[tokio::test]
    async fn kill_fires_cancellation_and_reports_killed() {
        let backend = backend();
        let handle = backend
            .register_virtual_task(snapshot("run-2"))
            .await
            .expect("registers");

        assert!(matches!(
            backend.kill_task("run-2").await,
            KillOutcome::Killed
        ));
        assert!(handle.cancellation.is_cancelled());
        // Owner winds down and completes; the explicit-kill flag survives so
        // the completion auto-wake stays suppressed.
        let done = backend
            .complete_virtual_task("run-2", "partial".into(), Some(130))
            .await
            .expect("completes");
        assert!(done.explicitly_killed);
        assert!(matches!(
            backend.kill_task("run-2").await,
            KillOutcome::AlreadyExited
        ));
    }

    #[tokio::test]
    async fn blocking_wait_wakes_on_completion() {
        let backend = Arc::new(backend());
        backend
            .register_virtual_task(snapshot("run-3"))
            .await
            .expect("registers");
        let waiter = {
            let backend = backend.clone();
            tokio::spawn(async move {
                backend
                    .wait_for_completion("run-3", Some(Duration::from_secs(5)))
                    .await
            })
        };
        tokio::task::yield_now().await;
        backend
            .complete_virtual_task("run-3", "done".into(), Some(0))
            .await
            .expect("completes");
        let snapshot = waiter.await.expect("join").expect("snapshot");
        assert!(snapshot.completed);
        assert_eq!(snapshot.output, "done");
    }

    #[tokio::test]
    async fn unknown_ids_delegate_to_inner() {
        let backend = backend();
        assert!(backend.get_task("nope").await.is_none());
        assert!(matches!(
            backend.kill_task("nope").await,
            KillOutcome::NotFound
        ));
        assert!(
            backend
                .complete_virtual_task("nope", String::new(), None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn owner_scoped_kill_sweep_hits_virtual_tasks() {
        let backend = backend();
        backend
            .register_virtual_task(snapshot("run-4"))
            .await
            .expect("registers");
        backend
            .kill_all_background_tasks_by_owner("session-1")
            .await;
        let killed = backend.get_task("run-4").await.expect("visible");
        assert!(killed.explicitly_killed);
    }
}
