//! Session bus host wiring for [`MvpAgent`]: starts the machine-local
//! presence/messaging bus during `initialize()`, announces root sessions on
//! it, and routes inbound peer messages into resident session actors.
//!
//! Delivery reuses the actor command channel exactly like the flat-team
//! mailbox (`deliver_root_followup`): look up the hosted handle, send
//! [`SessionCommand::PeerSessionMessage`], and let the recipient's run loop
//! decide between an interjection boundary (turn running) and an idle
//! wake-up. The router runs on the accept-loop task spawned by the host —
//! same `LocalSet`, so a `LocalRef` to the agent is safe.

use std::rc::Rc;

use agent_client_protocol as acp;

use super::MvpAgent;
use crate::session::SessionCommand;
use crate::session_bus::{AckStatus, PeerRouter, PeerSessionMessage, PresenceSession};

impl MvpAgent {
    /// Start the session bus once (idempotent, fail-open). Called from
    /// `initialize()` — before any session is created, so the client handle
    /// is live by the time tool resources are built.
    pub(super) fn start_session_bus(&self) {
        if self.session_bus_host.borrow().is_some() {
            return;
        }
        if !self.cfg.borrow().session_bus_enabled() {
            tracing::debug!("session bus disabled by config");
            return;
        }
        let agent_ref = super::LocalRef::new(self);
        let router: PeerRouter = Rc::new(move |msg: &PeerSessionMessage| {
            route_peer_message(agent_ref.get(), msg)
        });
        let probe = {
            let activity = self.activity.clone();
            Rc::new(move || activity.live_session_states())
        };
        let home = xai_grok_config::grok_home();
        match crate::session_bus::start_session_bus_with_probe(&home, router, Some(probe)) {
            Ok(host) => {
                *self.session_bus_client.borrow_mut() = host.client();
                *self.session_bus_host.borrow_mut() = Some(host);
                tracing::info!(?home, "session bus started");
            }
            Err(e) => {
                tracing::warn!(error = %e, "session bus failed to start; running bus-less");
            }
        }
    }

    /// Announce a root session on the bus. No-op when bus-less (disabled or
    /// failed startup). Subagent sessions are never announced — peers see
    /// the parent, which coordinates its own children.
    pub(super) fn register_session_on_bus(
        &self,
        session_id: &str,
        cwd: &str,
        model_id: Option<String>,
    ) {
        let host = self.session_bus_host.borrow();
        let Some(host) = host.as_ref() else {
            return;
        };
        host.register_session(PresenceSession {
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            project_name: crate::session_bus::project_name_from_cwd(cwd),
            model_id,
            title: None,
            status: "idle".to_string(),
            updated_at_ms: crate::session_bus::now_ms(),
        });
    }

    /// Withdraw a session from the bus (close/evict). No-op when bus-less;
    /// the heartbeat probe also reaps exited actors (self-heal).
    pub(super) fn unregister_session_from_bus(&self, session_id: &str) {
        let host = self.session_bus_host.borrow();
        let Some(host) = host.as_ref() else {
            return;
        };
        host.unregister_session(session_id);
    }
}

/// Deliver an inbound peer message to its target session actor.
fn route_peer_message(agent: &MvpAgent, msg: &PeerSessionMessage) -> AckStatus {
    let Some(handle) = agent.resident_handle(&acp::SessionId::new(msg.target_session.clone()))
    else {
        return AckStatus::UnknownSession;
    };
    let cmd = SessionCommand::PeerSessionMessage {
        message: msg.clone(),
    };
    match handle.cmd_tx.send(cmd) {
        Ok(()) => AckStatus::Accepted,
        // The actor exited between lookup and send; the channel closure is
        // the truthful "not hosted here" signal.
        Err(_) => AckStatus::UnknownSession,
    }
}
