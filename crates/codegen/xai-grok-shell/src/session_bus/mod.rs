//! Session bus: machine-local discovery and peer messaging between live
//! Open Grok sessions.
//!
//! Every process hosting an [`crate::agent::mvp_agent::MvpAgent`] binds a
//! local IPC listener and publishes a presence file under
//! `$OPENGROK_HOME/session-bus/`. Other processes scan that directory to
//! discover live sessions and dial the owning process's socket directly to
//! deliver peer messages. There is no hub process and no daemon: the
//! presence directory is the shared rendezvous, so a crashed process only
//! leaves a stale file behind (garbage-collected by heartbeat age and PID
//! liveness).
//!
//! Recipient-side delivery mirrors the flat-team agent mailbox: a running
//! turn consumes the message at its next interjection boundary; an idle
//! session wakes with a synthetic prompt. Peer bodies are model-authored
//! untrusted input and never carry user-consent or permission semantics.
//!
//! v1 simplifications (documented invariants):
//! - One request per connection; no connection reuse or multiplexing.
//! - No per-source inbox cap on the receiving side beyond the shared body
//!   cap — peer messages are consumed at turn boundaries exactly like user
//!   interjections, which are likewise unbounded. `AckStatus::InboxFull` is
//!   reserved on the wire but not emitted in v1.
//! - Presence conflicts (the same session id registered by two processes,
//!   e.g. a session resumed twice) resolve to the freshest heartbeat; the
//!   listing marks them.

pub mod collaboration;
pub mod host;
pub mod presence;
pub mod protocol;

pub use host::{
    ActivityProbe, PeerRouter, SessionBusClient, SessionBusHost, start_session_bus,
    start_session_bus_with_probe,
};
pub use presence::{PresenceFile, PresenceSession, live_sessions_in, now_ms, session_bus_dir};
pub use protocol::{AckStatus, InboundPeerMessage as PeerSessionMessage};

/// Windows named-pipe namespace for session-bus sockets (ignored on Unix).
pub const SESSION_BUS_PIPE_PREFIX: &str = "grok-sbus-";

/// Human-facing project label for presence and tool output: the cwd's
/// final component, or the whole trimmed cwd when it is a root (`/`, `C:\`).
pub fn project_name_from_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches('/');
    trimmed
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}
