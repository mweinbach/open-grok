//! Session bus host (listener + presence publisher) and client.
//!
//! [`SessionBusHost`] is `!Send` and lives on the `MvpAgent` LocalSet: it
//! owns the accept loop, the session table, and the presence heartbeat.
//! [`SessionBusClient`] is the `Send + Sync` handle injected into tool
//! resources; it performs read-only directory scans for listing and dials
//! peer sockets directly for delivery — including this process's own socket
//! (one uniform path, no in-process shortcut).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::SESSION_BUS_PIPE_PREFIX;
use super::presence::{self, LiveSession, PresenceFile, PresenceSession, now_ms, session_bus_dir};
use super::protocol::{
    self, AckStatus, ClientFrame, InboundPeerMessage, MAX_FRAME_LINE_BYTES, ServerFrame,
};

/// Presence rewrite cadence. Must be comfortably below
/// [`presence::STALE_TTL_MS`].
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// How often the heartbeat pass garbage-collects stale entries.
const GC_EVERY: u32 = 4;

/// Dial + ack round-trip budget for a single peer message.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Route an inbound peer message to a session hosted by this process.
/// Returns the ack verdict; `UnknownSession` when no live session matches.
pub type PeerRouter = Rc<dyn Fn(&InboundPeerMessage) -> AckStatus>;

/// Probe of this process's live sessions for the presence heartbeat:
/// returns `(session_id, busy)` per live session actor. Sessions absent
/// from the returned list are treated as exited and dropped from presence
/// (self-heal for actors that end without an explicit unregister).
pub type ActivityProbe = Rc<dyn Fn() -> Vec<(String, bool)>>;

/// Start the session bus for this process. `home` is the Open Grok home
/// (`$OPENGROK_HOME`); tests pass an isolated temp root. Fails only when
/// the bus directory or socket cannot be created — callers log a warning
/// and run bus-less (fail-open).
pub fn start_session_bus(home: &Path, router: PeerRouter) -> io::Result<SessionBusHost> {
    start_session_bus_with_probe(home, router, None)
}

/// [`start_session_bus`] with a live-session probe the heartbeat uses to
/// refresh busy/idle status and reap exited sessions from presence.
pub fn start_session_bus_with_probe(
    home: &Path,
    router: PeerRouter,
    probe: Option<ActivityProbe>,
) -> io::Result<SessionBusHost> {
    SessionBusHost::start(session_bus_dir(home), router, probe)
}

/// `!Send` host half; keep on the `MvpAgent` LocalSet. Dropping it cancels
/// the accept/heartbeat tasks and removes this instance's presence file and
/// socket; a crashed process leaves them behind for TTL garbage collection.
pub struct SessionBusHost {
    inner: Rc<RefCell<HostInner>>,
    client: SessionBusClient,
    shutdown: CancellationToken,
}

struct HostInner {
    dir: PathBuf,
    socket_path: PathBuf,
    instance: PresenceFile,
    sessions: HashMap<String, PresenceSession>,
}

impl SessionBusHost {
    fn start(dir: PathBuf, router: PeerRouter, probe: Option<ActivityProbe>) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let instance_id = presence::new_instance_id();
        let socket_path = dir.join(format!("{instance_id}.sock"));
        // Instance ids never repeat (pid + random suffix); clear any debris
        // anyway (defensive, best-effort).
        crate::local_ipc::remove_stale_socket(&socket_path);
        let listener = crate::local_ipc::bind(&socket_path, SESSION_BUS_PIPE_PREFIX)?;
        crate::local_ipc::restrict_socket_permissions(&socket_path);

        let now = now_ms();
        let inner = Rc::new(RefCell::new(HostInner {
            dir: dir.clone(),
            socket_path: socket_path.clone(),
            instance: PresenceFile {
                instance_id,
                pid: std::process::id(),
                socket_path: socket_path.to_string_lossy().into_owned(),
                protocol_version: presence::current_protocol_version(),
                heartbeat_at_ms: now,
                started_at_ms: now,
                sessions: Vec::new(),
            },
            sessions: HashMap::new(),
        }));

        let shutdown = CancellationToken::new();

        // Accept loop: one frame per connection, answer, close.
        {
            let router = router.clone();
            let cancel = shutdown.clone();
            tokio::task::spawn_local(async move {
                loop {
                    let conn = tokio::select! {
                        _ = cancel.cancelled() => break,
                        accepted = listener.accept() => match accepted {
                            Ok((stream, _)) => stream,
                            Err(e) => {
                                tracing::warn!(error = %e, "session-bus accept failed");
                                tokio::time::sleep(Duration::from_millis(200)).await;
                                continue;
                            }
                        },
                    };
                    let router = router.clone();
                    tokio::task::spawn_local(async move {
                        if let Err(e) = handle_connection(conn, router).await {
                            tracing::debug!(error = %e, "session-bus connection ended without ack");
                        }
                    });
                }
            });
        }

        // Heartbeat: refresh presence, periodically GC stale entries.
        {
            let inner = inner.clone();
            let cancel = shutdown.clone();
            tokio::task::spawn_local(async move {
                let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                let mut beat = 0u32;
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        _ = ticker.tick() => {}
                    }
                    beat = beat.wrapping_add(1);
                    if let Some(probe) = &probe {
                        refresh_from_probe(&inner, &probe());
                    }
                    rewrite_presence(&inner);
                    if beat % GC_EVERY == 0 {
                        let dir = inner.borrow().dir.clone();
                        presence::gc_stale(&dir, now_ms());
                    }
                }
            });
        }

        Ok(Self {
            inner,
            client: SessionBusClient { bus_dir: Some(dir) },
            shutdown,
        })
    }

    /// The `Send + Sync` handle for tool resources.
    pub fn client(&self) -> SessionBusClient {
        self.client.clone()
    }

    /// Announce a root session hosted by this process. Rewrites presence.
    pub fn register_session(&self, session: PresenceSession) {
        self.inner
            .borrow_mut()
            .sessions
            .insert(session.session_id.clone(), session);
        rewrite_presence(&self.inner);
    }

    /// Update a session's status (`"busy"` / `"idle"`). No-op when unknown.
    pub fn set_session_status(&self, session_id: &str, status: &str) {
        {
            let mut inner = self.inner.borrow_mut();
            let Some(entry) = inner.sessions.get_mut(session_id) else {
                return;
            };
            if entry.status == status {
                return;
            }
            entry.status = status.to_string();
            entry.updated_at_ms = now_ms();
        }
        rewrite_presence(&self.inner);
    }

    /// Update a session's model/title metadata. No-op when unknown or
    /// unchanged.
    pub fn update_session_meta(
        &self,
        session_id: &str,
        model_id: Option<String>,
        title: Option<String>,
    ) {
        {
            let mut inner = self.inner.borrow_mut();
            let Some(entry) = inner.sessions.get_mut(session_id) else {
                return;
            };
            if entry.model_id == model_id && entry.title == title {
                return;
            }
            entry.model_id = model_id;
            entry.title = title;
            entry.updated_at_ms = now_ms();
        }
        rewrite_presence(&self.inner);
    }

    /// Withdraw a session (close/evict). Removes the presence file when no
    /// sessions remain.
    pub fn unregister_session(&self, session_id: &str) {
        self.inner.borrow_mut().sessions.remove(session_id);
        rewrite_presence(&self.inner);
    }

    /// Stop the bus: cancel tasks and remove this instance's files.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
        let inner = self.inner.borrow();
        let _ = std::fs::remove_file(
            inner
                .dir
                .join(format!("{}.json", inner.instance.instance_id)),
        );
        let _ = std::fs::remove_file(&inner.socket_path);
    }
}

impl Drop for SessionBusHost {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Disk effect of a presence refresh, decided under one borrow and executed
/// after releasing it (no I/O under the RefCell borrow).
enum PresenceAction {
    Remove { file: PathBuf },
    Write { dir: PathBuf, file: PresenceFile },
}

/// Reconcile the hosted-session table against the activity probe: drop
/// sessions whose actor exited, refresh busy/idle status. Self-healing —
/// presence stays truthful even when no explicit unregister fired.
fn refresh_from_probe(inner: &Rc<RefCell<HostInner>>, live: &[(String, bool)]) {
    use std::collections::HashMap;
    let live: HashMap<&str, bool> = live.iter().map(|(id, b)| (id.as_str(), *b)).collect();
    let mut guard = inner.borrow_mut();
    let now = now_ms();
    guard.sessions.retain(|id, entry| {
        let Some(&busy) = live.get(id.as_str()) else {
            return false; // actor exited; drop from presence
        };
        let status = if busy { "busy" } else { "idle" };
        if entry.status != status {
            entry.status = status.to_string();
            entry.updated_at_ms = now;
        }
        true
    });
}

/// Publish the current table. The presence file exists only while at least
/// one session is hosted — a process with no sessions is invisible (and
/// unmessageable), which is the intended listing semantics.
fn rewrite_presence(inner: &Rc<RefCell<HostInner>>) {
    let action = {
        let mut guard = inner.borrow_mut();
        guard.instance.heartbeat_at_ms = now_ms();
        if guard.sessions.is_empty() {
            if guard.instance.sessions.is_empty() {
                return;
            }
            guard.instance.sessions = Vec::new();
            PresenceAction::Remove {
                file: guard
                    .dir
                    .join(format!("{}.json", guard.instance.instance_id)),
            }
        } else {
            let mut sessions: Vec<PresenceSession> = guard.sessions.values().cloned().collect();
            sessions.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            guard.instance.sessions = sessions;
            PresenceAction::Write {
                dir: guard.dir.clone(),
                file: guard.instance.clone(),
            }
        }
    };
    match action {
        PresenceAction::Remove { file } => {
            let _ = std::fs::remove_file(&file);
        }
        PresenceAction::Write { dir, file } => {
            if let Err(e) = presence::write_presence_atomic(&dir, &file) {
                tracing::warn!(error = %e, "session-bus presence write failed");
            }
        }
    }
}

/// Read one length-capped line, parse it as a client frame. `None` when the
/// peer closed without sending anything.
async fn read_client_frame(
    conn: &mut crate::local_ipc::LocalIpcStream,
) -> io::Result<Option<ClientFrame>> {
    let mut buf = read_line_capped(conn).await?;
    if buf.is_empty() {
        return Ok(None);
    }
    let frame =
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(frame))
}

/// Read bytes up to the first newline, enforcing the frame line cap.
/// Returns an empty vec on clean EOF before any byte.
async fn read_line_capped(conn: &mut crate::local_ipc::LocalIpcStream) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    loop {
        let n = conn.read(&mut chunk).await?;
        if n == 0 {
            return Ok(buf);
        }
        if let Some(pos) = chunk[..n].iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&chunk[..pos]);
            return Ok(buf);
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_FRAME_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session-bus frame exceeds line cap",
            ));
        }
    }
}

async fn handle_connection(
    mut conn: crate::local_ipc::LocalIpcStream,
    router: PeerRouter,
) -> io::Result<()> {
    let Some(frame) = read_client_frame(&mut conn).await? else {
        return Ok(());
    };
    let reply = match &frame {
        ClientFrame::Ping => ServerFrame::Pong,
        ClientFrame::Message { message_id, .. } => {
            let status = match protocol::validate_message(&frame) {
                Err(status) => status,
                Ok(()) => match frame.clone().into_message() {
                    Some(msg) => router(&msg),
                    None => AckStatus::Rejected,
                },
            };
            ServerFrame::Ack {
                message_id: message_id.clone(),
                status,
            }
        }
    };
    write_frame_line(&mut conn, &reply).await
}

async fn write_frame_line<T: serde::Serialize>(
    conn: &mut crate::local_ipc::LocalIpcStream,
    frame: &T,
) -> io::Result<()> {
    let mut line =
        serde_json::to_string(frame).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    conn.write_all(line.as_bytes()).await?;
    conn.flush().await
}

/// `Send + Sync` client for listing live sessions and delivering peer
/// messages. Cloned into tool resources.
#[derive(Clone, Debug)]
pub struct SessionBusClient {
    bus_dir: Option<PathBuf>,
}

/// Delivery outcome after a full dial + ack round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Accepted,
    UnknownSession,
    Rejected,
}

/// Errors that prevented learning an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    BusDisabled,
    BodyTooLarge,
    Io,
    Timeout,
}

impl SessionBusClient {
    /// A client for a bus-less process (bus disabled or failed to start).
    pub fn disabled() -> Self {
        Self { bus_dir: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.bus_dir.is_some()
    }

    /// All live sessions announced on the bus, freshest first.
    pub fn list_live_sessions(&self) -> Vec<LiveSession> {
        let Some(dir) = &self.bus_dir else {
            return Vec::new();
        };
        presence::live_sessions_in(dir, now_ms())
    }

    /// Deliver a peer message to a live session on any hosting process.
    pub async fn send_message(
        &self,
        target_session: &str,
        source_session: &str,
        source_project: &str,
        body: &str,
    ) -> Result<SendOutcome, SendError> {
        let Some(dir) = &self.bus_dir else {
            return Err(SendError::BusDisabled);
        };
        if body.len() > protocol::MAX_MESSAGE_BODY_BYTES {
            return Err(SendError::BodyTooLarge);
        }
        let target = presence::live_sessions_in(dir, now_ms())
            .into_iter()
            .find(|live| live.session.session_id == target_session);
        let Some(target) = target else {
            return Ok(SendOutcome::UnknownSession);
        };
        let socket_path = PathBuf::from(target.socket_path.clone());
        let frame = ClientFrame::Message {
            v: protocol::PROTOCOL_VERSION,
            message_id: uuid::Uuid::now_v7().to_string(),
            target_session: target_session.to_string(),
            source_session: source_session.to_string(),
            source_project: source_project.to_string(),
            body: body.to_string(),
        };
        match tokio::time::timeout(SEND_TIMEOUT, send_frame_and_read_ack(&socket_path, &frame))
            .await
        {
            Ok(Ok(ServerFrame::Ack { status, .. })) => match status {
                AckStatus::Accepted => Ok(SendOutcome::Accepted),
                AckStatus::UnknownSession => Ok(SendOutcome::UnknownSession),
                AckStatus::Rejected | AckStatus::InboxFull => Ok(SendOutcome::Rejected),
            },
            Ok(Ok(ServerFrame::Pong)) => Ok(SendOutcome::Rejected),
            Ok(Err(_)) => Err(SendError::Io),
            Err(_) => Err(SendError::Timeout),
        }
    }
}

async fn send_frame_and_read_ack(
    socket_path: &Path,
    frame: &ClientFrame,
) -> io::Result<ServerFrame> {
    let mut conn = crate::local_ipc::connect(socket_path, SESSION_BUS_PIPE_PREFIX).await?;
    write_frame_line(&mut conn, frame).await?;
    let buf = read_line_capped(&mut conn).await?;
    if buf.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "session-bus peer closed before ack",
        ));
    }
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn host_registers_lists_and_delivers() {
        let home = tempfile::TempDir::new().unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let delivered = Rc::new(RefCell::new(Vec::new()));
                let seen = delivered.clone();
                let router: PeerRouter = Rc::new(move |msg: &InboundPeerMessage| {
                    if msg.target_session == "s-1" {
                        seen.borrow_mut().push(msg.clone());
                        AckStatus::Accepted
                    } else {
                        AckStatus::UnknownSession
                    }
                });
                let host = start_session_bus(home.path(), router).unwrap();

                // Nothing listed before any session registers.
                assert!(host.client().list_live_sessions().is_empty());

                host.register_session(PresenceSession {
                    session_id: "s-1".into(),
                    cwd: "/repo".into(),
                    project_name: "repo".into(),
                    model_id: Some("grok-4".into()),
                    title: Some("Title".into()),
                    status: "idle".into(),
                    updated_at_ms: 1,
                });

                let live = host.client().list_live_sessions();
                assert_eq!(live.len(), 1);
                assert_eq!(live[0].session.session_id, "s-1");
                assert_eq!(live[0].pid, std::process::id());
                assert!(!live[0].conflict);

                // Deliver over the real socket (self-dial path).
                let outcome = host
                    .client()
                    .send_message("s-1", "s-other", "peer-repo", "hello there")
                    .await
                    .unwrap();
                assert_eq!(outcome, SendOutcome::Accepted);
                assert_eq!(delivered.borrow().len(), 1);
                assert_eq!(delivered.borrow()[0].body, "hello there");
                assert_eq!(delivered.borrow()[0].source_project, "peer-repo");

                // Unknown target surfaces as UnknownSession.
                let outcome = host
                    .client()
                    .send_message("nope", "s-other", "peer-repo", "hello")
                    .await
                    .unwrap();
                assert_eq!(outcome, SendOutcome::UnknownSession);

                host.shutdown();
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ping_pong_and_bad_version() {
        let home = tempfile::TempDir::new().unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let router: PeerRouter = Rc::new(|_| AckStatus::Accepted);
                let host = start_session_bus(home.path(), router).unwrap();
                let socket_path = host.inner.borrow().socket_path.clone();

                // Ping -> Pong.
                let mut conn = crate::local_ipc::connect(&socket_path, SESSION_BUS_PIPE_PREFIX)
                    .await
                    .unwrap();
                write_frame_line(&mut conn, &ClientFrame::Ping)
                    .await
                    .unwrap();
                let reply = read_ack_line(&mut conn).await.unwrap();
                assert_eq!(reply, ServerFrame::Pong);

                // Bad version -> Rejected ack.
                let bad = ClientFrame::Message {
                    v: 99,
                    message_id: "m".into(),
                    target_session: "s".into(),
                    source_session: "o".into(),
                    source_project: "p".into(),
                    body: "x".into(),
                };
                let mut conn2 = crate::local_ipc::connect(&socket_path, SESSION_BUS_PIPE_PREFIX)
                    .await
                    .unwrap();
                write_frame_line(&mut conn2, &bad).await.unwrap();
                let reply = read_ack_line(&mut conn2).await.unwrap();
                assert_eq!(
                    reply,
                    ServerFrame::Ack {
                        message_id: "m".into(),
                        status: AckStatus::Rejected
                    }
                );

                host.shutdown();
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unregister_removes_presence_and_disabled_client_is_inert() {
        let home = tempfile::TempDir::new().unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let router: PeerRouter = Rc::new(|_| AckStatus::Accepted);
                let host = start_session_bus(home.path(), router).unwrap();
                host.register_session(PresenceSession {
                    session_id: "s-1".into(),
                    cwd: "/repo".into(),
                    project_name: "repo".into(),
                    model_id: None,
                    title: None,
                    status: "idle".into(),
                    updated_at_ms: 1,
                });
                assert_eq!(host.client().list_live_sessions().len(), 1);
                host.unregister_session("s-1");
                assert!(host.client().list_live_sessions().is_empty());

                let disabled = SessionBusClient::disabled();
                assert!(!disabled.is_enabled());
                assert!(disabled.list_live_sessions().is_empty());
                assert_eq!(
                    disabled.send_message("x", "y", "p", "z").await.unwrap_err(),
                    SendError::BusDisabled
                );
            })
            .await;
    }

    async fn read_ack_line(conn: &mut crate::local_ipc::LocalIpcStream) -> io::Result<ServerFrame> {
        let buf = read_line_capped(conn).await?;
        if buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof"));
        }
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Two hosts in one bus directory stand in for two Open Grok processes
    /// on one machine: separate presence files, separate sockets, messages
    /// dialed peer-to-peer with no hub.
    #[tokio::test(flavor = "current_thread")]
    async fn two_hosts_exchange_messages_over_shared_bus_dir() {
        let home = tempfile::TempDir::new().unwrap();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let got_a = Rc::new(RefCell::new(Vec::new()));
                let seen_a = got_a.clone();
                let router_a: PeerRouter = Rc::new(move |msg: &InboundPeerMessage| {
                    if msg.target_session == "sess-a" {
                        seen_a.borrow_mut().push(msg.clone());
                        AckStatus::Accepted
                    } else {
                        AckStatus::UnknownSession
                    }
                });
                let got_b = Rc::new(RefCell::new(Vec::new()));
                let seen_b = got_b.clone();
                let router_b: PeerRouter = Rc::new(move |msg: &InboundPeerMessage| {
                    if msg.target_session == "sess-b" {
                        seen_b.borrow_mut().push(msg.clone());
                        AckStatus::Accepted
                    } else {
                        AckStatus::UnknownSession
                    }
                });
                let host_a = start_session_bus(home.path(), router_a).unwrap();
                let host_b = start_session_bus(home.path(), router_b).unwrap();

                host_a.register_session(test_session("sess-a", "idle"));
                host_b.register_session(test_session("sess-b", "busy"));

                // Each host sees both sessions (two presence files, one dir).
                let live = host_a.client().list_live_sessions();
                assert_eq!(live.len(), 2);
                assert!(
                    live.iter()
                        .any(|l| l.session.session_id == "sess-a" && l.session.status == "idle")
                );
                assert!(
                    live.iter()
                        .any(|l| l.session.session_id == "sess-b" && l.session.status == "busy")
                );

                // A dials B over B's socket.
                let out = host_a
                    .client()
                    .send_message("sess-b", "sess-a", "proj-a", "hello b")
                    .await
                    .unwrap();
                assert_eq!(out, SendOutcome::Accepted);
                assert_eq!(got_b.borrow().len(), 1);
                assert_eq!(got_b.borrow()[0].source_session, "sess-a");
                assert_eq!(got_b.borrow()[0].source_project, "proj-a");
                assert!(got_a.borrow().is_empty());

                // B replies over A's socket.
                let out = host_b
                    .client()
                    .send_message("sess-a", "sess-b", "proj-b", "hello a")
                    .await
                    .unwrap();
                assert_eq!(out, SendOutcome::Accepted);
                assert_eq!(got_a.borrow().len(), 1);
                assert_eq!(got_a.borrow()[0].body, "hello a");

                host_a.shutdown();
                host_b.shutdown();
            })
            .await;
    }

    fn test_session(id: &str, status: &str) -> PresenceSession {
        PresenceSession {
            session_id: id.into(),
            cwd: "/repo".into(),
            project_name: "repo".into(),
            model_id: None,
            title: None,
            status: status.into(),
            updated_at_ms: 1,
        }
    }

    #[test]
    fn refresh_from_probe_updates_status_and_reaps_exited_sessions() {
        let inner = Rc::new(RefCell::new(HostInner {
            dir: PathBuf::from("/does-not-matter"),
            socket_path: PathBuf::from("/does-not-matter/s.sock"),
            instance: PresenceFile {
                instance_id: "p1-abcdef01".into(),
                pid: 1,
                socket_path: String::new(),
                protocol_version: presence::current_protocol_version(),
                heartbeat_at_ms: 1,
                started_at_ms: 1,
                sessions: Vec::new(),
            },
            sessions: HashMap::from([
                ("s-1".to_string(), test_session("s-1", "idle")),
                ("s-2".to_string(), test_session("s-2", "busy")),
            ]),
        }));

        // Probe: s-1 now busy, s-2 idle, s-3 unknown (never hosted — ignored).
        refresh_from_probe(
            &inner,
            &[
                ("s-1".to_string(), true),
                ("s-2".to_string(), false),
                ("s-3".to_string(), true),
            ],
        );
        {
            let guard = inner.borrow();
            assert_eq!(guard.sessions["s-1"].status, "busy");
            assert_eq!(guard.sessions["s-2"].status, "idle");
            assert_eq!(guard.sessions.len(), 2);
        }

        // Probe: both actors exited -> both reaped from the table.
        refresh_from_probe(&inner, &[]);
        assert!(inner.borrow().sessions.is_empty());
    }
}
