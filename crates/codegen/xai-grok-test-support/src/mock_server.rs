//! Mock inference server with request logging and automatic cleanup.
//!
//! Serves `/v1/chat/completions`, `/v1/responses`, and `/v1/messages` in one
//! of two response modes: echo (default — streams `Echo: <last user message>`)
//! or a fixed text set via [`MockInferenceServer::set_response`] (streamed
//! with byte-exact reconstruction). A per-path FIFO of [`ScriptedResponse`]s
//! (see [`MockInferenceServer::enqueue_response`]) overrides the mode for
//! exact status/body/SSE control. `/v1/models` and `/v1/settings` return
//! configurable responses (settings is 404 until set). All requests are
//! logged — bodies and headers — for assertion in tests.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context as _;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub use crate::scripted::{ScriptedBody, ScriptedResponse, SseEvent};
use crate::sse;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub method: String,
    pub path: String,
    pub body: Option<Value>,
    /// Value of the `Authorization` header, if present.
    pub authorization: Option<String>,
    /// Request headers (lowercase names, arrival order), captured on the
    /// inference POST endpoints; the GET endpoints log an empty list.
    pub headers: Vec<(String, String)>,
}

impl LogEntry {
    /// First value of `name` (case-insensitive), if the request carried it.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }
}

pub struct RequestLog {
    count: AtomicU32,
    entries: std::sync::Mutex<Vec<LogEntry>>,
}

impl RequestLog {
    fn new() -> Self {
        Self {
            count: AtomicU32::new(0),
            entries: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn record(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        authorization: Option<&str>,
        headers: Vec<(String, String)>,
    ) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.entries.lock().unwrap().push(LogEntry {
            method: method.to_string(),
            path: path.to_string(),
            body: body.cloned(),
            authorization: authorization.map(String::from),
            headers,
        });
    }
}

type ScriptQueues = Arc<std::sync::Mutex<HashMap<String, VecDeque<ScriptedResponse>>>>;

/// A model entry for the mock `/v1/models` endpoint.
#[derive(Debug, Clone)]
pub struct MockModelEntry {
    /// Model ID (e.g. `"test-model"`).
    pub id: String,
    /// Optional agent type (e.g. `"cursor"`).
    /// Emitted as `agentType` inside `_meta` when set.
    pub agent_type: Option<String>,
    /// Optional API backend (e.g. `"messages"`). Emitted as `apiBackend`
    /// when set; absent means the shell's default backend.
    pub api_backend: Option<String>,
    /// Emitted as `supportsBackendSearch` when true.
    pub supports_backend_search: bool,
    /// Emitted as `supportsReasoningEffort` (top-level) when true.
    pub supports_reasoning_effort: bool,
    /// Emitted as `reasoningEffort` (top-level) when set.
    pub reasoning_effort: Option<String>,
    /// Emitted as `reasoningEfforts` (top-level) when non-empty. Each entry is a
    /// raw JSON option (a table `{ "value": ..., "id"?, "label"?, ... }` or a
    /// bare value string), matching what `parse_remote_model_value` reads.
    pub reasoning_efforts: Vec<Value>,
}

impl MockModelEntry {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            agent_type: None,
            api_backend: None,
            supports_backend_search: false,
            supports_reasoning_effort: false,
            reasoning_effort: None,
            reasoning_efforts: Vec::new(),
        }
    }

    pub fn with_agent_type(id: impl Into<String>, agent_type: impl Into<String>) -> Self {
        Self {
            agent_type: Some(agent_type.into()),
            ..Self::new(id)
        }
    }

    pub fn with_api_backend(mut self, api_backend: impl Into<String>) -> Self {
        self.api_backend = Some(api_backend.into());
        self
    }

    pub fn with_supports_backend_search(mut self, supports: bool) -> Self {
        self.supports_backend_search = supports;
        self
    }

    pub fn with_supports_reasoning_effort(mut self, supports: bool) -> Self {
        self.supports_reasoning_effort = supports;
        self
    }

    pub fn with_reasoning_effort(mut self, effort: impl Into<String>) -> Self {
        self.reasoning_effort = Some(effort.into());
        self
    }

    pub fn with_reasoning_efforts(mut self, efforts: Vec<Value>) -> Self {
        self.reasoning_efforts = efforts;
        self
    }

    fn to_json(&self) -> Value {
        let mut obj = json!({
            "id": self.id,
            "object": "model",
            "created": 1234567890,
            "owned_by": "test"
        });
        if let Some(ref at) = self.agent_type {
            obj["_meta"] = json!({ "agentType": at });
        }
        if let Some(ref backend) = self.api_backend {
            obj["apiBackend"] = json!(backend);
        }
        if self.supports_backend_search {
            obj["supportsBackendSearch"] = json!(true);
        }
        if self.supports_reasoning_effort {
            obj["supportsReasoningEffort"] = json!(true);
        }
        if let Some(ref effort) = self.reasoning_effort {
            obj["reasoningEffort"] = json!(effort);
        }
        if !self.reasoning_efforts.is_empty() {
            obj["reasoningEfforts"] = json!(self.reasoning_efforts);
        }
        obj
    }
}

/// What the inference endpoints stream back.
enum ResponseMode {
    /// Echo the last user message as `Echo: <msg>` (whitespace-collapsing).
    Echo,
    /// Stream a fixed text whose deltas reconstruct it byte-for-byte
    /// (newlines preserved — required for fenced code blocks).
    Fixed(String),
}

/// Opt-in barrier that holds an **agent turn's terminal SSE event** until the
/// test releases it, so the turn stays deterministically "running" while the
/// test interacts with it (queue edits/removals) — eliminating turn-end races.
///
/// Inert by default (`held == false`): [`wait_if_held`] returns immediately, so
/// every test that never calls [`MockInferenceServer::hold_agent_completions`]
/// is completely unaffected.
///
/// [`wait_if_held`]: CompletionGate::wait_if_held
#[derive(Default)]
struct CompletionGate {
    held: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CompletionGate {
    fn hold(&self) {
        self.held.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.held.store(false, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Block while the gate is held. Registers the wake-up interest *before*
    /// re-checking `held` so a concurrent `release` can never be missed.
    async fn wait_if_held(&self) {
        loop {
            let notified = self.notify.notified();
            if !self.held.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// Wrap SSE `events` in a stream that emits each one after `delay`. `None`
/// keeps instant emission (the default fast path); `Some(d)` paces the stream
/// so tests can interact with a turn while it is visibly in flight.
///
/// When `gate` is `Some`, the stream additionally blocks on the gate right
/// before emitting the **final** event (the SSE terminator), so a held gate
/// keeps the turn streaming-but-not-complete until released.
fn paced_events(
    events: Vec<axum::response::sse::Event>,
    delay: Option<Duration>,
    gate: Option<Arc<CompletionGate>>,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, Infallible>> {
    use futures_util::StreamExt as _;
    let last_idx = events.len().saturating_sub(1);
    stream::iter(events.into_iter().enumerate()).then(move |(idx, event)| {
        let gate = gate.clone();
        async move {
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            // Hold the terminal event until the gate is released.
            if idx == last_idx
                && let Some(gate) = gate.as_deref()
            {
                gate.wait_if_held().await;
            }
            Ok::<_, Infallible>(event)
        }
    })
}

/// Max body bytes retained on each accepted [`StorageUpload`] (keeps large
/// e2e artifacts from ballooning test memory; meta/small dumps stay intact).
const STORAGE_BODY_CAPTURE_CAP: usize = 256 * 1024;

/// One accepted (HTTP 200) mock `/v1/storage` upload.
#[derive(Debug, Clone)]
pub struct StorageUpload {
    pub path: String,
    pub size: usize,
    /// Request body when `size <= 256 KiB`; empty for larger payloads.
    pub body: Vec<u8>,
    /// `Authorization` header value as sent (e.g. `Bearer …`).
    pub authorization: Option<String>,
}

/// Mock `/v1/storage` state: a flippable 401 gate plus a record of accepted
/// uploads, so e2e tests can simulate an auth outage window and assert the
/// trace upload queue parks, then drains after the gate heals.
#[derive(Default)]
struct StorageState {
    unauthorized: AtomicBool,
    request_count: AtomicU32,
    uploads: std::sync::Mutex<Vec<StorageUpload>>,
}

/// Mock `/v1/chat/completions` + `/v1/responses` + `/v1/messages` +
/// `/v1/models` + `/v1/settings` + `/v1/storage` server.
/// Logs all requests. Shuts down on drop.
pub struct MockInferenceServer {
    addr: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    log: Arc<RequestLog>,
    models: Arc<std::sync::RwLock<Vec<Value>>>,
    settings: Arc<std::sync::RwLock<Option<Value>>>,
    response_mode: Arc<std::sync::RwLock<ResponseMode>>,
    scripted: ScriptQueues,
    /// Per-agent-turn assistant texts (see [`set_agent_turns`]).
    ///
    /// [`set_agent_turns`]: Self::set_agent_turns
    agent_turns: Arc<std::sync::Mutex<VecDeque<String>>>,
    /// `stop_reason` emitted by the `/v1/messages` terminal `message_delta`.
    messages_stop_reason: Arc<std::sync::RwLock<String>>,
    /// Optional per-SSE-event delay on all inference endpoints. `None`
    /// (default) streams instantly; `Some(d)` holds the turn "streaming" long
    /// enough for tests to interact with it mid-flight (e.g. Esc-cancel).
    chunk_delay: Arc<std::sync::RwLock<Option<Duration>>>,
    /// Mock `/v1/storage` 401 gate + accepted-upload record.
    storage: Arc<StorageState>,
    /// Opt-in barrier holding agent turns' terminal event (see
    /// [`Self::hold_agent_completions`]). Inert until a test holds it.
    completion_gate: Arc<CompletionGate>,
    /// See [`Self::set_user_subscription_tier`].
    user_tier: Arc<std::sync::RwLock<Option<String>>>,
}

impl MockInferenceServer {
    /// Start with a single default `test-model` (no agent_type).
    pub async fn start() -> anyhow::Result<Self> {
        Self::start_with_models(vec![MockModelEntry::new("test-model")]).await
    }

    /// Start with custom models. Use [`MockModelEntry::with_agent_type`] to
    /// configure models with specific harness types for agent-type tests.
    pub async fn start_with_models(models: Vec<MockModelEntry>) -> anyhow::Result<Self> {
        Self::start_inner(models, None).await
    }

    /// Start a mock that returns 401 on inference requests missing
    /// `Authorization: Bearer <required_token>`.
    pub async fn start_with_required_auth(
        models: Vec<MockModelEntry>,
        required_token: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Self::start_inner(models, Some(required_token.into())).await
    }

    async fn start_inner(
        models: Vec<MockModelEntry>,
        required_token: Option<String>,
    ) -> anyhow::Result<Self> {
        let log = Arc::new(RequestLog::new());
        let models_json: Vec<Value> = models.iter().map(MockModelEntry::to_json).collect();
        let shared_models = Arc::new(std::sync::RwLock::new(models_json));
        let shared_settings = Arc::new(std::sync::RwLock::new(None::<Value>));
        let response_mode = Arc::new(std::sync::RwLock::new(ResponseMode::Echo));
        let scripted: ScriptQueues = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let agent_turns = Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let messages_stop_reason = Arc::new(std::sync::RwLock::new("end_turn".to_string()));
        let chunk_delay = Arc::new(std::sync::RwLock::new(None::<Duration>));
        let storage = Arc::new(StorageState::default());
        let completion_gate = Arc::new(CompletionGate::default());
        let user_tier = Arc::new(std::sync::RwLock::new(None::<String>));
        let app = Self::build_router(
            log.clone(),
            shared_models.clone(),
            shared_settings.clone(),
            response_mode.clone(),
            scripted.clone(),
            agent_turns.clone(),
            messages_stop_reason.clone(),
            chunk_delay.clone(),
            storage.clone(),
            completion_gate.clone(),
            user_tier.clone(),
            required_token,
        );

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind mock inference server")?;
        let addr = listener.local_addr().context("local_addr")?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        // Wait for server readiness — try connecting instead of a fixed sleep.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::net::TcpStream::connect(addr).await.is_err() {
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("mock server not ready within 5s");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        Ok(Self {
            addr,
            shutdown_tx: Some(shutdown_tx),
            log,
            models: shared_models,
            settings: shared_settings,
            response_mode,
            scripted,
            agent_turns,
            messages_stop_reason,
            chunk_delay,
            storage,
            completion_gate,
            user_tier,
        })
    }

    /// Replace the model list at runtime. The next `/v1/models` request
    /// (e.g. during session resume) will return the new list.
    pub fn set_models(&self, models: Vec<MockModelEntry>) {
        let mut guard = self.models.write().unwrap();
        *guard = models.iter().map(MockModelEntry::to_json).collect();
    }

    /// Stream this fixed text from all inference endpoints instead of echoing
    /// the user message. Deltas reconstruct the text byte-for-byte (newlines
    /// preserved). Subsequent calls replace the text.
    pub fn set_response(&self, text: impl Into<String>) {
        *self.response_mode.write().unwrap() = ResponseMode::Fixed(text.into());
    }

    /// Queue a [`ScriptedResponse`] for the next request on `path` (e.g.
    /// `"/v1/chat/completions"`). Scripts are consumed FIFO per path by the
    /// three inference endpoints; when a path's queue is empty, requests fall
    /// back to the active response mode (echo/fixed).
    pub fn enqueue_response(&self, path: impl Into<String>, response: ScriptedResponse) {
        // Fail at the call site, not at serve time.
        response.validate();
        self.scripted
            .lock()
            .unwrap()
            .entry(path.into())
            .or_default()
            .push_back(response);
    }

    /// Queue one byte-exact response per agent turn, consumed FIFO. Only
    /// requests carrying 2+ tools count as agent turns, so aux requests
    /// (title/classifier) never steal a turn; an empty queue falls back to
    /// the active response mode.
    pub fn set_agent_turns(&self, turns: impl IntoIterator<Item = String>) {
        *self.agent_turns.lock().unwrap() = turns.into_iter().collect();
    }

    /// Replace the settings at runtime. The next `GET /v1/settings` request
    /// will return the new value as JSON. Until set, `/v1/settings` returns 404.
    pub fn set_settings(&self, settings: impl serde::Serialize) {
        let value = serde_json::to_value(settings).expect("serialize settings");
        let mut guard = self.settings.write().unwrap();
        *guard = Some(value);
    }

    /// Preset `/v1/settings` to the minimal `{"allow_access": true}` payload
    /// that opens the subscription gate (clients treat a missing field as
    /// `false` and would sit on the upsell screen).
    pub fn preset_allow_access(&self) {
        self.set_settings(json!({ "allow_access": true }));
    }

    /// Set the `subscriptionTier` served by `GET /v1/user`. `None`
    /// (default) omits the field, which the shell treats as "no qualifying
    /// subscription" (free tier).
    pub fn set_user_subscription_tier(&self, tier: Option<&str>) {
        *self.user_tier.write().unwrap() = tier.map(str::to_owned);
    }

    /// Set the `stop_reason` emitted by the `/v1/messages` terminal
    /// `message_delta` (default `"end_turn"`).
    pub fn set_messages_stop_reason(&self, stop_reason: impl Into<String>) {
        *self.messages_stop_reason.write().unwrap() = stop_reason.into();
    }

    /// Pace all inference SSE streams: each event is emitted after `delay`.
    /// `None` (default) restores instant streaming. Lets PTY e2e tests hold a
    /// turn visibly "streaming" long enough to interact with it mid-flight
    /// (e.g. Esc-cancel). Applies to requests started after the call.
    pub fn set_chunk_delay(&self, delay: Option<Duration>) {
        *self.chunk_delay.write().unwrap() = delay;
    }

    /// Hold every agent turn's terminal SSE event until
    /// [`release_agent_completions`] is called, keeping the turn
    /// deterministically "streaming-but-not-complete". Lets a test interact
    /// with a running turn (e.g. queue edits/removals) without racing turn
    /// end. Content deltas still stream normally; only completion is gated.
    /// Inert for tests that never call this.
    ///
    /// [`release_agent_completions`]: Self::release_agent_completions
    pub fn hold_agent_completions(&self) {
        self.completion_gate.hold();
    }

    /// Release a hold set by [`hold_agent_completions`], letting held (and
    /// future) agent turns emit their terminal event and complete.
    ///
    /// [`hold_agent_completions`]: Self::hold_agent_completions
    pub fn release_agent_completions(&self) {
        self.completion_gate.release();
    }

    /// e.g. `http://127.0.0.1:12345/v1`
    pub fn url(&self) -> String {
        format!("http://{}/v1", self.addr)
    }

    pub fn request_count(&self) -> u32 {
        self.log.count.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<LogEntry> {
        self.log.entries.lock().unwrap().clone()
    }

    /// Bodies of all received requests, in arrival order (body-less requests
    /// such as `GET /v1/models` are skipped).
    pub fn request_bodies(&self) -> Vec<Value> {
        self.log
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e.body.clone())
            .collect()
    }

    pub fn has_chat_completion_request(&self) -> bool {
        self.log
            .entries
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.path.contains("chat/completions"))
    }

    pub fn has_responses_request(&self) -> bool {
        self.log
            .entries
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.path.contains("responses"))
    }

    /// Number of `POST /v1/messages` requests received so far.
    pub fn messages_request_count(&self) -> usize {
        self.log
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.path == "/v1/messages")
            .count()
    }

    /// Format the request log for diagnostic output on test failures.
    pub fn request_log_summary(&self) -> String {
        let entries = self.log.entries.lock().unwrap();
        if entries.is_empty() {
            return "(no requests received)".to_string();
        }
        entries
            .iter()
            .enumerate()
            .map(|(i, e)| format!("  [{}] {} {}", i, e.method, e.path))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Get the system prompt from the most recent inference request.
    pub fn last_system_prompt(&self) -> Option<String> {
        let entries = self.log.entries.lock().unwrap();
        entries
            .iter()
            .rev()
            .find(|e| e.path.contains("chat/completions") || e.path.contains("responses"))
            .and_then(|e| e.body.as_ref())
            .and_then(|body| {
                // Chat completions format: messages[0].content (system message)
                body.get("messages")
                    .and_then(|m| m.as_array())
                    .and_then(|msgs| msgs.first())
                    .and_then(|msg| msg.get("content"))
                    .and_then(|c| c.as_str())
                    .map(String::from)
                    // Responses API format: instructions field
                    .or_else(|| {
                        body.get("instructions")
                            .and_then(|s| s.as_str())
                            .map(String::from)
                    })
            })
    }

    /// Flip the mock `/v1/storage` 401 gate. While `true`, every upload is
    /// rejected with 401 (the auth-outage window the park-on-401 e2e drives).
    pub fn set_storage_unauthorized(&self, unauthorized: bool) {
        self.storage
            .unauthorized
            .store(unauthorized, Ordering::SeqCst);
    }

    /// Total `/v1/storage` upload attempts seen, including 401-rejected ones.
    pub fn storage_request_count(&self) -> u32 {
        self.storage.request_count.load(Ordering::SeqCst)
    }

    /// Snapshot of accepted (HTTP 200) `/v1/storage` uploads.
    pub fn storage_uploads(&self) -> Vec<StorageUpload> {
        self.storage.uploads.lock().unwrap().clone()
    }

    /// Mock `/v1/storage` upload: count the attempt, reject with 401 while the
    /// gate is closed, else record the upload and mirror the proxy's
    /// `UploadResponse` JSON shape.
    fn storage_upload_handler(
        storage: &StorageState,
        headers: &HeaderMap,
        body: &axum::body::Bytes,
    ) -> Response {
        storage.request_count.fetch_add(1, Ordering::SeqCst);
        if storage.unauthorized.load(Ordering::SeqCst) {
            return (
                StatusCode::UNAUTHORIZED,
                r#"{"error":"Invalid or expired credentials (mock)"}"#,
            )
                .into_response();
        }

        let path = headers
            .get("X-Storage-Path")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let size = body.len();
        let captured_body = if size <= STORAGE_BODY_CAPTURE_CAP {
            body.to_vec()
        } else {
            Vec::new()
        };
        let authorization = Self::extract_auth(headers);
        let response = (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "bucket": "mock-bucket",
                "path": path,
                "size": size,
                "content_type": "application/octet-stream",
                "generation": 1,
            })
            .to_string(),
        );
        storage.uploads.lock().unwrap().push(StorageUpload {
            path,
            size,
            body: captured_body,
            authorization,
        });
        response.into_response()
    }

    fn extract_auth(headers: &HeaderMap) -> Option<String> {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
    }

    fn headers_vec(headers: &HeaderMap) -> Vec<(String, String)> {
        headers
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect()
    }

    fn pop_scripted(scripted: &ScriptQueues, path: &str) -> Option<ScriptedResponse> {
        scripted
            .lock()
            .unwrap()
            .get_mut(path)
            .and_then(VecDeque::pop_front)
    }

    /// Pop the next scripted turn, gated to agent turns (2+ tools) so aux
    /// requests don't consume one.
    fn pop_agent_turn(
        agent_turns: &Arc<std::sync::Mutex<VecDeque<String>>>,
        body: &Value,
    ) -> Option<String> {
        let tool_count = body
            .get("tools")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if tool_count < 2 {
            return None;
        }
        agent_turns.lock().unwrap().pop_front()
    }

    /// Returns `Some(401)` if auth is required and the Bearer token doesn't match.
    fn check_auth(auth: Option<&str>, required_token: Option<&str>) -> Option<Response> {
        let expected = required_token?;
        let valid = auth.is_some_and(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
                .is_some_and(|token| token == expected)
        });
        if valid {
            return None;
        }
        Some((
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "missing API key; set the x-api-key header or Authorization: Bearer header"
            })),
        ).into_response())
    }

    fn build_router(
        log: Arc<RequestLog>,
        models: Arc<std::sync::RwLock<Vec<Value>>>,
        settings: Arc<std::sync::RwLock<Option<Value>>>,
        response_mode: Arc<std::sync::RwLock<ResponseMode>>,
        scripted: ScriptQueues,
        agent_turns: Arc<std::sync::Mutex<VecDeque<String>>>,
        messages_stop_reason: Arc<std::sync::RwLock<String>>,
        chunk_delay: Arc<std::sync::RwLock<Option<Duration>>>,
        storage: Arc<StorageState>,
        completion_gate: Arc<CompletionGate>,
        user_tier: Arc<std::sync::RwLock<Option<String>>>,
        required_token: Option<String>,
    ) -> Router {
        let log_cc = log.clone();
        let log_rs = log.clone();
        let log_msg = log.clone();
        let token_cc = required_token.clone();
        let token_msg = required_token.clone();
        let token_rs = required_token;
        let mode_cc = response_mode.clone();
        let mode_rs = response_mode.clone();
        let mode_msg = response_mode;
        let scripted_cc = scripted.clone();
        let scripted_rs = scripted.clone();
        let scripted_settings = scripted.clone();
        let scripted_msg = scripted;
        let delay_cc = chunk_delay.clone();
        let delay_rs = chunk_delay.clone();
        let delay_msg = chunk_delay;

        Router::new()
            .route(
                "/v1/chat/completions",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let log = log_cc.clone();
                    let required = token_cc.clone();
                    let mode = mode_cc.clone();
                    let scripted = scripted_cc.clone();
                    let agent_turns = agent_turns.clone();
                    let delay = delay_cc.clone();
                    let completion_gate = completion_gate.clone();
                    async move {
                        let auth = Self::extract_auth(&headers);
                        log.record(
                            "POST",
                            "/v1/chat/completions",
                            Some(&body),
                            auth.as_deref(),
                            Self::headers_vec(&headers),
                        );

                        if let Some(s) = Self::pop_scripted(&scripted, "/v1/chat/completions") {
                            return s.into_response_paced(*delay.read().unwrap());
                        }

                        if let Some(rejection) =
                            Self::check_auth(auth.as_deref(), required.as_deref())
                        {
                            return rejection;
                        }

                        let user_msg = body
                            .get("messages")
                            .and_then(|m| m.as_array())
                            .and_then(|msgs| {
                                msgs.iter()
                                    .rev()
                                    .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                            })
                            .and_then(|m| m.get("content"))
                            .and_then(Value::as_str)
                            .unwrap_or("hello");

                        let model = body
                            .get("model")
                            .and_then(Value::as_str)
                            .unwrap_or("test-model");

                        // Only agent turns are gate-eligible: aux requests
                        // (title/classifier) must never block session startup.
                        let (events, gate) = match Self::pop_agent_turn(&agent_turns, &body) {
                            Some(text) => (
                                sse::chat_completion_events_exact(&text, model),
                                Some(completion_gate.clone()),
                            ),
                            None => {
                                let events = match &*mode.read().unwrap() {
                                    ResponseMode::Echo => sse::chat_completion_events(
                                        &format!("Echo: {user_msg}"),
                                        model,
                                    ),
                                    ResponseMode::Fixed(text) => {
                                        sse::chat_completion_events_exact(text, model)
                                    }
                                };
                                (events, None)
                            }
                        };
                        let stream = paced_events(events, *delay.read().unwrap(), gate);
                        Sse::new(stream)
                            .keep_alive(KeepAlive::default())
                            .into_response()
                    }
                }),
            )
            .route(
                "/v1/responses",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let log = log_rs.clone();
                    let required = token_rs.clone();
                    let mode = mode_rs.clone();
                    let scripted = scripted_rs.clone();
                    let delay = delay_rs.clone();
                    async move {
                        let auth = Self::extract_auth(&headers);
                        log.record(
                            "POST",
                            "/v1/responses",
                            Some(&body),
                            auth.as_deref(),
                            Self::headers_vec(&headers),
                        );

                        if let Some(s) = Self::pop_scripted(&scripted, "/v1/responses") {
                            return s.into_response_paced(*delay.read().unwrap());
                        }

                        if let Some(rejection) =
                            Self::check_auth(auth.as_deref(), required.as_deref())
                        {
                            return rejection;
                        }

                        let user_msg = body
                            .get("input")
                            .and_then(|i| i.as_array())
                            .and_then(|items| {
                                items.iter().rev().find(|item| {
                                    item.get("role").and_then(Value::as_str) == Some("user")
                                })
                            })
                            .and_then(|item| {
                                item.get("content").and_then(|c| {
                                    c.as_str().map(String::from).or_else(|| {
                                        c.as_array().and_then(|parts| {
                                            parts.iter().find_map(|p| {
                                                if p.get("type").and_then(Value::as_str)
                                                    == Some("input_text")
                                                {
                                                    p.get("text")
                                                        .and_then(Value::as_str)
                                                        .map(String::from)
                                                } else {
                                                    None
                                                }
                                            })
                                        })
                                    })
                                })
                            })
                            .unwrap_or_else(|| "hello".to_string());

                        let model = body
                            .get("model")
                            .and_then(Value::as_str)
                            .unwrap_or("test-model");

                        let events = match &*mode.read().unwrap() {
                            ResponseMode::Echo => {
                                sse::responses_api_events(&format!("Echo: {user_msg}"), model)
                            }
                            ResponseMode::Fixed(text) => {
                                sse::responses_api_events_exact(text, model)
                            }
                        };
                        let stream = paced_events(events, *delay.read().unwrap(), None);
                        Sse::new(stream)
                            .keep_alive(KeepAlive::default())
                            .into_response()
                    }
                }),
            )
            .route(
                "/v1/messages",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let log = log_msg.clone();
                    let required = token_msg.clone();
                    let mode = mode_msg.clone();
                    let scripted = scripted_msg.clone();
                    let stop_reason = messages_stop_reason.clone();
                    let delay = delay_msg.clone();
                    async move {
                        let auth = Self::extract_auth(&headers);
                        log.record(
                            "POST",
                            "/v1/messages",
                            Some(&body),
                            auth.as_deref(),
                            Self::headers_vec(&headers),
                        );

                        if let Some(s) = Self::pop_scripted(&scripted, "/v1/messages") {
                            return s.into_response_paced(*delay.read().unwrap());
                        }

                        if let Some(rejection) =
                            Self::check_auth(auth.as_deref(), required.as_deref())
                        {
                            return rejection;
                        }

                        // Anthropic content is either a plain string or an
                        // array of typed blocks; extract the last user text.
                        let user_msg = body
                            .get("messages")
                            .and_then(|m| m.as_array())
                            .and_then(|msgs| {
                                msgs.iter()
                                    .rev()
                                    .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
                            })
                            .and_then(|m| m.get("content"))
                            .and_then(|c| {
                                c.as_str().map(String::from).or_else(|| {
                                    c.as_array().and_then(|blocks| {
                                        blocks.iter().find_map(|b| {
                                            if b.get("type").and_then(Value::as_str) == Some("text")
                                            {
                                                b.get("text")
                                                    .and_then(Value::as_str)
                                                    .map(String::from)
                                            } else {
                                                None
                                            }
                                        })
                                    })
                                })
                            })
                            .unwrap_or_else(|| "hello".to_string());

                        let model = body
                            .get("model")
                            .and_then(Value::as_str)
                            .unwrap_or("test-model");

                        let stop = stop_reason.read().unwrap().clone();
                        // Messages streams its text as a single delta, so the
                        // fixed text is byte-exact by construction.
                        let events = match &*mode.read().unwrap() {
                            ResponseMode::Echo => {
                                sse::messages_api_events(&format!("Echo: {user_msg}"), model, &stop)
                            }
                            ResponseMode::Fixed(text) => {
                                sse::messages_api_events(text, model, &stop)
                            }
                        };
                        let stream = paced_events(events, *delay.read().unwrap(), None);
                        Sse::new(stream)
                            .keep_alive(KeepAlive::default())
                            .into_response()
                    }
                }),
            )
            .route(
                "/v1/models",
                get({
                    let log = log.clone();
                    move || {
                        let log = log.clone();
                        let models = models.clone();
                        async move {
                            log.record("GET", "/v1/models", None, None, Vec::new());
                            let models_json = models.read().unwrap().clone();
                            Json(json!({
                                "object": "list",
                                "data": models_json,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/v1/settings",
                get({
                    let log = log.clone();
                    move || {
                        let log = log.clone();
                        let settings = settings.clone();
                        let scripted = scripted_settings.clone();
                        async move {
                            log.record("GET", "/v1/settings", None, None, Vec::new());
                            // Scripted one-shots take precedence (FIFO), so a
                            // test can serve a transient payload (e.g. one
                            // stale gated snapshot) and fall back to the
                            // steady-state `set_settings` value afterwards.
                            if let Some(s) = Self::pop_scripted(&scripted, "/v1/settings") {
                                return s.into_response_paced(None);
                            }
                            let maybe = settings.read().unwrap().clone();
                            match maybe {
                                Some(s) => Json(s).into_response(),
                                None => StatusCode::NOT_FOUND.into_response(),
                            }
                        }
                    }
                }),
            )
            .route(
                "/v1/user",
                get(
                    move |axum::extract::RawQuery(query): axum::extract::RawQuery| {
                        let log = log.clone();
                        let user_tier = user_tier.clone();
                        async move {
                            // Keep the query string in the log so tests can
                            // count `?include=subscription` checks separately
                            // from plain enrichment fetches.
                            let path = match query {
                                Some(q) if !q.is_empty() => format!("/v1/user?{q}"),
                                _ => "/v1/user".to_owned(),
                            };
                            log.record("GET", &path, None, None, Vec::new());
                            let tier = user_tier.read().unwrap().clone();
                            let mut body = json!({
                                "userId": "mock-user",
                                "email": "mock-user@test.invalid",
                            });
                            if let Some(t) = tier {
                                body["subscriptionTier"] = json!(t);
                            }
                            Json(body).into_response()
                        }
                    },
                ),
            )
            .route(
                "/v1/storage",
                post({
                    let storage = storage.clone();
                    move |headers: HeaderMap, body: axum::body::Bytes| {
                        let storage = storage.clone();
                        async move { Self::storage_upload_handler(&storage, &headers, &body) }
                    }
                }),
            )
            // The shell probes these before/alongside per-file uploads. Answer
            // 404 ("old proxy") so it falls back to plain `POST /v1/storage`,
            // which is the path the park-on-401 e2e exercises.
            .route(
                "/v1/storage/exists",
                get(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/storage/batch_exists",
                post(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/storage/batch_upload_json",
                post(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/storage/batch_upload",
                post(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/v1/storage/limits",
                get(|| async { StatusCode::NOT_FOUND }),
            )
            // Body limit: repo-context archives can exceed axum's 2 MB default.
            .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
    }
}

impl Drop for MockInferenceServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MERMAID_TEXT: &str =
        "Here is a flow:\n\n```mermaid\nflowchart TD\n  A --> B\n```\n\nDone.\n";

    /// Payloads of all `data:` lines in an SSE body, minus the `[DONE]` marker.
    fn sse_data_payloads(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.trim_start().to_owned())
            .filter(|d| d != "[DONE]")
            .collect()
    }

    /// Concatenation of all chat-completion content deltas in an SSE body.
    fn chat_stream_text(body: &str) -> String {
        sse_data_payloads(body)
            .iter()
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter_map(|v| {
                v.get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("delta"))
                    .and_then(|d| d.get("content"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect()
    }

    /// Concatenation of all responses-API output_text deltas in an SSE body.
    fn responses_stream_text(body: &str) -> String {
        sse_data_payloads(body)
            .iter()
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter(|v| v.get("type").and_then(Value::as_str) == Some("response.output_text.delta"))
            .filter_map(|v| v.get("delta").and_then(Value::as_str).map(String::from))
            .collect()
    }

    /// Concatenation of all Anthropic Messages text deltas in an SSE body.
    fn messages_stream_text(body: &str) -> String {
        sse_data_payloads(body)
            .iter()
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .filter(|v| v.get("type").and_then(Value::as_str) == Some("content_block_delta"))
            .filter_map(|v| {
                v.get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect()
    }

    async fn post_chat(server: &MockInferenceServer, content: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/chat/completions", server.url()))
            .json(&json!({
                "model": "test-model",
                "messages": [{ "role": "user", "content": content }]
            }))
            .send()
            .await
            .expect("POST /v1/chat/completions")
    }

    #[tokio::test]
    async fn echo_mode_echoes_last_user_message() {
        let server = MockInferenceServer::start().await.unwrap();

        let body = post_chat(&server, "ping pong").await.text().await.unwrap();
        assert_eq!(chat_stream_text(&body), "Echo: ping pong");

        // Echo mode keeps its historical whitespace-collapsing semantics.
        let body = post_chat(&server, "a  b\nc").await.text().await.unwrap();
        assert_eq!(chat_stream_text(&body), "Echo: a b c");
    }

    #[tokio::test]
    async fn fixed_mode_reconstructs_byte_exact_over_http() {
        let server = MockInferenceServer::start().await.unwrap();
        server.set_response(MERMAID_TEXT);

        let body = post_chat(&server, "ignored").await.text().await.unwrap();
        assert_eq!(chat_stream_text(&body), MERMAID_TEXT);

        let body = reqwest::Client::new()
            .post(format!("{}/responses", server.url()))
            .json(&json!({
                "model": "test-model",
                "input": [{ "role": "user", "content": "ignored" }]
            }))
            .send()
            .await
            .expect("POST /v1/responses")
            .text()
            .await
            .unwrap();
        assert_eq!(responses_stream_text(&body), MERMAID_TEXT);

        let body = reqwest::Client::new()
            .post(format!("{}/messages", server.url()))
            .json(&json!({
                "model": "test-model",
                "messages": [{ "role": "user", "content": "ignored" }]
            }))
            .send()
            .await
            .expect("POST /v1/messages")
            .text()
            .await
            .unwrap();
        assert_eq!(messages_stream_text(&body), MERMAID_TEXT);
    }

    #[tokio::test]
    async fn settings_404_until_set_then_200() {
        let server = MockInferenceServer::start().await.unwrap();
        let url = format!("{}/settings", server.url());

        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 404);

        server.set_settings(json!({ "tips": ["t1"] }));
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body, json!({ "tips": ["t1"] }));

        server.preset_allow_access();
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body, json!({ "allow_access": true }));
    }

    #[tokio::test]
    async fn request_bodies_returns_bodies_in_arrival_order() {
        let server = MockInferenceServer::start().await.unwrap();

        post_chat(&server, "first").await.text().await.unwrap();
        // Body-less request in between must be skipped, not break ordering.
        reqwest::get(format!("{}/models", server.url()))
            .await
            .unwrap();
        post_chat(&server, "second").await.text().await.unwrap();

        let bodies = server.request_bodies();
        assert_eq!(bodies.len(), 2);
        assert_eq!(
            bodies[0]["messages"][0]["content"],
            json!("first"),
            "bodies must be in arrival order"
        );
        assert_eq!(bodies[1]["messages"][0]["content"], json!("second"));
    }

    #[tokio::test]
    async fn scripted_responses_serve_fifo_per_path_then_fall_back() {
        let server = MockInferenceServer::start().await.unwrap();
        server.enqueue_response(
            "/v1/chat/completions",
            ScriptedResponse::text(401, "Unauthorized"),
        );
        server.enqueue_response(
            "/v1/chat/completions",
            ScriptedResponse::json(500, json!({ "error": { "message": "boom" } })),
        );

        // FIFO: first the 401 text, then the 500 json.
        let resp = post_chat(&server, "hi").await;
        assert_eq!(resp.status(), 401);
        assert_eq!(resp.text().await.unwrap(), "Unauthorized");

        let resp = post_chat(&server, "hi").await;
        assert_eq!(resp.status(), 500);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body, json!({ "error": { "message": "boom" } }));

        // Queue drained: falls back to the active mode (echo).
        let body = post_chat(&server, "ping pong").await.text().await.unwrap();
        assert_eq!(chat_stream_text(&body), "Echo: ping pong");

        // Queues are per path: an unrelated endpoint is unaffected.
        server.enqueue_response("/v1/chat/completions", ScriptedResponse::text(503, "later"));
        let resp = reqwest::Client::new()
            .post(format!("{}/responses", server.url()))
            .json(&json!({
                "model": "test-model",
                "input": [{ "role": "user", "content": "hi there" }]
            }))
            .send()
            .await
            .expect("POST /v1/responses");
        assert_eq!(resp.status(), 200);
        assert_eq!(
            responses_stream_text(&resp.text().await.unwrap()),
            "Echo: hi there "
        );
    }

    /// Pins the documented precedence: a script bypasses the required-auth
    /// gate; once the queue empties, the gate is back.
    #[tokio::test]
    async fn scripted_response_takes_precedence_over_required_auth() {
        let server = MockInferenceServer::start_with_required_auth(
            vec![MockModelEntry::new("test-model")],
            "secret-token",
        )
        .await
        .unwrap();
        server.enqueue_response(
            "/v1/chat/completions",
            ScriptedResponse::text(200, "scripted"),
        );

        // No token: the script still serves.
        let resp = post_chat(&server, "hi").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "scripted");

        // Queue drained: the auth gate applies again.
        let resp = post_chat(&server, "hi").await;
        assert_eq!(resp.status(), 401);
    }

    /// Scripted response headers reach the client (the phase-2 script format's
    /// named consumer: 429 + Retry-After error injection).
    #[tokio::test]
    async fn scripted_response_headers_reach_the_client() {
        let server = MockInferenceServer::start().await.unwrap();
        let mut rate_limited = ScriptedResponse::text(429, "slow down");
        rate_limited
            .headers
            .push(("retry-after".to_string(), "7".to_string()));
        server.enqueue_response("/v1/chat/completions", rate_limited);

        let resp = post_chat(&server, "hi").await;
        assert_eq!(resp.status(), 429);
        assert_eq!(
            resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some("7")
        );
        assert_eq!(resp.text().await.unwrap(), "slow down");
    }

    #[tokio::test]
    async fn scripted_raw_body_served_byte_exact() {
        let server = MockInferenceServer::start().await.unwrap();
        let raw = "data: {\"choices\":[]}\n\ndata: not-json-at-all\n\ndata: [DONE]\n\n";
        server.enqueue_response("/v1/chat/completions", ScriptedResponse::text(200, raw));

        let resp = post_chat(&server, "hi").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), raw);
    }

    #[tokio::test]
    async fn scripted_sse_preserves_event_names_and_order() {
        let server = MockInferenceServer::start().await.unwrap();
        server.enqueue_response(
            "/v1/chat/completions",
            ScriptedResponse::sse(vec![
                SseEvent::with_event("custom.kind", "{\"a\":1}"),
                SseEvent::data("{\"b\":2}"),
            ]),
        );

        let body = post_chat(&server, "hi").await.text().await.unwrap();
        let named_then_plain = body
            .find("event: custom.kind")
            .zip(body.find("data: {\"b\":2}"))
            .is_some_and(|(named, plain)| named < plain);
        assert!(
            body.contains("event: custom.kind") && body.contains("data: {\"a\":1}"),
            "named event must carry both fields, got:\n{body}"
        );
        assert!(named_then_plain, "events must be served in order:\n{body}");
    }

    #[tokio::test]
    async fn request_log_captures_arbitrary_headers() {
        let server = MockInferenceServer::start().await.unwrap();

        reqwest::Client::new()
            .post(format!("{}/chat/completions", server.url()))
            .header("authorization", "Bearer log-me")
            .header("x-test-marker", "zap")
            .json(&json!({
                "model": "test-model",
                "messages": [{ "role": "user", "content": "hi" }]
            }))
            .send()
            .await
            .expect("POST /v1/chat/completions");

        let entry = server.requests().pop().expect("one logged request");
        assert_eq!(entry.header("x-test-marker"), Some("zap"));
        assert_eq!(entry.header("X-Test-Marker"), Some("zap"));
        assert_eq!(entry.header("authorization"), Some("Bearer log-me"));
        assert_eq!(entry.authorization.as_deref(), Some("Bearer log-me"));
        assert_eq!(entry.header("x-absent"), None);
    }

    #[tokio::test]
    async fn required_auth_enforced_in_both_response_modes() {
        let server = MockInferenceServer::start_with_required_auth(
            vec![MockModelEntry::new("test-model")],
            "secret-token",
        )
        .await
        .unwrap();
        let client = reqwest::Client::new();
        let url = format!("{}/chat/completions", server.url());
        let req_body = json!({
            "model": "test-model",
            "messages": [{ "role": "user", "content": "hi there" }]
        });

        // Echo mode: missing auth rejected, valid auth streams the echo.
        let resp = client.post(&url).json(&req_body).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        let resp = client
            .post(&url)
            .header("authorization", "Bearer secret-token")
            .json(&req_body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            chat_stream_text(&resp.text().await.unwrap()),
            "Echo: hi there"
        );

        // Fixed mode: same auth gate, fixed text streamed byte-exact.
        server.set_response(MERMAID_TEXT);
        let resp = client.post(&url).json(&req_body).send().await.unwrap();
        assert_eq!(resp.status(), 401);
        let resp = client
            .post(&url)
            .header("authorization", "Bearer secret-token")
            .json(&req_body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(chat_stream_text(&resp.text().await.unwrap()), MERMAID_TEXT);
    }
}
