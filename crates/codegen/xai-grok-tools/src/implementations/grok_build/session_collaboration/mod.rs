//! Cross-process session collaboration tools (machine-local session bus).
//!
//! These face the model the same way the flat-team mailbox tools do, but the
//! peers are other live Open Grok sessions — other terminals, other
//! projects, other processes on the same machine — discovered through the
//! shell's session bus (presence directory + per-process IPC sockets).
//!
//! The tools are thin: all reachability lives in the
//! [`SessionCollaborationBackend`] the shell installs per session via
//! [`SessionBusResource`]. A backend is installed even when the bus is
//! disabled, so the tools can explain that instead of vanishing.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{ToolMetadata, shared_resources};

/// Mirrors the session-bus wire body cap: peer messages are untrusted
/// model-authored input, kept small by construction.
const MAX_SESSION_MESSAGE_BYTES: usize = 32 * 1024;

/// Default number of persisted conversation entries `read_session` returns.
const DEFAULT_MAX_UPDATES: usize = 30;

/// Hard cap on `read_session` entries (bounded output for tool cards).
const MAX_READ_UPDATES: usize = 200;

/// One live Open Grok session on the machine-local bus.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiveSessionEntry {
    /// Stable session id; use verbatim for `read_session` / `message_session`.
    pub session_id: String,
    /// Working directory of that session.
    pub cwd: String,
    /// Human-facing project label (final cwd component).
    pub project_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// `"busy"` (turn running or interaction parked) or `"idle"`.
    pub status: String,
    /// True when this entry is the calling session itself.
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ListSessionsOutput {
    /// False when the session bus is disabled for this process — the list
    /// is then empty and messaging is unavailable.
    pub bus_enabled: bool,
    pub sessions: Vec<LiveSessionEntry>,
}

/// One persisted conversation entry, newest last.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TranscriptEntry {
    /// `"user"`, `"agent"`, or `"peer"` (message from another session).
    pub role: String,
    /// Extracted text of the update, already truncated to a per-entry cap.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReadSessionOutput {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the target is still live on the bus at read time.
    pub live: bool,
    pub updates: Vec<TranscriptEntry>,
}

/// Delivery verdict for [`MessageSessionTool`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MessageSessionStatus {
    /// Handed to the recipient process (queued into a running turn or woke
    /// an idle session).
    Accepted,
    /// No live session with that id.
    UnknownSession,
    /// The recipient process refused the message.
    Rejected,
}

/// Shell-provided reachability behind the tools. Implemented in the shell
/// crate over its session-bus client and session storage.
#[async_trait::async_trait]
pub trait SessionCollaborationBackend: Send + Sync + 'static {
    /// Live sessions across all Open Grok processes on this machine. The
    /// backend fills `bus_enabled` so a bus-less process can explain
    /// itself rather than look peerless.
    async fn list_sessions(&self) -> Result<ListSessionsOutput, xai_tool_runtime::ToolError>;

    /// Recent persisted conversation entries for a session id that is live
    /// on the bus (the presence record's cwd locates its session directory).
    async fn read_session(
        &self,
        session_id: &str,
        max_updates: usize,
    ) -> Result<ReadSessionOutput, xai_tool_runtime::ToolError>;

    /// Deliver a peer message from this session to a live session.
    async fn message_session(
        &self,
        target_session_id: &str,
        body: &str,
    ) -> Result<MessageSessionStatus, xai_tool_runtime::ToolError>;
}

/// Per-session resource the shell injects into the tool bridge.
#[derive(Clone)]
pub struct SessionBusResource {
    backend: Arc<dyn SessionCollaborationBackend>,
    self_session_id: String,
    self_project: String,
}

impl SessionBusResource {
    pub fn new(
        backend: Arc<dyn SessionCollaborationBackend>,
        self_session_id: String,
        self_project: String,
    ) -> Self {
        Self {
            backend,
            self_session_id,
            self_project,
        }
    }

    pub fn backend(&self) -> &dyn SessionCollaborationBackend {
        self.backend.as_ref()
    }

    pub fn self_session_id(&self) -> &str {
        &self.self_session_id
    }

    pub fn self_project(&self) -> &str {
        &self.self_project
    }
}

impl std::fmt::Debug for SessionBusResource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionBusResource")
            .field("self_session_id", &self.self_session_id)
            .field("self_project", &self.self_project)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Default)]
pub struct ListSessionsTool;

#[derive(Debug, Default)]
pub struct ReadSessionTool;

#[derive(Debug, Default)]
pub struct MessageSessionTool;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListSessionsInput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadSessionInput {
    #[schemars(description = "Session id from list_sessions.")]
    pub session_id: String,
    #[schemars(
        description = "Maximum conversation entries to return (newest last). Omit for 30; hard cap 200."
    )]
    #[serde(default)]
    pub max_updates: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageSessionInput {
    #[schemars(description = "Target session id from list_sessions.")]
    pub session_id: String,
    #[schemars(description = "Message body for the recipient session's model.")]
    pub message: String,
}

async fn bus_resource(
    ctx: &xai_tool_runtime::ToolCallContext,
) -> Result<SessionBusResource, xai_tool_runtime::ToolError> {
    let resources = shared_resources(ctx)?;
    let resources = resources.lock().await;
    resources
        .get::<SessionBusResource>()
        .cloned()
        .ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "missing_resource",
                "Session collaboration is not initialized for this session",
            )
        })
}

macro_rules! session_tool_metadata {
    ($tool:ty, $description:literal, $read_only:literal) => {
        impl ToolMetadata for $tool {
            fn kind(&self) -> ToolKind {
                ToolKind::AgentCollaboration
            }

            fn tool_namespace(&self) -> ToolNamespace {
                ToolNamespace::GrokBuild
            }

            fn description_template(&self) -> &str {
                $description
            }

            fn is_read_only(&self) -> bool {
                $read_only
            }
        }
    };
}

session_tool_metadata!(
    ListSessionsTool,
    "List the Open Grok sessions live on this machine's session bus — across every project, terminal, and process, including this one. Each entry carries a session id, project, model, title, and busy/idle status. Session ids are the addressing unit: use them verbatim with read_session and message_session. The roster changes as sessions open and close, so call this again when an id stops working.",
    true
);
session_tool_metadata!(
    ReadSessionTool,
    "Read the recent conversation of another live Open Grok session from its persisted history — the last user and agent messages, newest last. Use it to understand what another session is doing before deciding to message it. The target must be live on the session bus (it appears in list_sessions).",
    true
);
session_tool_metadata!(
    MessageSessionTool,
    "Send a message to another live Open Grok session on this machine. A recipient mid-turn receives it at its next turn boundary; an idle recipient wakes with it as a prompt. The recipient model decides what to do and can reply through message_session addressed to this session's id. Keep messages concise and self-contained, and never resend an accepted message.",
    false
);

impl xai_tool_runtime::Tool for ListSessionsTool {
    type Args = ListSessionsInput;
    type Output = ListSessionsOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("list_sessions").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "list_sessions",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        _input: ListSessionsInput,
    ) -> Result<ListSessionsOutput, xai_tool_runtime::ToolError> {
        let resource = bus_resource(&ctx).await?;
        resource.backend().list_sessions().await
    }
}

impl xai_tool_runtime::Tool for ReadSessionTool {
    type Args = ReadSessionInput;
    type Output = ReadSessionOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("read_session").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "read_session",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: ReadSessionInput,
    ) -> Result<ReadSessionOutput, xai_tool_runtime::ToolError> {
        let session_id = input.session_id.trim();
        if session_id.is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "session_id must not be empty",
            ));
        }
        let max_updates = input
            .max_updates
            .unwrap_or(DEFAULT_MAX_UPDATES)
            .clamp(1, MAX_READ_UPDATES);
        let resource = bus_resource(&ctx).await?;
        resource
            .backend()
            .read_session(session_id, max_updates)
            .await
    }
}

impl xai_tool_runtime::Tool for MessageSessionTool {
    type Args = MessageSessionInput;
    type Output = MessageSessionOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("message_session").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "message_session",
            ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: MessageSessionInput,
    ) -> Result<MessageSessionOutput, xai_tool_runtime::ToolError> {
        let target = input.session_id.trim();
        if target.is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "session_id must not be empty",
            ));
        }
        let message = input.message.trim();
        if message.is_empty() {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "message must not be empty",
            ));
        }
        if message.len() > MAX_SESSION_MESSAGE_BYTES {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "message exceeds the {MAX_SESSION_MESSAGE_BYTES}-byte limit"
            )));
        }
        let resource = bus_resource(&ctx).await?;
        let status = resource.backend().message_session(target, message).await?;
        Ok(MessageSessionOutput {
            target_session_id: target.to_string(),
            status,
        })
    }
}

/// Output envelope for [`MessageSessionTool`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MessageSessionOutput {
    pub target_session_id: String,
    pub status: MessageSessionStatus,
}

#[cfg(test)]
mod tests {
    use xai_tool_runtime::Tool;

    use super::*;

    #[test]
    fn session_tool_ids_and_kind_are_stable() {
        for (id, actual) in [
            ("list_sessions", Tool::id(&ListSessionsTool)),
            ("read_session", Tool::id(&ReadSessionTool)),
            ("message_session", Tool::id(&MessageSessionTool)),
        ] {
            assert_eq!(actual.as_str(), id);
        }
        assert_eq!(
            ToolMetadata::kind(&MessageSessionTool),
            ToolKind::AgentCollaboration
        );
    }

    #[test]
    fn message_status_serializes_snake_case() {
        let out = MessageSessionOutput {
            target_session_id: "s-1".into(),
            status: MessageSessionStatus::UnknownSession,
        };
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(json["status"], "unknown_session");
    }
}
