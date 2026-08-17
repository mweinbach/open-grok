//! JSON-lines wire protocol for the session bus.
//!
//! One client frame per connection in v1: the sender dials, writes a single
//! newline-terminated JSON frame, reads a single server frame, and closes.

use serde::{Deserialize, Serialize};

/// Current protocol version. Mismatched versions are rejected with
/// [`AckStatus::Rejected`] (no leader-style eviction needed).
pub const PROTOCOL_VERSION: u32 = 1;

/// Hard cap on a message body, matching the flat-team agent mailbox
/// (`MAX_AGENT_MESSAGE_BYTES`).
pub const MAX_MESSAGE_BODY_BYTES: usize = 32 * 1024;

/// Hard cap on a full wire line (body + envelope overhead slack).
pub const MAX_FRAME_LINE_BYTES: usize = MAX_MESSAGE_BODY_BYTES + 32 * 1024;

/// Frame sent by the peer dialing this process's socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Deliver a peer message to a session hosted by this process.
    Message {
        /// Sender's protocol version; must equal [`PROTOCOL_VERSION`].
        v: u32,
        message_id: String,
        target_session: String,
        source_session: String,
        /// Human-readable project name of the sender's workspace (display
        /// provenance only — never trusted for access decisions).
        source_project: String,
        body: String,
    },
    Ping,
}

/// Frame sent back by the hosting process.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Ack {
        message_id: String,
        status: AckStatus,
    },
    Pong,
}

/// Delivery verdict for a peer message. `accepted` means the target process
/// queued the message for the target session — not that the model has read
/// it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AckStatus {
    /// Routed to the target session's queue (interjection or wake-up).
    Accepted,
    /// No live session with that id is hosted by this process.
    UnknownSession,
    /// Rejected (version mismatch, oversized body, malformed frame).
    Rejected,
    /// Reserved for per-source inbox caps; not emitted in v1.
    InboxFull,
}

/// Validate an inbound message frame before routing. Pings carry no
/// message payload to validate.
pub fn validate_message(frame: &ClientFrame) -> Result<(), AckStatus> {
    let ClientFrame::Message {
        v,
        body,
        target_session,
        source_session,
        ..
    } = frame
    else {
        return Ok(());
    };

    if *v != PROTOCOL_VERSION {
        return Err(AckStatus::Rejected);
    }
    if body.len() > MAX_MESSAGE_BODY_BYTES {
        return Err(AckStatus::Rejected);
    }
    if target_session.trim().is_empty() || source_session.trim().is_empty() {
        return Err(AckStatus::Rejected);
    }
    Ok(())
}

/// A message validated and routed onward to the hosting process's session
/// registry.
#[derive(Debug, Clone)]
pub struct InboundPeerMessage {
    pub message_id: String,
    pub target_session: String,
    pub source_session: String,
    pub source_project: String,
    pub body: String,
}

impl ClientFrame {
    /// Extract the message payload if this frame is a [`ClientFrame::Message`].
    pub fn into_message(self) -> Option<InboundPeerMessage> {
        match self {
            ClientFrame::Message {
                message_id,
                target_session,
                source_session,
                source_project,
                body,
                ..
            } => Some(InboundPeerMessage {
                message_id,
                target_session,
                source_session,
                source_project,
                body,
            }),
            ClientFrame::Ping => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_message() -> ClientFrame {
        ClientFrame::Message {
            v: PROTOCOL_VERSION,
            message_id: "m1".into(),
            target_session: "target".into(),
            source_session: "source".into(),
            source_project: "open-grok".into(),
            body: "hello".into(),
        }
    }

    #[test]
    fn client_frame_roundtrips_through_json() {
        let frame = sample_message();
        let line = serde_json::to_string(&frame).unwrap();
        let parsed: ClientFrame = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, frame);
        // Wire tag is snake_case.
        assert!(line.contains(r#""type":"message""#));
    }

    #[test]
    fn server_frame_roundtrips_through_json() {
        let frame = ServerFrame::Ack {
            message_id: "m1".into(),
            status: AckStatus::Accepted,
        };
        let line = serde_json::to_string(&frame).unwrap();
        let parsed: ServerFrame = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, frame);
        assert!(line.contains(r#""status":"accepted""#));
    }

    #[test]
    fn unknown_frame_tags_fail_to_parse() {
        assert!(serde_json::from_str::<ClientFrame>(r#"{"type":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ServerFrame>(r#"{"type":"nope"}"#).is_err());
    }

    #[test]
    fn validate_rejects_version_mismatch() {
        let mut frame = sample_message();
        if let ClientFrame::Message { v, .. } = &mut frame {
            *v = PROTOCOL_VERSION + 1;
        }
        assert_eq!(validate_message(&frame), Err(AckStatus::Rejected));
    }

    #[test]
    fn validate_rejects_oversized_body() {
        let mut frame = sample_message();
        if let ClientFrame::Message { body, .. } = &mut frame {
            *body = "x".repeat(MAX_MESSAGE_BODY_BYTES + 1);
        }
        assert_eq!(validate_message(&frame), Err(AckStatus::Rejected));
    }

    #[test]
    fn validate_rejects_empty_ids() {
        let mut frame = sample_message();
        if let ClientFrame::Message { target_session, .. } = &mut frame {
            *target_session = "  ".into();
        }
        assert_eq!(validate_message(&frame), Err(AckStatus::Rejected));
    }

    #[test]
    fn validate_accepts_well_formed_message() {
        assert_eq!(validate_message(&sample_message()), Ok(()));
    }
}
