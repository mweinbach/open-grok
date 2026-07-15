//! AcpTerminalAdapter: implements `xai-grok-tools::TerminalBackend` using ACP gateway calls.
//!
//! This adapter enables bash tool execution over ACP (remote execution).
//! It translates xai-grok-tools' `TerminalBackend` trait into ACP protocol calls:
//!   `run()` → create_terminal → wait_for_exit → terminal_output → release_terminal
//!   `run_background()` → create_terminal + spawn exit watcher
//!   `get_task()` → terminal_output (merged with tracked metadata)
//!   `kill_task()` → kill_terminal_command (watcher detects exit)
//!   `wait_for_completion()` → wait_for_terminal_exit with timeout

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol as acp;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_tools::computer::types::{
    BackgroundHandle, ComputerError, KillOutcome, TaskSnapshot, TerminalBackend,
    TerminalRunRequest, TerminalRunResult,
};
use xai_grok_tools::notification::types::ToolNotificationHandle;

// ── Tracked task state ───────────────────────────────────────────────

struct TrackedTask {
    command: String,
    display_command: Option<String>,
    cwd: String,
    output_file: PathBuf,
    start_time: std::time::SystemTime,
    completed: bool,
    exit_code: Option<i32>,
    signal: Option<String>,
    last_output: String,
    last_truncated: bool,
    block_waited: bool,
    explicitly_killed: bool,
}

impl TrackedTask {
    fn mark_completed(
        &mut self,
        exit_code: Option<i32>,
        signal: Option<String>,
        output: String,
        truncated: bool,
    ) {
        self.completed = true;
        self.exit_code = exit_code;
        self.signal = signal;
        self.last_output = output;
        self.last_truncated = truncated;
    }

    fn to_snapshot(
        &self,
        task_id: &str,
        output: String,
        truncated: bool,
        exit_code: Option<i32>,
        signal: Option<String>,
    ) -> TaskSnapshot {
        let completed = self.completed || exit_code.is_some();
        TaskSnapshot {
            task_id: task_id.to_string(),
            command: self.command.clone(),
            display_command: self.display_command.clone(),
            cwd: self.cwd.clone(),
            start_time: self.start_time,
            end_time: completed.then(std::time::SystemTime::now),
            output,
            output_file: self.output_file.clone(),
            truncated,
            exit_code,
            signal,
            completed,
            block_waited: self.block_waited,
            explicitly_killed: self.explicitly_killed,
            kind: xai_grok_tools::computer::types::TaskKind::Bash,
            owner_session_id: None,
        }
    }
}

type TaskMap = Arc<Mutex<HashMap<String, TrackedTask>>>;

// ── Exit watcher ─────────────────────────────────────────────────────

/// Spawned per background task. Blocks on `WaitForTerminalExitRequest`,
/// then fetches final output, emits `TaskCompleted`, and releases the
/// terminal.
async fn watch_for_exit(
    gateway: GatewaySender,
    session_id: acp::SessionId,
    task_id: String,
    tasks: TaskMap,
    notification_handle: ToolNotificationHandle,
) {
    let terminal_id = acp::TerminalId::new(task_id.clone());

    match gateway
        .send(acp::WaitForTerminalExitRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .await
    {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                task_id,
                error = %e,
                "watch_for_exit: gateway error waiting for terminal exit, polling until exit"
            );
            if !poll_for_terminal_exit(&gateway, &session_id, &terminal_id, None).await {
                // Gateway lost — mark the task as completed so it doesn't
                // remain as a ghost "running" entry forever.
                let snapshot = {
                    let mut tasks = tasks.lock().unwrap();
                    let Some(task) = tasks.get_mut(&task_id) else {
                        return;
                    };
                    task.mark_completed(None, Some("gateway-lost".into()), String::new(), false);
                    task.to_snapshot(
                        &task_id,
                        String::new(),
                        false,
                        None,
                        Some("gateway-lost".into()),
                    )
                };
                notification_handle.send_task_complete(snapshot);
                let _ = gateway
                    .send(acp::ReleaseTerminalRequest::new(session_id, terminal_id))
                    .await;
                return;
            }
        }
    }

    let (exit_code, signal, output_text, truncated) = match gateway
        .send(acp::TerminalOutputRequest::new(
            session_id.clone(),
            terminal_id.clone(),
        ))
        .await
    {
        Ok(o) => {
            let (code, sig) = parse_exit(&o.exit_status);
            (code, sig, o.output, o.truncated)
        }
        Err(_) => (None, None, String::new(), false),
    };

    let snapshot = {
        let mut tasks = tasks.lock().unwrap();
        let Some(task) = tasks.get_mut(&task_id) else {
            return;
        };
        task.mark_completed(exit_code, signal.clone(), output_text.clone(), truncated);
        task.to_snapshot(&task_id, output_text, truncated, exit_code, signal)
    };

    notification_handle.send_task_complete(snapshot);

    let _ = gateway
        .send(acp::ReleaseTerminalRequest::new(session_id, terminal_id))
        .await;
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Poll `TerminalOutputRequest` at 500ms intervals until `exit_status` is
/// present, a deadline is hit, or 60 consecutive gateway errors occur.
/// Returns `true` when an exit was detected.
async fn poll_for_terminal_exit(
    gateway: &GatewaySender,
    session_id: &acp::SessionId,
    terminal_id: &acp::TerminalId,
    deadline: Option<tokio::time::Instant>,
) -> bool {
    let mut consecutive_errors = 0u32;
    loop {
        if let Some(dl) = deadline
            && tokio::time::Instant::now() >= dl
        {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        match gateway
            .send(acp::TerminalOutputRequest::new(
                session_id.clone(),
                terminal_id.clone(),
            ))
            .await
        {
            Ok(output) => {
                consecutive_errors = 0;
                if output.exit_status.is_some() {
                    return true;
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= 60 {
                    tracing::error!(
                        terminal_id = %terminal_id.0,
                        error = %e,
                        "gateway unreachable after 60 consecutive poll failures"
                    );
                    return false;
                }
            }
        }
    }
}

fn wrap_command(command: &str) -> Result<String, ComputerError> {
    // On Windows the ACP client (grok-desktop) spawns with `shell: true`
    // which delegates to cmd.exe.  Wrapping in /bin/bash would fail because
    // that path doesn't exist on Windows.  Send the raw command instead.
    #[cfg(not(unix))]
    {
        let _ = command;
        Ok(command.to_string())
    }
    #[cfg(unix)]
    {
        let quoted = shlex::try_quote(command).map_err(|_| ComputerError::CommandNotQuoted)?;
        Ok(format!(
            "{} -lc {quoted}",
            crate::terminal::default_shell_path()
        ))
    }
}

fn to_env(env: HashMap<String, String>) -> Vec<acp::EnvVariable> {
    env.into_iter()
        .map(|(name, value)| acp::EnvVariable::new(name, value))
        .collect()
}

fn parse_exit(status: &Option<acp::TerminalExitStatus>) -> (Option<i32>, Option<String>) {
    match status {
        Some(e) => (e.exit_code.map(|v| v as i32), e.signal.clone()),
        None => (None, None),
    }
}

// ── Adapter ──────────────────────────────────────────────────────────

/// Wraps xai-grok-shell's ACP gateway to satisfy xai-grok-tools' TerminalBackend.
pub struct AcpTerminalAdapter {
    gateway: GatewaySender,
    session_id: acp::SessionId,
    tasks: TaskMap,
}

impl AcpTerminalAdapter {
    pub fn new(gateway: GatewaySender, session_id: acp::SessionId) -> Self {
        Self {
            gateway,
            session_id,
            tasks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn create_terminal(
        &self,
        command: String,
        request: &TerminalRunRequest,
    ) -> Result<acp::CreateTerminalResponse, ComputerError> {
        self.gateway
            .send(
                acp::CreateTerminalRequest::new(self.session_id.clone(), command)
                    .args(vec![])
                    .env(to_env(request.env.clone()))
                    .cwd(Some(request.working_directory.clone()))
                    .output_byte_limit(Some(request.output_byte_limit as u64)),
            )
            .await
            .map_err(|e| ComputerError::io(e.to_string()))
    }

    fn terminal_id(&self, task_id: &str) -> acp::TerminalId {
        acp::TerminalId::new(task_id)
    }
}

#[async_trait::async_trait]
impl TerminalBackend for AcpTerminalAdapter {
    async fn run(&self, request: TerminalRunRequest) -> Result<TerminalRunResult, ComputerError> {
        let command = wrap_command(&request.command)?;
        let create_res = self.create_terminal(command, &request).await?;

        let timed_out = match tokio::time::timeout(
            request.timeout,
            self.gateway.send(acp::WaitForTerminalExitRequest::new(
                self.session_id.clone(),
                create_res.terminal_id.clone(),
            )),
        )
        .await
        {
            Ok(Ok(_)) => false,
            Ok(Err(e)) => return Err(ComputerError::io(e.to_string())),
            Err(_) => {
                let _ = self
                    .gateway
                    .send(acp::KillTerminalRequest::new(
                        self.session_id.clone(),
                        create_res.terminal_id.clone(),
                    ))
                    .await;
                true
            }
        };

        let output = self
            .gateway
            .send(acp::TerminalOutputRequest::new(
                self.session_id.clone(),
                create_res.terminal_id.clone(),
            ))
            .await
            .map_err(|e| ComputerError::io(e.to_string()))?;

        let _ = self
            .gateway
            .send(acp::ReleaseTerminalRequest::new(
                self.session_id.clone(),
                create_res.terminal_id,
            ))
            .await;

        let (exit_code, signal) = parse_exit(&output.exit_status);
        let total_bytes = output.output.len();
        Ok(TerminalRunResult {
            combined_output: output.output,
            exit_code,
            truncated: output.truncated,
            signal,
            timed_out,
            output_file: request.output_file,
            total_bytes,
            // ACP gateway does not surface a local PID -- the process
            // runs on the remote side.
            pid: None,
        })
    }

    async fn run_background(
        &self,
        request: TerminalRunRequest,
    ) -> Result<BackgroundHandle, ComputerError> {
        let command = wrap_command(&request.command)?;
        let notification_handle = request.notification_handle.clone();
        let display_command = request.display_command.clone();
        let cwd = request.working_directory.to_string_lossy().to_string();
        let output_file = request.output_file.clone();

        let create_res = self.create_terminal(command.clone(), &request).await?;
        let task_id = create_res.terminal_id.0.to_string();

        {
            let mut tasks = self.tasks.lock().unwrap();
            tasks.insert(
                task_id.clone(),
                TrackedTask {
                    command,
                    display_command,
                    cwd,
                    output_file: output_file.clone(),
                    start_time: std::time::SystemTime::now(),
                    completed: false,
                    exit_code: None,
                    signal: None,
                    last_output: String::new(),
                    last_truncated: false,
                    block_waited: false,
                    explicitly_killed: false,
                },
            );
        }

        tokio::spawn(watch_for_exit(
            self.gateway.clone(),
            self.session_id.clone(),
            task_id.clone(),
            Arc::clone(&self.tasks),
            notification_handle,
        ));

        Ok(BackgroundHandle {
            task_id,
            output_file,
            // ACP gateway does not surface a local PID -- the process
            // runs on the remote side.
            pid: None,
        })
    }

    async fn get_task(&self, task_id: &str) -> Option<TaskSnapshot> {
        let live = self
            .gateway
            .send(acp::TerminalOutputRequest::new(
                self.session_id.clone(),
                self.terminal_id(task_id),
            ))
            .await
            .ok();

        let tasks = self.tasks.lock().unwrap();
        let tracked = tasks.get(task_id);

        match (live, tracked) {
            (Some(output), Some(tracked)) => {
                let (exit_code, signal) = parse_exit(&output.exit_status);
                Some(tracked.to_snapshot(
                    task_id,
                    output.output,
                    output.truncated,
                    exit_code,
                    signal,
                ))
            }
            (Some(output), None) => {
                let (exit_code, signal) = parse_exit(&output.exit_status);
                let completed = exit_code.is_some();
                Some(TaskSnapshot {
                    task_id: task_id.to_string(),
                    command: String::new(),
                    display_command: None,
                    cwd: String::new(),
                    start_time: std::time::SystemTime::now(),
                    end_time: completed.then(std::time::SystemTime::now),
                    output: output.output,
                    output_file: PathBuf::new(),
                    truncated: output.truncated,
                    exit_code,
                    signal,
                    completed,
                    kind: xai_grok_tools::computer::types::TaskKind::Bash,
                    block_waited: false,
                    explicitly_killed: false,
                    owner_session_id: None,
                })
            }
            (None, Some(tracked)) if tracked.completed => Some(tracked.to_snapshot(
                task_id,
                tracked.last_output.clone(),
                tracked.last_truncated,
                tracked.exit_code,
                tracked.signal.clone(),
            )),
            _ => None,
        }
    }

    async fn kill_task(&self, task_id: &str) -> KillOutcome {
        // Mark as explicitly killed BEFORE sending the kill request so the
        // exit watcher's snapshot carries the flag.
        {
            let mut tasks = self.tasks.lock().unwrap();
            if let Some(task) = tasks.get_mut(task_id) {
                task.explicitly_killed = true;
            }
        }

        match self
            .gateway
            .send(acp::KillTerminalRequest::new(
                self.session_id.clone(),
                self.terminal_id(task_id),
            ))
            .await
        {
            Ok(_) => KillOutcome::Killed,
            Err(_) => KillOutcome::NotFound,
        }
    }

    async fn wait_for_completion(
        &self,
        task_id: &str,
        timeout: Option<Duration>,
    ) -> Option<TaskSnapshot> {
        let timeout = timeout.unwrap_or(Duration::from_secs(30));

        // Mark BEFORE waiting so watch_for_exit sees the flag in its snapshot.
        {
            let mut tasks = self.tasks.lock().unwrap();
            if let Some(task) = tasks.get_mut(task_id) {
                task.block_waited = true;
            }
        }

        let gateway_result = tokio::time::timeout(
            timeout,
            self.gateway.send(acp::WaitForTerminalExitRequest::new(
                self.session_id.clone(),
                self.terminal_id(task_id),
            )),
        )
        .await;

        match &gateway_result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(task_id, error = %e, "gateway error waiting for terminal exit, falling back to polling");
                let deadline = tokio::time::Instant::now() + timeout;
                poll_for_terminal_exit(
                    &self.gateway,
                    &self.session_id,
                    &self.terminal_id(task_id),
                    Some(deadline),
                )
                .await;
            }
            Err(_) => {
                tracing::debug!(task_id, "timeout waiting for terminal exit");
                // The block timed out: the agent did not receive the
                // completion result, so auto-wake should still fire
                // when the task eventually completes.
                let mut tasks = self.tasks.lock().unwrap();
                if let Some(task) = tasks.get_mut(task_id) {
                    task.block_waited = false;
                }
            }
        }

        self.get_task(task_id).await
    }

    async fn list_tasks(&self) -> Vec<TaskSnapshot> {
        let task_ids: Vec<String> = {
            let tasks = self.tasks.lock().unwrap();
            tasks.keys().cloned().collect()
        };
        let mut snapshots = Vec::new();
        for task_id in task_ids {
            if let Some(snapshot) = self.get_task(&task_id).await {
                snapshots.push(snapshot);
            }
        }
        snapshots
    }

    async fn kill_all_background_tasks(&self) {
        let task_ids: Vec<String> = {
            let tasks = self.tasks.lock().unwrap();
            tasks
                .iter()
                .filter(|(_, t)| !t.completed)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for task_id in task_ids {
            self.kill_task(&task_id).await;
        }
    }

    async fn kill_foreground_commands(&self) {
        let session_id = self.session_id.0.to_string();
        crate::terminal::kill_and_release_all_for_session(&session_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tracked_task(command: &str) -> TrackedTask {
        TrackedTask {
            command: command.to_string(),
            display_command: None,
            cwd: "/tmp".to_string(),
            output_file: PathBuf::from("/tmp/out.log"),
            start_time: std::time::SystemTime::now(),
            completed: false,
            exit_code: None,
            signal: None,
            last_output: String::new(),
            last_truncated: false,
            block_waited: false,
            explicitly_killed: false,
        }
    }

    #[test]
    fn wrap_command_quotes_shell_metacharacters() {
        let cmd = wrap_command("echo 'hello world' && ls").unwrap();
        #[cfg(unix)]
        {
            // The resolved bash path may live in any prefix (`/bin`,
            // `/opt/homebrew/bin`, `/run/current-system/sw/bin`, …), so just
            // assert the prefix shape: `<resolved-bash> -lc <quoted-cmd>`.
            let shell = crate::terminal::default_shell_path();
            assert!(
                cmd.starts_with(&format!("{shell} -lc")),
                "expected wrapped cmd to begin with `{shell} -lc`, got: {cmd}"
            );
        }
        #[cfg(not(unix))]
        assert_eq!(cmd, "echo 'hello world' && ls");
        assert!(cmd.contains("echo"));
    }

    #[test]
    fn parse_exit_with_code() {
        let status = Some(acp::TerminalExitStatus::new().exit_code(Some(42)));
        let (code, sig) = parse_exit(&status);
        assert_eq!(code, Some(42));
        assert_eq!(sig, None);
    }

    #[test]
    fn parse_exit_with_signal() {
        let status = Some(acp::TerminalExitStatus::new().signal(Some("SIGKILL".into())));
        let (code, sig) = parse_exit(&status);
        assert_eq!(code, None);
        assert_eq!(sig, Some("SIGKILL".into()));
    }

    #[test]
    fn parse_exit_none() {
        assert_eq!(parse_exit(&None), (None, None));
    }

    #[test]
    fn tracked_task_mark_completed() {
        let mut task = make_tracked_task("sleep 10");
        assert!(!task.completed);
        assert_eq!(task.exit_code, None);

        task.mark_completed(Some(137), Some("SIGTERM".into()), "output".into(), false);
        assert!(task.completed);
        assert_eq!(task.exit_code, Some(137));
        assert_eq!(task.signal, Some("SIGTERM".into()));
        assert_eq!(task.last_output, "output");
    }

    #[test]
    fn tracked_task_to_snapshot_running() {
        let task = make_tracked_task("ls -la");
        let snap = task.to_snapshot("t-1", "file1\nfile2".into(), false, None, None);

        assert_eq!(snap.task_id, "t-1");
        assert_eq!(snap.command, "ls -la");
        assert_eq!(snap.cwd, "/tmp");
        assert_eq!(snap.output, "file1\nfile2");
        assert!(!snap.completed);
        assert!(snap.end_time.is_none());
        assert_eq!(snap.exit_code, None);
    }

    #[test]
    fn tracked_task_to_snapshot_completed() {
        let mut task = make_tracked_task("echo done");
        task.mark_completed(Some(0), None, "done\n".into(), false);
        let snap = task.to_snapshot("t-2", "done\n".into(), false, Some(0), None);

        assert!(snap.completed);
        assert!(snap.end_time.is_some());
        assert_eq!(snap.exit_code, Some(0));
        assert_eq!(snap.signal, None);
    }

    #[test]
    fn tracked_task_to_snapshot_completed_by_exit_code_alone() {
        let task = make_tracked_task("fast cmd");
        let snap = task.to_snapshot("t-3", String::new(), false, Some(1), None);
        assert!(snap.completed);
        assert!(snap.end_time.is_some());
    }

    #[test]
    fn tracked_task_to_snapshot_preserves_display_command() {
        let mut task = make_tracked_task("/bin/bash -lc 'echo hi'");
        task.display_command = Some("echo hi".into());
        let snap = task.to_snapshot("t-4", String::new(), false, None, None);
        assert_eq!(snap.display_command, Some("echo hi".into()));
    }

    #[test]
    fn task_map_insert_and_mark_completed() {
        let tasks: TaskMap = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = tasks.lock().unwrap();
            map.insert("t-1".into(), make_tracked_task("sleep 60"));
        }
        {
            let mut map = tasks.lock().unwrap();
            let task = map.get_mut("t-1").unwrap();
            task.mark_completed(Some(143), Some("SIGTERM".into()), String::new(), false);
            assert!(task.completed);
        }
        {
            let map = tasks.lock().unwrap();
            let task = map.get("t-1").unwrap();
            assert!(task.completed);
            assert_eq!(task.exit_code, Some(143));
        }
    }

    #[test]
    fn task_map_filter_running() {
        let tasks: TaskMap = Arc::new(Mutex::new(HashMap::new()));
        {
            let mut map = tasks.lock().unwrap();
            map.insert("running-1".into(), make_tracked_task("sleep 60"));
            let mut done = make_tracked_task("echo done");
            done.mark_completed(Some(0), None, String::new(), false);
            map.insert("done-1".into(), done);
            map.insert("running-2".into(), make_tracked_task("sleep 120"));
        }
        let running: Vec<String> = {
            let map = tasks.lock().unwrap();
            map.iter()
                .filter(|(_, t)| !t.completed)
                .map(|(id, _)| id.clone())
                .collect()
        };
        assert_eq!(running.len(), 2);
        assert!(running.contains(&"running-1".into()));
        assert!(running.contains(&"running-2".into()));
    }

    #[test]
    fn completed_task_snapshot_uses_cached_output() {
        let mut task = make_tracked_task("echo hello");
        task.mark_completed(Some(0), None, "hello\n".into(), false);

        let snap = task.to_snapshot(
            "t-5",
            task.last_output.clone(),
            task.last_truncated,
            task.exit_code,
            task.signal.clone(),
        );
        assert!(snap.completed);
        assert_eq!(snap.output, "hello\n");
        assert_eq!(snap.exit_code, Some(0));
    }
}
