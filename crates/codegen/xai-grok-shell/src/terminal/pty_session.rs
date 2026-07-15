//! Agent-scoped interactive PTY manager. PTYs are keyed by `terminalId`,
//! outlive sessions, and multiplex I/O over the existing ACP WebSocket.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{Arc, LazyLock};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::{Mutex, mpsc};
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;

use crate::extensions::routing::{TargetClientId, send_routed_notification};
use crate::terminal::{TerminalExtError, TerminalInfo, TerminalStatus};

const NOTIFICATION_METHOD: &str = "x.ai/terminal/pty/notification";
const OUTPUT_RING_BUFFER_SIZE: usize = 256 * 1024;
const OUTPUT_BATCH_INTERVAL_MS: u64 = 16;
const BUSY_POLL_INTERVAL_MS: u64 = 500;
const INPUT_CHANNEL_CAPACITY: usize = 256;

pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    input_tx: mpsc::Sender<Vec<u8>>,
    output_offset: u64,
    output_ring: VecDeque<u8>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    cwd: Option<String>,
    name: Option<String>,
    created_at: u64,
    rows: u16,
    cols: u16,
    target_client_id: TargetClientId,
    busy: bool,
    gateway: GatewaySender,
}

type PtyMap = HashMap<String, Arc<Mutex<PtySession>>>;

static PTY_REGISTRY: LazyLock<Mutex<PtyMap>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub async fn get_pty(pty_id: &str) -> Option<Arc<Mutex<PtySession>>> {
    PTY_REGISTRY.lock().await.get(pty_id).cloned()
}

pub async fn require_pty(terminal_id: &str) -> Result<Arc<Mutex<PtySession>>, TerminalExtError> {
    get_pty(terminal_id)
        .await
        .ok_or_else(|| TerminalExtError::NotInteractive {
            terminal_id: terminal_id.into(),
        })
}

pub async fn create_pty(
    shell: Option<&str>,
    cwd: Option<&str>,
    env: HashMap<String, String>,
    rows: u16,
    cols: u16,
    name: Option<&str>,
    gateway: GatewaySender,
    target_client_id: TargetClientId,
) -> Result<String, TerminalExtError> {
    let pty_id = uuid::Uuid::now_v7().to_string();

    let pty_system = native_pty_system();
    let size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pair = pty_system
        .openpty(size)
        .map_err(|e| TerminalExtError::Internal(format!("failed to open pty: {e}")))?;

    let (shell_path, shell_args) = resolve_pty_shell(shell);

    let mut cmd = CommandBuilder::new(&shell_path);
    for arg in &shell_args {
        cmd.arg(arg);
    }

    if let Some(dir) = cwd {
        cmd.cwd(dir);
    } else if let Ok(dir) = std::env::current_dir() {
        cmd.cwd(dir);
    }

    for (k, v) in &env {
        cmd.env(k, v);
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("LANG", "en_US.UTF-8");
    cmd.env("LC_ALL", "en_US.UTF-8");

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| TerminalExtError::Internal(format!("failed to spawn shell: {e}")))?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| TerminalExtError::Internal(format!("failed to clone pty reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| TerminalExtError::Internal(format!("failed to take pty writer: {e}")))?;

    let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
    spawn_pty_input_loop(writer, input_rx);

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let resolved_cwd = cwd.map(|s| s.to_string()).or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    });

    let resolved_name = name.map(|s| s.to_string()).or_else(|| {
        // Default to cwd basename, fall back to shell basename.
        resolved_cwd
            .as_deref()
            .and_then(|p| {
                std::path::Path::new(p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .or_else(|| {
                std::path::Path::new(&shell_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
    });

    let session = PtySession {
        master: pair.master,
        input_tx,
        output_offset: 0,
        output_ring: VecDeque::with_capacity(OUTPUT_RING_BUFFER_SIZE),
        child,
        cwd: resolved_cwd,
        name: resolved_name,
        created_at,
        rows,
        cols,
        target_client_id,
        busy: false,
        gateway: gateway.clone(),
    };

    let entry = Arc::new(Mutex::new(session));
    PTY_REGISTRY
        .lock()
        .await
        .insert(pty_id.clone(), entry.clone());

    let pty_id_clone = pty_id.clone();
    tokio::task::spawn_local(run_pty_output_loop(reader, entry, pty_id_clone, gateway));

    Ok(pty_id)
}

fn spawn_pty_input_loop(mut writer: Box<dyn Write + Send>, mut input_rx: mpsc::Receiver<Vec<u8>>) {
    tokio::task::spawn_blocking(move || {
        while let Some(mut chunk) = input_rx.blocking_recv() {
            while let Ok(more) = input_rx.try_recv() {
                chunk.extend_from_slice(&more);
            }
            if writer.write_all(&chunk).is_err() {
                break;
            }
            if writer.flush().is_err() {
                break;
            }
        }
    });
}

/// Reads PTY output, batches on a 16ms tick, and sends notifications.
/// Also samples the foreground process group on a slower tick and pushes
/// `process_started`/`process_ended` on idle↔busy transitions — time-based
/// rather than output-driven because a busy process can be silent.
async fn run_pty_output_loop(
    reader: Box<dyn Read + Send>,
    pty: Arc<Mutex<PtySession>>,
    pty_id: String,
    gateway: GatewaySender,
) {
    use tokio::sync::mpsc;
    use tokio::time::{Duration, interval};

    let (data_tx, mut data_rx) = mpsc::channel::<Vec<u8>>(64);

    tokio::task::spawn_blocking(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if data_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut pending = Vec::new();
    let mut tick = interval(Duration::from_millis(OUTPUT_BATCH_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut busy_tick = interval(Duration::from_millis(BUSY_POLL_INTERVAL_MS));
    busy_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            chunk = data_rx.recv() => {
                match chunk {
                    Some(data) => {
                        {
                            let mut session = pty.lock().await;
                            session.output_ring.extend(data.iter().copied());
                            if session.output_ring.len() > OUTPUT_RING_BUFFER_SIZE {
                                let excess = session.output_ring.len() - OUTPUT_RING_BUFFER_SIZE;
                                session.output_ring.drain(..excess);
                            }
                            session.output_offset += data.len() as u64;
                        }
                        pending.extend_from_slice(&data);
                    }
                    None => {
                        if !pending.is_empty() {
                            flush_output(&pty, &pty_id, &mut pending, &gateway).await;
                        }
                        break;
                    }
                }
            }

            _ = tick.tick() => {
                if !pending.is_empty() {
                    flush_output(&pty, &pty_id, &mut pending, &gateway).await;
                }
            }

            _ = busy_tick.tick() => {
                sample_busy_transition(&pty, &pty_id).await;
            }
        }
    }

    // Child exited
    let (exit_code, signal, target_client_id, was_busy) = tokio::task::spawn_blocking({
        let pty = pty.clone();
        move || {
            let mut session = pty.blocking_lock();
            let target_client_id = session.target_client_id.clone();
            let was_busy = session.busy;
            match session.child.wait() {
                Ok(es) => (
                    Some(es.exit_code() as i32),
                    None::<String>,
                    target_client_id,
                    was_busy,
                ),
                Err(_) => (None, None, target_client_id, was_busy),
            }
        }
    })
    .await
    .unwrap_or_default();

    if was_busy {
        send_busy_notification(&pty_id, false, &target_client_id, &gateway);
    }

    send_routed_notification(
        &gateway,
        NOTIFICATION_METHOD,
        serde_json::json!({
            "terminalId": pty_id,
            "type": "exit",
            "exitCode": exit_code,
            "signal": signal,
        }),
        &target_client_id,
    );
}

/// Whether the PTY's controlling terminal has a foreground process group
/// distinct from the shell itself — i.e. a command is actively running
/// rather than the shell sitting idle at its prompt.
///
/// `process_group_leader()` issues `tcgetpgrp` on the master fd; an idle
/// shell is its own foreground process group, so it matches the shell
/// child's pid. When a command runs in the foreground the kernel reports
/// that command's process group instead. Returns false when the value is
/// unavailable (the shell exited or runs without job control).
///
/// Limitation: a shell that `exec`s a program in place keeps the same pid and
/// pgid, so `tcgetpgrp` still matches the recorded child pid and the program
/// reads as idle. Telling that apart from a real idle prompt needs per-OS
/// process inspection, so a command launched the usual way (fork then exec) is
/// detected while an `exec`-replaced shell is not.
#[cfg(unix)]
fn session_has_foreground_process(session: &PtySession) -> bool {
    let Some(foreground_pgid) = session.master.process_group_leader() else {
        return false;
    };
    match session.child.process_id() {
        Some(shell_pid) => i64::from(foreground_pgid) != i64::from(shell_pid),
        None => false,
    }
}

/// `tcgetpgrp` has no ConPTY equivalent (`process_group_leader` is
/// unix-only in portable-pty), so non-unix PTYs never report a foreground
/// process and clients close terminals without confirmation.
#[cfg(not(unix))]
fn session_has_foreground_process(_session: &PtySession) -> bool {
    false
}

/// Emits `process_started` / `process_ended` only on idle↔busy transitions so
/// a steady state never repeats notifications.
async fn sample_busy_transition(pty: &Arc<Mutex<PtySession>>, pty_id: &str) {
    let mut session = pty.lock().await;
    let now_busy = session_has_foreground_process(&session);
    if now_busy == session.busy {
        return;
    }
    session.busy = now_busy;
    send_busy_notification(
        pty_id,
        now_busy,
        &session.target_client_id,
        &session.gateway,
    );
}

fn send_busy_notification(
    pty_id: &str,
    busy: bool,
    target_client_id: &TargetClientId,
    gateway: &GatewaySender,
) {
    send_routed_notification(
        gateway,
        NOTIFICATION_METHOD,
        serde_json::json!({
            "terminalId": pty_id,
            "type": if busy { "process_started" } else { "process_ended" },
        }),
        target_client_id,
    );
}

async fn flush_output(
    pty: &Arc<Mutex<PtySession>>,
    pty_id: &str,
    pending: &mut Vec<u8>,
    gateway: &GatewaySender,
) {
    use base64::Engine as _;

    let (output_offset, target_client_id) = {
        let session = pty.lock().await;
        (session.output_offset, session.target_client_id.clone())
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&*pending);
    pending.clear();

    send_routed_notification(
        gateway,
        NOTIFICATION_METHOD,
        serde_json::json!({
            "terminalId": pty_id,
            "type": "output",
            "data": b64,
            "outputOffset": output_offset,
        }),
        &target_client_id,
    );
}

pub async fn write_pty_input(pty_id: &str, data: &[u8]) -> Result<(), TerminalExtError> {
    let entry = require_pty(pty_id).await?;
    let input_tx = { entry.lock().await.input_tx.clone() };
    input_tx
        .send(data.to_vec())
        .await
        .map_err(|_| TerminalExtError::InputClosed {
            terminal_id: pty_id.into(),
        })?;
    sample_busy_transition(&entry, pty_id).await;
    Ok(())
}

pub async fn resize_pty(pty_id: &str, rows: u16, cols: u16) -> Result<(), TerminalExtError> {
    let entry = require_pty(pty_id).await?;
    let mut session = entry.lock().await;
    session
        .master
        .resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| TerminalExtError::Internal(format!("failed to resize pty: {e}")))?;
    session.rows = rows;
    session.cols = cols;
    Ok(())
}

pub async fn is_exited(pty_id: &str) -> bool {
    match get_pty(pty_id).await {
        Some(entry) => entry.lock().await.child.try_wait().ok().flatten().is_some(),
        None => true,
    }
}

pub async fn close_pty(pty_id: &str) -> Result<(), String> {
    if let Some(entry) = PTY_REGISTRY.lock().await.remove(pty_id) {
        tokio::task::spawn_blocking(move || {
            let mut session = entry.blocking_lock();
            let _ = session.child.kill();
            let _ = session.child.wait();
        })
        .await
        .map_err(|e| format!("close task failed: {e}"))?;
    }
    Ok(())
}

/// Called on agent disconnect to clean up all PTYs.
pub async fn close_all() {
    let entries: Vec<Arc<Mutex<PtySession>>> = {
        let mut reg = PTY_REGISTRY.lock().await;
        reg.drain().map(|(_, v)| v).collect()
    };
    for entry in entries {
        let _ = tokio::task::spawn_blocking(move || {
            let mut session = entry.blocking_lock();
            let _ = session.child.kill();
            let _ = session.child.wait();
        })
        .await;
    }
}

/// Resolve the shell binary and arguments for an interactive PTY session.
///
/// Priority: explicit `shell` param > `$SHELL` env > platform default.
/// On Windows falls back to the `detect_windows_shell` cascade
/// (pwsh > powershell.exe > Git Bash > cmd.exe, overridable via
/// `GROK_SHELL`) since `$SHELL` is absent.
fn resolve_pty_shell(shell: Option<&str>) -> (String, Vec<String>) {
    if let Some(s) = shell {
        return (s.to_string(), vec![]);
    }

    #[cfg(unix)]
    {
        let path = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        (path, vec!["-l".to_string()])
    }

    #[cfg(not(unix))]
    {
        use xai_grok_config::shell::{WindowsShell, detect_windows_shell};
        match detect_windows_shell() {
            WindowsShell::GitBash(path) => (path.clone(), vec!["-l".to_string()]),
            WindowsShell::Pwsh => ("pwsh".to_string(), vec!["-NoLogo".to_string()]),
            WindowsShell::PowerShell => ("powershell.exe".to_string(), vec!["-NoLogo".to_string()]),
            WindowsShell::Cmd => ("cmd.exe".to_string(), vec![]),
        }
    }
}

pub async fn list_ptys() -> Vec<TerminalInfo> {
    let entries: Vec<(String, Arc<Mutex<PtySession>>)> = {
        let reg = PTY_REGISTRY.lock().await;
        reg.iter()
            .map(|(id, entry)| (id.clone(), entry.clone()))
            .collect()
    };

    let mut result = Vec::with_capacity(entries.len());
    for (id, entry) in entries {
        let mut session = entry.lock().await;
        let (status, exit_code) = match session.child.try_wait() {
            Ok(Some(es)) => (TerminalStatus::Exited, Some(es.exit_code() as i32)),
            Ok(None) => (TerminalStatus::Connected, None),
            Err(_) => (TerminalStatus::Error, None),
        };
        result.push(TerminalInfo {
            terminal_id: id,
            status,
            interactive: true,
            name: session.name.clone(),
            exit_code,
            cwd: session.cwd.clone(),
            output_offset: session.output_offset,
            created_at: session.created_at,
        });
    }
    result
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyLoadResult {
    pub terminal_id: String,
    pub rows: u16,
    pub cols: u16,
    pub exited: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Reconnect to a PTY. Replays the full ring buffer (with `isReplay: true`)
/// so the client can reset its VTE emulator and feed all bytes from scratch.
/// Exited PTYs are still loadable so the client can see final output.
///
/// Updates the stored `target_client_id` so that subsequent output
/// notifications from the output loop are routed to the reconnecting client.
pub async fn load(
    pty_id: &str,
    gateway: &GatewaySender,
    target_client_id: TargetClientId,
) -> Result<PtyLoadResult, TerminalExtError> {
    let entry = require_pty(pty_id).await?;
    let (replay, output_offset, exit_info, rows, cols, busy) = {
        let mut session = entry.lock().await;
        session.target_client_id = target_client_id.clone();
        let exit_info = session
            .child
            .try_wait()
            .ok()
            .flatten()
            .map(|es| es.exit_code() as i32);
        let busy = session_has_foreground_process(&session);
        session.busy = busy;
        (
            session.output_ring.iter().copied().collect::<Vec<u8>>(),
            session.output_offset,
            exit_info,
            session.rows,
            session.cols,
            busy,
        )
    };

    if !replay.is_empty() {
        use base64::Engine as _;
        send_routed_notification(
            gateway,
            NOTIFICATION_METHOD,
            serde_json::json!({
                "terminalId": pty_id,
                "type": "output",
                "data": base64::engine::general_purpose::STANDARD.encode(&replay),
                "outputOffset": output_offset,
                "isReplay": true,
            }),
            &target_client_id,
        );
    }

    let exited = exit_info.is_some();
    if exited {
        send_routed_notification(
            gateway,
            NOTIFICATION_METHOD,
            serde_json::json!({
                "terminalId": pty_id,
                "type": "exit",
                "exitCode": exit_info,
                "isReplay": true,
            }),
            &target_client_id,
        );
    } else {
        send_busy_notification(pty_id, busy, &target_client_id, gateway);
    }

    Ok(PtyLoadResult {
        terminal_id: pty_id.to_string(),
        rows,
        cols,
        exited,
        exit_code: exit_info,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use agent_client_protocol as acp;
    use xai_acp_lib::acp_gateway;

    type RecordedNotifications = Rc<RefCell<Vec<(String, serde_json::Value)>>>;

    struct RecordingClient {
        notifications: RecordedNotifications,
    }

    #[async_trait::async_trait(?Send)]
    impl acp::Client for RecordingClient {
        async fn request_permission(
            &self,
            _: acp::RequestPermissionRequest,
        ) -> acp::Result<acp::RequestPermissionResponse> {
            unimplemented!()
        }

        async fn session_notification(&self, _: acp::SessionNotification) -> acp::Result<()> {
            Ok(())
        }

        async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
            let params: serde_json::Value =
                serde_json::from_str(args.params.get()).unwrap_or_default();
            self.notifications
                .borrow_mut()
                .push((args.method.to_string(), params));
            Ok(())
        }
    }

    fn recording_gateway() -> (GatewaySender, RecordedNotifications) {
        let notifications: RecordedNotifications = Rc::new(RefCell::new(Vec::new()));
        let (sender, receiver) = acp_gateway::<acp::AgentSide, _>(RecordingClient {
            notifications: notifications.clone(),
        });
        tokio::task::spawn_local(receiver.run());
        (sender, notifications)
    }

    async fn create_test_pty(gateway: GatewaySender) -> String {
        let env = HashMap::from([("ENV".to_string(), String::new())]);
        create_pty(
            Some("/bin/sh"),
            None,
            env,
            24,
            80,
            None,
            gateway,
            TargetClientId::None,
        )
        .await
        .expect("create test pty")
    }

    fn busy_event_types(notifications: &RecordedNotifications, pty_id: &str) -> Vec<String> {
        notifications
            .borrow()
            .iter()
            .filter(|(method, params)| {
                method == NOTIFICATION_METHOD && params["terminalId"] == pty_id
            })
            .filter_map(|(_, params)| match params["type"].as_str() {
                Some(t @ ("process_started" | "process_ended")) => Some(t.to_string()),
                _ => None,
            })
            .collect()
    }

    async fn wait_for_busy_events(
        notifications: &RecordedNotifications,
        pty_id: &str,
        expected: &[&str],
        deadline: Duration,
    ) -> Vec<String> {
        let started = tokio::time::Instant::now();
        loop {
            let events = busy_event_types(notifications, pty_id);
            if events == expected {
                return events;
            }
            if started.elapsed() > deadline {
                return events;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn long_running_command_emits_balanced_started_then_ended_pair() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (gateway, notifications) = recording_gateway();
                let pty_id = create_test_pty(gateway).await;

                write_pty_input(&pty_id, b"sleep 2\n")
                    .await
                    .expect("write command");

                let after_start = wait_for_busy_events(
                    &notifications,
                    &pty_id,
                    &["process_started"],
                    Duration::from_secs(10),
                )
                .await;
                assert_eq!(after_start, vec!["process_started"]);

                let after_end = wait_for_busy_events(
                    &notifications,
                    &pty_id,
                    &["process_started", "process_ended"],
                    Duration::from_secs(10),
                )
                .await;
                assert_eq!(after_end, vec!["process_started", "process_ended"]);

                close_pty(&pty_id).await.expect("close pty");
            })
            .await;
    }

    #[tokio::test]
    async fn idle_shell_emits_no_busy_notifications() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (gateway, notifications) = recording_gateway();
                let pty_id = create_test_pty(gateway).await;

                tokio::time::sleep(Duration::from_millis(BUSY_POLL_INTERVAL_MS * 3)).await;

                assert_eq!(
                    busy_event_types(&notifications, &pty_id),
                    Vec::<String>::new()
                );

                close_pty(&pty_id).await.expect("close pty");
            })
            .await;
    }
}
