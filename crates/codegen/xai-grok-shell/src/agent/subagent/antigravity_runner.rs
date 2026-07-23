//! Out-of-process subagent execution via the Antigravity CLI.
//!
//! `run_shell_child` routes `antigravity:*` models here instead of spawning an
//! in-process child session: the whole "session" lives inside `agy --print`,
//! which brings its own model, login, and tool loop. The coordinator actor's
//! entry stays in `pending` for the duration of the run (its `cancel_token`
//! keeps kill/cancel flows working — no `StartedChild` promotion because
//! there is no child session handle); returning the `ChildRunOutput` moves it
//! straight to `completed`, so `get_task_output`, auto-wake, and
//! `resume_from` behave like any other subagent. Resume continues the CLI
//! conversation with `--conversation`, using the id persisted in `meta.json`.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol as acp;
use tokio_util::sync::CancellationToken;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_subagent_resolution::{EffectiveRuntimeConfig, ResumeSourceData};
use xai_tool_types::SubagentCapabilityMode;

use crate::agent::antigravity::{self, AgyRun, AgyRunError};
use crate::extensions::notification::SessionUpdate;
use crate::session::info::Info as SessionInfo;
use xai_grok_tools::implementations::grok_build::task::coordinator::ChildRunOutput;
use xai_grok_tools::implementations::grok_build::task::types::{SubagentRequest, SubagentResult};

use super::{
    ShellCompletionData, SubagentMeta, SubagentSpawnContext, child_run_output,
    emit_subagent_notification, failure_result, resolve_child_cwd, select_override_cwd,
    write_subagent_meta, write_subagent_output,
};

/// Fallback wall-clock budget for one `agy --print` run when
/// `OPENGROK_SUBAGENT_TIMEOUT_MS` is unset.
const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(600);

/// Cadence for the live `agy.log` tailer: at most one heartbeat / quota-probe
/// poll every this interval while a run is active.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

fn run_timeout() -> Duration {
    std::env::var("OPENGROK_SUBAGENT_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .filter(|d| *d >= Duration::from_secs(30))
        .unwrap_or(DEFAULT_RUN_TIMEOUT)
}

/// Everything the dispatch branch hands over from `run_shell_child`'s
/// locals (already resolved: overrides, resume source, worktree).
pub(super) struct AntigravityLaunch<'a> {
    pub request: SubagentRequest,
    /// Native agy model id (prefix already stripped).
    pub agy_model: String,
    pub effective_runtime: &'a EffectiveRuntimeConfig,
    pub resume_source: Option<&'a ResumeSourceData>,
    pub worktree_path: Option<PathBuf>,
    pub worktree_freshly_created: bool,
    pub cancel_token: CancellationToken,
    pub start: std::time::Instant,
}

pub(super) async fn run_antigravity_subagent(
    launch: AntigravityLaunch<'_>,
    ctx: &SubagentSpawnContext,
    gateway: &GatewaySender,
    mut completion_data: ShellCompletionData,
) -> ChildRunOutput<ShellCompletionData> {
    let AntigravityLaunch {
        request,
        agy_model,
        effective_runtime,
        resume_source,
        worktree_path,
        worktree_freshly_created,
        cancel_token,
        start,
    } = launch;
    // ── Authoritative feature gates ─────────────────────────────────────
    let state = antigravity::feature_state().await;
    if !state.enabled {
        let msg = "Antigravity subagents are disabled. Enable the \"Antigravity \
                   subagents\" setting (`[ui].antigravity_subagents`) first."
            .to_string();
        return child_run_output(failure_result(&request, &msg), completion_data, None);
    }
    let binary = antigravity::binary_name(&state.config);
    if !state.installed {
        let msg = format!("Antigravity CLI (`{binary}`) was not found on this system.");
        return child_run_output(failure_result(&request, &msg), completion_data, None);
    }
    let status = antigravity::cached_status(&binary).await;
    if !status.signed_in {
        let msg = format!(
            "Antigravity CLI is not signed in ({}). Run `{binary}` once in a terminal \
             to sign in, then retry.",
            status.detail.as_deref().unwrap_or("sign-in required")
        );
        return child_run_output(failure_result(&request, &msg), completion_data, None);
    }
    if !status.models.is_empty() && !status.models.iter().any(|m| m == &agy_model) {
        let msg = format!(
            "Unknown antigravity model \"{agy_model}\". Available: {}",
            status.prefixed_models().join(", ")
        );
        return child_run_output(failure_result(&request, &msg), completion_data, None);
    }
    // Pre-spawn availability: a prior run in this process may have cached
    // `GetAvailableModels`. Fail fast (with quota/availability context and a
    // reference-model suggestion) when it marks this model exhausted or absent.
    // No-op when the cache is empty, so a cold cache never blocks a spawn.
    if let Some(issue) = antigravity::model_availability_issue(&agy_model, &status.models) {
        let msg = antigravity::unavailable_model_message(&agy_model, issue);
        tracing::info!(
            subagent_id = %request.id, model = %agy_model, ?issue,
            "antigravity spawn short-circuited on cached availability"
        );
        return child_run_output(failure_result(&request, &msg), completion_data, None);
    }
    // Resume must have a stored conversation id — without one there is no
    // transcript to continue (the transcript lives inside agy, not in a
    // session dir we could copy).
    let inherited_conversation =
        resume_source.and_then(|source| source.antigravity_conversation_id.clone());
    if resume_source.is_some() && inherited_conversation.is_none() {
        let msg = format!(
            "Cannot resume antigravity subagent '{}': no stored Antigravity \
             conversation id (the source may predate this feature).",
            resume_source
                .map(|s| s.subagent_id.as_str())
                .unwrap_or_default()
        );
        return child_run_output(failure_result(&request, &msg), completion_data, None);
    }
    let full_slug = format!("{}{agy_model}", antigravity::MODEL_PREFIX);
    let subagent_id = request.id.clone();
    let child_session_id = acp::SessionId::new(subagent_id.clone());
    let override_cwd = select_override_cwd(resume_source, request.cwd.as_deref());
    let effective_cwd = resolve_child_cwd(worktree_path.as_deref(), override_cwd, &ctx.parent_cwd);
    let effective_cwd_str = effective_cwd.to_string_lossy().into_owned();
    let parent_session_dir = crate::session::persistence::session_dir(&SessionInfo {
        id: acp::SessionId::new(ctx.parent_session_id.clone()),
        cwd: ctx.parent_cwd.to_string_lossy().to_string(),
    });
    let subagent_meta_dir = parent_session_dir.join("subagents").join(&subagent_id);
    if let Err(e) = std::fs::create_dir_all(&subagent_meta_dir) {
        tracing::warn!(
            subagent_id = %subagent_id, error = %e,
            "failed to create antigravity subagent meta dir"
        );
    }
    let effective_source_str = if resume_source.is_some() {
        "resumed"
    } else {
        "new"
    };
    let mut meta = SubagentMeta {
        subagent_id: subagent_id.clone(),
        parent_session_id: ctx.parent_session_id.clone(),
        child_session_id: child_session_id.0.to_string(),
        subagent_type: request.subagent_type.clone(),
        description: request.description.clone(),
        prompt: request.prompt.clone(),
        status: "running".to_string(),
        started_at: chrono::Utc::now(),
        completed_at: None,
        duration_ms: None,
        tool_calls: None,
        turns: None,
        error: None,
        effective_context_source: Some(effective_source_str.to_string()),
        context_normalized: false,
        fork_copy_error: None,
        persona: effective_runtime.persona.clone(),
        resumed_from: request.resume_from.clone(),
        child_cwd: Some(effective_cwd_str.clone()),
        worktree_path: worktree_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        snapshot_ref: None,
        effective_model_id: Some(full_slug.clone()),
        model_route: None,
        antigravity_conversation_id: inherited_conversation.clone(),
    };
    write_subagent_meta(&subagent_meta_dir, &meta);
    xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::SubagentLaunched {
        subagent_id: subagent_id.clone(),
        parent_session_id: ctx.parent_session_id.clone(),
        subagent_type: request.subagent_type.clone(),
        persona: effective_runtime.persona.clone(),
        fork_context: false,
        resume_from: request.resume_from.clone(),
        isolated_worktree: worktree_path.is_some(),
        mcp_inherited_count: 0,
        mcp_owned_count: 0,
        skills_inherited_count: 0,
    });
    emit_subagent_notification(
        gateway,
        &ctx.parent_session_id,
        SessionUpdate::SubagentSpawned {
            subagent_id: subagent_id.clone(),
            child_session_id: child_session_id.0.to_string(),
            parent_session_id: ctx.parent_session_id.clone(),
            parent_prompt_id: request.parent_prompt_id.clone(),
            subagent_type: request.subagent_type.clone(),
            description: request.description.clone(),
            effective_context_source: Some(effective_source_str.to_string()),
            context_normalized: false,
            capability_mode: effective_runtime
                .capability_mode
                .and_then(|m| serde_json::to_value(m).ok())
                .and_then(|v| v.as_str().map(String::from)),
            persona: effective_runtime.persona.clone(),
            role: effective_runtime.role_name.clone(),
            model: Some(full_slug.clone()),
            resumed_from: request.resume_from.clone(),
            swarm_id: request.swarm.as_ref().map(|swarm| swarm.swarm_id.clone()),
            swarm_description: request
                .swarm
                .as_ref()
                .map(|swarm| swarm.description.clone()),
            swarm_index: request.swarm.as_ref().map(|swarm| swarm.index),
            swarm_item: request.swarm.as_ref().and_then(|swarm| swarm.item.clone()),
            swarm_expected_members: request.swarm.as_ref().map(|swarm| swarm.expected_members),
            workflow_run_id: request.owner.workflow_run_id().map(str::to_string),
        },
        ctx.parent_cmd_tx.as_ref(),
    );
    completion_data.spawned_notification_emitted = true;
    // Full access by default: headless agy auto-denies mutating tools
    // without its skip-permissions flag, which surfaced as constant
    // permission errors for implementation-stage subagents. Antigravity
    // members now run with the flag unless the operator opts out with
    // `[antigravity] skip_permissions = false` — and a caller that pins a
    // read-only capability mode (e.g. review/audit stages) always stays
    // read-only regardless.
    let skip_permissions = state.config.skip_permissions.unwrap_or(true)
        && !matches!(
            effective_runtime.capability_mode,
            Some(SubagentCapabilityMode::ReadOnly)
        );
    let agy_run = AgyRun {
        binary: binary.clone(),
        model: agy_model.clone(),
        effort: effective_runtime.reasoning_effort.clone(),
        prompt: request.prompt.clone(),
        workspace_dir: effective_cwd.clone(),
        log_file: subagent_meta_dir.join("agy.log"),
        timeout: run_timeout(),
        skip_permissions,
        conversation_id: inherited_conversation.clone(),
    };
    tracing::info!(
        subagent_id = %subagent_id, model = %agy_model, binary = %binary,
        cwd = %effective_cwd_str, skip_permissions,
        resumed_conversation = inherited_conversation.is_some(),
        "Running antigravity subagent via CLI"
    );
    // Run the CLI while tailing its log for two best-effort side channels that
    // never affect the run result:
    //  1. Quota probe — the LanguageServer binds a random HTTP port (logged
    //     within ~1s) that answers `RetrieveUserQuotaSummary` only while the
    //     run is live. We fire ONE request once the port appears and cache the
    //     parsed summary process-globally for `/usage`.
    //  2. Heartbeat — coarse milestone phases mapped from new log lines,
    //     emitted (deduped, ≤1 per interval) as a `SubagentStatus` so the TUI
    //     card shows a live phase + elapsed instead of nothing.
    let run_result = {
        let run_fut = antigravity::run_print(&agy_run, &status.models, &cancel_token);
        tokio::pin!(run_fut);
        let log_path = agy_run.log_file.clone();
        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut quota_probe_started = false;
        let mut last_phase: Option<&'static str> = None;
        loop {
            tokio::select! {
                biased;
                result = &mut run_fut => break result,
                _ = ticker.tick() => {
                    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
                    // One-shot quota probe against the live LS port.
                    if !quota_probe_started
                        && let Some(port) = antigravity::parse_http_port(&log)
                    {
                        quota_probe_started = true;
                        tokio::spawn(async move {
                            // Both RPCs share the live port; fetch concurrently.
                            let (quota, models) = tokio::join!(
                                antigravity::fetch_quota_summary(port),
                                antigravity::fetch_available_models(port),
                            );
                            match quota {
                                Ok(buckets) => antigravity::cache_quota_summary(buckets),
                                Err(e) => {
                                    tracing::debug!(error = %e, "agy quota probe failed");
                                }
                            }
                            match models {
                                Ok(models) => antigravity::cache_available_models(models),
                                Err(e) => {
                                    tracing::debug!(error = %e, "agy models probe failed");
                                }
                            }
                        });
                    }
                    // Heartbeat: emit only when the phase label changes; the
                    // card renders elapsed live from its own spawn timestamp.
                    let phase = antigravity::phase_from_log(&log);
                    if last_phase != Some(phase) {
                        last_phase = Some(phase);
                        emit_subagent_notification(
                            gateway,
                            &ctx.parent_session_id,
                            SessionUpdate::SubagentStatus {
                                subagent_id: subagent_id.clone(),
                                parent_session_id: ctx.parent_session_id.clone(),
                                child_session_id: child_session_id.0.to_string(),
                                status: "antigravity_phase".to_string(),
                                attempt: 0,
                                retry_after_ms: None,
                                label: Some(phase.to_string()),
                            },
                            ctx.parent_cmd_tx.as_ref(),
                        );
                    }
                }
            }
        }
    };
    let duration_ms = start.elapsed().as_millis() as u64;
    let (result, conversation_id) = match run_result {
        Ok(outcome) => {
            let result = SubagentResult {
                success: true,
                output: std::sync::Arc::from(outcome.output.as_str()),
                error: None,
                cancelled: false,
                subagent_id: subagent_id.clone(),
                child_session_id: child_session_id.0.to_string(),
                tool_calls: 0,
                turns: 1,
                duration_ms,
                tokens_used: 0,
                output_tokens_used: 0,
                total_tokens_used: 0,
                output_usage_incomplete: false,
                worktree_path: worktree_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().to_string()),
                backgrounded: false,
            };
            (result, outcome.conversation_id)
        }
        Err(AgyRunError::Cancelled) => {
            if worktree_freshly_created
                && let Some(ref wt_path) = worktree_path
                && let Err(e) = crate::session::worktree::remove_subagent_worktree(wt_path).await
            {
                tracing::warn!(
                    subagent_id = %subagent_id, worktree_path = %wt_path.display(),
                    error = %e,
                    "failed to remove pristine worktree for cancelled antigravity subagent"
                );
            }
            let result = SubagentResult {
                success: false,
                cancelled: true,
                error: Some("Subagent was cancelled".to_string()),
                subagent_id: subagent_id.clone(),
                child_session_id: child_session_id.0.to_string(),
                duration_ms,
                ..Default::default()
            };
            (result, inherited_conversation.clone())
        }
        Err(AgyRunError::Failed(msg)) => {
            // When the cached quota shows a relevant bucket exhausted, append it
            // to the failure so the operator sees the likely cause + reset time.
            let msg = match antigravity::exhausted_quota_note() {
                Some(note) => format!("{msg}\n{note}"),
                None => msg,
            };
            let result = SubagentResult {
                success: false,
                error: Some(msg),
                subagent_id: subagent_id.clone(),
                child_session_id: child_session_id.0.to_string(),
                duration_ms,
                ..Default::default()
            };
            (result, inherited_conversation.clone())
        }
    };
    // ── Persist terminal state (meta.json + output.json) ────────────────
    meta.status = result.status().to_string();
    meta.completed_at = Some(chrono::Utc::now());
    meta.duration_ms = Some(duration_ms);
    meta.tool_calls = Some(result.tool_calls);
    meta.turns = Some(result.turns);
    meta.error = result.error.clone();
    meta.antigravity_conversation_id = conversation_id.clone();
    write_subagent_meta(&subagent_meta_dir, &meta);
    let persisted_output_dir = (result.success
        && !result.output.is_empty()
        && write_subagent_output(&subagent_meta_dir, &result.output))
    .then(|| subagent_meta_dir.clone());
    completion_data.set_persisted_output_dir(persisted_output_dir);
    let outcome = if result.success {
        xai_grok_telemetry::events::Outcome::Completed
    } else if result.cancelled {
        xai_grok_telemetry::events::Outcome::Cancelled
    } else {
        xai_grok_telemetry::events::Outcome::Error
    };
    xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::SubagentCompleted {
        subagent_id: subagent_id.clone(),
        parent_session_id: ctx.parent_session_id.clone(),
        outcome,
        duration_ms,
        tool_calls: 0,
        tokens_used: None,
    });
    // The coordinator actor owns terminal disposition: result delivery,
    // SubagentFinished presentation, auto-wake, and the completed registry.
    child_run_output(result, completion_data, None)
}
