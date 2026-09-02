//! Unit tests for the Google AI Studio L2 stream transform.

use super::*;
use futures_util::stream;
use xai_grok_sampling_types::google_ai_studio::{
    Candidate, Content, FinishReason, FunctionCall, Part, UsageMetadata,
};

fn rid() -> RequestId {
    RequestId::from("goog-test")
}

#[tokio::test]
async fn stream_text_and_thinking_tokens() {
    let chunks = vec![
        Ok(GenerateContentResponse {
            candidates: vec![Candidate {
                content: Some(Content {
                    role: Some("model".to_string()),
                    parts: vec![Part {
                        thought: Some(true),
                        text: Some("Thinking step 1...".to_string()),
                        ..Default::default()
                    }],
                }),
                finish_reason: None,
                finish_message: None,
                index: Some(0),
            }],
            usage_metadata: None,
            model_version: Some("gemini-2.5-flash".to_string()),
        }),
        Ok(GenerateContentResponse {
            candidates: vec![Candidate {
                content: Some(Content {
                    role: Some("model".to_string()),
                    parts: vec![Part::text("Hello, ")],
                }),
                finish_reason: None,
                finish_message: None,
                index: Some(0),
            }],
            usage_metadata: None,
            model_version: Some("gemini-2.5-flash".to_string()),
        }),
        Ok(GenerateContentResponse {
            candidates: vec![Candidate {
                content: Some(Content {
                    role: Some("model".to_string()),
                    parts: vec![Part::text("world!")],
                }),
                finish_reason: Some(FinishReason::Stop),
                finish_message: None,
                index: Some(0),
            }],
            usage_metadata: Some(UsageMetadata {
                prompt_token_count: Some(10),
                candidates_token_count: Some(5),
                total_token_count: Some(15),
                cached_content_token_count: Some(2),
            }),
            model_version: Some("gemini-2.5-flash".to_string()),
        }),
    ];

    let raw = stream::iter(chunks).boxed();
    let events: Vec<SamplingEvent> =
        stream_google_ai_studio(raw, None, rid(), Duration::from_secs(5))
            .collect()
            .await;

    // Check event sequence:
    // StreamStarted, ResponseStarted, FirstToken, ChannelToken(Reasoning), ChannelToken(Text), ChannelToken(Text), Completed
    assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
    assert!(matches!(events[1], SamplingEvent::ResponseStarted { .. }));
    assert!(matches!(events[2], SamplingEvent::FirstToken { .. }));

    let reasoning_tokens: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ChannelToken {
                channel: SamplingChannel::Reasoning,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning_tokens, vec!["Thinking step 1..."]);

    let text_tokens: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ChannelToken {
                channel: SamplingChannel::Text,
                text,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_tokens, vec!["Hello, ", "world!"]);

    let completed = events
        .iter()
        .find_map(|e| match e {
            SamplingEvent::Completed { response, .. } => Some(response),
            _ => None,
        })
        .expect("Completed event");

    assert_eq!(completed.items.len(), 2);
    // Item 0: Reasoning
    match &completed.items[0] {
        ConversationItem::Reasoning(r) => {
            let text = match &r.summary[0] {
                rs::SummaryPart::SummaryText(t) => &t.text,
            };
            assert_eq!(text, "Thinking step 1...");
        }
        other => panic!("expected reasoning item, got {other:?}"),
    }
    // Item 1: Assistant
    match &completed.items[1] {
        ConversationItem::Assistant(a) => {
            assert_eq!(a.content.as_ref(), "Hello, world!");
            assert!(a.tool_calls.is_empty());
        }
        other => panic!("expected assistant item, got {other:?}"),
    }

    assert_eq!(completed.stop_reason, Some(StopReason::Stop));
    let usage = completed.usage.as_ref().expect("usage present");
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.total_tokens, 15);
    assert_eq!(usage.cached_prompt_tokens, 2);
}

#[tokio::test]
async fn stream_function_call_events() {
    let chunks = vec![Ok(GenerateContentResponse {
        candidates: vec![Candidate {
            content: Some(Content {
                role: Some("model".to_string()),
                parts: vec![Part {
                    function_call: Some(FunctionCall {
                        name: "calculator".to_string(),
                        args: serde_json::json!({ "expr": "2 + 2" }),
                        id: Some("call_calc_42".to_string()),
                    }),
                    ..Default::default()
                }],
            }),
            finish_reason: Some(FinishReason::Stop),
            finish_message: None,
            index: Some(0),
        }],
        usage_metadata: None,
        model_version: Some("gemini-2.5-flash".to_string()),
    })];

    let raw = stream::iter(chunks).boxed();
    let events: Vec<SamplingEvent> =
        stream_google_ai_studio(raw, None, rid(), Duration::from_secs(5))
            .collect()
            .await;

    let delta = events
        .iter()
        .find_map(|e| match e {
            SamplingEvent::ToolCallDelta {
                name,
                arguments_delta,
                id,
                ..
            } => Some((id.clone(), name.clone(), arguments_delta.clone())),
            _ => None,
        })
        .expect("ToolCallDelta event");
    assert_eq!(delta.0.as_deref(), Some("call_calc_42"));
    assert_eq!(delta.1.as_deref(), Some("calculator"));
    assert_eq!(delta.2.as_deref(), Some(r#"{"expr":"2 + 2"}"#));

    let complete = events
        .iter()
        .find_map(|e| match e {
            SamplingEvent::ToolCallArgumentsComplete { id, name, .. } => {
                Some((id.clone(), name.clone()))
            }
            _ => None,
        })
        .expect("ToolCallArgumentsComplete event");
    assert_eq!(complete.0.as_deref(), Some("call_calc_42"));
    assert_eq!(complete.1.as_deref(), Some("calculator"));

    let completed = events
        .iter()
        .find_map(|e| match e {
            SamplingEvent::Completed { response, .. } => Some(response),
            _ => None,
        })
        .expect("Completed event");

    assert_eq!(completed.stop_reason, Some(StopReason::ToolCalls));
    let assistant = match &completed.items[0] {
        ConversationItem::Assistant(a) => a,
        other => panic!("expected assistant item, got {other:?}"),
    };
    assert_eq!(assistant.tool_calls.len(), 1);
    assert_eq!(assistant.tool_calls[0].name, "calculator");
    assert_eq!(assistant.tool_calls[0].id.as_ref(), "call_calc_42");
}
