//! HTTP client for the xAI sampling APIs.
//!
//! Owns the `reqwest::Client`, default request headers, and per-method
//! defaults. Talks to three backend shapes:
//!
//! * Chat Completions (`/chat/completions`)
//! * Responses API (`/responses`)
//! * Anthropic Messages API (`/messages`)
//!
//! All trace-upload and URL-based header injection is intentionally
//! *not* here. The session is responsible for putting any per-request
//! headers (proxy auth, OTel context, etc.)
//! into [`SamplerConfig::extra_headers`] before constructing the client.

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use indexmap::IndexMap;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

#[cfg(test)]
use xai_grok_sampling_types::ReasoningEffort;
use xai_grok_sampling_types::error::{try_parse_stream_error, user_facing_api_error_message};
use xai_grok_sampling_types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, CodeModeTransport,
    ConversationRequest, ConversationResponse, CreateResponseWrapper, DOOM_LOOP_CHECK_HEADER,
    MessagesRequestWrapper, ModelProvider, NamedCustomToolOutputOccurrence,
    OriginalDetailCustomOutputImageOccurrence, ResponseModelMetadata, Result, SamplingError,
    build_messages_request, is_check_event, messages, rs,
};

use crate::config::{AuthScheme, OriginClientInfo, SamplerConfig};
#[cfg(test)]
use crate::provider::{
    EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT, MULTI_AGENT_MODE_CLOSE_TAG,
    MULTI_AGENT_MODE_OPEN_TAG, PROACTIVE_MULTI_AGENT_MODE_TEXT,
};
use crate::provider::{
    ProviderAdapter, ProviderRequestHeaders as GrokRequestHeaders, ResponsesRequestPolicy,
    X_CODEX_TURN_STATE_HEADER, provider_adapter,
};

// Re-export ApiBackend from the shared types crate for downstream callers.
pub use xai_grok_sampling_types::ApiBackend;

/// Product identifier baked into User-Agent strings.
const AGENT_PRODUCT: &str = "grok-shell";
const ANTHROPIC_DEFAULT_MAX_TOKENS: u32 = 128_000;
const X_CODEX_BETA_FEATURES_HEADER: &str = "x-codex-beta-features";
const REMOTE_COMPACTION_V2_FEATURE: &str = "remote_compaction_v2";

/// Parse the `Retry-After` response header as delta-seconds.
/// Our inference backends only emit integer seconds (never HTTP-date),
/// so we only handle that form. HTTP-dates silently return `None` and
/// the caller falls back to exponential backoff.
/// Capped at 120s to prevent absurdly long sleeps from a misbehaving upstream.
/// Deserialize a Responses API SSE event, with fallbacks for valid wire
/// shapes that `async_openai` can't parse.
///
/// The API echoes the request's `tools` array in `ResponseCompleted` and
/// `ResponseCreated` events. If we sent `{"type": "x_search"}`, the response
/// includes it, and `rs::Tool` deserialization fails. Custom-tool events can
/// also omit fields that async-openai 0.33.1 requires. On failure, we normalize
/// those compatibility gaps, strip unrecognized tools, and retry.
///
/// On `response.completed` / `response.incomplete`, this also rewrites
/// `response.usage.total_tokens` in place to the live context length
/// (`context_details.input_tokens + context_details.output_tokens`)
/// when the API emits the xAI-specific `context_details` field.
/// Async-openai's typed `ResponseUsage` doesn't model `context_details`,
/// so we peek the raw JSON for it. The cumulative `input_tokens` /
/// `output_tokens` / `cached_tokens` continue to flow from the typed
/// `ResponseUsage` unchanged so billing telemetry stays correct. When
/// the API doesn't emit `context_details` (older deployments) `total_tokens`
/// passes through unchanged.
#[cfg(test)]
fn deserialize_response_event(data: &str) -> Result<rs::ResponseStreamEvent> {
    deserialize_response_event_for_adapter(data, provider_adapter(ModelProvider::Xai))
}

fn deserialize_response_event_for_adapter(
    data: &str,
    adapter: &dyn ProviderAdapter,
) -> Result<rs::ResponseStreamEvent> {
    let mut event = match serde_json::from_str::<rs::ResponseStreamEvent>(data) {
        Ok(event) => event,
        Err(first_err) => {
            // Try sanitizing provider extensions at the typed SDK boundary.
            if adapter.normalizes_response_events()
                && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(data)
            {
                normalize_response_event_compat(&mut value);
                if let Ok(mut event) = serde_json::from_value::<rs::ResponseStreamEvent>(value) {
                    apply_terminal_event_overrides(&mut event, data);
                    return Ok(event);
                }
            }
            tracing::error!(
                error = %first_err,
                raw_data = %data,
                "Failed to deserialize ResponseStreamEvent from stream"
            );
            return Err(SamplingError::Serialization(first_err));
        }
    };
    apply_terminal_event_overrides(&mut event, data);
    Ok(event)
}

/// Absorb Codex's forward-compatible `response.metadata` side channel.
///
/// `async-openai` 0.33.1 uses a closed enum for Responses stream events and
/// therefore rejects this valid Codex event before the ordinary stream
/// transformer can see it. codex-rs parses the event through an open string
/// discriminator, captures `x-codex-turn-state`, and otherwise treats it as a
/// side channel. Mirror that behavior here instead of manufacturing a typed
/// event or failing the request.
///
/// `response_id` is deliberately observed only for diagnostics. Codex reuses
/// it as `previous_response_id` exclusively for incremental WebSocket
/// requests; the HTTP Responses transport sends the complete input on every
/// request and uses `prompt_cache_key` for cache affinity.
/// Return true only when serde rejected the top-level Responses event kind,
/// not when a known event was malformed internally. codex-rs intentionally
/// uses an open string discriminator and ignores future event kinds; this
/// predicate lets the Codex HTTP stream preserve that forward compatibility
/// without hiding schema errors in events we already understand.
#[cfg(test)]
fn is_unknown_top_level_response_event(error: &SamplingError, data: &str) -> bool {
    provider_adapter(ModelProvider::Codex).ignores_unknown_response_event(error, data)
}

/// Normalize valid Responses stream shapes that async-openai 0.33.1 models
/// more strictly than the wire protocol used by some deployments.
///
/// The normalization mutates only missing compatibility fields. In particular,
/// optional `status` and provider extension fields remain on the raw object for
/// deserialization by newer dependency versions.
fn normalize_response_event_compat(value: &mut serde_json::Value) {
    let Some(mut event_type) = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };

    // xAI's Responses stream does not require OpenAI's sequence number.
    // async-openai does, so supply a neutral local value when it is absent.
    if event_type.starts_with("response.")
        && let Some(event) = value.as_object_mut()
    {
        event
            .entry("sequence_number".to_owned())
            .or_insert_with(|| serde_json::Value::Number(0_u64.into()));
    }

    // xAI also accepts `response.done` as its terminal spelling. The local
    // stream transformer already treats ResponseCompleted as the successful
    // terminal event, so normalize only at this dependency boundary.
    if event_type == "response.done" {
        value["type"] = serde_json::Value::String("response.completed".to_owned());
        event_type = "response.completed".to_owned();
    }

    // async-openai 0.33.1 does not model xAI's hosted X-search progress
    // events. OutputItemAdded/Done carry the actual lifecycle and payload, so
    // preserve these frames as recognized no-op progress events rather than
    // failing the whole stream or mislabeling them as web search.
    if matches!(
        event_type.as_str(),
        "response.x_search_call.in_progress"
            | "response.x_search_call.searching"
            | "response.x_search_call.completed"
    ) {
        value["type"] = serde_json::Value::String("response.web_search_call.searching".to_owned());
        event_type = "response.web_search_call.searching".to_owned();
    }

    // OpenAI may announce a web search before its action is populated. The
    // typed SDK requires `action` on every WebSearchToolCall, so project this
    // nonterminal OutputItemAdded frame onto the equivalent lifecycle event.
    // The later OutputItemDone remains strict and carries the durable action.
    if event_type == "response.output_item.added"
        && normalize_actionless_web_search_item_added(value)
    {
        return;
    }

    match event_type.as_str() {
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = value.get_mut("item") {
                normalize_response_output_item(item);
            }
        }
        "response.created"
        | "response.in_progress"
        | "response.completed"
        | "response.failed"
        | "response.incomplete" => {
            let default_status = match event_type.as_str() {
                "response.completed" => "completed",
                "response.failed" => "failed",
                "response.incomplete" => "incomplete",
                _ => "in_progress",
            };
            if let Some(response) = value.get_mut("response") {
                normalize_response_compat(response, default_status);
            }
        }
        "response.custom_tool_call_input.delta" | "response.custom_tool_call_input.done" => {
            let Some(event) = value.as_object_mut() else {
                return;
            };
            if !event.contains_key("item_id") {
                let item_id = event
                    .get("call_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                event.insert("item_id".to_owned(), serde_json::Value::String(item_id));
            }
        }
        _ => {}
    }
}

/// Convert only nonterminal actionless web-search announcements into the
/// progress event the typed SDK can represent without inventing an action.
fn normalize_actionless_web_search_item_added(value: &mut serde_json::Value) -> bool {
    let Some(event) = value.as_object_mut() else {
        return false;
    };
    let Some(item_id) = (|| {
        let item = event.get("item")?.as_object()?;
        if item.get("type")?.as_str()? != "web_search_call"
            || !matches!(
                item.get("status").and_then(serde_json::Value::as_str),
                Some("in_progress" | "searching")
            )
            || item.get("action").is_some_and(|action| !action.is_null())
        {
            return None;
        }
        item.get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    })() else {
        return false;
    };

    event.insert(
        "type".to_owned(),
        serde_json::Value::String("response.web_search_call.in_progress".to_owned()),
    );
    event.insert("item_id".to_owned(), serde_json::Value::String(item_id));
    true
}

/// Normalize a complete Responses object before handing it to async-openai.
/// This supports both synchronous responses and response objects nested in SSE
/// terminal frames without changing the provider-native conversation model.
fn normalize_response_compat(value: &mut serde_json::Value, default_status: &str) {
    let Some(response) = value.as_object_mut() else {
        return;
    };

    insert_json_default(response, "created_at", serde_json::json!(0));
    insert_json_default(response, "id", serde_json::json!(""));
    insert_json_default(response, "model", serde_json::json!(""));
    insert_json_default(response, "object", serde_json::json!("response"));
    insert_json_default(response, "output", serde_json::json!([]));
    insert_json_default(response, "status", serde_json::json!(default_status));

    if let Some(output) = response
        .get_mut("output")
        .and_then(serde_json::Value::as_array_mut)
    {
        for item in output {
            normalize_response_output_item(item);
        }
    }

    // The response may echo xAI-only hosted tool declarations such as
    // `{"type":"x_search"}`. Retain every tool the typed SDK understands and
    // drop only declarations it cannot represent.
    if let Some(tools) = response
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        tools.retain(|tool| serde_json::from_value::<rs::Tool>(tool.clone()).is_ok());
    }

    if let Some(usage) = response
        .get_mut("usage")
        .and_then(serde_json::Value::as_object_mut)
    {
        insert_json_default(usage, "input_tokens", serde_json::json!(0));
        insert_json_default(usage, "output_tokens", serde_json::json!(0));
        let total_tokens = usage
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
            .saturating_add(
                usage
                    .get("output_tokens")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default(),
            )
            .min(u64::from(u32::MAX));
        insert_json_default(usage, "total_tokens", serde_json::json!(total_tokens));
        insert_json_default(
            usage,
            "input_tokens_details",
            serde_json::json!({"cached_tokens": 0}),
        );
        insert_json_default(
            usage,
            "output_tokens_details",
            serde_json::json!({"reasoning_tokens": 0}),
        );
        if let Some(details) = usage
            .get_mut("input_tokens_details")
            .and_then(serde_json::Value::as_object_mut)
        {
            insert_json_default(details, "cached_tokens", serde_json::json!(0));
        }
        if let Some(details) = usage
            .get_mut("output_tokens_details")
            .and_then(serde_json::Value::as_object_mut)
        {
            insert_json_default(details, "reasoning_tokens", serde_json::json!(0));
        }
    }
}

fn insert_json_default(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    default: serde_json::Value,
) {
    if object.get(key).is_none_or(serde_json::Value::is_null) {
        object.insert(key.to_owned(), default);
    }
}

fn normalize_response_output_item(item: &mut serde_json::Value) {
    normalize_x_search_call(item);
    fill_missing_custom_tool_call_id(item);
    fill_missing_compaction_output_id(item);
    fill_missing_web_search_action(item);
}

/// async-openai 0.33.1 requires `CompactionBody.id`, while the Responses
/// wire contract permits a compaction item without one. Use an empty
/// typed-boundary sentinel; the conversation converter deliberately omits
/// that sentinel from raw provider replay.
fn fill_missing_compaction_output_id(item: &mut serde_json::Value) {
    let Some(item) = item.as_object_mut() else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) == Some("compaction")
        && !item.contains_key("id")
    {
        item.insert("id".to_owned(), serde_json::Value::String(String::new()));
    }
}

/// async-openai 0.33.1 requires `action` on every `WebSearchToolCall`, but
/// Codex can omit it on any frame that carries the item — terminal
/// `response.output_item.done` and `response.completed` outputs included, not
/// just the nonterminal announcements projected to progress events above.
/// Fill the shared empty-search sentinel so the frame parses instead of
/// failing the whole turn; request serialization strips this exact sentinel
/// before provider replay.
fn fill_missing_web_search_action(item: &mut serde_json::Value) {
    let Some(item) = item.as_object_mut() else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) == Some("web_search_call")
        && item.get("action").is_none_or(serde_json::Value::is_null)
    {
        item.insert(
            "action".to_owned(),
            xai_grok_sampling_types::sentinel_web_search_action_json(),
        );
    }
}

/// Project xAI's current hosted `x_search_call` output item into the legacy
/// backend CustomToolCall shape already used throughout Open Grok. This is a
/// typed-SDK adapter only: the tool remains provider-executed and is never
/// exposed to the local tool dispatcher.
fn normalize_x_search_call(item: &mut serde_json::Value) {
    let Some(item) = item.as_object_mut() else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) != Some("x_search_call") {
        return;
    }

    let nonempty_string = |key: &str| {
        item.get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let id = nonempty_string("id")
        .or_else(|| nonempty_string("call_id"))
        .unwrap_or_else(|| "x_search".to_owned());
    let call_id = nonempty_string("call_id").unwrap_or_else(|| id.clone());
    let name = nonempty_string("name").unwrap_or_else(|| "x_search".to_owned());
    let input = nonempty_string("input")
        .or_else(|| nonempty_string("arguments"))
        .or_else(|| {
            item.get("action")
                .and_then(|action| serde_json::to_string(action).ok())
        })
        .unwrap_or_else(|| "{}".to_owned());

    item.insert(
        "type".to_owned(),
        serde_json::Value::String("custom_tool_call".to_owned()),
    );
    item.insert("id".to_owned(), serde_json::Value::String(id));
    item.insert("call_id".to_owned(), serde_json::Value::String(call_id));
    item.insert("name".to_owned(), serde_json::Value::String(name));
    item.insert("input".to_owned(), serde_json::Value::String(input));
}

/// Apply Codex-only fields that async-openai 0.33.1 cannot represent.
///
/// Max and Ultra are distinct local choices, but Codex accepts `max` for both.
/// On live Codex v2 models, Ultra opts into proactive multi-agent delegation.
/// The policy item exists only in this request body and is never persisted to chat.
#[cfg(test)]
fn patch_codex_request_compat(
    request_body: &mut serde_json::Value,
    provider: ModelProvider,
    multi_agent_v2: bool,
    local_effort: Option<ReasoningEffort>,
    reasoning_summary: Option<xai_grok_sampling_types::ReasoningSummary>,
) {
    provider_adapter(provider).patch_responses_request(
        request_body,
        ResponsesRequestPolicy {
            multi_agent_v2,
            local_effort,
            reasoning_summary,
        },
    );
}

#[cfg(test)]
fn is_multi_agent_mode_item(item: &serde_json::Value) -> bool {
    if item.get("role").and_then(serde_json::Value::as_str) != Some("developer") {
        return false;
    }
    match item.get("content") {
        Some(serde_json::Value::String(text)) => text.contains(MULTI_AGENT_MODE_OPEN_TAG),
        Some(serde_json::Value::Array(content)) => content.iter().any(|part| {
            part.get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| text.contains(MULTI_AGENT_MODE_OPEN_TAG))
        }),
        _ => false,
    }
}

fn fill_missing_custom_tool_call_id(item: &mut serde_json::Value) {
    let Some(item) = item.as_object_mut() else {
        return;
    };
    if item.get("type").and_then(serde_json::Value::as_str) != Some("custom_tool_call")
        || item.contains_key("id")
    {
        return;
    }
    let Some(call_id) = item
        .get("call_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    else {
        return;
    };
    item.insert("id".to_owned(), serde_json::Value::String(call_id));
}

/// Restore native custom-output fields that async-openai 0.33.1 cannot
/// represent in its typed request model. Locations are captured from the
/// original [`ConversationRequest`] before conversion, so repeated call IDs,
/// names, or image URLs remain unambiguous.
fn patch_custom_tool_output_wire_fields(
    request_body: &mut serde_json::Value,
    named_outputs: &[NamedCustomToolOutputOccurrence],
    original_images: &[OriginalDetailCustomOutputImageOccurrence],
) {
    let Some(input) = request_body
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };

    for occurrence in named_outputs {
        let Some(item) = input
            .get_mut(occurrence.input_item_index)
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        if item.get("type").and_then(serde_json::Value::as_str) != Some("custom_tool_call_output")
            || item.get("call_id").and_then(serde_json::Value::as_str)
                != Some(occurrence.call_id.as_str())
        {
            continue;
        }
        item.insert(
            "name".to_owned(),
            serde_json::Value::String(occurrence.name.clone()),
        );
    }

    for occurrence in original_images {
        let Some(content) = input
            .get_mut(occurrence.input_item_index)
            .and_then(|item| item.get_mut("output"))
            .and_then(serde_json::Value::as_array_mut)
            .and_then(|output| output.get_mut(occurrence.output_content_index))
            .and_then(serde_json::Value::as_object_mut)
        else {
            continue;
        };
        if content.get("type").and_then(serde_json::Value::as_str) != Some("input_image")
            || content.get("image_url").and_then(serde_json::Value::as_str)
                != Some(occurrence.image_url.as_ref())
        {
            continue;
        }
        content.insert(
            "detail".to_owned(),
            serde_json::Value::String("original".to_owned()),
        );
    }
}

/// Restore opaque provider-native input items after async-openai serializes
/// their typed placeholders. This is used only for Codex replacement history
/// returned by `/responses/compact`.
fn patch_raw_input_replacements(
    request_body: &mut serde_json::Value,
    replacements: &[xai_grok_sampling_types::RawInputItemReplacement],
) -> Result<()> {
    if replacements.is_empty() {
        return Ok(());
    }
    let input = request_body
        .get_mut("input")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or(SamplingError::InvalidConfiguration(
            "Codex raw input replacements require an input array",
        ))?;
    for replacement in replacements {
        let slot = input.get_mut(replacement.input_item_index).ok_or(
            SamplingError::InvalidConfiguration(
                "Codex raw input replacement index is out of range",
            ),
        )?;
        *slot = replacement.value.clone();
    }
    Ok(())
}

fn contains_native_custom_responses_lane(body: &serde_json::Value) -> bool {
    let custom_tool = body
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|tool| tool.get("type").and_then(serde_json::Value::as_str) == Some("custom"));
    let custom_input = body
        .get("input")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .any(|item| {
            item.get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| matches!(kind, "custom_tool_call" | "custom_tool_call_output"))
        });
    let custom_choice = body
        .get("tool_choice")
        .and_then(serde_json::Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("custom");
    custom_tool || custom_input || custom_choice
}

/// Final defense-in-depth boundary before a Responses body reaches the
/// network. Providers without native custom-tool support must receive the
/// function-envelope projection, never `type: "custom"` declarations or
/// custom call/output history.
fn validate_responses_wire_body_for_provider(
    body: &serde_json::Value,
    provider: ModelProvider,
) -> Result<()> {
    if provider.profile().code_mode_transport != CodeModeTransport::NativeCustomGrammar
        && contains_native_custom_responses_lane(body)
    {
        return Err(SamplingError::InvalidConfiguration(
            "selected provider does not support native Responses custom tools",
        ));
    }
    Ok(())
}

fn retain_codex_compact_request_fields(request_body: &mut serde_json::Value) -> Result<()> {
    let body = request_body
        .as_object_mut()
        .ok_or(SamplingError::InvalidConfiguration(
            "Codex compact request did not serialize to an object",
        ))?;
    body.retain(|key, _| {
        matches!(
            key.as_str(),
            "model"
                | "input"
                | "instructions"
                | "tools"
                | "parallel_tool_calls"
                | "reasoning"
                | "service_tier"
                | "prompt_cache_key"
                | "text"
        )
    });
    body.retain(|key, value| {
        !value.is_null() || matches!(key.as_str(), "model" | "input" | "parallel_tool_calls")
    });
    Ok(())
}

fn retain_codex_remote_compaction_v2_request_fields(
    request_body: &mut serde_json::Value,
) -> Result<()> {
    let body = request_body
        .as_object_mut()
        .ok_or(SamplingError::InvalidConfiguration(
            "Codex remote compaction v2 request did not serialize to an object",
        ))?;
    body.retain(|key, _| {
        matches!(
            key.as_str(),
            "model"
                | "input"
                | "instructions"
                | "tools"
                | "tool_choice"
                | "parallel_tool_calls"
                | "reasoning"
                | "service_tier"
                | "prompt_cache_key"
                | "text"
                | "include"
                | "store"
                | "stream"
        )
    });
    body.retain(|key, value| {
        !value.is_null()
            || matches!(
                key.as_str(),
                "model" | "input" | "parallel_tool_calls" | "store" | "stream"
            )
    });
    Ok(())
}

/// On terminal Responses API events (`response.completed` /
/// `response.incomplete`), rewrite `response.usage.total_tokens` to the
/// live context length when the wire includes
/// `response.usage.context_details.{input_tokens, output_tokens}`.
///
/// `total_tokens` drives the CLI's `/context` bar, the auto-compact
/// threshold, and `meta.totalTokens` on persisted sessions. Under
/// server-side multi-turn loops (e.g. `web_search`, `x_search`) the
/// wire's cumulative total inflates as the loop runs; `context_details`
/// reports the final turn's prompt + output tokens — the real live
/// context the model is sitting in. Billing fields
/// (`input_tokens`, `output_tokens`, `input_tokens_details.cached_tokens`,
/// `output_tokens_details.reasoning_tokens`) stay on the cumulative
/// wire values so telemetry is unaffected.
///
/// No-op when:
/// - the event is not terminal,
/// - `response.usage` is `None`,
/// - `context_details` is absent (older backends / non-loop responses),
/// - or either of `context_details.{input_tokens, output_tokens}` is
///   missing — we don't guess the missing half.
fn apply_terminal_event_overrides(event: &mut rs::ResponseStreamEvent, data: &str) {
    let response = match event {
        rs::ResponseStreamEvent::ResponseCompleted(e) => &mut e.response,
        rs::ResponseStreamEvent::ResponseIncomplete(e) => &mut e.response,
        _ => return,
    };
    // Re-parse for fields async_openai's types omit (context total, cost ticks).
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return;
    };
    // Stash cost ticks in metadata for stream_responses.
    if let Some(ticks) = xai_grok_sampling_types::reported_cost_ticks(
        value
            .pointer("/response/usage/cost_in_usd_ticks")
            .and_then(|v| v.as_i64()),
    ) {
        response
            .metadata
            .get_or_insert_with(Default::default)
            .insert(COST_USD_TICKS_METADATA_KEY.to_owned(), ticks.to_string());
    }
    let Some(usage) = response.usage.as_mut() else {
        return;
    };
    let Some(total) = extract_context_total(&value) else {
        return;
    };
    usage.total_tokens = total;
}

/// Metadata key for cost ticks past typed Response events.
pub(crate) const COST_USD_TICKS_METADATA_KEY: &str = "xai.cost_usd_ticks";

/// Read `response.usage.context_details.{input_tokens, output_tokens}`
/// from the parsed terminal-event JSON and return their sum. Returns `None`
/// if either field is missing or out of `u32` range.
fn extract_context_total(value: &serde_json::Value) -> Option<u32> {
    let cd = value.pointer("/response/usage/context_details")?;
    let i = u32::try_from(cd.get("input_tokens")?.as_u64()?).ok()?;
    let o = u32::try_from(cd.get("output_tokens")?.as_u64()?).ok()?;
    Some(i.saturating_add(o))
}

/// Record `success=false` + `error` on the active inference span when a stream
/// request fails before any response (transport/connect/TLS errors). Without
/// this the `#[instrument]` span closes with both fields Empty, so an outage
/// shows zero `success=false` and error-rate alerts never fire.
fn record_stream_request_failure(err: &reqwest::Error) {
    let span = tracing::Span::current();
    span.record("success", false);
    span.record("error", err.to_string().as_str());
}

fn extract_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(|s| s.min(120))
}

fn extract_should_retry(headers: &reqwest::header::HeaderMap) -> Option<bool> {
    headers
        .get("x-should-retry")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            if s.eq_ignore_ascii_case("true") {
                Some(true)
            } else if s.eq_ignore_ascii_case("false") {
                Some(false)
            } else {
                None // unknown value — treat as absent
            }
        })
}

fn extract_model_metadata(headers: &reqwest::header::HeaderMap) -> Option<ResponseModelMetadata> {
    let context_window = headers
        .get("x-grok-context-window")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let max_completion_tokens = headers
        .get("x-grok-max-completion-tokens")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u32>().ok());

    let models_etag = headers
        .get("x-models-etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if context_window.is_some() || max_completion_tokens.is_some() || models_etag.is_some() {
        Some(ResponseModelMetadata {
            context_window,
            max_completion_tokens,
            models_etag,
        })
    } else {
        None
    }
}

/// Wrapper for streaming chat completion requests that adds `stream` and
/// `stream_options` fields without modifying the original `ChatCompletionRequest`.
///
/// Uses `#[serde(flatten)]` to inline all fields from the inner request,
/// allowing single-pass serialization instead of the previous two-pass
/// approach (serialize to `Value`, mutate, serialize to bytes).
#[derive(Serialize)]
struct StreamingChatRequest<'a> {
    #[serde(flatten)]
    inner: &'a ChatCompletionRequest,
    stream: bool,
    stream_options: StreamOptions,
}

#[derive(Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Deserialize)]
struct CodexCompactHistoryResponse {
    output: Vec<serde_json::Value>,
}

/// Successful Codex remote-compaction-v2 stream result.
///
/// The output item is stored in the unified conversation's opaque Codex
/// carrier. `response_id` and `usage` come from the required terminal
/// `response.completed` event and are exposed for lifecycle telemetry; HTTP
/// callers must not send the ID back as `previous_response_id`.
#[derive(Debug, Clone)]
pub struct CodexRemoteCompactionV2Result {
    pub compaction_item: xai_grok_sampling_types::ConversationItem,
    pub response_id: String,
    pub usage: Option<rs::ResponseUsage>,
}

#[derive(Default)]
struct CodexRemoteCompactionV2Collector {
    output_item_count: usize,
    compaction_items: Vec<serde_json::Value>,
    completed_response_id: Option<String>,
    completed_usage: Option<rs::ResponseUsage>,
    saw_completed: bool,
}

impl CodexRemoteCompactionV2Collector {
    #[cfg(test)]
    fn absorb(
        &mut self,
        event_name: &str,
        data: &str,
        codex_turn_state: Option<&Arc<OnceLock<String>>>,
    ) -> Result<()> {
        self.absorb_with_adapter(
            provider_adapter(ModelProvider::Codex),
            event_name,
            data,
            codex_turn_state,
        )
    }

    fn absorb_with_adapter(
        &mut self,
        adapter: &dyn ProviderAdapter,
        event_name: &str,
        data: &str,
        codex_turn_state: Option<&Arc<OnceLock<String>>>,
    ) -> Result<()> {
        if adapter.absorb_response_metadata(event_name, data, codex_turn_state) {
            return Ok(());
        }
        if let Some(error) = try_parse_stream_error(data) {
            return Err(error);
        }
        let value = serde_json::from_str::<serde_json::Value>(data)
            .map_err(SamplingError::Serialization)?;
        let event_type = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(event_name);
        match event_type {
            "response.output_item.done" => {
                self.output_item_count = self.output_item_count.saturating_add(1);
                let item = value
                    .get("item")
                    .and_then(serde_json::Value::as_object)
                    .ok_or_else(|| {
                        SamplingError::serialization_message(
                            "Codex remote compaction v2 output_item.done contained no item",
                        )
                    })?;
                if item.get("type").and_then(serde_json::Value::as_str) == Some("compaction") {
                    self.compaction_items
                        .push(serde_json::Value::Object(item.clone()));
                }
            }
            "response.completed" => {
                let response = value
                    .get("response")
                    .and_then(serde_json::Value::as_object)
                    .ok_or_else(|| {
                        SamplingError::serialization_message(
                            "Codex remote compaction v2 response.completed contained no response",
                        )
                    })?;
                let response_id = response
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        SamplingError::serialization_message(
                            "Codex remote compaction v2 response.completed contained no response id",
                        )
                    })?;
                self.completed_response_id = Some(response_id.to_owned());
                self.completed_usage = response
                    .get("usage")
                    .filter(|usage| !usage.is_null())
                    .cloned()
                    .map(normalize_codex_remote_compaction_usage)
                    .transpose()?;
                self.saw_completed = true;
            }
            _ => {
                // Compaction streams may include ordinary message, reasoning,
                // progress, and future side-channel events. Only the durable
                // output_item.done compaction and terminal completion matter.
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<CodexRemoteCompactionV2Result> {
        if !self.saw_completed {
            return Err(SamplingError::EventStreamError(
                "Codex remote compaction v2 stream closed before response.completed".to_owned(),
            ));
        }
        if self.compaction_items.len() != 1 {
            return Err(SamplingError::serialization_message(format!(
                "Codex remote compaction v2 expected exactly one compaction output item, got {} from {} output items",
                self.compaction_items.len(),
                self.output_item_count,
            )));
        }
        let mut items = xai_grok_sampling_types::codex_compact_output_to_conversation_items(
            self.compaction_items,
        )
        .map_err(SamplingError::serialization_message)?;
        let compaction_item = items.pop().ok_or_else(|| {
            SamplingError::serialization_message(
                "Codex remote compaction v2 produced no replayable compaction item",
            )
        })?;
        Ok(CodexRemoteCompactionV2Result {
            compaction_item,
            response_id: self.completed_response_id.ok_or_else(|| {
                SamplingError::serialization_message(
                    "Codex remote compaction v2 completed without a response id",
                )
            })?,
            usage: self.completed_usage,
        })
    }
}

fn normalize_codex_remote_compaction_usage(usage: serde_json::Value) -> Result<rs::ResponseUsage> {
    let mut usage = usage
        .as_object()
        .cloned()
        .ok_or_else(|| SamplingError::serialization_message("Responses usage is not an object"))?;
    insert_json_default(&mut usage, "input_tokens", serde_json::json!(0));
    insert_json_default(&mut usage, "output_tokens", serde_json::json!(0));
    let total_tokens = usage
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default()
        .saturating_add(
            usage
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default(),
        )
        .min(u64::from(u32::MAX));
    insert_json_default(&mut usage, "total_tokens", serde_json::json!(total_tokens));
    insert_json_default(
        &mut usage,
        "input_tokens_details",
        serde_json::json!({"cached_tokens": 0}),
    );
    insert_json_default(
        &mut usage,
        "output_tokens_details",
        serde_json::json!({"reasoning_tokens": 0}),
    );
    if let Some(details) = usage
        .get_mut("input_tokens_details")
        .and_then(serde_json::Value::as_object_mut)
    {
        insert_json_default(details, "cached_tokens", serde_json::json!(0));
    }
    if let Some(details) = usage
        .get_mut("output_tokens_details")
        .and_then(serde_json::Value::as_object_mut)
    {
        insert_json_default(details, "reasoning_tokens", serde_json::json!(0));
    }
    serde_json::from_value(serde_json::Value::Object(usage)).map_err(SamplingError::Serialization)
}

/// Resolve `env_http_headers` (`header -> env var`) into `headers` via `getenv`, skipping unset/blank/invalid entries and trimming values.
fn apply_env_http_headers(
    env_http_headers: &IndexMap<String, String>,
    getenv: impl Fn(&str) -> Option<String>,
    headers: &mut HeaderMap,
) {
    for (key, env_var) in env_http_headers {
        let Some(value) = getenv(env_var) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let (Ok(name), Ok(header_value)) = (
            HeaderName::try_from(key.as_str()),
            HeaderValue::from_str(value),
        ) else {
            tracing::warn!(
                header = %key,
                env_var = %env_var,
                "skipping env_http_header with an invalid header name or value"
            );
            continue;
        };
        headers.insert(name, header_value);
    }
}

/// HTTP client for sampling. Cheap to clone; carries an `Arc`-backed
/// `reqwest::Client` and the default headers/request-defaults computed from a
/// [`SamplerConfig`] at construction time.
#[derive(Clone)]
pub struct SamplingClient {
    http: reqwest::Client,
    default_headers: HeaderMap,
    base_url: String,
    defaults: ClientDefaults,
    provider_adapter: &'static dyn ProviderAdapter,
    /// Optional 401-attribution hook. The shell wires this to emit a
    /// structured event at every UNAUTHORIZED arm so 401s can be
    /// bucketed by stale-snapshot vs. live-token-rejected. `None` for
    /// sampler-only callers and tests.
    attribution_callback: Option<crate::attribution::SharedAttributionCallback>,
    /// Per-request bearer override. See `SamplerConfig::bearer_resolver`.
    bearer_resolver: Option<crate::config::SharedBearerResolver>,
    /// Per-request header injection (OTel traceparent).
    header_injector: Option<crate::config::SharedHeaderInjector>,
    /// First-value-wins sticky-routing token for one logical Codex turn.
    /// `None` for every non-Codex/non-Responses client.
    codex_turn_state: Option<Arc<OnceLock<String>>>,
    /// Endpoint URL builder, resolved once from `base_url` + `query_params`.
    endpoint: EndpointTemplate,
}

impl std::fmt::Debug for SamplingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamplingClient")
            .field("base_url", &self.base_url)
            .field("defaults", &self.defaults)
            .field("provider_adapter", &self.provider_adapter.provider())
            .field(
                "has_attribution_callback",
                &self.attribution_callback.is_some(),
            )
            .field("has_bearer_resolver", &self.bearer_resolver.is_some())
            .field("has_codex_turn_state", &self.codex_turn_state.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, Default)]
struct ClientDefaults {
    model: String,
    max_completion_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    api_backend: ApiBackend,
    provider: xai_grok_sampling_types::ModelProvider,
    auth_scheme: AuthScheme,
    stream_tool_calls: bool,
    idle_timeout_secs: Option<u64>,
    codex_multi_agent_v2: bool,
    reasoning_effort: Option<xai_grok_sampling_types::ReasoningEffort>,
    reasoning_summary: Option<xai_grok_sampling_types::ReasoningSummary>,
    doom_loop_recovery: Option<xai_grok_sampling_types::DoomLoopRecoveryPolicy>,
}

/// Endpoint URL builder, resolved once at client construction so each request
/// only appends its path.
#[derive(Clone, Debug)]
enum EndpointTemplate {
    /// No query params and no query on the base URL (or an unparseable base):
    /// append the path to the base verbatim.
    Plain(String),
    /// Query params configured: `{prefix}/{path}{suffix}`. `suffix` starts with
    /// `?` and folds any base-URL params, with a configured key winning over the
    /// same key in `base_url` (percent-encoded, no duplicates).
    WithQuery { prefix: String, suffix: String },
}

impl EndpointTemplate {
    fn new(base_url: &str, query_params: &IndexMap<String, String>) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        // The fast path is safe only when there is nothing to fold: no configured
        // params and no query already on the base (which would otherwise land
        // before the appended path).
        if query_params.is_empty() && !base.contains('?') {
            return Self::Plain(base);
        }
        let mut url = match reqwest::Url::parse(&base) {
            Ok(url) => url,
            Err(error) => {
                tracing::warn!(
                    url = %base,
                    %error,
                    "failed to parse base URL for endpoint; sending without folded query"
                );
                return Self::Plain(base);
            }
        };
        let overridden: std::collections::HashSet<&str> =
            query_params.keys().map(String::as_str).collect();
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| !overridden.contains(k.as_ref()))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        let prefix = {
            let mut prefix_url = url.clone();
            prefix_url.set_query(None);
            prefix_url.as_str().trim_end_matches('/').to_string()
        };
        {
            let mut pairs = url.query_pairs_mut();
            pairs.clear();
            for (key, value) in &kept {
                pairs.append_pair(key, value);
            }
            for (key, value) in query_params {
                pairs.append_pair(key, value);
            }
        }
        let suffix = url.query().map(|q| format!("?{q}")).unwrap_or_default();
        Self::WithQuery { prefix, suffix }
    }

    fn url_for_path(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        match self {
            Self::Plain(base) => format!("{base}/{path}"),
            Self::WithQuery { prefix, suffix } => format!("{prefix}/{path}{suffix}"),
        }
    }
}

// =============================================================================
// User-Agent helpers
// =============================================================================

#[derive(Clone, Debug, Eq, PartialEq)]
struct PlatformInfo {
    os: String,
    arch: String,
}

impl PlatformInfo {
    fn current() -> Self {
        let os = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            other => other,
        }
        .to_string();

        let arch = match std::env::consts::ARCH {
            "arm64" => "aarch64",
            "x86_64" => "x86_64",
            other => other,
        }
        .to_string();

        Self { os, arch }
    }
}

fn agent_version() -> String {
    xai_grok_version::VERSION.to_string()
}

/// Render a User-Agent string for the given origin client.
///
/// Mirrors the shell's `user_agent_string_for` but uses sampler-local
/// constants. The session typically owns the canonical User-Agent
/// rendering for process-wide HTTP clients; this helper is for
/// per-session sampling clients that want to override it.
pub fn user_agent_string_for(origin: &OriginClientInfo) -> String {
    let agent_version = agent_version();
    let platform = PlatformInfo::current();

    if origin.product == AGENT_PRODUCT && origin.version.as_deref() == Some(agent_version.as_str())
    {
        return format!(
            "{}/{} ({}; {})",
            AGENT_PRODUCT, agent_version, platform.os, platform.arch
        );
    }

    match origin.version.as_deref() {
        Some(origin_version) => format!(
            "{}/{} {}/{} ({}; {})",
            origin.product,
            origin_version,
            AGENT_PRODUCT,
            agent_version,
            platform.os,
            platform.arch
        ),
        None => format!(
            "{} {}/{} ({}; {})",
            origin.product, AGENT_PRODUCT, agent_version, platform.os, platform.arch
        ),
    }
}

// =============================================================================
// SamplingClient
// =============================================================================

impl SamplingClient {
    /// Construct a sampling client from a [`SamplerConfig`].
    ///
    /// Grabs the process-wide shared `reqwest::Client` (HTTP/2 by
    /// default, HTTP/1.1 when `config.force_http1` is set) and
    /// pre-computes the default request headers. This does not perform
    /// any network I/O.
    pub fn new(config: SamplerConfig) -> Result<Self> {
        Self::new_inner(config, None)
    }

    /// Construct a client attached to a single logical Codex turn.
    ///
    /// The state is ignored unless this is a Codex Responses client, so the
    /// provider-private header can never cross onto xAI or another backend.
    pub fn new_with_codex_turn_state(
        config: SamplerConfig,
        codex_turn_state: Arc<OnceLock<String>>,
    ) -> Result<Self> {
        Self::new_inner(config, Some(codex_turn_state))
    }

    fn new_inner(
        config: SamplerConfig,
        codex_turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<Self> {
        let provider_adapter = provider_adapter(config.provider);
        provider_adapter.validate_backend(&config.api_backend)?;
        let codex_turn_state = provider_adapter
            .supports_turn_state(&config.api_backend)
            .then_some(codex_turn_state)
            .flatten();
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if let Some(ref api_key) = config.api_key {
            match config.auth_scheme {
                AuthScheme::XApiKey => {
                    let header_value = HeaderValue::from_str(api_key).map_err(|_| {
                        tracing::debug!(
                            "Invalid api_key: cannot be converted to a valid HTTP header"
                        );
                        SamplingError::Auth(
                            "Invalid api_key: cannot be converted to a valid HTTP header"
                                .to_string(),
                        )
                    })?;
                    headers.insert(HeaderName::from_static("x-api-key"), header_value);
                }
                AuthScheme::Bearer => {
                    let bearer = format!("Bearer {}", api_key);
                    let header_value = HeaderValue::from_str(&bearer).map_err(|_| {
                        tracing::debug!(
                            "Invalid api_key: cannot be converted to a valid HTTP Authorization header"
                        );
                        SamplingError::Auth(
                            "Invalid api_key: cannot be converted to a valid HTTP Authorization header"
                                .to_string(),
                        )
                    })?;
                    headers.insert(AUTHORIZATION, header_value);
                }
            }
        }

        // Apply all extra headers verbatim. This is the single
        // injection point for proxy-auth headers and any other URL- or
        // environment-specific headers the session decides to set.
        for (key, value) in &config.extra_headers {
            let header_name = HeaderName::try_from(key.as_str())
                .map_err(|_| SamplingError::InvalidConfiguration("Invalid extra header name"))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|_| SamplingError::InvalidConfiguration("Invalid extra header value"))?;
            headers.insert(header_name, header_value);
        }

        // Resolve here, not into `extra_headers`, so an env-sourced secret stays
        // out of persisted state.
        apply_env_http_headers(
            &config.env_http_headers,
            |var| std::env::var(var).ok(),
            &mut headers,
        );

        if let Some(resolver) = &config.bearer_resolver {
            for name in resolver.reserved_headers() {
                headers.remove(*name);
            }
        }

        // Provider-private identity headers are transport policy, independent
        // of the auth headers resolved above.
        provider_adapter.apply_default_headers(&mut headers, &config);

        // Always set User-Agent: per-session origin if available, else fallback.
        {
            let ua_string = match config.origin_client.as_ref() {
                Some(origin) => user_agent_string_for(origin),
                None => user_agent_string_for(&OriginClientInfo {
                    product: AGENT_PRODUCT.to_string(),
                    version: Some(agent_version()),
                }),
            };
            if let Ok(v) = HeaderValue::from_str(&ua_string) {
                headers.insert(USER_AGENT, v);
            }
        }

        let http = if config.force_http1 {
            tracing::info!("Using HTTP/1.1 for sampling client (force_http1=true)");
            crate::shared_http::client_http1().map_err(SamplingError::Http)?
        } else {
            crate::shared_http::client().map_err(SamplingError::Http)?
        };

        tracing::info!(
            target: crate::sampling_log::TARGET,
            event = "client_new",
            base_url = %config.base_url,
            model = %config.model,
            api_backend = ?config.api_backend,
            provider = ?config.provider,
            auth_scheme = ?config.auth_scheme,
            // "unset" (not "none"): `ReasoningEffort::None` is a real wire value;
            // logging the absent Option as "none" looked like we were sending it.
            reasoning_effort = config.reasoning_effort.map_or("unset", |e| e.as_str()),
            has_api_key = config.api_key.is_some(),
            has_bearer_resolver = config.bearer_resolver.is_some(),
            has_authorization_header = headers.get(AUTHORIZATION).is_some(),
            has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
        );

        let defaults = ClientDefaults {
            model: config.model,
            max_completion_tokens: config.max_completion_tokens,
            temperature: config.temperature,
            top_p: config.top_p,
            api_backend: config.api_backend,
            provider: config.provider,
            auth_scheme: config.auth_scheme,
            stream_tool_calls: config.stream_tool_calls,
            idle_timeout_secs: config.idle_timeout_secs,
            codex_multi_agent_v2: config.codex_multi_agent_v2,
            reasoning_effort: config.reasoning_effort,
            reasoning_summary: config.reasoning_summary,
            doom_loop_recovery: config.doom_loop_recovery,
        };

        let endpoint = EndpointTemplate::new(&config.base_url, &config.query_params);

        Ok(Self {
            http,
            default_headers: headers,
            base_url: config.base_url,
            defaults,
            provider_adapter,
            attribution_callback: config.attribution_callback,
            bearer_resolver: config.bearer_resolver,
            header_injector: config.header_injector,
            codex_turn_state,
            endpoint,
        })
    }

    /// The configured API backend for this client.
    pub fn api_backend(&self) -> ApiBackend {
        self.defaults.api_backend
    }

    /// Provider selected when this client was constructed.
    pub fn provider(&self) -> xai_grok_sampling_types::ModelProvider {
        self.defaults.provider
    }

    fn capture_codex_turn_state(&self, headers: &HeaderMap) {
        self.provider_adapter
            .capture_turn_state(headers, self.codex_turn_state.as_ref());
    }

    /// Replace a live bearer resolver without rebuilding the client's model,
    /// endpoint, or request defaults. Returns `false` when this client was
    /// constructed with static credentials and therefore must not be upgraded
    /// into session auth (for example, an explicit BYOK auxiliary model).
    pub fn replace_bearer_resolver_if_present(
        &mut self,
        resolver: crate::config::SharedBearerResolver,
    ) -> bool {
        let Some(previous) = self.bearer_resolver.as_ref() else {
            return false;
        };
        for name in previous
            .reserved_headers()
            .iter()
            .chain(resolver.reserved_headers())
        {
            self.default_headers.remove(*name);
        }
        self.bearer_resolver = Some(resolver);
        true
    }

    /// POST with default headers. Overrides auth from resolver if wired.
    fn post(&self, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
        let mut headers = self.default_headers.clone();
        if let Some(resolver) = &self.bearer_resolver {
            for name in resolver.reserved_headers() {
                headers.remove(*name);
            }
            let resolved = resolver.current_auth();
            if resolved.is_some() || resolver.fail_closed_on_missing() {
                headers.remove(AUTHORIZATION);
                headers.remove(HeaderName::from_static("x-api-key"));
            }
            if let Some(resolved) = resolved {
                match self.defaults.auth_scheme {
                    AuthScheme::XApiKey => {
                        if let Ok(value) = HeaderValue::from_str(&resolved.bearer) {
                            headers.insert(HeaderName::from_static("x-api-key"), value);
                        }
                    }
                    AuthScheme::Bearer => {
                        if let Ok(value) =
                            HeaderValue::from_str(&format!("Bearer {}", resolved.bearer))
                        {
                            headers.insert(AUTHORIZATION, value);
                        }
                    }
                }
                for (name, value) in resolved.extra_headers {
                    match (
                        HeaderName::try_from(name.as_str()),
                        HeaderValue::from_str(&value),
                    ) {
                        (Ok(name), Ok(value)) => {
                            headers.insert(name, value);
                        }
                        _ => {
                            tracing::warn!(
                                header = %name,
                                "live auth resolver returned an invalid provider header"
                            );
                        }
                    }
                }
            }
        }
        tracing::info!(
            target: crate::sampling_log::TARGET,
            event = "client_post",
            base_url = %self.base_url,
            model = %self.defaults.model,
            api_backend = ?self.defaults.api_backend,
            provider = ?self.defaults.provider,
            auth_scheme = ?self.defaults.auth_scheme,
            has_bearer_resolver = self.bearer_resolver.is_some(),
            has_authorization_header = headers.get(AUTHORIZATION).is_some(),
            has_x_api_key_header = headers.get(HeaderName::from_static("x-api-key")).is_some(),
        );
        if let Some(injector) = &self.header_injector {
            injector.inject(&mut headers);
        }

        self.provider_adapter.sanitize_headers(&mut headers);

        self.provider_adapter
            .apply_turn_state_header(&mut headers, self.codex_turn_state.as_ref());
        self.http.post(url).headers(headers)
    }

    /// Bearer prefix for 401 attribution. Prefers live resolver, falls back to default_headers.
    fn current_sent_bearer_prefix(&self) -> Option<String> {
        let bearer = match self.bearer_resolver.as_ref() {
            Some(resolver) => resolver.current_bearer().or_else(|| {
                (!resolver.fail_closed_on_missing())
                    .then(|| self.extract_sent_bearer())
                    .flatten()
            }),
            None => self.extract_sent_bearer(),
        };
        bearer.map(|mut s| {
            s.truncate(crate::attribution::SENT_BEARER_PREFIX_LEN.min(s.len()));
            s
        })
    }

    /// Extract the bearer from `default_headers`, truncated to prefix length.
    /// Reads `x-api-key` (Anthropic Messages API) or `Authorization` (OpenAI-completions).
    fn extract_sent_bearer(&self) -> Option<String> {
        let raw = match self.defaults.auth_scheme {
            AuthScheme::XApiKey => self
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string()),
            AuthScheme::Bearer => self
                .default_headers
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.strip_prefix("Bearer "))
                .map(|s| s.to_string()),
        };
        raw.map(|mut s| {
            // Truncate in-place so we never materialize a heap-resident
            // copy of the full bearer outside the local stack of this
            // function. `String::truncate` operates on byte indices and
            // panics on a non-char-boundary cut; bearer tokens are
            // ASCII (per the `Authorization` and `x-api-key` header
            // grammars) so the byte index is always safe.
            s.truncate(crate::attribution::SENT_BEARER_PREFIX_LEN.min(s.len()));
            s
        })
    }

    /// Invoke the optional 401 attribution callback for one logical
    /// 401 response. Each of the six UNAUTHORIZED arms in this file
    /// calls this helper immediately before returning
    /// `SamplingError::Auth(...)`. Emit happens at the lowest layer
    /// that saw the status, so higher layers that react to a 401 must
    /// not emit a duplicate event.
    ///
    /// The bearer passed to the callback is already truncated to
    /// [`crate::attribution::SENT_BEARER_PREFIX_LEN`] characters by
    /// [`Self::extract_sent_bearer`]; the trait contract guarantees
    /// that callers downstream of this crate never see the full
    /// bearer.
    fn record_401_attribution(&self, consumer: crate::attribution::SamplingConsumer) {
        if let Some(cb) = self.attribution_callback.as_ref() {
            let sent_prefix = self.current_sent_bearer_prefix();
            cb.record_401(consumer, sent_prefix.as_deref());
        }
    }

    pub fn auth_info(&self) -> crate::sampling_log::AuthInfo {
        let auth_prefix = self.current_sent_bearer_prefix();
        let auth_type = match (&self.defaults.auth_scheme, &auth_prefix) {
            (AuthScheme::XApiKey, Some(_)) => "x-api-key",
            (AuthScheme::Bearer, Some(_)) => "bearer",
            (_, None) => "none",
        };
        crate::sampling_log::AuthInfo {
            auth_type,
            auth_prefix,
        }
    }

    /// Check if a header name contains sensitive information that should be redacted.
    fn is_sensitive_header(name: &str) -> bool {
        let lower = name.to_lowercase();
        lower.contains("authorization")
            || lower.contains("api-key")
            || lower.contains("apikey")
            || lower.contains("token")
            || lower.contains("secret")
            || lower == X_CODEX_TURN_STATE_HEADER
    }

    /// Short lossy body snippet for error logs (never user-facing).
    fn body_preview(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).chars().take(500).collect()
    }

    /// Log all headers from a request at debug level (redacting sensitive values).
    fn log_request_headers(request: &reqwest::Request, endpoint_name: &str) {
        for (name, value) in request.headers().iter() {
            let value_str = if Self::is_sensitive_header(name.as_str()) {
                "[REDACTED]"
            } else {
                value.to_str().unwrap_or("[non-utf8]")
            };
            tracing::debug!(
                header_name = %name,
                header_value = %value_str,
                "Request header ({})",
                endpoint_name
            );
        }
    }

    fn endpoint(&self, path: &str) -> String {
        self.endpoint.url_for_path(path)
    }

    fn apply_defaults(&self, mut request: ChatCompletionRequest) -> Result<ChatCompletionRequest> {
        if request.model.is_none() {
            request.model = Some(self.defaults.model.clone());
        }

        if request.max_tokens.is_none() {
            request.max_tokens = self.defaults.max_completion_tokens;
        }

        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }

        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }

        self.provider_adapter.sanitize_chat_request(&mut request);

        Ok(request)
    }

    async fn handle_response(&self, response: reqwest::Response) -> Result<ChatCompletionResponse> {
        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(crate::attribution::SamplingConsumer::ChatCompletions);
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401): {server_message}"
                )));
            }
            let message = user_facing_api_error_message(status, bytes.as_ref());
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let completion = serde_json::from_slice::<ChatCompletionResponse>(&bytes).map_err(|e| {
            let raw_body = String::from_utf8_lossy(&bytes);
            tracing::error!(
                error = %e,
                raw_body = %raw_body,
                "Failed to deserialize ChatCompletionResponse"
            );
            SamplingError::Serialization(e)
        })?;
        Ok(completion)
    }

    // =========================================================================
    // Chat Completions API
    // =========================================================================

    pub async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse> {
        let payload = self.apply_defaults(request)?;
        let x_grok_conv_id = &payload.x_grok_conv_id.clone().unwrap_or_default();
        let x_grok_req_id = &payload.x_grok_req_id.clone().unwrap_or_default();
        let model_id = payload.model.clone().unwrap_or_default();

        tracing::debug!(
            base_url = %self.base_url,
            model_id = %model_id,
            "Sending chat completion request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: payload.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: payload.x_grok_turn_idx.as_deref(),
            agent_id: payload.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: payload.x_grok_deployment_id.as_deref(),
            user_id: payload.x_grok_user_id.as_deref(),
        };
        let http_request = self
            .provider_adapter
            .apply_request_headers(self.post(self.endpoint("chat/completions")), grok_headers)
            .json(&payload);

        let response = http_request.send().await.map_err(|e| {
            // Log at debug level; errors are surfaced to the caller.
            tracing::debug!("HTTP request failed: {}", e);
            e
        })?;

        self.handle_response(response).await
    }

    /// Start a streaming chat completion request. Returns a stream of typed chunks.
    #[tracing::instrument(
        name = "http.chat_completion_stream",
        skip_all,
        fields(
            endpoint = %self.endpoint("chat/completions"),
            model_id = request.model.as_deref().unwrap_or(""),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub async fn chat_completion_stream(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        let payload = self.apply_defaults(request)?;
        let x_grok_conv_id = &payload.x_grok_conv_id.clone().unwrap_or_default();
        let x_grok_req_id = &payload.x_grok_req_id.clone().unwrap_or_default();
        let model_id = payload.model.clone().unwrap_or_default();

        // Wrap the request with streaming fields and serialize once.
        // Previously this path serialized twice: first to serde_json::Value
        // (to inject `stream` and `stream_options`), then to HTTP body bytes.
        let streaming_request = StreamingChatRequest {
            inner: &payload,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: payload.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: payload.x_grok_turn_idx.as_deref(),
            agent_id: payload.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: payload.x_grok_deployment_id.as_deref(),
            user_id: payload.x_grok_user_id.as_deref(),
        };
        let http_request = self
            .provider_adapter
            .apply_request_headers(self.post(self.endpoint("chat/completions")), grok_headers)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .json(&streaming_request);

        let built_request = http_request.build().map_err(|e| {
            tracing::error!("Failed to build HTTP request: {}", e);
            SamplingError::Http(e)
        })?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending chat/completions request"
        );
        Self::log_request_headers(&built_request, "chat/completions");

        let response = self.http.execute(built_request).await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            record_stream_request_failure(&e);
            e
        })?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                self.record_401_attribution(
                    crate::attribution::SamplingConsumer::ChatCompletionsStream,
                );
                let endpoint = self.endpoint("chat/completions");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }

            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "chat/completions API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        // Strip UTF-8 BOM if present: eventsource-stream 0.2.3 incorrectly slices BOM at byte 1 instead of 3.
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        // Map SSE events into ChatCompletionChunk.
        // Uses `scan` so that `[DONE]` and transport errors both terminate the
        // stream (`None`). The first transport error is emitted to the consumer,
        // then subsequent polls return `None` -- preventing an infinite busy-loop
        // when the HTTP/2 connection drops and h2 keeps producing errors.
        let chunks = event_stream
            .scan(false, |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "chat_completions",
                            data = %data,
                        );

                        if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Err(stream_error))
                        } else {
                            Some(
                                serde_json::from_str::<ChatCompletionChunk>(data).map_err(|e| {
                                    tracing::error!(
                                        error = %e,
                                        raw_data = %data,
                                        "Failed to deserialize ChatCompletionChunk from stream"
                                    );
                                    SamplingError::Serialization(e)
                                }),
                            )
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Err(SamplingError::EventStreamError(e.to_string())))
                    }
                };
                std::future::ready(item)
            })
            .boxed();

        Ok((chunks, model_metadata))
    }

    // =========================================================================
    // Responses API
    // =========================================================================

    fn codex_compaction_request_body(
        &self,
        request: &ConversationRequest,
        instructions: &str,
        remote_v2: bool,
    ) -> Result<serde_json::Value> {
        let extra_tool_entries = xai_grok_sampling_types::extra_tool_entries(&request.hosted_tools);
        let named_custom_tool_outputs = request.named_custom_tool_outputs();
        let mut original_detail_custom_output_images =
            request.original_detail_custom_output_images();
        original_detail_custom_output_images
            .extend(request.original_detail_function_output_images());
        let raw_input_replacements = request.raw_codex_input_replacements();
        let local_reasoning_effort = request.reasoning_effort;

        let mut inner: rs::CreateResponse = request.into();
        inner.instructions = (!instructions.is_empty()).then(|| instructions.to_owned());
        inner.parallel_tool_calls = Some(true);
        if inner.prompt_cache_key.is_none() {
            inner.prompt_cache_key = self
                .provider_adapter
                .prompt_cache_key(request.x_grok_session_id.as_deref());
        }
        if remote_v2 {
            inner.store = Some(false);
            inner.stream = Some(true);
            let include = inner.include.get_or_insert_with(Vec::new);
            if !include.contains(&rs::IncludeEnum::ReasoningEncryptedContent) {
                include.push(rs::IncludeEnum::ReasoningEncryptedContent);
            }
        }
        let mut request_body = serde_json::to_value(inner).map_err(|error| {
            tracing::error!(%error, remote_v2, "failed to serialize Codex compact request");
            SamplingError::Serialization(error)
        })?;
        if !extra_tool_entries.is_empty() {
            if let Some(tools) = request_body
                .get_mut("tools")
                .and_then(serde_json::Value::as_array_mut)
            {
                tools.extend(extra_tool_entries);
            } else {
                request_body["tools"] = serde_json::Value::Array(extra_tool_entries);
            }
        }
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        xai_grok_sampling_types::strip_sentinel_web_search_actions(&mut request_body);
        patch_custom_tool_output_wire_fields(
            &mut request_body,
            &named_custom_tool_outputs,
            &original_detail_custom_output_images,
        );
        patch_raw_input_replacements(&mut request_body, &raw_input_replacements)?;
        self.provider_adapter.patch_responses_request(
            &mut request_body,
            ResponsesRequestPolicy {
                multi_agent_v2: self.defaults.codex_multi_agent_v2,
                local_effort: local_reasoning_effort,
                reasoning_summary: self.defaults.reasoning_summary,
            },
        );
        if remote_v2 {
            if request_body
                .get("tool_choice")
                .is_none_or(serde_json::Value::is_null)
            {
                request_body["tool_choice"] = serde_json::Value::String("auto".to_owned());
            }
            retain_codex_remote_compaction_v2_request_fields(&mut request_body)?;
            request_body
                .get_mut("input")
                .and_then(serde_json::Value::as_array_mut)
                .ok_or(SamplingError::InvalidConfiguration(
                    "Codex remote compaction v2 request requires an input array",
                ))?
                .push(serde_json::json!({"type": "compaction_trigger"}));
        } else {
            retain_codex_compact_request_fields(&mut request_body)?;
        }
        Ok(request_body)
    }

    fn codex_remote_compaction_v2_beta_header(&self) -> Result<HeaderValue> {
        let mut features = self
            .default_headers
            .get(X_CODEX_BETA_FEATURES_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|feature| !feature.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !features
            .iter()
            .any(|feature| feature == REMOTE_COMPACTION_V2_FEATURE)
        {
            features.push(REMOTE_COMPACTION_V2_FEATURE.to_owned());
        }
        HeaderValue::from_str(&features.join(",")).map_err(|_| {
            SamplingError::InvalidConfiguration("invalid x-codex-beta-features header")
        })
    }

    /// Replace a Codex conversation history through OpenAI's unary
    /// `/responses/compact` endpoint.
    ///
    /// The endpoint accepts the same model/input/tools/reasoning controls as a
    /// normal Responses turn, but returns input-ready replacement items rather
    /// than a `Response`. Authentication and account routing flow through the
    /// same live bearer resolver as inference, keeping bearer, account ID, and
    /// FedRAMP headers on one credential snapshot.
    pub async fn compact_codex_conversation(
        &self,
        mut request: ConversationRequest,
        instructions: &str,
    ) -> Result<Vec<xai_grok_sampling_types::ConversationItem>> {
        if self.provider_adapter.profile().responses_dialect()
            != Some(xai_grok_sampling_types::ResponsesDialect::Codex)
            || self.defaults.api_backend != ApiBackend::Responses
        {
            return Err(SamplingError::InvalidConfiguration(
                "responses/compact is available only for the Codex Responses provider",
            ));
        }
        if request.items.is_empty() {
            return Ok(Vec::new());
        }

        self.apply_conversation_defaults(&mut request)?;
        self.project_responses_conversation(&mut request)?;
        request.hosted_tools = xai_grok_sampling_types::hosted_tools_for_provider(
            &request.hosted_tools,
            self.defaults.provider,
        );
        let request_body = self.codex_compaction_request_body(&request, instructions, false)?;

        let endpoint = self.endpoint("responses/compact");
        let timeout_secs = self
            .defaults
            .idle_timeout_secs
            .unwrap_or(300)
            .saturating_mul(4)
            .max(1);
        tracing::debug!(%endpoint, timeout_secs, "sending Codex compact request");
        let response = self
            .post(&endpoint)
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .json(&request_body)
            .send()
            .await
            .map_err(SamplingError::Http)?;
        let status = response.status();
        if status.is_success() {
            self.capture_codex_turn_state(response.headers());
        }
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await.map_err(SamplingError::Http)?;
        if !status.is_success() {
            let server_message = user_facing_api_error_message(status, bytes.as_ref());
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(crate::attribution::SamplingConsumer::Responses);
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }
            return Err(SamplingError::Api {
                status,
                message: server_message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }
        let response: CodexCompactHistoryResponse =
            serde_json::from_slice(&bytes).map_err(SamplingError::Serialization)?;
        if response.output.is_empty() {
            return Err(SamplingError::serialization_message(
                "Codex compact response contained no replacement history",
            ));
        }
        xai_grok_sampling_types::codex_compact_output_to_conversation_items(response.output)
            .map_err(SamplingError::serialization_message)
    }

    /// Run Codex remote compaction v2 over the normal streaming Responses
    /// endpoint.
    ///
    /// The request is a normal full-input Codex Responses call whose final
    /// input item is `{"type":"compaction_trigger"}`. The collector accepts
    /// exactly one durable `response.output_item.done` compaction item and a
    /// required `response.completed`; unrelated output and progress events are
    /// intentionally ignored. Installation remains the caller's responsibility
    /// so a disconnected/retried stream cannot partially mutate conversation
    /// state.
    pub async fn compact_codex_conversation_v2(
        &self,
        mut request: ConversationRequest,
        instructions: &str,
    ) -> Result<CodexRemoteCompactionV2Result> {
        if self.provider_adapter.profile().responses_dialect()
            != Some(xai_grok_sampling_types::ResponsesDialect::Codex)
            || self.defaults.api_backend != ApiBackend::Responses
        {
            return Err(SamplingError::InvalidConfiguration(
                "remote compaction v2 is available only for the Codex Responses provider",
            ));
        }
        if request.items.is_empty() {
            return Err(SamplingError::InvalidConfiguration(
                "remote compaction v2 requires non-empty conversation input",
            ));
        }

        self.apply_conversation_defaults(&mut request)?;
        self.project_responses_conversation(&mut request)?;
        request.hosted_tools = xai_grok_sampling_types::hosted_tools_for_provider(
            &request.hosted_tools,
            self.defaults.provider,
        );
        let request_body = self.codex_compaction_request_body(&request, instructions, true)?;
        let endpoint = self.endpoint("responses");
        let timeout_secs = self
            .defaults
            .idle_timeout_secs
            .unwrap_or(300)
            .saturating_mul(4)
            .max(1);
        let mut beta_headers = HeaderMap::new();
        beta_headers.insert(
            HeaderName::from_static(X_CODEX_BETA_FEATURES_HEADER),
            self.codex_remote_compaction_v2_beta_header()?,
        );
        tracing::debug!(%endpoint, timeout_secs, "sending Codex remote compaction v2 request");
        let response = self
            .post(&endpoint)
            .headers(beta_headers)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .json(&request_body)
            .send()
            .await
            .map_err(SamplingError::Http)?;
        let status = response.status();
        if status.is_success() {
            self.capture_codex_turn_state(response.headers());
        }
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        if !status.is_success() {
            let bytes = response.bytes().await.map_err(SamplingError::Http)?;
            let server_message = user_facing_api_error_message(status, bytes.as_ref());
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(crate::attribution::SamplingConsumer::ResponsesStream);
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }
            return Err(SamplingError::Api {
                status,
                message: server_message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if first {
                    first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });
        let mut event_stream = byte_stream.eventsource();
        let mut collector = CodexRemoteCompactionV2Collector::default();
        while let Some(event) = event_stream.next().await {
            let event =
                event.map_err(|error| SamplingError::EventStreamError(error.to_string()))?;
            if event.data == "[DONE]" {
                break;
            }
            collector.absorb_with_adapter(
                self.provider_adapter,
                &event.event,
                &event.data,
                self.codex_turn_state.as_ref(),
            )?;
            if collector.saw_completed {
                break;
            }
        }
        collector.finish()
    }

    /// Apply default configuration to a Responses API request.
    fn apply_response_defaults(&self, request: &mut CreateResponseWrapper) -> Result<()> {
        // Apply model default if not specified
        if request.inner.model.is_none() {
            request.inner.model = Some(self.defaults.model.clone());
        }

        // Apply temperature default if not specified
        if request.inner.temperature.is_none() {
            request.inner.temperature = self.defaults.temperature;
        }

        // Apply top_p default if not specified
        if request.inner.top_p.is_none() {
            request.inner.top_p = self.defaults.top_p;
        }

        // Apply max_output_tokens default if not specified
        if request.inner.max_output_tokens.is_none() {
            request.inner.max_output_tokens = self.defaults.max_completion_tokens;
        }

        // Set store to false if not specified (default is true, but that breaks ZDR compliance)
        if request.inner.store.is_none() {
            request.inner.store = Some(false);
        }

        // Include encrypted reasoning content if not specified
        let includes = request.inner.include.get_or_insert_with(Vec::new);
        if !includes.contains(&rs::IncludeEnum::ReasoningEncryptedContent) {
            includes.push(rs::IncludeEnum::ReasoningEncryptedContent);
        }

        // codex-rs keys HTTP prompt caching by the stable session ID. Keep
        // this provider-scoped so xAI's request body remains unchanged, and
        // preserve an explicit cache key if a caller supplied one.
        if request.inner.prompt_cache_key.is_none() {
            request.inner.prompt_cache_key = self
                .provider_adapter
                .prompt_cache_key(request.x_grok_session_id.as_deref());
        }

        Ok(())
    }

    /// Create a response using the Responses API (non-streaming).
    ///
    /// This uses the Responses API format which provides a simpler interface
    /// for multi-turn conversations and tool calling.
    pub async fn create_response(
        &self,
        mut request: CreateResponseWrapper,
    ) -> Result<rs::Response> {
        self.apply_response_defaults(&mut request)?;

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone().unwrap_or_default();

        // The trace field is process-local: it is consumed by upstream
        // session code (which may upload a payload artifact) and is not
        // forwarded by the sampler. Drop it before we send.
        request.trace.take();

        tracing::debug!("create_response: {:?}", &request);
        tracing::debug!("endpoint: {:?}", self.endpoint("responses"));

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let mut request_body = serde_json::to_value(&request.inner).map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        let extra_tool_entries = std::mem::take(&mut request.extra_tool_entries);
        if !extra_tool_entries.is_empty() {
            if let Some(tools) = request_body.get_mut("tools").and_then(|v| v.as_array_mut()) {
                tools.extend(extra_tool_entries);
            } else {
                request_body["tools"] = serde_json::Value::Array(extra_tool_entries);
            }
        }
        // async-openai's ReasoningTextContent struct omits the `type`
        // discriminator that the Responses API requires on input. Patch
        // it in post-serialize. This is the last surviving piece of the
        // old raw_output machinery.
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        xai_grok_sampling_types::strip_sentinel_web_search_actions(&mut request_body);
        patch_custom_tool_output_wire_fields(
            &mut request_body,
            &request.named_custom_tool_outputs,
            &request.original_detail_custom_output_images,
        );
        patch_raw_input_replacements(&mut request_body, &request.raw_input_replacements)?;
        self.provider_adapter.patch_responses_request(
            &mut request_body,
            ResponsesRequestPolicy {
                multi_agent_v2: self.defaults.codex_multi_agent_v2,
                local_effort: request.local_reasoning_effort,
                reasoning_summary: self.defaults.reasoning_summary,
            },
        );
        validate_responses_wire_body_for_provider(&request_body, self.defaults.provider)?;
        let http_request = self
            .provider_adapter
            .apply_request_headers(self.post(self.endpoint("responses")), grok_headers)
            .json(&request_body);

        let response = http_request.send().await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            e
        })?;

        let status = response.status();
        if status.is_success() {
            self.capture_codex_turn_state(response.headers());
        }
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(crate::attribution::SamplingConsumer::Responses);
                let endpoint = self.endpoint("responses");
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }

            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::warn!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "responses API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let response_obj = (|| {
            let mut value = serde_json::from_slice::<serde_json::Value>(&bytes)?;
            if self.provider_adapter.normalizes_response_events() {
                normalize_response_compat(&mut value, "completed");
            }
            serde_json::from_value::<rs::Response>(value)
        })()
        .map_err(|e| {
            let raw_body = String::from_utf8_lossy(&bytes);
            tracing::error!(
                error = %e,
                raw_body = %raw_body,
                "Failed to deserialize rs::Response"
            );
            SamplingError::Serialization(e)
        })?;
        Ok(response_obj)
    }

    /// Create a streaming response using the Responses API.
    ///
    /// Returns a stream of `rs::ResponseStreamEvent` which includes events like:
    /// - `response.created` - Initial response object
    /// - `response.output_text.delta` - Text content deltas
    /// - `response.function_call_arguments.delta` - Function call argument deltas
    /// - `response.completed` - Final response with all output
    ///
    /// The third tuple element is a per-request doom-loop signal collector,
    /// `Some` only when `SamplerConfig::doom_loop_recovery` is set — the same
    /// gate that adds the xAI-only `x-grok-doom-loop-check` request header.
    /// Codex still gets an isolated collector but never receives the xAI
    /// header. The collector is filled by the SSE decoder as the server
    /// reports triggers and is meant to be handed to `stream_responses` so
    /// the signals land on the final `ConversationResponse`.
    #[tracing::instrument(
        name = "http.create_response_stream",
        skip_all,
        fields(
            endpoint = %self.endpoint("responses"),
            model_id = request.inner.model.as_deref().unwrap_or(""),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    #[allow(clippy::type_complexity)]
    pub async fn create_response_stream(
        &self,
        mut request: CreateResponseWrapper,
    ) -> Result<(
        BoxStream<'static, Result<rs::ResponseStreamEvent>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        self.apply_response_defaults(&mut request)?;

        // Enable streaming
        request.inner.stream = Some(true);

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone().unwrap_or_default();

        // Drop process-local trace data (see note in `create_response`).
        request.trace.take();

        tracing::debug!(
            base_url = %self.base_url,
            model_id = model_id.as_str(),
            "Sending responses API stream request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let extra_tool_entries = std::mem::take(&mut request.extra_tool_entries);
        let mut request_body = serde_json::to_value(&request.inner).map_err(|e| {
            tracing::error!("Failed to serialize responses request: {}", e);
            SamplingError::Serialization(e)
        })?;
        // Inject xAI-specific fields not in async-openai's CreateResponse type.
        if self.defaults.stream_tool_calls {
            request_body["stream_tool_calls"] = serde_json::json!(true);
        }
        // Inject xAI-specific tools (e.g., x_search) that can't be expressed
        // via async_openai's rs::Tool enum.
        if !extra_tool_entries.is_empty() {
            if let Some(tools) = request_body.get_mut("tools").and_then(|v| v.as_array_mut()) {
                tools.extend(extra_tool_entries);
            } else {
                request_body["tools"] = serde_json::Value::Array(extra_tool_entries);
            }
        }
        xai_grok_sampling_types::patch_reasoning_text_types(&mut request_body);
        xai_grok_sampling_types::strip_sentinel_web_search_actions(&mut request_body);
        patch_custom_tool_output_wire_fields(
            &mut request_body,
            &request.named_custom_tool_outputs,
            &request.original_detail_custom_output_images,
        );
        patch_raw_input_replacements(&mut request_body, &request.raw_input_replacements)?;
        self.provider_adapter.patch_responses_request(
            &mut request_body,
            ResponsesRequestPolicy {
                multi_agent_v2: self.defaults.codex_multi_agent_v2,
                local_effort: request.local_reasoning_effort,
                reasoning_summary: self.defaults.reasoning_summary,
            },
        );
        validate_responses_wire_body_for_provider(&request_body, self.defaults.provider)?;
        // Fresh per attempt so signals never leak across retries; `None`
        // disables collection. The opt-in wire header is xAI-only below.
        let doom_loop = self
            .defaults
            .doom_loop_recovery
            .map(crate::doom_loop::DoomLoopSignalCollector::new);
        let mut http_request = self
            .provider_adapter
            .apply_request_headers(self.post(self.endpoint("responses")), grok_headers)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"));
        if doom_loop.is_some() && self.provider_adapter.sends_doom_loop_opt_in() {
            // Presence opts in; the server ignores the value.
            http_request = http_request.header(DOOM_LOOP_CHECK_HEADER, "true");
        }
        let http_request = http_request.json(&request_body);

        let built_request = http_request.build().map_err(|e| {
            tracing::error!("Failed to build HTTP request: {}", e);
            SamplingError::Http(e)
        })?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending responses API stream request"
        );
        Self::log_request_headers(&built_request, "responses");

        let response = self.http.execute(built_request).await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            record_stream_request_failure(&e);
            e
        })?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                self.record_401_attribution(crate::attribution::SamplingConsumer::ResponsesStream);
                let endpoint = self.endpoint("responses");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "responses API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        self.capture_codex_turn_state(response.headers());

        let model_metadata = extract_model_metadata(response.headers());

        // Strip UTF-8 BOM if present
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        let doom_loop_for_stream = doom_loop.clone();
        let codex_turn_state_for_stream = self.codex_turn_state.clone();
        let provider_adapter_for_stream = self.provider_adapter;

        // The scan item is an `Option`: `Some(None)` skips an absorbed
        // doom-loop event without terminating the stream (`filter_map`
        // below), while an outer `None` still ends it.
        let events = event_stream
            .scan(false, move |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "responses",
                            data = %data,
                        );

                        // Intercept the non-standard doom-loop event before
                        // typed deserialization; async-openai's event enum
                        // does not know it and would fail to parse it. With
                        // the check disabled, the shared name-or-payload-type
                        // predicate guards against a server emitting it
                        // despite no opt-in (rollout skew), named or not.
                        let swallow_metadata = provider_adapter_for_stream
                            .absorb_response_metadata(
                                &event.event,
                                data,
                                codex_turn_state_for_stream.as_ref(),
                            );
                        let swallow = swallow_metadata
                            || match &doom_loop_for_stream {
                                Some(collector) => collector.absorb(&event.event, data),
                                None => is_check_event(&event.event, data),
                            };
                        if swallow {
                            Some(None)
                        } else if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Some(Err(stream_error)))
                        } else {
                            let decoded = deserialize_response_event_for_adapter(
                                data,
                                provider_adapter_for_stream,
                            );
                            if decoded.as_ref().is_err_and(|error| {
                                provider_adapter_for_stream
                                    .ignores_unknown_response_event(error, data)
                            }) {
                                tracing::debug!(
                                    event_type = ?serde_json::from_str::<serde_json::Value>(data)
                                        .ok()
                                        .and_then(|value| value.get("type").cloned()),
                                    "ignoring an unmodeled Codex Responses event"
                                );
                                Some(None)
                            } else {
                                Some(Some(decoded))
                            }
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Some(Err(SamplingError::EventStreamError(e.to_string()))))
                    }
                };
                std::future::ready(item)
            })
            .filter_map(std::future::ready)
            .boxed();

        Ok((events, model_metadata, doom_loop))
    }

    // =========================================================================
    // Anthropic Messages API
    // =========================================================================

    /// Apply default configuration to a Messages API request.
    fn apply_message_defaults(&self, request: &mut MessagesRequestWrapper) -> Result<()> {
        // Apply model default if not specified
        if request.inner.model.is_empty() {
            request.inner.model = self.defaults.model.clone();
        }

        if request.inner.max_tokens == 0 {
            request.inner.max_tokens = self
                .defaults
                .max_completion_tokens
                .unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);
        }

        // Apply temperature default if not specified
        if request.inner.temperature.is_none() {
            request.inner.temperature = self.defaults.temperature;
        }

        // Apply top_p default if not specified
        if request.inner.top_p.is_none() {
            request.inner.top_p = self.defaults.top_p;
        }

        Ok(())
    }

    /// Create a message using the Anthropic Messages API (non-streaming).
    pub async fn create_message(
        &self,
        mut request: MessagesRequestWrapper,
    ) -> Result<messages::MessagesResponse> {
        self.apply_message_defaults(&mut request)?;

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone();

        // Drop process-local trace data.
        request.trace.take();

        tracing::debug!("create_message: {:?}", &request.inner);
        tracing::debug!("endpoint: {:?}", self.endpoint("messages"));

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let http_request = self
            .provider_adapter
            .apply_request_headers(self.post(self.endpoint("messages")), grok_headers)
            .json(&request.inner);

        let response = http_request.send().await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            e
        })?;

        let status = response.status();
        let model_metadata = extract_model_metadata(response.headers());
        let retry_after_secs = extract_retry_after(response.headers());
        let should_retry = extract_should_retry(response.headers());
        let bytes = response.bytes().await?;

        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.record_401_attribution(crate::attribution::SamplingConsumer::Messages);
                let endpoint = self.endpoint("messages");
                let server_message = user_facing_api_error_message(status, bytes.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }

            let message = user_facing_api_error_message(status, bytes.as_ref());
            tracing::warn!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "messages API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let response_obj =
            serde_json::from_slice::<messages::MessagesResponse>(&bytes).map_err(|e| {
                let raw_body = String::from_utf8_lossy(&bytes);
                tracing::error!(
                    error = %e,
                    raw_body = %raw_body,
                    "Failed to deserialize MessagesResponse"
                );
                SamplingError::Serialization(e)
            })?;
        Ok(response_obj)
    }

    /// Create a streaming message using the Anthropic Messages API.
    ///
    /// Returns a stream of `MessageStreamEvent` which includes events like:
    /// - `message_start` - Initial message object
    /// - `content_block_start` / `content_block_delta` / `content_block_stop` - Content blocks
    /// - `message_delta` / `message_stop` - Final message with stop reason
    #[tracing::instrument(
        name = "http.create_message_stream",
        skip_all,
        fields(
            endpoint = %self.endpoint("messages"),
            model_id = request.inner.model.as_str(),
            status_code = tracing::field::Empty,
            success = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
    pub async fn create_message_stream(
        &self,
        mut request: MessagesRequestWrapper,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_message_defaults(&mut request)?;

        // Enable streaming
        request.inner.stream = Some(true);

        let x_grok_conv_id = request.x_grok_conv_id.as_deref().unwrap_or_default();
        let x_grok_req_id = request.x_grok_req_id.as_deref().unwrap_or_default();
        let model_id = request.inner.model.clone();

        // Drop process-local trace data.
        request.trace.take();

        tracing::debug!(
            base_url = %self.base_url,
            model_id = model_id.as_str(),
            "Sending Messages API stream request"
        );

        let grok_headers = GrokRequestHeaders {
            conv_id: x_grok_conv_id,
            req_id: x_grok_req_id,
            model_id: &model_id,
            session_id: request.x_grok_session_id.as_deref().unwrap_or_default(),
            turn_idx: request.x_grok_turn_idx.as_deref(),
            agent_id: request.x_grok_agent_id.as_deref().unwrap_or_default(),
            deployment_id: request.x_grok_deployment_id.as_deref(),
            user_id: request.x_grok_user_id.as_deref(),
        };
        let http_request = self
            .provider_adapter
            .apply_request_headers(self.post(self.endpoint("messages")), grok_headers)
            .header(ACCEPT, HeaderValue::from_static("text/event-stream"))
            .json(&request.inner);

        let built_request = http_request.build().map_err(|e| {
            tracing::error!("Failed to build HTTP request: {}", e);
            SamplingError::Http(e)
        })?;

        tracing::debug!(
            url = %built_request.url(),
            method = %built_request.method(),
            "Sending messages API stream request"
        );
        Self::log_request_headers(&built_request, "messages");

        let response = self.http.execute(built_request).await.map_err(|e| {
            tracing::debug!("HTTP request failed: {}", e);
            record_stream_request_failure(&e);
            e
        })?;

        let status = response.status();
        let span = tracing::Span::current();
        span.record("status_code", status.as_u16() as i64);
        span.record("success", status.is_success());
        if !status.is_success() {
            if status == reqwest::StatusCode::UNAUTHORIZED {
                span.record("error", "unauthorized (401)");
                self.record_401_attribution(crate::attribution::SamplingConsumer::MessagesStream);
                let endpoint = self.endpoint("messages");
                let body = response.bytes().await.unwrap_or_default();
                let server_message = user_facing_api_error_message(status, body.as_ref());
                return Err(SamplingError::Auth(format!(
                    "Unauthorized (401) from {endpoint}: {server_message}"
                )));
            }
            let model_metadata = extract_model_metadata(response.headers());
            let retry_after_secs = extract_retry_after(response.headers());
            let should_retry = extract_should_retry(response.headers());
            let bytes = response.bytes().await?;
            let message = user_facing_api_error_message(status, bytes.as_ref());
            span.record("error", message.as_str());
            tracing::error!(
                status = %status,
                error_message = %message,
                body_preview = %Self::body_preview(bytes.as_ref()),
                model_id = %model_id,
                "messages API error"
            );
            return Err(SamplingError::Api {
                status,
                message,
                model_metadata,
                retry_after_secs,
                should_retry,
            });
        }

        let model_metadata = extract_model_metadata(response.headers());

        // Strip UTF-8 BOM if present
        const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];
        let mut is_first = true;
        let byte_stream = response.bytes_stream().map(move |result| {
            result.map(|bytes| {
                if is_first {
                    is_first = false;
                    if bytes.starts_with(UTF8_BOM) {
                        return bytes.slice(UTF8_BOM.len()..);
                    }
                }
                bytes
            })
        });

        // Turn raw bytes into SSE events
        let event_stream = byte_stream.eventsource();

        // Map SSE events into MessageStreamEvent.
        // Uses `scan` so transport errors terminate the stream after the first
        // error (same pattern as `chat_completion_stream`).
        let events = event_stream
            .scan(false, |had_transport_error, event_res| {
                if *had_transport_error {
                    return std::future::ready(None);
                }
                let item = match event_res {
                    Ok(event) => {
                        let data = &event.data;
                        if data == "[DONE]" {
                            return std::future::ready(None);
                        }

                        tracing::info!(
                            target: crate::sampling_log::TARGET,
                            event = "sse_chunk",
                            backend = "messages",
                            data = %data,
                        );

                        if let Some(stream_error) = try_parse_stream_error(data) {
                            Some(Err(stream_error))
                        } else {
                            Some(
                                serde_json::from_str::<messages::MessageStreamEvent>(data).map_err(
                                    |e| {
                                        tracing::error!(
                                            error = %e,
                                            raw_data = %data,
                                            "Failed to deserialize MessageStreamEvent from stream"
                                        );
                                        SamplingError::Serialization(e)
                                    },
                                ),
                            )
                        }
                    }
                    Err(e) => {
                        *had_transport_error = true;
                        Some(Err(SamplingError::EventStreamError(e.to_string())))
                    }
                };
                std::future::ready(item)
            })
            .boxed();

        Ok((events, model_metadata))
    }

    // =========================================================================
    // Unified Conversation API
    // =========================================================================

    /// Apply default configuration to a ConversationRequest.
    fn apply_conversation_defaults(&self, request: &mut ConversationRequest) -> Result<()> {
        if request.model.is_none() {
            request.model = Some(self.defaults.model.clone());
        }

        if request.temperature.is_none() {
            request.temperature = self.defaults.temperature;
        }

        if request.top_p.is_none() {
            request.top_p = self.defaults.top_p;
        }

        if request.max_output_tokens.is_none() {
            request.max_output_tokens = self.defaults.max_completion_tokens;
        }

        if request.reasoning_effort.is_none() {
            request.reasoning_effort = self.defaults.reasoning_effort;
        }

        // Codex rejects invalid base64 data URLs, remote image URLs, and
        // detail=low tool images with a non-retryable 400 that bricks the
        // turn. Prepare images before conversion (parity with codex-rs
        // `prepare_response_items`). Other providers keep existing behavior.
        if self.defaults.provider == ModelProvider::Codex {
            let prepared = request.prepare_images_for_codex();
            if prepared > 0 {
                tracing::warn!(
                    prepared,
                    "replaced {prepared} unsendable image(s) before Codex request"
                );
            }
        }

        Ok(())
    }

    /// Apply the provider's Code Mode wire projection before Responses
    /// serialization. The provider comes from the selected model's sampling
    /// configuration, never from its slug or endpoint.
    fn project_responses_conversation(&self, request: &mut ConversationRequest) -> Result<()> {
        request
            .project_code_mode_for_provider(self.defaults.provider)
            .map_err(|error| {
                tracing::error!(
                    provider = ?self.defaults.provider,
                    %error,
                    "Responses request is incompatible with the selected provider"
                );
                SamplingError::InvalidConfiguration(
                    "Responses request contains unsupported native custom tool content",
                )
            })
    }

    /// Apply all typed request preparation that must happen before a
    /// `ConversationRequest` is converted to the Responses wire model.
    fn prepare_responses_conversation(&self, request: &mut ConversationRequest) -> Result<()> {
        self.apply_conversation_defaults(request)?;
        self.project_responses_conversation(request)?;
        request.hosted_tools = xai_grok_sampling_types::hosted_tools_for_provider(
            &request.hosted_tools,
            self.defaults.provider,
        );
        Ok(())
    }

    /// Normalize native Responses Code Mode history before converting it to a
    /// function-only API. The provider is the explicit catalog-selected
    /// profile carried by this client, never inferred from model or URL.
    pub(crate) fn project_function_backend_conversation(
        &self,
        request: &mut ConversationRequest,
    ) -> Result<()> {
        request
            .project_code_mode_for_function_backend(self.defaults.provider)
            .map_err(|error| {
                tracing::error!(
                    provider = ?self.defaults.provider,
                    %error,
                    "request history is incompatible with the selected function-only backend"
                );
                SamplingError::InvalidConfiguration(
                    "request contains unsupported native custom tool history",
                )
            })
    }

    /// Send a conversation request using the Chat Completions API (streaming).
    ///
    /// Converts the `ConversationRequest` to `ChatCompletionRequest` internally.
    /// Returns the stream and any model metadata extracted from response headers.
    pub async fn conversation_stream(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<ChatCompletionChunk>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_conversation_defaults(&mut request)?;
        self.project_function_backend_conversation(&mut request)?;

        let trace = request.trace.take();
        let mut chat_request: ChatCompletionRequest = request.into();
        if let Some(trace) = trace {
            chat_request.trace = Some(trace);
        }

        self.chat_completion_stream(chat_request).await
    }

    /// Send a conversation request using the Chat Completions API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to `ChatCompletionRequest` internally.
    pub async fn conversation(
        &self,
        mut request: ConversationRequest,
    ) -> Result<ChatCompletionResponse> {
        self.apply_conversation_defaults(&mut request)?;
        self.project_function_backend_conversation(&mut request)?;

        let trace = request.trace.take();
        let mut chat_request: ChatCompletionRequest = request.into();
        if let Some(trace) = trace {
            chat_request.trace = Some(trace);
        }

        self.chat_completion(chat_request).await
    }

    /// Send a conversation request using the Responses API (streaming).
    ///
    /// Converts the `ConversationRequest` to Responses API format internally.
    /// The third tuple element is the per-request doom-loop signal collector
    /// (see [`Self::create_response_stream`]); callers that don't consume the
    /// signals can ignore it.
    #[allow(clippy::type_complexity)]
    pub async fn conversation_stream_responses(
        &self,
        request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<rs::ResponseStreamEvent>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
    )> {
        let (stream, metadata, doom_loop, _) = self
            .conversation_stream_responses_with_client_custom_tools(request)
            .await?;
        Ok((stream, metadata, doom_loop))
    }

    /// Streaming Responses request plus the native custom-tool names from the
    /// exact post-projection request. The actor needs those names to classify
    /// returned custom calls without projecting the conversation a second
    /// time. External callers retain the stable three-part API above.
    #[allow(clippy::type_complexity)]
    pub(crate) async fn conversation_stream_responses_with_client_custom_tools(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<rs::ResponseStreamEvent>>,
        Option<ResponseModelMetadata>,
        Option<crate::doom_loop::DoomLoopSignalCollector>,
        Vec<String>,
    )> {
        self.prepare_responses_conversation(&mut request)?;
        let client_custom_tool_names = request.client_custom_tool_names();

        let trace = request.trace.take();
        let local_reasoning_effort = request.reasoning_effort;
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        // Collect provider extensions that can't be expressed via rs::Tool
        // (e.g., x_search). These are injected as raw JSON after serialization.
        let extra_tools = xai_grok_sampling_types::extra_tool_entries(&request.hosted_tools);
        let named_custom_tool_outputs = request.named_custom_tool_outputs();
        let raw_input_replacements =
            request.raw_responses_input_replacements(self.defaults.provider);
        let mut original_detail_custom_output_images =
            request.original_detail_custom_output_images();
        original_detail_custom_output_images
            .extend(request.original_detail_function_output_images());

        let responses_request: rs::CreateResponse = (&request).into();

        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.local_reasoning_effort = local_reasoning_effort;
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;
        wrapper.extra_tool_entries = extra_tools;
        wrapper.named_custom_tool_outputs = named_custom_tool_outputs;
        wrapper.original_detail_custom_output_images = original_detail_custom_output_images;
        wrapper.raw_input_replacements = raw_input_replacements;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        let (stream, metadata, doom_loop) = self.create_response_stream(wrapper).await?;
        Ok((stream, metadata, doom_loop, client_custom_tool_names))
    }

    /// Send a conversation request using the Responses API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to Responses API format internally.
    pub async fn conversation_responses(
        &self,
        mut request: ConversationRequest,
    ) -> Result<rs::Response> {
        self.prepare_responses_conversation(&mut request)?;

        let trace = request.trace.take();
        let local_reasoning_effort = request.reasoning_effort;
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();
        let extra_tools = xai_grok_sampling_types::extra_tool_entries(&request.hosted_tools);
        let named_custom_tool_outputs = request.named_custom_tool_outputs();
        let raw_input_replacements =
            request.raw_responses_input_replacements(self.defaults.provider);
        let mut original_detail_custom_output_images =
            request.original_detail_custom_output_images();
        original_detail_custom_output_images
            .extend(request.original_detail_function_output_images());

        let responses_request: rs::CreateResponse = (&request).into();

        let mut wrapper = CreateResponseWrapper::new(responses_request);
        wrapper.local_reasoning_effort = local_reasoning_effort;
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;
        wrapper.extra_tool_entries = extra_tools;
        wrapper.named_custom_tool_outputs = named_custom_tool_outputs;
        wrapper.original_detail_custom_output_images = original_detail_custom_output_images;
        wrapper.raw_input_replacements = raw_input_replacements;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_response(wrapper).await
    }

    /// Send a conversation request using the Anthropic Messages API (streaming).
    ///
    /// Converts the `ConversationRequest` to Messages API format internally.
    pub async fn conversation_stream_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<(
        BoxStream<'static, Result<messages::MessageStreamEvent>>,
        Option<ResponseModelMetadata>,
    )> {
        self.apply_conversation_defaults(&mut request)?;
        self.project_function_backend_conversation(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let messages_request = build_messages_request(&request);

        let mut wrapper = MessagesRequestWrapper::new(messages_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_message_stream(wrapper).await
    }

    /// Send a conversation request using the Anthropic Messages API (non-streaming).
    ///
    /// Converts the `ConversationRequest` to Messages API format internally.
    pub async fn conversation_messages(
        &self,
        mut request: ConversationRequest,
    ) -> Result<messages::MessagesResponse> {
        self.apply_conversation_defaults(&mut request)?;
        self.project_function_backend_conversation(&mut request)?;

        let trace = request.trace.take();
        let x_grok_conv_id = request.x_grok_conv_id.clone();
        let x_grok_req_id = request.x_grok_req_id.clone();
        let x_grok_session_id = request.x_grok_session_id.clone();
        let x_grok_turn_idx = request.x_grok_turn_idx.clone();
        let x_grok_agent_id = request.x_grok_agent_id.clone();

        let messages_request = build_messages_request(&request);

        let mut wrapper = MessagesRequestWrapper::new(messages_request);
        wrapper.x_grok_conv_id = x_grok_conv_id;
        wrapper.x_grok_req_id = x_grok_req_id;
        wrapper.x_grok_session_id = x_grok_session_id;
        wrapper.x_grok_turn_idx = x_grok_turn_idx;
        wrapper.x_grok_agent_id = x_grok_agent_id;

        if let Some(trace) = trace {
            wrapper.trace = Some(trace);
        }

        self.create_message(wrapper).await
    }

    /// Backend-aware streaming call that collects the full response.
    pub async fn conversation_collect(
        &self,
        request: ConversationRequest,
    ) -> Result<ConversationResponse> {
        let request_id = crate::types::RequestId::random();
        let idle_timeout = std::time::Duration::from_secs(300);
        let result = match self.api_backend() {
            ApiBackend::ChatCompletions => {
                let (raw, meta) = self.conversation_stream(request).await?;
                let events =
                    crate::stream::stream_chat_completions(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
            ApiBackend::Responses => {
                let (raw, meta, doom_loop, client_custom_tool_names) = self
                    .conversation_stream_responses_with_client_custom_tools(request)
                    .await?;
                let events = crate::stream::stream_responses_with_client_custom_tools(
                    raw,
                    meta,
                    request_id,
                    idle_timeout,
                    doom_loop,
                    client_custom_tool_names,
                );
                crate::stream::collect_response(events).await
            }
            ApiBackend::Messages => {
                let (raw, meta) = self.conversation_stream_messages(request).await?;
                let events = crate::stream::stream_messages(raw, meta, request_id, idle_timeout);
                crate::stream::collect_response(events).await
            }
        };
        result
            .map(|(response, _metrics)| response)
            .map_err(|info| SamplingError::Api {
                status: info
                    .status_code
                    .and_then(|c| reqwest::StatusCode::from_u16(c).ok())
                    .unwrap_or(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
                message: info.message,
                model_metadata: info.model_metadata,
                retry_after_secs: info.retry_after_secs,
                should_retry: None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use xai_grok_sampling_types::ModelProvider;
    use xai_grok_sampling_types::types::ChatRequestMessage;

    #[test]
    fn responses_wire_guard_rejects_native_custom_for_xai_and_allows_codex() {
        for body in [
            serde_json::json!({"tools": [{"type": "custom", "name": "exec"}]}),
            serde_json::json!({"input": [{"type": "custom_tool_call", "name": "exec"}]}),
            serde_json::json!({"input": [{"type": "custom_tool_call_output", "call_id": "c"}]}),
            serde_json::json!({"tool_choice": {"type": "custom", "name": "exec"}}),
        ] {
            assert!(matches!(
                validate_responses_wire_body_for_provider(&body, ModelProvider::Xai),
                Err(SamplingError::InvalidConfiguration(_))
            ));
            validate_responses_wire_body_for_provider(&body, ModelProvider::Codex)
                .expect("Codex supports native custom Responses content");
        }

        validate_responses_wire_body_for_provider(
            &serde_json::json!({
                "tools": [{
                    "type": "function",
                    "name": "exec",
                    "parameters": {"type": "object"}
                }],
                "input": [
                    {
                        "type": "function_call",
                        "call_id": "call-exec",
                        "name": "exec",
                        "arguments": "{\"source\":\"return 42\"}"
                    },
                    {
                        "type": "function_call_output",
                        "call_id": "call-exec",
                        "output": "42"
                    }
                ]
            }),
            ModelProvider::Xai,
        )
        .expect("xAI function-envelope Code Mode is supported");

        validate_responses_wire_body_for_provider(
            &serde_json::json!({
                "tools": [{
                    "type": "function",
                    "name": "inspect_schema",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "examples": {
                                "type": "array",
                                "examples": [{"type": "custom"}]
                            }
                        }
                    }
                }],
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "custom"}]
                }]
            }),
            ModelProvider::Xai,
        )
        .expect("nested schema examples are data, not Responses custom-tool lanes");
    }

    #[test]
    fn responses_preparation_reports_custom_names_from_projected_request() {
        fn custom_exec_request() -> ConversationRequest {
            ConversationRequest::from_items(vec![xai_grok_sampling_types::ConversationItem::user(
                "run JavaScript",
            )])
            .with_client_tools([xai_grok_sampling_types::ClientTool::Custom {
                name: "exec".to_owned(),
                description: Some("Execute JavaScript".to_owned()),
                format: xai_grok_sampling_types::rs::CustomToolParamFormat::Text,
            }])
        }

        let mut xai_config = minimal_config();
        xai_config.api_backend = ApiBackend::Responses;
        xai_config.provider = ModelProvider::Xai;
        let xai_client = SamplingClient::new(xai_config).expect("xAI Responses client");
        let mut xai_request = custom_exec_request();
        xai_client
            .prepare_responses_conversation(&mut xai_request)
            .expect("xAI should project exec to a function envelope");
        assert!(xai_request.client_custom_tool_names().is_empty());
        assert!(xai_request.tools.iter().any(|tool| tool.name == "exec"));

        let mut codex_config = minimal_config();
        codex_config.api_backend = ApiBackend::Responses;
        codex_config.provider = ModelProvider::Codex;
        let codex_client = SamplingClient::new(codex_config).expect("Codex Responses client");
        let mut codex_request = custom_exec_request();
        codex_client
            .prepare_responses_conversation(&mut codex_request)
            .expect("Codex should retain native custom exec");
        assert_eq!(
            codex_request.client_custom_tool_names(),
            vec!["exec".to_owned()]
        );
        assert!(!codex_request.tools.iter().any(|tool| tool.name == "exec"));
    }

    fn minimal_config() -> SamplerConfig {
        SamplerConfig {
            api_key: Some("test-key".to_string()),
            base_url: "https://example.test".to_string(),
            model: "test-model".to_string(),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            api_backend: ApiBackend::ChatCompletions,
            provider: Default::default(),
            auth_scheme: AuthScheme::Bearer,
            extra_headers: IndexMap::new(),
            query_params: IndexMap::new(),
            env_http_headers: IndexMap::new(),
            context_window: 8192,
            force_http1: false,
            max_retries: None,
            stream_tool_calls: false,
            idle_timeout_secs: None,
            reasoning_effort: None,
            reasoning_summary: None,
            origin_client: None,
            client_identifier: None,
            deployment_id: None,
            user_id: None,
            client_version: None,
            attribution_callback: None,
            bearer_resolver: None,
            supports_backend_search: false,
            codex_multi_agent_v2: false,
            compactions_remaining: None,
            compaction_at_tokens: None,
            doom_loop_recovery: None,
            header_injector: None,
        }
    }

    #[test]
    fn codex_compact_body_keeps_only_endpoint_contract_fields() {
        let mut body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "input": [{"type": "message", "role": "user", "content": "hello"}],
            "instructions": "system",
            "tools": null,
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high", "summary": "concise"},
            "service_tier": null,
            "prompt_cache_key": null,
            "text": null,
            "store": false,
            "stream": false,
            "include": ["reasoning.encrypted_content"],
            "temperature": 0.7,
            "max_output_tokens": 1234
        });
        retain_codex_compact_request_fields(&mut body).unwrap();
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["parallel_tool_calls"], true);
        assert!(body.get("reasoning").is_some());
        assert!(body.get("tools").is_none());
        assert!(body.get("service_tier").is_none());
        for forbidden in [
            "store",
            "stream",
            "include",
            "temperature",
            "max_output_tokens",
        ] {
            assert!(body.get(forbidden).is_none(), "unexpected {forbidden}");
        }
    }

    #[test]
    fn codex_remote_compaction_v2_body_keeps_stream_contract_fields() {
        let mut body = serde_json::json!({
            "model": "gpt-5.6-sol",
            "input": [{"type": "message", "role": "user", "content": "hello"}],
            "instructions": "system",
            "tools": [{"type": "function", "name": "exec"}],
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high", "summary": "detailed"},
            "service_tier": "priority",
            "prompt_cache_key": "session-key",
            "text": {"format": {"type": "text"}},
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
            "temperature": 0.7,
            "max_output_tokens": 1234
        });
        retain_codex_remote_compaction_v2_request_fields(&mut body).unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["prompt_cache_key"], "session-key");
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn idless_compaction_output_uses_typed_sentinel_only() {
        let mut raw = serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        });
        normalize_response_output_item(&mut raw);
        assert_eq!(raw["id"], "");
        let typed: rs::OutputItem = serde_json::from_value(raw).unwrap();
        let rs::OutputItem::Compaction(compaction) = typed else {
            panic!("expected typed compaction");
        };
        assert!(compaction.id.is_empty());
    }

    #[test]
    fn remote_compaction_v2_collector_requires_one_done_item_and_completion() {
        let turn_state = Arc::new(OnceLock::new());
        let mut collector = CodexRemoteCompactionV2Collector::default();
        collector
            .absorb(
                "response.metadata",
                &serde_json::json!({
                    "type": "response.metadata",
                    "response_id": "resp_meta",
                    "headers": {"X-Codex-Turn-State": "sticky-state"}
                })
                .to_string(),
                Some(&turn_state),
            )
            .unwrap();
        collector
            .absorb(
                "response.output_item.done",
                &serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {"type": "message", "id": "ignored"}
                })
                .to_string(),
                Some(&turn_state),
            )
            .unwrap();
        collector
            .absorb(
                "response.output_item.done",
                &serde_json::json!({
                    "type": "response.output_item.done",
                    "output_index": 1,
                    "item": {"type": "compaction", "encrypted_content": "opaque"}
                })
                .to_string(),
                Some(&turn_state),
            )
            .unwrap();
        collector
            .absorb(
                "response.completed",
                &serde_json::json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_completed",
                        "usage": {"input_tokens": 120, "output_tokens": 8}
                    }
                })
                .to_string(),
                Some(&turn_state),
            )
            .unwrap();

        let result = collector.finish().unwrap();
        assert_eq!(result.response_id, "resp_completed");
        assert_eq!(result.usage.unwrap().total_tokens, 128);
        assert_eq!(turn_state.get().map(String::as_str), Some("sticky-state"));
        let request = ConversationRequest::from_items(vec![result.compaction_item]);
        let replay = request.raw_codex_input_replacements();
        assert_eq!(replay[0].value["encrypted_content"], "opaque");
        assert!(replay[0].value.get("id").is_none());
    }

    #[test]
    fn remote_compaction_v2_collector_rejects_missing_compaction_item() {
        let mut collector = CodexRemoteCompactionV2Collector::default();
        collector
            .absorb(
                "response.completed",
                &serde_json::json!({
                    "type": "response.completed",
                    "response": {"id": "resp_completed"}
                })
                .to_string(),
                None,
            )
            .unwrap();

        let error = collector.finish().unwrap_err().to_string();
        assert!(error.contains("expected exactly one compaction output item, got 0"));
    }

    #[test]
    fn remote_compaction_v2_collector_rejects_duplicate_compaction_items() {
        let mut collector = CodexRemoteCompactionV2Collector::default();
        let done = serde_json::json!({
            "type": "response.output_item.done",
            "item": {"type": "compaction", "encrypted_content": "opaque"}
        })
        .to_string();
        collector
            .absorb("response.output_item.done", &done, None)
            .unwrap();
        collector
            .absorb("response.output_item.done", &done, None)
            .unwrap();
        collector
            .absorb(
                "response.completed",
                &serde_json::json!({
                    "type": "response.completed",
                    "response": {"id": "resp_completed"}
                })
                .to_string(),
                None,
            )
            .unwrap();

        let error = collector.finish().unwrap_err().to_string();
        assert!(error.contains("expected exactly one compaction output item, got 2"));
    }

    #[test]
    fn remote_compaction_v2_collector_rejects_eof_before_completion() {
        let mut collector = CodexRemoteCompactionV2Collector::default();
        collector
            .absorb(
                "response.output_item.done",
                &serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {"type": "compaction", "encrypted_content": "opaque"}
                })
                .to_string(),
                None,
            )
            .unwrap();

        let error = collector.finish().unwrap_err().to_string();
        assert!(error.contains("closed before response.completed"));
    }

    #[test]
    fn remote_compaction_v2_collector_rejects_completed_without_response_id() {
        let mut collector = CodexRemoteCompactionV2Collector::default();
        let error = collector
            .absorb(
                "response.completed",
                &serde_json::json!({
                    "type": "response.completed",
                    "response": {"id": ""}
                })
                .to_string(),
                None,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("contained no response id"));
    }

    #[test]
    fn codex_raw_input_patch_restores_exact_compaction_item() {
        let raw = serde_json::json!({
            "type": "compaction",
            "encrypted_content": "opaque"
        });
        let mut body = serde_json::json!({
            "input": [
                {"type": "message", "role": "user", "content": "before"},
                {"type": "message", "role": "assistant", "content": "placeholder"}
            ]
        });
        patch_raw_input_replacements(
            &mut body,
            &[xai_grok_sampling_types::RawInputItemReplacement {
                input_item_index: 1,
                value: raw.clone(),
            }],
        )
        .unwrap();
        assert_eq!(body["input"][1], raw);
    }

    /// Verify the serialized shape of StreamingChatRequest matches the
    /// expected wire format: all ChatCompletionRequest fields flattened at
    /// top level, plus `stream: true` and `stream_options.include_usage: true`.
    #[test]
    fn streaming_chat_request_serializes_correctly() {
        let request = ChatCompletionRequest {
            model: Some("test-model".into()),
            messages: vec![ChatRequestMessage::user("hello")],
            temperature: Some(0.7),
            max_tokens: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            user: None,
            tools: None,
            tool_choice: None,
            search_parameters: None,
            response_format: None,
            reasoning_effort: None,
            x_grok_conv_id: None,
            x_grok_req_id: None,
            x_grok_session_id: None,
            x_grok_turn_idx: None,
            x_grok_agent_id: None,
            x_grok_deployment_id: None,
            x_grok_user_id: None,
            trace: None,
        };

        let wrapper = StreamingChatRequest {
            inner: &request,
            stream: true,
            stream_options: StreamOptions {
                include_usage: true,
            },
        };

        let json: serde_json::Value = serde_json::to_value(&wrapper).unwrap();
        let obj = json.as_object().unwrap();

        assert_eq!(obj.get("stream").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            obj.get("stream_options")
                .and_then(|v| v.get("include_usage"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );

        assert!(
            obj.get("inner").is_none(),
            "inner field should be flattened"
        );
        assert_eq!(
            obj.get("model").and_then(|v| v.as_str()),
            Some("test-model")
        );
        assert!(obj.get("messages").is_some());
        let temp = obj.get("temperature").and_then(|v| v.as_f64()).unwrap();
        assert!((temp - 0.7).abs() < 0.001, "temperature should be ~0.7");

        assert!(obj.get("max_tokens").is_none());
        assert!(obj.get("tools").is_none());
    }

    #[test]
    fn extract_retry_after_parses_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(30));
    }

    #[test]
    fn extract_retry_after_caps_at_120() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(120));
    }

    #[test]
    fn extract_retry_after_zero_is_valid() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(extract_retry_after(&headers), Some(0));
    }

    #[test]
    fn extract_retry_after_ignores_http_date() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Fri, 31 Dec 2025 23:59:59 GMT".parse().unwrap(),
        );
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn extract_retry_after_none_when_missing() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_retry_after(&headers), None);
    }

    #[test]
    fn extract_should_retry_true() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "true".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(true));
    }

    #[test]
    fn extract_should_retry_true_case_insensitive() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "TRUE".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(true));
    }

    #[test]
    fn extract_should_retry_false() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "false".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), Some(false));
    }

    #[test]
    fn extract_should_retry_unknown_value_is_none() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-should-retry", "banana".parse().unwrap());
        assert_eq!(extract_should_retry(&headers), None);
    }

    #[test]
    fn extract_should_retry_absent_is_none() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(extract_should_retry(&headers), None);
    }

    #[test]
    fn new_with_minimal_config_succeeds() {
        let client = SamplingClient::new(minimal_config()).expect("client should construct");
        assert_eq!(client.api_backend(), ApiBackend::ChatCompletions);
    }

    #[test]
    fn conversation_defaults_apply_reasoning_effort_without_overriding_request() {
        let mut config = minimal_config();
        config.reasoning_effort = Some(ReasoningEffort::Max);
        let client = SamplingClient::new(config).expect("client should construct");

        let mut inherited = ConversationRequest::default();
        client
            .apply_conversation_defaults(&mut inherited)
            .expect("defaults should apply");
        assert_eq!(inherited.reasoning_effort, Some(ReasoningEffort::Max));

        let mut explicit = ConversationRequest {
            reasoning_effort: Some(ReasoningEffort::Low),
            ..ConversationRequest::default()
        };
        client
            .apply_conversation_defaults(&mut explicit)
            .expect("defaults should apply");
        assert_eq!(explicit.reasoning_effort, Some(ReasoningEffort::Low));
    }

    #[test]
    fn provider_backend_matrix_rejects_unsupported_codex_protocols() {
        for backend in [ApiBackend::ChatCompletions, ApiBackend::Messages] {
            let config = SamplerConfig {
                provider: ModelProvider::Codex,
                api_backend: backend,
                ..minimal_config()
            };
            assert!(
                matches!(
                    SamplingClient::new(config),
                    Err(SamplingError::InvalidConfiguration(_))
                ),
                "Codex must reject unsupported backend"
            );
        }

        let config = SamplerConfig {
            provider: ModelProvider::Codex,
            api_backend: ApiBackend::Responses,
            ..minimal_config()
        };
        assert!(SamplingClient::new(config).is_ok());
    }

    #[test]
    fn new_applies_extra_headers() {
        let mut cfg = minimal_config();
        cfg.extra_headers
            .insert("x-test-header".to_string(), "test-value".to_string());
        cfg.extra_headers
            .insert("x-XAI-token-auth".to_string(), "xai-grok-cli".to_string());
        let _client = SamplingClient::new(cfg).expect("client with extra headers should construct");
    }

    #[test]
    fn codex_omits_synthesized_x_grok_headers_but_keeps_provider_headers() {
        let mut cfg = minimal_config();
        cfg.provider = xai_grok_sampling_types::ModelProvider::Codex;
        cfg.api_backend = ApiBackend::Responses;
        cfg.client_identifier = Some("must-not-leak".to_string());
        cfg.client_version = Some("must-not-leak".to_string());
        cfg.deployment_id = Some("must-not-leak".to_string());
        cfg.user_id = Some("must-not-leak".to_string());
        cfg.extra_headers
            .insert("originator".to_string(), "codex_cli_rs".to_string());
        cfg.extra_headers
            .insert("X-Grok-Conv-ID".to_string(), "must-not-leak".to_string());

        let client = SamplingClient::new(cfg).expect("Codex client should construct");
        assert!(
            client
                .default_headers
                .keys()
                .all(|name| !name.as_str().starts_with("x-grok-")),
            "Codex client must not synthesize x-grok headers: {:?}",
            client.default_headers
        );
        assert_eq!(
            client
                .default_headers
                .get("originator")
                .and_then(|value| value.to_str().ok()),
            Some("codex_cli_rs")
        );
    }

    #[test]
    fn codex_turn_state_is_reserved_and_emitted_once_from_internal_state() {
        let mut cfg = minimal_config();
        cfg.provider = xai_grok_sampling_types::ModelProvider::Codex;
        cfg.api_backend = ApiBackend::Responses;
        cfg.extra_headers.insert(
            X_CODEX_TURN_STATE_HEADER.to_owned(),
            "operator-stale-state".to_owned(),
        );
        let turn_state = Arc::new(OnceLock::new());
        let client = SamplingClient::new_with_codex_turn_state(cfg, Arc::clone(&turn_state))
            .expect("Codex client should construct");

        let first = client
            .post("http://localhost/first")
            .build()
            .expect("first request should build");
        assert!(
            first.headers().get(X_CODEX_TURN_STATE_HEADER).is_none(),
            "configured stale turn state must not seed a new logical turn",
        );

        turn_state
            .set("captured-turn-state".to_owned())
            .expect("empty turn state should accept its first value");
        let second = client
            .post("http://localhost/second")
            .build()
            .expect("second request should build");
        let values = second
            .headers()
            .get_all(X_CODEX_TURN_STATE_HEADER)
            .iter()
            .collect::<Vec<_>>();
        assert_eq!(values.len(), 1, "turn state must never be duplicated");
        assert_eq!(values[0].to_str().ok(), Some("captured-turn-state"));
        assert!(values[0].is_sensitive());
    }

    #[derive(Debug)]
    struct ScopedAuthResolver(Option<crate::config::ResolvedBearerAuth>);

    impl crate::config::BearerResolver for ScopedAuthResolver {
        fn current_bearer(&self) -> Option<String> {
            self.0.as_ref().map(|auth| auth.bearer.clone())
        }

        fn current_auth(&self) -> Option<crate::config::ResolvedBearerAuth> {
            self.0.clone()
        }

        fn reserved_headers(&self) -> &'static [&'static str] {
            &["chatgpt-account-id", "x-openai-fedramp"]
        }

        fn fail_closed_on_missing(&self) -> bool {
            true
        }
    }

    #[test]
    fn scoped_auth_snapshot_atomically_replaces_reserved_header_overrides() {
        let mut provider_headers = IndexMap::new();
        provider_headers.insert("ChatGPT-Account-ID".to_string(), "account-b".to_string());
        provider_headers.insert("X-OpenAI-Fedramp".to_string(), "true".to_string());
        let mut cfg = minimal_config();
        cfg.provider = ModelProvider::Codex;
        cfg.api_backend = ApiBackend::Responses;
        cfg.api_key = Some("stale-token-a".to_string());
        cfg.extra_headers.insert(
            "cHaTgPt-aCcOuNt-Id".to_string(),
            "spoofed-account".to_string(),
        );
        cfg.extra_headers
            .insert("x-OPENAI-fedramp".to_string(), "false".to_string());
        cfg.bearer_resolver = Some(std::sync::Arc::new(ScopedAuthResolver(Some(
            crate::config::ResolvedBearerAuth {
                bearer: "current-token-b".to_string(),
                extra_headers: provider_headers,
            },
        ))));

        let request = SamplingClient::new(cfg)
            .unwrap()
            .post("https://example.test/backend-api/codex/responses")
            .build()
            .unwrap();

        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer current-token-b")
        );
        assert_eq!(
            request
                .headers()
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok()),
            Some("account-b")
        );
        assert_eq!(
            request
                .headers()
                .get("x-openai-fedramp")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
    }

    #[test]
    fn scoped_auth_snapshot_fails_closed_when_identity_is_unavailable() {
        let mut cfg = minimal_config();
        cfg.provider = ModelProvider::Codex;
        cfg.api_backend = ApiBackend::Responses;
        cfg.api_key = Some("must-not-be-sent".to_string());
        cfg.extra_headers.insert(
            "ChatGPT-Account-ID".to_string(),
            "stale-account".to_string(),
        );
        cfg.extra_headers
            .insert("X-OpenAI-Fedramp".to_string(), "true".to_string());
        cfg.bearer_resolver = Some(std::sync::Arc::new(ScopedAuthResolver(None)));

        let request = SamplingClient::new(cfg)
            .unwrap()
            .post("https://example.test/backend-api/codex/responses")
            .build()
            .unwrap();

        assert!(request.headers().get(AUTHORIZATION).is_none());
        assert!(request.headers().get("chatgpt-account-id").is_none());
        assert!(request.headers().get("x-openai-fedramp").is_none());
    }

    #[test]
    fn grok_request_headers_are_xai_only() {
        let headers = GrokRequestHeaders {
            conv_id: "conv-1",
            req_id: "req-1",
            model_id: "model-1",
            session_id: "session-1",
            turn_idx: Some("7"),
            agent_id: "agent-1",
            deployment_id: Some("deployment-1"),
            user_id: Some("user-1"),
        };
        let http = reqwest::Client::new();
        let xai = headers
            .apply_for_provider(http.post("https://example.test"), ModelProvider::Xai)
            .build()
            .expect("xAI request should build");
        for (name, value) in [
            ("x-grok-conv-id", "conv-1"),
            ("x-grok-req-id", "req-1"),
            ("x-grok-model-override", "model-1"),
            ("x-grok-session-id", "session-1"),
            ("x-grok-turn-idx", "7"),
            ("x-grok-agent-id", "agent-1"),
            ("x-grok-deployment-id", "deployment-1"),
            ("x-grok-user-id", "user-1"),
        ] {
            assert_eq!(
                xai.headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok()),
                Some(value),
                "missing xAI tracking header {name}"
            );
        }

        let codex = headers
            .apply_for_provider(http.post("https://example.test"), ModelProvider::Codex)
            .build()
            .expect("Codex request should build");
        assert!(
            codex
                .headers()
                .keys()
                .all(|name| !name.as_str().starts_with("x-grok-")),
            "Codex must not receive any x-grok tracking headers: {:?}",
            codex.headers()
        );
    }

    #[test]
    fn apply_env_http_headers_resolves_trims_skips_and_overrides() {
        let mut map = IndexMap::new();
        map.insert("x-tenant-token".to_string(), "TENANT".to_string());
        map.insert("x-blank".to_string(), "BLANK".to_string());
        map.insert("x-missing".to_string(), "MISSING".to_string());
        map.insert("x-override".to_string(), "OVERRIDE".to_string());
        map.insert("x invalid".to_string(), "INVALID".to_string());

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-override"),
            HeaderValue::from_static("static"),
        );

        apply_env_http_headers(
            &map,
            |var| match var {
                // Leading space + trailing newline exercises trimming.
                "TENANT" => Some(" tenant-secret\n".to_string()),
                "BLANK" => Some("   ".to_string()),
                "OVERRIDE" => Some("from-env".to_string()),
                "INVALID" => Some("value".to_string()),
                _ => None,
            },
            &mut headers,
        );

        assert_eq!(headers.get("x-tenant-token").unwrap(), "tenant-secret");
        assert!(headers.get("x-blank").is_none());
        assert!(headers.get("x-missing").is_none());
        // A resolved env value overrides an existing header of the same name.
        assert_eq!(headers.get("x-override").unwrap(), "from-env");
        // An invalid header name is skipped rather than panicking.
        assert!(headers.get("x invalid").is_none());
    }

    #[test]
    fn endpoint_appends_path_before_a_base_url_query_without_configured_params() {
        let template =
            EndpointTemplate::new("https://gateway.example/v1?api-version=x", &IndexMap::new());
        let url = template.url_for_path("responses");
        assert!(
            url.starts_with("https://gateway.example/v1/responses?"),
            "url: {url}"
        );
        assert!(url.contains("api-version=x"), "url: {url}");
        assert!(!url.contains("x/responses"), "url: {url}");
    }

    #[test]
    fn messages_plus_anthropic_api_key_uses_x_api_key_and_not_authorization() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .is_some()
        );
        assert!(client.default_headers.get(AUTHORIZATION).is_none());
    }

    #[test]
    fn messages_plus_bearer_uses_authorization_and_not_x_api_key() {
        let cfg = SamplerConfig {
            api_key: Some("bearer-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(client.default_headers.get(AUTHORIZATION).is_some());
        assert!(
            client
                .default_headers
                .get(HeaderName::from_static("x-api-key"))
                .is_none()
        );
    }

    // Regression: a past change dropped User-Agent from sampling requests.
    #[test]
    fn sampling_client_always_has_user_agent() {
        let client = SamplingClient::new(minimal_config()).expect("build");
        assert!(client.default_headers.contains_key(USER_AGENT));
    }

    // Regression: a past change dropped HeaderInjector (traceparent) from sampling requests.
    #[test]
    fn header_injector_is_called_in_post() {
        #[derive(Debug)]
        struct TestInjector;
        impl crate::config::HeaderInjector for TestInjector {
            fn inject(&self, headers: &mut HeaderMap) {
                headers.insert(
                    HeaderName::from_static("traceparent"),
                    HeaderValue::from_static("00-test-trace-id-00"),
                );
            }
        }

        let mut config = minimal_config();
        config.header_injector = Some(std::sync::Arc::new(TestInjector));
        let client = SamplingClient::new(config).expect("build");
        let req = client
            .post("http://localhost/test")
            .build()
            .expect("build request");
        assert!(
            req.headers().contains_key("traceparent"),
            "HeaderInjector should inject traceparent into post() requests"
        );
    }

    #[test]
    fn user_agent_includes_origin_and_agent_product() {
        let origin = OriginClientInfo {
            product: "my-client".to_string(),
            version: Some("1.2.3".to_string()),
        };
        let ua = user_agent_string_for(&origin);
        assert!(ua.contains("my-client/1.2.3"));
        assert!(ua.contains(AGENT_PRODUCT));
    }

    #[test]
    fn user_agent_omits_origin_version_when_absent() {
        let origin = OriginClientInfo {
            product: "my-client".to_string(),
            version: None,
        };
        let ua = user_agent_string_for(&origin);
        // No slash between product and the grok-shell agent product.
        assert!(ua.starts_with("my-client grok-shell/"));
    }

    #[test]
    fn user_agent_collapses_when_origin_matches_agent() {
        let agent_version = xai_grok_version::VERSION.to_string();
        let origin = OriginClientInfo {
            product: AGENT_PRODUCT.to_string(),
            version: Some(agent_version.clone()),
        };
        let ua = user_agent_string_for(&origin);
        // Single product/version slot when the origin and agent match.
        assert!(ua.starts_with(&format!("{}/{}", AGENT_PRODUCT, agent_version)));
    }

    /// Counts callbacks for assertions in the tests below.
    #[derive(Default, Debug)]
    struct CountingCallback {
        invocations: std::sync::Mutex<Vec<(crate::attribution::SamplingConsumer, Option<String>)>>,
    }

    #[derive(Debug)]
    struct StaticBearerResolver(&'static str);

    impl crate::config::BearerResolver for StaticBearerResolver {
        fn current_bearer(&self) -> Option<String> {
            Some(self.0.to_string())
        }

        fn fail_closed_on_missing(&self) -> bool {
            false
        }
    }

    #[derive(Debug)]
    struct ScopedBearerResolver {
        bearer: &'static str,
        account: &'static str,
    }

    impl crate::config::BearerResolver for ScopedBearerResolver {
        fn current_bearer(&self) -> Option<String> {
            Some(self.bearer.to_owned())
        }

        fn current_auth(&self) -> Option<crate::config::ResolvedBearerAuth> {
            Some(crate::config::ResolvedBearerAuth {
                bearer: self.bearer.to_owned(),
                extra_headers: indexmap::indexmap! {
                    "ChatGPT-Account-ID".to_owned() => self.account.to_owned(),
                },
            })
        }

        fn reserved_headers(&self) -> &'static [&'static str] {
            &["chatgpt-account-id"]
        }

        fn fail_closed_on_missing(&self) -> bool {
            true
        }
    }

    #[test]
    fn replacing_existing_resolver_preserves_route_and_rebinds_scoped_auth() {
        let cfg = SamplerConfig {
            api_key: Some("stale-static-token".to_owned()),
            provider: xai_grok_sampling_types::ModelProvider::Codex,
            api_backend: ApiBackend::Responses,
            extra_headers: indexmap::indexmap! {
                "CHATGPT-ACCOUNT-ID".to_owned() => "spoofed-account".to_owned(),
            },
            bearer_resolver: Some(std::sync::Arc::new(ScopedBearerResolver {
                bearer: "unbound-token",
                account: "unbound-account",
            })),
            ..minimal_config()
        };
        let mut client = SamplingClient::new(cfg).expect("client should build");
        assert!(
            client.replace_bearer_resolver_if_present(std::sync::Arc::new(ScopedBearerResolver {
                bearer: "bound-token",
                account: "bound-account",
            },))
        );
        assert_eq!(
            client.provider(),
            xai_grok_sampling_types::ModelProvider::Codex
        );

        let request = client
            .post("https://example.test/backend-api/codex/responses")
            .build()
            .expect("request should build");
        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer bound-token")
        );
        assert_eq!(
            request
                .headers()
                .get("chatgpt-account-id")
                .and_then(|value| value.to_str().ok()),
            Some("bound-account")
        );
    }

    #[test]
    fn replacing_resolver_refuses_static_byok_client() {
        let cfg = SamplerConfig {
            api_key: Some("codex-byok".to_owned()),
            provider: xai_grok_sampling_types::ModelProvider::Codex,
            api_backend: ApiBackend::Responses,
            bearer_resolver: None,
            ..minimal_config()
        };
        let mut client = SamplingClient::new(cfg).expect("client should build");
        assert!(
            !client.replace_bearer_resolver_if_present(std::sync::Arc::new(ScopedBearerResolver {
                bearer: "must-not-win",
                account: "oauth-account",
            },))
        );

        let request = client
            .post("https://example.test/backend-api/codex/responses")
            .build()
            .expect("request should build");
        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer codex-byok")
        );
        assert!(request.headers().get("chatgpt-account-id").is_none());
    }

    impl crate::attribution::Auth401AttributionCallback for CountingCallback {
        fn record_401(
            &self,
            consumer: crate::attribution::SamplingConsumer,
            sent_bearer: Option<&str>,
        ) {
            self.invocations
                .lock()
                .unwrap()
                .push((consumer, sent_bearer.map(|s| s.to_string())));
        }
    }

    /// `extract_sent_bearer` strips the `"Bearer "` prefix off
    /// `Authorization` for OpenAI-completions backends and truncates the
    /// remaining bearer to the cross-crate prefix length.
    #[test]
    fn extract_sent_bearer_strips_bearer_prefix_for_openai_compat() {
        let cfg = SamplerConfig {
            api_key: Some("test-bearer-1234567890".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let bearer = client.extract_sent_bearer();
        // Bearer is truncated at the crate boundary -- callers
        // downstream of this method only ever see the prefix.
        assert_eq!(bearer.as_deref(), Some("test-bearer-"));
        assert_eq!(
            bearer.as_deref().map(str::len),
            Some(crate::attribution::SENT_BEARER_PREFIX_LEN),
        );
    }

    /// `extract_sent_bearer` reads `x-api-key` for Anthropic Messages API
    /// and truncates the value to the cross-crate prefix length.
    #[test]
    fn extract_sent_bearer_reads_x_api_key_for_messages() {
        let cfg = SamplerConfig {
            api_key: Some("anthropic-key-abc123".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let bearer = client.extract_sent_bearer();
        assert_eq!(bearer.as_deref(), Some("anthropic-ke"));
        assert_eq!(
            bearer.as_deref().map(str::len),
            Some(crate::attribution::SENT_BEARER_PREFIX_LEN),
        );
    }

    /// `extract_sent_bearer` returns `None` when no auth header is set.
    #[test]
    fn extract_sent_bearer_returns_none_when_no_header() {
        let cfg = SamplerConfig {
            api_key: None,
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert!(client.extract_sent_bearer().is_none());
    }

    #[test]
    fn live_bearer_resolver_uses_authorization_for_messages_plus_bearer() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-bearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let request = client
            .post("https://example.test/v1/messages")
            .build()
            .expect("request should build");
        let auth = request
            .headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        assert_eq!(auth, Some("Bearer fresh-bearer"));
        assert!(request.headers().get("x-api-key").is_none());
    }

    /// Regression: when `api_key` (which seeds `default_headers` with an
    /// `Authorization: Bearer ...`) AND a `bearer_resolver` are both set,
    /// `post()` must produce **exactly one** `Authorization` header on the
    /// wire. The pre-fix code used `RequestBuilder::header(AUTHORIZATION, ...)`
    /// which appends rather than replaces, causing two identical
    /// `Authorization` headers and a 400 from cli-chat-proxy.
    #[test]
    fn post_emits_single_authorization_with_api_key_and_bearer_resolver() {
        let cfg = SamplerConfig {
            api_key: Some("stale-bearer".to_string()),
            api_backend: ApiBackend::Responses,
            auth_scheme: AuthScheme::Bearer,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-bearer"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let request = client
            .post("https://example.test/v1/responses")
            .build()
            .expect("request should build");
        let auth_count = request.headers().get_all(AUTHORIZATION).iter().count();
        assert_eq!(
            auth_count, 1,
            "expected exactly one Authorization header, got {auth_count}"
        );
        assert_eq!(
            request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer fresh-bearer"),
        );
    }

    #[test]
    fn live_bearer_resolver_uses_x_api_key_for_messages_plus_anthropic_api_key() {
        let cfg = SamplerConfig {
            api_key: Some("stale-anthropic".to_string()),
            api_backend: ApiBackend::Messages,
            auth_scheme: AuthScheme::XApiKey,
            bearer_resolver: Some(std::sync::Arc::new(StaticBearerResolver("fresh-anthropic"))),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        let request = client
            .post("https://example.test/v1/messages")
            .build()
            .expect("request should build");
        let api_key = request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok());
        assert_eq!(api_key, Some("fresh-anthropic"));
        assert!(request.headers().get(AUTHORIZATION).is_none());
    }

    /// Bearers shorter than the prefix length pass through unchanged.
    /// Defensive against the truncation logic inadvertently widening
    /// short bearers (no panics, no zero-padding).
    #[test]
    fn extract_sent_bearer_short_bearer_passes_through_unchanged() {
        let cfg = SamplerConfig {
            api_key: Some("abc".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        assert_eq!(client.extract_sent_bearer().as_deref(), Some("abc"));
    }

    /// `record_401_attribution` invokes the wired callback with the
    /// expected `consumer` and the truncated bearer prefix that the
    /// wire would carry. The key assertion is that the callback
    /// receives the prefix only -- the full bearer never crosses the
    /// crate boundary.
    #[test]
    fn record_401_attribution_invokes_callback_with_extracted_bearer() {
        let cb = std::sync::Arc::new(CountingCallback::default());
        let cb_dyn: crate::attribution::SharedAttributionCallback = cb.clone();
        let cfg = SamplerConfig {
            api_key: Some("the-bearer-1234567890-extra-tail".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            attribution_callback: Some(cb_dyn),
            bearer_resolver: None,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        client.record_401_attribution(crate::attribution::SamplingConsumer::ChatCompletionsStream);
        let calls = cb.invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0,
            crate::attribution::SamplingConsumer::ChatCompletionsStream
        );
        // Prefix-only -- the `extra-tail` portion of the bearer is
        // dropped by `extract_sent_bearer` before the callback fires.
        assert_eq!(calls[0].1.as_deref(), Some("the-bearer-1"));
        assert_eq!(
            calls[0].1.as_deref().map(str::len),
            Some(crate::attribution::SENT_BEARER_PREFIX_LEN),
        );
    }

    /// Regression test: when a bearer_resolver is wired, `post()` must
    /// *replace* the Authorization header from `default_headers`, not
    /// append a second one. Duplicate Authorization headers cause
    /// Cloudflare to return 400 Bad Request.
    #[test]
    fn bearer_resolver_replaces_authorization_header() {
        #[derive(Debug)]
        struct StaticResolver(String);
        impl crate::config::BearerResolver for StaticResolver {
            fn current_bearer(&self) -> Option<String> {
                Some(self.0.clone())
            }

            fn fail_closed_on_missing(&self) -> bool {
                false
            }
        }

        let resolver: crate::config::SharedBearerResolver =
            std::sync::Arc::new(StaticResolver("fresh-token".to_string()));
        let cfg = SamplerConfig {
            api_key: Some("stale-token".to_string()),
            api_backend: ApiBackend::Responses,
            bearer_resolver: Some(resolver),
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");

        // Build a request to inspect the final headers.
        let builder = client.post("https://example.test/v1/responses");
        let request = builder.body("").build().expect("request should build");

        let auth_values: Vec<_> = request.headers().get_all(AUTHORIZATION).iter().collect();
        assert_eq!(
            auth_values.len(),
            1,
            "expected exactly one Authorization header, got {}: {:?}",
            auth_values.len(),
            auth_values
        );
        assert_eq!(
            auth_values[0].to_str().unwrap(),
            "Bearer fresh-token",
            "Authorization header should contain the resolver's fresh token"
        );
    }

    /// `record_401_attribution` is a no-op when `attribution_callback`
    /// is `None` (the BYOK / sampler-only path). The previous tests
    /// in this module construct clients without a callback and rely
    /// on this property holding.
    #[test]
    fn record_401_attribution_is_noop_without_callback() {
        let cfg = SamplerConfig {
            api_key: Some("bearer".to_string()),
            api_backend: ApiBackend::ChatCompletions,
            attribution_callback: None,
            bearer_resolver: None,
            ..minimal_config()
        };
        let client = SamplingClient::new(cfg).expect("client should build");
        // Must not panic.
        client.record_401_attribution(crate::attribution::SamplingConsumer::ChatCompletions);
    }

    #[test]
    fn codex_max_and_ultra_both_use_max_wire_effort() {
        for effort in [ReasoningEffort::Max, ReasoningEffort::Ultra] {
            let mut body = serde_json::json!({
                "input": [{ "type": "message", "role": "user", "content": "hello" }],
                "reasoning": { "effort": "xhigh" },
            });
            patch_codex_request_compat(
                &mut body,
                ModelProvider::Codex,
                false,
                Some(effort),
                Some(xai_grok_sampling_types::ReasoningSummary::Auto),
            );
            assert_eq!(
                body.pointer("/reasoning/effort")
                    .and_then(serde_json::Value::as_str),
                Some("max"),
            );
            assert_ne!(
                body.pointer("/reasoning/effort")
                    .and_then(serde_json::Value::as_str),
                Some("ultra"),
            );
        }
    }

    #[test]
    fn codex_uses_catalog_reasoning_summary_without_changing_xai() {
        use xai_grok_sampling_types::ReasoningSummary;

        for (summary, expected) in [
            (Some(ReasoningSummary::Auto), Some("auto")),
            (Some(ReasoningSummary::Concise), Some("concise")),
            (Some(ReasoningSummary::Detailed), Some("detailed")),
            (Some(ReasoningSummary::None), None),
            (None, None),
        ] {
            let mut codex = serde_json::json!({
                "input": [],
                "reasoning": {"effort": "high", "summary": "concise"}
            });
            patch_codex_request_compat(&mut codex, ModelProvider::Codex, false, None, summary);
            assert_eq!(
                codex
                    .pointer("/reasoning/summary")
                    .and_then(serde_json::Value::as_str),
                expected,
                "summary mode {summary:?}: {codex}",
            );
        }

        let mut xai = serde_json::json!({
            "input": [],
            "reasoning": {"effort": "high", "summary": "concise"}
        });
        patch_codex_request_compat(&mut xai, ModelProvider::Xai, false, None, None);
        assert_eq!(
            xai.pointer("/reasoning/summary"),
            Some(&serde_json::json!("concise"))
        );
    }

    #[test]
    fn codex_projects_base_system_prompt_to_instructions_and_later_system_to_developer() {
        let mut body = serde_json::json!({
            "input": [
                { "type": "message", "role": "system", "content": "base one" },
                {
                    "type": "message",
                    "role": "system",
                    "content": [{ "type": "input_text", "text": "base two" }]
                },
                { "type": "message", "role": "user", "content": "hello" },
                { "type": "message", "role": "system", "content": "late context" }
            ],
            "instructions": null
        });

        patch_codex_request_compat(&mut body, ModelProvider::Codex, false, None, None);

        assert_eq!(body["instructions"], "base one\n\nbase two");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "developer");
        assert!(input.iter().all(|item| item["role"] != "system"));
    }

    #[test]
    fn xai_keeps_system_roles_in_responses_input() {
        let mut body = serde_json::json!({
            "input": [
                { "type": "message", "role": "system", "content": "base" },
                { "type": "message", "role": "user", "content": "hello" }
            ],
            "instructions": null
        });

        patch_codex_request_compat(&mut body, ModelProvider::Xai, false, None, None);

        assert_eq!(body["input"][0]["role"], "system");
        assert!(body["instructions"].is_null());
    }

    #[test]
    fn codex_keeps_explicit_compaction_instructions_while_removing_system_input() {
        let mut body = serde_json::json!({
            "input": [
                { "type": "message", "role": "system", "content": "duplicate base" },
                { "type": "message", "role": "user", "content": "hello" }
            ],
            "instructions": "authoritative compact instructions"
        });

        patch_codex_request_compat(&mut body, ModelProvider::Codex, false, None, None);

        assert_eq!(body["instructions"], "authoritative compact instructions");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["role"], "user");
    }

    #[test]
    fn sol_ultra_injects_one_proactive_developer_policy() {
        let mut body = serde_json::json!({
            "input": [{ "type": "message", "role": "user", "content": "hello" }],
            "reasoning": { "effort": "xhigh" },
        });
        patch_codex_request_compat(
            &mut body,
            ModelProvider::Codex,
            true,
            Some(ReasoningEffort::Ultra),
            None,
        );
        patch_codex_request_compat(
            &mut body,
            ModelProvider::Codex,
            true,
            Some(ReasoningEffort::Ultra),
            None,
        );

        let input = body["input"].as_array().unwrap();
        let policies = input
            .iter()
            .filter(|item| is_multi_agent_mode_item(item))
            .collect::<Vec<_>>();
        assert_eq!(policies.len(), 1, "request patch must be idempotent");
        assert_eq!(policies[0]["role"], "developer");
        let text = policies[0]
            .pointer("/content/0/text")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(text.starts_with(MULTI_AGENT_MODE_OPEN_TAG));
        assert!(text.contains(PROACTIVE_MULTI_AGENT_MODE_TEXT));
        assert!(text.ends_with(MULTI_AGENT_MODE_CLOSE_TAG));
        assert_eq!(input.last().unwrap()["role"], "user");
    }

    #[test]
    fn non_ultra_sol_effort_keeps_delegation_explicit_request_only() {
        let mut body = serde_json::json!({
            "input": [{ "type": "message", "role": "user", "content": "hello" }],
            "reasoning": { "effort": "xhigh" },
        });
        patch_codex_request_compat(
            &mut body,
            ModelProvider::Codex,
            true,
            Some(ReasoningEffort::Max),
            None,
        );
        let policy = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| is_multi_agent_mode_item(item))
            .unwrap();
        let text = policy
            .pointer("/content/0/text")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(text.contains(EXPLICIT_REQUEST_ONLY_MULTI_AGENT_MODE_TEXT));
        assert!(!text.contains(PROACTIVE_MULTI_AGENT_MODE_TEXT));
    }

    #[test]
    fn multi_agent_policy_is_gated_to_codex_v2_models() {
        for (provider, multi_agent_v2) in
            [(ModelProvider::Xai, true), (ModelProvider::Codex, false)]
        {
            let mut body = serde_json::json!({
                "input": [{ "type": "message", "role": "user", "content": "hello" }],
                "reasoning": { "effort": "xhigh" },
            });
            patch_codex_request_compat(
                &mut body,
                provider,
                multi_agent_v2,
                Some(ReasoningEffort::Ultra),
                None,
            );
            assert!(
                body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| !is_multi_agent_mode_item(item)),
            );
        }
    }

    #[test]
    fn live_catalog_v2_capability_enables_proactive_policy() {
        let mut body = serde_json::json!({
            "input": [{ "type": "message", "role": "user", "content": "hello" }],
            "reasoning": { "effort": "xhigh" },
        });
        patch_codex_request_compat(
            &mut body,
            ModelProvider::Codex,
            true,
            Some(ReasoningEffort::Ultra),
            None,
        );
        let policy = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| is_multi_agent_mode_item(item))
            .unwrap();
        assert!(
            policy
                .pointer("/content/0/text")
                .and_then(serde_json::Value::as_str)
                .unwrap()
                .contains(PROACTIVE_MULTI_AGENT_MODE_TEXT),
        );
    }

    #[test]
    fn max_response_effort_parses_at_typed_boundary() {
        // The vendored async-openai fork carries a native `Max` variant, so a
        // Codex-echoed `max` effort parses without any pre-normalization and
        // the distinct tier survives into typed state.
        let response = serde_json::json!({
            "background": false,
            "created_at": 0,
            "id": "resp_max",
            "model": "gpt-5.6-sol",
            "object": "response",
            "output": [],
            "reasoning": { "effort": "max" },
            "status": "completed"
        });
        let typed = serde_json::from_value::<rs::Response>(response).unwrap();
        assert_eq!(
            typed.reasoning.and_then(|reasoning| reasoning.effort),
            Some(rs::ReasoningEffort::Max),
        );
    }

    #[test]
    fn streamed_max_response_effort_parses_before_event_parse() {
        let event = serde_json::json!({
            "type": "response.completed",
            "sequence_number": 1,
            "response": {
                "background": false,
                "created_at": 0,
                "id": "resp_max_stream",
                "model": "gpt-5.6-sol",
                "object": "response",
                "output": [],
                "reasoning": { "effort": "max" },
                "status": "completed"
            }
        })
        .to_string();
        let typed = deserialize_response_event(&event).unwrap();
        let rs::ResponseStreamEvent::ResponseCompleted(completed) = typed else {
            panic!("expected response.completed");
        };
        assert_eq!(
            completed
                .response
                .reasoning
                .and_then(|reasoning| reasoning.effort),
            Some(rs::ReasoningEffort::Max),
        );
    }

    #[test]
    fn codex_actionless_web_search_item_added_is_treated_as_progress() {
        let sse = r#"{
            "type": "response.output_item.added",
            "sequence_number": 2,
            "output_index": 0,
            "item": {
                "id": "ws_123",
                "type": "web_search_call",
                "status": "in_progress"
            }
        }"#;
        assert!(serde_json::from_str::<rs::ResponseStreamEvent>(sse).is_err());

        let event =
            deserialize_response_event_for_adapter(sse, provider_adapter(ModelProvider::Codex))
                .expect("an actionless in-progress web search item should parse as progress");
        let rs::ResponseStreamEvent::ResponseWebSearchCallInProgress(event) = event else {
            panic!("expected web search progress event");
        };
        assert_eq!(event.sequence_number, 2);
        assert_eq!(event.output_index, 0);
        assert_eq!(event.item_id, "ws_123");
    }

    #[test]
    fn codex_actionless_web_search_item_done_parses_with_sentinel_action() {
        let sse = r#"{
            "type": "response.output_item.done",
            "sequence_number": 3,
            "output_index": 0,
            "item": {
                "id": "ws_123",
                "type": "web_search_call",
                "status": "completed"
            }
        }"#;
        assert!(serde_json::from_str::<rs::ResponseStreamEvent>(sse).is_err());

        let event =
            deserialize_response_event_for_adapter(sse, provider_adapter(ModelProvider::Codex))
                .expect("an actionless completed web search item should parse with the sentinel");
        let rs::ResponseStreamEvent::ResponseOutputItemDone(event) = event else {
            panic!("expected ResponseOutputItemDone");
        };
        let rs::OutputItem::WebSearchCall(call) = event.item else {
            panic!("expected WebSearchCall");
        };
        assert_eq!(call.id, "ws_123");
        assert_eq!(call.status, rs::WebSearchToolCallStatus::Completed);
        assert!(xai_grok_sampling_types::is_sentinel_web_search_action(
            &call.action
        ));
    }

    #[test]
    fn codex_actionless_web_search_item_added_completed_parses_with_sentinel_action() {
        // A terminal-status added frame bypasses the in-progress/searching
        // projection above, so it must parse through the sentinel fill.
        let sse = r#"{
            "type": "response.output_item.added",
            "sequence_number": 2,
            "output_index": 0,
            "item": {
                "id": "ws_123",
                "type": "web_search_call",
                "status": "completed"
            }
        }"#;
        assert!(serde_json::from_str::<rs::ResponseStreamEvent>(sse).is_err());

        let event =
            deserialize_response_event_for_adapter(sse, provider_adapter(ModelProvider::Codex))
                .expect("an actionless completed web search item should parse with the sentinel");
        let rs::ResponseStreamEvent::ResponseOutputItemAdded(event) = event else {
            panic!("expected ResponseOutputItemAdded");
        };
        let rs::OutputItem::WebSearchCall(call) = event.item else {
            panic!("expected WebSearchCall");
        };
        assert!(xai_grok_sampling_types::is_sentinel_web_search_action(
            &call.action
        ));
    }

    #[test]
    fn codex_actionless_web_search_in_completed_output_parses_with_sentinel_action() {
        let event = serde_json::json!({
            "type": "response.completed",
            "sequence_number": 9,
            "response": {
                "created_at": 0,
                "id": "resp_ws",
                "model": "gpt-5.3-codex-spark",
                "object": "response",
                "output": [
                    {
                        "id": "ws_123",
                        "type": "web_search_call",
                        "status": "completed"
                    }
                ],
                "status": "completed"
            }
        })
        .to_string();
        assert!(serde_json::from_str::<rs::ResponseStreamEvent>(&event).is_err());

        let typed =
            deserialize_response_event_for_adapter(&event, provider_adapter(ModelProvider::Codex))
                .expect("an actionless web search item in the final output should parse");
        let rs::ResponseStreamEvent::ResponseCompleted(completed) = typed else {
            panic!("expected response.completed");
        };
        let [rs::OutputItem::WebSearchCall(call)] = completed.response.output.as_slice() else {
            panic!("expected a single WebSearchCall output item");
        };
        assert_eq!(call.id, "ws_123");
        assert!(xai_grok_sampling_types::is_sentinel_web_search_action(
            &call.action
        ));
    }

    #[test]
    fn deserialize_response_event_fills_custom_tool_call_id_on_output_item_added() {
        let sse = r#"{
            "type": "response.output_item.added",
            "sequence_number": 1,
            "output_index": 0,
            "item": {
                "type": "custom_tool_call",
                "call_id": "call_custom_1",
                "name": "code",
                "input": "",
                "status": "in_progress"
            }
        }"#;
        assert!(serde_json::from_str::<rs::ResponseStreamEvent>(sse).is_err());

        let event = deserialize_response_event(sse).expect("compatibility frame should parse");
        let rs::ResponseStreamEvent::ResponseOutputItemAdded(event) = event else {
            panic!("expected ResponseOutputItemAdded");
        };
        let rs::OutputItem::CustomToolCall(call) = event.item else {
            panic!("expected CustomToolCall");
        };
        assert_eq!(call.call_id, "call_custom_1");
        assert_eq!(call.id, "call_custom_1");
        assert_eq!(call.name, "code");
        assert_eq!(call.input, "");
    }

    #[test]
    fn deserialize_response_event_normalizes_current_x_search_call_shape() {
        let sse = r#"{
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "x_search_call",
                "id": "xs_123",
                "name": "",
                "arguments": "{\"query\":\"current xAI news\"}",
                "call_id": "",
                "status": "completed"
            }
        }"#;

        let event = deserialize_response_event(sse).expect("x_search_call frame should parse");
        let rs::ResponseStreamEvent::ResponseOutputItemAdded(event) = event else {
            panic!("expected ResponseOutputItemAdded");
        };
        assert_eq!(event.sequence_number, 0);
        let rs::OutputItem::CustomToolCall(call) = event.item else {
            panic!("expected normalized backend CustomToolCall");
        };
        assert_eq!(call.id, "xs_123");
        assert_eq!(call.call_id, "xs_123");
        assert_eq!(call.name, "x_search");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&call.input).unwrap(),
            serde_json::json!({"query": "current xAI news"}),
        );
    }

    #[test]
    fn deserialize_response_event_keeps_x_search_progress_as_noop_progress() {
        for event_type in [
            "response.x_search_call.in_progress",
            "response.x_search_call.searching",
            "response.x_search_call.completed",
        ] {
            let sse = serde_json::json!({
                "type": event_type,
                "output_index": 2,
                "item_id": "xs_progress"
            })
            .to_string();
            let event = deserialize_response_event(&sse)
                .unwrap_or_else(|error| panic!("{event_type} should parse: {error}"));
            let rs::ResponseStreamEvent::ResponseWebSearchCallSearching(event) = event else {
                panic!("expected recognized no-op progress for {event_type}");
            };
            assert_eq!(event.sequence_number, 0);
            assert_eq!(event.output_index, 2);
            assert_eq!(event.item_id, "xs_progress");
        }
    }

    #[test]
    fn deserialize_response_done_normalizes_x_search_and_minimal_usage() {
        let sse = serde_json::json!({
            "type": "response.done",
            "response": {
                "id": "resp_x_search",
                "object": "response",
                "model": "grok-4.5",
                "status": "completed",
                "output": [{
                    "type": "x_search_call",
                    "id": "xs_action",
                    "status": "completed",
                    "action": {"type": "search", "query": "OpenAI Codex"}
                }],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        })
        .to_string();

        let event = deserialize_response_event(&sse).expect("response.done should parse");
        let rs::ResponseStreamEvent::ResponseCompleted(event) = event else {
            panic!("expected normalized ResponseCompleted");
        };
        assert_eq!(event.sequence_number, 0);
        assert_eq!(event.response.created_at, 0);
        let usage = event
            .response
            .usage
            .expect("minimal usage should normalize");
        assert_eq!(usage.total_tokens, 15);
        assert_eq!(usage.input_tokens_details.cached_tokens, 0);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 0);
        let rs::OutputItem::CustomToolCall(call) = &event.response.output[0] else {
            panic!("expected normalized x_search backend item");
        };
        assert_eq!(call.id, "xs_action");
        assert_eq!(call.call_id, "xs_action");
        assert_eq!(call.name, "x_search");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&call.input).unwrap(),
            serde_json::json!({"type": "search", "query": "OpenAI Codex"}),
        );
    }

    #[test]
    fn synchronous_response_normalizes_x_search_and_echoed_provider_tool() {
        let mut response = serde_json::json!({
            "id": "resp_sync_x_search",
            "object": "response",
            "model": "grok-4.5",
            "status": "completed",
            "output": [{
                "type": "x_search_call",
                "id": "xs_sync",
                "status": "completed",
                "action": {"query": "Open Grok"}
            }],
            "tools": [{"type": "x_search"}],
            "usage": {"input_tokens": 3, "output_tokens": 2}
        });

        normalize_response_compat(&mut response, "completed");
        let response = serde_json::from_value::<rs::Response>(response)
            .expect("synchronous xAI response should fit the typed boundary");
        assert_eq!(response.created_at, 0);
        assert_eq!(response.tools, Some(Vec::new()));
        assert!(matches!(
            &response.output[0],
            rs::OutputItem::CustomToolCall(call)
                if call.id == "xs_sync" && call.name == "x_search"
        ));
        assert_eq!(response.usage.unwrap().total_tokens, 5);
    }

    #[test]
    fn deserialize_response_event_fills_custom_tool_call_id_in_terminal_output_arrays() {
        for (event_type, response_status) in [
            ("response.completed", "completed"),
            ("response.incomplete", "incomplete"),
            ("response.failed", "failed"),
        ] {
            let sse = serde_json::json!({
                "type": event_type,
                "sequence_number": 2,
                "response": {
                    "id": "resp_custom_1",
                    "object": "response",
                    "created_at": 0,
                    "model": "grok-build",
                    "status": response_status,
                    "output": [{
                        "type": "custom_tool_call",
                        "call_id": "call_custom_2",
                        "name": "code",
                        "input": "println!(\"hello\");",
                        "status": "completed",
                        "provider_extension": { "retained": true }
                    }]
                }
            })
            .to_string();
            assert!(serde_json::from_str::<rs::ResponseStreamEvent>(&sse).is_err());

            let event = deserialize_response_event(&sse)
                .unwrap_or_else(|error| panic!("{event_type} compatibility frame: {error}"));
            let output = match event {
                rs::ResponseStreamEvent::ResponseCompleted(event) => event.response.output,
                rs::ResponseStreamEvent::ResponseIncomplete(event) => event.response.output,
                rs::ResponseStreamEvent::ResponseFailed(event) => event.response.output,
                other => panic!("unexpected terminal event: {other:?}"),
            };
            let rs::OutputItem::CustomToolCall(call) = &output[0] else {
                panic!("expected CustomToolCall");
            };
            assert_eq!(call.call_id, "call_custom_2");
            assert_eq!(call.id, "call_custom_2");
            assert_eq!(call.name, "code");
            assert_eq!(call.input, "println!(\"hello\");");
        }
    }

    #[test]
    fn codex_unknown_event_filter_only_accepts_unknown_top_level_kinds() {
        let future = r#"{
            "type": "response.future_control",
            "sequence_number": 1,
            "payload": {"forward_compatible": true}
        }"#;
        let future_error = deserialize_response_event(future).unwrap_err();
        assert!(is_unknown_top_level_response_event(&future_error, future));

        let known_with_bad_nested_kind = serde_json::json!({
            "type": "response.completed",
            "sequence_number": 2,
            "response": {
                "id": "resp-future-output",
                "object": "response",
                "created_at": 0,
                "model": "gpt-5.6-sol",
                "status": "completed",
                "output": [{"type": "future_output"}]
            }
        })
        .to_string();
        let nested_error = deserialize_response_event(&known_with_bad_nested_kind).unwrap_err();
        assert!(
            !is_unknown_top_level_response_event(&nested_error, &known_with_bad_nested_kind),
            "a known event with a malformed nested payload must still fail"
        );

        let malformed = r#"{"type":"response.future_control""#;
        let malformed_error = deserialize_response_event(malformed).unwrap_err();
        assert!(!is_unknown_top_level_response_event(
            &malformed_error,
            malformed
        ));
    }

    #[test]
    fn deserialize_response_event_fills_custom_input_delta_item_id_from_call_id() {
        let sse = r#"{
            "type": "response.custom_tool_call_input.delta",
            "sequence_number": 3,
            "output_index": 1,
            "call_id": "call_custom_3",
            "delta": "partial input",
            "status": "in_progress",
            "provider_extension": { "retained": true }
        }"#;
        assert!(serde_json::from_str::<rs::ResponseStreamEvent>(sse).is_err());

        let event = deserialize_response_event(sse).expect("compatibility frame should parse");
        let rs::ResponseStreamEvent::ResponseCustomToolCallInputDelta(event) = event else {
            panic!("expected ResponseCustomToolCallInputDelta");
        };
        assert_eq!(event.item_id, "call_custom_3");
        assert_eq!(event.output_index, 1);
        assert_eq!(event.delta, "partial input");
    }

    #[test]
    fn deserialize_response_event_fills_custom_input_done_item_id_from_call_id() {
        let sse = r#"{
            "type": "response.custom_tool_call_input.done",
            "sequence_number": 4,
            "output_index": 2,
            "call_id": "call_custom_4",
            "input": "complete input",
            "status": "completed",
            "provider_extension": { "retained": true }
        }"#;
        assert!(serde_json::from_str::<rs::ResponseStreamEvent>(sse).is_err());

        let event = deserialize_response_event(sse).expect("compatibility frame should parse");
        let rs::ResponseStreamEvent::ResponseCustomToolCallInputDone(event) = event else {
            panic!("expected ResponseCustomToolCallInputDone");
        };
        assert_eq!(event.item_id, "call_custom_4");
        assert_eq!(event.output_index, 2);
        assert_eq!(event.input, "complete input");
    }

    #[test]
    fn normalize_response_event_compat_preserves_optional_custom_tool_fields() {
        let mut value = serde_json::json!({
            "type": "response.custom_tool_call_input.delta",
            "sequence_number": 5,
            "output_index": 3,
            "call_id": "call_custom_5",
            "delta": "input",
            "status": "in_progress",
            "provider_extension": { "retained": true }
        });

        normalize_response_event_compat(&mut value);

        assert_eq!(value["item_id"], "call_custom_5");
        assert_eq!(value["status"], "in_progress");
        assert_eq!(value["provider_extension"]["retained"], true);
    }

    #[test]
    fn custom_output_wire_patch_restores_name_and_original_image_detail() {
        let request = ConversationRequest::from_items(vec![
            xai_grok_sampling_types::ConversationItem::custom_tool_output(
                xai_grok_sampling_types::CustomToolOutputItem::new(
                    "call-code",
                    [
                        xai_grok_sampling_types::CustomToolOutputContent::text("A"),
                        xai_grok_sampling_types::CustomToolOutputContent::image(
                            "data:image/png;base64,iVBOR",
                            xai_grok_sampling_types::CustomToolOutputImageDetail::Original,
                        ),
                        xai_grok_sampling_types::CustomToolOutputContent::text("B"),
                    ],
                )
                .with_item_id("out-code")
                .with_name("exec"),
            ),
            xai_grok_sampling_types::ConversationItem::tool_result_with_ordered_content(
                "call-wait",
                vec![
                    xai_grok_sampling_types::CustomToolOutputContent::text("C"),
                    xai_grok_sampling_types::CustomToolOutputContent::image(
                        "data:image/png;base64,WAIT",
                        xai_grok_sampling_types::CustomToolOutputImageDetail::Original,
                    ),
                ],
            ),
        ]);
        let named_outputs = request.named_custom_tool_outputs();
        let mut original_images = request.original_detail_custom_output_images();
        original_images.extend(request.original_detail_function_output_images());
        let mut body = serde_json::to_value(rs::CreateResponse::from(&request)).unwrap();

        assert!(body["input"][0].get("name").is_none());
        assert_eq!(body["input"][0]["output"][1]["detail"], "high");
        assert_eq!(body["input"][1]["output"][1]["detail"], "high");

        patch_custom_tool_output_wire_fields(&mut body, &named_outputs, &original_images);

        assert_eq!(body["input"][0]["name"], "exec");
        assert_eq!(body["input"][0]["output"][1]["detail"], "original");
        assert_eq!(body["input"][0]["output"][0]["text"], "A");
        assert_eq!(body["input"][0]["output"][2]["text"], "B");
        assert_eq!(body["input"][1]["output"][1]["detail"], "original");
    }

    /// `response.completed` carrying
    /// `usage.context_details.{input_tokens, output_tokens}` rewrites
    /// `usage.total_tokens` in place to the live context length
    /// (`ctx.input + ctx.output`). Billing fields stay on the wire's
    /// cumulative values.
    #[test]
    fn deserialize_response_event_overrides_total_tokens_from_context_details() {
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 6003,
                    "input_tokens_details": { "cached_tokens": 1984 },
                    "output_tokens": 711,
                    "output_tokens_details": { "reasoning_tokens": 388 },
                    "total_tokens": 6714,
                    "context_details": {
                        "input_tokens": 5022,
                        "output_tokens": 571
                    }
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        // Billing fields stay cumulative — unchanged by context_details.
        assert_eq!(usage.input_tokens, 6003);
        assert_eq!(usage.output_tokens, 711);
        assert_eq!(usage.input_tokens_details.cached_tokens, 1984);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 388);
        // total_tokens rewritten to ctx.input + ctx.output (5022 + 571).
        // NOT the wire's cumulative total (6714).
        assert_eq!(usage.total_tokens, 5_593);
    }

    #[test]
    fn deserialize_response_event_stashes_cost_in_metadata() {
        let make = |ticks: i64| {
            format!(
                r#"{{
                "type": "response.completed",
                "sequence_number": 0,
                "response": {{
                    "id": "resp_1", "object": "response", "created_at": 0,
                    "model": "grok-build", "status": "completed", "output": [],
                    "usage": {{
                        "input_tokens": 10,
                        "input_tokens_details": {{ "cached_tokens": 0 }},
                        "output_tokens": 5,
                        "output_tokens_details": {{ "reasoning_tokens": 0 }},
                        "total_tokens": 15,
                        "cost_in_usd_ticks": {ticks}
                    }}
                }}
            }}"#
            )
        };

        let event = deserialize_response_event(&make(78)).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        assert_eq!(
            e.response
                .metadata
                .as_ref()
                .and_then(|m| m.get(COST_USD_TICKS_METADATA_KEY))
                .map(String::as_str),
            Some("78")
        );

        // The REST mapper backfills 0 for unbilled requests: no stash.
        let event = deserialize_response_event(&make(0)).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        assert!(e.response.metadata.is_none());
    }

    #[test]
    fn deserialize_response_event_total_tokens_unchanged_when_context_details_absent() {
        // Older / non-Responses backends omit `context_details`.
        // `total_tokens` passes through from the wire unchanged.
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 10000,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens": 100,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 10100
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        assert_eq!(usage.total_tokens, 10_100);
    }

    #[test]
    fn deserialize_response_event_total_tokens_unchanged_when_context_details_partial() {
        // Defensive: if the backend ever ships only one of the two
        // context_details fields, we don't have a complete picture of
        // the live context size, so leave `total_tokens` on the wire's
        // cumulative value instead of guessing (treating the missing
        // half as 0 would silently under-report).
        let sse = r#"{
            "type": "response.completed",
            "sequence_number": 0,
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0,
                "model": "grok-build",
                "status": "completed",
                "output": [],
                "usage": {
                    "input_tokens": 6003,
                    "input_tokens_details": { "cached_tokens": 1984 },
                    "output_tokens": 711,
                    "output_tokens_details": { "reasoning_tokens": 388 },
                    "total_tokens": 6714,
                    "context_details": {
                        "input_tokens": 5022
                    }
                }
            }
        }"#;
        let event = deserialize_response_event(sse).expect("parse");
        let rs::ResponseStreamEvent::ResponseCompleted(e) = event else {
            panic!("expected ResponseCompleted");
        };
        let usage = e.response.usage.expect("usage present");
        assert_eq!(usage.total_tokens, 6_714);
    }

    #[test]
    fn deserialize_response_event_ignores_context_details_on_non_terminal_events() {
        // Non-terminal events don't carry final usage; even if the backend ever
        // echoed `context_details` on one, we don't touch it.
        let sse = r#"{
            "type": "response.output_text.delta",
            "sequence_number": 0,
            "item_id": "item-1",
            "output_index": 0,
            "content_index": 0,
            "delta": "hello",
            "logprobs": []
        }"#;
        let event = deserialize_response_event(sse).expect("non-terminal event parses");
        assert!(matches!(
            event,
            rs::ResponseStreamEvent::ResponseOutputTextDelta(_)
        ));
    }
}
