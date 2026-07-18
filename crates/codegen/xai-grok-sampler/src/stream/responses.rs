//! Layer-2 stream transform for the OpenAI Responses API.
//!
//! Consumes a raw `rs::ResponseStreamEvent` stream and produces
//! [`SamplingEvent`]s. Pure: no I/O, no shell coupling.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::{BoxStream, Stream};

use xai_grok_sampling_types::{
    ConversationItem, ConversationResponse, ResponseModelMetadata, SamplingError, StopReason,
    TokenUsage, rs,
};

use crate::events::{SamplingChannel, SamplingErrorInfo, SamplingEvent};
use crate::metrics::InferenceLatencyStats;
use crate::stream::display_citations::{DisplayCitationFilter, strip_display_citations_in_items};
use crate::types::RequestId;

/// Returns whether a Responses API event reflects real model progress
/// rather than a liveness-only heartbeat / status transition.
pub(crate) fn responses_event_has_meaningful_content(event: &rs::ResponseStreamEvent) -> bool {
    use rs::ResponseStreamEvent;

    match event {
        ResponseStreamEvent::ResponseCreated(_)
        | ResponseStreamEvent::ResponseInProgress(_)
        | ResponseStreamEvent::ResponseQueued(_) => false,
        ResponseStreamEvent::ResponseOutputTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseOutputTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseRefusalDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseRefusalDone(event) => !event.refusal.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseFunctionCallArgumentsDone(event) => {
            !event.arguments.is_empty() || event.name.as_ref().is_some_and(|name| !name.is_empty())
        }
        ResponseStreamEvent::ResponseReasoningSummaryTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningSummaryTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseReasoningTextDone(event) => !event.text.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseMCPCallArgumentsDone(event) => !event.arguments.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCodeInterpreterCallCodeDone(event) => !event.code.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDelta(event) => !event.delta.is_empty(),
        ResponseStreamEvent::ResponseCustomToolCallInputDone(event) => !event.input.is_empty(),
        ResponseStreamEvent::ResponseCompleted(_)
        | ResponseStreamEvent::ResponseFailed(_)
        | ResponseStreamEvent::ResponseIncomplete(_)
        | ResponseStreamEvent::ResponseOutputItemAdded(_)
        | ResponseStreamEvent::ResponseOutputItemDone(_)
        | ResponseStreamEvent::ResponseContentPartAdded(_)
        | ResponseStreamEvent::ResponseContentPartDone(_)
        | ResponseStreamEvent::ResponseFileSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseFileSearchCallSearching(_)
        | ResponseStreamEvent::ResponseFileSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseWebSearchCallInProgress(_)
        | ResponseStreamEvent::ResponseWebSearchCallSearching(_)
        | ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartAdded(_)
        | ResponseStreamEvent::ResponseReasoningSummaryPartDone(_)
        | ResponseStreamEvent::ResponseImageGenerationCallCompleted(_)
        | ResponseStreamEvent::ResponseImageGenerationCallGenerating(_)
        | ResponseStreamEvent::ResponseImageGenerationCallInProgress(_)
        | ResponseStreamEvent::ResponseImageGenerationCallPartialImage(_)
        | ResponseStreamEvent::ResponseMCPCallCompleted(_)
        | ResponseStreamEvent::ResponseMCPCallFailed(_)
        | ResponseStreamEvent::ResponseMCPCallInProgress(_)
        | ResponseStreamEvent::ResponseMCPListToolsCompleted(_)
        | ResponseStreamEvent::ResponseMCPListToolsFailed(_)
        | ResponseStreamEvent::ResponseMCPListToolsInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInProgress(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallInterpreting(_)
        | ResponseStreamEvent::ResponseCodeInterpreterCallCompleted(_)
        | ResponseStreamEvent::ResponseOutputTextAnnotationAdded(_)
        | ResponseStreamEvent::ResponseError(_) => true,
    }
}

/// Transform a raw Responses API event stream into a stream of
/// [`SamplingEvent`]s.
///
/// Yields exactly one terminal event ([`SamplingEvent::Completed`] or
/// [`SamplingEvent::Failed`]) per request. Server-side `ResponseFailed`
/// and `ResponseError` events are translated to
/// `SamplingError::Api { status: 500, .. }` so the actor's retry loop
/// treats them as retryable.
///
/// `doom_loop` is the collector returned alongside `raw_stream` by
/// `SamplingClient::conversation_stream_responses`; any signals the SSE
/// decoder recorded are drained onto the final `ConversationResponse`.
/// `None` (check disabled) leaves the response untouched.
pub fn stream_responses<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    stream_responses_with_client_custom_tools(
        raw_stream,
        model_metadata,
        request_id,
        idle_timeout,
        doom_loop,
        Vec::new(),
    )
}

/// Responses stream transform with request-scoped custom-tool declarations.
/// The names distinguish client-executed custom calls from backend x_search,
/// which uses the same Responses `CustomToolCall` wire item.
pub fn stream_responses_with_client_custom_tools<'a>(
    raw_stream: BoxStream<'a, Result<rs::ResponseStreamEvent, SamplingError>>,
    model_metadata: Option<ResponseModelMetadata>,
    request_id: RequestId,
    idle_timeout: Duration,
    doom_loop: Option<crate::doom_loop::DoomLoopSignalCollector>,
    client_custom_tool_names: Vec<String>,
) -> impl Stream<Item = SamplingEvent> + Send + 'a {
    async_stream::stream! {
        use rs::{ResponseStreamEvent, Status};

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

        let mut final_response: Option<rs::Response> = None;
        let mut chunk_index: u64 = 0;
        let mut message_chunk_count: u64 = 0;
        let mut first_token_emitted = false;
        let mut reasoning_text_acc = String::new();
        let mut reasoning_summary_parts: BTreeMap<(u32, String, u32), String> = BTreeMap::new();
        // Codex treats `response.output_item.done` as the durable output
        // carrier. Its terminal `response.completed` frame can contain only
        // response metadata and usage, with an empty `response.output`.
        // Retain the finished items so encrypted reasoning, messages, and
        // complete tool calls survive conversion below.
        let mut completed_output_items: BTreeMap<u32, rs::OutputItem> = BTreeMap::new();
        let mut last_content_chunk_at = Instant::now();

        // Maps Responses API `output_index` to our tool-only `tool_index`.
        // Populated when `ResponseOutputItemAdded` carries a `FunctionCall`;
        // later `ResponseFunctionCallArgumentsDelta` events
        // look up `output_index` here to find the matching `tool_index`.
        let mut output_to_tool_index: BTreeMap<u32, u32> = BTreeMap::new();
        let mut custom_input_started: BTreeSet<u32> = BTreeSet::new();
        let mut backend_output_started: BTreeSet<u32> = BTreeSet::new();
        let mut backend_tool_started: BTreeSet<String> = BTreeSet::new();
        let mut next_tool_index: u32 = 0;
        // Hosted web/file search trains models to emit PUA display-citation
        // widgets (`\ue200cite\ue202turnNsearchM\ue201`). Strip them from the
        // live text stream so the TUI never paints raw citation markers.
        let mut display_citation_filter = DisplayCitationFilter::new();

        let mut stream = raw_stream;
        loop {
            let event_result = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(event_result)) => event_result,
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

            let event = match event_result {
                Ok(event) => event,
                Err(err) => {
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }
            };

            // A confident server-detected loop aborts the attempt (dropping
            // the SSE connection) so the retry loop can resample instead of
            // streaming the burning tail. Checked before the event is
            // processed so a terminal frame carrying the signal never
            // becomes the accepted response while the abort is armed.
            if let Some(triggers) = doom_loop.as_ref().and_then(|c| c.abort_triggers()) {
                let err = SamplingError::DoomLoopDetected {
                    triggers,
                    aborted_at_chunk: Some(chunk_index),
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            let event_has_content = responses_event_has_meaningful_content(&event);

            // Track whether ResponseIncomplete should break the loop
            // after the content-aware idle check below.
            let mut should_break = false;

            match event {
                ResponseStreamEvent::ResponseOutputTextDelta(text_delta_event) => {
                    let delta = text_delta_event.delta;
                    if !delta.is_empty() {
                        let visible = display_citation_filter.push(&delta);
                        if !visible.is_empty() {
                            if !first_token_emitted {
                                first_token_emitted = true;
                                yield SamplingEvent::FirstToken {
                                    request_id: request_id.clone(),
                                };
                            }
                            chunk_timestamps.push(Instant::now());
                            chunk_index += 1;
                            message_chunk_count += 1;
                            yield SamplingEvent::ChannelToken {
                                request_id: request_id.clone(),
                                channel: SamplingChannel::Text,
                                text: visible,
                                chunk_index,
                            };
                        }
                    }
                }

                ResponseStreamEvent::ResponseReasoningSummaryTextDelta(summary_event) => {
                    let delta = summary_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        reasoning_summary_parts
                            .entry((
                                summary_event.output_index,
                                summary_event.item_id.clone(),
                                summary_event.summary_index,
                            ))
                            .or_default()
                            .push_str(&delta);
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningSummaryTextDone(summary_event) => {
                    let summary = reasoning_summary_parts
                        .entry((
                            summary_event.output_index,
                            summary_event.item_id,
                            summary_event.summary_index,
                        ))
                        .or_default();
                    // Some Codex deployments send only the terminal text,
                    // while others send deltas followed by the same complete
                    // text. Emit and fill an empty part without duplicating
                    // the latter.
                    if summary.is_empty() && !summary_event.text.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        *summary = summary_event.text.clone();
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: summary_event.text,
                            chunk_index,
                        };
                    }
                }

                ResponseStreamEvent::ResponseReasoningTextDelta(reasoning_event) => {
                    let delta = reasoning_event.delta;
                    if !delta.is_empty() {
                        if !first_token_emitted {
                            first_token_emitted = true;
                            yield SamplingEvent::FirstToken {
                                request_id: request_id.clone(),
                            };
                        }
                        chunk_index += 1;
                        reasoning_text_acc.push_str(&delta);
                        yield SamplingEvent::ChannelToken {
                            request_id: request_id.clone(),
                            channel: SamplingChannel::Reasoning,
                            text: delta,
                            chunk_index,
                        };
                    }
                }

                // Start of a client-executable Responses tool call — emit
                // initial id+name and remember output_index → tool_index.
                ResponseStreamEvent::ResponseOutputItemAdded(added_event) => {
                    match added_event.item {
                        rs::OutputItem::FunctionCall(fc) => {
                            let tool_index = next_tool_index;
                            next_tool_index += 1;
                            output_to_tool_index.insert(added_event.output_index, tool_index);

                            yield SamplingEvent::ToolCallDelta {
                                request_id: request_id.clone(),
                                tool_index,
                                id: Some(fc.call_id),
                                name: Some(fc.name),
                                arguments_delta: None,
                            };
                        }
                        rs::OutputItem::CustomToolCall(ct)
                            if client_custom_tool_names.iter().any(|name| name == &ct.name) =>
                        {
                            let tool_index = next_tool_index;
                            next_tool_index += 1;
                            output_to_tool_index.insert(added_event.output_index, tool_index);
                            let has_initial_input = !ct.input.is_empty();
                            if has_initial_input {
                                custom_input_started.insert(added_event.output_index);
                            }
                            let call = xai_grok_sampling_types::ToolCall::custom(
                                &ct.call_id,
                                &ct.id,
                                &ct.name,
                                ct.input.clone(),
                            );

                            yield SamplingEvent::ToolCallDelta {
                                request_id: request_id.clone(),
                                tool_index,
                                // ACP/UI consumers need the provider call ID;
                                // the opaque ID envelope remains on the final
                                // Conversation ToolCall for lossless replay.
                                id: Some(call.call_id().to_string()),
                                name: Some(ct.name),
                                arguments_delta: has_initial_input.then_some(ct.input),
                            };
                        }
                        rs::OutputItem::CustomToolCall(ct) => {
                            // xAI's current Responses schema returns hosted X
                            // search as `x_search_call`. The transport adapter
                            // normalizes that provider-only item to a backend
                            // CustomToolCall so the rest of the harness can keep
                            // one provider-neutral lifecycle.
                            let was_started = backend_output_started.contains(&added_event.output_index)
                                || backend_tool_started.contains(&ct.id)
                                || backend_tool_started.contains(&ct.call_id);
                            backend_output_started.insert(added_event.output_index);
                            backend_tool_started.insert(ct.id.clone());
                            backend_tool_started.insert(ct.call_id.clone());
                            if !was_started {
                                yield SamplingEvent::BackendToolCallStarted {
                                    request_id: request_id.clone(),
                                    call_id: ct.id,
                                    name: "x_search".to_string(),
                                };
                            }
                        }
                        _ => {}
                    }
                }

                // Continuation chunk for a streaming FunctionCall's args.
                // Drop silently if no preceding OutputItemAdded mapped.
                ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(args_event) => {
                    let delta = args_event.delta;
                    if !delta.is_empty()
                        && let Some(&tool_index) =
                            output_to_tool_index.get(&args_event.output_index)
                    {
                        yield SamplingEvent::ToolCallDelta {
                            request_id: request_id.clone(),
                            tool_index,
                            id: None,
                            name: None,
                            arguments_delta: Some(delta),
                        };
                    }
                }

                // Native custom tools stream raw text rather than JSON args.
                ResponseStreamEvent::ResponseCustomToolCallInputDelta(input_event) => {
                    let delta = input_event.delta;
                    if !delta.is_empty()
                        && let Some(&tool_index) =
                            output_to_tool_index.get(&input_event.output_index)
                    {
                        custom_input_started.insert(input_event.output_index);
                        yield SamplingEvent::ToolCallDelta {
                            request_id: request_id.clone(),
                            tool_index,
                            id: None,
                            name: None,
                            arguments_delta: Some(delta),
                        };
                    }
                }

                ResponseStreamEvent::ResponseCompleted(completed_event) => {
                    final_response = Some(completed_event.response);
                }

                ResponseStreamEvent::ResponseIncomplete(incomplete_event) => {
                    final_response = Some(incomplete_event.response);
                    should_break = true;
                }

                ResponseStreamEvent::ResponseFailed(failed_event) => {
                    let response = failed_event.response;
                    let error_message = response
                        .error
                        .as_ref()
                        .map(|e| format!("{}: {}", e.code, e.message))
                        .unwrap_or_else(|| "Response failed with unknown error".to_string());
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                ResponseStreamEvent::ResponseError(error_event) => {
                    let code = error_event.code.unwrap_or_else(|| "error".to_string());
                    let error_message = format!("{}: {}", code, error_event.message);
                    let err = SamplingError::Api {
                        status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                        message: error_message,
                        model_metadata: None,
                        retry_after_secs: None,
                        should_retry: None,
                    };
                    yield SamplingEvent::Failed {
                        request_id: request_id.clone(),
                        error: SamplingErrorInfo::from(&err),
                    };
                    return;
                }

                // ── Backend-hosted tool lifecycle events ────────────
                // These tools are executed server-side by the agentic
                // sampler. We emit progress events so the shell/pager
                // can show status to the user.

                // Web search
                ResponseStreamEvent::ResponseWebSearchCallInProgress(ev) => {
                    let was_started = backend_output_started.contains(&ev.output_index)
                        || backend_tool_started.contains(&ev.item_id);
                    backend_output_started.insert(ev.output_index);
                    backend_tool_started.insert(ev.item_id.clone());
                    if !was_started {
                        yield SamplingEvent::BackendToolCallStarted {
                            request_id: request_id.clone(),
                            call_id: ev.item_id.clone(),
                            name: "web_search".to_string(),
                        };
                    }
                }
                // Completed/Searching carry no data — the real payload
                // arrives via ResponseOutputItemDone(WebSearchCall) below.
                ResponseStreamEvent::ResponseWebSearchCallCompleted(_)
                | ResponseStreamEvent::ResponseWebSearchCallSearching(_) => {}

                // OutputItemDone carries the full result for backend tools.
                // For WebSearchCall this includes the query and source URLs.
                // For CustomToolCall this includes x_search results.
                ResponseStreamEvent::ResponseOutputItemDone(done_event) => {
                    completed_output_items
                        .insert(done_event.output_index, done_event.item.clone());
                    match &done_event.item {
                        rs::OutputItem::WebSearchCall(ws) => {
                            let was_started = backend_output_started.contains(&done_event.output_index)
                                || backend_tool_started.contains(&ws.id);
                            backend_output_started.insert(done_event.output_index);
                            backend_tool_started.insert(ws.id.clone());
                            if !was_started {
                                yield SamplingEvent::BackendToolCallStarted {
                                    request_id: request_id.clone(),
                                    call_id: ws.id.clone(),
                                    name: "web_search".to_string(),
                                };
                            }
                            let result = serde_json::to_value(ws).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ws.id.clone(),
                                name: "web_search".to_string(),
                                result,
                            };
                        }
                        // X search results arrive as CustomToolCall with
                        // names like x_keyword_search, x_semantic_search, etc.
                        // Use "x_search" consistently (matching the Started event);
                        // the specific sub-type is in the serialized result payload
                        // and extracted by the pager from raw_output.name.
                        rs::OutputItem::CustomToolCall(ct)
                            if !client_custom_tool_names.iter().any(|name| name == &ct.name) =>
                        {
                            let was_started = backend_output_started.contains(&done_event.output_index)
                                || backend_tool_started.contains(&ct.id)
                                || backend_tool_started.contains(&ct.call_id);
                            backend_output_started.insert(done_event.output_index);
                            backend_tool_started.insert(ct.id.clone());
                            backend_tool_started.insert(ct.call_id.clone());
                            if !was_started {
                                yield SamplingEvent::BackendToolCallStarted {
                                    request_id: request_id.clone(),
                                    call_id: ct.id.clone(),
                                    name: "x_search".to_string(),
                                };
                            }
                            let result = serde_json::to_value(ct).ok();
                            yield SamplingEvent::BackendToolCallCompleted {
                                request_id: request_id.clone(),
                                call_id: ct.id.clone(),
                                name: "x_search".to_string(),
                                result,
                            };
                        }
                        _ => {}
                    }
                }

                // A done event without a preceding delta still carries the
                // complete custom input. For backend custom calls it remains
                // the x_search lifecycle start signal.
                ResponseStreamEvent::ResponseCustomToolCallInputDone(ev) => {
                    if let Some(&tool_index) = output_to_tool_index.get(&ev.output_index) {
                        if !custom_input_started.contains(&ev.output_index) && !ev.input.is_empty() {
                            yield SamplingEvent::ToolCallDelta {
                                request_id: request_id.clone(),
                                tool_index,
                                id: None,
                                name: None,
                                arguments_delta: Some(ev.input),
                            };
                        }
                    } else {
                        let was_started = backend_output_started.contains(&ev.output_index)
                            || backend_tool_started.contains(&ev.item_id);
                        backend_output_started.insert(ev.output_index);
                        backend_tool_started.insert(ev.item_id.clone());
                        if !was_started {
                            yield SamplingEvent::BackendToolCallStarted {
                                request_id: request_id.clone(),
                                call_id: ev.item_id.clone(),
                                name: "x_search".to_string(),
                            };
                        }
                    }
                }

                // All other events (intermediate progress, annotations,
                // image gen, file search, etc.) — no action needed.
                _ => {}
            }

            if event_has_content {
                last_content_chunk_at = Instant::now();
            } else if last_content_chunk_at.elapsed() > idle_timeout {
                let err = SamplingError::IdleTimeout {
                    elapsed_secs: idle_timeout.as_secs(),
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }

            if should_break {
                break;
            }
        }

        // ── Build the final response ─────────────────────────────────
        let mut response = match final_response {
            Some(r) => r,
            None => {
                let err = SamplingError::Api {
                    status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                    message: "No ResponseCompleted or ResponseIncomplete event received from \
                              Responses API"
                        .to_string(),
                    model_metadata: None,
                    retry_after_secs: None,
                    should_retry: None,
                };
                yield SamplingEvent::Failed {
                    request_id: request_id.clone(),
                    error: SamplingErrorInfo::from(&err),
                };
                return;
            }
        };

        // Normal xAI/Responses streams include the full output on the
        // terminal response. Keep that authoritative when present; otherwise
        // reconstruct the Codex response from its ordered done items. Never
        // merge both representations, which would duplicate messages and
        // tool calls.
        if response.output.is_empty() && !completed_output_items.is_empty() {
            response.output = completed_output_items.into_values().collect();
        }

        // Billing fields (`prompt_tokens`, `completion_tokens`,
        // `cached_prompt_tokens`, `reasoning_tokens`) are the cumulative
        // wire values — they sum across every server-side turn of the
        // agent loop and are what we bill on / log to telemetry.
        //
        // `total_tokens` is the live context length used to drive the
        // CLI `/context` bar, the auto-compact threshold, and
        // `meta.totalTokens` on persisted sessions. The SSE decoder
        // (`deserialize_response_event`) has already rewritten
        // `u.total_tokens` to `context_details.input + output` when
        // the backend emits it; on older deployments the wire
        // value passes through unchanged.
        let usage = response.usage.as_ref().map(|u| TokenUsage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
            reasoning_tokens: u.output_tokens_details.reasoning_tokens,
            cached_prompt_tokens: u.input_tokens_details.cached_tokens,
        });

        let cost_usd_ticks = response
            .metadata
            .as_mut()
            .and_then(|m| m.remove(crate::client::COST_USD_TICKS_METADATA_KEY))
            .and_then(|s| s.parse::<i64>().ok());

        let status = response.status.clone();

        // Drop any mid-widget citation buffer so unfinished markers never
        // surface via fallback reconstruction paths.
        display_citation_filter.finish();

        // Convert to ConversationItem(s); patch in accumulated reasoning
        // text as a fallback when the final response lacks `content` /
        // `summary` (the streaming deltas may have arrived out of band).
        // Splice policy lives in `inject_streaming_reasoning_fallback`.
        let mut items =
            xai_grok_sampling_types::response_to_conversation_items_with_client_custom_tools(
                response,
                &client_custom_tool_names,
            );
        // Final response output still carries raw display citations; strip
        // them so chat history, resume, and next-turn context stay clean.
        strip_display_citations_in_items(&mut items);
        let mut summaries_by_item: BTreeMap<(u32, String), Vec<(u32, String)>> = BTreeMap::new();
        for ((output_index, item_id, summary_index), text) in reasoning_summary_parts {
            if !text.is_empty() {
                summaries_by_item
                    .entry((output_index, item_id))
                    .or_default()
                    .push((summary_index, text));
            }
        }
        if summaries_by_item.is_empty() {
            xai_grok_sampling_types::inject_streaming_reasoning_fallback(
                &mut items,
                reasoning_text_acc,
            );
        } else {
            let summaries = summaries_by_item
                .into_iter()
                .map(|((output_index, item_id), mut parts)| {
                    parts.sort_by_key(|(summary_index, _)| *summary_index);
                    (
                        output_index,
                        item_id,
                        parts.into_iter().map(|(_, text)| text).collect(),
                    )
                })
                .collect();
            xai_grok_sampling_types::inject_streaming_reasoning_summary_fallbacks(
                &mut items,
                summaries,
            );
        }

        let has_tool_calls = items.iter().any(|i| match i {
            ConversationItem::Assistant(a) => !a.tool_calls.is_empty(),
            _ => false,
        });

        let stop_reason = if has_tool_calls {
            Some(StopReason::ToolCalls)
        } else {
            match status {
                Status::Completed => Some(StopReason::Stop),
                Status::Incomplete => Some(StopReason::Length),
                _ => None,
            }
        };

        let stream_end = Instant::now();
        let metrics =
            InferenceLatencyStats::from_timestamps(stream_start, &chunk_timestamps, stream_end);

        // Warn-only for now: surface the server-reported triggers once per
        // request (raw labels only — ZDR-safe) and attach them for callers.
        let doom_loop_signals = doom_loop
            .as_ref()
            .map(|collector| collector.take())
            .unwrap_or_default();
        if !doom_loop_signals.is_empty() {
            tracing::warn!(
                request_id = %request_id,
                triggers = ?doom_loop_signals.iter().map(|s| s.raw.as_str()).collect::<Vec<_>>(),
                "server reported doom-loop triggers for this response"
            );
        }

        let conversation_response = ConversationResponse {
            items,
            stop_reason,
            usage,
            cost_usd_ticks,
            message_chunks_emitted: message_chunk_count,
            doom_loop_signals,
            stop_message: None, // not reported on the Responses API
        };

        yield SamplingEvent::Completed {
            request_id: request_id.clone(),
            response: Box::new(conversation_response),
            metrics,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::responses as rs_types;
    use futures_util::stream;
    use std::pin::pin;

    fn rid() -> RequestId {
        RequestId::from("resp-test")
    }

    /// Build a minimal `rs_types::Response` for use in `ResponseCompleted`
    fn build_response(status: rs_types::Status) -> rs_types::Response {
        rs_types::Response {
            background: None,
            billing: None,
            conversation: None,
            created_at: 0,
            completed_at: None,
            error: None,
            id: "resp_1".into(),
            incomplete_details: None,
            instructions: None,
            max_output_tokens: None,
            metadata: None,
            model: "test-model".into(),
            object: "response".into(),
            output: vec![],
            parallel_tool_calls: None,
            previous_response_id: None,
            prompt: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            reasoning: None,
            safety_identifier: None,
            service_tier: None,
            status,
            temperature: None,
            text: None,
            tool_choice: None,
            tools: None,
            top_logprobs: None,
            top_p: None,
            truncation: None,
            usage: None,
        }
    }

    fn empty_completed_response() -> rs_types::Response {
        build_response(rs_types::Status::Completed)
    }

    fn failed_response_with_error(message: &str) -> rs_types::Response {
        let mut r = build_response(rs_types::Status::Failed);
        r.error = Some(rs_types::ErrorObject {
            code: "server_error".into(),
            message: message.into(),
        });
        r
    }

    fn text_delta_event(delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputTextDelta(rs_types::ResponseTextDeltaEvent {
            sequence_number: 0,
            item_id: "item-1".into(),
            output_index: 0,
            content_index: 0,
            delta: delta.into(),
            logprobs: None,
        })
    }

    fn reasoning_summary_delta_event(summary_index: u32, delta: &str) -> rs::ResponseStreamEvent {
        reasoning_summary_delta_event_for(0, "reasoning-1", summary_index, delta)
    }

    fn reasoning_summary_delta_event_for(
        output_index: u32,
        item_id: &str,
        summary_index: u32,
        delta: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseReasoningSummaryTextDelta(
            rs_types::ResponseReasoningSummaryTextDeltaEvent {
                sequence_number: 0,
                item_id: item_id.into(),
                output_index,
                summary_index,
                delta: delta.into(),
            },
        )
    }

    fn reasoning_summary_done_event(
        output_index: u32,
        item_id: &str,
        summary_index: u32,
        text: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseReasoningSummaryTextDone(
            rs_types::ResponseReasoningSummaryTextDoneEvent {
                sequence_number: 0,
                item_id: item_id.into(),
                output_index,
                summary_index,
                text: text.into(),
            },
        )
    }

    fn empty_reasoning_output(item_id: &str) -> rs_types::OutputItem {
        rs_types::OutputItem::Reasoning(rs_types::ReasoningItem {
            id: item_id.into(),
            summary: Vec::new(),
            content: None,
            encrypted_content: Some(format!("encrypted-{item_id}")),
            status: None,
        })
    }

    fn completed_event() -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
            response: empty_completed_response(),
            sequence_number: 0,
        })
    }

    fn output_item_done_event(
        output_index: u32,
        item: rs_types::OutputItem,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemDone(rs_types::ResponseOutputItemDoneEvent {
            sequence_number: u64::from(output_index),
            output_index,
            item,
        })
    }

    fn assistant_message_output(id: &str, text: &str) -> rs_types::OutputItem {
        serde_json::from_value(serde_json::json!({
            "type": "message",
            "id": id,
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": text,
                "annotations": []
            }]
        }))
        .expect("valid assistant message output")
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
    async fn missing_completed_event_yields_failed() {
        let raw =
            stream::iter(Vec::<Result<rs::ResponseStreamEvent, SamplingError>>::new()).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_delta_then_completed_yields_completed_with_stop() {
        let raw = stream::iter(vec![Ok(text_delta_event("hello")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

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
        assert_eq!(text_tokens, vec!["hello"]);

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.stop_reason, Some(StopReason::Stop));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn display_citation_widgets_are_stripped_from_stream_and_items() {
        use crate::stream::display_citations::{CITATION_DELIMITER, CITATION_START, CITATION_STOP};

        // Matches the Workouts-iOS session log shape for hosted web_search.
        let marker = format!(
            "{CITATION_START}cite{CITATION_DELIMITER}turn1search0\
             {CITATION_DELIMITER}turn7search0{CITATION_STOP}"
        );
        let stream_text = format!("available reports. {marker}\n\n**Validation**");
        let final_text = format!("final answer {marker} end");

        let mut completed = empty_completed_response();
        completed.output = vec![assistant_message_output("msg_1", &final_text)];

        // Split on a char boundary so multi-byte PUA sentinels stay intact.
        let split_at = marker
            .char_indices()
            .nth(marker.chars().count() / 2)
            .map(|(i, _)| i)
            .unwrap_or(marker.len());

        let raw = stream::iter(vec![
            Ok(text_delta_event("available reports. ")),
            // Split the widget across deltas so the stream filter must buffer.
            Ok(text_delta_event(&marker[..split_at])),
            Ok(text_delta_event(&marker[split_at..])),
            Ok(text_delta_event("\n\n**Validation**")),
            Ok(rs::ResponseStreamEvent::ResponseCompleted(
                rs_types::ResponseCompletedEvent {
                    response: completed,
                    sequence_number: 1,
                },
            )),
        ])
        .boxed();

        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let streamed: String = events
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
        assert_eq!(streamed, "available reports. \n\n**Validation**");
        assert!(
            !streamed.contains(CITATION_START)
                && !streamed.contains("turn1search0")
                && !streamed.contains("cite"),
            "streamed text must not retain display citation widgets: {streamed:?}"
        );
        // Sanity: the unfiltered stream payload did contain the marker.
        assert!(stream_text.contains("turn1search0"));

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                let assistant = response
                    .assistant()
                    .expect("completed response should have assistant text");
                assert_eq!(assistant.content.as_ref(), "final answer  end");
                assert!(!assistant.content.contains(CITATION_START));
                assert!(!assistant.content.contains("turn1search0"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reasoning_summary_deltas_survive_when_terminal_response_omits_summary() {
        let raw = stream::iter(vec![
            Ok(reasoning_summary_delta_event(0, "First part.")),
            Ok(reasoning_summary_delta_event(1, "Second part.")),
            Ok(completed_event()),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let reasoning_tokens = events
            .iter()
            .filter_map(|event| match event {
                SamplingEvent::ChannelToken {
                    channel: SamplingChannel::Reasoning,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reasoning_tokens, vec!["First part.", "Second part."]);

        let SamplingEvent::Completed { response, .. } = events.last().unwrap() else {
            panic!("expected completed response, got {:?}", events.last());
        };
        let summary_parts = response.items.iter().find_map(|item| match item {
            ConversationItem::Reasoning(reasoning) => Some(
                reasoning
                    .summary
                    .iter()
                    .map(|part| {
                        let rs::SummaryPart::SummaryText(text) = part;
                        text.text.as_str()
                    })
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        });
        assert_eq!(summary_parts, Some(vec!["First part.", "Second part."]));
    }

    #[tokio::test]
    async fn reasoning_summaries_are_scoped_to_their_output_item_ids() {
        let raw = stream::iter(vec![
            Ok(reasoning_summary_delta_event_for(
                0,
                "reasoning-a",
                0,
                "Summary A",
            )),
            Ok(reasoning_summary_delta_event_for(
                1,
                "reasoning-b",
                0,
                "Summary B",
            )),
            Ok(completed_event_with_output(vec![
                empty_reasoning_output("reasoning-a"),
                empty_reasoning_output("reasoning-b"),
            ])),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let SamplingEvent::Completed { response, .. } = events.last().unwrap() else {
            panic!("expected completed response, got {:?}", events.last());
        };
        let summaries = response
            .items
            .iter()
            .filter_map(|item| match item {
                ConversationItem::Reasoning(reasoning) => Some((
                    reasoning.id.as_str(),
                    reasoning.summary.first().map(|part| {
                        let rs::SummaryPart::SummaryText(text) = part;
                        text.text.as_str()
                    }),
                )),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(summaries.get("reasoning-a"), Some(&Some("Summary A")));
        assert_eq!(summaries.get("reasoning-b"), Some(&Some("Summary B")));
    }

    #[tokio::test]
    async fn done_only_reasoning_summary_is_rendered_once_and_persisted() {
        let raw = stream::iter(vec![
            Ok(reasoning_summary_done_event(
                0,
                "reasoning-done",
                0,
                "Done-only summary",
            )),
            Ok(completed_event_with_output(vec![empty_reasoning_output(
                "reasoning-done",
            )])),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let tokens = events
            .iter()
            .filter_map(|event| match event {
                SamplingEvent::ChannelToken {
                    channel: SamplingChannel::Reasoning,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tokens, vec!["Done-only summary"]);

        let SamplingEvent::Completed { response, .. } = events.last().unwrap() else {
            panic!("expected completed response, got {:?}", events.last());
        };
        let summary = response.items.iter().find_map(|item| match item {
            ConversationItem::Reasoning(reasoning) if reasoning.id == "reasoning-done" => {
                reasoning.summary.first().map(|part| {
                    let rs::SummaryPart::SummaryText(text) = part;
                    text.text.as_str()
                })
            }
            _ => None,
        });
        assert_eq!(summary, Some("Done-only summary"));
    }

    #[tokio::test]
    async fn output_item_done_reconstructs_metadata_only_codex_completion() {
        let raw = stream::iter(vec![
            Ok(reasoning_summary_delta_event_for(
                0,
                "reasoning-durable",
                0,
                "Visible summary",
            )),
            Ok(text_delta_event("Durable answer")),
            Ok(output_item_done_event(
                0,
                empty_reasoning_output("reasoning-durable"),
            )),
            Ok(output_item_done_event(
                1,
                assistant_message_output("message-durable", "Durable answer"),
            )),
            Ok(completed_event()),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let streamed = events
            .iter()
            .filter_map(|event| match event {
                SamplingEvent::ChannelToken { channel, text, .. } => {
                    Some((channel.clone(), text.as_str()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            streamed,
            vec![
                (SamplingChannel::Reasoning, "Visible summary"),
                (SamplingChannel::Text, "Durable answer"),
            ],
            "done items must not emit a second set of live tokens",
        );

        let SamplingEvent::Completed { response, .. } = events.last().unwrap() else {
            panic!("expected completed response, got {:?}", events.last());
        };
        assert_eq!(response.items.len(), 2);
        let ConversationItem::Reasoning(reasoning) = &response.items[0] else {
            panic!("expected durable reasoning item");
        };
        assert_eq!(
            reasoning.encrypted_content.as_deref(),
            Some("encrypted-reasoning-durable")
        );
        let rs::SummaryPart::SummaryText(summary) = &reasoning.summary[0];
        assert_eq!(summary.text, "Visible summary");
        let ConversationItem::Assistant(assistant) = &response.items[1] else {
            panic!("expected durable assistant message");
        };
        assert_eq!(assistant.content.as_ref(), "Durable answer");
    }

    #[tokio::test]
    async fn terminal_output_remains_authoritative_over_done_items() {
        let raw = stream::iter(vec![
            Ok(output_item_done_event(
                0,
                assistant_message_output("message-done", "from done item"),
            )),
            Ok(completed_event_with_output(vec![assistant_message_output(
                "message-terminal",
                "from terminal response",
            )])),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        let SamplingEvent::Completed { response, .. } = events.last().unwrap() else {
            panic!("expected completed response, got {:?}", events.last());
        };
        assert_eq!(response.items.len(), 1);
        let ConversationItem::Assistant(assistant) = &response.items[0] else {
            panic!("expected terminal assistant message");
        };
        assert_eq!(assistant.content.as_ref(), "from terminal response");
    }

    #[tokio::test]
    async fn response_failed_yields_failed_500() {
        let failed = rs::ResponseStreamEvent::ResponseFailed(rs_types::ResponseFailedEvent {
            response: failed_response_with_error("boom"),
            sequence_number: 0,
        });
        let raw = stream::iter(vec![Ok(failed)]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::Api);
                assert_eq!(error.status_code, Some(500));
                assert!(error.message.contains("boom"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mid_stream_transport_error_yields_failed() {
        let raw = stream::iter(vec![
            Ok(text_delta_event("hi")),
            Err(SamplingError::EventStreamError("conn reset".into())),
        ])
        .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Failed { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_when_stream_stalls() {
        let raw = stream::iter(vec![Ok(text_delta_event("hi"))])
            .chain(stream::pending())
            .boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_millis(100),
            None,
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(error.kind, crate::events::SamplingErrorKind::IdleTimeout);
            }
            other => panic!("expected Failed(IdleTimeout), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn model_metadata_yielded_after_stream_started() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let metadata = ResponseModelMetadata {
            context_window: Some(8192),
            ..Default::default()
        };
        let events = collect(stream_responses(
            raw,
            Some(metadata),
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
        assert!(matches!(events[1], SamplingEvent::ModelMetadata { .. }));
    }

    #[test]
    fn meaningful_content_classifier_basics() {
        // Text delta with content is meaningful.
        let event = text_delta_event("foo");
        assert!(responses_event_has_meaningful_content(&event));
        // Empty text delta is not.
        let empty = text_delta_event("");
        assert!(!responses_event_has_meaningful_content(&empty));
        // Completed is meaningful (terminal).
        assert!(responses_event_has_meaningful_content(&completed_event()));
    }

    fn function_call_added_event(
        output_index: u32,
        call_id: &str,
        name: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemAdded(rs_types::ResponseOutputItemAddedEvent {
            sequence_number: 0,
            output_index,
            item: rs_types::OutputItem::FunctionCall(rs_types::FunctionToolCall {
                arguments: String::new(),
                call_id: call_id.into(),
                name: name.into(),
                id: None,
                status: None,
            }),
        })
    }

    fn function_call_args_delta_event(output_index: u32, delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(
            rs_types::ResponseFunctionCallArgumentsDeltaEvent {
                sequence_number: 0,
                item_id: format!("item-{output_index}"),
                output_index,
                delta: delta.into(),
            },
        )
    }

    fn custom_tool_call(
        call_id: &str,
        item_id: &str,
        name: &str,
        input: &str,
    ) -> rs_types::CustomToolCall {
        serde_json::from_value(serde_json::json!({
            "call_id": call_id,
            "id": item_id,
            "name": name,
            "input": input,
        }))
        .unwrap()
    }

    fn custom_call_added_event(
        output_index: u32,
        call_id: &str,
        item_id: &str,
        name: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemAdded(rs_types::ResponseOutputItemAddedEvent {
            sequence_number: 0,
            output_index,
            item: rs_types::OutputItem::CustomToolCall(custom_tool_call(
                call_id, item_id, name, "",
            )),
        })
    }

    fn custom_call_added_event_with_input(
        output_index: u32,
        call_id: &str,
        item_id: &str,
        name: &str,
        input: &str,
    ) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseOutputItemAdded(rs_types::ResponseOutputItemAddedEvent {
            sequence_number: 0,
            output_index,
            item: rs_types::OutputItem::CustomToolCall(custom_tool_call(
                call_id, item_id, name, input,
            )),
        })
    }

    fn custom_call_input_delta_event(output_index: u32, delta: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseCustomToolCallInputDelta(
            rs_types::ResponseCustomToolCallInputDeltaEvent {
                sequence_number: 0,
                output_index,
                item_id: format!("ctc_{output_index}"),
                delta: delta.into(),
            },
        )
    }

    fn custom_call_input_done_event(output_index: u32, input: &str) -> rs::ResponseStreamEvent {
        rs::ResponseStreamEvent::ResponseCustomToolCallInputDone(
            rs_types::ResponseCustomToolCallInputDoneEvent {
                sequence_number: 0,
                output_index,
                item_id: format!("ctc_{output_index}"),
                input: input.into(),
            },
        )
    }

    fn completed_event_with_output(output: Vec<rs_types::OutputItem>) -> rs::ResponseStreamEvent {
        let mut response = empty_completed_response();
        response.output = output;
        rs::ResponseStreamEvent::ResponseCompleted(rs_types::ResponseCompletedEvent {
            response,
            sequence_number: 0,
        })
    }

    type Delta = (u32, Option<String>, Option<String>, Option<String>);

    /// Extract all ToolCallDelta events as (tool_index, id, name, arguments_delta).
    fn tool_call_deltas(evs: &[SamplingEvent]) -> Vec<Delta> {
        evs.iter()
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
            .collect()
    }

    #[tokio::test]
    async fn output_item_done_preserves_complete_codex_function_call() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_complete", "do_thing")),
            Ok(function_call_args_delta_event(0, "{\"x\":")),
            Ok(function_call_args_delta_event(0, "1}")),
            Ok(output_item_done_event(
                0,
                rs_types::OutputItem::FunctionCall(rs_types::FunctionToolCall {
                    arguments: "{\"x\":1}".into(),
                    call_id: "call_complete".into(),
                    name: "do_thing".into(),
                    id: Some("fc_complete".into()),
                    status: Some(rs_types::OutputStatus::Completed),
                }),
            )),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;

        assert_eq!(tool_call_deltas(&evs).len(), 3);
        let SamplingEvent::Completed { response, .. } = evs.last().unwrap() else {
            panic!("expected completed response");
        };
        assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
        let ConversationItem::Assistant(assistant) = response.items.last().unwrap() else {
            panic!("expected trailing assistant");
        };
        let call = assistant
            .tool_calls
            .first()
            .expect("complete function call");
        assert_eq!(call.call_id(), "call_complete");
        assert_eq!(call.name, "do_thing");
        assert_eq!(call.arguments.as_ref(), "{\"x\":1}");
    }

    #[tokio::test]
    async fn function_call_emits_initial_id_name_then_arg_deltas() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_xyz", "do_thing")),
            Ok(function_call_args_delta_event(0, "{\"x\":")),
            Ok(function_call_args_delta_event(0, "1}")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].0, 0);
        assert_eq!(deltas[0].1.as_deref(), Some("call_xyz"));
        assert_eq!(deltas[0].2.as_deref(), Some("do_thing"));
        assert_eq!(deltas[0].3, None);
        assert_eq!(deltas[1].0, 0);
        assert_eq!(deltas[1].1, None);
        assert_eq!(deltas[1].2, None);
        assert_eq!(deltas[1].3.as_deref(), Some("{\"x\":"));
        assert_eq!(deltas[2].3.as_deref(), Some("1}"));
    }

    #[tokio::test]
    async fn function_call_args_delta_without_added_event_is_dropped() {
        // ArgumentsDelta with no preceding OutputItemAdded has no
        // output_index → tool_index mapping; drop silently.
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_args_delta_event(7, "{\"oops\":1}")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        assert_eq!(tool_call_deltas(&evs).len(), 0);
    }

    #[tokio::test]
    async fn client_custom_call_streams_raw_input_and_completes_as_tool_call() {
        let final_call =
            custom_tool_call("call_code", "ctc_code", "code", "const answer = 40 + 2;");
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(custom_call_added_event(0, "call_code", "ctc_code", "code")),
            Ok(custom_call_input_delta_event(0, "const answer = ")),
            Ok(custom_call_input_delta_event(0, "40 + 2;")),
            Ok(custom_call_input_done_event(0, "const answer = 40 + 2;")),
            Ok(completed_event_with_output(vec![
                rs_types::OutputItem::CustomToolCall(final_call),
            ])),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses_with_client_custom_tools(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            vec!["code".into()],
        ))
        .await;

        let deltas = tool_call_deltas(&evs);
        assert_eq!(deltas.len(), 3);
        assert_eq!(deltas[0].1.as_deref(), Some("call_code"));
        assert_eq!(deltas[0].2.as_deref(), Some("code"));
        assert_eq!(deltas[0].3, None);
        assert_eq!(deltas[1].3.as_deref(), Some("const answer = "));
        assert_eq!(deltas[2].3.as_deref(), Some("40 + 2;"));
        assert!(!evs.iter().any(|event| matches!(
            event,
            SamplingEvent::BackendToolCallStarted { .. }
                | SamplingEvent::BackendToolCallCompleted { .. }
        )));

        let SamplingEvent::Completed { response, .. } = evs.last().unwrap() else {
            panic!("expected completed response");
        };
        assert_eq!(response.stop_reason, Some(StopReason::ToolCalls));
        let ConversationItem::Assistant(assistant) = response.items.last().unwrap() else {
            panic!("expected trailing assistant");
        };
        let call = assistant.tool_calls.first().expect("custom tool call");
        assert!(call.is_custom());
        assert_eq!(call.call_id(), "call_code");
        assert_eq!(call.custom_item_id(), Some("ctc_code"));
        assert_eq!(call.custom_input(), Some("const answer = 40 + 2;"));
    }

    #[tokio::test]
    async fn client_custom_input_is_not_duplicated_after_added_and_delta_events() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(custom_call_added_event_with_input(
                0,
                "call_code",
                "ctc_code",
                "code",
                "const answer = ",
            )),
            Ok(custom_call_input_delta_event(0, "40 + 2;")),
            Ok(custom_call_input_done_event(0, "const answer = 40 + 2;")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses_with_client_custom_tools(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            vec!["code".into()],
        ))
        .await;

        let streamed_input: String = tool_call_deltas(&evs)
            .into_iter()
            .filter_map(|(_, _, _, delta)| delta)
            .collect();
        assert_eq!(streamed_input, "const answer = 40 + 2;");
    }

    #[tokio::test]
    async fn undeclared_custom_call_remains_backend_xsearch() {
        let final_call = custom_tool_call(
            "call_search",
            "ctc_search",
            "x_keyword_search",
            "custom tools",
        );
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(custom_call_added_event(
                0,
                "call_search",
                "ctc_search",
                "x_keyword_search",
            )),
            Ok(custom_call_input_done_event(0, "custom tools")),
            Ok(rs::ResponseStreamEvent::ResponseOutputItemDone(
                rs_types::ResponseOutputItemDoneEvent {
                    sequence_number: 0,
                    output_index: 0,
                    item: rs_types::OutputItem::CustomToolCall(final_call.clone()),
                },
            )),
            Ok(completed_event_with_output(vec![
                rs_types::OutputItem::CustomToolCall(final_call),
            ])),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses_with_client_custom_tools(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            vec!["code".into()],
        ))
        .await;

        assert!(tool_call_deltas(&evs).is_empty());
        assert_eq!(
            evs.iter()
                .filter(|event| matches!(
                    event,
                    SamplingEvent::BackendToolCallStarted { name, .. } if name == "x_search"
                ))
                .count(),
            1,
            "output-item and input-done frames must share one hosted-tool lifecycle",
        );
        assert!(evs.iter().any(|event| matches!(
            event,
            SamplingEvent::BackendToolCallCompleted { name, .. } if name == "x_search"
        )));
        let SamplingEvent::Completed { response, .. } = evs.last().unwrap() else {
            panic!("expected completed response");
        };
        assert_eq!(response.stop_reason, Some(StopReason::Stop));
        assert!(matches!(
            &response.items[0],
            ConversationItem::BackendToolCall(_)
        ));
    }

    #[tokio::test]
    async fn normalized_xsearch_item_needs_no_custom_input_event_for_lifecycle() {
        let final_call = custom_tool_call(
            "xs_123",
            "xs_123",
            "x_search",
            r#"{"query":"current xAI news"}"#,
        );
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(custom_call_added_event(0, "xs_123", "xs_123", "x_search")),
            Ok(rs::ResponseStreamEvent::ResponseOutputItemDone(
                rs_types::ResponseOutputItemDoneEvent {
                    sequence_number: 1,
                    output_index: 0,
                    item: rs_types::OutputItem::CustomToolCall(final_call.clone()),
                },
            )),
            Ok(completed_event_with_output(vec![
                rs_types::OutputItem::CustomToolCall(final_call),
            ])),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses_with_client_custom_tools(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
            vec!["code".into()],
        ))
        .await;

        assert_eq!(
            evs.iter()
                .filter(|event| matches!(
                    event,
                    SamplingEvent::BackendToolCallStarted { name, .. } if name == "x_search"
                ))
                .count(),
            1,
        );
        assert_eq!(
            evs.iter()
                .filter(|event| matches!(
                    event,
                    SamplingEvent::BackendToolCallCompleted { name, .. } if name == "x_search"
                ))
                .count(),
            1,
        );
    }

    #[tokio::test]
    async fn multiple_function_calls_get_distinct_tool_indices() {
        let events: Vec<Result<rs::ResponseStreamEvent, SamplingError>> = vec![
            Ok(function_call_added_event(0, "call_a", "tool_a")),
            Ok(function_call_added_event(1, "call_b", "tool_b")),
            Ok(function_call_args_delta_event(0, "a-args")),
            Ok(function_call_args_delta_event(1, "b-args")),
            Ok(completed_event()),
        ];
        let raw = stream::iter(events).boxed();
        let evs = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        let deltas = tool_call_deltas(&evs);

        assert_eq!(deltas.len(), 4);
        assert_eq!(deltas[0].0, 0);
        assert_eq!(deltas[0].1.as_deref(), Some("call_a"));
        assert_eq!(deltas[1].0, 1);
        assert_eq!(deltas[1].1.as_deref(), Some("call_b"));
        assert_eq!(deltas[2].0, 0);
        assert_eq!(deltas[2].3.as_deref(), Some("a-args"));
        assert_eq!(deltas[3].0, 1);
        assert_eq!(deltas[3].3.as_deref(), Some("b-args"));
    }

    #[tokio::test]
    async fn doom_loop_collector_signals_land_on_completed_response() {
        use xai_grok_sampling_types::doom_loop::{
            DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA,
        };
        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb(DOOM_LOOP_CHECK_EVENT_TYPE, SAMPLE_CHECK_EVENT_DATA));
        let raw = stream::iter(vec![Ok(text_delta_event("hello")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;

        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
                assert_eq!(
                    response.doom_loop_signals[0].raw,
                    "tail_repetition:4@response"
                );
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// An armed collector holding a confident signal aborts the attempt with
    /// a retryable doom-loop failure; disarmed, the same stream completes and
    /// the signals ride the response instead.
    #[tokio::test]
    async fn confident_signal_aborts_stream_unless_disarmed() {
        let confident = r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:8@thinking"]}}"#;

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        let raw = stream::iter(vec![Ok(text_delta_event("hi")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Failed { error, .. } => {
                assert_eq!(
                    error.kind,
                    crate::events::SamplingErrorKind::DoomLoopDetected
                );
                assert!(error.is_retryable);
                assert_eq!(
                    error.doom_loop_triggers.as_deref(),
                    Some(&["tail_repetition:8@thinking".to_string()][..])
                );
            }
            other => panic!("expected Failed(DoomLoopDetected), got {other:?}"),
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SamplingEvent::Completed { .. }))
        );

        let collector = crate::doom_loop::DoomLoopSignalCollector::default();
        assert!(collector.absorb("response.doom_loop_check", confident));
        collector.disarm_abort();
        let raw = stream::iter(vec![Ok(text_delta_event("hi")), Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(collector),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert_eq!(response.doom_loop_signals.len(), 1);
            }
            other => panic!("expected Completed after disarm, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn doom_loop_signals_empty_without_collector_or_triggers() {
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            None,
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }

        // A collector that never saw a trigger also leaves the field empty.
        let raw = stream::iter(vec![Ok(completed_event())]).boxed();
        let events = collect(stream_responses(
            raw,
            None,
            rid(),
            Duration::from_secs(60),
            Some(crate::doom_loop::DoomLoopSignalCollector::default()),
        ))
        .await;
        match events.last().unwrap() {
            SamplingEvent::Completed { response, .. } => {
                assert!(response.doom_loop_signals.is_empty());
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }
}
