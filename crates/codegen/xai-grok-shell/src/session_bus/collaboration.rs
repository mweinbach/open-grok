//! Shell-side backend for the cross-process session-collaboration tools.
//!
//! Bridges [`SessionCollaborationBackend`] onto the machine-local session
//! bus: listing reads the presence directory (any process), reading parses
//! the target session's persisted `updates.jsonl` from disk, and messaging
//! dials the owning process's socket. The same user's own machine-local
//! state is the trust boundary — peer bodies stay untrusted model-authored
//! input on the receiving side.

use xai_grok_tools::implementations::grok_build::session_collaboration::{
    ListSessionsOutput, LiveSessionEntry, MessageSessionStatus, ReadSessionOutput,
    SessionCollaborationBackend, TranscriptEntry,
};
use xai_tool_runtime::ToolError;

use super::host::{SendError, SendOutcome, SessionBusClient};

/// Per-session backend handle injected into the tool bridge.
pub struct ShellSessionCollaboration {
    client: SessionBusClient,
    self_session_id: String,
    self_project: String,
}

impl ShellSessionCollaboration {
    pub fn new(client: SessionBusClient, self_session_id: String, self_project: String) -> Self {
        Self {
            client,
            self_session_id,
            self_project,
        }
    }

    fn bus_disabled_error() -> ToolError {
        ToolError::custom(
            "session_bus_disabled",
            "The session bus is disabled for this process (config `[session_bus] enabled = false`)",
        )
    }
}

#[async_trait::async_trait]
impl SessionCollaborationBackend for ShellSessionCollaboration {
    async fn list_sessions(&self) -> Result<ListSessionsOutput, ToolError> {
        let is_self = |id: &str| id == self.self_session_id;
        let sessions = self
            .client
            .list_live_sessions()
            .into_iter()
            .map(|live| {
                let is_self = is_self(&live.session.session_id);
                LiveSessionEntry {
                    session_id: live.session.session_id,
                    cwd: live.session.cwd,
                    project_name: live.session.project_name,
                    model_id: live.session.model_id,
                    title: live.session.title,
                    status: live.session.status,
                    is_self,
                }
            })
            .collect();
        Ok(ListSessionsOutput {
            bus_enabled: self.client.is_enabled(),
            sessions,
        })
    }

    async fn read_session(
        &self,
        session_id: &str,
        max_updates: usize,
    ) -> Result<ReadSessionOutput, ToolError> {
        if !self.client.is_enabled() {
            return Err(Self::bus_disabled_error());
        }
        // The presence record's cwd locates the session directory.
        let live = self
            .client
            .list_live_sessions()
            .into_iter()
            .find(|l| l.session.session_id == session_id)
            .ok_or_else(|| {
                ToolError::custom(
                    "unknown_session",
                    "target session is not live on the session bus",
                )
            })?;
        let path = xai_grok_config::sessions_cwd_dir(&live.session.cwd)
            .join(session_id)
            .join("updates.jsonl");
        let contents = match tokio::fs::read_to_string(&path).await {
            Ok(contents) => contents,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                return Err(ToolError::custom(
                    "read_failed",
                    format!("failed to read session updates: {e}"),
                ));
            }
        };
        let updates = extract_transcript_entries(&contents, max_updates);
        Ok(ReadSessionOutput {
            session_id: session_id.to_string(),
            title: live.session.title,
            live: true,
            updates,
        })
    }

    async fn message_session(
        &self,
        target_session_id: &str,
        body: &str,
    ) -> Result<MessageSessionStatus, ToolError> {
        if !self.client.is_enabled() {
            return Err(Self::bus_disabled_error());
        }
        match self
            .client
            .send_message(
                target_session_id,
                &self.self_session_id,
                &self.self_project,
                body,
            )
            .await
        {
            Ok(SendOutcome::Accepted) => Ok(MessageSessionStatus::Accepted),
            Ok(SendOutcome::UnknownSession) => Ok(MessageSessionStatus::UnknownSession),
            Ok(SendOutcome::Rejected) => Ok(MessageSessionStatus::Rejected),
            Err(SendError::BusDisabled) => Err(Self::bus_disabled_error()),
            Err(SendError::BodyTooLarge) => Err(ToolError::invalid_arguments(
                "message exceeds the session-bus body cap",
            )),
            Err(SendError::Io) | Err(SendError::Timeout) => Err(ToolError::custom(
                "delivery_failed",
                "could not reach the session's hosting process",
            )),
        }
    }
}

/// Per-entry truncation for `read_session` output.
const MAX_ENTRY_CHARS: usize = 2_000;

/// Whole-output budget for `read_session` (tool cards stay bounded).
const MAX_TOTAL_CHARS: usize = 64 * 1024;

/// Extract user/agent/peer conversation entries from persisted
/// `updates.jsonl` content, newest last, capped to `max_entries`.
fn extract_transcript_entries(contents: &str, max_entries: usize) -> Vec<TranscriptEntry> {
    let mut collected: Vec<TranscriptEntry> = Vec::new();
    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue; // tolerate corrupt/foreign lines
        };
        let Some(update) = value.get("params").and_then(|p| p.get("update")) else {
            continue;
        };
        let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if let Some((role, text)) = extract_one(method, update) {
            collected.push(TranscriptEntry { role, text });
        }
    }
    let start = collected.len().saturating_sub(max_entries);
    let mut out = Vec::with_capacity(collected.len() - start);
    let mut total = 0usize;
    for entry in collected.drain(start..) {
        let mut text = entry.text;
        if text.chars().count() > MAX_ENTRY_CHARS {
            text = format!(
                "{}…",
                text.chars().take(MAX_ENTRY_CHARS).collect::<String>()
            );
        }
        total += text.chars().count();
        if total > MAX_TOTAL_CHARS {
            break;
        }
        out.push(TranscriptEntry {
            role: entry.role,
            text,
        });
    }
    out
}

/// One persisted update → `(role, text)` when it is a conversation entry.
fn extract_one(method: &str, update: &serde_json::Value) -> Option<(String, String)> {
    let kind = update.get("sessionUpdate").and_then(|k| k.as_str())?;
    let (role, text): (&str, &str) = match (method, kind) {
        ("session/update", "user_message_chunk") => {
            ("user", update.get("content")?.get("text")?.as_str()?)
        }
        ("session/update", "agent_message_chunk") => {
            ("agent", update.get("content")?.get("text")?.as_str()?)
        }
        // Peer-session messages persist as xAI extension updates.
        ("_x.ai/session/update", "peer_session_message") => ("peer", update.get("body")?.as_str()?),
        _ => return None,
    };
    Some((role.to_string(), text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(value: serde_json::Value) -> String {
        format!("{value}\n")
    }

    fn chunk_line(kind: &str, text: String) -> String {
        line(serde_json::json!({
            "timestamp": 1,
            "method": "session/update",
            "params": {"update": {
                "sessionUpdate": kind,
                "content": {"type": "text", "text": text},
            }},
        }))
    }

    #[test]
    fn extracts_user_agent_and_peer_entries_with_tail_cap() {
        let mut lines = String::new();
        for i in 0..5 {
            lines.push_str(&chunk_line("user_message_chunk", format!("u{i}")));
            lines.push_str(&chunk_line("agent_message_chunk", format!("a{i}")));
        }
        lines.push_str(&line(serde_json::json!({
            "timestamp": 2,
            "method": "_x.ai/session/update",
            "params": {"update": {
                "sessionUpdate": "peer_session_message",
                "message_id": "m",
                "from_session_id": "s-x",
                "body": "hello peer",
            }},
        })));
        // Noise that must be ignored.
        lines.push_str(&line(serde_json::json!({
            "timestamp": 2,
            "method": "session/update",
            "params": {"update": {"sessionUpdate": "tool_call", "toolCallId": "t1"}},
        })));
        lines.push_str("not json\n");

        // 11 conversation entries collected; the tail (newest last) is kept.
        let entries = extract_transcript_entries(&lines, 3);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            (entries[0].role.as_str(), entries[0].text.as_str()),
            ("user", "u4")
        );
        assert_eq!(
            (entries[1].role.as_str(), entries[1].text.as_str()),
            ("agent", "a4")
        );
        assert_eq!(
            (entries[2].role.as_str(), entries[2].text.as_str()),
            ("peer", "hello peer")
        );
    }

    #[test]
    fn truncates_long_entries() {
        let long = "x".repeat(MAX_ENTRY_CHARS + 50);
        let line = chunk_line("agent_message_chunk", long);
        let entries = extract_transcript_entries(&line, 10);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].text.chars().count() <= MAX_ENTRY_CHARS + 1);
        assert!(entries[0].text.ends_with('…'));
    }
}
