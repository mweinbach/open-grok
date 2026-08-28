use crate::implementations::grok_build::task::backend::SubagentBackendResource;
use crate::implementations::grok_build::task::types::{
    AgentMailboxIdentity, AgentMailboxMessage, AgentMailboxMessageKind, NativeAgentMessage,
    NativeAgentOperation, NativeAgentSpawn,
};
use crate::types::output::ToolOutput;
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{ToolMetadata, shared_resources};
use serde::{Deserialize, Serialize};
use xai_tool_runtime::{Tool, ToolCallContext, ToolError};

#[derive(Clone, Copy, Default)]
pub struct NativeAgentsEnabled(pub bool);

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnAgentInput {
    pub task_name: String,
    #[schemars(extend("encrypted" = true))]
    pub message: String,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub fork_turns: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MessageInput {
    pub target: String,
    #[schemars(extend("encrypted" = true))]
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListInput {
    pub path_prefix: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitInput {
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InterruptInput {
    pub target: String,
}

macro_rules! dynamic_input {
    ($($input:ty),+ $(,)?) => {$(
        impl From<$input> for crate::types::tool_io::ToolInput {
            fn from(input: $input) -> Self {
                Self::Dynamic(serde_json::to_value(input).expect("native tool input is serializable"))
            }
        }
    )+};
}
dynamic_input!(
    SpawnAgentInput,
    MessageInput,
    ListInput,
    WaitInput,
    InterruptInput
);

async fn resources(
    ctx: &ToolCallContext,
) -> Result<(SubagentBackendResource, AgentMailboxIdentity), ToolError> {
    let resources = shared_resources(ctx)?;
    let resources = resources.lock().await;
    if !resources
        .get::<NativeAgentsEnabled>()
        .is_some_and(|enabled| enabled.0)
    {
        return Err(ToolError::custom(
            "tool_unavailable",
            "Native multi-agent v2 is not enabled for this session",
        ));
    }
    let backend = resources
        .get::<SubagentBackendResource>()
        .cloned()
        .ok_or_else(|| {
            ToolError::custom("missing_resource", "Subagent coordinator is unavailable")
        })?;
    let identity = resources
        .get::<AgentMailboxIdentity>()
        .cloned()
        .ok_or_else(|| ToolError::custom("missing_resource", "Agent identity is unavailable"))?;
    Ok((backend, identity))
}

fn message(
    identity: &AgentMailboxIdentity,
    body: String,
    kind: AgentMailboxMessageKind,
    ctx: &ToolCallContext,
) -> Result<AgentMailboxMessage, ToolError> {
    if body.trim().is_empty() || body.len() > 32 * 1024 {
        return Err(ToolError::invalid_arguments(
            "message must be nonempty and at most 32768 bytes",
        ));
    }
    Ok(AgentMailboxMessage {
        message_id: format!("amsg_{}", uuid::Uuid::now_v7()),
        team_scope_id: identity.team_scope_id.clone(),
        from_agent_id: identity.agent_id.clone(),
        to_agent_id: String::new(),
        kind,
        body,
        created_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        native: Some(NativeAgentMessage {
            author: String::new(),
            recipient: String::new(),
            encrypted: ctx
                .get::<super::multi_agent_wire::NativeMessageEncryption>()
                .map(|encryption| encryption.0)
                .unwrap_or_else(|| {
                    super::multi_agent_wire::codex_message_is_encrypted(ctx.call_id.as_str())
                }),
            trigger_prompt_id: None,
        }),
    })
}

async fn operate(
    ctx: &ToolCallContext,
    operation: NativeAgentOperation,
) -> Result<ToolOutput, ToolError> {
    let (backend, identity) = resources(ctx).await?;
    backend
        .backend()
        .native_agent(identity, operation)
        .await
        .map(|output| ToolOutput::Dynamic(output.into()))
        .map_err(|error| ToolError::custom("agent_collaboration", error))
}

pub fn parse_fork_turns(
    value: Option<&str>,
) -> Result<(xai_tool_types::SubagentContextMode, Option<usize>), String> {
    match value.unwrap_or("all") {
        "none" => Ok((xai_tool_types::SubagentContextMode::Fresh, None)),
        "all" => Ok((xai_tool_types::SubagentContextMode::Fork, None)),
        count => count
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .map(|count| (xai_tool_types::SubagentContextMode::Fork, Some(count)))
            .ok_or_else(|| "fork_turns must be none, all, or a positive integer string".to_owned()),
    }
}

macro_rules! native_tool {
    ($tool:ident, $name:literal, $input:ty, $kind:expr, $read_only:literal, $description:literal, $handler:ident) => {
        #[derive(Debug, Default)]
        pub struct $tool;
        impl ToolMetadata for $tool {
            fn kind(&self) -> ToolKind {
                $kind
            }
            fn tool_namespace(&self) -> ToolNamespace {
                ToolNamespace::Codex
            }
            fn is_read_only(&self) -> bool {
                $read_only
            }
            fn description_template(&self) -> &str {
                $description
            }
        }
        impl Tool for $tool {
            type Args = $input;
            type Output = ToolOutput;
            fn id(&self) -> xai_tool_protocol::ToolId {
                xai_tool_protocol::ToolId::new($name).expect("valid native tool id")
            }
            fn description(
                &self,
                _ctx: &xai_tool_runtime::ListToolsContext,
            ) -> xai_tool_types::ToolDescription {
                xai_tool_types::ToolDescription::new($name, $description)
            }
            fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
                xai_tool_protocol::ToolCapabilities {
                    is_read_only: $read_only,
                    tool_scope: Some(if $read_only {
                        xai_tool_protocol::ToolScope::Read
                    } else {
                        xai_tool_protocol::ToolScope::Write
                    }),
                    ..Default::default()
                }
            }
            async fn run(
                &self,
                ctx: ToolCallContext,
                input: Self::Args,
            ) -> Result<ToolOutput, ToolError> {
                $handler(ctx, input).await
            }
        }
    };
}

native_tool!(
    SpawnAgentTool,
    "spawn_agent",
    SpawnAgentInput,
    ToolKind::AgentCollaboration,
    false,
    "Start a named background agent. task_name uses lowercase letters, digits, and underscores. fork_turns defaults to all; use none for fresh context or a positive integer string for recent turns. Same-model forks retain compatible history; cross-model forks use a plaintext digest. Encrypted messages cannot cross provider boundaries; use the ordinary host task tool for plaintext cross-provider delegation. The host's depth and permission limits still apply. Reuse an existing task with followup_task.",
    spawn
);
native_tool!(
    SendMessageTool,
    "send_message",
    MessageInput,
    ToolKind::AgentCollaboration,
    false,
    "Deliver a message promptly to an agent by canonical task path, relative task name, or ID. Does not start a turn when the recipient is idle. Use followup_task to assign work.",
    send
);
native_tool!(
    FollowupTaskTool,
    "followup_task",
    MessageInput,
    ToolKind::AgentCollaboration,
    false,
    "Send work to a non-root agent. Starts a turn when idle and delivers promptly when running. Reuses a completed named agent with its own model, role, working directory, and history.",
    followup
);
native_tool!(
    ListAgentsTool,
    "list_agents",
    ListInput,
    ToolKind::AgentCollaboration,
    true,
    "List this team's named agents and lifecycle status without exposing transcripts. Optionally filter by a canonical task-path prefix without a trailing slash.",
    list
);
native_tool!(
    WaitAgentTool,
    "wait_agent",
    WaitInput,
    ToolKind::AgentCollaboration,
    true,
    "Wait for agent messages or final-status activity. Returns an activity summary, not message contents. Ends early for steered user input. timeout_ms defaults to 30000, maximum 600000; 0 polls.",
    wait
);
native_tool!(
    InterruptAgentTool,
    "interrupt_agent",
    InterruptInput,
    ToolKind::AgentCollaboration,
    false,
    "Interrupt an agent's current turn without deleting its history or task identity. The agent remains reusable with followup_task. Cannot target the root or yourself.",
    interrupt
);

async fn spawn(mut ctx: ToolCallContext, input: SpawnAgentInput) -> Result<ToolOutput, ToolError> {
    let (_, identity) = resources(&ctx).await?;
    let (context, fork_turns) =
        parse_fork_turns(input.fork_turns.as_deref()).map_err(ToolError::invalid_arguments)?;
    let message = message(
        &identity,
        input.message,
        AgentMailboxMessageKind::NativeFollowup,
        &ctx,
    )?;
    let task_name = input.task_name;
    if task_name.is_empty()
        || task_name.len() > 64
        || task_name == "root"
        || !task_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ToolError::invalid_arguments(
            "task_name must use lowercase letters, digits, and underscores; root is reserved",
        ));
    }
    ctx.insert(NativeAgentSpawn {
        task_name: task_name.clone(),
        fork_turns,
        service_tier: input.service_tier,
        message: Some(message),
    });
    crate::implementations::grok_build::task::TaskTool
        .run(
            ctx,
            xai_tool_types::TaskToolInput {
                task_id: None,
                description: task_name,
                prompt: "Complete the task in the agent message supplied with this turn."
                    .to_owned(),
                subagent_type: input
                    .agent_type
                    .unwrap_or_else(|| "general-purpose".to_owned()),
                model: input.model,
                reasoning_effort: input.reasoning_effort,
                context: Some(context),
                run_in_background: true,
                resume_from: None,
                cwd: None,
                capability_mode: None,
                isolation: None,
            },
        )
        .await
}

async fn send(ctx: ToolCallContext, input: MessageInput) -> Result<ToolOutput, ToolError> {
    let (_, identity) = resources(&ctx).await?;
    let message = message(
        &identity,
        input.message,
        AgentMailboxMessageKind::NativeMessage,
        &ctx,
    )?;
    operate(
        &ctx,
        NativeAgentOperation::Message {
            target: input.target,
            message,
        },
    )
    .await
}

async fn followup(ctx: ToolCallContext, input: MessageInput) -> Result<ToolOutput, ToolError> {
    let (_, identity) = resources(&ctx).await?;
    let mut message = message(
        &identity,
        input.message,
        AgentMailboxMessageKind::NativeFollowup,
        &ctx,
    )?;
    if identity.agent_id == identity.team_scope_id {
        let resources = shared_resources(&ctx)?;
        message
            .native
            .as_mut()
            .expect("native message")
            .trigger_prompt_id = resources
            .lock()
            .await
            .get::<crate::implementations::grok_build::task::types::CurrentPromptIdResource>()
            .map(|prompt| prompt.0.clone());
    }
    operate(
        &ctx,
        NativeAgentOperation::Message {
            target: input.target,
            message,
        },
    )
    .await
}

async fn list(ctx: ToolCallContext, input: ListInput) -> Result<ToolOutput, ToolError> {
    operate(
        &ctx,
        NativeAgentOperation::List {
            path_prefix: input.path_prefix,
        },
    )
    .await
}

async fn wait(ctx: ToolCallContext, input: WaitInput) -> Result<ToolOutput, ToolError> {
    operate(
        &ctx,
        NativeAgentOperation::Wait {
            timeout_ms: input.timeout_ms.unwrap_or(30_000),
        },
    )
    .await
}

async fn interrupt(ctx: ToolCallContext, input: InterruptInput) -> Result<ToolOutput, ToolError> {
    operate(
        &ctx,
        NativeAgentOperation::Interrupt {
            target: input.target,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_agent_message_preserves_bytes_and_dispatch_encryption_context() {
        let identity = AgentMailboxIdentity {
            team_scope_id: "team".to_owned(),
            agent_id: "root".to_owned(),
        };
        for encrypted in [None, Some(false), Some(true)] {
            let mut ctx =
                ToolCallContext::new(xai_tool_protocol::ToolCallId::new("call_native").unwrap());
            if let Some(encrypted) = encrypted {
                ctx.insert(super::super::multi_agent_wire::NativeMessageEncryption(
                    encrypted,
                ));
            }
            let body = "  opaque-test-payload\n";
            let result = message(
                &identity,
                body.to_owned(),
                AgentMailboxMessageKind::NativeMessage,
                &ctx,
            )
            .unwrap();
            assert_eq!(result.body, body);
            let suffix = result
                .message_id
                .strip_prefix("amsg_")
                .expect("native agent message IDs must use the Responses prefix");
            assert!(uuid::Uuid::parse_str(suffix).is_ok());
            assert_eq!(result.native_wire_item().unwrap()["id"], result.message_id);
            assert_eq!(result.native.unwrap().encrypted, encrypted.unwrap_or(true));
        }
    }

    #[test]
    fn native_agents_validate_fork_turns_and_reject_obsolete_parameters() {
        assert_eq!(
            parse_fork_turns(None).unwrap(),
            (xai_tool_types::SubagentContextMode::Fork, None)
        );
        assert_eq!(
            parse_fork_turns(Some("none")).unwrap(),
            (xai_tool_types::SubagentContextMode::Fresh, None)
        );
        assert_eq!(
            parse_fork_turns(Some("3")).unwrap(),
            (xai_tool_types::SubagentContextMode::Fork, Some(3))
        );
        for invalid in ["0", "-1", "1.5", "fresh", ""] {
            assert!(parse_fork_turns(Some(invalid)).is_err());
        }
        assert!(
            serde_json::from_value::<SpawnAgentInput>(serde_json::json!({
                "task_name":"worker", "message":"work", "fork_context":true,
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn native_agents_require_host_opt_in_before_dispatch() {
        for flag in [None, Some(false)] {
            let mut resources = crate::types::resources::Resources::new();
            if let Some(flag) = flag {
                resources.insert(NativeAgentsEnabled(flag));
            }
            let result = SendMessageTool
                .run(
                    crate::types::tool_metadata::test_ctx(resources.into_shared()),
                    MessageInput {
                        target: "/root/worker".to_owned(),
                        message: "test".to_owned(),
                    },
                )
                .await;
            assert!(result.unwrap_err().to_string().contains("not enabled"));
        }
    }
}
