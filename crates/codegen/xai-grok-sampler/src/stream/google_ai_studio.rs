//! Layer-2 stream transform for Google AI Studio API (`models/{model}:streamGenerateContent?alt=sse`).
//!
//! Consumes a raw `GenerateContentResponse` stream and produces [`SamplingEvent`]s. Pure: no I/O.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};

use xai_grok_sampling_types::google_ai_studio::{FinishReason, GenerateContentResponse};
use xai_grok_sampling_types::{
    AssistantItem, ConversationItem, ConversationResponse, ResponseModelMetadata, SamplingError,
    StopReason, TokenUsage, ToolCall, rs,
};

use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::types::RequestId;

/// Transform a raw Google AI Studio API stream into a stream of [`SamplingEvent`]s.
pub fn stream_google_ai_studio<'a>(
    raw_stream: BoxStream<'a, Result<GenerateContentResponse, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    async_stream::stream! {
        let stream_start = Instant::now();
        let mut chunk_timestamps: Vec<Instant> = Vec::new();

        yield SamplingEvent::StreamStarted {
            request_id: request_id.clone(),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        if let Some(metadata) = model_metadata {
            yield SamplingEvent::ModelMetadata {
                request_id: request_id.clone(),
                metadata,
            };
        }

        let mut final_model: Option<String> = None;
        let mut final_prompt_tokens: u32 = 0;
        let mut final_completion_tokens: u32 = 0;
        let mut final_total_tokens: u32 = 0;
        let mut final_cached_tokens: u32 = 0;

        let mut final_stop_reason: Option<StopReason> = None;
        let mut final_stop_message: Option<String> = None;
        let mut final_raw_stop_reason: Option<String> = None;

        let mut assistant_text = String::new();
        let mut thinking_text = String::new();
        let mut assistant_tool_calls: BTreeMap<u32, ToolCall> = BTreeMap::new();
        let mut next_tool_index: u32 = 0;

        let mut chunk_index: u64 = 0;
        let mut message_chunk_count: u64 = 0;
        let mut first_token_emitted = false;
        let mut response_started_emitted = false;

        let mut stream = raw_stream;
        loop {
            let chunk_result = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(res)) => res,
                Ok(None) => break,
                Err(_elapsed) => {
                    let err = SamplingError::IdleTimeout {
                        elapsed_secs: idle_timeout.as_secs(),
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            let resp = match chunk_result {
                Ok(chunk) => chunk,
                Err(err) => {
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            chunk_timestamps.push(Instant::now());

            if let Some(m) = &resp.model_version {
                if final_model.is_none() {
                    final_model = Some(m.clone());
                }
            }

            if let Some(u) = &resp.usage_metadata {
                if let Some(p) = u.prompt_token_count {
                    final_prompt_tokens = p;
                }
                if let Some(c) = u.candidates_token_count {
                    final_completion_tokens = c;
                }
                if let Some(t) = u.total_token_count {
                    final_total_tokens = t;
                }
                if let Some(cached) = u.cached_content_token_count {
                    final_cached_tokens = cached;
                }
            }

            if !response_started_emitted {
                response_started_emitted = true;
                yield SamplingEvent::ResponseStarted {
                    request_id: request_id.clone(),
                    message_id: format!("google-ai-studio-{}", request_id.as_str()),
                    model: final_model.clone().unwrap_or_default(),
                    input_tokens: u64::from(final_prompt_tokens),
                    cache_read_input_tokens: u64::from(final_cached_tokens),
                    cache_creation_input_tokens: 0,
                };
            }

            for candidate in resp.candidates {
                if let Some(finish_reason) = candidate.finish_reason {
                    let mapped = match finish_reason {
                        FinishReason::Stop => StopReason::Stop,
                        FinishReason::MaxTokens => StopReason::Length,
                        FinishReason::Safety
                        | FinishReason::Blocklist
                        | FinishReason::ProhibitedContent
                        | FinishReason::Spii
                        | FinishReason::ImageSafety => StopReason::ContentFilter,
                        _ => StopReason::Stop,
                    };
                    final_stop_reason = Some(mapped);
                    final_raw_stop_reason = Some(format!("{finish_reason:?}"));
                }
                if let Some(msg) = candidate.finish_message {
                    final_stop_message = Some(msg);
                }

                if let Some(content) = candidate.content {
                    for part in content.parts {
                        if part.thought == Some(true) {
                            if let Some(text) = part.text {
                                thinking_text.push_str(&text);
                                if !first_token_emitted {
                                    first_token_emitted = true;
                                    yield SamplingEvent::FirstToken {
                                        request_id: request_id.clone(),
                                    };
                                }
                                chunk_index += 1;
                                yield SamplingEvent::ChannelToken {
                                    request_id: request_id.clone(),
                                    channel: SamplingChannel::Reasoning,
                                    text,
                                    chunk_index,
                                };
                            }
                        } else if let Some(text) = part.text {
                            assistant_text.push_str(&text);
                            if !first_token_emitted {
                                first_token_emitted = true;
                                yield SamplingEvent::FirstToken {
                                    request_id: request_id.clone(),
                                };
                            }
                            chunk_index += 1;
                            message_chunk_count += 1;
                            yield SamplingEvent::ChannelToken {
                                request_id: request_id.clone(),
                                channel: SamplingChannel::Text,
                                text,
                                chunk_index,
                            };
                        }

                        if let Some(fc) = part.function_call {
                            let tool_idx = next_tool_index;
                            next_tool_index += 1;
                            let call_id = fc.id.unwrap_or_else(|| {
                                format!("call_{}_{}", tool_idx, fc.name)
                            });
                            let args_str = serde_json::to_string(&fc.args).unwrap_or_default();

                            yield SamplingEvent::ToolCallDelta {
                                request_id: request_id.clone(),
                                tool_index: tool_idx,
                                id: Some(call_id.clone()),
                                name: Some(fc.name.clone()),
                                arguments_delta: Some(args_str.clone()),
                            };
                            yield SamplingEvent::ToolCallArgumentsComplete {
                                request_id: request_id.clone(),
                                tool_index: tool_idx,
                                id: Some(call_id.clone()),
                                name: Some(fc.name.clone()),
                            };

                            assistant_tool_calls.insert(
                                tool_idx,
                                ToolCall {
                                    id: Arc::<str>::from(call_id),
                                    name: fc.name,
                                    arguments: Arc::<str>::from(args_str),
                                },
                            );
                        }
                    }
                }
            }
        }

        let mut items = Vec::new();
        if !thinking_text.is_empty() {
            let summary = vec![rs::SummaryPart::SummaryText(
                rs::SummaryTextContent {
                    text: thinking_text,
                },
            )];
            let reasoning = rs::ReasoningItem {
                id: String::new(),
                summary,
                content: None,
                encrypted_content: None,
                status: None,
            };
            items.push(ConversationItem::Reasoning(reasoning));
        }

        let stop_reason = if final_stop_reason == Some(StopReason::Length) {
            final_stop_reason
        } else if !assistant_tool_calls.is_empty() {
            Some(StopReason::ToolCalls)
        } else {
            final_stop_reason.or(Some(StopReason::Stop))
        };

        items.push(ConversationItem::Assistant(AssistantItem {
            content: Arc::<str>::from(assistant_text),
            tool_calls: assistant_tool_calls.into_values().collect(),
            model_id: final_model,
            model_fingerprint: None,
            reasoning_effort: None,
        }));

        let response = ConversationResponse {
            items,
            stop_reason,
            usage: Some(TokenUsage {
                prompt_tokens: final_prompt_tokens,
                completion_tokens: final_completion_tokens,
                total_tokens: final_total_tokens,
                reasoning_tokens: 0,
                cached_prompt_tokens: final_cached_tokens,
                cache_creation_prompt_tokens: 0,
            }),
            cost_usd_ticks: None,
            message_chunks_emitted: message_chunk_count,
            doom_loop_signals: Vec::new(),
            stop_message: final_stop_message,
            message_id: None,
            raw_stop_reason: final_raw_stop_reason,
            stop_sequence: None,
        };

        let stream_end = Instant::now();
        let metrics =
            InferenceLatencyStats::from_timestamps(stream_start, &chunk_timestamps, stream_end);
        yield SamplingEvent::Completed {
            request_id,
            response: Box::new(response),
            metrics,
        };
    }
}

#[cfg(test)]
#[path = "google_ai_studio_tests.rs"]
mod tests;

