//! `workflow` — deterministic multi-subagent orchestration driven by a
//! JavaScript script.
//!
//! The script runs in the code-mode V8 runtime (fresh isolate per run) with a
//! prelude that provides `agent()`, `parallel()`, `pipeline()`, `phase()`,
//! `log()`, `args`, `meta`, and `budget`. Every `agent()` call becomes a real
//! foreground subagent spawned through the shared [`SubagentBackend`]; the
//! script owns control flow (loops, fan-out, barriers) while the host owns
//! concurrency capping, journaling, progress, and cancellation.
//!
//! Layering mirrors `agent_swarm`: the tool runs at depth 0, children run at
//! depth 1 with `task`/`agent_swarm`/`workflow` stripped, so the tree stays
//! flat. Children carry the parent prompt id, so turn-cancel sweeps them, and
//! swarm cohort metadata (swarm id = run id) groups them in the TUI.

pub mod host;
pub mod meta;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;
use xai_grok_code_mode::InProcessCodeModeSession;
use xai_grok_code_mode_protocol::{
    CodeModeSession, CodeModeToolKind, ExecuteRequest, FunctionCallOutputContentItem,
    RuntimeResponse, ToolDefinition as CodeModeToolDefinition, ToolName as CodeModeToolName,
    WaitRequest,
};
use xai_tool_types::WorkflowToolInput;

use crate::implementations::grok_build::task::MAX_SUBAGENT_DEPTH;
use crate::implementations::grok_build::task::backend::SubagentBackendResource;
use crate::implementations::grok_build::task::types::{
    CurrentPromptIdResource, SessionIdResource, SubagentDepthCounter, TaskModelValidator,
};
use crate::types::output::ToolOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::resources::{NotificationHandle, SessionFolder};
use crate::types::tool::{ToolKind, ToolNamespace};
use host::{
    ReplayPlan, ResumeBoundary, ResumeMode, WORKFLOW_AGENT_NESTED_TOOL, WorkflowHost,
    WorkflowHostConfig,
};
use meta::SplitScript;

/// JS prelude installed ahead of every workflow script body.
const WORKFLOW_PRELUDE: &str = include_str!("prelude.js");

/// Marker the prelude prefixes onto the encoded script return value.
const RETURN_MARKER: &str = "__WF_RETURN__";

/// Upper bound on workflow script size.
const MAX_SCRIPT_BYTES: usize = 512 * 1024;

/// Cadence of the completion-polling `wait` loop. Progress streams through
/// notifications independently, so this only bounds cancellation latency of
/// the observer (cancellation itself is select!-ed against the token).
const WAIT_YIELD_MS: u64 = 30_000;

/// Default per-agent wall-clock timeout (mirrors `agent_swarm`).
const DEFAULT_AGENT_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Debug, Default)]
pub struct WorkflowTool;

impl crate::types::tool_metadata::ToolMetadata for WorkflowTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Workflow
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        concat!(
            "Execute a JavaScript workflow script that orchestrates many subagents ",
            "deterministically. Use workflow when control flow across agents should be code ",
            "(loops, fan-out, barriers, verification passes) rather than model-driven tool ",
            "calls: parallel review with adversarial verification, migrate-every-file ",
            "pipelines, loop-until-dry discovery, judged multi-attempt design.\n\n",
            "Every script must begin with `export const meta = { name, description, phases? }` ",
            "as a pure literal (phases: [{ title, detail? }]). The body runs top-level in an ",
            "async context — use await directly, and `return` a JSON-serializable value as the ",
            "workflow result.\n\n",
            "Script hooks:\n",
            "- agent(prompt, opts?) -> Promise: spawn a subagent and await its final text. ",
            "opts: {label, phase, schema, model, effort, isolation: 'worktree', agentType}. ",
            "With schema (a JSON Schema), the agent must reply with matching JSON and the call ",
            "resolves to the parsed value (one corrective retry is applied). Returns null when ",
            "the agent fails or is cancelled — filter with .filter(Boolean). Throws on invalid ",
            "options or an exhausted token budget.\n",
            "- parallel(thunks) -> Promise<any[]>: run tasks concurrently and await all; a ",
            "thunk that throws resolves to null.\n",
            "- pipeline(items, ...stages) -> Promise<any[]>: run each item through all stages ",
            "independently with NO barrier between stages; each stage receives (prev, ",
            "originalItem, index); a stage that throws drops that item to null. Prefer ",
            "pipeline over sequential parallel calls.\n",
            "- phase(title): group subsequent agents under a progress phase. log(message): ",
            "emit a progress line.\n",
            "- args: the tool input `args` value, verbatim. meta: the parsed meta object. ",
            "budget: {total, spent(), remaining()} tracking child token usage against ",
            "token_budget.\n\n",
            "Scripts are plain JavaScript (no TypeScript, no imports). Date.now(), ",
            "Math.random(), and argless new Date() throw — pass timestamps via args and vary ",
            "prompts by item index; runs are journaled and resumable via resume_from_run_id ",
            "(unchanged agent() calls replay instantly from the journal; prefer script_path ",
            "over regenerating the script so prompts stay identical). To resume an EDITED ",
            "script, set resume_mode=\"positional\" (replay by call position). To go back to a ",
            "specific point, set resume_through to a phase title, agent label, or call index — ",
            "results through that point replay, everything after re-runs. Concurrent agents ",
            "are capped and queue transparently; at most 1000 agents per run. agentType ",
            "accepts the same types as the task tool. workflow must be the only tool call in ",
            "the model response, and workflow agents cannot spawn further task, agent_swarm, ",
            "or workflow calls."
        )
    }

    fn emitted_notifications(&self) -> &'static [&'static str] {
        &["WorkflowProgress"]
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::Value(ToolRequirement::tool_kind(ToolKind::Task))
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl xai_tool_runtime::Tool for WorkflowTool {
    type Args = WorkflowToolInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("workflow").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "workflow",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: WorkflowToolInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let cwd = crate::types::tool_metadata::resolve_cwd(&ctx, &resources).await?;
        let (
            depth,
            backend,
            model_validator,
            parent_session_id,
            parent_prompt_id,
            session_folder,
            notifications,
        ) = {
            let res = resources.lock().await;
            (
                res.get::<SubagentDepthCounter>().map(|d| d.0).unwrap_or(0),
                res.get::<SubagentBackendResource>()
                    .ok_or_else(|| {
                        xai_tool_runtime::ToolError::custom(
                            "missing_resource",
                            "SubagentBackendResource (subagent support not initialized)",
                        )
                    })?
                    .clone(),
                res.get::<TaskModelValidator>().cloned(),
                res.get::<SessionIdResource>()
                    .map(|s| s.0.clone())
                    .unwrap_or_default(),
                res.get::<CurrentPromptIdResource>()
                    .map(|p| p.0.clone())
                    .filter(|id| !id.is_empty()),
                res.get::<SessionFolder>().map(|f| f.0.clone()),
                res.get::<NotificationHandle>()
                    .map(|handle| handle.0.clone())
                    .unwrap_or_default(),
            )
        };
        if depth >= MAX_SUBAGENT_DEPTH {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Subagent depth limit exceeded (current depth: {depth}, max: \
                 {MAX_SUBAGENT_DEPTH}). workflow cannot run inside a subagent."
            )));
        }

        let source = load_script(&input, &cwd)?;
        let SplitScript { meta, body } =
            meta::split_script(&source).map_err(xai_tool_runtime::ToolError::invalid_arguments)?;

        let run_id = uuid::Uuid::now_v7().to_string();
        let run_dir = session_folder
            .as_deref()
            .map(|folder| folder.join("workflows").join(&run_id));
        let script_path = persist_script(run_dir.as_deref(), &source);
        let journal_path = run_dir.as_deref().map(|dir| dir.join("journal.jsonl"));

        let resume_mode = parse_resume_mode(input.resume_mode.as_deref())
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        let resume_boundary = parse_resume_boundary(input.resume_through.as_ref())
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        if input.resume_from_run_id.is_none()
            && (resume_mode != ResumeMode::Exact || resume_boundary.is_some())
        {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "resume_mode / resume_through require resume_from_run_id",
            ));
        }
        let replay = match input.resume_from_run_id.as_deref() {
            Some(resume_id) => {
                let entries = load_resume_journal(session_folder.as_deref(), resume_id)?;
                ReplayPlan::build(entries, resume_mode, resume_boundary)
                    .map_err(xai_tool_runtime::ToolError::invalid_arguments)?
            }
            None => ReplayPlan::default(),
        };

        let cancellation = ctx
            .extensions
            .get::<xai_tool_runtime::Cancellation>()
            .map(|c| c.0.clone())
            .unwrap_or_default();

        let host = WorkflowHost::new(WorkflowHostConfig {
            backend: backend.0.clone(),
            model_validator,
            parent_session_id,
            parent_prompt_id,
            run_id: run_id.clone(),
            workflow_name: meta.name.clone(),
            tool_call_id: ctx.call_id.to_string(),
            notifications,
            concurrency: workflow_concurrency_from_env()
                .map_err(xai_tool_runtime::ToolError::invalid_arguments)?,
            per_agent_timeout: agent_timeout_from_env()
                .map_err(xai_tool_runtime::ToolError::invalid_arguments)?,
            token_budget: input.token_budget,
            journal_path,
            replay,
        });

        let module = assemble_module(&meta.value, input.args.as_ref(), input.token_budget, &body);
        let outcome = drive_script(host.clone(), &ctx, module, &cancellation).await?;

        Ok(ToolOutput::Text(
            render_output(
                &meta,
                &run_id,
                script_path.as_deref(),
                host.as_ref(),
                &outcome,
            )
            .into(),
        ))
    }
}

/// Terminal state of one script execution.
struct ScriptOutcome {
    /// JSON-encoded script return value (absent when the script errored or
    /// returned nothing recoverable).
    return_value: Option<JsonValue>,
    /// JS error text (message + stack) when the script threw.
    error_text: Option<String>,
}

async fn drive_script(
    host: Arc<WorkflowHost>,
    ctx: &xai_tool_runtime::ToolCallContext,
    module: String,
    cancellation: &CancellationToken,
) -> Result<ScriptOutcome, xai_tool_runtime::ToolError> {
    let session: Arc<dyn CodeModeSession> = Arc::new(InProcessCodeModeSession::with_delegate(host));
    let mut shutdown_guard = SessionShutdownGuard {
        session: Some(session.clone()),
    };

    let request = ExecuteRequest {
        tool_call_id: ctx.call_id.to_string(),
        enabled_tools: vec![CodeModeToolDefinition {
            name: WORKFLOW_AGENT_NESTED_TOOL.to_string(),
            tool_name: CodeModeToolName::plain(WORKFLOW_AGENT_NESTED_TOOL),
            description: "workflow host agent spawn (internal)".to_string(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            output_schema: None,
        }],
        source: module,
        yield_time_ms: Some(WAIT_YIELD_MS),
        max_output_tokens: None,
    };

    let execute_error = |error: String| {
        xai_tool_runtime::ToolError::custom(
            "workflow_runtime",
            format!("workflow runtime: {error}"),
        )
    };

    let started = session.execute(request).await.map_err(execute_error)?;
    let cell_id = started.cell_id.clone();

    let mut content_items: Vec<FunctionCallOutputContentItem> = Vec::new();
    let mut response = tokio::select! {
        response = started.initial_response() => response.map_err(execute_error)?,
        _ = cancellation.cancelled() => {
            let _ = session.terminate(cell_id).await;
            return finish_cancelled(&mut shutdown_guard, session).await;
        }
    };
    let (final_items, error_text) = loop {
        match response {
            RuntimeResponse::Result {
                content_items: items,
                error_text,
                ..
            } => break (items, error_text),
            RuntimeResponse::Terminated {
                content_items: items,
                ..
            } => {
                content_items.extend(items);
                break (Vec::new(), Some("workflow cell was terminated".to_string()));
            }
            RuntimeResponse::Yielded {
                cell_id,
                content_items: items,
            } => {
                content_items.extend(items);
                response = tokio::select! {
                    outcome = session.wait(WaitRequest {
                        cell_id: cell_id.clone(),
                        yield_time_ms: WAIT_YIELD_MS,
                    }) => outcome.map_err(execute_error)?.into(),
                    _ = cancellation.cancelled() => {
                        let _ = session.terminate(cell_id).await;
                        return finish_cancelled(&mut shutdown_guard, session).await;
                    }
                };
            }
        }
    };
    content_items.extend(final_items);

    shutdown_guard.disarm();
    let _ = session.shutdown().await;

    let return_value = content_items.iter().rev().find_map(|item| match item {
        FunctionCallOutputContentItem::InputText { text } => text
            .strip_prefix(RETURN_MARKER)
            .and_then(|encoded| serde_json::from_str::<JsonValue>(encoded).ok()),
        _ => None,
    });

    Ok(ScriptOutcome {
        return_value,
        error_text,
    })
}

async fn finish_cancelled(
    guard: &mut SessionShutdownGuard,
    session: Arc<dyn CodeModeSession>,
) -> Result<ScriptOutcome, xai_tool_runtime::ToolError> {
    guard.disarm();
    let _ = session.shutdown().await;
    Ok(ScriptOutcome {
        return_value: None,
        error_text: Some("workflow cancelled".to_string()),
    })
}

/// Shuts the code-mode session down even when the tool future is dropped
/// mid-run (turn cancellation drops tool futures).
struct SessionShutdownGuard {
    session: Option<Arc<dyn CodeModeSession>>,
}

impl SessionShutdownGuard {
    fn disarm(&mut self) {
        self.session = None;
    }
}

impl Drop for SessionShutdownGuard {
    fn drop(&mut self) {
        if let Some(session) = self.session.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                let _ = session.shutdown().await;
            });
        }
    }
}

fn load_script(
    input: &WorkflowToolInput,
    cwd: &Path,
) -> Result<String, xai_tool_runtime::ToolError> {
    let source = match (&input.script, &input.script_path) {
        (Some(script), None) => script.clone(),
        (None, Some(path)) => {
            let resolved = if Path::new(path).is_absolute() {
                PathBuf::from(path)
            } else {
                cwd.join(path)
            };
            std::fs::read_to_string(&resolved).map_err(|error| {
                xai_tool_runtime::ToolError::invalid_arguments(format!(
                    "cannot read script_path `{}`: {error}",
                    resolved.display()
                ))
            })?
        }
        (Some(_), Some(_)) => {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "provide either `script` or `script_path`, not both",
            ));
        }
        (None, None) => {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(
                "provide a workflow `script` (or `script_path`)",
            ));
        }
    };
    if source.len() > MAX_SCRIPT_BYTES {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "workflow script is {} bytes; the maximum is {MAX_SCRIPT_BYTES}",
            source.len()
        )));
    }
    Ok(source)
}

fn persist_script(run_dir: Option<&Path>, source: &str) -> Option<PathBuf> {
    let run_dir = run_dir?;
    std::fs::create_dir_all(run_dir).ok()?;
    let path = run_dir.join("script.js");
    std::fs::write(&path, source).ok()?;
    Some(path)
}

fn load_resume_journal(
    session_folder: Option<&Path>,
    resume_id: &str,
) -> Result<Vec<host::JournalEntry>, xai_tool_runtime::ToolError> {
    if resume_id.is_empty()
        || !resume_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "invalid resume_from_run_id `{resume_id}`"
        )));
    }
    let Some(folder) = session_folder else {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(
            "resume_from_run_id requires session persistence, which is unavailable here",
        ));
    };
    let journal = folder
        .join("workflows")
        .join(resume_id)
        .join("journal.jsonl");
    if !journal.is_file() {
        return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
            "no journal found for run `{resume_id}` (expected {})",
            journal.display()
        )));
    }
    Ok(host::load_journal_entries(&journal))
}

fn parse_resume_mode(raw: Option<&str>) -> Result<ResumeMode, String> {
    match raw.map(str::trim) {
        None | Some("") | Some("exact") => Ok(ResumeMode::Exact),
        Some("positional") => Ok(ResumeMode::Positional),
        Some(other) => Err(format!(
            "invalid resume_mode `{other}` (expected \"exact\" or \"positional\")"
        )),
    }
}

fn parse_resume_boundary(raw: Option<&JsonValue>) -> Result<Option<ResumeBoundary>, String> {
    match raw {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::Number(number)) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(|index| Some(ResumeBoundary::Index(index)))
            .ok_or_else(|| format!("invalid resume_through index `{number}`")),
        Some(JsonValue::String(text)) if !text.trim().is_empty() => {
            Ok(Some(ResumeBoundary::Point(text.clone())))
        }
        Some(other) => Err(format!(
            "invalid resume_through `{other}` (expected a phase title, agent label, or call index)"
        )),
    }
}

/// Assemble the executable ES module: prelude, injected globals, then the
/// user body wrapped in an async main whose return value is re-emitted as a
/// marked `text()` content item.
fn assemble_module(
    meta_value: &JsonValue,
    args: Option<&JsonValue>,
    token_budget: Option<u64>,
    body: &str,
) -> String {
    let args_json = serde_json::to_string(args.unwrap_or(&JsonValue::Null))
        .unwrap_or_else(|_| "null".to_string());
    let meta_json = serde_json::to_string(meta_value).unwrap_or_else(|_| "{}".to_string());
    let budget_json = match token_budget {
        Some(total) => total.to_string(),
        None => "null".to_string(),
    };
    format!(
        "{WORKFLOW_PRELUDE}\n\
         const args = {args_json};\n\
         const meta = {meta_json};\n\
         __wf.budgetTotal = {budget_json};\n\
         const __wf_main = async () => {{\n\
         {body}\n\
         }};\n\
         text(__wf_encodeReturn(await __wf_main()));\n"
    )
}

fn workflow_concurrency_from_env() -> Result<usize, String> {
    match std::env::var("OPENGROK_WORKFLOW_MAX_CONCURRENCY") {
        Ok(raw) => {
            let parsed: usize = raw
                .trim()
                .parse()
                .map_err(|_| format!("invalid OPENGROK_WORKFLOW_MAX_CONCURRENCY `{raw}`"))?;
            if parsed == 0 {
                return Err("OPENGROK_WORKFLOW_MAX_CONCURRENCY must be at least 1".to_string());
            }
            Ok(parsed)
        }
        Err(_) => {
            let cores = std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(8);
            Ok(cores.saturating_sub(2).clamp(2, 16))
        }
    }
}

fn agent_timeout_from_env() -> Result<Option<Duration>, String> {
    match std::env::var("OPENGROK_SUBAGENT_TIMEOUT_MS") {
        Ok(raw) => {
            let parsed: u64 = raw
                .trim()
                .parse()
                .map_err(|_| format!("invalid OPENGROK_SUBAGENT_TIMEOUT_MS `{raw}`"))?;
            Ok((parsed > 0).then(|| Duration::from_millis(parsed)))
        }
        Err(_) => Ok(Some(DEFAULT_AGENT_TIMEOUT)),
    }
}

fn render_output(
    meta: &meta::WorkflowMeta,
    run_id: &str,
    script_path: Option<&Path>,
    host: &WorkflowHost,
    outcome: &ScriptOutcome,
) -> String {
    let mut out = String::new();
    out.push_str("<workflow_result>\n");
    out.push_str(&format!("<name>{}</name>\n", meta.name));
    out.push_str(&format!("<run_id>{run_id}</run_id>\n"));
    if let Some(path) = script_path {
        out.push_str(&format!("<script_path>{}</script_path>\n", path.display()));
    }
    let status = if outcome.error_text.is_some() {
        "failed"
    } else {
        "completed"
    };
    out.push_str(&format!("<status>{status}</status>\n"));

    let (agents, log_tail) = host.snapshot(|state| {
        let agents = state
            .agents
            .iter()
            .map(|agent| {
                let status = match agent.status {
                    host::AgentStatus::Running => "running",
                    host::AgentStatus::Done => "done",
                    host::AgentStatus::Failed => "failed",
                    host::AgentStatus::Cached => "cached",
                };
                let phase = agent
                    .phase
                    .as_deref()
                    .map(|phase| format!(" phase=\"{phase}\""))
                    .unwrap_or_default();
                let detail = agent
                    .detail
                    .as_deref()
                    .map(|detail| format!(" — {detail}"))
                    .unwrap_or_default();
                format!(
                    "  <agent index=\"{}\" status=\"{status}\"{phase}>{}{detail}</agent>",
                    agent.index, agent.label
                )
            })
            .collect::<Vec<_>>();
        (agents, state.render_tail())
    });
    out.push_str(&format!(
        "<agents total=\"{}\" tokens_used=\"{}\">\n{}\n</agents>\n",
        host.agents_started(),
        host.tokens_spent(),
        agents.join("\n")
    ));
    if !log_tail.trim().is_empty() {
        out.push_str(&format!("<log_tail>\n{log_tail}</log_tail>\n"));
    }
    match (&outcome.error_text, &outcome.return_value) {
        (Some(error), _) => {
            out.push_str(&format!("<error>\n{error}\n</error>\n"));
        }
        (None, Some(value)) => {
            let rendered =
                serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            out.push_str(&format!("<result>\n{rendered}\n</result>\n"));
        }
        (None, None) => {
            out.push_str("<result>null</result>\n");
        }
    }
    out.push_str(&format!(
        "<resume_hint>To resume with journal replay, call workflow again with \
         resume_from_run_id=\"{run_id}\"{}. Unchanged agent() calls replay for free. If you \
         edit the script, add resume_mode=\"positional\" so completed positions still replay, \
         and use resume_through=\"<phase, label, or index>\" to go back to a specific point \
         and re-run everything after it.</resume_hint>\n",
        script_path
            .map(|path| format!(
                " and script_path=\"{}\" (edit that file in place rather than \
                 regenerating the script — reworded prompts do not replay under exact mode)",
                path.display()
            ))
            .unwrap_or_default()
    ));
    out.push_str("</workflow_result>");
    crate::util::truncate::truncate_str_with_marker(&out, crate::DEFAULT_TOOL_OUTPUT_BYTES)
        .into_owned()
}

#[cfg(test)]
#[path = "workflow_tests.rs"]
mod workflow_tests;
