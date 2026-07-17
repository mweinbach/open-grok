//! Shell adapter for the persistent in-process Code Mode runtime.
//!
//! The embedded runtime is `Send + Sync`, while [`SessionActor`] intentionally
//! runs on a `LocalSet`. Runtime callbacks therefore cross an unbounded channel
//! and are dispatched by the single local task started with
//! [`CodeModeRuntime::start_dispatch_loop`].

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use agent_client_protocol as acp;
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::{OnceCell, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xai_grok_code_mode::InProcessCodeModeSessionProvider;
use xai_grok_code_mode_protocol::{
    CellId, CodeModeNestedToolCall, CodeModeSession, CodeModeSessionDelegate,
    CodeModeSessionProvider, CodeModeToolKind, ExecuteRequest, FunctionCallOutputContentItem,
    ImageDetail, NotificationFuture, RuntimeResponse, ToolDefinition as CodeModeToolDefinition,
    ToolInvocationFuture, ToolName, WaitOutcome, WaitRequest,
};
use xai_grok_tools::types::definition::ToolDefinition as GrokToolDefinition;
use xai_grok_tools::util::{ceil_char_boundary, truncate_str};

use super::acp_session::SessionActor;
use crate::sampling::rs::{CustomGrammarFormatParam, CustomToolParamFormat, GrammarSyntax};
use crate::sampling::{ClientTool, CustomToolOutputContent, CustomToolOutputImageDetail, ToolSpec};

pub(crate) const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";
pub(crate) const CODE_MODE_TRANSPORT_META_KEY: &str = "open-grok/codeModeTransport";

/// The model uses these tools as an implementation detail for the persistent
/// JavaScript runtime. They must remain in the model conversation, but they are
/// not user-facing tool calls: the nested tools dispatched by `exec` already
/// emit ordinary ACP tool cards with their decoded inputs and results.
pub(crate) fn is_code_mode_transport_tool(name: &str) -> bool {
    matches!(
        name,
        xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME | xai_grok_code_mode_protocol::WAIT_TOOL_NAME
    )
}

/// Whether a persisted ACP update was explicitly emitted as a Code Mode
/// transport wrapper. Tool names alone are not sufficient: plugins and MCP
/// servers are allowed to expose ordinary tools named `exec` or `wait`.
pub(crate) fn is_code_mode_transport_meta(meta: Option<&acp::Meta>) -> bool {
    meta.and_then(|meta| meta.get(CODE_MODE_TRANSPORT_META_KEY))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

/// Human-interaction tools must remain model-visible in Code Mode Only: a
/// JavaScript callback cannot safely own an ACP question whose answer pauses
/// the model turn. Collaboration lifecycle tools also stay direct, matching
/// GPT-5.6 Sol's Codex multi-agent-v2 `DirectModelOnly` exposure.
pub(crate) fn is_code_mode_direct_only_tool(name: &str) -> bool {
    matches!(
        name,
        "ask_user_question"
            | "request_user_input"
            | "task"
            | "spawn_subagent"
            | "get_task_output"
            | "get_command_or_subagent_output"
            | "wait_tasks"
            | "wait_commands_or_subagents"
            | "kill_task"
            | "kill_command_or_subagent"
    )
}

/// Apply Code Mode Only's provider-specific hosted-tool policy.
///
/// Provider-hosted search remains top-level beside `exec` for both providers:
/// xAI keeps native web/X search, while Codex keeps native OpenAI web search
/// and excludes `x_search` at the provider boundary.
pub(crate) fn hosted_tools_for_code_mode(
    hosted_tools: &[xai_grok_sampling_types::HostedTool],
    tool_mode: xai_grok_sampling_types::ToolMode,
    provider: xai_grok_sampling_types::ModelProvider,
) -> Vec<xai_grok_sampling_types::HostedTool> {
    let provider_tools = xai_grok_sampling_types::hosted_tools_for_provider(hosted_tools, provider);
    if tool_mode != xai_grok_sampling_types::ToolMode::CodeModeOnly {
        return provider_tools;
    }
    if provider != xai_grok_sampling_types::ModelProvider::Codex {
        return provider_tools;
    }
    provider_tools
        .into_iter()
        .filter(|tool| matches!(tool, xai_grok_sampling_types::HostedTool::WebSearch { .. }))
        .collect()
}

/// Avoid advertising a provider-hosted web search a second time inside the
/// Code Mode `exec` tool.
pub(crate) fn nested_tool_definitions_for_provider(
    definitions: &[GrokToolDefinition],
    _provider: xai_grok_sampling_types::ModelProvider,
    hosted_tools: &[xai_grok_sampling_types::HostedTool],
) -> Vec<GrokToolDefinition> {
    let has_hosted_web = hosted_tools
        .iter()
        .any(|tool| matches!(tool, xai_grok_sampling_types::HostedTool::WebSearch { .. }));
    definitions
        .iter()
        .filter(|definition| !has_hosted_web || definition.function.name != "web_search")
        .cloned()
        .collect()
}

const CODE_MODE_FREEFORM_GRAMMAR: &str = r#"
start: pragma_source | plain_source
pragma_source: PRAGMA_LINE NEWLINE SOURCE
plain_source: SOURCE

PRAGMA_LINE: /[ \t]*\/\/ @exec:[^\r\n]*/
NEWLINE: /\r?\n/
SOURCE: /[\s\S]+/
"#;

enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<serde_json::Value, String>>,
    },
    Notify {
        call_id: String,
        text: String,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
}

/// A session-scoped, lazily initialized Code Mode host.
///
/// Construct this once per [`SessionActor`], call [`Self::start_dispatch_loop`]
/// after the actor has been wrapped in an [`Arc`], and retain it for the full
/// actor lifetime. Calls to [`Self::exec`] share one persistent JavaScript
/// session, including values written through `store()`.
pub(crate) struct CodeModeRuntime {
    session: OnceCell<Arc<dyn CodeModeSession>>,
    known_cells: Mutex<HashSet<CellId>>,
    dispatch_tx: mpsc::UnboundedSender<DispatchMessage>,
    dispatch_rx: Mutex<Option<mpsc::UnboundedReceiver<DispatchMessage>>>,
    dispatch_task: Mutex<Option<tokio::task::AbortHandle>>,
    shutting_down: AtomicBool,
}

/// The V8 session must not strongly retain its owner: `CodeModeRuntime` owns
/// the session, so installing the runtime itself as the delegate would create
/// an `Arc` cycle and keep every reset isolate allocated forever.
struct CodeModeRuntimeDelegate {
    runtime: Weak<CodeModeRuntime>,
}

impl CodeModeRuntime {
    pub(crate) fn new() -> Arc<Self> {
        let (dispatch_tx, dispatch_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            session: OnceCell::new(),
            known_cells: Mutex::new(HashSet::new()),
            dispatch_tx,
            dispatch_rx: Mutex::new(Some(dispatch_rx)),
            dispatch_task: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        })
    }

    /// Starts the sole local callback dispatcher.
    ///
    /// The receiver can only be taken once. A second call returns an error
    /// rather than silently creating a competing consumer.
    async fn start_dispatch_loop(
        self: &Arc<Self>,
        session_actor: Weak<SessionActor>,
        generation: RuntimeGeneration,
    ) -> Result<(), String> {
        let mut receiver = self
            .dispatch_rx
            .lock()
            .take()
            .ok_or_else(|| "code mode dispatch loop already started".to_string())?;

        let task = tokio::task::spawn_local(async move {
            while let Some(message) = receiver.recv().await {
                match message {
                    message @ DispatchMessage::InvokeTool { .. } => {
                        spawn_dispatch_message(session_actor.clone(), generation.clone(), message);
                    }
                    message @ DispatchMessage::Notify { .. } => {
                        dispatch_message(session_actor.clone(), &generation, message).await;
                    }
                }
            }
        });
        self.dispatch_task.lock().replace(task.abort_handle());
        Ok(())
    }

    /// Executes raw custom-tool input in the persistent JavaScript session.
    ///
    /// `raw_input` is parsed with the pinned Codex first-line pragma parser;
    /// callers must not JSON-wrap the JavaScript source.
    pub(crate) async fn exec(
        self: &Arc<Self>,
        call_id: &str,
        raw_input: &str,
        enabled_tools: &[GrokToolDefinition],
    ) -> Result<CodeModeToolOutput, String> {
        let parsed = xai_grok_code_mode_protocol::parse_exec_source(raw_input)?;
        let enabled_tools = collect_code_mode_tool_definitions(enabled_tools);
        let max_output_tokens = parsed.max_output_tokens;
        let started_at = Instant::now();
        let started_cell = self
            .session()
            .await?
            .execute(ExecuteRequest {
                tool_call_id: call_id.to_string(),
                enabled_tools,
                source: parsed.code,
                yield_time_ms: parsed.yield_time_ms,
                max_output_tokens,
            })
            .await?;
        self.known_cells.lock().insert(started_cell.cell_id.clone());
        let cell_id = started_cell.cell_id.clone();
        let response = match started_cell.initial_response().await {
            Ok(response) => response,
            Err(error) => {
                self.known_cells.lock().remove(&cell_id);
                return Err(error);
            }
        };
        if is_terminal_runtime_response(&response) {
            self.known_cells.lock().remove(&cell_id);
        }
        Ok(format_runtime_response(
            response,
            max_output_tokens,
            started_at.elapsed(),
        ))
    }

    /// Waits for, or terminates, a yielded cell using the function tool's raw
    /// JSON arguments.
    pub(crate) async fn wait(
        self: &Arc<Self>,
        raw_arguments: &str,
    ) -> Result<CodeModeToolOutput, String> {
        let arguments = parse_wait_arguments(raw_arguments)?;
        let cell_id = CellId::new(arguments.cell_id);
        if !self.known_cells.lock().contains(&cell_id) {
            return Err(format!(
                "code mode wait cell `{cell_id}` is not live in the current runtime generation; it belongs to a prior runtime generation or has already completed, and yielded cells cannot be resumed after session reload or timeline reset"
            ));
        }
        let started_at = Instant::now();
        let session = self.session().await?;
        let response: RuntimeResponse = if arguments.terminate {
            session.terminate(cell_id.clone()).await?
        } else {
            session
                .wait(WaitRequest {
                    cell_id: cell_id.clone(),
                    yield_time_ms: arguments.yield_time_ms,
                })
                .await?
        }
        .into();
        if is_terminal_runtime_response(&response) {
            self.known_cells.lock().remove(&cell_id);
        }
        Ok(format_runtime_response(
            response,
            arguments.max_tokens,
            started_at.elapsed(),
        ))
    }

    /// Shuts down an initialized runtime without creating an otherwise unused
    /// session. Initialization racing with shutdown is joined and cancelled,
    /// matching Codex's session-lifecycle contract.
    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.shutting_down.store(true, Ordering::Release);
        let result = match self
            .session
            .get_or_try_init(|| async {
                Err::<Arc<dyn CodeModeSession>, String>(
                    "code mode session is shutting down".to_string(),
                )
            })
            .await
        {
            Ok(session) => session.shutdown().await,
            Err(_) => Ok(()),
        };
        if let Some(task) = self.dispatch_task.lock().take() {
            task.abort();
        }
        result
    }

    async fn session(self: &Arc<Self>) -> Result<Arc<dyn CodeModeSession>, String> {
        if self.shutting_down.load(Ordering::Acquire) {
            return Err("code mode session is shutting down".to_string());
        }
        self.session
            .get_or_try_init(|| {
                let delegate: Arc<dyn CodeModeSessionDelegate> =
                    Arc::new(CodeModeRuntimeDelegate {
                        runtime: Arc::downgrade(self),
                    });
                async move {
                    if self.shutting_down.load(Ordering::Acquire) {
                        return Err("code mode session is shutting down".to_string());
                    }
                    let session = InProcessCodeModeSessionProvider
                        .create_session(delegate)
                        .await?;
                    if self.shutting_down.load(Ordering::Acquire) {
                        let _ = session.shutdown().await;
                        return Err("code mode session is shutting down".to_string());
                    }
                    Ok(session)
                }
            })
            .await
            .map(Arc::clone)
    }
}

impl CodeModeSessionDelegate for CodeModeRuntimeDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            let runtime = self
                .runtime
                .upgrade()
                .ok_or_else(|| "code mode runtime owner is unavailable".to_string())?;
            CodeModeSessionDelegate::invoke_tool(runtime.as_ref(), invocation, cancellation_token)
                .await
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            let runtime = self
                .runtime
                .upgrade()
                .ok_or_else(|| "code mode runtime owner is unavailable".to_string())?;
            CodeModeSessionDelegate::notify(
                runtime.as_ref(),
                call_id,
                cell_id,
                text,
                cancellation_token,
            )
            .await
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        if let Some(runtime) = self.runtime.upgrade() {
            CodeModeSessionDelegate::cell_closed(runtime.as_ref(), cell_id);
        }
    }
}

#[derive(Clone)]
struct RuntimeGeneration {
    current: Arc<AtomicU64>,
    expected: u64,
}

struct BindRuntimeRequest {
    runtime: Arc<CodeModeRuntime>,
    generation: RuntimeGeneration,
    respond_to: oneshot::Sender<Result<(), String>>,
}

impl RuntimeGeneration {
    fn is_current(&self) -> bool {
        self.current.load(Ordering::Acquire) == self.expected
    }
}

/// Replaceable owner for the session's persistent JavaScript runtime.
///
/// A model/provider transition or conversation rewind can invalidate the old
/// timeline without making future Code Mode calls permanently unusable. Each
/// dispatcher carries the generation it was created for, so callbacks that
/// race shutdown fail closed instead of attaching to the replacement runtime's
/// conversation.
pub(crate) struct CodeModeRuntimeSlot {
    current: Mutex<Arc<CodeModeRuntime>>,
    generation: Arc<AtomicU64>,
    bind_tx: Mutex<Option<mpsc::UnboundedSender<BindRuntimeRequest>>>,
    bind_task: Mutex<Option<tokio::task::AbortHandle>>,
    reset_lock: tokio::sync::Mutex<()>,
}

impl CodeModeRuntimeSlot {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(CodeModeRuntime::new()),
            generation: Arc::new(AtomicU64::new(0)),
            bind_tx: Mutex::new(None),
            bind_task: Mutex::new(None),
            reset_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) fn current(&self) -> Arc<CodeModeRuntime> {
        Arc::clone(&self.current.lock())
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Snapshot the current runtime before awaiting. The slot mutex is never
    /// held across V8 work, while a concurrent reset can still swap the owner,
    /// shut down this snapshot, and invalidate its callback generation.
    pub(crate) async fn exec(
        &self,
        call_id: &str,
        raw_input: &str,
        enabled_tools: &[GrokToolDefinition],
    ) -> Result<CodeModeToolOutput, String> {
        self.current().exec(call_id, raw_input, enabled_tools).await
    }

    pub(crate) async fn wait(&self, raw_arguments: &str) -> Result<CodeModeToolOutput, String> {
        self.current().wait(raw_arguments).await
    }

    pub(crate) async fn start_dispatch_loop(
        self: &Arc<Self>,
        session_actor: Weak<SessionActor>,
    ) -> Result<(), String> {
        let (bind_tx, mut bind_rx) = mpsc::unbounded_channel::<BindRuntimeRequest>();
        {
            let mut active = self.bind_tx.lock();
            if active.is_some() {
                return Err("code mode runtime binder already started".to_string());
            }
            active.replace(bind_tx.clone());
        }
        let bind_task = tokio::task::spawn_local(async move {
            while let Some(request) = bind_rx.recv().await {
                let result = request
                    .runtime
                    .start_dispatch_loop(session_actor.clone(), request.generation)
                    .await;
                let _ = request.respond_to.send(result);
            }
        });
        self.bind_task.lock().replace(bind_task.abort_handle());
        let generation = RuntimeGeneration {
            current: Arc::clone(&self.generation),
            expected: self.generation(),
        };
        let result = bind_runtime(&bind_tx, self.current(), generation).await;
        if result.is_err() {
            self.bind_tx.lock().take();
            if let Some(task) = self.bind_task.lock().take() {
                task.abort();
            }
        }
        result
    }

    /// Install a fresh lazy runtime and invalidate every callback owned by the
    /// prior timeline. An unbound slot (unit-test / pre-actor construction)
    /// remains unbound until `start_dispatch_loop` is called.
    pub(crate) async fn reset(&self) -> Result<(), String> {
        let _reset_guard = self.reset_lock.lock().await;
        let next_generation = self
            .generation()
            .checked_add(1)
            .ok_or_else(|| "code mode runtime generation exhausted".to_string())?;
        let replacement = CodeModeRuntime::new();
        let bind_tx = self.bind_tx.lock().clone();
        if let Some(bind_tx) = bind_tx {
            bind_runtime(
                &bind_tx,
                replacement.clone(),
                RuntimeGeneration {
                    current: Arc::clone(&self.generation),
                    expected: next_generation,
                },
            )
            .await?;
        }
        let previous = {
            let mut current = self.current.lock();
            self.generation.store(next_generation, Ordering::Release);
            std::mem::replace(&mut *current, replacement)
        };
        if let Err(error) = previous.shutdown().await {
            tracing::warn!(%error, "failed to shut down superseded Code Mode runtime");
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        let _reset_guard = self.reset_lock.lock().await;
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.bind_tx.lock().take();
        if let Some(task) = self.bind_task.lock().take() {
            task.abort();
        }
        self.current().shutdown().await
    }
}

async fn bind_runtime(
    bind_tx: &mpsc::UnboundedSender<BindRuntimeRequest>,
    runtime: Arc<CodeModeRuntime>,
    generation: RuntimeGeneration,
) -> Result<(), String> {
    let (respond_to, response) = oneshot::channel();
    bind_tx
        .send(BindRuntimeRequest {
            runtime,
            generation,
            respond_to,
        })
        .map_err(|_| "code mode runtime binder is unavailable".to_string())?;
    response
        .await
        .map_err(|_| "code mode runtime binder stopped".to_string())?
}

impl CodeModeSessionDelegate for CodeModeRuntime {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        _cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if text.trim().is_empty() {
                return Ok(());
            }
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::Notify {
                    call_id,
                    text,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .map_err(|_| "code mode notification dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.known_cells.lock().remove(cell_id);
    }
}

fn spawn_dispatch_message(
    session_actor: Weak<SessionActor>,
    generation: RuntimeGeneration,
    message: DispatchMessage,
) {
    tokio::task::spawn_local(async move {
        dispatch_message(session_actor, &generation, message).await;
    });
}

/// Dispatch one runtime callback. Notifications are awaited serially by the
/// receiver loop so repeated `notify()` outputs retain FIFO order; nested tool
/// invocations call this from their own local tasks and may run concurrently.
async fn dispatch_message(
    session_actor: Weak<SessionActor>,
    generation: &RuntimeGeneration,
    message: DispatchMessage,
) {
    match message {
        DispatchMessage::InvokeTool {
            invocation,
            cancellation_token,
            response_tx,
        } => {
            let response = match wait_for_active_code_mode_turn(
                &session_actor,
                &cancellation_token,
                generation,
                "nested tool call",
            )
            .await
            {
                Ok(session_actor) => tokio::select! {
                    response = session_actor.dispatch_code_mode_nested_tool(
                        invocation,
                        cancellation_token.clone(),
                    ) => response,
                    _ = cancellation_token.cancelled() => {
                        Err("code mode nested tool call cancelled".to_string())
                    }
                },
                Err(error) => Err(error),
            };
            let _ = response_tx.send(response);
        }
        DispatchMessage::Notify {
            call_id,
            text,
            cancellation_token,
            response_tx,
        } => {
            let response = match wait_for_active_code_mode_turn(
                &session_actor,
                &cancellation_token,
                generation,
                "notification",
            )
            .await
            {
                Ok(session_actor) => tokio::select! {
                    _ = session_actor.record_code_mode_notification(call_id, text) => Ok(()),
                    _ = cancellation_token.cancelled() => {
                        Err("code mode notification cancelled".to_string())
                    }
                },
                Err(error) => Err(error),
            };
            let _ = response_tx.send(response);
        }
    }
}

/// Wait for the next Code Mode turn instead of rejecting callbacks that arrive
/// after a yielded cell's originating turn has ended. Codex queues these
/// callbacks while no turn-scoped receiver is installed; polling the actor's
/// existing lifecycle flags gives the embedded adapter the same behavior
/// without keeping a strong actor reference alive between turns.
async fn wait_for_active_code_mode_turn(
    session_actor: &Weak<SessionActor>,
    cancellation_token: &CancellationToken,
    generation: &RuntimeGeneration,
    operation: &str,
) -> Result<Arc<SessionActor>, String> {
    loop {
        if !generation.is_current() {
            return Err(format!(
                "code mode {operation} belongs to a superseded runtime generation"
            ));
        }
        if cancellation_token.is_cancelled() {
            return Err(format!("code mode {operation} cancelled"));
        }

        let Some(actor) = session_actor.upgrade() else {
            return Err(format!("code mode {operation} dispatcher is unavailable"));
        };
        let turn_active = actor.session_turn_active.load(Ordering::Acquire);
        let code_mode_active = matches!(
            actor.agent.borrow().tool_mode(),
            xai_grok_sampling_types::ToolMode::CodeMode
                | xai_grok_sampling_types::ToolMode::CodeModeOnly
        );
        if turn_active && code_mode_active {
            return Ok(actor);
        }
        drop(actor);

        tokio::select! {
            _ = cancellation_token.cancelled() => {
                return Err(format!("code mode {operation} cancelled"));
            }
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
        }
    }
}

/// Model-facing result from `exec` or `wait`.
///
/// `content` retains the runtime's text/image ordering and image fidelity.
/// Missing runtime details are resolved to Codex's `high` default.
#[derive(Clone, Debug)]
pub(crate) struct CodeModeToolOutput {
    pub(crate) content: Vec<CustomToolOutputContent>,
    pub(crate) cell_id: Option<String>,
    pub(crate) success: bool,
}

impl CodeModeToolOutput {
    /// Concatenates text parts without inventing separators. The Codex status
    /// header already ends in a newline before the first body part.
    pub(crate) fn text(&self) -> String {
        let mut text = String::new();
        for part in &self.content {
            if let CustomToolOutputContent::Text { text: part_text } = part {
                text.push_str(part_text);
            }
        }
        text
    }
}

/// Converts a Grok function definition into the embedded runtime protocol.
///
/// The JavaScript-visible name is normalized, while `tool_name` retains the
/// original registry key used by [`SessionActor::dispatch_code_mode_nested_tool`].
pub(crate) fn to_code_mode_tool_definition(
    definition: &GrokToolDefinition,
) -> CodeModeToolDefinition {
    let raw_name = definition.function.name.clone();
    let (kind, input_schema) = if raw_name == APPLY_PATCH_TOOL_NAME {
        // GPT-5.6 Sol's pinned Codex profile exposes apply_patch as a
        // free-form nested tool. The shell dispatcher adapts the raw patch
        // string back into Grok Build's existing `{ patch }` function input.
        (CodeModeToolKind::Freeform, None)
    } else {
        (
            CodeModeToolKind::Function,
            Some(definition.function.parameters.clone()),
        )
    };
    CodeModeToolDefinition {
        name: xai_grok_code_mode_protocol::normalize_code_mode_identifier(&raw_name),
        tool_name: ToolName::plain(raw_name),
        description: definition.function.description.clone().unwrap_or_default(),
        kind,
        input_schema,
        output_schema: None,
    }
}

pub(crate) fn collect_code_mode_tool_definitions(
    definitions: &[GrokToolDefinition],
) -> Vec<CodeModeToolDefinition> {
    let mut definitions = definitions
        .iter()
        .map(to_code_mode_tool_definition)
        .filter(|definition| {
            !is_code_mode_direct_only_tool(&definition.tool_name.name)
                && xai_grok_code_mode_protocol::is_code_mode_nested_tool(&definition.name)
        })
        .collect::<Vec<_>>();
    definitions.sort_by(|left, right| left.name.cmp(&right.name));
    definitions.dedup_by(|left, right| left.name == right.name);
    definitions
}

/// Creates the native Responses custom `exec` declaration.
pub(crate) fn create_exec_tool(
    enabled_tools: &[GrokToolDefinition],
    code_mode_only: bool,
) -> ClientTool {
    let enabled_tools = collect_code_mode_tool_definitions(enabled_tools);
    ClientTool::Custom {
        name: xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME.to_string(),
        description: Some(xai_grok_code_mode_protocol::build_exec_tool_description(
            &enabled_tools,
            &[],
            &BTreeMap::new(),
            code_mode_only,
        )),
        format: CustomToolParamFormat::Grammar(CustomGrammarFormatParam {
            definition: CODE_MODE_FREEFORM_GRAMMAR.to_string(),
            syntax: GrammarSyntax::Lark,
        }),
    }
}

/// Creates the xAI Responses function-envelope `exec` declaration. Its nested
/// tool guidance matches native Code Mode, while the input contract explicitly
/// describes the required JSON `source` string instead of raw top-level input.
pub(crate) fn create_exec_function_tool(
    enabled_tools: &[GrokToolDefinition],
    code_mode_only: bool,
) -> ToolSpec {
    let enabled_tools = collect_code_mode_tool_definitions(enabled_tools);
    let native_description = xai_grok_code_mode_protocol::build_exec_tool_description(
        &enabled_tools,
        &[],
        &BTreeMap::new(),
        code_mode_only,
    );
    ToolSpec {
        name: xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME.to_string(),
        description: Some(
            xai_grok_sampling_types::code_mode_exec_function_description(Some(&native_description)),
        ),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Raw JavaScript source to run in the persistent Code Mode session."
                }
            },
            "required": ["source"],
            "additionalProperties": false
        }),
    }
}

/// Creates the ordinary function `wait` declaration with the pinned Codex
/// schema and description.
pub(crate) fn create_wait_tool() -> ToolSpec {
    ToolSpec {
        name: xai_grok_code_mode_protocol::WAIT_TOOL_NAME.to_string(),
        description: Some(format!(
            "Waits on a yielded `{}` cell and returns new output or completion.\n{}",
            xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME,
            xai_grok_code_mode_protocol::build_wait_tool_description().trim()
        )),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "cell_id": {
                    "type": "string",
                    "description": "Identifier of the running exec cell."
                },
                "max_tokens": {
                    "type": "number",
                    "description": "Output token budget for this wait call. Defaults to 10000 tokens."
                },
                "terminate": {
                    "type": "boolean",
                    "description": "True stops the running exec cell; false or omitted waits for output."
                },
                "yield_time_ms": {
                    "type": "number",
                    "description": "Wait before yielding more output. Defaults to 10000 ms."
                }
            },
            "required": ["cell_id"],
            "additionalProperties": false
        }),
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
struct WaitArguments {
    cell_id: String,
    #[serde(default = "default_wait_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    terminate: bool,
}

fn default_wait_yield_time_ms() -> u64 {
    xai_grok_code_mode_protocol::DEFAULT_WAIT_YIELD_TIME_MS
}

fn parse_wait_arguments(arguments: &str) -> Result<WaitArguments, String> {
    serde_json::from_str(arguments)
        .map_err(|error| format!("failed to parse function arguments: {error}"))
}

fn format_runtime_response(
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    wall_time: Duration,
) -> CodeModeToolOutput {
    let (status, cell_id, mut items, success) = match response {
        RuntimeResponse::Yielded {
            cell_id,
            content_items,
        } => (
            format!("Script running with cell ID {cell_id}"),
            cell_id,
            content_items,
            true,
        ),
        RuntimeResponse::Terminated {
            cell_id,
            content_items,
        } => (
            "Script terminated".to_string(),
            cell_id,
            content_items,
            true,
        ),
        RuntimeResponse::Result {
            cell_id,
            content_items,
            error_text,
        } => {
            let success = error_text.is_none();
            let status = if success {
                "Script completed"
            } else {
                "Script failed"
            };
            let mut content_items = content_items;
            if let Some(error_text) = error_text {
                content_items.push(FunctionCallOutputContentItem::InputText {
                    text: format!("Script error:\n{error_text}"),
                });
            }
            (status.to_string(), cell_id, content_items, success)
        }
    };

    items = truncate_code_mode_result(items, resolve_max_tokens(max_output_tokens));
    let wall_time_seconds = (wall_time.as_secs_f32() * 10.0).round() / 10.0;
    items.insert(
        0,
        FunctionCallOutputContentItem::InputText {
            text: format!("{status}\nWall time {wall_time_seconds:.1} seconds\nOutput:\n"),
        },
    );

    let mut content = Vec::with_capacity(items.len());
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => {
                content.push(CustomToolOutputContent::text(text));
            }
            FunctionCallOutputContentItem::InputImage { image_url, detail } => {
                let detail =
                    match detail.unwrap_or(xai_grok_code_mode_protocol::DEFAULT_IMAGE_DETAIL) {
                        ImageDetail::Auto => CustomToolOutputImageDetail::Auto,
                        ImageDetail::Low => CustomToolOutputImageDetail::Low,
                        ImageDetail::High => CustomToolOutputImageDetail::High,
                        ImageDetail::Original => CustomToolOutputImageDetail::Original,
                    };
                content.push(CustomToolOutputContent::image(image_url, detail));
            }
        }
    }

    CodeModeToolOutput {
        content,
        cell_id: Some(cell_id.to_string()),
        success,
    }
}

fn is_terminal_runtime_response(response: &RuntimeResponse) -> bool {
    matches!(
        response,
        RuntimeResponse::Terminated { .. } | RuntimeResponse::Result { .. }
    )
}

fn resolve_max_tokens(max_output_tokens: Option<usize>) -> usize {
    max_output_tokens
        .unwrap_or(xai_grok_code_mode_protocol::DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL)
}

fn truncate_code_mode_result(
    items: Vec<FunctionCallOutputContentItem>,
    max_tokens: usize,
) -> Vec<FunctionCallOutputContentItem> {
    if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        return formatted_truncate_text_items(items, max_tokens);
    }

    let mut output = Vec::with_capacity(items.len());
    let mut remaining_tokens = max_tokens;
    let mut omitted_text_items = 0usize;
    for item in items {
        match item {
            FunctionCallOutputContentItem::InputText { text } => {
                if remaining_tokens == 0 {
                    omitted_text_items += 1;
                    continue;
                }
                let cost = approximate_token_count(&text);
                if cost <= remaining_tokens {
                    output.push(FunctionCallOutputContentItem::InputText { text });
                    remaining_tokens = remaining_tokens.saturating_sub(cost);
                } else {
                    let text = truncate_middle_with_token_budget(&text, remaining_tokens);
                    if text.is_empty() {
                        omitted_text_items += 1;
                    } else {
                        output.push(FunctionCallOutputContentItem::InputText { text });
                    }
                    remaining_tokens = 0;
                }
            }
            image @ FunctionCallOutputContentItem::InputImage { .. } => output.push(image),
        }
    }
    if omitted_text_items > 0 {
        output.push(FunctionCallOutputContentItem::InputText {
            text: format!("[omitted {omitted_text_items} text items ...]"),
        });
    }
    output
}

fn formatted_truncate_text_items(
    items: Vec<FunctionCallOutputContentItem>,
    max_tokens: usize,
) -> Vec<FunctionCallOutputContentItem> {
    let text_segments = items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => Some(text.as_str()),
            FunctionCallOutputContentItem::InputImage { .. } => None,
        })
        .collect::<Vec<_>>();
    if text_segments.is_empty() {
        return items;
    }
    let combined = text_segments.join("\n");
    if combined.len() <= approximate_bytes_for_tokens(max_tokens) {
        return items;
    }

    let original_token_count = approximate_token_count(&combined);
    let total_lines = combined.lines().count();
    let truncated = truncate_middle_with_token_budget(&combined, max_tokens);
    vec![FunctionCallOutputContentItem::InputText {
        text: format!(
            "Warning: truncated output (original token count: {original_token_count})\n\
             Total output lines: {total_lines}\n\n{truncated}"
        ),
    }]
}

fn approximate_token_count(text: &str) -> usize {
    approximate_tokens_from_byte_count(text.len())
}

fn approximate_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(4)
}

fn approximate_tokens_from_byte_count(bytes: usize) -> usize {
    bytes.saturating_add(3) / 4
}

fn truncate_middle_with_token_budget(text: &str, max_tokens: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let max_bytes = approximate_bytes_for_tokens(max_tokens);
    if max_tokens > 0 && text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes == 0 {
        return format!("…{} tokens truncated…", approximate_token_count(text));
    }

    let left_budget = max_bytes / 2;
    let right_budget = max_bytes - left_budget;
    let prefix = truncate_str(text, left_budget);
    let suffix_target = text.len().saturating_sub(right_budget);
    let suffix_start = ceil_char_boundary(text, suffix_target).max(prefix.len());
    let removed_tokens = approximate_tokens_from_byte_count(text.len().saturating_sub(max_bytes));
    format!(
        "{prefix}…{removed_tokens} tokens truncated…{}",
        &text[suffix_start..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_slot_remains_send_and_sync_without_storing_the_local_actor() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CodeModeRuntimeSlot>();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_reset_replaces_generation_and_rejects_stale_callbacks() {
        let slot = CodeModeRuntimeSlot::new();
        let previous_runtime = slot.current();
        let previous_generation = RuntimeGeneration {
            current: Arc::clone(&slot.generation),
            expected: slot.generation(),
        };
        assert!(previous_generation.is_current());

        slot.reset().await.expect("runtime reset must succeed");

        assert_eq!(slot.generation(), 1);
        assert!(!previous_generation.is_current());
        assert!(!Arc::ptr_eq(&previous_runtime, &slot.current()));
        let error = match wait_for_active_code_mode_turn(
            &Weak::<SessionActor>::new(),
            &CancellationToken::new(),
            &previous_generation,
            "nested tool call",
        )
        .await
        {
            Ok(_) => panic!("a stale callback must fail before it can reach the actor"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            "code mode nested tool call belongs to a superseded runtime generation"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn runtime_shutdown_invalidates_the_live_generation() {
        let slot = CodeModeRuntimeSlot::new();
        let live_generation = RuntimeGeneration {
            current: Arc::clone(&slot.generation),
            expected: slot.generation(),
        };

        slot.shutdown()
            .await
            .expect("runtime shutdown must succeed");

        assert!(!live_generation.is_current());
    }

    #[test]
    fn runtime_cell_closed_forgets_yielded_cell() {
        let runtime = CodeModeRuntime::new();
        let cell_id = CellId::new("closed-cell".to_string());
        runtime.known_cells.lock().insert(cell_id.clone());

        CodeModeSessionDelegate::cell_closed(runtime.as_ref(), &cell_id);

        assert!(
            !runtime.known_cells.lock().contains(&cell_id),
            "a runtime close callback must make later wait calls fail locally"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cold_runtime_rejects_wait_from_a_prior_generation_without_initializing_v8() {
        let runtime = CodeModeRuntime::new();

        let error = runtime
            .wait(r#"{"cell_id":"persisted-cell","yield_time_ms":1}"#)
            .await
            .expect_err("a fresh runtime must reject a persisted yielded cell");

        assert_eq!(
            error,
            "code mode wait cell `persisted-cell` is not live in the current runtime generation; it belongs to a prior runtime generation or has already completed, and yielded cells cannot be resumed after session reload or timeline reset"
        );
        assert!(
            runtime.session.get().is_none(),
            "rejecting a prior-generation wait must not initialize a replacement isolate"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn initialized_runtime_drops_after_shutdown_without_a_delegate_cycle() {
        let runtime = CodeModeRuntime::new();
        let weak_runtime = Arc::downgrade(&runtime);

        let output = runtime
            .exec("cycle-test", r#"text("done");"#, &[])
            .await
            .expect("simple V8 cell must complete");
        assert!(output.success);
        runtime.shutdown().await.expect("runtime shutdown");
        drop(runtime);

        assert!(
            weak_runtime.upgrade().is_none(),
            "the V8 session delegate must not strongly retain its runtime owner"
        );
    }

    #[test]
    fn only_exec_and_wait_are_hidden_transport_tools() {
        assert!(is_code_mode_transport_tool(
            xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME
        ));
        assert!(is_code_mode_transport_tool(
            xai_grok_code_mode_protocol::WAIT_TOOL_NAME
        ));
        assert!(!is_code_mode_transport_tool("exec_command"));
        assert!(!is_code_mode_transport_tool("apply_patch"));
        assert!(!is_code_mode_transport_tool("web_search"));
    }

    #[test]
    fn replay_transport_marker_must_be_explicitly_true() {
        let mut marked = serde_json::Map::new();
        marked.insert(
            CODE_MODE_TRANSPORT_META_KEY.to_string(),
            serde_json::Value::Bool(true),
        );
        let mut false_marker = serde_json::Map::new();
        false_marker.insert(
            CODE_MODE_TRANSPORT_META_KEY.to_string(),
            serde_json::Value::Bool(false),
        );
        let unrelated = json!({ "x.ai/tool": { "name": "exec" } })
            .as_object()
            .cloned()
            .unwrap();

        assert!(is_code_mode_transport_meta(Some(&marked)));
        assert!(!is_code_mode_transport_meta(Some(&false_marker)));
        assert!(!is_code_mode_transport_meta(Some(&unrelated)));
        assert!(!is_code_mode_transport_meta(None));
    }

    #[test]
    fn exec_tool_uses_pinned_lark_grammar_and_description() {
        let tools = vec![GrokToolDefinition::function(
            "update-plan",
            Some("Update the plan"),
            json!({"type": "object"}),
        )];
        let tool = create_exec_tool(&tools, true);
        let ClientTool::Custom {
            name,
            description,
            format,
        } = tool
        else {
            panic!("exec must be a native custom tool");
        };

        assert_eq!(name, xai_grok_code_mode_protocol::PUBLIC_TOOL_NAME);
        assert_eq!(
            description,
            Some(xai_grok_code_mode_protocol::build_exec_tool_description(
                &collect_code_mode_tool_definitions(&tools),
                &[],
                &BTreeMap::new(),
                true,
            ))
        );
        assert_eq!(
            format,
            CustomToolParamFormat::Grammar(CustomGrammarFormatParam {
                definition: CODE_MODE_FREEFORM_GRAMMAR.to_string(),
                syntax: GrammarSyntax::Lark,
            })
        );
    }

    #[test]
    fn wait_tool_matches_pinned_schema() {
        let tool = create_wait_tool();
        assert_eq!(tool.name, xai_grok_code_mode_protocol::WAIT_TOOL_NAME);
        assert_eq!(
            tool.description,
            Some(format!(
                "Waits on a yielded `exec` cell and returns new output or completion.\n{}",
                xai_grok_code_mode_protocol::build_wait_tool_description().trim()
            ))
        );
        assert_eq!(
            tool.parameters,
            json!({
                "type": "object",
                "properties": {
                    "cell_id": {
                        "type": "string",
                        "description": "Identifier of the running exec cell."
                    },
                    "max_tokens": {
                        "type": "number",
                        "description": "Output token budget for this wait call. Defaults to 10000 tokens."
                    },
                    "terminate": {
                        "type": "boolean",
                        "description": "True stops the running exec cell; false or omitted waits for output."
                    },
                    "yield_time_ms": {
                        "type": "number",
                        "description": "Wait before yielding more output. Defaults to 10000 ms."
                    }
                },
                "required": ["cell_id"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn grok_tool_conversion_normalizes_only_javascript_name() {
        let tool = GrokToolDefinition::function(
            "mcp/server.tool",
            Some("Call it"),
            json!({"type": "object", "properties": {"value": {"type": "string"}}}),
        );
        let converted = to_code_mode_tool_definition(&tool);
        assert_eq!(converted.name, "mcp_server_tool");
        assert_eq!(converted.tool_name, ToolName::plain("mcp/server.tool"));
        assert_eq!(converted.kind, CodeModeToolKind::Function);
        assert_eq!(converted.description, "Call it");
        assert_eq!(converted.input_schema, Some(tool.function.parameters));
    }

    #[test]
    fn direct_model_only_question_and_collaboration_tools_are_not_nested_in_exec() {
        let tools = vec![
            GrokToolDefinition::function(
                "ask_user_question",
                Some("Ask the user"),
                json!({"type": "object"}),
            ),
            GrokToolDefinition::function(
                "read_file",
                Some("Read a file"),
                json!({"type": "object"}),
            ),
            GrokToolDefinition::function(
                "spawn_subagent",
                Some("Launch a subagent"),
                json!({"type": "object"}),
            ),
            GrokToolDefinition::function(
                "get_command_or_subagent_output",
                Some("Read subagent output"),
                json!({"type": "object"}),
            ),
        ];
        let nested = collect_code_mode_tool_definitions(&tools);
        assert_eq!(
            nested
                .iter()
                .map(|definition| definition.tool_name.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read_file"]
        );
        for direct_only in [
            "ask_user_question",
            "request_user_input",
            "task",
            "spawn_subagent",
            "get_task_output",
            "get_command_or_subagent_output",
            "wait_tasks",
            "wait_commands_or_subagents",
            "kill_task",
            "kill_command_or_subagent",
        ] {
            assert!(
                is_code_mode_direct_only_tool(direct_only),
                "{direct_only} must remain model-visible"
            );
        }
        assert!(!is_code_mode_direct_only_tool("read_file"));
    }

    #[test]
    fn codex_code_mode_only_keeps_hosted_web_without_x_search_or_nested_duplicate() {
        let hosted = vec![
            xai_grok_sampling_types::HostedTool::web_search(None),
            xai_grok_sampling_types::HostedTool::XSearch,
        ];
        let effective_hosted = hosted_tools_for_code_mode(
            &hosted,
            xai_grok_sampling_types::ToolMode::CodeModeOnly,
            xai_grok_sampling_types::ModelProvider::Codex,
        );
        assert_eq!(effective_hosted.len(), 1);
        assert!(matches!(
            effective_hosted[0],
            xai_grok_sampling_types::HostedTool::WebSearch { .. }
        ));

        let definitions = vec![
            GrokToolDefinition::function(
                "web_search",
                Some("Local search"),
                json!({"type": "object"}),
            ),
            GrokToolDefinition::function(
                "read_file",
                Some("Read a file"),
                json!({"type": "object"}),
            ),
        ];
        let nested = nested_tool_definitions_for_provider(
            &definitions,
            xai_grok_sampling_types::ModelProvider::Codex,
            &effective_hosted,
        );
        assert_eq!(
            nested
                .iter()
                .map(|definition| definition.function.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read_file"]
        );
    }

    #[test]
    fn xai_code_mode_only_preserves_native_hosted_search() {
        let hosted = vec![
            xai_grok_sampling_types::HostedTool::web_search(None),
            xai_grok_sampling_types::HostedTool::XSearch,
        ];
        let effective = hosted_tools_for_code_mode(
            &hosted,
            xai_grok_sampling_types::ToolMode::CodeModeOnly,
            xai_grok_sampling_types::ModelProvider::Xai,
        );
        assert_eq!(effective.len(), 2);
        assert!(matches!(
            effective[0],
            xai_grok_sampling_types::HostedTool::WebSearch { .. }
        ));
        assert!(matches!(
            effective[1],
            xai_grok_sampling_types::HostedTool::XSearch
        ));

        let definitions = vec![
            GrokToolDefinition::function(
                "web_search",
                Some("Local search"),
                json!({"type": "object"}),
            ),
            GrokToolDefinition::function(
                "read_file",
                Some("Read a file"),
                json!({"type": "object"}),
            ),
        ];
        let nested = nested_tool_definitions_for_provider(
            &definitions,
            xai_grok_sampling_types::ModelProvider::Xai,
            &effective,
        );
        assert_eq!(
            nested
                .iter()
                .map(|definition| definition.function.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read_file"]
        );
    }

    #[test]
    fn apply_patch_uses_codex_freeform_nested_contract() {
        let tool = GrokToolDefinition::function(
            APPLY_PATCH_TOOL_NAME,
            Some("Apply a patch"),
            json!({
                "type": "object",
                "properties": {"patch": {"type": "string"}},
                "required": ["patch"]
            }),
        );
        let converted = to_code_mode_tool_definition(&tool);
        assert_eq!(converted.tool_name, ToolName::plain(APPLY_PATCH_TOOL_NAME));
        assert_eq!(converted.kind, CodeModeToolKind::Freeform);
        assert_eq!(converted.input_schema, None);
    }

    #[test]
    fn wait_arguments_apply_codex_defaults() {
        assert_eq!(
            parse_wait_arguments(r#"{"cell_id":"7"}"#).unwrap(),
            WaitArguments {
                cell_id: "7".to_string(),
                yield_time_ms: 10_000,
                max_tokens: None,
                terminate: false,
            }
        );
        assert_eq!(
            resolve_max_tokens(
                parse_wait_arguments(r#"{"cell_id":"7"}"#)
                    .unwrap()
                    .max_tokens
            ),
            10_000
        );
        assert!(parse_wait_arguments(r#"{"yield_time_ms":1}"#).is_err());
    }

    #[test]
    fn yielded_response_has_exact_status_and_preserves_image_detail() {
        let output = format_runtime_response(
            RuntimeResponse::Yielded {
                cell_id: CellId::new("12".to_string()),
                content_items: vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "hello".to_string(),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,AA==".to_string(),
                        detail: Some(ImageDetail::Original),
                    },
                ],
            },
            None,
            Duration::from_millis(149),
        );

        assert_eq!(
            output.text(),
            "Script running with cell ID 12\nWall time 0.1 seconds\nOutput:\nhello"
        );
        assert_eq!(output.cell_id.as_deref(), Some("12"));
        assert!(output.success);
        assert!(matches!(
            output.content.as_slice(),
            [
                CustomToolOutputContent::Text { .. },
                CustomToolOutputContent::Text { text },
                CustomToolOutputContent::Image {
                    url,
                    detail: CustomToolOutputImageDetail::Original,
                },
            ] if text.as_ref() == "hello" && url.as_ref() == "data:image/png;base64,AA=="
        ));
    }

    #[test]
    fn failed_response_uses_exact_status_and_error_body() {
        let output = format_runtime_response(
            RuntimeResponse::Result {
                cell_id: CellId::new("3".to_string()),
                content_items: Vec::new(),
                error_text: Some("boom".to_string()),
            },
            None,
            Duration::ZERO,
        );
        assert_eq!(
            output.text(),
            "Script failed\nWall time 0.0 seconds\nOutput:\nScript error:\nboom"
        );
        assert!(!output.success);
    }

    #[test]
    fn text_truncation_matches_codex_warning_and_is_utf8_safe() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "0123456789012345678901234567890123456789".to_string(),
        }];
        assert_eq!(
            truncate_code_mode_result(items, 5),
            vec![FunctionCallOutputContentItem::InputText {
                text: concat!(
                    "Warning: truncated output (original token count: 10)\n",
                    "Total output lines: 1\n\n",
                    "0123456789…5 tokens truncated…0123456789"
                )
                .to_string(),
            }]
        );

        // 15 UTF-8 bytes with a 4-byte budget makes the removed-byte count 11,
        // deliberately landing inside a three-byte character.
        let unicode = truncate_middle_with_token_budget("日本語日本", 1);
        assert!(unicode.contains("tokens truncated"));
        assert!(std::str::from_utf8(unicode.as_bytes()).is_ok());
    }
}
