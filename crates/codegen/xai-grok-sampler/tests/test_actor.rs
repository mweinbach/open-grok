//! Integration tests for the M4 actor + request_task layer.
//!
//! Tests are integration-style (in `tests/`) rather than unit tests
//! because they require a real `tokio::runtime` and a mock HTTP
//! server (axum) to talk to the `SamplingClient`. Happy-path SSE
//! payloads come from `xai_grok_test_support::sse`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use futures_util::stream::{self, StreamExt};
use indexmap::IndexMap;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use xai_grok_sampler::{
    ApiBackend, RequestId, RetryPolicy, SamplerActor, SamplerConfig, SamplingChannel,
    SamplingClient, SamplingErrorKind, SamplingEvent,
};
use xai_grok_sampling_types::{
    ClientTool, ConversationItem, ConversationRequest, CustomToolOutputContent,
    CustomToolOutputItem, DoomLoopRecoveryPolicy, HostedTool, ModelProvider, ReasoningSummary,
    ToolCall, ToolSpec, UserItem,
};
use xai_grok_test_support::{SseEvent, sse};

// ---------------------------------------------------------------------------
// Mock server harness
// ---------------------------------------------------------------------------

struct MockServer {
    addr: SocketAddr,
    shutdown_tx: oneshot::Sender<()>,
}

impl MockServer {
    async fn spawn(app: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        // Give the server a moment to start.
        tokio::time::sleep(Duration::from_millis(20)).await;
        Self { addr, shutdown_tx }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
    }
}

// ---------------------------------------------------------------------------
// Config + request helpers
// ---------------------------------------------------------------------------

fn test_config(base_url: String, model: &str) -> SamplerConfig {
    SamplerConfig {
        api_key: Some("test-key".into()),
        base_url,
        model: model.into(),
        max_completion_tokens: Some(1024),
        temperature: None,
        top_p: None,
        api_backend: ApiBackend::ChatCompletions,
        provider: Default::default(),
        auth_scheme: Default::default(),
        extra_headers: IndexMap::new(),
        query_params: IndexMap::new(),
        env_http_headers: IndexMap::new(),
        context_window: 128_000,
        force_http1: false,
        // Keep retries minimal so tests don't take forever.
        max_retries: Some(2),
        stream_tool_calls: false,
        idle_timeout_secs: Some(30),
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

fn user_request(text: &str) -> ConversationRequest {
    ConversationRequest {
        items: vec![ConversationItem::User(UserItem {
            content: vec![xai_grok_sampling_types::ContentPart::Text {
                text: std::sync::Arc::<str>::from(text),
            }],
            synthetic_reason: None,
            ..Default::default()
        })],
        ..Default::default()
    }
}

fn native_exec_history_request() -> ConversationRequest {
    let call = ToolCall::custom(
        "call-native-exec",
        "ctc-native-exec",
        "exec",
        "const answer = 40 + 2;",
    );
    let encoded_result_id = call.id.clone();
    ConversationRequest::from_items(vec![
        ConversationItem::assistant_tool_calls(vec![call]),
        ConversationItem::custom_tool_output(
            CustomToolOutputItem::text("call-native-exec", "progress").with_name("exec"),
        ),
        ConversationItem::tool_result_with_ordered_content(
            encoded_result_id.as_ref(),
            vec![CustomToolOutputContent::text("42")],
        ),
        ConversationItem::user("continue"),
    ])
}

fn xai_function_exec_history_request() -> ConversationRequest {
    ConversationRequest::from_items(vec![
        ConversationItem::assistant_tool_calls(vec![ToolCall {
            id: "call-xai-exec".into(),
            name: "exec".into(),
            arguments: r#"{"source":"return 42"}"#.into(),
        }]),
        ConversationItem::custom_tool_output(
            CustomToolOutputItem::text("call-xai-exec", "progress").with_name("exec"),
        ),
        ConversationItem::tool_result_with_ordered_content(
            "call-xai-exec",
            vec![CustomToolOutputContent::text("42")],
        ),
        ConversationItem::user("continue"),
    ])
}

// ---------------------------------------------------------------------------
// SSE generators
// ---------------------------------------------------------------------------

/// Render test-helper [`SseEvent`]s (optional `event:` name + `data:`) as
/// axum SSE events for this file's router-based harness.
fn sse_events_to_axum(events: Vec<SseEvent>) -> Vec<Event> {
    events
        .into_iter()
        .map(|e| {
            let ev = Event::default().data(e.data);
            match e.event {
                Some(name) => ev.event(name),
                None => ev,
            }
        })
        .collect()
}

fn text_chunk_event(content: &str, finish: bool) -> Event {
    let chunk = json!({
        "id": "chatcmpl-test",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": "test-model",
        "choices": [{
            "index": 0,
            "delta": { "role": "assistant", "content": content },
            "finish_reason": if finish { json!("stop") } else { json!(null) }
        }]
    });
    Event::default().data(chunk.to_string())
}

// ---------------------------------------------------------------------------
// Actor lifecycle
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_then_active_count_zero_then_cancel_unknown_is_noop() {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cfg = test_config("http://127.0.0.1:0/v1".into(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);
    assert_eq!(handle.active_count().await, 0);
    handle.cancel(RequestId::from("nonexistent"));
    // Re-querying should still be 0 (cancel of unknown id is no-op).
    assert_eq!(handle.active_count().await, 0);
}

// ---------------------------------------------------------------------------
// Submit + event flow
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_emits_started_first_token_channel_completed() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let events = sse::chat_completion_events("hello world", "test-model");
            Sse::new(stream::iter(
                events.into_iter().map(Ok::<_, std::convert::Infallible>),
            ))
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-1");
    handle.submit(rid.clone(), user_request("hi"));

    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(5)).await;
    server.shutdown();

    assert!(matches!(events[0], SamplingEvent::StreamStarted { .. }));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SamplingEvent::FirstToken { .. }))
    );

    let texts: Vec<&str> = events
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
    assert_eq!(texts.join(""), "hello world");

    match events.last().unwrap() {
        SamplingEvent::Completed {
            request_id,
            response,
            ..
        } => {
            assert_eq!(request_id, &rid);
            if let Some(a) = response.assistant() {
                assert_eq!(a.content.as_ref(), "hello world");
            } else {
                panic!("expected Assistant message");
            }
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kimi_chat_request_uses_provider_key_and_standard_function_tools() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(
            move |headers: HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = Arc::clone(&captured_handler);
                async move {
                    captured.lock().unwrap().push((headers, body));
                    let events = sse::chat_completion_events("ok", "kimi-k3");
                    Sse::new(stream::iter(
                        events.into_iter().map(Ok::<_, std::convert::Infallible>),
                    ))
                }
            },
        ),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let mut config = test_config(server.base_url(), "kimi-k3");
    config.provider = ModelProvider::Kimi;
    config.temperature = Some(0.7);
    config.top_p = Some(0.95);
    config.reasoning_effort = Some(xai_grok_sampling_types::ReasoningEffort::Max);
    config.client_identifier = Some("must-not-become-x-grok-metadata".into());
    let handle = SamplerActor::spawn(config, RetryPolicy::default(), event_tx);

    let mut request = user_request("inspect this repository");
    request.x_grok_session_id = Some("must-not-leak".into());
    request.tools.push(xai_grok_sampling_types::ToolSpec {
        name: "read_file".into(),
        description: Some("Read a repository file".into()),
        parameters: json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }),
    });
    handle
        .submit_and_collect(RequestId::from("req-kimi-chat"), request)
        .await
        .expect("Kimi Chat Completions request should complete");
    server.shutdown();

    let captured = captured.lock().unwrap();
    let (headers, body) = captured.first().expect("one Kimi request");
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-key")
    );
    assert!(
        headers
            .keys()
            .all(|name| !name.as_str().starts_with("x-grok-")),
        "Kimi must not receive xAI-private request headers: {headers:?}"
    );
    assert_eq!(body["model"], "kimi-k3");
    assert_eq!(body["reasoning_effort"], "max");
    assert!(body.get("temperature").is_none());
    assert!(body.get("top_p").is_none());
    assert_eq!(body.pointer("/tools/0/type"), Some(&json!("function")));
    assert_eq!(
        body.pointer("/tools/0/function/name"),
        Some(&json!("read_file"))
    );
}

// ---------------------------------------------------------------------------
// submit_and_collect
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_and_collect_returns_response() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let events = sse::chat_completion_events("collected response", "test-model");
            Sse::new(stream::iter(
                events.into_iter().map(Ok::<_, std::convert::Infallible>),
            ))
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-collect");
    let result = handle
        .submit_and_collect(rid, user_request("hi"))
        .await
        .expect("collected ok");
    server.shutdown();

    let (response, _metrics) = result;
    let a = response.assistant().expect("assistant item present");
    assert_eq!(a.content.as_ref(), "collected response");
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_in_flight_request_terminates_task() {
    // Server that yields one chunk then hangs.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            let stream = stream::iter(vec![Ok::<_, std::convert::Infallible>(text_chunk_event(
                "starting", false,
            ))])
            .chain(stream::pending());
            Sse::new(stream)
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-cancel");
    handle.submit(rid.clone(), user_request("hi"));

    // Wait for the first token to arrive so we know the request is in flight.
    let _ = await_event_matching(
        &mut event_rx,
        |e| matches!(e, SamplingEvent::FirstToken { .. }),
        Duration::from_secs(5),
    )
    .await
    .expect("first token");

    handle.cancel(rid.clone());

    // Expect a Failed event with the cancellation message.
    let failed = await_event_matching(
        &mut event_rx,
        |e| matches!(e, SamplingEvent::Failed { .. }),
        Duration::from_secs(5),
    )
    .await
    .expect("Failed event after cancel");

    if let SamplingEvent::Failed { error, .. } = failed {
        assert!(error.message.contains("cancelled"));
    }

    // Wait briefly for the task to clean up.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(handle.active_count().await, 0);
    server.shutdown();
}

// ---------------------------------------------------------------------------
// Concurrent requests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_requests_complete_with_correct_request_ids() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let events = sse::chat_completion_events(&format!("response-{n}"), "test-model");
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid_a = RequestId::from("req-a");
    let rid_b = RequestId::from("req-b");
    handle.submit(rid_a.clone(), user_request("a"));
    handle.submit(rid_b.clone(), user_request("b"));

    // Drain until we see Completed for both.
    let mut completed_a = false;
    let mut completed_b = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(completed_a && completed_b) {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            panic!(
                "timed out waiting for both requests to complete: a={completed_a}, b={completed_b}"
            );
        }
        let remaining = deadline - now;
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Some(SamplingEvent::Completed { request_id, .. })) if request_id == rid_a => {
                completed_a = true;
            }
            Ok(Some(SamplingEvent::Completed { request_id, .. })) if request_id == rid_b => {
                completed_b = true;
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("event channel closed"),
            Err(_) => panic!("timeout"),
        }
    }
    server.shutdown();
}

// ---------------------------------------------------------------------------
// Retry on transient transport error
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_on_500_then_succeeds() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // First attempt: server error.
                    Err::<Sse<_>, (StatusCode, String)>((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({ "error": { "message": "transient" } }).to_string(),
                    ))
                } else {
                    // Subsequent attempts: success.
                    let events = sse::chat_completion_events("ok", "test-model");
                    Ok(Sse::new(stream::iter(
                        events.into_iter().map(Ok::<_, std::convert::Infallible>),
                    )))
                }
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    // Lots of retries available; backoff is jittered around 2s on first
    // retry, so this test takes a bit to run.
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-retry");
    handle.submit(rid.clone(), user_request("hi"));

    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(15)).await;
    server.shutdown();

    let saw_retrying = events
        .iter()
        .any(|e| matches!(e, SamplingEvent::Retrying { .. }));
    assert!(saw_retrying, "expected at least one Retrying event");

    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            if let Some(a) = response.assistant() {
                assert_eq!(a.content.as_ref(), "ok");
            }
        }
        other => panic!("expected Completed after retry, got {other:?}"),
    }

    assert!(
        counter.load(Ordering::SeqCst) >= 2,
        "server hit at least twice"
    );
}

// ---------------------------------------------------------------------------
// Rate limit exhausts threshold
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rate_limit_exhausts_at_threshold_and_yields_failed() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<
                    Sse<
                        futures_util::stream::Iter<
                            std::vec::IntoIter<Result<Event, std::convert::Infallible>>,
                        >,
                    >,
                    (StatusCode, String),
                >((
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({ "error": { "message": "slow down" } }).to_string(),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-429");
    handle.submit(rid.clone(), user_request("hi"));

    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(60)).await;
    server.shutdown();

    match events.last().unwrap() {
        SamplingEvent::Failed { error, .. } => {
            assert_eq!(error.kind, SamplingErrorKind::RateLimited);
            assert_eq!(error.status_code, Some(429));
        }
        other => panic!("expected Failed(RateLimited), got {other:?}"),
    }

    let hits = counter.load(Ordering::SeqCst);
    // RATE_LIMIT_RETRY_THRESHOLD = 2, so the actor stops after two
    // attempts (the first attempt + one retry that also 429s = 2
    // hits). Allow a small slack in case scheduling fires a third
    // attempt before the threshold check.
    assert!((1..=3).contains(&hits), "expected 1-3 hits, got {hits}");
}

// ---------------------------------------------------------------------------
// Auth error -> EmitToSession (immediate)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_401_emits_failed_immediately_no_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err::<
                    Sse<
                        futures_util::stream::Iter<
                            std::vec::IntoIter<Result<Event, std::convert::Infallible>>,
                        >,
                    >,
                    (StatusCode, String),
                >((StatusCode::UNAUTHORIZED, "unauthorized".to_string()))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "test-model");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let rid = RequestId::from("req-auth");
    handle.submit(rid.clone(), user_request("hi"));

    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(5)).await;
    server.shutdown();

    // Auth errors are session-owned -- `classify_error` returns
    // `EmitToSession` so the actor emits Failed immediately without
    // retrying.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SamplingEvent::Retrying { .. }))
    );
    match events.last().unwrap() {
        SamplingEvent::Failed { error, .. } => {
            assert_eq!(error.kind, SamplingErrorKind::Auth);
        }
        other => panic!("expected Failed(Auth), got {other:?}"),
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1, "no retries on 401");
}

// ---------------------------------------------------------------------------
// Anthropic Messages API: refusal stop_reason + mid-stream parse failure
// ---------------------------------------------------------------------------

fn messages_config(base_url: String) -> SamplerConfig {
    let mut cfg = test_config(base_url, "messages-compatible-model");
    cfg.api_backend = ApiBackend::Messages;
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_backend_projects_native_exec_history_before_streaming_conversion() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/messages",
        post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = Arc::clone(&captured_handler);
            async move {
                captured.lock().unwrap().push(body);
                let events =
                    sse::messages_api_events("done", "messages-compatible-model", "end_turn");
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let mut config = messages_config(server.base_url());
    config.provider = ModelProvider::Xai;
    config.model = "gpt-slug-does-not-select-provider".into();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(config, RetryPolicy::default(), event_tx);

    handle
        .submit_and_collect(
            RequestId::from("messages-native-exec-history"),
            native_exec_history_request(),
        )
        .await
        .expect("native exec history should project before Messages conversion");
    server.shutdown();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let blocks = captured[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    let call = blocks
        .iter()
        .find(|block| block["type"] == "tool_use")
        .expect("projected exec tool use");
    assert_eq!(call["id"], "call-native-exec");
    assert_eq!(call["name"], "exec");
    assert_eq!(call["input"], json!({"source": "const answer = 40 + 2;"}));
    let results = blocks
        .iter()
        .filter(|block| block["type"] == "tool_result")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_use_id"], "call-native-exec");
}

/// Regression for the refusal-stop_reason incident: a well-formed stream
/// terminated by `stop_reason: "refusal"` must produce a successful
/// completion from EXACTLY ONE request — no retry storm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_refusal_stream_completes_with_single_request() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/messages",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let events = sse::messages_api_events(
                    "I can't help with that.",
                    "messages-compatible-model",
                    "refusal",
                );
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        messages_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    let result = handle
        .submit_and_collect(RequestId::from("req-refusal"), user_request("hi"))
        .await;
    server.shutdown();

    let (response, _metrics) = result.expect("refusal-terminated turn must complete");
    let a = response.assistant().expect("assistant item present");
    assert_eq!(a.content.as_ref(), "I can't help with that.");
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "refusal must not trigger retries"
    );
}

/// Empty-bodied refusal: `message_start → message_delta(refusal) →
/// message_stop` with zero content blocks must complete from exactly one
/// request — the content-less response must not be classified as a retryable
/// EmptyResponse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_empty_refusal_completes_without_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/messages",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut events =
                    sse::messages_api_events("", "messages-compatible-model", "refusal");
                // Drop the content block events; keep start/delta/stop only.
                events.drain(1..4);
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        messages_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    handle.submit(RequestId::from("req-empty-refusal"), user_request("hi"));
    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(10)).await;
    server.shutdown();

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SamplingEvent::Retrying { .. })),
        "content-less refusal must not be retried"
    );
    match events.last().unwrap() {
        SamplingEvent::Completed { response, .. } => {
            assert_eq!(
                response.stop_reason,
                Some(xai_grok_sampling_types::StopReason::ContentFilter)
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1, "exactly one request");
}

/// A mid-stream event that fails serde (after a valid `message_start`) is a
/// deterministic response-parse failure: Fatal on the first attempt, surfaced
/// as a non-retryable Serialization error — never a retry storm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_unparseable_event_is_fatal_without_retry() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app =
        Router::new().route(
            "/v1/messages",
            post(move || {
                let counter = Arc::clone(&counter_handler);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let mut events =
                        sse::messages_api_events("hello", "messages-compatible-model", "end_turn");
                    // Replace the tail with a `message_delta` missing the
                    // required `delta` field — fails MessageStreamEvent serde.
                    events.truncate(4);
                    events.push(Event::default().data(
                        json!({"type":"message_delta","usage":{"output_tokens":1}}).to_string(),
                    ));
                    Sse::new(stream::iter(
                        events.into_iter().map(Ok::<_, std::convert::Infallible>),
                    ))
                }
            }),
        );
    let server = MockServer::spawn(app).await;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        messages_config(server.base_url()),
        RetryPolicy::default(),
        event_tx,
    );

    handle.submit(RequestId::from("req-bad-event"), user_request("hi"));
    let events = drain_until_terminal(&mut event_rx, Duration::from_secs(10)).await;
    server.shutdown();

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SamplingEvent::Retrying { .. })),
        "serde failures must not be retried"
    );
    match events.last().unwrap() {
        SamplingEvent::Failed { error, .. } => {
            assert_eq!(error.kind, SamplingErrorKind::Serialization);
            assert!(!error.is_retryable, "surfaced info must be non-retryable");
        }
        other => panic!("expected Failed(Serialization), got {other:?}"),
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1, "exactly one attempt");
}

// ---------------------------------------------------------------------------
// UpdateConfig invalidates cache + applies to subsequent requests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_config_changes_subsequent_request_model() {
    use std::sync::Mutex;

    let captured_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured_models);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = Arc::clone(&captured_handler);
            async move {
                let model = body
                    .get("model")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string();
                captured.lock().unwrap().push(model);
                let events = sse::chat_completion_events("ok", "test-model");
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let cfg = test_config(server.base_url(), "model-A");
    let handle = SamplerActor::spawn(cfg, RetryPolicy::default(), event_tx);

    let _ = handle
        .submit_and_collect(RequestId::from("req-1"), user_request("hi"))
        .await
        .expect("first req ok");

    let mut new_cfg = test_config(server.base_url(), "model-B");
    new_cfg.api_key = Some("test-key".into());
    handle.update_config(new_cfg);

    let _ = handle
        .submit_and_collect(RequestId::from("req-2"), user_request("hi"))
        .await
        .expect("second req ok");

    server.shutdown();

    let models = captured_models.lock().unwrap();
    assert_eq!(
        models.as_slice(),
        &["model-A".to_string(), "model-B".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Responses doom-loop check signals
// ---------------------------------------------------------------------------

fn responses_config(base_url: String, doom_loop: Option<DoomLoopRecoveryPolicy>) -> SamplerConfig {
    let mut cfg = test_config(base_url, "test-model");
    cfg.api_backend = ApiBackend::Responses;
    cfg.doom_loop_recovery = doom_loop;
    cfg
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_responses_wire_has_live_web_search_sources_and_never_x_search() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Vec<(bool, HeaderMap, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/responses",
        post(
            move |headers: HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = Arc::clone(&captured_handler);
                async move {
                    let streaming = body
                        .get("stream")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    captured.lock().unwrap().push((streaming, headers, body));

                    let events = sse::responses_api_reasoning_and_text_events(
                        "searched",
                        "answer",
                        "test-model",
                    );
                    if streaming {
                        Sse::new(stream::iter(
                            sse_events_to_axum(events)
                                .into_iter()
                                .map(Ok::<_, std::convert::Infallible>),
                        ))
                        .into_response()
                    } else {
                        let response = events
                            .into_iter()
                            .find_map(|event| {
                                let payload =
                                    serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
                                (payload.get("type").and_then(serde_json::Value::as_str)
                                    == Some("response.completed"))
                                .then(|| payload["response"].clone())
                            })
                            .expect("test fixture must include a completed response");
                        axum::Json(response).into_response()
                    }
                }
            },
        ),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let mut config = responses_config(server.base_url(), None);
    config.provider = ModelProvider::Codex;
    config.reasoning_summary = Some(ReasoningSummary::Auto);
    config.client_identifier = Some("must-not-leak".into());
    config.client_version = Some("must-not-leak".into());
    config.deployment_id = Some("must-not-leak".into());
    config.user_id = Some("must-not-leak".into());
    config.doom_loop_recovery = Some(DoomLoopRecoveryPolicy::default());
    config
        .extra_headers
        .insert("originator".into(), "codex_cli_rs".into());

    let codex_request = || {
        let mut request = ConversationRequest::from_items(vec![
            ConversationItem::system("Open Grok Codex base instructions"),
            ConversationItem::user("search the web"),
        ]);
        request.hosted_tools = vec![
            HostedTool::web_search(None),
            HostedTool::XSearch { options: None },
        ];
        request.x_grok_conv_id = Some("must-not-leak".into());
        request.x_grok_req_id = Some("must-not-leak".into());
        request.x_grok_session_id = Some("must-not-leak".into());
        request.x_grok_turn_idx = Some("must-not-leak".into());
        request.x_grok_agent_id = Some("must-not-leak".into());
        request
    };

    let handle = SamplerActor::spawn(config.clone(), RetryPolicy::default(), event_tx);

    handle
        .submit_and_collect(
            RequestId::from("req-codex-web-search-stream"),
            codex_request(),
        )
        .await
        .expect("streaming Codex hosted search request should complete");

    SamplingClient::new(config)
        .expect("Codex sampling client should construct")
        .conversation_responses(codex_request())
        .await
        .expect("non-streaming Codex hosted search request should complete");
    server.shutdown();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2, "expected one request per Responses path");
    assert_eq!(
        captured
            .iter()
            .map(|(streaming, _, _)| *streaming)
            .collect::<Vec<_>>(),
        vec![true, false],
        "must exercise both streaming and non-streaming Responses paths"
    );
    for (streaming, headers, body) in captured.iter() {
        let x_grok_headers = headers
            .keys()
            .filter(|name| name.as_str().starts_with("x-grok-"))
            .map(|name| name.as_str())
            .collect::<Vec<_>>();
        assert!(
            x_grok_headers.is_empty(),
            "Codex Responses request (streaming={streaming}) leaked x-grok headers: {x_grok_headers:?}"
        );
        assert_eq!(
            headers
                .get("originator")
                .and_then(|value| value.to_str().ok()),
            Some("codex_cli_rs"),
            "Codex provider headers must survive filtering"
        );
        assert_eq!(
            body["tools"],
            json!([{"type": "web_search", "external_web_access": true}])
        );
        assert_eq!(
            body["instructions"], "Open Grok Codex base instructions",
            "Codex base prompt must use top-level instructions: {body}"
        );
        assert!(
            body["input"]
                .as_array()
                .is_some_and(|input| input.iter().all(|item| {
                    item.get("role").and_then(serde_json::Value::as_str) != Some("system")
                })),
            "Codex Responses input must not contain system-role messages: {body}"
        );
        assert!(
            body["include"].as_array().is_some_and(|includes| includes
                .iter()
                .any(|include| { include.as_str() == Some("web_search_call.action.sources") })),
            "Codex web search must request source URLs: {body}"
        );
        assert!(
            body["include"].as_array().is_some_and(|includes| includes
                .iter()
                .any(|include| include.as_str() == Some("reasoning.encrypted_content"))),
            "Codex must request encrypted reasoning for lossless replay: {body}"
        );
        assert_eq!(
            body.pointer("/reasoning/summary")
                .and_then(serde_json::Value::as_str),
            Some("auto"),
            "Codex reasoning summaries must match codex-rs model defaults: {body}"
        );
        assert_eq!(
            body.get("prompt_cache_key")
                .and_then(serde_json::Value::as_str),
            Some("must-not-leak"),
            "Codex HTTP prompt caching must use the stable session ID: {body}"
        );
        assert!(
            body.get("previous_response_id")
                .is_none_or(serde_json::Value::is_null),
            "HTTP Responses sends full input; previous_response_id is WebSocket-only in codex-rs: {body}"
        );
        assert!(
            !body.to_string().contains("x_search"),
            "x_search must never be sent to Codex: {body}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xai_responses_projects_exec_to_function_and_rejects_other_custom_before_network() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Vec<(bool, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/responses",
        post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = Arc::clone(&captured_handler);
            async move {
                let streaming = body
                    .get("stream")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                captured.lock().unwrap().push((streaming, body));
                let events =
                    sse::responses_api_reasoning_and_text_events("projected", "done", "test-model");
                if streaming {
                    Sse::new(stream::iter(
                        sse_events_to_axum(events)
                            .into_iter()
                            .map(Ok::<_, std::convert::Infallible>),
                    ))
                    .into_response()
                } else {
                    let response = events
                        .into_iter()
                        .find_map(|event| {
                            let payload =
                                serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
                            (payload.get("type").and_then(serde_json::Value::as_str)
                                == Some("response.completed"))
                            .then(|| payload["response"].clone())
                        })
                        .expect("test fixture must include a completed response");
                    axum::Json(response).into_response()
                }
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let mut config = responses_config(server.base_url(), None);
    config.provider = ModelProvider::Xai;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(config.clone(), RetryPolicy::default(), event_tx);
    let client = SamplingClient::new(config).expect("xAI Responses client");

    let source = "const answer = 40 + 2;";
    let valid = ConversationRequest::from_items(vec![
        ConversationItem::assistant_tool_calls(vec![ToolCall::custom(
            "call-exec",
            "ctc-exec",
            "exec",
            source,
        )]),
        ConversationItem::custom_tool_output(
            CustomToolOutputItem::text("call-exec", "42").with_name("exec"),
        ),
        ConversationItem::user("continue"),
    ])
    .with_tools(vec![ToolSpec {
        name: "read_file".into(),
        description: Some("Read a file".into()),
        parameters: json!({"type": "object"}),
    }])
    .with_client_tools([ClientTool::Custom {
        name: "exec".into(),
        description: Some("Execute JavaScript".into()),
        format: xai_grok_sampling_types::rs::CustomToolParamFormat::Text,
    }]);

    handle
        .submit_and_collect(RequestId::from("xai-projected-exec-stream"), valid.clone())
        .await
        .expect("the actor should use the client's single projection pass");
    client
        .conversation_responses(valid)
        .await
        .expect("xAI exec should use the function envelope");

    let invalid = ConversationRequest::from_items(vec![ConversationItem::user("run")])
        .with_client_tools([ClientTool::Custom {
            name: "other_custom".into(),
            description: None,
            format: xai_grok_sampling_types::rs::CustomToolParamFormat::Text,
        }]);
    let error = handle
        .submit_and_collect(RequestId::from("xai-invalid-custom-stream"), invalid)
        .await
        .expect_err("non-exec xAI custom tool must fail before network");
    assert!(matches!(
        error,
        xai_grok_sampling_types::SamplingError::InvalidConfiguration(_)
    ));
    server.shutdown();

    let captured = captured.lock().unwrap();
    assert_eq!(
        captured.len(),
        2,
        "the rejected native custom request must never reach the server"
    );
    assert_eq!(
        captured
            .iter()
            .map(|(streaming, _)| *streaming)
            .collect::<Vec<_>>(),
        vec![true, false]
    );
    for (_, body) in captured.iter() {
        let serialized = body.to_string();
        assert!(
            !serialized.contains("\"type\":\"custom\"") && !serialized.contains("custom_tool_call"),
            "xAI request leaked a native custom wire shape: {body}"
        );
        let tools = body["tools"].as_array().expect("function tools");
        assert!(
            tools
                .iter()
                .any(|tool| { tool["type"] == "function" && tool["name"] == "read_file" })
        );
        assert!(
            tools
                .iter()
                .any(|tool| { tool["type"] == "function" && tool["name"] == "exec" })
        );
        let exec = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call" && item["name"] == "exec")
            .expect("projected exec call");
        assert_eq!(exec["call_id"], "call-exec");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(exec["arguments"].as_str().unwrap()).unwrap(),
            json!({"source": source})
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_backend_projects_native_exec_history_before_streaming_conversion() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |axum::Json(body): axum::Json<serde_json::Value>| {
            let captured = Arc::clone(&captured_handler);
            async move {
                captured.lock().unwrap().push(body);
                Sse::new(stream::iter(vec![
                    Ok::<_, std::convert::Infallible>(text_chunk_event("done", false)),
                    Ok::<_, std::convert::Infallible>(text_chunk_event("", true)),
                    Ok::<_, std::convert::Infallible>(Event::default().data("[DONE]")),
                ]))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let mut config = test_config(server.base_url(), "gpt-slug-does-not-select-provider");
    config.provider = ModelProvider::Kimi;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(config, RetryPolicy::default(), event_tx);

    handle
        .submit_and_collect(
            RequestId::from("chat-native-exec-history"),
            native_exec_history_request(),
        )
        .await
        .expect("native exec history should project before Chat conversion");

    let invalid = ConversationRequest::from_items(vec![
        ConversationItem::assistant_tool_calls(vec![ToolCall::custom(
            "call-code",
            "ctc-code",
            "code",
            "return 42",
        )]),
        ConversationItem::custom_tool_output(
            CustomToolOutputItem::text("call-code", "42").with_name("code"),
        ),
    ]);
    handle
        .submit_and_collect(RequestId::from("chat-invalid-custom"), invalid)
        .await
        .expect_err("non-exec native custom history must fail before network");
    server.shutdown();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1, "invalid custom history reached Chat API");
    let messages = captured[0]["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .find(|call| call["function"]["name"] == "exec")
        .expect("projected exec function call");
    assert_eq!(call["id"], "call-native-exec");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(call["function"]["arguments"].as_str().unwrap())
            .unwrap(),
        json!({"source": "const answer = 40 + 2;"})
    );
    let results = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_call_id"], "call-native-exec");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unary_chat_and_messages_project_native_exec_history_before_conversion() {
    use std::sync::Mutex;

    let chat_body: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let messages_body: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    let chat_handler = Arc::clone(&chat_body);
    let messages_handler = Arc::clone(&messages_body);
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = Arc::clone(&chat_handler);
                async move {
                    *captured.lock().unwrap() = Some(body);
                    axum::Json(json!({
                        "id": "chatcmpl-unary",
                        "object": "chat.completion",
                        "created": 0,
                        "model": "test-model",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "done"},
                            "finish_reason": "stop"
                        }]
                    }))
                }
            }),
        )
        .route(
            "/v1/messages",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = Arc::clone(&messages_handler);
                async move {
                    *captured.lock().unwrap() = Some(body);
                    axum::Json(json!({
                        "id": "msg-unary",
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "text", "text": "done"}],
                        "model": "test-model",
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1, "output_tokens": 1}
                    }))
                }
            }),
        );
    let server = MockServer::spawn(app).await;

    let mut chat_config = test_config(server.base_url(), "misleading-gpt-slug");
    chat_config.provider = ModelProvider::Kimi;
    SamplingClient::new(chat_config)
        .unwrap()
        .conversation(native_exec_history_request())
        .await
        .expect("unary Chat request");

    let mut messages_config = messages_config(server.base_url());
    messages_config.provider = ModelProvider::Xai;
    messages_config.model = "gpt-slug-does-not-select-provider".into();
    SamplingClient::new(messages_config)
        .unwrap()
        .conversation_messages(native_exec_history_request())
        .await
        .expect("unary Messages request");
    server.shutdown();

    let assert_projected = |body: &serde_json::Value, call_kind: &str, id_key: &str| {
        let blocks = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|message| {
                if call_kind == "tool_use" {
                    message["content"].as_array().into_iter().flatten()
                } else {
                    message["tool_calls"].as_array().into_iter().flatten()
                }
            })
            .collect::<Vec<_>>();
        let call = blocks
            .iter()
            .find(|call| {
                if call_kind == "tool_use" {
                    call["type"] == call_kind
                } else {
                    call["function"]["name"] == "exec"
                }
            })
            .expect("projected native exec call");
        assert_eq!(call[id_key], "call-native-exec");
    };
    assert_projected(
        chat_body.lock().unwrap().as_ref().unwrap(),
        "function",
        "id",
    );
    assert_projected(
        messages_body.lock().unwrap().as_ref().unwrap(),
        "tool_use",
        "id",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_compact_uses_unary_endpoint_auth_headers_and_exact_history() {
    use std::sync::Mutex;

    let captured: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);
    let replayed: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let replayed_handler = Arc::clone(&replayed);
    let compact_output = json!([
        {
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "retained user context"}]
        },
        {
            "type": "compaction",
            "encrypted_content": "opaque-server-summary"
        }
    ]);
    let response_output = compact_output.clone();
    let app = Router::new()
        .route(
            "/v1/responses/compact",
            post(
                move |headers: HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                    let captured = Arc::clone(&captured_handler);
                    let output = response_output.clone();
                    async move {
                        captured.lock().unwrap().push((headers, body));
                        axum::Json(json!({"output": output}))
                    }
                },
            ),
        )
        .route(
            "/v1/responses",
            post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let replayed = Arc::clone(&replayed_handler);
                async move {
                    replayed.lock().unwrap().push(body);
                    let response = sse::responses_api_reasoning_and_text_events(
                        "continued",
                        "answer",
                        "gpt-5.6-sol",
                    )
                    .into_iter()
                    .find_map(|event| {
                        let payload =
                            serde_json::from_str::<serde_json::Value>(&event.data).ok()?;
                        (payload.get("type").and_then(serde_json::Value::as_str)
                            == Some("response.completed"))
                        .then(|| payload["response"].clone())
                    })
                    .expect("fixture must contain a completed response");
                    axum::Json(response)
                }
            }),
        );
    let server = MockServer::spawn(app).await;
    let mut config = responses_config(server.base_url(), None);
    config.provider = ModelProvider::Codex;
    config.model = "gpt-5.6-sol".into();
    config.reasoning_summary = Some(ReasoningSummary::Auto);
    config
        .extra_headers
        .insert("originator".into(), "codex_cli_rs".into());
    config
        .extra_headers
        .insert("chatgpt-account-id".into(), "acct_test".into());
    config
        .extra_headers
        .insert("x-openai-fedramp".into(), "false".into());

    let client = SamplingClient::new(config).expect("Codex sampling client should construct");
    let mut compact_request = xai_function_exec_history_request();
    compact_request.x_grok_session_id = Some("session-cache-key".into());
    let replacement = client
        .compact_codex_conversation(compact_request, "base instructions")
        .await
        .expect("Codex compact request should complete");
    let mut replay_request = ConversationRequest::from_items(replacement);
    replay_request.x_grok_session_id = Some("session-cache-key".into());
    let replay = replay_request.raw_codex_input_replacements();
    client
        .conversation_responses(replay_request)
        .await
        .expect("the exact compacted history should be accepted on the next Codex turn");
    server.shutdown();

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1, "compact must be one unary request");
    let (headers, body) = &captured[0];
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-key")
    );
    assert_eq!(
        headers
            .get("chatgpt-account-id")
            .and_then(|value| value.to_str().ok()),
        Some("acct_test")
    );
    assert_eq!(
        headers
            .get("x-openai-fedramp")
            .and_then(|value| value.to_str().ok()),
        Some("false")
    );
    assert_eq!(
        headers
            .get("originator")
            .and_then(|value| value.to_str().ok()),
        Some("codex_cli_rs")
    );
    assert_eq!(body["model"], "gpt-5.6-sol");
    assert_eq!(body["instructions"], "base instructions");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["prompt_cache_key"], "session-cache-key");
    assert_eq!(body.pointer("/reasoning/summary"), Some(&json!("auto")));
    assert!(body["input"].is_array());
    let compact_input = body["input"].as_array().unwrap();
    assert_eq!(
        compact_input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .count(),
        1,
        "xAI function exec notifications must coalesce before Codex compaction: {body}"
    );
    assert!(
        compact_input
            .iter()
            .all(|item| item["type"] != "custom_tool_call_output")
    );
    for forbidden in [
        "store",
        "stream",
        "include",
        "temperature",
        "max_output_tokens",
    ] {
        assert!(
            body.get(forbidden).is_none(),
            "unexpected {forbidden}: {body}"
        );
    }

    assert_eq!(
        replay
            .into_iter()
            .map(|replacement| replacement.value)
            .collect::<Vec<_>>(),
        compact_output.as_array().unwrap().clone(),
        "the next Codex turn must receive the exact ordered replacement history"
    );
    let replayed = replayed.lock().unwrap();
    assert_eq!(replayed.len(), 1);
    assert_eq!(
        replayed[0]["input"], compact_output,
        "the normal Responses transport must splice the exact compact output, not typed placeholders"
    );
    assert_eq!(replayed[0]["prompt_cache_key"], "session-cache-key");
    assert!(
        replayed[0]
            .get("previous_response_id")
            .is_none_or(serde_json::Value::is_null),
        "HTTP continuation must replay full compacted input instead of a WebSocket response ID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_remote_compaction_v2_uses_responses_stream_contract() {
    use std::sync::{Mutex, OnceLock};

    const TURN_STATE: &str = "x-codex-turn-state";
    let captured: Arc<Mutex<Vec<(HeaderMap, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/responses",
        post(
            move |headers: HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                let captured = Arc::clone(&captured_handler);
                async move {
                    captured.lock().unwrap().push((headers, body));
                    let events = vec![
                        Event::default().event("response.metadata").data(
                            json!({
                                "type": "response.metadata",
                                "response_id": "resp_metadata",
                                "headers": {"X-CoDeX-TuRn-StAtE": "metadata-state"}
                            })
                            .to_string(),
                        ),
                        Event::default().event("response.output_item.done").data(
                            json!({
                                "type": "response.output_item.done",
                                "output_index": 0,
                                "item": {"type": "message", "id": "ignored-message"}
                            })
                            .to_string(),
                        ),
                        Event::default().event("response.output_item.done").data(
                            json!({
                                "type": "response.output_item.done",
                                "output_index": 1,
                                "item": {
                                    "type": "compaction",
                                    "encrypted_content": "opaque-v2-summary"
                                }
                            })
                            .to_string(),
                        ),
                        Event::default().event("response.completed").data(
                            json!({
                                "type": "response.completed",
                                "response": {
                                    "id": "resp_compact_v2",
                                    "usage": {
                                        "input_tokens": 321,
                                        "output_tokens": 9,
                                        "input_tokens_details": {"cached_tokens": 123},
                                        "output_tokens_details": {"reasoning_tokens": 7}
                                    }
                                }
                            })
                            .to_string(),
                        ),
                    ];
                    let mut response = Sse::new(stream::iter(
                        events.into_iter().map(Ok::<_, std::convert::Infallible>),
                    ))
                    .into_response();
                    response
                        .headers_mut()
                        .insert(TURN_STATE, "header-state".parse().unwrap());
                    response
                }
            },
        ),
    );
    let server = MockServer::spawn(app).await;
    let mut config = responses_config(server.base_url(), None);
    config.provider = ModelProvider::Codex;
    config.model = "gpt-5.6-sol".into();
    config.reasoning_effort = Some(xai_grok_sampling_types::ReasoningEffort::High);
    config.reasoning_summary = Some(ReasoningSummary::Detailed);
    config
        .extra_headers
        .insert("x-codex-beta-features".into(), "existing_feature".into());

    let turn_state = Arc::new(OnceLock::new());
    let client = SamplingClient::new_with_codex_turn_state(config, Arc::clone(&turn_state))
        .expect("Codex sampling client should construct");
    let mut request = xai_function_exec_history_request();
    request.x_grok_session_id = Some("session-cache-key".into());
    request.reasoning_effort = Some(xai_grok_sampling_types::ReasoningEffort::High);
    request.hosted_tools = vec![HostedTool::web_search(None)];
    request.json_schema = Some(json!({
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
        "additionalProperties": false
    }));
    let result = client
        .compact_codex_conversation_v2(request, "base instructions")
        .await
        .expect("remote compaction v2 should complete over /responses SSE");
    server.shutdown();

    assert_eq!(result.response_id, "resp_compact_v2");
    let usage = result.usage.expect("completion usage should be captured");
    assert_eq!(usage.input_tokens, 321);
    assert_eq!(usage.output_tokens, 9);
    assert_eq!(usage.total_tokens, 330);
    assert_eq!(usage.input_tokens_details.cached_tokens, 123);
    assert_eq!(usage.output_tokens_details.reasoning_tokens, 7);
    assert_eq!(turn_state.get().map(String::as_str), Some("header-state"));
    let replay = ConversationRequest::from_items(vec![result.compaction_item])
        .raw_codex_input_replacements();
    assert_eq!(replay[0].value["encrypted_content"], "opaque-v2-summary");
    assert!(
        replay[0].value.get("id").is_none(),
        "the typed empty-ID sentinel must never leak into replay input"
    );

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1, "v2 compaction must make one request");
    let (headers, body) = &captured[0];
    assert_eq!(
        headers
            .get("x-codex-beta-features")
            .and_then(|value| value.to_str().ok()),
        Some("existing_feature,remote_compaction_v2")
    );
    assert_eq!(body["model"], "gpt-5.6-sol");
    assert_eq!(body["instructions"], "base instructions");
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["parallel_tool_calls"], true);
    assert_eq!(body["prompt_cache_key"], "session-cache-key");
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert_eq!(body.pointer("/reasoning/effort"), Some(&json!("high")));
    assert_eq!(body.pointer("/reasoning/summary"), Some(&json!("detailed")));
    assert_eq!(
        body.pointer("/text/format/type"),
        Some(&json!("json_schema"))
    );
    assert!(body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool.get("type") == Some(&json!("web_search")))
    }));
    assert!(body["include"].as_array().is_some_and(|includes| {
        includes
            .iter()
            .any(|include| include == "reasoning.encrypted_content")
    }));
    let input = body["input"].as_array().expect("input must be an array");
    assert_eq!(
        input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .count(),
        1,
        "xAI function exec notifications must coalesce before remote v2 compaction: {body}"
    );
    assert!(
        input
            .iter()
            .all(|item| item["type"] != "custom_tool_call_output")
    );
    assert_eq!(
        input.last(),
        Some(&json!({"type": "compaction_trigger"})),
        "the compaction trigger must be the exact final input item"
    );
    assert_eq!(
        input
            .iter()
            .filter(|item| item.get("type") == Some(&json!("compaction_trigger")))
            .count(),
        1,
        "the request must contain exactly one compaction trigger"
    );
    assert!(
        body.get("previous_response_id")
            .is_none_or(serde_json::Value::is_null),
        "HTTP compaction v2 replays full input and must not chain response IDs"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_response_metadata_is_forward_compatible_and_replays_turn_state() {
    use std::sync::Mutex;

    const TURN_STATE: &str = "x-codex-turn-state";
    let request_number = Arc::new(AtomicU32::new(0));
    let request_number_handler = Arc::clone(&request_number);
    let captured: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);

    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap| {
            let request_number = Arc::clone(&request_number_handler);
            let captured = Arc::clone(&captured_handler);
            async move {
                captured.lock().unwrap().push(
                    headers
                        .get(TURN_STATE)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                );
                let attempt = request_number.fetch_add(1, Ordering::SeqCst);
                let mut events =
                    sse::responses_api_reasoning_and_text_events("routing", "ok", "gpt-5.6-sol");
                events.insert(
                    1,
                    SseEvent::data(
                        json!({
                            "type": "response.metadata",
                            "sequence_number": 1,
                            "response_id": format!("resp-metadata-{attempt}"),
                            "headers": {
                                "X-CoDeX-TuRn-StAtE": [if attempt == 0 {
                                    "metadata-state"
                                } else {
                                    "ignored-later-state"
                                }]
                            },
                            "metadata": {
                                "future_extension": {"accepted": true}
                            }
                        })
                        .to_string(),
                    ),
                );
                events.insert(
                    2,
                    SseEvent::data(
                        json!({
                            "type": "response.future_control",
                            "sequence_number": 2,
                            "response_id": format!("resp-metadata-{attempt}"),
                            "payload": {"forward_compatible": true}
                        })
                        .to_string(),
                    ),
                );
                Sse::new(stream::iter(
                    sse_events_to_axum(events)
                        .into_iter()
                        .map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let mut config = responses_config(server.base_url(), None);
    config.provider = ModelProvider::Codex;
    config.model = "gpt-5.6-sol".into();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(config, RetryPolicy::default(), event_tx);

    handle.begin_codex_turn();
    handle
        .submit_and_collect(RequestId::from("metadata-1"), user_request("first"))
        .await
        .expect("response.metadata must not fail typed SSE decoding");
    handle
        .submit_and_collect(
            RequestId::from("metadata-2"),
            user_request("same turn continuation"),
        )
        .await
        .expect("a continuation must replay metadata turn state");
    server.shutdown();

    assert_eq!(
        captured.lock().unwrap().as_slice(),
        &[None, Some("metadata-state".into())],
        "the first metadata turn-state value must be replayed exactly once per request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_turn_state_replays_across_responses_and_compact_then_resets() {
    use std::sync::Mutex;

    const TURN_STATE: &str = "x-codex-turn-state";
    let response_headers: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let response_headers_handler = Arc::clone(&response_headers);
    let compact_headers: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let compact_headers_handler = Arc::clone(&compact_headers);

    let app = Router::new()
        .route(
            "/v1/responses",
            post(
                move |headers: HeaderMap, axum::Json(body): axum::Json<serde_json::Value>| {
                    let response_headers = Arc::clone(&response_headers_handler);
                    async move {
                        response_headers.lock().unwrap().push(
                            headers
                                .get(TURN_STATE)
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned),
                        );
                        let events = sse::responses_api_reasoning_and_text_events(
                            "routing",
                            "ok",
                            "gpt-5.6-sol",
                        );
                        let mut response = if body
                            .get("stream")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                        {
                            Sse::new(stream::iter(
                                sse_events_to_axum(events)
                                    .into_iter()
                                    .map(Ok::<_, std::convert::Infallible>),
                            ))
                            .into_response()
                        } else {
                            let completed = events
                                .into_iter()
                                .find_map(|event| {
                                    let payload =
                                        serde_json::from_str::<serde_json::Value>(&event.data)
                                            .ok()?;
                                    (payload.get("type").and_then(serde_json::Value::as_str)
                                        == Some("response.completed"))
                                    .then(|| payload["response"].clone())
                                })
                                .expect("fixture must contain a completed response");
                            axum::Json(completed).into_response()
                        };
                        response
                            .headers_mut()
                            .insert(TURN_STATE, "response-state".parse().unwrap());
                        response
                    }
                },
            ),
        )
        .route(
            "/v1/responses/compact",
            post(move |headers: HeaderMap| {
                let compact_headers = Arc::clone(&compact_headers_handler);
                async move {
                    compact_headers.lock().unwrap().push(
                        headers
                            .get(TURN_STATE)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned),
                    );
                    let mut response = axum::Json(json!({
                        "output": [{
                            "type": "compaction",
                            "encrypted_content": "opaque-summary"
                        }]
                    }))
                    .into_response();
                    response
                        .headers_mut()
                        .insert(TURN_STATE, "compact-state".parse().unwrap());
                    response
                }
            }),
        );
    let server = MockServer::spawn(app).await;
    let mut config = responses_config(server.base_url(), None);
    config.provider = ModelProvider::Codex;
    config.model = "gpt-5.6-sol".into();
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(config.clone(), RetryPolicy::default(), event_tx);

    // A successful Responses SSE seeds the turn; compact and later Responses
    // calls replay that first value even when compact returns a different one.
    handle.begin_codex_turn();
    handle
        .submit_and_collect(RequestId::from("turn-a-1"), user_request("first"))
        .await
        .expect("first response should complete");
    SamplingClient::new_with_codex_turn_state(config.clone(), handle.codex_turn_state())
        .expect("turn-scoped Codex client should construct")
        .compact_codex_conversation(user_request("compact a"), "instructions")
        .await
        .expect("compact should complete");
    handle
        .submit_and_collect(RequestId::from("turn-a-2"), user_request("continue"))
        .await
        .expect("continuation should complete");

    // Compact can also be the first successful request and seed sampling.
    handle.begin_codex_turn();
    SamplingClient::new_with_codex_turn_state(config.clone(), handle.codex_turn_state())
        .expect("fresh turn-scoped Codex client should construct")
        .compact_codex_conversation(user_request("compact b"), "instructions")
        .await
        .expect("pre-turn compact should complete");
    handle
        .submit_and_collect(RequestId::from("turn-b-1"), user_request("after compact"))
        .await
        .expect("post-compact response should complete");

    // A new logical turn starts empty. Even if its state becomes populated,
    // attaching that cell to an xAI client must never put the Codex header on
    // the wire.
    handle.begin_codex_turn();
    handle
        .submit_and_collect(RequestId::from("turn-c-1"), user_request("new turn"))
        .await
        .expect("new-turn response should complete");
    let mut xai_config = config;
    xai_config.provider = ModelProvider::Xai;
    SamplingClient::new_with_codex_turn_state(xai_config, handle.codex_turn_state())
        .expect("xAI client should construct")
        .conversation_responses(user_request("provider isolation"))
        .await
        .expect("xAI Responses request should complete");
    server.shutdown();

    assert_eq!(
        compact_headers.lock().unwrap().as_slice(),
        &[Some("response-state".into()), None],
        "compact must inherit Responses state and may seed an empty turn"
    );
    assert_eq!(
        response_headers.lock().unwrap().as_slice(),
        &[
            None,
            Some("response-state".into()),
            Some("compact-state".into()),
            None,
            None,
        ],
        "state must persist only within one Codex turn and never reach xAI"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_turn_state_survives_http1_retry_client_rebuild() {
    use std::sync::Mutex;

    const TURN_STATE: &str = "x-codex-turn-state";
    let attempts = Arc::new(AtomicU32::new(0));
    let attempts_handler = Arc::clone(&attempts);
    let captured: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_handler = Arc::clone(&captured);
    let app = Router::new().route(
        "/v1/responses",
        post(move |headers: HeaderMap| {
            let attempts = Arc::clone(&attempts_handler);
            let captured = Arc::clone(&captured_handler);
            async move {
                captured.lock().unwrap().push(
                    headers
                        .get(TURN_STATE)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned),
                );
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 1 {
                    return (StatusCode::INTERNAL_SERVER_ERROR, "temporary failure")
                        .into_response();
                }
                let events = sse_events_to_axum(sse::responses_api_reasoning_and_text_events(
                    "routing",
                    "ok",
                    "gpt-5.6-sol",
                ));
                let mut response = Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
                .into_response();
                response
                    .headers_mut()
                    .insert(TURN_STATE, "response-state".parse().unwrap());
                response
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let mut config = responses_config(server.base_url(), None);
    config.provider = ModelProvider::Codex;
    config.model = "gpt-5.6-sol".into();
    config.max_retries = Some(2);
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(config, RetryPolicy::default(), event_tx);
    handle.begin_codex_turn();

    handle
        .submit_and_collect(RequestId::from("seed-state"), user_request("seed"))
        .await
        .expect("state-seeding response should complete");
    handle
        .submit_and_collect(RequestId::from("rebuild-client"), user_request("retry"))
        .await
        .expect("HTTP/1 retry should complete");
    server.shutdown();

    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        captured.lock().unwrap().as_slice(),
        &[
            None,
            Some("response-state".into()),
            Some("response-state".into()),
        ],
        "the rebuilt HTTP/1 client must retain the current turn's state"
    );
}

/// Server-reported doom-loop triggers flow through the actor rung onto the
/// completed response, without retries. The trigger is non-confident
/// (`@response` channel), so the recovery — which resamples only confident
/// signals — leaves it alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_doom_loop_signals_reach_completed_response() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                let events = sse_events_to_axum(sse::responses_api_doom_loop_terminal_only_events(
                    &["tail_repetition:4@response"],
                    "some thought",
                    "an answer",
                    "test-model",
                ));
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        responses_config(server.base_url(), Some(DoomLoopRecoveryPolicy::default())),
        RetryPolicy::default(),
        event_tx,
    );

    let result = handle
        .submit_and_collect(RequestId::from("req-doom-signal"), user_request("hi"))
        .await;
    server.shutdown();

    let (response, _metrics) = result.expect("a signalled turn still completes");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "warn-only: no resample");
    assert_eq!(response.doom_loop_signals.len(), 1);
    assert_eq!(
        response.doom_loop_signals[0].raw,
        "tail_repetition:4@response"
    );
    assert_eq!(response.assistant_text(), "an answer");
}

/// Acceptance spec for the recovery rung: a confident signal
/// (`tail_repetition:8@thinking` at the default threshold) is resampled once
/// and the clean second response is accepted, on its own budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_confident_doom_loop_signal_resamples_once() {
    let counter = Arc::new(AtomicU32::new(0));
    let counter_handler = Arc::clone(&counter);
    let app = Router::new().route(
        "/v1/responses",
        post(move || {
            let counter = Arc::clone(&counter_handler);
            async move {
                let attempt = counter.fetch_add(1, Ordering::SeqCst);
                let events = if attempt == 0 {
                    sse::responses_api_doom_loop_terminal_only_events(
                        &["tail_repetition:8@thinking"],
                        "loop loop loop",
                        "poisoned answer",
                        "test-model",
                    )
                } else {
                    sse::responses_api_reasoning_and_text_events(
                        "fresh thought",
                        "clean answer",
                        "test-model",
                    )
                };
                let events = sse_events_to_axum(events);
                Sse::new(stream::iter(
                    events.into_iter().map(Ok::<_, std::convert::Infallible>),
                ))
            }
        }),
    );
    let server = MockServer::spawn(app).await;
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let handle = SamplerActor::spawn(
        responses_config(server.base_url(), Some(DoomLoopRecoveryPolicy::default())),
        RetryPolicy::default(),
        event_tx,
    );

    let result = handle
        .submit_and_collect(RequestId::from("req-doom-resample"), user_request("hi"))
        .await;
    server.shutdown();

    let (response, _metrics) = result.expect("recovery accepts the clean resample");
    assert_eq!(counter.load(Ordering::SeqCst), 2, "exactly one resample");
    assert_eq!(response.assistant_text(), "clean answer");
    assert!(
        response.doom_loop_signals.is_empty(),
        "the accepted response is the clean resample"
    );
}

// ---------------------------------------------------------------------------
// Helpers for draining the event channel
// ---------------------------------------------------------------------------

/// Drain the event channel until a terminal event (`Completed` or
/// `Failed`) is received, or until `deadline` elapses.
async fn drain_until_terminal(
    rx: &mut mpsc::UnboundedReceiver<SamplingEvent>,
    timeout: Duration,
) -> Vec<SamplingEvent> {
    let mut out = Vec::new();
    let start = tokio::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            panic!(
                "drain_until_terminal timed out after {:?}; got {} events",
                timeout,
                out.len()
            );
        }
        let remaining = timeout - elapsed;
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let terminal = matches!(
                    ev,
                    SamplingEvent::Completed { .. } | SamplingEvent::Failed { .. }
                );
                out.push(ev);
                if terminal {
                    return out;
                }
            }
            Ok(None) => panic!("event channel closed before terminal event"),
            Err(_) => panic!(
                "drain_until_terminal timed out after {:?}; got {} events",
                timeout,
                out.len()
            ),
        }
    }
}

/// Wait for the next event matching `pred`, or return `None` on
/// timeout.
async fn await_event_matching(
    rx: &mut mpsc::UnboundedReceiver<SamplingEvent>,
    mut pred: impl FnMut(&SamplingEvent) -> bool,
    timeout: Duration,
) -> Option<SamplingEvent> {
    let start = tokio::time::Instant::now();
    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return None;
        }
        let remaining = timeout - elapsed;
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                if pred(&ev) {
                    return Some(ev);
                }
            }
            Ok(None) => return None,
            Err(_) => return None,
        }
    }
}
