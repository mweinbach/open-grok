use std::sync::Arc;

use xai_grok_sampling_types::ConversationItem;
use xai_grok_sampling_types::Role;
use xai_grok_tools::implementations::grok_build::SearchCommands;
use xai_grok_tools::implementations::grok_build::StandaloneWebSearchBackend;
use xai_grok_tools::implementations::grok_build::StandaloneWebSearchFuture;

const MAX_ASSISTANT_CONTEXT_CHARS: usize = 8_000;

#[derive(Clone)]
pub(crate) struct SamplerStandaloneWebSearchBackend {
    client: xai_grok_sampler::SamplingClient,
    chat_state: xai_chat_state::ChatStateHandle,
    session_id: String,
    model: String,
}

impl SamplerStandaloneWebSearchBackend {
    pub(crate) fn new(
        config: xai_grok_sampler::SamplerConfig,
        chat_state: xai_chat_state::ChatStateHandle,
        session_id: String,
    ) -> Result<Arc<Self>, xai_grok_sampling_types::SamplingError> {
        let model = config.model.clone();
        let client = xai_grok_sampler::SamplingClient::new(config)?;
        Ok(Arc::new(Self {
            client,
            chat_state,
            session_id,
            model,
        }))
    }

    async fn request_input(&self) -> Vec<xai_grok_sampler::StandaloneSearchMessage> {
        let conversation = self.chat_state.get_conversation().await;
        let mut visible = Vec::new();
        let mut user_indices = Vec::new();
        for item in &conversation {
            let text = item.text_content();
            if text.trim().is_empty() {
                continue;
            }
            match item.role() {
                Role::User
                    if matches!(
                        item,
                        ConversationItem::User(user) if user.synthetic_reason.is_none()
                    ) =>
                {
                    user_indices.push(visible.len());
                    visible.push((Role::User, text));
                }
                Role::Assistant => {
                    visible.push((Role::Assistant, text));
                }
                _ => {}
            }
        }

        let Some(&last_user_index) = user_indices.last() else {
            return Vec::new();
        };
        let first_user_index = user_indices
            .iter()
            .rev()
            .nth(1)
            .copied()
            .unwrap_or(last_user_index);
        let mut assistant_chars = 0;
        let mut messages = Vec::new();
        for (role, text) in visible
            .into_iter()
            .skip(first_user_index)
            .take(last_user_index - first_user_index + 1)
        {
            match role {
                Role::User => {
                    messages.push(xai_grok_sampler::StandaloneSearchMessage::user(text));
                }
                Role::Assistant if assistant_chars < MAX_ASSISTANT_CONTEXT_CHARS => {
                    let remaining = MAX_ASSISTANT_CONTEXT_CHARS - assistant_chars;
                    let text = truncate_chars(&text, remaining);
                    assistant_chars += text.chars().count();
                    messages.push(xai_grok_sampler::StandaloneSearchMessage::assistant(text));
                }
                _ => {}
            }
        }
        messages
    }
}

impl std::fmt::Debug for SamplerStandaloneWebSearchBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SamplerStandaloneWebSearchBackend")
            .field("session_id", &self.session_id)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl StandaloneWebSearchBackend for SamplerStandaloneWebSearchBackend {
    fn search<'a>(&'a self, commands: SearchCommands) -> StandaloneWebSearchFuture<'a> {
        Box::pin(async move {
            let model = self
                .chat_state
                .get_sampling_config()
                .await
                .map(|config| config.model)
                .unwrap_or_else(|| self.model.clone());
            let request = xai_grok_sampler::StandaloneSearchRequest {
                id: self.session_id.clone(),
                model,
                reasoning: None,
                input: Some(xai_grok_sampler::StandaloneSearchInput::Items(
                    self.request_input().await,
                )),
                commands: serde_json::to_value(commands).map_err(|error| error.to_string())?,
                settings:
                    xai_grok_sampler::StandaloneSearchSettings::direct_with_external_web_access(),
                max_output_tokens: Some(10_000),
            };
            self.client
                .standalone_web_search(&request)
                .await
                .map(|response| response.output)
                .map_err(|error| error.to_string())
        })
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use xai_grok_sampling_types::ConversationItem;

    use super::SamplerStandaloneWebSearchBackend;

    #[tokio::test]
    async fn request_input_keeps_two_visible_user_turns_and_intervening_assistant_text() {
        let conversation = vec![
            ConversationItem::user("old user"),
            ConversationItem::assistant("old assistant"),
            ConversationItem::user("previous user"),
            ConversationItem::assistant("previous assistant"),
            ConversationItem::user("current user"),
            ConversationItem::assistant("trailing assistant"),
        ];
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let chat_state = xai_chat_state::ChatStateActor::spawn(
            conversation,
            xai_grok_sampling_types::SamplingConfig {
                base_url: "https://example.test/v1".to_string(),
                model: "gpt-test".to_string(),
                max_completion_tokens: None,
                temperature: None,
                top_p: None,
                api_backend: Default::default(),
                provider: Default::default(),
                extra_headers: Default::default(),
                query_params: Default::default(),
                env_http_headers: Default::default(),
                context_window: NonZeroU64::new(256_000).unwrap(),
                reasoning_effort: None,
                service_tier: None,
                stream_tool_calls: None,
            },
            Box::new(xai_chat_state::NullChatPersistence),
            event_tx,
            CancellationToken::new(),
        );
        let backend = SamplerStandaloneWebSearchBackend::new(
            xai_grok_sampler::SamplerConfig::default(),
            chat_state,
            "child-session".to_string(),
        )
        .expect("standalone backend should build");

        let input = backend.request_input().await;

        assert_eq!(
            serde_json::to_value(input).unwrap(),
            serde_json::json!([
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "previous user"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "previous assistant"}]
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "current user"}]
                }
            ])
        );
    }
}
