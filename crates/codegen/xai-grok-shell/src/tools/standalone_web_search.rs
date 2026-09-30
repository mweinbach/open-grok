use std::sync::Arc;

use xai_grok_sampler::StandaloneSearchInput;
use xai_grok_sampler::StandaloneSearchMessage;
use xai_grok_sampling_types::ConversationItem;
use xai_grok_sampling_types::Role;
use xai_grok_tools::implementations::grok_build::SearchCommands;
use xai_grok_tools::implementations::grok_build::StandaloneWebSearchBackend;
use xai_grok_tools::implementations::grok_build::StandaloneWebSearchFuture;
use xai_grok_tools::util::ceil_char_boundary;
use xai_grok_tools::util::truncate_str;

const ASSISTANT_CONTEXT_TOKEN_LIMIT: usize = 1_000;
// Latest Codex Code Mode models advertise a 10,000-token truncation policy.
const DEFAULT_SEARCH_OUTPUT_TOKEN_BUDGET: u64 = 10_000;

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

    async fn request_input(&self) -> Option<StandaloneSearchInput> {
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

        let &last_user_index = user_indices.last()?;
        let first_user_index = user_indices
            .iter()
            .rev()
            .nth(1)
            .copied()
            .unwrap_or(last_user_index);
        let mut assistant_tokens_remaining = ASSISTANT_CONTEXT_TOKEN_LIMIT;
        let mut messages = Vec::new();
        for (role, text) in visible
            .into_iter()
            .skip(first_user_index)
            .take(last_user_index - first_user_index + 1)
        {
            match role {
                Role::User => {
                    messages.push(StandaloneSearchMessage::user(text));
                }
                Role::Assistant if assistant_tokens_remaining > 0 => {
                    let token_count = approximate_token_count(&text);
                    let text = if token_count <= assistant_tokens_remaining {
                        assistant_tokens_remaining =
                            assistant_tokens_remaining.saturating_sub(token_count);
                        text
                    } else {
                        let text =
                            truncate_middle_with_token_budget(&text, assistant_tokens_remaining);
                        assistant_tokens_remaining = 0;
                        text
                    };
                    messages.push(StandaloneSearchMessage::assistant(text));
                }
                _ => {}
            }
        }
        Some(StandaloneSearchInput::Items(messages))
    }

    async fn build_request(
        &self,
        commands: SearchCommands,
    ) -> Result<xai_grok_sampler::StandaloneSearchRequest, String> {
        let model = self
            .chat_state
            .get_sampling_config()
            .await
            .map(|config| config.model)
            .unwrap_or_else(|| self.model.clone());
        Ok(xai_grok_sampler::StandaloneSearchRequest {
            id: self.session_id.clone(),
            model,
            reasoning: None,
            input: self.request_input().await,
            commands: serde_json::to_value(commands).map_err(|error| error.to_string())?,
            settings: xai_grok_sampler::StandaloneSearchSettings::direct_with_external_web_access(),
            max_output_tokens: Some(DEFAULT_SEARCH_OUTPUT_TOKEN_BUDGET),
        })
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
            let request = self.build_request(commands).await?;
            self.client
                .standalone_web_search(&request)
                .await
                .map(|response| response.output)
                .map_err(|error| error.to_string())
        })
    }
}

fn approximate_token_count(text: &str) -> usize {
    approximate_tokens_from_byte_count(text.len())
}

fn approximate_tokens_from_byte_count(bytes: usize) -> usize {
    bytes.saturating_add(3) / 4
}

fn truncate_middle_with_token_budget(text: &str, max_tokens: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let max_bytes = max_tokens.saturating_mul(4);
    if max_tokens > 0 && text.len() <= max_bytes {
        text.to_string()
    } else if max_bytes == 0 {
        format!("…{} tokens truncated…", approximate_token_count(text))
    } else {
        let left_budget = max_bytes / 2;
        let right_budget = max_bytes - left_budget;
        let prefix = truncate_str(text, left_budget);
        let suffix_target = text.len().saturating_sub(right_budget);
        let suffix_start = ceil_char_boundary(text, suffix_target).max(prefix.len());
        let removed_tokens =
            approximate_tokens_from_byte_count(text.len().saturating_sub(max_bytes));
        format!(
            "{prefix}…{removed_tokens} tokens truncated…{}",
            &text[suffix_start..]
        )
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;
    use std::sync::Arc;

    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use xai_grok_sampling_types::ConversationItem;

    use super::SamplerStandaloneWebSearchBackend;

    fn backend_with_conversation(
        conversation: Vec<ConversationItem>,
    ) -> Arc<SamplerStandaloneWebSearchBackend> {
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let chat_state = xai_chat_state::ChatStateActor::spawn(
            conversation,
            xai_grok_sampling_types::SamplingConfig {
                max_request_bytes: None,
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
        SamplerStandaloneWebSearchBackend::new(
            xai_grok_sampler::SamplerConfig::default(),
            chat_state,
            "child-session".to_string(),
        )
        .expect("standalone backend should build")
    }

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
        let backend = backend_with_conversation(conversation);

        let input = backend
            .request_input()
            .await
            .expect("visible user messages must produce input");

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

    #[tokio::test]
    async fn request_input_is_omitted_without_a_visible_user_message() {
        let backend =
            backend_with_conversation(vec![ConversationItem::assistant("assistant-only context")]);

        assert_eq!(backend.request_input().await, None);
    }

    #[tokio::test]
    async fn request_envelope_matches_upstream_defaults() {
        let backend =
            backend_with_conversation(vec![ConversationItem::assistant("assistant-only context")]);
        let request = backend
            .build_request(Default::default())
            .await
            .expect("search request");

        assert_eq!(request.max_output_tokens, Some(10_000));
        assert_eq!(request.input, None);
        let serialized = serde_json::to_value(request).expect("serialized search request");
        assert!(serialized.get("input").is_none());
        assert!(serialized.get("reasoning").is_none());
        assert_eq!(serialized["max_output_tokens"], 10_000);
        assert_eq!(serialized["settings"]["allowed_callers"], json!(["direct"]));
        assert_eq!(serialized["settings"]["external_web_access"], true);
    }

    #[tokio::test]
    async fn request_input_caps_assistant_context_at_upstream_token_budget() {
        let oversized = format!("prefix-{}-suffix", "x".repeat(5_000));
        let backend = backend_with_conversation(vec![
            ConversationItem::user("previous user"),
            ConversationItem::assistant(oversized),
            ConversationItem::assistant("assistant text after the exhausted budget"),
            ConversationItem::user("current user"),
        ]);

        let input = serde_json::to_value(
            backend
                .request_input()
                .await
                .expect("visible user messages must produce input"),
        )
        .unwrap();
        let items = input.as_array().expect("search input items");
        assert_eq!(items.len(), 3);
        let assistant_text = items[1]["content"][0]["text"]
            .as_str()
            .expect("assistant output text");
        assert!(assistant_text.starts_with("prefix-"));
        assert!(assistant_text.ends_with("-suffix"));
        assert!(assistant_text.contains("tokens truncated"));
        assert!(!assistant_text.contains("after the exhausted budget"));
    }
}
