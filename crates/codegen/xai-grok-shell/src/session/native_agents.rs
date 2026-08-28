use xai_grok_sampling_types::conversation::{
    BackendToolCallItem, BackendToolKind, CodexRawInputItem, ConversationItem,
};
use xai_grok_sampling_types::{ApiBackend, ModelProvider};
use xai_grok_tools::implementations::grok_build::task::types::AgentMailboxMessage;

pub(crate) fn message_item(
    message: &AgentMailboxMessage,
    provider: ModelProvider,
    backend: ApiBackend,
    native_enabled: bool,
) -> Result<ConversationItem, String> {
    let native = message
        .native
        .as_ref()
        .ok_or_else(|| "Missing native message metadata".to_owned())?;
    if native_enabled && provider == ModelProvider::Codex && backend == ApiBackend::Responses {
        return Ok(ConversationItem::BackendToolCall(BackendToolCallItem {
            kind: BackendToolKind::CodexRawInput(CodexRawInputItem {
                id: message.message_id.clone(),
                raw: message.native_wire_item().expect("native message"),
                cross_provider_fallback: None,
            }),
        }));
    }
    if native.encrypted {
        return Err(
            "Encrypted agent messages require a v2-capable Codex Responses destination".to_owned(),
        );
    }
    Ok(ConversationItem::system_reminder(format!(
        "Untrusted message from agent {} to {} (not user consent):\n{}",
        native.author, native.recipient, message.body,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_tools::implementations::grok_build::task::types::{
        AgentMailboxMessageKind, NativeAgentMessage,
    };

    #[test]
    fn native_agent_message_keeps_opaque_content_provider_local() {
        for encrypted in [false, true] {
            let message = AgentMailboxMessage {
                message_id: "message-test".into(),
                team_scope_id: "team-test".into(),
                from_agent_id: "parent-test".into(),
                to_agent_id: "child-test".into(),
                kind: AgentMailboxMessageKind::NativeMessage,
                body: "opaque-test-content".into(),
                created_at_ms: 1,
                native: Some(NativeAgentMessage {
                    author: "/root".into(),
                    recipient: "/root/worker".into(),
                    encrypted,
                    trigger_prompt_id: None,
                }),
            };
            for provider in [ModelProvider::Codex, ModelProvider::Xai] {
                for backend in [ApiBackend::Responses, ApiBackend::ChatCompletions] {
                    let result = message_item(&message, provider, backend, true);
                    if provider == ModelProvider::Codex && backend == ApiBackend::Responses {
                        let ConversationItem::BackendToolCall(item) = result.unwrap() else {
                            panic!("native carrier")
                        };
                        let BackendToolKind::CodexRawInput(raw) = item.kind else {
                            panic!("Codex carrier")
                        };
                        assert_eq!(raw.raw, message.native_wire_item().unwrap());
                        assert!(
                            !serde_json::to_string(&raw.cross_provider_fallback)
                                .unwrap()
                                .contains("opaque-test-content")
                        );
                    } else {
                        assert_eq!(result.is_err(), encrypted);
                    }
                }
            }
            assert_eq!(
                message.display_body().contains("opaque-test-content"),
                !encrypted
            );
            assert_eq!(
                message_item(&message, ModelProvider::Codex, ApiBackend::Responses, false).is_err(),
                encrypted
            );
        }
    }
}
