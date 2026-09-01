//! Unit tests for the [`super`] Messages L2 stream transform. Extracted
//! from `messages.rs` so the implementation reads top-to-bottom; wired in
//! via `#[path = "messages_tests.rs"] mod tests;` in messages.rs.

use super::*;
use futures_util::stream;
use std::pin::pin;
use xai_grok_sampling_types::messages::{
    ContentBlock, MessageDeltaBody, MessageDeltaUsage, MessagesResponse, MessagesUsage,
    StreamDelta, StreamError,
};

fn rid() -> RequestId {
    RequestId::from("msg-test")
}

fn message_start() -> MessageStreamEvent {
    MessageStreamEvent::MessageStart {
        message: MessagesResponse {
            id: "msg_1".into(),
            r#type: "message".into(),
            role: "assistant".into(),
            content: vec![],
            model: "messages-compatible-model".into(),
            stop_reason: None,
            usage: MessagesUsage {
                input_tokens: 10,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        },
    }
}

fn text_block_start(index: u32) -> MessageStreamEvent {
    MessageStreamEvent::ContentBlockStart {
        index,
        content_block: ContentBlock::Text {
            text: String::new(),
            cache_control: None,
        },
    }
}

fn text_delta(index: u32, text: &str) -> MessageStreamEvent {
    MessageStreamEvent::ContentBlockDelta {
        index,
        delta: StreamDelta::TextDelta { text: text.into() },
    }
}

fn block_stop(index: u32) -> MessageStreamEvent {
    MessageStreamEvent::ContentBlockStop { index }
}

fn tool_use_start(index: u32, id: &str, input: serde_json::Value) -> MessageStreamEvent {
    MessageStreamEvent::ContentBlockStart {
        index,
        content_block: ContentBlock::ToolUse {
            id: id.into(),
            name: "do_thing".into(),
            input,
            cache_control: None,
        },
    }
}

fn message_delta_with_stop(stop: messages::StopReason) -> MessageStreamEvent {
    MessageStreamEvent::MessageDelta {
        delta: MessageDeltaBody {
            stop_reason: Some(stop),
            stop_details: None,
            stop_sequence: None,
        },
        usage: MessageDeltaUsage {
            output_tokens: 5,
            input_tokens: Some(10),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        },
    }
}

/// A refusal `message_delta` carrying a provider `stop_details.explanation`,
/// mirroring the Anthropic Messages API ToS auto-refusal wire shape.
fn message_delta_refusal_with_explanation(explanation: &str) -> MessageStreamEvent {
    MessageStreamEvent::MessageDelta {
        delta: MessageDeltaBody {
            stop_reason: Some(messages::StopReason::Refusal),
            stop_details: Some(messages::StopDetails {
                r#type: Some("refusal".to_string()),
                category: Some("frontier_llm".to_string()),
                explanation: Some(explanation.to_string()),
            }),
            stop_sequence: None,
        },
        usage: MessageDeltaUsage {
            output_tokens: 0,
            input_tokens: Some(10),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        },
    }
}

async fn collect(s: impl Stream<Item = SamplingEvent>) -> Vec<SamplingEvent> {
    let mut out = Vec::new();
    let mut s = pin!(s);
    while let Some(ev) = s.next().await {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn empty_stream_yields_started_then_completed() {
    let raw = stream::iter(Vec::<Result<MessageStreamEvent, SamplingError>>::new()).boxed();
    let events = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
    assert!(matches!(events[1], SamplingEvent::Completed { .. }));
}

#[tokio::test]
async fn text_block_assembles_into_completed_response() {
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(text_block_start(0)),
        Ok(text_delta(0, "Hello, ")),
        Ok(text_delta(0, "world!")),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::EndTurn)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    let text_tokens: Vec<&str> = evs
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

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            let a = response.assistant().expect("assistant item present");
            assert_eq!(a.content.as_ref(), "Hello, world!");
            assert_eq!(a.model_id.as_deref(), Some("messages-compatible-model"));
            assert_eq!(response.stop_reason, Some(StopReason::Stop));
            // Provider message id and the verbatim wire stop reason survive
            // onto the response (collapsed `stop_reason` loses the string).
            assert_eq!(response.message_id.as_deref(), Some("msg_1"));
            assert_eq!(response.raw_stop_reason.as_deref(), Some("end_turn"));
            let u = response.usage.as_ref().expect("usage extracted");
            assert_eq!(u.prompt_tokens, 10);
            assert_eq!(u.completion_tokens, 5);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn thinking_block_emits_reasoning_channel_and_preserved_in_response() {
    let thinking_start = MessageStreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlock::Thinking {
            thinking: String::new(),
            signature: String::new(),
        },
    };
    let thinking_delta = MessageStreamEvent::ContentBlockDelta {
        index: 0,
        delta: StreamDelta::ThinkingDelta {
            thinking: "let me think...".into(),
        },
    };
    let sig_delta = MessageStreamEvent::ContentBlockDelta {
        index: 0,
        delta: StreamDelta::SignatureDelta {
            signature: "abc123".into(),
        },
    };
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(thinking_start),
        Ok(thinking_delta),
        Ok(sig_delta),
        Ok(block_stop(0)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    let reasoning_tokens: Vec<&str> = evs
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
    assert_eq!(reasoning_tokens, vec!["let me think..."]);

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            let r = response
                .reasoning_items()
                .next()
                .expect("reasoning sibling preserved");
            let rs::SummaryPart::SummaryText(t) = &r.summary[0];
            assert_eq!(t.text, "let me think...");
            assert_eq!(r.encrypted_content.as_deref(), Some("abc123"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// `thinking(sig1) → text → thinking(sig2)` must surface each thinking block's
/// OWN signature, in order, on its own `ReasoningCompleted` (emitted at that
/// block's stop) — so the per-index signature reaches the headless reducer and
/// each block keeps its own signature rather than collapsing to one.
#[tokio::test]
async fn multiple_thinking_blocks_emit_per_block_signatures_in_order() {
    let thinking_block = |index: u32, text: &str, sig: &str| {
        vec![
            Ok(MessageStreamEvent::ContentBlockStart {
                index,
                content_block: ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: String::new(),
                },
            }),
            Ok(MessageStreamEvent::ContentBlockDelta {
                index,
                delta: StreamDelta::ThinkingDelta {
                    thinking: text.into(),
                },
            }),
            Ok(MessageStreamEvent::ContentBlockDelta {
                index,
                delta: StreamDelta::SignatureDelta {
                    signature: sig.into(),
                },
            }),
            Ok(block_stop(index)),
        ]
    };
    let mut events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![Ok(message_start())];
    events.extend(thinking_block(0, "first", "sig-1"));
    events.push(Ok(text_block_start(1)));
    events.push(Ok(text_delta(1, "interlude")));
    events.push(Ok(block_stop(1)));
    events.extend(thinking_block(2, "second", "sig-2"));
    events.push(Ok(MessageStreamEvent::MessageStop));

    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    let sigs: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ReasoningCompleted { signature, .. } => Some(signature.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sigs,
        vec!["sig-1", "sig-2"],
        "each thinking block emits its own signature in order"
    );
}

#[tokio::test]
async fn tool_use_block_assembles_into_tool_call() {
    let tool_start = MessageStreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlock::ToolUse {
            id: "call_xyz".into(),
            name: "do_thing".into(),
            input: serde_json::json!({}),
            // Set: a parser matching only the absent case must fail here.
            cache_control: Some(xai_grok_sampling_types::messages::CacheControl::ephemeral()),
        },
    };
    let arg_delta_1 = MessageStreamEvent::ContentBlockDelta {
        index: 0,
        delta: StreamDelta::InputJsonDelta {
            partial_json: "{\"x\":".into(),
        },
    };
    let arg_delta_2 = MessageStreamEvent::ContentBlockDelta {
        index: 0,
        delta: StreamDelta::InputJsonDelta {
            partial_json: "1}".into(),
        },
    };
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(tool_start),
        Ok(arg_delta_1),
        Ok(arg_delta_2),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::ToolUse)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    // Should yield three ToolCallDelta events: id+name, then two
    // arguments fragments.
    let deltas: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ToolCallDelta {
                tool_index,
                id,
                name,
                arguments_delta,
                ..
            } => Some((
                *tool_index,
                id.clone(),
                name.clone(),
                arguments_delta.clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 3);
    assert_eq!(deltas[0].0, 0);
    assert_eq!(deltas[0].1.as_deref(), Some("call_xyz"));
    assert_eq!(deltas[0].2.as_deref(), Some("do_thing"));
    assert_eq!(deltas[0].3, None);
    assert_eq!(deltas[1].3.as_deref(), Some("{\"x\":"));
    assert_eq!(deltas[2].3.as_deref(), Some("1}"));

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            let calls = response.tool_calls();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id.as_ref(), "call_xyz");
            assert_eq!(calls[0].name, "do_thing");
            assert_eq!(calls[0].arguments.as_ref(), "{\"x\":1}");
            assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// Regression: a stream whose terminal `message_delta` carries
/// `stop_reason: "refusal"` must complete cleanly — not error out and
/// discard the already-streamed response.
#[tokio::test]
async fn refusal_stop_reason_completes_stream() {
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(text_block_start(0)),
        Ok(text_delta(0, "I can't help with that.")),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::Refusal)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    assert!(
        !evs.iter()
            .any(|e| matches!(e, SamplingEvent::Failed { .. })),
        "refusal stream must not yield Failed: {evs:?}"
    );
    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            let a = response.assistant().expect("assistant item present");
            assert_eq!(a.content.as_ref(), "I can't help with that.");
            assert_eq!(response.stop_reason, Some(StopReason::ContentFilter));
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// A refusal `stop_details.explanation` on the terminal delta must be
/// normalized onto the completed `ConversationResponse.stop_message` so the
/// agent loop can surface the provider's reason (empty-turn silence otherwise).
#[tokio::test]
async fn refusal_stop_message_flows_to_response() {
    let explanation = "This request was blocked by the provider's content policy.";
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(message_delta_refusal_with_explanation(explanation)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(response.stop_reason, Some(StopReason::ContentFilter));
            assert_eq!(
                response.stop_message.as_deref(),
                Some(explanation),
                "provider explanation normalized onto stop_message"
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn pause_turn_and_unknown_stop_reasons_complete_as_stop() {
    for stop in [
        messages::StopReason::PauseTurn,
        messages::StopReason::Unknown("mystery_reason".to_string()),
    ] {
        let label = format!("{stop:?}");
        let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
            Ok(message_start()),
            Ok(text_block_start(0)),
            Ok(text_delta(0, "partial answer")),
            Ok(block_stop(0)),
            Ok(message_delta_with_stop(stop)),
            Ok(MessageStreamEvent::MessageStop),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;
        match evs.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(
                    response.stop_reason,
                    Some(StopReason::Stop),
                    "{label} must end the turn like stop"
                );
            }
            other => panic!("{label}: expected Completed, got {other:?}"),
        }
    }
}

/// Pins the model_context_window_exceeded decision: it stays in the
/// max_tokens truncation class (fatal, non-retryable), not the
/// context-length Api class.
#[tokio::test]
async fn max_tokens_text_only_completes_with_length_stop() {
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(text_block_start(0)),
        Ok(text_delta(0, "cut answ")),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::MaxTokens)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(response.stop_reason, Some(StopReason::Length));
            assert_eq!(response.assistant_text(), "cut answ");
        }
        other => panic!("expected Completed(Length), got {other:?}"),
    }
}

/// A max_tokens stop carrying a completed tool_use block keeps `stop_reason=Length`.
/// The ToolCalls override must not mask the truncation: the block's arguments may be a silently-truncated prefix.
#[tokio::test]
async fn max_tokens_with_tool_use_keeps_length_stop() {
    let tool_start = MessageStreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlock::ToolUse {
            id: "call_cut".into(),
            name: "do_thing".into(),
            input: serde_json::json!({}),
            cache_control: None,
        },
    };
    let arg_delta = MessageStreamEvent::ContentBlockDelta {
        index: 0,
        delta: StreamDelta::InputJsonDelta {
            partial_json: "{\"x\": \"trunc".into(),
        },
    };
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(tool_start),
        Ok(arg_delta),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::MaxTokens)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(response.stop_reason, Some(StopReason::Length));
            assert_eq!(response.tool_calls().len(), 1, "tool call still carried");
        }
        other => panic!("expected Completed(Length), got {other:?}"),
    }
}

/// A tool_use block closed with zero argument deltas collects as an empty-arguments tool call.
/// That is the shape `LengthPolicy::verdict` salvages.
#[tokio::test]
async fn max_tokens_tool_use_without_arg_deltas_collects_empty_arguments() {
    let tool_start = MessageStreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlock::ToolUse {
            id: "call_no_args".into(),
            name: "do_thing".into(),
            input: serde_json::json!({}),
            cache_control: None,
        },
    };
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(tool_start),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::MaxTokens)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(response.stop_reason, Some(StopReason::Length));
            assert_eq!(response.tool_calls().len(), 1);
            assert_eq!(response.tool_calls()[0].arguments.as_ref(), "");
        }
        other => panic!("expected Completed(Length), got {other:?}"),
    }
}

/// Pins the model_context_window_exceeded decision: it maps to the Length stop class and COMPLETES with the partial preserved.
/// Fail-vs-salvage belongs to `drive_l2`, not this transform.
#[tokio::test]
async fn model_context_window_exceeded_completes_with_length_stop() {
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(text_block_start(0)),
        Ok(text_delta(0, "truncated answ")),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(
            messages::StopReason::ModelContextWindowExceeded,
        )),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(response.stop_reason, Some(StopReason::Length));
            assert_eq!(
                response.assistant_text(),
                "truncated answ",
                "partial content must be preserved"
            );
        }
        other => panic!("expected Completed(Length), got {other:?}"),
    }
}

#[tokio::test]
async fn model_context_window_exceeded_fails_as_max_tokens_truncation() {
    let events = vec![
        Ok(message_start()),
        Ok(text_block_start(0)),
        Ok(text_delta(0, "truncated answ")),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(
            messages::StopReason::ModelContextWindowExceeded,
        )),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let events = collect(stream_messages(
        stream::iter(events).boxed(),
        None,
        rid(),
        Duration::from_secs(60),
    ))
    .await;
    let Some(SamplingEvent::Completed { response, .. }) = events.into_iter().last() else {
        panic!("stream must preserve the partial response before applying policy");
    };
    let error =
        crate::client::apply_length_policy(xai_grok_sampling_types::LengthPolicy::Fail, *response)
            .expect_err("legacy failure policy remains deterministic");
    assert!(matches!(error, SamplingError::MaxTokensTruncation));
    assert!(!error.is_retryable());
}

/// Pins the pre-existing override: completed tool_use blocks beat a terminal
/// Refusal, so the agent loop still resolves the calls.
#[tokio::test]
async fn refusal_after_tool_use_blocks_keeps_tool_calls_stop_reason() {
    let tool_start = MessageStreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlock::ToolUse {
            id: "call_refused".into(),
            name: "do_thing".into(),
            input: serde_json::json!({}),
            cache_control: None,
        },
    };
    let arg_delta = MessageStreamEvent::ContentBlockDelta {
        index: 0,
        delta: StreamDelta::InputJsonDelta {
            partial_json: "{}".into(),
        },
    };
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(tool_start),
        Ok(arg_delta),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::Refusal)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    match evs.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(response.tool_calls().len(), 1);
            assert_eq!(
                response.stop_reason,
                Some(StopReason::ToolCalls),
                "tool_use blocks must win over the refusal stop_reason"
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn server_error_event_yields_failed_500() {
    let err_event = MessageStreamEvent::Error {
        error: StreamError {
            r#type: "overloaded_error".into(),
            message: "rate limit hit".into(),
        },
    };
    let raw = stream::iter(vec![Ok(message_start()), Ok(err_event)]).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    match evs.last().unwrap() {
        SamplingEvent::Failed { error, .. } => {
            assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
            assert_eq!(error.status_code, Some(500));
            assert!(error.message.contains("overloaded_error"));
            // Messages error events have no code slot; a code appearing here
            // would make typed events eligible for a destructive image strip.
            assert_eq!(error.error_code, None);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn mid_stream_transport_error_yields_failed() {
    let raw = stream::iter(vec![
        Ok(message_start()),
        Err(SamplingError::EventStreamError("conn reset".into())),
    ])
    .boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;
    assert!(
        evs.iter()
            .any(|e| matches!(e, SamplingEvent::Failed { .. }))
    );
    assert!(
        !evs.iter()
            .any(|e| matches!(e, SamplingEvent::Completed { .. }))
    );
}

#[tokio::test(start_paused = true)]
async fn idle_timeout_when_stream_stalls() {
    let raw = stream::iter(vec![Ok(message_start())])
        .chain(stream::pending())
        .boxed();
    let evs = collect(stream_messages(
        raw,
        None,
        rid(),
        Duration::from_millis(100),
    ))
    .await;

    match evs.last().unwrap() {
        SamplingEvent::Failed { error, .. } => {
            assert_eq!(error.kind, crate::events::SamplingErrorKind::IdleTimeout);
        }
        other => panic!("expected Failed(IdleTimeout), got {other:?}"),
    }
}

#[tokio::test]
async fn model_metadata_yielded_after_stream_started() {
    let raw = stream::iter(vec![Ok(MessageStreamEvent::MessageStop)]).boxed();
    let metadata = ResponseModelMetadata {
        context_window: Some(200_000),
        ..Default::default()
    };
    let evs = collect(stream_messages(
        raw,
        Some(metadata),
        rid(),
        Duration::from_secs(60),
    ))
    .await;

    assert!(matches!(evs[0], SamplingEvent::StreamStarted { .. }));
    assert!(matches!(evs[1], SamplingEvent::ModelMetadata { .. }));
}

#[test]
fn meaningful_content_classifier_treats_ping_as_keepalive() {
    assert!(!messages_event_has_meaningful_content(
        &MessageStreamEvent::Ping
    ));
    assert!(messages_event_has_meaningful_content(
        &MessageStreamEvent::MessageStop
    ));
}

// ── Token usage: Anthropic Messages API cache-bucket accounting ────────────

fn message_start_with_cache(
    input: u32,
    cache_read: u32,
    cache_creation: u32,
) -> MessageStreamEvent {
    MessageStreamEvent::MessageStart {
        message: MessagesResponse {
            id: "msg_cache".into(),
            r#type: "message".into(),
            role: "assistant".into(),
            content: vec![],
            model: "messages-compatible-model".into(),
            stop_reason: None,
            usage: MessagesUsage {
                input_tokens: input,
                output_tokens: 0,
                cache_creation_input_tokens: cache_creation,
                cache_read_input_tokens: cache_read,
            },
        },
    }
}

fn message_delta_with_cache(
    output: u32,
    input: Option<u32>,
    cache_read: Option<u32>,
    cache_creation: Option<u32>,
) -> MessageStreamEvent {
    MessageStreamEvent::MessageDelta {
        delta: MessageDeltaBody {
            stop_reason: Some(messages::StopReason::EndTurn),
            stop_details: None,
            stop_sequence: None,
        },
        usage: MessageDeltaUsage {
            output_tokens: output,
            input_tokens: input,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_creation,
        },
    }
}

/// Helper: drive a minimal stream with the supplied usage events and
/// pluck the `TokenUsage` out of the terminal `Completed` event.
async fn usage_from_stream(events: Vec<MessageStreamEvent>) -> TokenUsage {
    let raw = stream::iter(
        events
            .into_iter()
            .map(Ok::<_, SamplingError>)
            .collect::<Vec<_>>(),
    )
    .boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;
    match evs.last().expect("at least one event") {
        SamplingEvent::Completed { response, .. } => response
            .usage
            .clone()
            .expect("usage should be emitted when prompt or output tokens > 0"),
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn prompt_tokens_sums_all_three_anthropic_buckets() {
    // prompt_tokens = uncached + cache_read + cache_creation;
    // cached_prompt_tokens = cache_read only (writes aren't a hit).
    let usage = usage_from_stream(vec![
        message_start_with_cache(100, 5000, 200),
        text_block_start(0),
        text_delta(0, "ok"),
        block_stop(0),
        message_delta_with_cache(7, None, None, None),
        MessageStreamEvent::MessageStop,
    ])
    .await;

    assert_eq!(usage.prompt_tokens, 100 + 5000 + 200);
    assert_eq!(usage.cached_prompt_tokens, 5000);
    assert_eq!(usage.cache_creation_prompt_tokens, 200);
    assert_eq!(usage.completion_tokens, 7);
    assert_eq!(usage.total_tokens, 100 + 5000 + 200 + 7);
}

#[tokio::test]
async fn message_delta_cache_fields_override_message_start() {
    // Providers can report zero cache at message_start and emit the real
    // values on the final delta; honor the delta when present.
    let usage = usage_from_stream(vec![
        message_start_with_cache(10, 0, 0),
        message_delta_with_cache(4, Some(10), Some(900), Some(50)),
        MessageStreamEvent::MessageStop,
    ])
    .await;

    assert_eq!(usage.prompt_tokens, 10 + 900 + 50);
    assert_eq!(usage.cached_prompt_tokens, 900);
    assert_eq!(usage.cache_creation_prompt_tokens, 50);
    assert_eq!(usage.completion_tokens, 4);
}

#[tokio::test]
async fn pure_cache_hit_with_zero_uncached_still_emits_usage() {
    // 100% cache hit: Anthropic Messages API reports input_tokens=0 with cache_read>0.
    // The emit-guard must still fire so callers see the cached cost.
    let usage = usage_from_stream(vec![
        message_start_with_cache(0, 2500, 0),
        message_delta_with_cache(1, None, None, None),
        MessageStreamEvent::MessageStop,
    ])
    .await;

    assert_eq!(usage.prompt_tokens, 2500);
    assert_eq!(usage.cached_prompt_tokens, 2500);
    assert_eq!(usage.total_tokens, 2501);
}

#[tokio::test]
async fn tool_use_block_stop_emits_arguments_complete_once() {
    let tool_start = MessageStreamEvent::ContentBlockStart {
        index: 0,
        content_block: ContentBlock::ToolUse {
            id: "call_c".into(),
            name: "exec".into(),
            input: serde_json::json!({}),
            cache_control: None,
        },
    };
    let arg_delta = MessageStreamEvent::ContentBlockDelta {
        index: 0,
        delta: StreamDelta::InputJsonDelta {
            partial_json: "{\"source\":\"return 1\"}".into(),
        },
    };
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(tool_start),
        Ok(arg_delta),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::ToolUse)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    let completes: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ToolCallArgumentsComplete {
                tool_index,
                id,
                name,
                ..
            } => Some((*tool_index, id.clone(), name.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(completes.len(), 1, "exactly one completion per block");
    assert_eq!(completes[0].0, 0);
    assert_eq!(completes[0].1.as_deref(), Some("call_c"));
    assert_eq!(completes[0].2.as_deref(), Some("exec"));

    // The completion lands after the last fragment and before Completed.
    let complete_pos = evs
        .iter()
        .position(|e| matches!(e, SamplingEvent::ToolCallArgumentsComplete { .. }))
        .unwrap();
    let last_delta_pos = evs
        .iter()
        .rposition(|e| matches!(e, SamplingEvent::ToolCallDelta { .. }))
        .unwrap();
    let completed_pos = evs
        .iter()
        .position(|e| matches!(e, SamplingEvent::Completed { .. }))
        .unwrap();
    assert!(last_delta_pos < complete_pos && complete_pos < completed_pos);
}

#[tokio::test]
async fn two_tool_use_blocks_emit_two_completions() {
    let tool_start = |index: u32, id: &str| MessageStreamEvent::ContentBlockStart {
        index,
        content_block: ContentBlock::ToolUse {
            id: id.into(),
            name: "do_thing".into(),
            input: serde_json::json!({}),
            cache_control: None,
        },
    };
    let events: Vec<Result<MessageStreamEvent, SamplingError>> = vec![
        Ok(message_start()),
        Ok(tool_start(1, "call_a")),
        Ok(MessageStreamEvent::ContentBlockDelta {
            index: 1,
            delta: StreamDelta::InputJsonDelta {
                partial_json: "{}".into(),
            },
        }),
        Ok(block_stop(1)),
        Ok(tool_start(3, "call_b")),
        Ok(block_stop(3)),
        Ok(message_delta_with_stop(messages::StopReason::ToolUse)),
        Ok(MessageStreamEvent::MessageStop),
    ];
    let raw = stream::iter(events).boxed();
    let evs = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    let mut indexes: Vec<u32> = evs
        .iter()
        .filter_map(|e| match e {
            SamplingEvent::ToolCallArgumentsComplete { tool_index, .. } => Some(*tool_index),
            _ => None,
        })
        .collect();
    indexes.sort_unstable();
    assert_eq!(indexes, vec![0, 1], "one completion per tool call");
}

#[tokio::test]
async fn empty_tool_use_input_emits_valid_object_before_completion() {
    let raw = stream::iter(vec![
        Ok(message_start()),
        Ok(MessageStreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolUse {
                id: "call_empty".into(),
                name: "empty_tool".into(),
                input: serde_json::json!({}),
                cache_control: None,
            },
        }),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::ToolUse)),
        Ok(MessageStreamEvent::MessageStop),
    ])
    .boxed();
    let events = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    let arguments_position = events
        .iter()
        .position(|event| {
            matches!(
                event,
                SamplingEvent::ToolCallDelta {
                    arguments_delta: Some(arguments),
                    ..
                } if arguments == "{}"
            )
        })
        .unwrap();
    let completion_position = events
        .iter()
        .position(|event| matches!(event, SamplingEvent::ToolCallArgumentsComplete { .. }))
        .unwrap();
    assert!(arguments_position < completion_position);

    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(response.tool_calls()[0].arguments.as_ref(), "{}");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn populated_tool_use_start_preserves_input_without_deltas() {
    let raw = stream::iter(vec![
        Ok(message_start()),
        Ok(MessageStreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolUse {
                id: "call_initial".into(),
                name: "do_thing".into(),
                input: serde_json::json!({"value": 7}),
                cache_control: None,
            },
        }),
        Ok(block_stop(0)),
        Ok(message_delta_with_stop(messages::StopReason::ToolUse)),
        Ok(MessageStreamEvent::MessageStop),
    ])
    .boxed();
    let events = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    let fragments: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SamplingEvent::ToolCallDelta {
                arguments_delta: Some(arguments),
                ..
            } => Some(arguments.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(fragments, ["{\"value\":7}"]);
    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(response.tool_calls()[0].arguments.as_ref(), "{\"value\":7}");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn interleaved_tool_blocks_preserve_call_indexes_and_canonical_order() {
    let tool_start = |index: u32, id: &str| MessageStreamEvent::ContentBlockStart {
        index,
        content_block: ContentBlock::ToolUse {
            id: id.into(),
            name: "do_thing".into(),
            input: serde_json::json!({}),
            cache_control: None,
        },
    };
    let argument_delta = |index: u32, arguments: &str| MessageStreamEvent::ContentBlockDelta {
        index,
        delta: StreamDelta::InputJsonDelta {
            partial_json: arguments.into(),
        },
    };
    let raw = stream::iter(vec![
        Ok(message_start()),
        Ok(tool_start(2, "call_first")),
        Ok(argument_delta(2, "{\"first\":")),
        Ok(tool_start(5, "call_second")),
        Ok(argument_delta(5, "{\"second\":2}")),
        Ok(block_stop(5)),
        Ok(argument_delta(2, "1}")),
        Ok(block_stop(2)),
        Ok(message_delta_with_stop(messages::StopReason::ToolUse)),
        Ok(MessageStreamEvent::MessageStop),
    ])
    .boxed();
    let events = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    let completion_indexes: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SamplingEvent::ToolCallArgumentsComplete { tool_index, .. } => Some(*tool_index),
            _ => None,
        })
        .collect();
    assert_eq!(completion_indexes, [1, 0]);
    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            let calls = response.tool_calls();
            assert_eq!(calls[0].id.as_ref(), "call_first");
            assert_eq!(calls[0].arguments.as_ref(), "{\"first\":1}");
            assert_eq!(calls[1].id.as_ref(), "call_second");
            assert_eq!(calls[1].arguments.as_ref(), "{\"second\":2}");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn interrupted_tool_use_block_never_emits_arguments_completion() {
    let raw = stream::iter(vec![
        Ok(message_start()),
        Ok(MessageStreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolUse {
                id: "call_interrupted".into(),
                name: "do_thing".into(),
                input: serde_json::json!({}),
                cache_control: None,
            },
        }),
        Ok(MessageStreamEvent::ContentBlockDelta {
            index: 0,
            delta: StreamDelta::InputJsonDelta {
                partial_json: "{\"value\":".into(),
            },
        }),
        Err(SamplingError::EventStreamError("connection closed".into())),
    ])
    .boxed();
    let events = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;

    assert!(
        !events
            .iter()
            .any(|event| { matches!(event, SamplingEvent::ToolCallArgumentsComplete { .. }) })
    );
    assert!(matches!(events.last(), Some(SamplingEvent::Failed { .. })));
}

#[tokio::test]
async fn ambiguous_empty_tool_waits_for_known_non_length_stop() {
    for (stop, expected_arguments, expected_stop) in [
        (
            Some(messages::StopReason::ToolUse),
            "{}",
            StopReason::ToolCalls,
        ),
        (
            Some(messages::StopReason::EndTurn),
            "{}",
            StopReason::ToolCalls,
        ),
        (
            Some(messages::StopReason::MaxTokens),
            "",
            StopReason::Length,
        ),
        (
            Some(messages::StopReason::ModelContextWindowExceeded),
            "",
            StopReason::Length,
        ),
        (None, "", StopReason::ToolCalls),
    ] {
        let stop_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = stop_observed.clone();
        let mut raw_events = vec![
            Ok(message_start()),
            Ok(tool_use_start(0, "call_empty", serde_json::json!({}))),
            Ok(block_stop(0)),
        ];
        if let Some(stop) = stop {
            raw_events.push(Ok(message_delta_with_stop(stop)));
        }
        raw_events.push(Ok(MessageStreamEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: None,
                stop_details: None,
                stop_sequence: None,
            },
            usage: MessageDeltaUsage {
                output_tokens: 7,
                input_tokens: None,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            },
        }));
        raw_events.push(Ok(MessageStreamEvent::MessageStop));
        let raw = stream::iter(raw_events)
            .inspect(move |event| {
                if matches!(event, Ok(MessageStreamEvent::MessageDelta { delta, .. }) if delta.stop_reason.is_some()) {
                    observed.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            })
            .boxed();
        let mut transformed = pin!(stream_messages(raw, None, rid(), Duration::from_secs(60)));
        let mut events = Vec::new();
        while let Some(event) = transformed.next().await {
            if matches!(
                &event,
                SamplingEvent::ToolCallArgumentsComplete { .. }
                    | SamplingEvent::ToolCallDelta {
                        arguments_delta: Some(_),
                        ..
                    }
            ) {
                assert!(stop_observed.load(std::sync::atomic::Ordering::Relaxed));
            }
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, SamplingEvent::ToolCallArgumentsComplete { .. }))
                .count(),
            usize::from(expected_arguments == "{}"),
        );
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(expected_stop));
                assert_eq!(
                    response.tool_calls()[0].arguments.as_ref(),
                    expected_arguments
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn closed_empty_tool_followed_by_error_never_completes() {
    let raw = stream::iter(vec![
        Ok(message_start()),
        Ok(tool_use_start(0, "call_empty", serde_json::json!({}))),
        Ok(block_stop(0)),
        Err(SamplingError::EventStreamError("connection closed".into())),
    ])
    .boxed();
    let events = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;
    assert!(!events.iter().any(|event| matches!(
        event,
        SamplingEvent::ToolCallArgumentsComplete { .. }
            | SamplingEvent::ToolCallDelta {
                arguments_delta: Some(_),
                ..
            }
    )));
    assert!(matches!(events.last(), Some(SamplingEvent::Failed { .. })));
}

#[tokio::test]
async fn max_tokens_tool_completion_requires_complete_json() {
    use xai_grok_sampling_types::{LengthPolicy, LengthVerdict};

    for (arguments, complete) in [
        ("{}", true),
        ("{\"value\":7}", true),
        ("{\"value\":", false),
    ] {
        let stop_observed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = stop_observed.clone();
        let raw = stream::iter(vec![
            Ok(message_start()),
            Ok(tool_use_start(0, "call_json", serde_json::json!({}))),
            Ok(MessageStreamEvent::ContentBlockDelta {
                index: 0,
                delta: StreamDelta::InputJsonDelta {
                    partial_json: arguments.into(),
                },
            }),
            Ok(block_stop(0)),
            Ok(message_delta_with_stop(messages::StopReason::MaxTokens)),
            Ok(MessageStreamEvent::MessageStop),
        ])
        .inspect(move |event| {
            if matches!(event, Ok(MessageStreamEvent::MessageDelta { .. })) {
                observed.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .boxed();
        let mut transformed = pin!(stream_messages(raw, None, rid(), Duration::from_secs(60)));
        let mut completions = 0;
        let mut completed = false;
        while let Some(event) = transformed.next().await {
            match event {
                SamplingEvent::ToolCallArgumentsComplete { .. } => {
                    completions += 1;
                    assert!(complete);
                    assert!(!stop_observed.load(std::sync::atomic::Ordering::Relaxed));
                }
                SamplingEvent::Completed { response, .. } => {
                    completed = true;
                    assert_eq!(response.stop_reason, Some(StopReason::Length));
                    assert_eq!(response.tool_calls()[0].arguments.as_ref(), arguments);
                    assert_eq!(
                        LengthPolicy::CompleteToolCalls.verdict(&response),
                        if complete {
                            LengthVerdict::SalvageToolCalls
                        } else {
                            LengthVerdict::Fail
                        },
                    );
                }
                SamplingEvent::Failed { error, .. } => panic!("unexpected failure: {error:?}"),
                _ => {}
            }
        }
        assert!(completed);
        assert_eq!(completions, usize::from(complete));
    }
}

#[tokio::test]
async fn deferred_empty_tools_preserve_canonical_order() {
    for (stop, expected_empty_arguments, expected_completions) in [
        (messages::StopReason::ToolUse, "{}", vec![1, 0]),
        (messages::StopReason::MaxTokens, "", vec![1]),
    ] {
        let raw = stream::iter(vec![
            Ok(message_start()),
            Ok(tool_use_start(2, "call_first", serde_json::json!({}))),
            Ok(tool_use_start(
                5,
                "call_second",
                serde_json::json!({"value": 7}),
            )),
            Ok(block_stop(5)),
            Ok(block_stop(2)),
            Ok(message_delta_with_stop(stop)),
            Ok(MessageStreamEvent::MessageStop),
        ])
        .boxed();
        let events = collect(stream_messages(raw, None, rid(), Duration::from_secs(60))).await;
        let completion_indexes: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                SamplingEvent::ToolCallArgumentsComplete { tool_index, .. } => Some(*tool_index),
                _ => None,
            })
            .collect();
        assert_eq!(completion_indexes, expected_completions);
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let calls = response.tool_calls();
                assert_eq!(calls[0].id.as_ref(), "call_first");
                assert_eq!(calls[0].arguments.as_ref(), expected_empty_arguments);
                assert_eq!(calls[1].id.as_ref(), "call_second");
                assert_eq!(calls[1].arguments.as_ref(), "{\"value\":7}");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
