//! Headless single-turn mode (`open-grok -p "prompt"`).
//!
//! Runs the agent in-process via `spawn_grok_shell`, drives the ACP lifecycle
//! (init, auth, session, prompt), streams to stdout, and exits via `CancellationToken`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use agent_client_protocol as acp;
use xai_acp_lib::{AcpAgentTx, AcpClientMessageBox, AcpClientRx, acp_send};
use xai_grok_shell::agent::auth_method::AuthMethodKind;
use xai_grok_shell::agent::config::Config as AgentConfig;
use xai_grok_shell::extensions::task::{CancelSubagentRequest, KillTaskRequest};
use xai_grok_shell::sampling::error::{
    RATE_LIMITED_ERROR_CODE, error_detail_from_data, format_rate_limited_user_message,
};
use xai_grok_shell::sampling::types::{
    REASONING_EFFORT_META_KEY, parse_canonical_effort_token, reasoning_effort_meta_value,
};
use xai_grok_shell::util::config as cli_config;
use xai_grok_telemetry::startup::PendingStartup;

use crate::acp::model_state::{EffortTokenError, ModelState};
use crate::acp::spawn::{AgentShutdownGuard, spawn_grok_shell};
use crate::client_identity::{HEADLESS_CLIENT_TYPE, PAGER_CLIENT_VERSION};
use crate::headless::reducer::{
    Lifecycle, McpServer, Reducer, SessionContext, StreamEvent, TurnEnd, map_session_update,
    reducer_for,
};

mod ext_protocol;
mod reducer;
use ext_protocol::{ExtEvent, handle_ext_notification, reply_headless_ext_method};

mod cli;
pub use cli::{HeadlessPrompt, OutputFormat, parse_json_schema, parse_permission_rules_lenient};
pub(crate) use cli::{ResolvedAgent, resolve_agent_arg};
use cli::{apply_agent_flag, parse_cli_agents, parse_comma_list, parse_permission_rules_strict};

#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    pub session_id: Option<String>,
    pub resume: Option<String>,
    /// Resume was pinned pre-sandbox; materialization must not re-run title selection.
    pub resume_title_pinned: bool,
    pub cwd: Option<PathBuf>,
    pub yolo: bool,
    pub trust: bool,
    pub output_format: OutputFormat,
    /// Emit `stream_event` deltas for `streaming-messages-json`.
    pub include_partial_messages: bool,
    pub json_schema: Option<serde_json::Value>,
    pub model: Option<String>,
    pub rules: Option<String>,
    pub system_prompt_override: Option<String>,
    pub continue_last_session: bool,
    /// Fork on resume/continue (`--fork-session`).
    pub fork_session: bool,
    pub worktree: Option<String>,
    pub restore_code: bool,
    pub agent: Option<String>,
    pub agents_json: Option<String>,
    pub cli_tools: Option<String>,
    pub cli_disallowed_tools: Option<String>,
    pub disable_web_search: bool,
    pub allow_rules: Vec<String>,
    pub deny_rules: Vec<String>,
    pub max_turns: Option<u32>,
    pub permission_mode_flag: Option<String>,
    /// Effort token (`--reasoning-effort` / `--effort`); resolved like `/effort` after models load.
    pub reasoning_effort: Option<String>,
    /// Wait for background tasks to report `task_completed` before exiting (default true).
    pub wait_for_background: bool,
    /// Max time to wait for background quiescence after the first turn ends.
    pub background_wait_timeout: Duration,
}

struct HeadlessEmitter {
    format: OutputFormat,
    parse_structured_output: bool,
    text_buffer: String,
    thought_buffer: String,
    /// Schema-validated output read from the prompt-response `_meta`.
    structured_output: Option<Result<serde_json::Value, String>>,
    usage: Option<serde_json::Value>,
    /// Reducer for the streaming formats; `None` for `plain`/`json`.
    reducer: Option<Box<dyn Reducer>>,
    /// Set when the prompt is sent; the terminal `result.duration_ms` wall-clock.
    prompt_started: Option<Instant>,
    out: std::io::Stdout,
    /// Latched once stdout is unwritable so later writes are dropped instead of panicking.
    output_closed: bool,
    /// First hard stdout IO error (not a broken pipe), surfaced so the process exits non-zero.
    write_error: Option<std::io::Error>,
}

impl HeadlessEmitter {
    fn new(format: OutputFormat, parse_structured_output: bool) -> Self {
        Self {
            format,
            parse_structured_output,
            text_buffer: String::new(),
            thought_buffer: String::new(),
            structured_output: None,
            usage: None,
            reducer: reducer_for(format),
            prompt_started: None,
            out: std::io::stdout(),
            output_closed: false,
            write_error: None,
        }
    }

    /// Checked write to stdout: broken pipe latches a clean stop, any other error is latched and returned.
    fn write_out(&mut self, bytes: &[u8], flush: bool) -> std::io::Result<()> {
        if self.output_closed {
            return Ok(());
        }
        use std::io::Write as _;
        let result = {
            let mut handle = self.out.lock();
            handle
                .write_all(bytes)
                .and_then(|()| if flush { handle.flush() } else { Ok(()) })
        };
        self.record_write_result(result)
    }

    /// Fold a write result into the latches: broken pipe is a clean stop, any other error is surfaced.
    fn record_write_result(&mut self, result: std::io::Result<()>) -> std::io::Result<()> {
        let Err(e) = result else {
            return Ok(());
        };
        self.output_closed = true;
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            tracing::debug!("headless: stdout closed (broken pipe); halting output");
            return Ok(());
        }
        tracing::error!(error = %e, "headless: stdout write failed; halting output");
        if self.write_error.is_none() {
            self.write_error = Some(std::io::Error::new(e.kind(), e.to_string()));
        }
        Err(e)
    }

    /// Take the latched hard stdout error, if any.
    fn take_output_error(&mut self) -> Option<std::io::Error> {
        self.write_error.take()
    }

    /// Emit one compact NDJSON wire line plus newline.
    fn emit_line(&mut self, line: &serde_json::Value) {
        let mut buf = line.to_string();
        buf.push('\n');
        let _ = self.write_out(buf.as_bytes(), false);
    }

    /// Mark the wall-clock start of the run for `result.duration_ms`.
    fn mark_prompt_started(&mut self) {
        self.prompt_started = Some(Instant::now());
    }

    fn duration_ms(&self) -> u64 {
        self.prompt_started
            .map_or(0, |t| t.elapsed().as_millis() as u64)
    }

    /// Emit the reducer preamble once the session context is known.
    fn begin_session(&mut self, ctx: SessionContext) {
        let Some(reducer) = self.reducer.as_mut() else {
            return;
        };
        let lines = reducer.begin(ctx);
        self.emit_lines(lines);
    }

    /// Emit a batch of NDJSON wire lines produced by the reducer.
    fn emit_lines(&mut self, lines: Vec<serde_json::Value>) {
        for line in lines {
            self.emit_line(&line);
        }
    }

    /// Render an `x.ai/*` lifecycle notification for the active format.
    fn on_lifecycle(&mut self, event: Lifecycle) {
        match self.format {
            OutputFormat::Plain => {
                eprintln!("{}", event.plain_message());
            }
            OutputFormat::Json => {}
            OutputFormat::StreamingJson | OutputFormat::StreamingMessagesJson => {
                self.reduce_and_emit(StreamEvent::Lifecycle(event));
            }
        }
    }

    /// Fold one event through the reducer and emit its lines; a no-op for `plain`/`json`.
    fn reduce_and_emit(&mut self, event: StreamEvent) {
        let Some(reducer) = self.reducer.as_mut() else {
            return;
        };
        let lines = reducer.reduce(event);
        self.emit_lines(lines);
    }

    /// Schema output for a terminal line: `Ok`, `Err`, or `None` when not requested.
    fn resolved_structured_output(&self) -> Option<Result<serde_json::Value, String>> {
        if !self.parse_structured_output {
            return None;
        }
        Some(
            self.structured_output
                .clone()
                .unwrap_or_else(|| Err("model did not produce structured output".to_string())),
        )
    }

    /// Read structured output (or its error) from the prompt-response `_meta`.
    fn set_structured_output_from_meta(&mut self, meta: Option<&acp::Meta>) {
        if !self.parse_structured_output {
            return;
        }
        let Some(meta) = meta else { return };
        if let Some(err) = meta.get("structuredOutputError").and_then(|v| v.as_str()) {
            self.structured_output = Some(Err(err.to_string()));
        } else if let Some(value) = meta.get("structuredOutput") {
            self.structured_output = Some(Ok(value.clone()));
        }
    }

    fn set_usage_from_meta(&mut self, meta: Option<&acp::Meta>) {
        let Some(meta) = meta else { return };
        self.usage = meta.get("usage").cloned();
    }

    fn on_text_chunk(&mut self, text: &str) {
        match self.format {
            OutputFormat::Plain => {
                let _ = self.write_out(text.as_bytes(), true);
            }
            OutputFormat::Json => {
                self.text_buffer.push_str(text);
            }
            OutputFormat::StreamingMessagesJson => {
                self.text_buffer.push_str(text);
                self.reduce_and_emit(StreamEvent::AgentMessage(text.to_string()));
            }
            OutputFormat::StreamingJson => {
                self.reduce_and_emit(StreamEvent::AgentMessage(text.to_string()));
            }
        }
    }

    fn on_thought_chunk(&mut self, text: &str) {
        match self.format {
            OutputFormat::Plain => { /* no-op */ }
            OutputFormat::Json => {
                self.thought_buffer.push_str(text);
            }
            OutputFormat::StreamingJson | OutputFormat::StreamingMessagesJson => {
                self.reduce_and_emit(StreamEvent::AgentThought(text.to_string()));
            }
        }
    }

    fn attach_structured_output(&self, target: &mut serde_json::Value) {
        if !self.parse_structured_output {
            return;
        }
        // Only the agent's validated `_meta` output is trusted; never parse the raw text buffer.
        let result = self
            .structured_output
            .clone()
            .unwrap_or_else(|| Err("model did not produce structured output".to_string()));
        crate::headless::reducer::attach_structured_output(target, Some(result));
    }

    /// Final object for `--output-format json`, including spend fields when present.
    fn build_json_result(
        &self,
        stop_reason: &str,
        session_id: &str,
        request_id: &str,
    ) -> serde_json::Value {
        let mut result = serde_json::json!({
            "text": self.text_buffer,
            "stopReason": stop_reason,
            "sessionId": session_id,
            "requestId": request_id
        });
        if !self.thought_buffer.is_empty() {
            result["thought"] = serde_json::Value::String(self.thought_buffer.clone());
        }
        if let Some(usage) = &self.usage {
            attach_result_usage(&mut result, usage);
        }
        self.attach_structured_output(&mut result);
        result
    }

    fn on_end(&mut self, stop_reason: &str, session_id: &str, request_id: &str) {
        match self.format {
            OutputFormat::Plain => {
                let _ = self.write_out(b"\n", false);
            }
            OutputFormat::Json => {
                let result = self.build_json_result(stop_reason, session_id, request_id);
                let mut rendered =
                    serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
                rendered.push('\n');
                let _ = self.write_out(rendered.as_bytes(), false);
            }
            OutputFormat::StreamingJson | OutputFormat::StreamingMessagesJson => {
                let usage = self.usage.clone();
                let structured_output = self.resolved_structured_output();
                let result_text = self.text_buffer.clone();
                let duration_ms = self.duration_ms();
                let lines = self.reducer.as_mut().map(|reducer| {
                    let end = TurnEnd {
                        stop_reason,
                        session_id,
                        request_id,
                        usage: usage.as_ref(),
                        structured_output,
                        result_text: result_text.as_str(),
                        duration_ms,
                    };
                    reducer.finish(&end)
                });
                if let Some(lines) = lines {
                    self.emit_lines(lines);
                }
            }
        }
    }

    /// Emit the max turns marker for the active format.
    fn on_max_turns(&mut self) {
        match self.format {
            OutputFormat::Plain => eprintln!("Max turns reached"),
            // Conveyed by `stopReason` in the terminal JSON and result.
            OutputFormat::Json => {}
            OutputFormat::StreamingJson | OutputFormat::StreamingMessagesJson => {
                let lines = self.reducer.as_mut().map(|reducer| reducer.max_turns());
                if let Some(lines) = lines {
                    self.emit_lines(lines);
                }
            }
        }
    }

    /// Emit the terminal error; `stop_reason_override` stamps a Messages stop reason (e.g. `max_tokens`).
    fn on_error(&mut self, message: &str, stop_reason_override: Option<&str>) {
        match self.format {
            OutputFormat::Plain => eprintln!("{message}"),
            OutputFormat::Json => {
                let mut err = serde_json::json!({"type":"error","message": message});
                if let Some(usage) = &self.usage {
                    attach_result_usage(&mut err, usage);
                }
                self.emit_line(&err);
            }
            OutputFormat::StreamingJson | OutputFormat::StreamingMessagesJson => {
                let usage = self.usage.clone();
                let duration_ms = self.duration_ms();
                let lines = self.reducer.as_mut().map(|reducer| {
                    reducer.error(message, usage.as_ref(), duration_ms, stop_reason_override)
                });
                if let Some(lines) = lines {
                    self.emit_lines(lines);
                }
            }
        }
    }
}

pub(crate) fn attach_result_usage(result: &mut serde_json::Value, usage: &serde_json::Value) {
    xai_grok_shell::extensions::notification::attach_result_usage_fail_closed(result, usage);
}

/// Snake_case wire token for an ACP stop reason.
fn stop_reason_wire(reason: acp::StopReason) -> String {
    match reason {
        acp::StopReason::EndTurn => "end_turn",
        acp::StopReason::MaxTokens => "max_tokens",
        acp::StopReason::MaxTurnRequests => "max_turn_requests",
        acp::StopReason::Refusal => "refusal",
        acp::StopReason::Cancelled => "cancelled",
        // Fail loud on an unknown future variant, then degrade to `end_turn`.
        other => {
            tracing::warn!(
                stop_reason = ?other,
                "headless: unknown ACP StopReason; defaulting wire token to end_turn"
            );
            "end_turn"
        }
    }
    .to_string()
}

/// Configured MCP servers for the `init` line; all report `"connected"` (status is not resolved here).
fn mcp_server_names(cwd: &Path) -> Vec<McpServer> {
    let servers =
        cli_config::load_mcp_servers(cwd, &xai_grok_tools::types::compat::CompatConfig::default());
    servers
        .iter()
        .filter_map(|s| {
            let name = match s {
                acp::McpServer::Http(h) => h.name.clone(),
                acp::McpServer::Sse(h) => h.name.clone(),
                acp::McpServer::Stdio(h) => h.name.clone(),
                _ => return None,
            };
            Some(McpServer {
                name,
                status: "connected".to_string(),
            })
        })
        .collect()
}

fn auto_respond_to_permissions(
    args: &acp::RequestPermissionRequest,
    option_kinds: &[acp::PermissionOptionKind],
) -> Option<acp::RequestPermissionResponse> {
    for &option_kind in option_kinds {
        for option in &args.options {
            if option.kind == option_kind {
                return Some(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                        option.option_id.clone(),
                    )),
                ));
            }
        }
    }
    None
}

/// "Not signed in" error message, tailored to the session type.
fn auth_required_message(interactive: bool) -> String {
    if interactive {
        "Not signed in. Run `open-grok login` to authenticate \
         (or `open-grok login --device-code` if no browser is available)."
            .to_string()
    } else {
        "Not signed in. To authenticate without a browser, run:\n  \
         open-grok login --device-code\n\n\
         Alternatively, set the XAI_API_KEY environment variable \
         or run `open-grok login` on a machine with a browser."
            .to_string()
    }
}

/// Resolve non-xAI provider-local authentication before the legacy ACP/xAI
/// auth gate runs. Provider-owned credentials intentionally stay out of ACP
/// auth-method ordering.
fn selected_provider_auth(
    config: &AgentConfig,
    requested_model: Option<&str>,
    prefer_credentialed_slug: bool,
) -> Option<anyhow::Result<bool>> {
    let model_id = requested_model.or(config.models.default.as_deref())?;
    let models = xai_grok_shell::agent::config::resolve_model_list(config, None);
    let exact = models.get(model_id);
    let credentialed_slug = models
        .values()
        .rev()
        .find(|entry| entry.info().model == model_id && entry.has_own_credentials());
    let named = models
        .values()
        .find(|entry| entry.info().name.as_deref() == Some(model_id));
    let matching_slug = models.values().find(|entry| entry.info().model == model_id);
    let exact_has_provider_auth = exact.is_some_and(|entry| {
        if entry.has_own_credentials() {
            return true;
        }
        match entry.info().provider.profile().session_auth {
            // xAI still uses the legacy ACP auth gate below. Keep its exact
            // row stable rather than trying to pre-authorize an alias here.
            xai_grok_shell::sampling::types::BuiltInSessionAuthKind::XaiSession => true,
            xai_grok_shell::sampling::types::BuiltInSessionAuthKind::ApiKeyOnly => false,
            xai_grok_shell::sampling::types::BuiltInSessionAuthKind::CodexOAuth => {
                xai_grok_shell::agent::config::resolve_credentials(entry, None)
                    .api_key
                    .is_some()
            }
        }
    });
    // A persisted session stores the routing slug, which may also be the key
    // of an embedded OAuth row. If a configured BYOK alias carries that same
    // slug, it is the selectable row when OAuth is absent and must win for
    // resume. Explicit CLI/profile keys keep exact-key semantics.
    let model = if prefer_credentialed_slug && !exact_has_provider_auth {
        credentialed_slug.or(exact).or(named).or(matching_slug)
    } else {
        exact.or(named).or(matching_slug)
    }?;
    match model.info().provider.profile().session_auth {
        xai_grok_shell::sampling::types::BuiltInSessionAuthKind::XaiSession => None,
        xai_grok_shell::sampling::types::BuiltInSessionAuthKind::ApiKeyOnly => {
            Some(if model.has_own_credentials() {
                Ok(true)
            } else {
                Err(anyhow::anyhow!(
                    "The selected provider requires an API key. Configure `api_key` or `env_key` \
                     on this model."
                ))
            })
        }
        xai_grok_shell::sampling::types::BuiltInSessionAuthKind::CodexOAuth => {
            if model.has_own_credentials() {
                return Some(Ok(true));
            }
            let credentials = xai_grok_shell::agent::config::resolve_credentials(model, None);
            Some(if credentials.api_key.is_some() {
                Ok(false)
            } else {
                Err(anyhow::anyhow!(
                    "Not signed in to Codex. Run `open-grok login --codex --device-auth` for a \
                         headless login, or configure an API key on this model."
                ))
            })
        }
    }
}

/// Authenticate via the agent's `defaultAuthMethodId`, failing closed when none is available.
/// Returns whether the selected method is API-key auth.
async fn authenticate(
    acp_tx: &AcpAgentTx,
    auths: &[acp::AuthMethod],
    default_auth_method_id: Option<&acp::AuthMethodId>,
) -> anyhow::Result<bool> {
    let method_id = crate::acp::select_eager_auth_method(auths, default_auth_method_id)
        .ok_or_else(|| {
            use std::io::IsTerminal;
            let interactive = std::io::stdin().is_terminal()
                && !xai_grok_shell::util::clipboard::is_remote_session();
            anyhow::anyhow!("{}", auth_required_message(interactive))
        })?;
    let kind = AuthMethodKind::from_id(&method_id);
    // Prefer non-interactive methods only; interactive login is not usable headless.
    if kind.needs_interactive_login() {
        use std::io::IsTerminal;
        let interactive =
            std::io::stdin().is_terminal() && !xai_grok_shell::util::clipboard::is_remote_session();
        anyhow::bail!("{}", auth_required_message(interactive));
    }
    let is_api_key_auth = kind.is_api_key();
    let _resp: acp::AuthenticateResponse = acp_send(
        acp::AuthenticateRequest::new(method_id)
            .meta(serde_json::json!({"headless": true}).as_object().cloned()),
        acp_tx,
    )
    .await?;
    Ok(is_api_key_auth)
}

fn build_headless_init_request(
    rules: Option<&str>,
    system_prompt_override: Option<&str>,
) -> acp::InitializeRequest {
    let mut meta = serde_json::json!({
        "clientType": HEADLESS_CLIENT_TYPE,
        "clientVersion": PAGER_CLIENT_VERSION,
    });
    if let Some(rules) = rules {
        meta["rules"] = serde_json::json!(rules);
    }
    if let Some(system_prompt_override) = system_prompt_override {
        meta["systemPromptOverride"] = serde_json::json!(system_prompt_override);
    }
    meta["startupHints"] = serde_json::json!({
        "nonInteractive": true,
        "skipGitStatus": true,
        "skipProjectLayout": true,
    });

    acp::InitializeRequest::new(acp::ProtocolVersion::V1)
        .client_capabilities(
            acp::ClientCapabilities::new()
                .fs(acp::FileSystemCapabilities::new())
                .terminal(false),
        )
        .meta(meta.as_object().cloned())
}

struct OpenedSession {
    session_id: acp::SessionId,
    models: ModelState,
    /// Directory the session is anchored to (launch cwd, resume `original_cwd`, or fork `write_cwd`).
    cwd: PathBuf,
}

async fn open_session(
    acp_tx: &AcpAgentTx,
    cwd: &Path,
    session_id_flag: Option<&str>,
    restore_code: Option<bool>,
    initial_model_id: Option<&str>,
) -> anyhow::Result<OpenedSession> {
    // Pager opens sessions before the agent resolves per-vendor compat;
    // default (all-on) preserves existing behavior — the agent applies
    // the resolved config once the session is live.
    let mcp_servers =
        cli_config::load_mcp_servers(cwd, &xai_grok_tools::types::compat::CompatConfig::default());

    if let Some(sid) = session_id_flag {
        let try_load: Result<acp::LoadSessionResponse, _> = acp_send(
            acp::LoadSessionRequest::new(acp::SessionId::new(sid.to_string()), cwd.to_path_buf())
                .mcp_servers(mcp_servers.clone())
                .meta({
                    let mut m = acp::Meta::new();
                    m.insert("noReplay".into(), serde_json::Value::Bool(true));
                    if let Some(rc) = restore_code {
                        m.insert("x.ai/restore_code".into(), serde_json::Value::Bool(rc));
                    }
                    if let Some(model_id) = initial_model_id {
                        m.insert(
                            "modelId".into(),
                            serde_json::Value::String(model_id.to_string()),
                        );
                    }
                    Some(m)
                }),
            acp_tx,
        )
        .await;
        if let Ok(resp) = try_load {
            return Ok(OpenedSession {
                session_id: acp::SessionId::new(sid.to_string()),
                models: ModelState::from(resp.models),
                cwd: cwd.to_path_buf(),
            });
        }
        anyhow::bail!("Session does not exist");
    }

    let mut request = acp::NewSessionRequest::new(cwd.to_path_buf()).mcp_servers(mcp_servers);
    if let Some(model_id) = initial_model_id {
        request = request.meta(
            serde_json::json!({ "modelId": model_id })
                .as_object()
                .cloned(),
        );
    }
    let new_resp: acp::NewSessionResponse = acp_send(request, acp_tx).await?;
    Ok(OpenedSession {
        session_id: new_resp.session_id,
        models: ModelState::from(new_resp.models),
        cwd: cwd.to_path_buf(),
    })
}

async fn open_session_with_id(
    acp_tx: &AcpAgentTx,
    cwd: &Path,
    session_id: &str,
    initial_model_id: Option<&str>,
) -> anyhow::Result<OpenedSession> {
    let cwd_str = cwd.to_string_lossy();
    crate::app::session_startup::ensure_session_id_available(session_id, &cwd_str)?;
    let mcp_servers =
        cli_config::load_mcp_servers(cwd, &xai_grok_tools::types::compat::CompatConfig::default());
    let mut meta = serde_json::Map::from_iter([(
        "sessionId".to_string(),
        serde_json::Value::String(session_id.to_string()),
    )]);
    if let Some(model_id) = initial_model_id {
        meta.insert(
            "modelId".to_string(),
            serde_json::Value::String(model_id.to_string()),
        );
    }
    let new_resp: acp::NewSessionResponse = acp_send(
        acp::NewSessionRequest::new(cwd.to_path_buf())
            .mcp_servers(mcp_servers)
            .meta(Some(meta)),
        acp_tx,
    )
    .await?;
    Ok(OpenedSession {
        session_id: new_resp.session_id,
        models: ModelState::from(new_resp.models),
        cwd: cwd.to_path_buf(),
    })
}

async fn fork_then_open(
    acp_tx: &AcpAgentTx,
    launch_cwd: &Path,
    parent_id: &str,
    parent_cwd: Option<&Path>,
    new_id: Option<&str>,
    restore_code: Option<bool>,
    initial_model_id: Option<&str>,
) -> anyhow::Result<OpenedSession> {
    use crate::app::session_startup::{
        effective_fork_new_cwd, ensure_session_id_available, fork_response_error,
        fork_response_new_session_id, fork_session_params, parent_session_is_worktree,
    };
    let launch_cwd_str = launch_cwd.to_string_lossy().into_owned();
    // Align with interactive: child lands under parent session cwd when the
    // parent was resolved from another directory (`newCwd` = parent_cwd).
    let new_cwd_str = effective_fork_new_cwd(&launch_cwd_str, parent_cwd);
    let write_cwd = PathBuf::from(&new_cwd_str);
    if let Some(nid) = new_id {
        ensure_session_id_available(nid, &new_cwd_str)?;
    }
    let parent_is_worktree = parent_session_is_worktree(parent_id, &write_cwd);
    let payload = fork_session_params(parent_id, &write_cwd, new_id, parent_is_worktree);
    let req = acp::ExtRequest::new(
        "x.ai/session/fork",
        serde_json::value::to_raw_value(&payload)
            .expect("serialize fork params")
            .into(),
    );
    let resp = acp_send(req, acp_tx).await?;
    if let Some(err) = fork_response_error(resp.0.get()) {
        anyhow::bail!("fork failed: {err}");
    }
    let child = fork_response_new_session_id(resp.0.get())
        .ok_or_else(|| anyhow::anyhow!("fork response missing newSessionId"))?;
    match open_session(
        acp_tx,
        &write_cwd,
        Some(&child),
        restore_code,
        initial_model_id,
    )
    .await
    {
        Ok(opened) => Ok(opened),
        Err(e) => Err(anyhow::anyhow!(
            "fork succeeded as {child} but load failed: {e}"
        )),
    }
}

/// Apply `-m` / effort after session open (via `resolve_effort_for_model`, then
/// SetSessionModel).
///
/// Headless maps the classified [`EffortTokenError`] differently from the TUI: a
/// one-shot run soft-ignores effort on a non-supporting model (still applying
/// `-m`) but hard-fails on a genuinely unknown token. The TUI instead keeps the
/// `-m` switch and only toasts — intentional, since headless has no scrollback
/// to carry a non-fatal warning.
async fn apply_headless_model_and_effort(
    acp_tx: &AcpAgentTx,
    session_id: &acp::SessionId,
    models: &ModelState,
    model_name: Option<&str>,
    effort_token: Option<&str>,
) -> anyhow::Result<()> {
    if model_name.is_none() && effort_token.is_none() {
        return Ok(());
    }

    let model_id = if let Some(name) = model_name {
        models
            .resolve_by_name_or_id(name)
            .unwrap_or_else(|| acp::ModelId::new(name))
    } else {
        models.current.clone().ok_or_else(|| {
            anyhow::anyhow!("--effort/--reasoning-effort: no active model to apply effort to")
        })?
    };

    let effort = match effort_token {
        None => None,
        // Pre-catalog: the canonical token was already stamped into the agent
        // config; a remapped menu id can't resolve without a loaded catalog.
        Some(token) if models.available.is_empty() => {
            if parse_canonical_effort_token(token).is_none() {
                // Do not hardcode a level list here: without a catalog we cannot
                // know what the model offers, and advertising none/minimal/… has
                // led users to try values that then 400 on the API.
                anyhow::bail!(
                    "--effort/--reasoning-effort: unknown effort level '{token}' \
                     (model catalog unavailable; remapped menu ids require a loaded catalog)"
                );
            }
            None
        }
        Some(token) => match models.resolve_effort_for_model(&model_id, token) {
            Ok(effort) => Some(effort),
            // Soft-ignore effort on a non-supporting model; still apply `-m`.
            Err(EffortTokenError::Unsupported) => {
                tracing::warn!(
                    model = %model_id.0,
                    token,
                    "--effort/--reasoning-effort: model does not support reasoning effort; ignoring"
                );
                None
            }
            Err(err) => anyhow::bail!("--effort/--reasoning-effort: {}", err.message()),
        },
    };

    // Nothing to apply (effort pre-stamped or ignored, and no model override):
    // skip the no-op SetSessionModel.
    if model_name.is_none() && effort.is_none() {
        return Ok(());
    }

    let meta = effort.map(|eff| {
        let mut m = acp::Meta::new();
        m.insert(
            REASONING_EFFORT_META_KEY.to_string(),
            reasoning_effort_meta_value(eff),
        );
        m
    });

    acp_send(
        acp::SetSessionModelRequest::new(session_id.clone(), model_id.clone()).meta(meta),
        acp_tx,
    )
    .await
    .map_err(|e| {
        if let Some(name) = model_name {
            anyhow::anyhow!(
                "Couldn't set model '{}': {}. Run 'open-grok models' to see available models.",
                name,
                e
            )
        } else {
            anyhow::anyhow!("Couldn't apply reasoning effort: {e}")
        }
    })?;
    tracing::debug!(
        model_id = %model_id.0,
        effort = ?effort,
        "headless: model/effort set"
    );
    Ok(())
}

/// Startup-materialization context for headless (`-p`) runs. Never chat:
/// `HeadlessOptions` carries no chat flag, so headless resume targets are
/// always disk/GCS Build sessions. `--worktree` is ignored here: headless
/// never creates a worktree, so remote miss must not take `DeferToWorktree`.
fn headless_materialize_ctx(
    resume_title_pinned: bool,
    restore_code: bool,
) -> crate::app::session_startup::MaterializeCtx {
    crate::app::session_startup::MaterializeCtx {
        has_worktree: false,
        allow_remote_restore:
            crate::app::session_startup::MaterializeCtx::default_allow_remote_restore(),
        chat_mode: false,
        title_resolution: if resume_title_pinned {
            crate::app::session_startup::TitleResolution::PinnedPreSandbox
        } else {
            crate::app::session_startup::TitleResolution::Allowed
        },
        restore_code,
        restore_progress_on_stdout: false,
    }
}

fn persisted_summary_by_session_id(
    session_id: &str,
) -> Option<xai_grok_shell::session::persistence::Summary> {
    let session_dir = xai_grok_shell::session::persistence::find_session_dir_by_id(session_id)?;
    let summary = std::fs::read(session_dir.join("summary.json")).ok()?;
    serde_json::from_slice(&summary).ok()
}

/// Run a headless single-turn prompt: spawn the agent, drive the ACP lifecycle, stream to stdout.
pub async fn run_single_turn(
    prompt: HeadlessPrompt,
    verbatim: bool,
    options: HeadlessOptions,
) -> Result<()> {
    // Stamp proxy requests as headless before the agent issues its first request.
    xai_grok_shell::http::set_process_client_mode_headless();

    let cwd = match options.cwd {
        None => std::env::current_dir()?,
        Some(ref p) => dunce::canonicalize(p)?,
    };

    let mut emitter = HeadlessEmitter::new(options.output_format, options.json_schema.is_some());

    if options.include_partial_messages
        && options.output_format != OutputFormat::StreamingMessagesJson
    {
        eprintln!(
            "warning: --include-partial-messages only affects --output-format streaming-messages-json; ignoring it"
        );
    }

    // Load config and spawn agent
    let t_spawn = Instant::now();
    let raw_config = xai_grok_shell::config::load_effective_config()
        .map_err(|e| anyhow::anyhow!("Failed to load config: {e}"))?;
    let mut agent_config = AgentConfig::new_from_toml_cfg(&raw_config)
        .map_err(|e| anyhow::anyhow!("Failed to create agent config: {e}"))?;

    // Canonical-only early stamp; remaps need the post-session catalog resolve below.
    if let Some(ref token) = options.reasoning_effort
        && let Some(effort) = parse_canonical_effort_token(token)
    {
        agent_config.reasoning_effort_override = Some(effort);
    }
    // So initial system prompt / `system_prompt_label` use `-m`, not a later SetSessionModel.
    if let Some(ref model) = options.model {
        agent_config.default_model_override = Some(model.clone());
    }

    agent_config.resolve_runtime_fields(&xai_grok_shell::agent::config::RuntimeResolutionContext {
        raw_config: &raw_config,
        remote_settings: None,
        is_headless: true,
        cli_subagents: None,
        cli_web_search_model: None,
        cli_session_summary_model: None,
        cli_experimental_memory: false,
        cli_no_memory: false,
        disable_web_search: options.disable_web_search,
        todo_gate: false,
        laziness_debug_log: None,
        storage_mode: None,
    });

    agent_config.mode = xai_grok_shell::agent::config::AgentMode::Headless;
    agent_config.default_yolo_mode = options.yolo;
    // Remote arg is None: the remote settings permission_mode soft-default is
    // TUI-only; headless runs must not change permission behavior on a
    // remote flag flip.
    agent_config.default_auto_mode = xai_grok_shell::util::config::effective_auto_for_launch(
        options.yolo,
        options.permission_mode_flag.as_deref(),
        None,
    );

    apply_agent_flag(&options.agent, &mut agent_config);

    if let Some(ref json) = options.agents_json {
        agent_config.cli_agents = parse_cli_agents(json)?;
    }

    let mut pending_startup = Some(PendingStartup::new());
    let timer = xai_grok_telemetry::startup::begin(crate::acp::Owner::Client);
    let mut report_startup_failure = |timer: &crate::acp::StartupTimer| {
        timer.emit_telemetry(
            crate::acp::AgentKind::Embedded,
            crate::acp::StartupOutcome::Error,
            None,
            false,
        );
        PendingStartup::finish_held(&mut pending_startup, crate::acp::StartupOutcome::Error);
    };

    // Materialize resume/fork intent before authentication so provider auth is
    // chosen from the persisted session model, not an unrelated configured
    // default. This remains the same startup plan consumed below.
    use crate::app::session_startup::{self, MaterializedStartup, SessionStartupFlags};
    let has_resume_id = options.resume.as_deref().filter(|s| !s.is_empty());
    let resume_most_recent = options.resume.as_deref() == Some("");
    let intent = session_startup::session_startup_intent_from_flags(SessionStartupFlags {
        session_id: options.session_id.as_deref(),
        resume_session_id: has_resume_id,
        resume_most_recent,
        continue_last_session: options.continue_last_session,
        fork_session: options.fork_session,
        // Headless never creates a worktree from `-w`.
        has_worktree: false,
    })
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let cwd_str = cwd.to_string_lossy().to_string();
    let materialized = session_startup::materialize_startup_for_cwd(
        headless_materialize_ctx(options.resume_title_pinned, options.restore_code),
        intent,
        &cwd_str,
    )
    .await
    .inspect_err(|_| {
        report_startup_failure(&timer);
    })?;
    let persisted_model = match &materialized {
        MaterializedStartup::Resume { session_id, .. } => {
            persisted_summary_by_session_id(session_id)
                .map(|summary| summary.current_model_id.0.to_string())
        }
        MaterializedStartup::Fork {
            parent_session_id, ..
        } => persisted_summary_by_session_id(parent_session_id)
            .map(|summary| summary.current_model_id.0.to_string()),
        MaterializedStartup::NewAuto | MaterializedStartup::NewWithId { .. } => None,
    };
    let base_auth_model = options.model.as_deref().or(persisted_model.as_deref());
    let resolved_models = xai_grok_shell::agent::config::resolve_model_list(&agent_config, None);
    let base_entry = base_auth_model
        .or(agent_config.models.default.as_deref())
        .and_then(|model_id| {
            resolved_models.get(model_id).or_else(|| {
                resolved_models
                    .values()
                    .find(|entry| entry.info().model == model_id)
            })
        });
    let agent_definition = xai_grok_shell::agent::MvpAgent::resolve_agent_definition(
        &cwd,
        agent_config.agent_profile_path.as_deref(),
        &agent_config.agent,
        None,
        base_entry.map(|entry| entry.info().agent_type.as_str()),
    );
    let profile_model = match &agent_definition.model {
        xai_grok_agent::config::ModelOverride::Override(model_id)
            if resolved_models.contains_key(model_id)
                || resolved_models
                    .values()
                    .any(|entry| entry.info().model == *model_id) =>
        {
            Some(model_id.as_str())
        }
        _ => None,
    };
    let explicit_initial_model = options.model.as_deref().and_then(|requested| {
        resolved_models
            .get_key_value(requested)
            .map(|(key, _)| key.as_str())
            .or_else(|| {
                resolved_models.iter().find_map(|(key, entry)| {
                    (entry.info().model == requested
                        || entry.info().name.as_deref() == Some(requested))
                    .then_some(key.as_str())
                })
            })
    });
    let auth_model = profile_model.or(explicit_initial_model).or(base_auth_model);
    let prefer_persisted_credentials = options.model.is_none()
        && profile_model.is_none()
        && explicit_initial_model.is_none()
        && persisted_model.is_some();
    let provider_auth_override =
        selected_provider_auth(&agent_config, auth_model, prefer_persisted_credentials);

    agent_config.cli_agent_overrides = xai_grok_shell::agent::config::CliAgentOverrides {
        tools: parse_comma_list(options.cli_tools.as_deref()),
        disallowed_tools: parse_comma_list(options.cli_disallowed_tools.as_deref()),
        permission_rules: parse_permission_rules_strict(&options.allow_rules, &options.deny_rules)?,
        max_turns: options.max_turns,
        permission_mode: options
            .permission_mode_flag
            .as_deref()
            .map(|s| {
                serde_json::from_value(serde_json::Value::String(s.to_string()))
                    .map_err(|e| anyhow::anyhow!("--permission-mode: invalid value: {e}"))
            })
            .transpose()?,
    };

    // Persist an explicit --trust grant before the agent starts.
    if options.trust {
        xai_grok_shell::agent::folder_trust::grant_folder_trust(&cwd);
    }

    let cancel = CancellationToken::new();
    let memory_config = agent_config.memory_config.clone();
    let spawned = match spawn_grok_shell(agent_config, &cancel, memory_config).await {
        Ok(s) => s,
        Err(e) => {
            report_startup_failure(&timer);
            let msg = format!("Couldn't start session: {e}");
            emitter.on_error(&msg, None);
            anyhow::bail!("{msg}");
        }
    };
    // Cancel + join on every return path (success or bail).
    let _agent_guard = AgentShutdownGuard::new(cancel.clone(), Some(spawned.thread_handle));
    let (acp_tx, mut acp_rx) = (spawned.channel.tx, spawned.channel.rx);
    crate::unified_log::init(acp_tx.clone());
    crate::unified_log::info(
        "pager started",
        None,
        Some(serde_json::json!({"mode": "headless"})),
    );
    crate::unified_log::flush();

    let init_req = build_headless_init_request(
        options.rules.as_deref(),
        options.system_prompt_override.as_deref(),
    );
    xai_grok_telemetry::startup::enter(crate::acp::StartupPhase::AcpInitialize);
    let init_resp: acp::InitializeResponse = match acp_send(init_req, &acp_tx).await {
        Ok(r) => r,
        Err(e) => {
            report_startup_failure(&timer);
            let msg = format!("Couldn't initialize: {e}");
            emitter.on_error(&msg, None);
            anyhow::bail!("{msg}");
        }
    };
    tracing::debug!(
        elapsed_ms = t_spawn.elapsed().as_millis() as u64,
        "headless: spawn + initialize complete"
    );

    // Authenticate. Non-xAI providers resolve their own auth before the
    // legacy ACP gate (`provider_auth_override`); xAI uses the agent's
    // `defaultAuthMethodId` (preferred_method pin).
    let t_auth = Instant::now();
    xai_grok_telemetry::startup::enter(crate::acp::StartupPhase::EagerAuth);
    let default_auth_method_id = crate::acp::parse_default_auth_method_id(init_resp.meta.as_ref());
    let auth_result = match provider_auth_override {
        Some(result) => result,
        None => {
            authenticate(
                &acp_tx,
                &init_resp.auth_methods,
                default_auth_method_id.as_ref(),
            )
            .await
        }
    };
    let is_api_key_auth = match auth_result {
        Ok(is_api_key) => is_api_key,
        Err(e) => {
            report_startup_failure(&timer);
            emitter.on_error(&e.to_string(), None);
            return Err(e);
        }
    };
    tracing::debug!(
        elapsed_ms = t_auth.elapsed().as_millis() as u64,
        "headless: authenticate complete"
    );
    // Connect ends here; session phases stay out of the phase histogram.
    timer.emit_telemetry(
        crate::acp::AgentKind::Embedded,
        crate::acp::StartupOutcome::Ok,
        None,
        false,
    );

    // Open session (reuse early materialization; honor suppress_code_restore).

    let restore_code = match &materialized {
        MaterializedStartup::Resume {
            suppress_code_restore: true,
            ..
        }
        | MaterializedStartup::Fork {
            suppress_code_restore: true,
            ..
        } => Some(false),
        _ => options.restore_code.then_some(true),
    };
    let t_session = Instant::now();
    xai_grok_telemetry::startup::enter(crate::acp::StartupPhase::SessionCreate);
    let opened = match materialized {
        MaterializedStartup::NewAuto => {
            open_session(&acp_tx, &cwd, None, None, explicit_initial_model).await
        }
        MaterializedStartup::NewWithId { session_id } => {
            open_session_with_id(&acp_tx, &cwd, &session_id, explicit_initial_model).await
        }
        MaterializedStartup::Resume {
            session_id,
            original_cwd,
            ..
        } => {
            let load_cwd = original_cwd.as_deref().unwrap_or(cwd.as_path());
            open_session(
                &acp_tx,
                load_cwd,
                Some(session_id.as_str()),
                restore_code,
                explicit_initial_model,
            )
            .await
        }
        MaterializedStartup::Fork {
            parent_session_id,
            parent_cwd,
            new_session_id,
            ..
        } => {
            fork_then_open(
                &acp_tx,
                &cwd,
                &parent_session_id,
                parent_cwd.as_deref(),
                new_session_id.as_deref(),
                restore_code,
                explicit_initial_model,
            )
            .await
        }
    };
    let OpenedSession {
        session_id,
        models: session_models,
        cwd: session_cwd,
    } = match opened {
        Ok(v) => v,
        Err(e) => {
            PendingStartup::finish_held(&mut pending_startup, crate::acp::StartupOutcome::Error);
            let msg = format!("Couldn't create session: {e}");
            emitter.on_error(&msg, None);
            anyhow::bail!("{msg}");
        }
    };
    PendingStartup::finish_held(&mut pending_startup, crate::acp::StartupOutcome::Ok);
    tracing::debug!(
        elapsed_ms = t_session.elapsed().as_millis() as u64,
        session_id = %session_id.0,
        "headless: open_session complete"
    );

    // Debug: track headless sessions in active_sessions.json when env var is set.
    let track_active = std::env::var("GROK_TRACK_HEADLESS").is_ok();
    if track_active {
        let _ = xai_grok_shell::active_sessions::register(
            xai_grok_shell::active_sessions::ActiveSession {
                session_id: session_id.clone(),
                pid: std::process::id(),
                cwd: cwd.display().to_string(),
                opened_at: chrono::Utc::now(),
            },
        );
    }

    // Seed the reducer's session context BEFORE applying model/effort so a later failure carries it.
    {
        let model = options
            .model
            .clone()
            .or_else(|| session_models.current_model_name());
        let permission_mode = options
            .permission_mode_flag
            .clone()
            .or_else(|| options.yolo.then(|| "bypassPermissions".to_string()));
        emitter.begin_session(SessionContext {
            session_id: session_id.0.to_string(),
            model,
            cwd: session_cwd.to_string_lossy().to_string(),
            permission_mode,
            mcp_servers: mcp_server_names(&session_cwd),
            include_partial_messages: options.include_partial_messages,
            api_key_auth: is_api_key_auth,
            context_window: session_models.get_context_window(),
        });
    }

    // One bounded catalog read covers what the session catalog cannot resolve.
    let effort_unresolved = |token: &str| {
        if parse_canonical_effort_token(token).is_some() {
            return false;
        }
        let target = options
            .model
            .as_deref()
            .and_then(|m| session_models.resolve_by_name_or_id(m))
            .or_else(|| session_models.current.clone());
        match target {
            Some(model_id) => matches!(
                session_models.resolve_effort_for_model(&model_id, token),
                Err(EffortTokenError::UnknownToken { .. } | EffortTokenError::NoActiveModel)
            ),
            None => true,
        }
    };
    let needs_fresh_catalog = options
        .model
        .as_deref()
        .is_some_and(|m| session_models.resolve_by_name_or_id(m).is_none())
        || options
            .reasoning_effort
            .as_deref()
            .is_some_and(effort_unresolved);
    let session_models = if needs_fresh_catalog {
        match xai_grok_shell::cli_models::fetch_model_state(&acp_tx).await {
            Ok(state) => ModelState::from(Some(state)),
            Err(e) => {
                tracing::warn!(error = %e, "headless: model catalog refresh failed; using session state");
                session_models
            }
        }
    } else {
        session_models
    };

    if let Err(e) = apply_headless_model_and_effort(
        &acp_tx,
        &session_id,
        &session_models,
        options.model.as_deref(),
        options.reasoning_effort.as_deref(),
    )
    .await
    {
        let msg = e.to_string();
        emitter.on_error(&msg, None);
        anyhow::bail!("{msg}");
    }

    // Send prompt and stream response
    let prompt_blocks = prompt.into_content_blocks();

    let prompt_meta = {
        let mut meta = serde_json::Map::new();
        if verbatim {
            meta.insert("verbatim".to_string(), serde_json::Value::Bool(true));
        }
        if let Some(ref schema) = options.json_schema {
            meta.insert("outputSchema".to_string(), schema.clone());
        }
        // Screen-mode telemetry (`prompt_submitted.screen_mode`): headless is
        // its own mode, distinct from the TUI's fullscreen/inline/minimal.
        meta.insert(
            "screenMode".to_string(),
            serde_json::Value::String("headless".to_string()),
        );
        Some(meta)
    };

    let request = acp::PromptRequest::new(session_id.clone(), prompt_blocks).meta(prompt_meta);
    let t_prompt = Instant::now();
    emitter.mark_prompt_started();
    let mut ttf_logged = false;
    let mut prompt_fut = Box::pin(acp_send(request, &acp_tx));
    let mut prompt_result = None;
    // Tracked regardless of wait_for_background so the exit reaper always sees running work.
    let mut pending_bg: HashSet<BackgroundWork> = HashSet::new();
    // Tombstone of completed ids so an out-of-order backgrounded never re-arms them.
    let mut completed_bg: HashSet<BackgroundWork> = HashSet::new();
    let mut prompt_done_at: Option<Instant> = None;
    // On mid-turn channel close, break (not bail) so the exit path still drains and reaps.
    let mut connection_closed = false;

    loop {
        if emitter.write_error.is_some() {
            tracing::warn!("headless: stdout write failed; stopping the stream loop");
            break;
        }
        // Drain buffered ACP first: PromptResponse can complete while task_backgrounded is still queued.
        if options.wait_for_background && prompt_result.is_some() && pending_bg.is_empty() {
            drain_pending_acp_messages(
                &mut acp_rx,
                &mut emitter,
                t_prompt,
                &mut ttf_logged,
                options.yolo,
                &mut pending_bg,
                &mut completed_bg,
            );
            if pending_bg.is_empty() {
                tracing::debug!("headless: no pending background tasks, exiting");
                break;
            }
        }

        // Safety valve so evals don't hang on long-lived monitors or stuck tasks.
        if options.wait_for_background
            && let Some(done_at) = prompt_done_at
            && done_at.elapsed() >= options.background_wait_timeout
        {
            tracing::warn!(
                pending_bg = pending_bg.len(),
                timeout_secs = options.background_wait_timeout.as_secs(),
                "headless: background wait timed out, exiting"
            );
            break;
        }

        // Only needed while waiting on tasks (timeout enforcement); otherwise
        // the loop blocks on ACP until task_completed or PromptResponse.
        let timeout_deadline = if options.wait_for_background
            && prompt_result.is_some()
            && !pending_bg.is_empty()
            && let Some(done_at) = prompt_done_at
        {
            let remaining = options
                .background_wait_timeout
                .saturating_sub(done_at.elapsed());
            if remaining.is_zero() {
                Duration::from_millis(50)
            } else {
                remaining
            }
        } else {
            Duration::from_secs(3600)
        };

        tokio::select! {
            biased;
            msg = acp_rx.recv() => {
                let Some(msg) = msg else {
                    emitter.on_error("Connection closed unexpectedly", None);
                    connection_closed = true;
                    break;
                };
                handle_headless_acp_message(
                    msg.boxed(),
                    &mut emitter,
                    t_prompt,
                    &mut ttf_logged,
                    options.yolo,
                    &mut pending_bg,
                    &mut completed_bg,
                );
            }
            res = &mut prompt_fut, if prompt_result.is_none() => {
                prompt_result = Some(res);
                prompt_done_at = Some(Instant::now());
                if !options.wait_for_background {
                    // PromptResponse and its final notifications travel on
                    // different channels. Give notifications a short bounded
                    // grace window so a just-behind lifecycle/text update is
                    // not silently lost at process exit.
                    drain_acp_with_grace(
                        &mut acp_rx,
                        Duration::from_millis(750),
                        &mut emitter,
                        t_prompt,
                        &mut ttf_logged,
                        options.yolo,
                        &mut pending_bg,
                        &mut completed_bg,
                    )
                    .await;
                    break;
                }
                // With wait_for_background: keep draining ACP for task_completed.
                drain_pending_acp_messages(
                    &mut acp_rx,
                    &mut emitter,
                    t_prompt,
                    &mut ttf_logged,
                    options.yolo,
                    &mut pending_bg,
                    &mut completed_bg,
                );
            }
            _ = tokio::time::sleep(timeout_deadline), if options.wait_for_background
                && prompt_result.is_some()
                && !pending_bg.is_empty() =>
            {
                // Wake to re-check background_wait_timeout at the top of the loop.
            }
        }
    }

    // Final drain-to-empty so the reaper sees work buffered right at exit (the timeout path skips draining).
    drain_pending_acp_messages(
        &mut acp_rx,
        &mut emitter,
        t_prompt,
        &mut ttf_logged,
        options.yolo,
        &mut pending_bg,
        &mut completed_bg,
    );

    // Kill background tasks/subagents still pending at exit (background-wait
    // timeout or --no-wait-for-background) so they don't outlive the process.
    if !pending_bg.is_empty() {
        tracing::warn!(
            pending_bg = pending_bg.len(),
            "headless: killing background work still pending at exit"
        );
        reap_pending_background_tasks(&pending_bg, &session_id, &acp_tx).await;
    }

    // Flush buffered unified log entries before exit.
    crate::unified_log::flush_blocking().await;

    if track_active {
        // Non-blocking flock so a slow/network ~/.opengrok can't hang exit.
        let _ = xai_grok_shell::active_sessions::try_unregister(&session_id);
    }
    // A mid-turn ACP close already reaped above; return that error before the normal outcome.
    if connection_closed {
        anyhow::bail!("Connection closed unexpectedly");
    }
    let outcome: Result<()> = match prompt_result {
        Some(Ok(resp)) => {
            let stop_reason = stop_reason_wire(resp.stop_reason);
            emitter.set_structured_output_from_meta(resp.meta.as_ref());
            emitter.set_usage_from_meta(resp.meta.as_ref());
            // Prefer the response `_meta` ids, falling back to the typed session id rather than "".
            let sid = resp
                .meta
                .as_ref()
                .and_then(|m| m.get("sessionId"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| session_id.0.as_ref());
            let rid = match resp
                .meta
                .as_ref()
                .and_then(|m| m.get("requestId"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                Some(r) => r,
                None => {
                    tracing::warn!(
                        "headless: prompt response carried no requestId; emitting an empty requestId"
                    );
                    ""
                }
            };
            let is_max_turns = resp
                .meta
                .as_ref()
                .and_then(|m| m.get("cancellationCategory"))
                .and_then(|v| v.as_str())
                == Some("max_turns_reached");
            if is_max_turns {
                emitter.on_max_turns();
                emitter.on_end(&stop_reason, sid, rid);
                Err(anyhow::anyhow!("max turns reached"))
            } else {
                emitter.on_end(&stop_reason, sid, rid);
                Ok(())
            }
        }
        Some(Err(err)) => {
            let msg = if i32::from(err.code) == RATE_LIMITED_ERROR_CODE {
                let detail = err.data.as_ref().and_then(error_detail_from_data);
                crate::app::sanitize_user_error(&format_rate_limited_user_message(
                    detail.as_deref(),
                    is_api_key_auth,
                ))
            } else {
                err.to_string()
            };
            if let Some(usage) = xai_grok_shell::sampling::error::prompt_usage_from_error(&err) {
                match serde_json::to_value(&usage) {
                    Ok(v) => emitter.usage = Some(v),
                    // Log rather than swallow: a serialize failure would drop the frozen spend fields.
                    Err(e) => tracing::warn!(
                        error = %e,
                        "headless: failed to serialize prompt-error usage; spend fields dropped"
                    ),
                }
            }
            let stop_reason_override =
                (xai_grok_shell::sampling::error::stop_reason_for_turn_error(&err) == "MaxTokens")
                    .then_some("max_tokens");
            emitter.on_error(&msg, stop_reason_override);
            Err(anyhow::anyhow!("{msg}"))
        }
        None => Ok(()),
    };

    // A hard stdout write error outranks the normal outcome: output is dead, so exit non-zero.
    if let Some(err) = emitter.take_output_error() {
        return Err(anyhow::Error::new(err).context("headless: stdout write failed"));
    }
    outcome
}

/// Background work tracked for exit: bash/monitor tasks and background subagents, keyed by id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum BackgroundWork {
    Task(String),
    Subagent(String),
}

/// Ext request that kills one unit of background work (subagent cancel or task kill).
fn reap_request_for_work(
    work: &BackgroundWork,
    session_id: &acp::SessionId,
) -> serde_json::Result<acp::ExtRequest> {
    let (method, params) = match work {
        BackgroundWork::Subagent(id) => (
            "x.ai/subagent/cancel",
            serde_json::value::to_raw_value(&CancelSubagentRequest {
                subagent_id: id.clone(),
            })?,
        ),
        BackgroundWork::Task(id) => (
            "x.ai/task/kill",
            serde_json::value::to_raw_value(&KillTaskRequest {
                session_id: session_id.0.to_string(),
                task_id: id.clone(),
                source: xai_grok_shell::extensions::task::TaskKillSource::Teardown,
            })?,
        ),
    };
    Ok(acp::ExtRequest::new(method, params.into()))
}

/// Best-effort kill of background work still pending when headless exits
/// (background-wait timeout or `--no-wait-for-background`) so model-spawned
/// processes never outlive the process. Failures are logged, never fatal.
async fn reap_pending_background_tasks(
    pending_bg: &HashSet<BackgroundWork>,
    session_id: &acp::SessionId,
    acp_tx: &AcpAgentTx,
) {
    for work in pending_bg {
        let request = match reap_request_for_work(work, session_id) {
            Ok(request) => request,
            Err(e) => {
                tracing::warn!(?work, error = %e, "headless: failed to build reap request");
                continue;
            }
        };
        let method = request.method.clone();
        match tokio::time::timeout(Duration::from_secs(10), acp_send(request, acp_tx)).await {
            Ok(Ok(_)) => {
                tracing::debug!(?work, %method, "headless: reaped pending background work")
            }
            Ok(Err(e)) => {
                tracing::warn!(?work, %method, error = %e, "headless: failed to reap background work")
            }
            Err(_) => {
                tracing::warn!(?work, %method, "headless: timed out reaping background work")
            }
        }
    }
}

/// Track a background lifecycle event. `completed_bg` tombstones finished ids so a late or
/// out-of-order backgrounded/spawned cannot resurrect them into `pending_bg`.
fn track_background_lifecycle(
    event: ExtEvent,
    pending_bg: &mut HashSet<BackgroundWork>,
    completed_bg: &mut HashSet<BackgroundWork>,
) {
    match event {
        ExtEvent::TaskBackgrounded {
            task_id,
            is_monitor,
        } => {
            let work = BackgroundWork::Task(task_id);
            if completed_bg.contains(&work) {
                tracing::debug!(
                    is_monitor,
                    "headless: ignoring task_backgrounded for already-completed task"
                );
            } else {
                pending_bg.insert(work);
                tracing::debug!(
                    pending = pending_bg.len(),
                    is_monitor,
                    "headless: tracking background task"
                );
            }
        }
        ExtEvent::TaskCompleted { task_id } => {
            let work = BackgroundWork::Task(task_id);
            let was_pending = pending_bg.remove(&work);
            completed_bg.insert(work);
            if was_pending {
                tracing::debug!(
                    pending = pending_bg.len(),
                    "headless: background task completed"
                );
            }
        }
        ExtEvent::SubagentSpawned { subagent_id } => {
            let work = BackgroundWork::Subagent(subagent_id);
            if completed_bg.contains(&work) {
                tracing::debug!(
                    "headless: ignoring subagent_spawned for already-finished subagent"
                );
            } else {
                pending_bg.insert(work);
                tracing::debug!(
                    pending = pending_bg.len(),
                    "headless: tracking background subagent"
                );
            }
        }
        ExtEvent::SubagentFinished { subagent_id } => {
            let work = BackgroundWork::Subagent(subagent_id);
            let was_pending = pending_bg.remove(&work);
            completed_bg.insert(work);
            if was_pending {
                tracing::debug!(
                    pending = pending_bg.len(),
                    "headless: background subagent finished"
                );
            }
        }
        // Routed to the emitter by the caller, never tracked.
        ExtEvent::MonitorEvent | ExtEvent::None | ExtEvent::Lifecycle(_) | ExtEvent::Stream(_) => {}
    }
}

/// Non-blocking drain-to-empty of `acp_rx`, so background work buffered around prompt
/// completion is recorded in `pending_bg` before the empty-check decides whether to exit.
#[allow(clippy::too_many_arguments)]
fn drain_pending_acp_messages(
    acp_rx: &mut AcpClientRx,
    emitter: &mut HeadlessEmitter,
    t_prompt: Instant,
    ttf_logged: &mut bool,
    yolo: bool,
    pending_bg: &mut HashSet<BackgroundWork>,
    completed_bg: &mut HashSet<BackgroundWork>,
) {
    while let Ok(msg) = acp_rx.try_recv() {
        handle_headless_acp_message(
            msg.boxed(),
            emitter,
            t_prompt,
            ttf_logged,
            yolo,
            pending_bg,
            completed_bg,
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_acp_with_grace(
    acp_rx: &mut AcpClientRx,
    grace: Duration,
    emitter: &mut HeadlessEmitter,
    t_prompt: Instant,
    ttf_logged: &mut bool,
    yolo: bool,
    pending_bg: &mut HashSet<BackgroundWork>,
    completed_bg: &mut HashSet<BackgroundWork>,
) {
    let deadline = Instant::now() + grace;
    loop {
        while let Ok(msg) = acp_rx.try_recv() {
            handle_headless_acp_message(
                msg.boxed(),
                emitter,
                t_prompt,
                ttf_logged,
                yolo,
                pending_bg,
                completed_bg,
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::select! {
            biased;
            msg = acp_rx.recv() => {
                let Some(msg) = msg else { break; };
                handle_headless_acp_message(
                    msg.boxed(),
                    emitter,
                    t_prompt,
                    ttf_logged,
                    yolo,
                    pending_bg,
                    completed_bg,
                );
            }
            _ = tokio::time::sleep(remaining) => {
                break;
            }
        }
    }
}

/// Process one inbound ACP client message; shared by `recv()` and `try_recv()`.
#[allow(clippy::too_many_arguments)]
fn handle_headless_acp_message(
    msg: AcpClientMessageBox,
    emitter: &mut HeadlessEmitter,
    t_prompt: Instant,
    ttf_logged: &mut bool,
    yolo: bool,
    pending_bg: &mut HashSet<BackgroundWork>,
    completed_bg: &mut HashSet<BackgroundWork>,
) {
    match msg {
        AcpClientMessageBox::SessionNotification(boxed) => {
            match &boxed.request.update {
                acp::SessionUpdate::AgentMessageChunk(chunk) => {
                    if let acp::ContentBlock::Text(text) = &chunk.content
                        && !text.text.is_empty()
                    {
                        if !*ttf_logged {
                            *ttf_logged = true;
                            tracing::debug!(
                                elapsed_ms = t_prompt.elapsed().as_millis() as u64,
                                "headless: time-to-first-chunk"
                            );
                        }
                        emitter.on_text_chunk(&text.text);
                    }
                }
                acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                    if let acp::ContentBlock::Text(text) = &chunk.content
                        && !text.text.is_empty()
                    {
                        if !*ttf_logged {
                            *ttf_logged = true;
                            tracing::debug!(
                                elapsed_ms = t_prompt.elapsed().as_millis() as u64,
                                "headless: time-to-first-thought"
                            );
                        }
                        emitter.on_thought_chunk(&text.text);
                    }
                }
                acp::SessionUpdate::ToolCall(_)
                | acp::SessionUpdate::ToolCallUpdate(_)
                | acp::SessionUpdate::Plan(_)
                | acp::SessionUpdate::AvailableCommandsUpdate(_) => {
                    if let Some(event) = map_session_update(&boxed.request.update) {
                        emitter.reduce_and_emit(event);
                    }
                }
                _ => {}
            }
            let _ = boxed.response_tx.send(Ok(()));
        }
        AcpClientMessageBox::RequestPermission(req) => {
            if yolo {
                if let Some(resp) = auto_respond_to_permissions(
                    &req.request,
                    &[
                        acp::PermissionOptionKind::AllowOnce,
                        acp::PermissionOptionKind::AllowAlways,
                    ],
                ) {
                    let _ = req.response_tx.send(Ok(resp));
                } else {
                    let _ = req.response_tx.send(Ok(acp::RequestPermissionResponse::new(
                        acp::RequestPermissionOutcome::Cancelled,
                    )));
                }
            } else {
                let _ = req.response_tx.send(Ok(acp::RequestPermissionResponse::new(
                    acp::RequestPermissionOutcome::Cancelled,
                )));
            }
        }
        AcpClientMessageBox::ExtNotification(notif) => {
            let event = handle_ext_notification(&notif);
            let _ = notif.response_tx.send(Ok(()));
            match event {
                ExtEvent::Lifecycle(l) => emitter.on_lifecycle(l),
                ExtEvent::Stream(event) => emitter.reduce_and_emit(*event),
                other => track_background_lifecycle(other, pending_bg, completed_bg),
            }
        }
        AcpClientMessageBox::WaitForTerminalExit(args) => {
            args.response_tx
                .send(Err(crate::acp::wait_for_exit_not_supported(
                    "headless mode",
                )))
                .ok();
        }
        AcpClientMessageBox::ExtMethod(args) => reply_headless_ext_method(args),
        _ => {}
    }
}

#[cfg(test)]
#[path = "headless_tests.rs"]
mod tests;
