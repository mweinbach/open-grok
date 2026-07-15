//! Leader-mode (`grok agent --leader stdio`) test harness.
//!
//! Spawns the real binary as a stdio client whose bridge elects a leader
//! subprocess hosting the actual sessions, speaks ACP over pipes, and
//! exposes lock-file helpers for leader-lifecycle assertions. Unix-only:
//! the leader transport is a unix socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use agent_client_protocol::{self as acp, Agent as _};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use xai_acp_lib::LineBufferedRead;

use crate::env::grok_binary;
use crate::mock_server::MockInferenceServer;
use crate::process::spawn_piped_with_stderr_capture;

/// Env var naming the binary that elects/hosts the leader in a two-binary
/// (version-skew) test. Falls back to [`grok_binary`]'s resolution.
pub const LEADER_BINARY_ENV: &str = "GROK_BINARY_LEADER";

/// Env var naming the binary for the second (usually newer) client in a
/// two-binary test. Falls back to [`grok_binary`]'s resolution.
pub const CLIENT_BINARY_ENV: &str = "GROK_BINARY_CLIENT";

fn role_binary(env_key: &str) -> PathBuf {
    if let Ok(path) = std::env::var(env_key) {
        let p = PathBuf::from(path);
        assert!(p.exists(), "{env_key} does not exist: {}", p.display());
        return p;
    }
    grok_binary()
}

/// Binary for the leader-electing side of a version-skew test
/// (`GROK_BINARY_LEADER`, else the shared [`grok_binary`] resolution).
pub fn leader_binary() -> PathBuf {
    role_binary(LEADER_BINARY_ENV)
}

/// Binary for the client side of a version-skew test (`GROK_BINARY_CLIENT`,
/// else the shared [`grok_binary`] resolution).
pub fn client_binary() -> PathBuf {
    role_binary(CLIENT_BINARY_ENV)
}

/// Capture for notifications + reconnect signals.
#[derive(Default)]
pub struct Capture {
    chunks: std::sync::Mutex<Vec<String>>,
    notification_count: AtomicU32,
    reconnected_count: AtomicU32,
}

struct LeaderAcpClient {
    capture: Arc<Capture>,
}

#[async_trait::async_trait(?Send)]
impl acp::Client for LeaderAcpClient {
    async fn request_permission(
        &self,
        args: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let outcome = args
            .options
            .iter()
            .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
            .or(args.options.first())
            .map(|o| {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    o.option_id.clone(),
                ))
            })
            .unwrap_or(acp::RequestPermissionOutcome::Cancelled);
        Ok(acp::RequestPermissionResponse::new(outcome))
    }

    async fn session_notification(&self, args: acp::SessionNotification) -> acp::Result<()> {
        self.capture
            .notification_count
            .fetch_add(1, Ordering::SeqCst);
        if let acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk { content, .. }) =
            args.update
            && let acp::ContentBlock::Text(t) = content
        {
            self.capture.chunks.lock().unwrap().push(t.text);
        }
        Ok(())
    }

    async fn ext_notification(&self, args: acp::ExtNotification) -> acp::Result<()> {
        if &*args.method == "x.ai/leader_reconnected" {
            self.capture
                .reconnected_count
                .fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// A `grok agent --leader stdio` client subprocess speaking ACP over pipes.
/// The leader subprocess it elects hosts the actual sessions.
pub struct LeaderStdioClient {
    pub conn: acp::ClientSideConnection,
    // Exposed for PID assertions.
    pub child: tokio::process::Child,
    capture: Arc<Capture>,
    stderr: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl LeaderStdioClient {
    pub async fn spawn(server: &MockInferenceServer, cwd: &Path, home: &Path) -> Self {
        Self::spawn_with_binary(&grok_binary(), server, cwd, home).await
    }

    /// [`Self::spawn`] with an explicit binary, for two-binary version-skew
    /// tests (pair with [`leader_binary`] / [`client_binary`]).
    pub async fn spawn_with_binary(
        binary: &Path,
        server: &MockInferenceServer,
        cwd: &Path,
        home: &Path,
    ) -> Self {
        let mut cmd = tokio::process::Command::new(binary);
        cmd.args(["agent", "--leader", "stdio"])
            .current_dir(cwd)
            // Hermetic env: the developer's shell may export GROK_* vars
            // (e.g. GROK_LEADER_SOCKET pointing at a REAL leader on this
            // machine). env_clear + explicit allowlist guarantees the test
            // can never touch a leader outside its sandbox home.
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("GROK_HOME", home.join(".grok"))
            // Pin the socket inside the sandbox. The lock file is the
            // sibling `.lock` (leader.sock -> leader.lock), and the spawned
            // leader subprocess inherits/forwards this env var, so every
            // (re-)elected leader binds the same sandboxed path.
            .env("GROK_LEADER_SOCKET", home.join(".grok").join("leader.sock"))
            .env("GROK_CLI_CHAT_PROXY_BASE_URL", server.url())
            .env("GROK_XAI_API_BASE_URL", server.url())
            .env("XAI_API_KEY", "test-key-for-ci")
            .env("GROK_TELEMETRY_ENABLED", "false")
            .env("GROK_FEEDBACK_ENABLED", "false")
            .env("GROK_TRACE_UPLOAD", "false")
            .env("GROK_INSTRUMENTATION", "disabled")
            // Inherited by the spawned leader, whose stderr goes to
            // ~/.grok/leader.log — keep it chatty for diagnosis.
            .env("RUST_LOG", "xai_grok_shell=debug");

        let (mut child, stderr) = spawn_piped_with_stderr_capture(cmd);

        let outgoing = child.stdin.take().unwrap().compat_write();
        let incoming = child.stdout.take().unwrap().compat();

        let capture = Arc::new(Capture::default());
        let client = LeaderAcpClient {
            capture: capture.clone(),
        };
        let incoming = LineBufferedRead::spawn_local(incoming);
        let (conn, handle_io) = acp::ClientSideConnection::new(client, outgoing, incoming, |fut| {
            tokio::task::spawn_local(fut);
        });
        tokio::task::spawn_local(handle_io);

        Self {
            conn,
            child,
            capture,
            stderr,
        }
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }

    pub fn captured_text(&self) -> String {
        self.capture.chunks.lock().unwrap().join("")
    }

    pub async fn initialize(&self) -> acp::InitializeResponse {
        let init = tokio::time::timeout(
            Duration::from_secs(60),
            self.conn.initialize(
                acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                    .client_capabilities(
                        acp::ClientCapabilities::new()
                            .fs(acp::FileSystemCapabilities::new())
                            .terminal(false),
                    )
                    .meta(
                        serde_json::json!({
                            "startupHints": {
                                "nonInteractive": true,
                                "skipGitStatus": true,
                                "skipProjectLayout": true
                            },
                            "clientType": "test-client",
                            "clientVersion": "0.0.0-test"
                        })
                        .as_object()
                        .cloned(),
                    ),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("initialize timed out\nstderr:\n{}", self.stderr_text()))
        .expect("initialize failed");

        let api_key_method = init
            .auth_methods
            .iter()
            .find(|m| &*m.id().0 == "xai.api_key")
            .expect("xai.api_key auth method");
        self.conn
            .authenticate(
                acp::AuthenticateRequest::new(api_key_method.id().clone())
                    .meta(serde_json::json!({"headless": true}).as_object().cloned()),
            )
            .await
            .expect("authenticate failed");
        init
    }

    pub async fn create_session(&self, cwd: &Path) -> acp::SessionId {
        self.create_session_inner(cwd, None).await
    }

    pub async fn create_session_with_model(&self, cwd: &Path, model_id: &str) -> acp::SessionId {
        self.create_session_inner(
            cwd,
            serde_json::json!({ "modelId": model_id })
                .as_object()
                .cloned(),
        )
        .await
    }

    async fn create_session_inner(&self, cwd: &Path, meta: Option<acp::Meta>) -> acp::SessionId {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.conn.new_session(
                acp::NewSessionRequest::new(cwd.to_path_buf())
                    .mcp_servers(vec![])
                    .meta(meta),
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("session/new timed out\nstderr:\n{}", self.stderr_text()))
        .expect("session/new failed")
        .session_id
    }

    pub async fn prompt(
        &self,
        session_id: &acp::SessionId,
        text: &str,
    ) -> acp::Result<acp::PromptResponse> {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.conn.prompt(acp::PromptRequest::new(
                session_id.clone(),
                vec![acp::ContentBlock::Text(acp::TextContent::new(
                    text.to_string(),
                ))],
            )),
        )
        .await
        .unwrap_or_else(|_| panic!("prompt timed out\nstderr:\n{}", self.stderr_text()))
    }

    pub fn reconnected_count(&self) -> u32 {
        self.capture.reconnected_count.load(Ordering::SeqCst)
    }

    pub fn notification_count(&self) -> u32 {
        self.capture.notification_count.load(Ordering::SeqCst)
    }
}

pub fn leader_lock_path(home: &Path) -> PathBuf {
    home.join(".grok").join("leader.lock")
}

pub fn read_leader_pid(home: &Path) -> Option<u32> {
    std::fs::read_to_string(leader_lock_path(home))
        .ok()?
        .trim()
        .parse()
        .ok()
}

pub fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Wait until the leader lock file contains a live PID, return it.
pub async fn wait_for_live_leader(home: &Path, timeout: Duration) -> Option<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Some(pid) = read_leader_pid(home)
            && pid_alive(pid)
        {
            return Some(pid);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Wait until the leader lock file contains a live PID *different* from `old_pid`.
pub async fn wait_for_new_leader(home: &Path, old_pid: u32, timeout: Duration) -> Option<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if let Some(pid) = read_leader_pid(home)
            && pid != old_pid
            && pid_alive(pid)
        {
            return Some(pid);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Wait for evidence that the bridge finished its reconnect replay.
///
/// The `x.ai/leader_reconnected` ext notification is dropped by the typed
/// `ClientSideConnection` (bare `x.ai/*` methods are rejected by the ACP
/// decoder), so we wait for the replayed `session/load` to emit session
/// notifications instead: the notification count rises above `baseline`.
pub async fn wait_for_replay_notifications(
    client: &LeaderStdioClient,
    baseline: u32,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if client.reconnected_count() > 0 || client.notification_count() > baseline {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

pub fn leader_log(home: &Path) -> String {
    std::fs::read_to_string(home.join(".grok").join("leader.log")).unwrap_or_default()
}
