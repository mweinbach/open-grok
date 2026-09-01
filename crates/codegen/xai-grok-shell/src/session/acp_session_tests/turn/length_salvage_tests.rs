//! Integration tests that run the real turn loop against a scripted Chat Completions backend whose responses end with `finish_reason: "length"`.
//!
//! There is deliberately no test of the default agent with the env gate ON: env mutation is process-global and racy in the parallel test binary.
//! That case gains coverage when the RemoteSettings gate makes the budget injectable.
use super::support::*;
use super::*;
use std::sync::Arc;
use std::time::Duration;
use xai_grok_test_support::sse::chat_completion_script_exact;
use xai_grok_test_support::{MockInferenceServer, ScriptedResponse, SseEvent};
/// Distinctive fragment of the continue reminder.
const REMINDER_MARKER: &str = "exceeded the output token limit";
/// `SessionActor` turn futures overflow the default test thread stack.
fn block_on_session(run: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run)
        .expect("spawn large-stack test thread")
        .join()
        .expect("test thread");
}
fn current_thread_local<Future>(future: Future)
where
    Future: std::future::Future<Output = ()> + 'static,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");
    tokio::task::LocalSet::new().block_on(&rt, future);
}
/// SSE stream ending `finish_reason: "length"`.
/// Its reasoning delta pins that report aggregation spans a synthesized `Reasoning` sibling.
fn length_sse(text: &str) -> ScriptedResponse {
    ScriptedResponse::sse(vec![
        SseEvent::data(
            serde_json::json!({
                "id": "chatcmpl-len",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": "test",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "reasoning_content": "thinking about the cut",
                        "content": text
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
        ),
        SseEvent::data(
            serde_json::json!({
                "id": "chatcmpl-len",
                "object": "chat.completion.chunk",
                "created": 1234567890,
                "model": "test",
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "length" }],
                "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
            })
            .to_string(),
        ),
        SseEvent::data("[DONE]".to_string()),
    ])
}
fn stop_sse(text: &str) -> ScriptedResponse {
    ScriptedResponse::sse(chat_completion_script_exact(text, "test"))
}
/// Build an actor wired to the mock server on the Chat Completions backend.
async fn salvage_test_actor(server: &MockInferenceServer) -> Arc<SessionActor> {
    salvage_test_actor_with_context(server, 0, 256_000).await
}
/// [`salvage_test_actor`] with a seeded token total and context window, for the tests where salvage interacts with compaction.
async fn salvage_test_actor_with_context(
    server: &MockInferenceServer,
    total_tokens: u64,
    context_window: u64,
) -> Arc<SessionActor> {
    salvage_test_actor_on_backend(
        server,
        total_tokens,
        context_window,
        xai_grok_sampling_types::ApiBackend::ChatCompletions,
    )
    .await
}
/// [`salvage_test_actor`] on an explicit backend.
/// Exercises the shared length policy across provider stream layers.
async fn salvage_test_actor_on_backend(
    server: &MockInferenceServer,
    total_tokens: u64,
    context_window: u64,
    backend: xai_grok_sampling_types::ApiBackend,
) -> Arc<SessionActor> {
    let sampling_cfg = xai_grok_sampler::SamplerConfig {
        api_key: Some("test-key".to_string()),
        base_url: server.url(),
        model: "test".to_string(),
        api_backend: backend.clone(),
        context_window,
        max_retries: Some(0),
        idle_timeout_secs: Some(30),
        ..Default::default()
    };
    let (sampler_event_tx, sampler_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_grok_sampler::SamplingEvent>();
    let sampler_handle = xai_grok_sampler::SamplerActor::spawn(
        sampling_cfg,
        xai_grok_sampler::RetryPolicy {
            max_retries: 0,
            rate_limit_retry_threshold: 0,
            ..Default::default()
        },
        sampler_event_tx,
    );
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let _ = spawn_capturing_gateway_loop(gateway_rx);
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    spawn_persistence_ack_drainer(persistence_rx);
    let mut actor =
        create_test_actor(total_tokens, context_window, 85, gateway_tx, persistence_tx).await;
    actor.sampler_handle = sampler_handle;
    let mut cfg = actor
        .chat_state_handle
        .get_sampling_config()
        .await
        .expect("test actor has sampling config");
    cfg.base_url = server.url();
    cfg.api_backend = backend;
    cfg.model = "test".to_string();
    actor.chat_state_handle.update_sampling_config(cfg);
    let mut creds = actor.chat_state_handle.get_credentials().await;
    creds.api_key = Some("test-key".to_string());
    actor.chat_state_handle.update_credentials(creds);
    actor
        .workspace_ops
        .bind_local_session(
            &actor.session_id_string(),
            actor.tool_context.cwd.as_path().to_path_buf(),
            actor.tool_context.hunk_tracker_handle.clone(),
            actor.agent.borrow().tool_bridge().toolset(),
            None,
        )
        .expect("bind_local_session");
    let actor = Arc::new(actor);
    {
        let drainer = actor.clone();
        let mut sampler_event_rx = sampler_event_rx;
        tokio::task::spawn_local(async move {
            while let Some(event) = sampler_event_rx.recv().await {
                drainer.handle_sampling_event(event).await;
            }
        });
    }
    actor
}
async fn run_prompt(actor: &Arc<SessionActor>, prompt_id: &str) -> PromptTurnResult {
    let prompt_blocks = vec![acp::ContentBlock::Text(acp::TextContent::new(
        "write out the numbers".to_string(),
    ))];
    tokio::time::timeout(
        Duration::from_secs(60),
        actor.handle_prompt(
            prompt_id,
            prompt_blocks,
            PromptMode::Agent,
            None,
            None,
            None,
            None,
            true,
            false,
            None,
            None,
            None,
        ),
    )
    .await
    .expect("turn must finish within timeout")
}
/// Default agent with the gate off (no env var): a Length response is the legacy non-retryable hard failure.
/// This is the safety property that nothing changes for production default agents until the rollout flag lands.
#[test]
fn default_agent_gate_off_hard_fails_on_length() {
    if std::env::var_os("OPENGROK_LENGTH_SALVAGE").is_some() {
        eprintln!("skipping: ambient OPENGROK_LENGTH_SALVAGE would flip the gate under test");
        return;
    }
    block_on_session(|| {
        current_thread_local(async {
            let server = MockInferenceServer::start().await.expect("mock server");
            server.enqueue_response("/v1/chat/completions", length_sse("one, two, three,"));
            let actor = salvage_test_actor(&server).await;
            let outcome = run_prompt(&actor, "length-gate-off").await;
            let err = outcome.expect_err("gate off: Length must hard-fail the turn");
            let err_str = format!("{err:?}");
            assert!(
                err_str.contains("max_tokens") || err_str.contains("max tokens"),
                "the failure must be the max-tokens truncation error: {err_str}"
            );
            let conv = actor.chat_state_handle.get_conversation().await;
            let all_text: Vec<String> = conv.iter().map(|item| item.text_content()).collect();
            assert!(
                !all_text.iter().any(|text| text.contains(REMINDER_MARKER)),
                "no continue reminder without the gate: {all_text:#?}"
            );
        });
    });
}
/// Terminal error whose metadata reports a tiny context window.
/// The overflow heuristic compares the token estimate to that window, so it fires for any nonempty history.
fn error_with_tiny_window(
    kind: xai_grok_sampler::SamplingErrorKind,
    status_code: u16,
) -> xai_grok_sampler::SamplingErrorInfo {
    xai_grok_sampler::SamplingErrorInfo {
        kind,
        status_code: Some(status_code),
        message: "terminal failure".to_string(),
        should_retry: None,
        error_code: None,
        is_retryable: false,
        retry_after_secs: None,
        model_metadata: Some(xai_grok_sampling_types::ResponseModelMetadata {
            context_window: Some(1),
            max_completion_tokens: None,
            models_etag: None,
        }),
        empty_response_context: None,
        doom_loop_triggers: None,
        doom_loop_aborted_at_chunk: None,
        credential: xai_grok_sampling_types::SentCredential::Unknown,
    }
}
/// A rate-limited terminal error mid-continuation keeps its terminal arm even when the estimate exceeds the reported window.
/// The quiet truncated-complete arm is only for overflows and empty caps, never for kinds naming a non-overflow cause.
#[test]
fn rate_limit_mid_continuation_stays_terminal() {
    block_on_session(|| {
        current_thread_local(async {
            let server = MockInferenceServer::start().await.expect("mock server");
            let actor = salvage_test_actor(&server).await;
            actor.chat_state_handle.push_user_message(
                xai_grok_sampling_types::ConversationItem::user(
                    "enough history that the token estimate clears the tiny window",
                ),
            );
            let error =
                error_with_tiny_window(xai_grok_sampler::SamplingErrorKind::RateLimited, 429);
            let Err(err) = actor
                .handle_sampling_failure_with_recovery(error, 0, transient_state(0, true), true)
                .await
            else {
                panic!("a mid-salvage rate limit is still terminal");
            };
            assert_eq!(
                i32::from(err.code),
                crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                "must take the rate-limited arm, not the quiet truncated-complete arm: {err:?}"
            );
        });
    });
}
/// A genuine context overflow mid-continuation completes the turn truncated even while auto-compaction is sticky-suppressed.
/// The overflow signal must not depend on the compaction gate.
#[test]
fn suppressed_overflow_mid_continuation_still_completes_truncated() {
    block_on_session(|| {
        current_thread_local(async {
            let server = MockInferenceServer::start().await.expect("mock server");
            let actor = salvage_test_actor(&server).await;
            actor.chat_state_handle.push_user_message(
                xai_grok_sampling_types::ConversationItem::user(
                    "enough history that the token estimate clears the tiny window",
                ),
            );
            actor.compaction.auto_compact_suppressed.store(
                crate::session::compaction_config::SUPPRESS_STICKY,
                std::sync::atomic::Ordering::Relaxed,
            );
            let error = error_with_tiny_window(xai_grok_sampler::SamplingErrorKind::Api, 500);
            let Err(err) = actor
                .handle_sampling_failure_with_recovery(error, 0, transient_state(0, true), true)
                .await
            else {
                panic!("the quiet arm returns the typed error");
            };
            assert!(
                crate::sampling::error::is_max_tokens_turn_error(&err),
                "the turn loop needs the max-tokens marker to complete truncated: {err:?}"
            );
            let cause = err
                .data
                .as_ref()
                .and_then(|data| data.get(crate::sampling::error::SALVAGE_CAUSE_KEY))
                .and_then(|value| value.as_str());
            assert_eq!(
                cause,
                Some(crate::sampling::error::SALVAGE_CAUSE_OVERFLOW),
                "an over-window failure is the overflow population"
            );
        });
    });
}
/// Messages-backend SSE: text and a completed `tool_use` block, terminated `stop_reason: "max_tokens"`.
/// Only this backend delivers `Length` with tool calls to the turn loop.
/// Chat Completions rewrites the stop reason to `ToolCalls` at the stream layer.
fn messages_length_with_tool_call_sse(
    call_id: &str,
    name: &str,
    arguments: &str,
) -> ScriptedResponse {
    let events = vec![
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_len_tools", "type": "message", "role": "assistant",
                "content": [], "model": "test", "stop_reason": null,
                "usage": {
                    "input_tokens": 10, "output_tokens": 0,
                    "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
                }
            }
        }),
        serde_json::json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "recording the todo"}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({
            "type": "content_block_start", "index": 1,
            "content_block": {"type": "tool_use", "id": call_id, "name": name, "input": {}}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": arguments}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 1}),
        serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "max_tokens"},
            "usage": {"output_tokens": 5, "input_tokens": 10}
        }),
        serde_json::json!({"type": "message_stop"}),
    ];
    ScriptedResponse::sse(
        events
            .into_iter()
            .map(|event| SseEvent::data(event.to_string()))
            .collect(),
    )
}
/// Messages SSE: plain text terminated `stop_reason: "end_turn"`.
fn messages_stop_sse(text: &str) -> ScriptedResponse {
    let events = vec![
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_stop", "type": "message", "role": "assistant",
                "content": [], "model": "test", "stop_reason": null,
                "usage": {
                    "input_tokens": 10, "output_tokens": 0,
                    "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
                }
            }
        }),
        serde_json::json!({
            "type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}
        }),
        serde_json::json!({
            "type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": text}
        }),
        serde_json::json!({"type": "content_block_stop", "index": 0}),
        serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 5, "input_tokens": 10}
        }),
        serde_json::json!({"type": "message_stop"}),
    ];
    ScriptedResponse::sse(
        events
            .into_iter()
            .map(|event| SseEvent::data(event.to_string()))
            .collect(),
    )
}

fn transient_state(step_attempts: u32, enabled: bool) -> TransientRetryState {
    TransientRetryState {
        step_attempts,
        prompt_attempts: 0,
        episode_start: None,
        enabled,
    }
}
