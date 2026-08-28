use crate::notification::types::{AsyncUserMessage, ToolNotification};
use crate::types::resources::NotificationHandle;
use crate::types::tool::{ToolKind, ToolNamespace};
use crate::types::tool_metadata::{ToolMetadata, shared_resources};

pub const TOOL_NAME: &str = "send_user_message_async";
pub const ASYNC_USER_MESSAGE_META_KEY: &str = "x.ai/async_user_message";

#[derive(Clone, Copy, Default)]
pub struct AsyncUserMessagesEnabled(pub bool);

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendUserMessageAsyncInput {
    pub message: String,
}

impl From<SendUserMessageAsyncInput> for crate::types::tool_io::ToolInput {
    fn from(input: SendUserMessageAsyncInput) -> Self {
        Self::Dynamic(serde_json::json!({ "message": input.message }))
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SendUserMessageAsyncOutput {
    pub accepted: bool,
}

impl xai_tool_runtime::ToolOutput for SendUserMessageAsyncOutput {}

impl From<SendUserMessageAsyncOutput> for crate::types::output::ToolOutput {
    fn from(output: SendUserMessageAsyncOutput) -> Self {
        Self::Dynamic(serde_json::json!({ "accepted": output.accepted }).into())
    }
}

#[derive(Debug, Default)]
pub struct SendUserMessageAsyncTool;

impl ToolMetadata for SendUserMessageAsyncTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Codex
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn emitted_notifications(&self) -> &'static [&'static str] {
        &["AsyncUserMessage"]
    }

    fn description_template(&self) -> &str {
        "Send a concise acknowledgment, important update, or question to the user without ending the turn or waiting for an answer. Returns immediately; any reply arrives asynchronously as a new user message."
    }
}

impl xai_tool_runtime::Tool for SendUserMessageAsyncTool {
    type Args = SendUserMessageAsyncInput;
    type Output = SendUserMessageAsyncOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(TOOL_NAME, self.description_template())
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
        input: Self::Args,
    ) -> Result<Self::Output, xai_tool_runtime::ToolError> {
        let message = input.message.trim();
        if message.is_empty() {
            return Err(xai_tool_runtime::ToolError::custom(
                "invalid_message",
                "message must not be empty",
            ));
        }
        let resources = shared_resources(&ctx)?;
        let resources = resources.lock().await;
        if !resources
            .get::<AsyncUserMessagesEnabled>()
            .is_some_and(|enabled| enabled.0)
        {
            return Err(xai_tool_runtime::ToolError::custom(
                "tool_unavailable",
                "Asynchronous user messages are not enabled for this session",
            ));
        }
        let handle = resources.get::<NotificationHandle>().ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "delivery_unavailable",
                "User message delivery is not available",
            )
        })?;
        handle
            .0
            .send(ToolNotification::AsyncUserMessage(AsyncUserMessage {
                tool_call_id: ctx.call_id.as_str().to_owned(),
                message: message.to_owned(),
            }));
        Ok(SendUserMessageAsyncOutput { accepted: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::ToolNotificationHandle;
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx_with_call_id;
    use xai_tool_runtime::Tool;

    #[tokio::test]
    async fn async_user_message_delivers_without_waiting_for_a_reply() {
        let (handle, mut receiver) = ToolNotificationHandle::channel();
        let mut resources = Resources::new();
        resources.insert(NotificationHandle(handle));
        resources.insert(AsyncUserMessagesEnabled(true));
        let output = SendUserMessageAsyncTool
            .run(
                test_ctx_with_call_id(resources.into_shared(), "call-message"),
                SendUserMessageAsyncInput {
                    message: "  Which option?  ".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(output).unwrap(),
            serde_json::json!({ "accepted": true })
        );
        let ToolNotification::AsyncUserMessage(message) = receiver.try_recv().unwrap() else {
            panic!("expected an async user message");
        };
        assert_eq!(message.tool_call_id, "call-message");
        assert_eq!(message.message, "Which option?");
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn async_user_message_requires_opt_in_and_delivery() {
        for enabled in [None, Some(false), Some(true)] {
            let mut resources = Resources::new();
            if let Some(enabled) = enabled {
                resources.insert(AsyncUserMessagesEnabled(enabled));
            }
            let result = SendUserMessageAsyncTool
                .run(
                    test_ctx_with_call_id(resources.into_shared(), "call-message"),
                    SendUserMessageAsyncInput {
                        message: "Question".into(),
                    },
                )
                .await;
            assert!(result.is_err());
        }
    }

    #[tokio::test]
    async fn async_user_message_rejects_empty_messages_and_unknown_fields() {
        let result = SendUserMessageAsyncTool
            .run(
                test_ctx_with_call_id(Resources::new().into_shared(), "call-message"),
                SendUserMessageAsyncInput {
                    message: " \n\t ".into(),
                },
            )
            .await;
        assert!(result.is_err());
        assert!(
            serde_json::from_value::<SendUserMessageAsyncInput>(serde_json::json!({
                "message": "Question", "wait": true,
            }))
            .is_err()
        );
    }
}
