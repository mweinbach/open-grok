//! Team-scoped subagent discovery and mailbox tools.

use std::time::{SystemTime, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::implementations::grok_build::task::backend::SubagentBackendResource;
use crate::implementations::grok_build::task::types::{
    AgentMailboxIdentity, AgentMailboxMessage, AgentMailboxMessageKind, AgentMessageSendOutput,
    ListAgentsOutput, WaitAgentMessagesOutput,
};
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{ToolMetadata, shared_resources};

const MAX_AGENT_MESSAGE_BYTES: usize = 32 * 1024;
const DEFAULT_WAIT_MS: u64 = 30_000;
const MAX_WAIT_MS: u64 = 600_000;

#[derive(Debug, Default)]
pub struct ListAgentsTool;

#[derive(Debug, Default)]
pub struct SendAgentMessageTool;

#[derive(Debug, Default)]
pub struct FollowupAgentTaskTool;

#[derive(Debug, Default)]
pub struct WaitAgentTool;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListAgentsInput {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendAgentMessageInput {
    #[schemars(description = "Agent ID from list_agents, or \"root\" for the team root.")]
    pub target: String,
    #[schemars(description = "Message text to queue for the target agent.")]
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitAgentInput {
    #[schemars(
        description = "Maximum wait in milliseconds. Omit for 30 seconds; pass 0 for a non-blocking inbox poll."
    )]
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

async fn collaboration_resources_async(
    ctx: &xai_tool_runtime::ToolCallContext,
) -> Result<(SubagentBackendResource, AgentMailboxIdentity), xai_tool_runtime::ToolError> {
    let resources = shared_resources(ctx)?;
    let resources = resources.lock().await;
    let backend = resources
        .get::<SubagentBackendResource>()
        .cloned()
        .ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "missing_resource",
                "Subagent mailbox is not initialized for this session",
            )
        })?;
    let identity = resources
        .get::<AgentMailboxIdentity>()
        .cloned()
        .ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "missing_resource",
                "Agent mailbox identity is not initialized for this session",
            )
        })?;
    Ok((backend, identity))
}

fn validate_message(
    input: SendAgentMessageInput,
) -> Result<(String, String), xai_tool_runtime::ToolError> {
    let target = input.target.trim();
    if target.is_empty() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "target must not be empty",
        ));
    }
    let message = input.message.trim();
    if message.is_empty() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "message must not be empty",
        ));
    }
    if message.len() > MAX_AGENT_MESSAGE_BYTES {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "message exceeds the {MAX_AGENT_MESSAGE_BYTES}-byte limit"
        )));
    }
    Ok((target.to_string(), message.to_string()))
}

fn stamped_message(
    identity: &AgentMailboxIdentity,
    target: String,
    body: String,
    kind: AgentMailboxMessageKind,
) -> AgentMailboxMessage {
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    AgentMailboxMessage {
        message_id: uuid::Uuid::now_v7().to_string(),
        team_scope_id: identity.team_scope_id.clone(),
        from_agent_id: identity.agent_id.clone(),
        to_agent_id: target,
        kind,
        body,
        created_at_ms,
        native: None,
    }
}

macro_rules! collaboration_metadata {
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

collaboration_metadata!(
    ListAgentsTool,
    "List the root and subagents in this session's collaboration team. Returns stable agent IDs, lifecycle status, task labels, resume provenance, and worktree paths. It does not expose agent transcripts. The roster only changes when agents spawn or finish, so call this once and reuse the result.",
    true
);
collaboration_metadata!(
    SendAgentMessageTool,
    "Send a steering message to another live agent. Use list_agents to discover exact target IDs. A running recipient receives it promptly mid-turn; an idle recipient is woken with it; a recipient that has not started yet receives it as soon as it starts. Send each message exactly once — a queued status means it will still be delivered, so never resend it.",
    false
);
collaboration_metadata!(
    FollowupAgentTaskTool,
    "Queue a follow-up task in another live agent's mailbox without interrupting its current work. The recipient reads queued tasks with wait_agent when it reaches a stopping point. Use send_message instead when the recipient must react immediately.",
    false
);
collaboration_metadata!(
    WaitAgentTool,
    "Read this agent's queued mailbox messages, such as follow-up tasks, waiting for activity when requested. Only messages addressed to the calling agent are returned. Steering messages from peers are delivered to you automatically, so call this to pick up queued follow-up work or to deliberately wait for mail — do not poll it speculatively.",
    true
);

impl xai_tool_runtime::Tool for ListAgentsTool {
    type Args = ListAgentsInput;
    type Output = ListAgentsOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("list_agents").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "list_agents",
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
        _input: ListAgentsInput,
    ) -> Result<ListAgentsOutput, xai_tool_runtime::ToolError> {
        let (backend, identity) = collaboration_resources_async(&ctx).await?;
        Ok(backend.backend().list_agents(identity).await)
    }
}

macro_rules! message_tool {
    ($tool:ty, $id:literal, $kind:expr) => {
        impl xai_tool_runtime::Tool for $tool {
            type Args = SendAgentMessageInput;
            type Output = AgentMessageSendOutput;

            fn id(&self) -> xai_tool_protocol::ToolId {
                xai_tool_protocol::ToolId::new($id).expect("valid tool id")
            }

            fn description(
                &self,
                _ctx: &xai_tool_runtime::ListToolsContext,
            ) -> xai_tool_types::ToolDescription {
                xai_tool_types::ToolDescription::new(
                    $id,
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
                input: SendAgentMessageInput,
            ) -> Result<AgentMessageSendOutput, xai_tool_runtime::ToolError> {
                let (target, body) = validate_message(input)?;
                let (backend, identity) = collaboration_resources_async(&ctx).await?;
                let message = stamped_message(&identity, target.clone(), body, $kind);
                backend
                    .backend()
                    .send_agent_message(identity, &target, message)
                    .await
                    .map_err(|error| xai_tool_runtime::ToolError::custom("agent_mailbox", error))
            }
        }
    };
}

message_tool!(
    SendAgentMessageTool,
    "send_message",
    AgentMailboxMessageKind::Message
);
message_tool!(
    FollowupAgentTaskTool,
    "followup_task",
    AgentMailboxMessageKind::FollowupTask
);

impl xai_tool_runtime::Tool for WaitAgentTool {
    type Args = WaitAgentInput;
    type Output = WaitAgentMessagesOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("wait_agent").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "wait_agent",
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
        input: WaitAgentInput,
    ) -> Result<WaitAgentMessagesOutput, xai_tool_runtime::ToolError> {
        let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS);
        let (backend, identity) = collaboration_resources_async(&ctx).await?;
        Ok(backend
            .backend()
            .wait_agent_messages(identity, timeout_ms)
            .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_tool_runtime::Tool;

    #[test]
    fn collaboration_tool_ids_and_kind_are_stable() {
        for (id, actual) in [
            ("list_agents", Tool::id(&ListAgentsTool)),
            ("send_message", Tool::id(&SendAgentMessageTool)),
            ("followup_task", Tool::id(&FollowupAgentTaskTool)),
            ("wait_agent", Tool::id(&WaitAgentTool)),
        ] {
            assert_eq!(actual.as_str(), id);
        }
        assert_eq!(
            ToolMetadata::kind(&SendAgentMessageTool),
            ToolKind::AgentCollaboration
        );
    }

    #[test]
    fn message_validation_rejects_empty_and_oversized_text() {
        assert!(
            validate_message(SendAgentMessageInput {
                target: "child".to_string(),
                message: " ".to_string(),
            })
            .is_err()
        );
        assert!(
            validate_message(SendAgentMessageInput {
                target: "child".to_string(),
                message: "x".repeat(MAX_AGENT_MESSAGE_BYTES + 1),
            })
            .is_err()
        );
        assert_eq!(
            validate_message(SendAgentMessageInput {
                target: " child ".to_string(),
                message: " hello ".to_string(),
            })
            .expect("valid message"),
            ("child".to_string(), "hello".to_string())
        );
    }

    #[test]
    fn wait_timeout_is_capped_by_tool_contract() {
        assert_eq!(DEFAULT_WAIT_MS, 30_000);
        assert_eq!(MAX_WAIT_MS, 600_000);
    }
}
