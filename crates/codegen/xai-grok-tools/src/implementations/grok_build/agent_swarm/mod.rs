//! Foreground, ramped parallel subagent orchestration.

use std::{
    collections::{HashSet, VecDeque},
    env,
    sync::Arc,
    time::Duration,
};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::mpsc;
use xai_tool_types::{AgentSwarmToolInput, is_not_sentinel};

use crate::{
    implementations::grok_build::task::{
        MAX_SUBAGENT_DEPTH,
        backend::{SubagentBackend, SubagentBackendResource},
        types::{
            CurrentPromptIdResource, ModelOverrideProvenance, SWARM_RATE_LIMIT_RETRY_BASE_MS,
            SessionIdResource, SubagentDepthCounter, SubagentRateLimitDecision, SubagentRequest,
            SubagentResult, SubagentRuntimeOverrides, SubagentStatusEvent,
            SubagentValidateTypeOutcome, SwarmMemberMeta, TaskModelValidator,
            swarm_rate_limit_backoff,
        },
    },
    types::{
        output::ToolOutput,
        requirements::{Expr, ToolRequirement},
        tool::{ToolKind, ToolNamespace},
    },
};

const MAX_MEMBERS: usize = 128;
const INITIAL_LAUNCHES: usize = 5;
const RAMP_INTERVAL: Duration = Duration::from_millis(700);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const RATE_LIMIT_SHRINK_INTERVAL: Duration = Duration::from_secs(2);
const RATE_LIMIT_RECOVERY_QUIET_PERIOD: Duration = Duration::from_secs(3 * 60);

#[derive(Debug, Default)]
pub struct AgentSwarmTool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberMode {
    New,
    Resume,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedMember {
    index: u32,
    item: Option<String>,
    prompt: String,
    resume_from: Option<String>,
    mode: MemberMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberOutcome {
    Completed,
    Failed,
    Aborted,
}

impl MemberOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemberState {
    Started,
    #[allow(dead_code)] // Reserved for a future cancellation result that can be rendered.
    NotStarted,
}

impl MemberState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::NotStarted => "not_started",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemberResult {
    index: u32,
    item: Option<String>,
    agent_id: String,
    outcome: MemberOutcome,
    state: MemberState,
    mode: MemberMode,
    body: String,
}

#[derive(Debug, Clone)]
struct AdaptiveCapacity {
    ceiling: usize,
    current: usize,
    last_rate_limit_at: Option<tokio::time::Instant>,
    last_shrink_at: Option<tokio::time::Instant>,
    recovered_since_last_rate_limit: bool,
}

#[derive(Debug)]
struct PendingRetry {
    subagent_id: String,
    attempt: u32,
    eligible_at: tokio::time::Instant,
    decision_tx: mpsc::UnboundedSender<SubagentRateLimitDecision>,
}

#[derive(Debug, Clone)]
struct RateLimitGate {
    active: bool,
    retry_interval: Duration,
    rate_limit_pause_until: tokio::time::Instant,
    last_launch_at: Option<tokio::time::Instant>,
}

impl RateLimitGate {
    fn new(now: tokio::time::Instant) -> Self {
        Self {
            active: false,
            retry_interval: Duration::from_millis(SWARM_RATE_LIMIT_RETRY_BASE_MS),
            rate_limit_pause_until: now,
            last_launch_at: None,
        }
    }

    fn active(&self) -> bool {
        self.active
    }

    fn note_rate_limit(&mut self, now: tokio::time::Instant) {
        self.active = true;
        self.rate_limit_pause_until = self.rate_limit_pause_until.max(now + self.retry_interval);
        self.retry_interval = self.retry_interval.saturating_mul(2);
    }

    fn note_ready(&mut self) {
        if self.active {
            self.retry_interval = Duration::from_millis(SWARM_RATE_LIMIT_RETRY_BASE_MS);
        }
    }

    fn note_launch(&mut self, now: tokio::time::Instant) {
        self.last_launch_at = Some(now);
    }

    fn launch_deadline(&self, task_eligible_at: tokio::time::Instant) -> tokio::time::Instant {
        let paced_launch_at = self
            .last_launch_at
            .map(|last| last + self.retry_interval)
            .unwrap_or(self.rate_limit_pause_until);
        task_eligible_at
            .max(self.rate_limit_pause_until)
            .max(paced_launch_at)
    }
}

impl AdaptiveCapacity {
    fn new(total: usize, concurrency_cap: Option<usize>) -> Self {
        let ceiling = concurrency_cap.unwrap_or(total).min(total).max(1);
        Self {
            ceiling,
            current: ceiling,
            last_rate_limit_at: None,
            last_shrink_at: None,
            recovered_since_last_rate_limit: false,
        }
    }

    fn current(&self) -> usize {
        self.current
    }

    fn note_rate_limit(&mut self, now: tokio::time::Instant, started_count: usize) {
        if self.last_rate_limit_at.is_none() {
            let active_before_shrink = self.current.min(started_count.max(1));
            self.current = active_before_shrink.saturating_sub(1).max(1);
            self.last_shrink_at = Some(now);
        } else if self
            .last_shrink_at
            .is_none_or(|last| now.duration_since(last) >= RATE_LIMIT_SHRINK_INTERVAL)
        {
            self.current = self.current.saturating_sub(1).max(1);
            self.last_shrink_at = Some(now);
        }
        self.last_rate_limit_at = Some(now);
        self.recovered_since_last_rate_limit = false;
    }

    fn recovery_deadline(&self) -> Option<tokio::time::Instant> {
        if self.recovered_since_last_rate_limit || self.current >= self.ceiling {
            return None;
        }
        self.last_rate_limit_at
            .map(|last| last + RATE_LIMIT_RECOVERY_QUIET_PERIOD)
    }

    fn recover_if_due(&mut self, now: tokio::time::Instant) -> bool {
        let Some(deadline) = self.recovery_deadline() else {
            return false;
        };
        if now < deadline {
            return false;
        }
        self.current = (self.current + 1).min(self.ceiling);
        self.recovered_since_last_rate_limit = true;
        true
    }
}

impl crate::types::tool_metadata::ToolMetadata for AgentSwarmTool {
    fn kind(&self) -> ToolKind {
        ToolKind::AgentSwarm
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        concat!(
            "Launch multiple foreground subagents from one prompt template, existing agent ",
            "resumes, or both. Use agent_swarm when many subagents should run the same kind of ",
            "task over different inputs. The placeholder is exactly {{item}}. For a few ",
            "differently-shaped tasks, use ordinary task calls instead. Use resume_agent_ids to ",
            "continue unfinished or timed-out subagents by mapping each agent_id to its exact ",
            "continuation prompt, often 'continue'. You may combine resume_agent_ids with items, ",
            "but do not duplicate resumed work in items; resume slots launch first. Validation ",
            "is fail-fast before any child starts: provide at least 2 items unless ",
            "resume_agent_ids is supplied; items require prompt_template containing literal ",
            "{{item}}; expanded prompts must be distinct; and total members are capped at 128. ",
            "Pass model to run every new member on a specific available model slug (resumed ",
            "members keep their prior model); if the slug is rejected, report the error instead ",
            "of re-running the swarm on a different model. ",
            "Results return together in input slot order as agent_swarm_result XML with resume ",
            "hints for unfinished members. agent_swarm must be the only tool call in the model ",
            "response. Keep the tree flat: swarm members cannot launch further task or ",
            "agent_swarm tools."
        )
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::Value(ToolRequirement::tool_kind(ToolKind::Task))
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl xai_tool_runtime::Tool for AgentSwarmTool {
    type Args = AgentSwarmToolInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("agent_swarm").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "agent_swarm",
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
        input: AgentSwarmToolInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        // Everything below this line that can reject a call is resolved before
        // any child request reaches the backend.
        let members =
            validate_and_plan(&input).map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        let concurrency_cap =
            swarm_concurrency_from_env().map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        let timeout =
            subagent_timeout_from_env().map_err(xai_tool_runtime::ToolError::invalid_arguments)?;
        let resources = crate::types::tool_metadata::shared_resources(&ctx)?;
        let (depth, backend, model_validator, parent_session_id, parent_prompt_id) = {
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
            )
        };
        if depth >= MAX_SUBAGENT_DEPTH {
            return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                "Subagent depth limit exceeded (current depth: {depth}, max: {MAX_SUBAGENT_DEPTH}). Cannot spawn further nested subagents."
            )));
        }
        if members.iter().any(|member| member.mode == MemberMode::New) {
            match backend
                .backend()
                .validate_type(&input.subagent_type, &parent_session_id)
                .await
            {
                SubagentValidateTypeOutcome::Ok => {}
                SubagentValidateTypeOutcome::Unknown { available } => {
                    let suffix = (!available.is_empty())
                        .then(|| format!(". Available types: {}", available.join(", ")))
                        .unwrap_or_default();
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Unknown subagent type: {}{suffix}",
                        input.subagent_type
                    )));
                }
                SubagentValidateTypeOutcome::Disabled => {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "Subagent '{}' is disabled",
                        input.subagent_type
                    )));
                }
                SubagentValidateTypeOutcome::NotAllowed { allowed } => {
                    return Err(xai_tool_runtime::ToolError::invalid_arguments(format!(
                        "agent can only spawn: {}; '{}' not allowed",
                        allowed.join(", "),
                        input.subagent_type
                    )));
                }
                SubagentValidateTypeOutcome::ValidationUnavailable => {
                    return Err(xai_tool_runtime::ToolError::custom(
                        "validation_unavailable",
                        "Cannot validate subagent type: the subagent coordinator is unreachable.",
                    ));
                }
            }
        }

        // Same eager gate as the task tool: an unknown slug — or a slug whose
        // provider has no usable credentials — must reject the whole swarm
        // here, not spawn members that die at setup and silently inherit the
        // parent model.
        let member_model = xai_tool_types::sanitize_optional_arg(input.model);
        if let Some(ref requested) = member_model {
            let validator = model_validator.ok_or_else(|| {
                xai_tool_runtime::ToolError::custom(
                    "validation_unavailable",
                    "Cannot validate agent_swarm.model: model catalog validator is unavailable.",
                )
            })?;
            if let Some(error) = validator.error_for(requested) {
                return Err(xai_tool_runtime::ToolError::invalid_arguments(error));
            }
        }

        let results = run_scheduler(
            backend.0.clone(),
            members,
            SwarmRequestContext {
                swarm_id: ctx.call_id.to_string(),
                description: input.description,
                subagent_type: input.subagent_type,
                parent_session_id,
                parent_prompt_id,
                model: member_model,
            },
            concurrency_cap,
            timeout,
        )
        .await;
        Ok(ToolOutput::Text(render_xml(&results).into()))
    }
}

#[derive(Clone)]
struct SwarmRequestContext {
    swarm_id: String,
    description: String,
    subagent_type: String,
    parent_session_id: String,
    parent_prompt_id: Option<String>,
    /// Model override applied to every new member; resumed members keep
    /// their prior model (the resume path pins it).
    model: Option<String>,
}

fn validate_and_plan(input: &AgentSwarmToolInput) -> Result<Vec<PlannedMember>, String> {
    let resumes = input
        .resume_agent_ids
        .clone()
        .unwrap_or_default()
        .into_entries();
    let items = input.items.clone().unwrap_or_default();
    let total = resumes.len() + items.len();
    if total > MAX_MEMBERS {
        return Err(format!(
            "agent_swarm supports at most {MAX_MEMBERS} total members"
        ));
    }
    if resumes.is_empty() && items.len() < 2 {
        return Err(
            "agent_swarm requires at least 2 items unless resume_agent_ids is supplied".to_string(),
        );
    }
    let item_template = if items.is_empty() {
        None
    } else {
        let template = input
            .prompt_template
            .as_deref()
            .ok_or("prompt_template is required when items is supplied")?;
        if !template.contains("{{item}}") {
            return Err(
                "prompt_template must contain literal {{item}} when items is supplied".to_string(),
            );
        }
        let expanded: HashSet<String> = items
            .iter()
            .map(|item| template.replace("{{item}}", item))
            .collect();
        if expanded.len() != items.len() {
            return Err(
                "prompt_template must expand to distinct prompts for each item".to_string(),
            );
        }
        Some(template)
    };

    let mut members = Vec::with_capacity(total);
    for (index, (agent_id, prompt)) in resumes.into_iter().enumerate() {
        if !is_not_sentinel(&agent_id) {
            return Err(
                "resume_agent_ids must not contain empty or placeholder agent IDs".to_string(),
            );
        }
        members.push(PlannedMember {
            index: index as u32,
            item: None,
            // Preserve user-provided prompt bytes exactly. Unlike IDs, prompts
            // are task content rather than an optional/sentinel argument.
            prompt,
            resume_from: Some(agent_id.trim().to_string()),
            mode: MemberMode::Resume,
        });
    }
    let start = members.len() as u32;
    for (offset, item) in items.into_iter().enumerate() {
        members.push(PlannedMember {
            index: start + offset as u32,
            prompt: item_template
                .expect("items require a template")
                .replace("{{item}}", &item),
            item: Some(item),
            resume_from: None,
            mode: MemberMode::New,
        });
    }
    Ok(members)
}

fn parse_positive_env(name: &str) -> Result<Option<usize>, String> {
    match env::var(name) {
        Ok(value) => parse_positive_value(name, &value),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(_) => Ok(None),
    }
}

fn parse_positive_value(name: &str, value: &str) -> Result<Option<usize>, String> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    value
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or_else(|| format!("{name} must be a positive integer when set"))
}

/// `None` means no active-member cap; the initial burst and ramp still apply.
fn swarm_concurrency_from_env() -> Result<Option<usize>, String> {
    Ok(parse_positive_env("OPENGROK_AGENT_SWARM_MAX_CONCURRENCY")?
        .or(parse_positive_env("KIMI_CODE_AGENT_SWARM_MAX_CONCURRENCY")?))
}

fn subagent_timeout_from_env() -> Result<Option<Duration>, String> {
    let value = env::var("OPENGROK_SUBAGENT_TIMEOUT_MS")
        .ok()
        .or_else(|| env::var("KIMI_SUBAGENT_TIMEOUT_MS").ok());
    match value {
        None => Ok(Some(DEFAULT_TIMEOUT)),
        Some(value) if value.trim().is_empty() => Ok(Some(DEFAULT_TIMEOUT)),
        Some(value) => value
            .trim()
            .parse::<u64>()
            .map_err(|_| {
                "OPENGROK_SUBAGENT_TIMEOUT_MS must be a non-negative integer when set".to_string()
            })
            .map(|ms| (ms != 0).then(|| Duration::from_millis(ms))),
    }
}

async fn run_scheduler(
    backend: Arc<dyn SubagentBackend>,
    members: Vec<PlannedMember>,
    context: SwarmRequestContext,
    concurrency_cap: Option<usize>,
    timeout: Option<Duration>,
) -> Vec<MemberResult> {
    let expected = members.len() as u32;
    let mut slots = vec![None; members.len()];
    let mut next = 0usize;
    let mut active = FuturesUnordered::new();
    let mut waiting = HashSet::new();
    let mut pending_retries = VecDeque::new();
    let mut ready_normal_launches = HashSet::new();
    let mut capacity = AdaptiveCapacity::new(members.len(), concurrency_cap);
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();
    let mut rate_limit_gate = RateLimitGate::new(tokio::time::Instant::now());
    let initial = initial_launch_count(members.len(), concurrency_cap);
    while next < initial {
        let agent_id = uuid::Uuid::now_v7().to_string();
        active.push(spawn_member(
            backend.clone(),
            members[next].clone(),
            context.clone(),
            expected,
            agent_id,
            Some(status_tx.clone()),
            timeout,
        ));
        next += 1;
    }

    // The initial cohort is the only burst. A later launch never occurs before
    // this deadline, even if all initial members already completed.
    let mut next_launch = tokio::time::Instant::now() + RAMP_INTERVAL;
    while next < members.len() || !active.is_empty() || !pending_retries.is_empty() {
        let effective_active = active.len().saturating_sub(waiting.len());
        let has_queued_work = next < members.len() || !pending_retries.is_empty();
        let can_launch_normal = !rate_limit_gate.active()
            && next < members.len()
            && effective_active < capacity.current();
        let can_launch_rate_limited =
            rate_limit_gate.active() && has_queued_work && effective_active < capacity.current();
        let task_eligible_at = pending_retries
            .front()
            .map(|retry: &PendingRetry| retry.eligible_at)
            .unwrap_or_else(tokio::time::Instant::now);
        let rate_limited_launch_at = rate_limit_gate.launch_deadline(task_eligible_at);
        let recovery_deadline = (rate_limit_gate.active() && has_queued_work)
            .then(|| capacity.recovery_deadline())
            .flatten();
        let recovery_sleep_until = recovery_deadline.unwrap_or(rate_limited_launch_at);
        tokio::select! {
            biased;
            Some(event) = status_rx.recv() => {
                match event {
                    SubagentStatusEvent::ProviderRequestStarted { subagent_id } => {
                        ready_normal_launches.insert(subagent_id);
                        rate_limit_gate.note_ready();
                    }
                    SubagentStatusEvent::RateLimitWaiting {
                        subagent_id,
                        attempt,
                        decision_tx,
                    } => {
                        let unfinished = slots.iter().filter(|slot| slot.is_none()).count();
                        if unfinished <= 1 {
                            let _ = decision_tx.send(SubagentRateLimitDecision::Fail);
                            continue;
                        }
                        waiting.insert(subagent_id.clone());
                        let now = tokio::time::Instant::now();
                        capacity.note_rate_limit(now, ready_normal_launches.len());
                        rate_limit_gate.note_rate_limit(now);
                        pending_retries.retain(|retry| retry.subagent_id != subagent_id);
                        pending_retries.push_front(PendingRetry {
                            subagent_id,
                            attempt,
                            eligible_at: now + swarm_rate_limit_backoff(attempt),
                            decision_tx,
                        });
                    }
                    SubagentStatusEvent::RateLimitRetrying { .. } => {}
                }
            }
            Some((scheduled_agent_id, result)) = active.next(), if !active.is_empty() => {
                waiting.remove(&scheduled_agent_id);
                pending_retries.retain(|retry| retry.subagent_id != scheduled_agent_id);
                store_result(&mut slots, result);
            }
            _ = tokio::time::sleep_until(recovery_sleep_until), if recovery_deadline.is_some() => {
                capacity.recover_if_due(tokio::time::Instant::now());
            }
            _ = tokio::time::sleep_until(next_launch), if can_launch_normal => {
                let agent_id = uuid::Uuid::now_v7().to_string();
                active.push(spawn_member(
                    backend.clone(),
                    members[next].clone(),
                    context.clone(),
                    expected,
                    agent_id,
                    Some(status_tx.clone()),
                    timeout,
                ));
                next += 1;
                next_launch = tokio::time::Instant::now() + RAMP_INTERVAL;
            }
            _ = tokio::time::sleep_until(rate_limited_launch_at), if can_launch_rate_limited => {
                let now = tokio::time::Instant::now();
                if let Some(retry) = pending_retries.pop_front() {
                    waiting.remove(&retry.subagent_id);
                    tracing::info!(
                        subagent_id = %retry.subagent_id,
                        attempt = retry.attempt,
                        "retrying rate-limited swarm member on its existing child session"
                    );
                    let _ = retry.decision_tx.send(SubagentRateLimitDecision::Retry);
                } else {
                    let agent_id = uuid::Uuid::now_v7().to_string();
                    active.push(spawn_member(
                        backend.clone(),
                        members[next].clone(),
                        context.clone(),
                        expected,
                        agent_id,
                        Some(status_tx.clone()),
                        timeout,
                    ));
                    next += 1;
                }
                rate_limit_gate.note_launch(now);
            }
        }
    }
    slots
        .into_iter()
        .map(|slot| slot.expect("every scheduled swarm member has a result"))
        .collect()
}

fn store_result(slots: &mut [Option<MemberResult>], result: MemberResult) {
    let index = result.index as usize;
    slots[index] = Some(result);
}

fn initial_launch_count(total: usize, concurrency_cap: Option<usize>) -> usize {
    total
        .min(INITIAL_LAUNCHES)
        .min(concurrency_cap.unwrap_or(INITIAL_LAUNCHES))
}

fn build_member_request(
    member: PlannedMember,
    context: SwarmRequestContext,
    expected_members: u32,
    id: String,
    status_tx: Option<mpsc::UnboundedSender<SubagentStatusEvent>>,
) -> SubagentRequest {
    SubagentRequest {
        id,
        prompt: member.prompt,
        description: context.description.clone(),
        subagent_type: context.subagent_type,
        parent_session_id: context.parent_session_id,
        parent_prompt_id: context.parent_prompt_id,
        swarm: Some(SwarmMemberMeta {
            swarm_id: context.swarm_id,
            description: context.description,
            index: member.index,
            item: member.item,
            expected_members,
            status_tx,
        }),
        // Resumed members keep their prior model; the override applies to
        // new members only (mirrors the task tool's soft-ignore on resume).
        runtime_overrides: SubagentRuntimeOverrides {
            model: member
                .resume_from
                .is_none()
                .then(|| context.model.clone())
                .flatten(),
            model_override_provenance: ModelOverrideProvenance::Tool,
            reasoning_effort: None,
            persona: None,
            capability_mode: None,
            isolation: None,
            harness_agent_type: None,
            completion_output_cap: None,
            spawn_depth: None,
            loop_task_id: None,
            output_schema: None,
            output_token_budget: None,
        },
        resume_from: member.resume_from,
        cwd: None,
        run_in_background: false,
        surface_completion: false,
        await_to_completion: false,
        fork_context: false,
        owner: crate::implementations::grok_build::task::types::SubagentOwner::Task,
        cancel_token: tokio_util::sync::CancellationToken::new(),
    }
}

async fn spawn_member(
    backend: Arc<dyn SubagentBackend>,
    member: PlannedMember,
    context: SwarmRequestContext,
    expected_members: u32,
    agent_id: String,
    status_tx: Option<mpsc::UnboundedSender<SubagentStatusEvent>>,
    timeout: Option<Duration>,
) -> (String, MemberResult) {
    let request = build_member_request(
        member.clone(),
        context,
        expected_members,
        agent_id.clone(),
        status_tx,
    );
    let result = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, backend.spawn(request)).await {
            Ok(result) => result,
            Err(_) => {
                let _ = backend.cancel(&agent_id).await;
                return (
                    agent_id.clone(),
                    MemberResult {
                        index: member.index,
                        item: member.item,
                        agent_id,
                        outcome: MemberOutcome::Failed,
                        state: MemberState::Started,
                        mode: member.mode,
                        body: "subagent timed out".to_string(),
                    },
                );
            }
        },
        None => backend.spawn(request).await,
    };
    let member_result = match result {
        Ok(result) => member_result(
            member.index,
            member.item,
            member.mode,
            agent_id.clone(),
            result,
        ),
        Err(error) => MemberResult {
            index: member.index,
            item: member.item,
            agent_id: agent_id.clone(),
            outcome: MemberOutcome::Failed,
            state: MemberState::Started,
            mode: member.mode,
            body: error.to_string(),
        },
    };
    (agent_id, member_result)
}

fn member_result(
    index: u32,
    item: Option<String>,
    mode: MemberMode,
    fallback_agent_id: String,
    result: SubagentResult,
) -> MemberResult {
    let outcome = if result.cancelled {
        MemberOutcome::Aborted
    } else if result.success {
        MemberOutcome::Completed
    } else {
        MemberOutcome::Failed
    };
    let body = if outcome == MemberOutcome::Completed {
        result.output.to_string()
    } else {
        result.error.unwrap_or_else(|| result.output.to_string())
    };
    MemberResult {
        index,
        item,
        agent_id: if result.subagent_id.is_empty() {
            fallback_agent_id
        } else {
            result.subagent_id
        },
        outcome,
        state: MemberState::Started,
        mode,
        body,
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn render_xml(results: &[MemberResult]) -> String {
    let completed = results
        .iter()
        .filter(|result| result.outcome == MemberOutcome::Completed)
        .count();
    let failed = results
        .iter()
        .filter(|result| result.outcome == MemberOutcome::Failed)
        .count();
    let aborted = results
        .iter()
        .filter(|result| result.outcome == MemberOutcome::Aborted)
        .count();
    let has_resume_target = results
        .iter()
        .any(|result| result.outcome != MemberOutcome::Completed && !result.agent_id.is_empty());
    let mut xml = format!(
        "<agent_swarm_result><summary>completed={completed} failed={failed} aborted={aborted}</summary>"
    );
    if has_resume_target {
        xml.push_str(
            "<resume_hint>Call agent_swarm with resume_agent_ids mapping unfinished agent_id values to continuation prompts.</resume_hint>",
        );
    }
    for result in results {
        xml.push_str("<subagent");
        xml.push_str(&format!(" agent_id=\"{}\"", xml_escape(&result.agent_id)));
        if let Some(item) = result.item.as_deref() {
            xml.push_str(&format!(" item=\"{}\"", xml_escape(item)));
        }
        xml.push_str(&format!(
            " outcome=\"{}\" state=\"{}\"",
            result.outcome.as_str(),
            result.state.as_str(),
        ));
        if result.mode == MemberMode::Resume {
            xml.push_str(" mode=\"resume\"");
        }
        xml.push('>');
        xml.push_str(&xml_escape(&result.body));
        xml.push_str("</subagent>");
    }
    xml.push_str("</agent_swarm_result>");
    xml
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use tokio::sync::oneshot;

    use super::*;
    use crate::implementations::grok_build::task::types::{
        SubagentCancelOutcome, SubagentDescribeOutcome, SubagentSnapshot,
    };

    fn input(
        items: Option<Vec<&str>>,
        resumes: Option<Vec<(&str, &str)>>,
        template: Option<&str>,
    ) -> AgentSwarmToolInput {
        AgentSwarmToolInput {
            description: "work".into(),
            subagent_type: "general-purpose".into(),
            model: None,
            prompt_template: template.map(str::to_string),
            items: items.map(|items| items.into_iter().map(str::to_string).collect()),
            resume_agent_ids: resumes.map(|entries| {
                entries
                    .into_iter()
                    .map(|(id, prompt)| (id.to_string(), prompt.to_string()))
                    .collect()
            }),
        }
    }

    fn context() -> SwarmRequestContext {
        SwarmRequestContext {
            swarm_id: "call-id".to_string(),
            description: "description".to_string(),
            subagent_type: "general-purpose".to_string(),
            parent_session_id: "parent".to_string(),
            parent_prompt_id: Some("turn".to_string()),
            model: None,
        }
    }

    #[test]
    fn validation_requires_two_items_without_resume() {
        assert!(validate_and_plan(&input(Some(vec!["a"]), None, Some("{{item}}"))).is_err());
    }

    #[test]
    fn validation_uses_ordered_resume_prompt_mapping_exactly() {
        let plan = validate_and_plan(&input(
            Some(vec!["a", "b"]),
            Some(vec![(" old ", "  exact resume prompt  "), ("next", "p2")]),
            Some("do {{item}}"),
        ))
        .unwrap();
        assert_eq!(plan[0].resume_from.as_deref(), Some("old"));
        assert_eq!(plan[0].prompt, "  exact resume prompt  ");
        assert_eq!(plan[1].prompt, "p2");
        assert_eq!(plan[2].item.as_deref(), Some("a"));
        assert_eq!(plan[0].mode, MemberMode::Resume);
    }

    #[test]
    fn validation_rejects_missing_template_duplicate_expansion_and_empty_resume_id() {
        assert!(validate_and_plan(&input(Some(vec!["a", "b"]), None, None)).is_err());
        assert!(validate_and_plan(&input(Some(vec!["a", "a"]), None, Some("{{item}}"))).is_err());
        assert!(validate_and_plan(&input(None, Some(vec![(" ", "prompt")]), None)).is_err());
    }

    #[test]
    fn ordered_resume_object_deserializes_in_source_order() {
        let input: AgentSwarmToolInput = serde_json::from_str(
            r#"{
            "description":"work",
            "resume_agent_ids":{"second":"p2","first":"p1"}
        }"#,
        )
        .unwrap();
        let plan = validate_and_plan(&input).unwrap();
        assert_eq!(plan[0].resume_from.as_deref(), Some("second"));
        assert_eq!(plan[0].prompt, "p2");
        assert_eq!(plan[1].resume_from.as_deref(), Some("first"));
    }

    #[test]
    fn positive_cap_parser_rejects_invalid_nonempty_values() {
        assert_eq!(parse_positive_value("CAP", "3").unwrap(), Some(3));
        assert_eq!(parse_positive_value("CAP", " ").unwrap(), None);
        assert!(parse_positive_value("CAP", "0").is_err());
        assert!(parse_positive_value("CAP", "bad").is_err());
    }

    #[test]
    fn initial_launches_respect_cap_and_uncapped_burst() {
        assert_eq!(initial_launch_count(128, None), 5);
        assert_eq!(initial_launch_count(128, Some(2)), 2);
        assert_eq!(initial_launch_count(128, Some(99)), 5);
    }

    #[test]
    fn rate_limit_backoff_is_three_six_twelve_twenty_four_seconds() {
        assert_eq!(swarm_rate_limit_backoff(1), Duration::from_secs(3));
        assert_eq!(swarm_rate_limit_backoff(2), Duration::from_secs(6));
        assert_eq!(swarm_rate_limit_backoff(3), Duration::from_secs(12));
        assert_eq!(swarm_rate_limit_backoff(4), Duration::from_secs(24));
    }

    #[test]
    fn ready_attempt_resets_global_retry_interval() {
        let start = tokio::time::Instant::now();
        let mut gate = RateLimitGate::new(start);
        gate.note_rate_limit(start);
        assert_eq!(gate.launch_deadline(start), start + Duration::from_secs(3));
        gate.note_launch(start + Duration::from_secs(3));
        assert_eq!(gate.launch_deadline(start), start + Duration::from_secs(9));
        gate.note_ready();
        assert_eq!(gate.launch_deadline(start), start + Duration::from_secs(6));
    }

    #[test]
    fn tool_description_pins_swarm_workflow_and_flat_tree() {
        let description =
            crate::types::tool_metadata::ToolMetadata::description_template(&AgentSwarmTool);
        assert!(description.contains("literal {{item}}"));
        assert!(description.contains("capped at 128"));
        assert!(description.contains("only tool call"));
        assert!(description.contains("Keep the tree flat"));
    }

    #[test]
    fn adaptive_capacity_shrinks_at_most_every_two_seconds_and_recovers_once() {
        let start = tokio::time::Instant::now();
        let mut capacity = AdaptiveCapacity::new(20, None);
        capacity.note_rate_limit(start, 5);
        assert_eq!(capacity.current(), 4);

        capacity.note_rate_limit(start + Duration::from_secs(1), 5);
        assert_eq!(capacity.current(), 4);

        let second_shrink = start + RATE_LIMIT_SHRINK_INTERVAL;
        capacity.note_rate_limit(second_shrink, 5);
        assert_eq!(capacity.current(), 3);
        assert!(!capacity.recover_if_due(
            second_shrink + RATE_LIMIT_RECOVERY_QUIET_PERIOD - Duration::from_millis(1)
        ));
        assert!(capacity.recover_if_due(second_shrink + RATE_LIMIT_RECOVERY_QUIET_PERIOD));
        assert_eq!(capacity.current(), 4);
        assert!(!capacity.recover_if_due(second_shrink + RATE_LIMIT_RECOVERY_QUIET_PERIOD * 2));
    }

    #[test]
    fn request_carries_foreground_swarm_metadata() {
        let request = build_member_request(
            PlannedMember {
                index: 3,
                item: Some("item".to_string()),
                prompt: "prompt".to_string(),
                resume_from: Some("resume".to_string()),
                mode: MemberMode::Resume,
            },
            context(),
            7,
            "child".to_string(),
            None,
        );
        let swarm = request.swarm.expect("swarm metadata");
        assert_eq!(swarm.swarm_id, "call-id");
        assert_eq!(swarm.index, 3);
        assert!(swarm.status_tx.is_none());
        assert_eq!(request.resume_from.as_deref(), Some("resume"));
        assert!(!request.run_in_background);
        assert!(!request.surface_completion);
    }

    #[test]
    fn model_override_applies_to_new_members_only() {
        let ctx_with_model = SwarmRequestContext {
            model: Some("glm-5.2-fast".to_string()),
            ..context()
        };
        let new_member = build_member_request(
            PlannedMember {
                index: 0,
                item: Some("item".to_string()),
                prompt: "prompt".to_string(),
                resume_from: None,
                mode: MemberMode::New,
            },
            ctx_with_model.clone(),
            2,
            "child-new".to_string(),
            None,
        );
        assert_eq!(
            new_member.runtime_overrides.model.as_deref(),
            Some("glm-5.2-fast")
        );

        let resumed_member = build_member_request(
            PlannedMember {
                index: 1,
                item: None,
                prompt: "continue".to_string(),
                resume_from: Some("resume".to_string()),
                mode: MemberMode::Resume,
            },
            ctx_with_model,
            2,
            "child-resume".to_string(),
            None,
        );
        assert!(
            resumed_member.runtime_overrides.model.is_none(),
            "resumed members keep their prior model"
        );
    }

    #[test]
    fn xml_is_ordered_escaped_and_only_hints_incomplete_members() {
        let xml = render_xml(&[
            MemberResult {
                index: 0,
                item: None,
                agent_id: "resume&".into(),
                outcome: MemberOutcome::Failed,
                state: MemberState::Started,
                mode: MemberMode::Resume,
                body: "<failure>".into(),
            },
            MemberResult {
                index: 1,
                item: Some("<item>".into()),
                agent_id: "done".into(),
                outcome: MemberOutcome::Completed,
                state: MemberState::Started,
                mode: MemberMode::New,
                body: "<&>".into(),
            },
        ]);
        assert!(xml.starts_with("<agent_swarm_result>"));
        assert!(xml.contains("<summary>completed=1 failed=1 aborted=0</summary>"));
        assert!(xml.contains(
            "<resume_hint>Call agent_swarm with resume_agent_ids mapping unfinished agent_id values to continuation prompts.</resume_hint>"
        ));
        assert!(xml.contains(
            "agent_id=\"resume&amp;\" outcome=\"failed\" state=\"started\" mode=\"resume\""
        ));
        assert!(xml.contains("item=\"&lt;item&gt;\""));
        assert!(xml.contains("&lt;&amp;&gt;"));
        assert!(xml.find("resume&amp;").unwrap() < xml.find("agent_id=\"done\"").unwrap());
        assert!(!xml.contains("<output>"));
        let complete = render_xml(&[MemberResult {
            index: 0,
            item: None,
            agent_id: "done".into(),
            outcome: MemberOutcome::Completed,
            state: MemberState::Started,
            mode: MemberMode::New,
            body: "ok".into(),
        }]);
        assert!(!complete.contains("resume_hint"));
    }

    #[derive(Default)]
    struct ImmediateBackend {
        requests: Mutex<Vec<SubagentRequest>>,
    }

    #[async_trait::async_trait]
    impl SubagentBackend for ImmediateBackend {
        async fn spawn(
            &self,
            request: SubagentRequest,
        ) -> Result<SubagentResult, xai_tool_runtime::ToolError> {
            let id = request.id.clone();
            self.requests.lock().unwrap().push(request);
            Ok(SubagentResult {
                success: true,
                subagent_id: id.clone(),
                child_session_id: id,
                output: Arc::from("ok"),
                ..Default::default()
            })
        }
        async fn query(&self, _: &str, _: bool, _: Option<u64>) -> Option<SubagentSnapshot> {
            None
        }
        async fn cancel(&self, _: &str) -> SubagentCancelOutcome {
            SubagentCancelOutcome::NotFound
        }
        async fn validate_type(&self, _: &str, _: &str) -> SubagentValidateTypeOutcome {
            SubagentValidateTypeOutcome::Ok
        }
        async fn describe_subagent_type(
            &self,
            _: &str,
            _: Option<&str>,
            _: &str,
        ) -> SubagentDescribeOutcome {
            SubagentDescribeOutcome::Unavailable
        }
    }

    #[derive(Default)]
    struct HoldingBackend {
        requests: Mutex<Vec<SubagentRequest>>,
        waiters: Mutex<VecDeque<oneshot::Receiver<SubagentResult>>>,
    }

    #[async_trait::async_trait]
    impl SubagentBackend for HoldingBackend {
        async fn spawn(
            &self,
            request: SubagentRequest,
        ) -> Result<SubagentResult, xai_tool_runtime::ToolError> {
            self.requests.lock().unwrap().push(request);
            let (_, receiver) = oneshot::channel();
            self.waiters.lock().unwrap().push_back(receiver);
            futures::future::pending::<Result<SubagentResult, xai_tool_runtime::ToolError>>().await
        }
        async fn query(&self, _: &str, _: bool, _: Option<u64>) -> Option<SubagentSnapshot> {
            None
        }
        async fn cancel(&self, _: &str) -> SubagentCancelOutcome {
            SubagentCancelOutcome::NotFound
        }
        async fn validate_type(&self, _: &str, _: &str) -> SubagentValidateTypeOutcome {
            SubagentValidateTypeOutcome::Ok
        }
        async fn describe_subagent_type(
            &self,
            _: &str,
            _: Option<&str>,
            _: &str,
        ) -> SubagentDescribeOutcome {
            SubagentDescribeOutcome::Unavailable
        }
    }

    struct RateLimitBackend {
        rate_limited_indices: HashSet<u32>,
        requests: Mutex<Vec<(u32, String)>>,
        decisions: Mutex<Vec<(String, SubagentRateLimitDecision)>>,
    }

    impl RateLimitBackend {
        fn new(rate_limited_indices: impl IntoIterator<Item = u32>) -> Self {
            Self {
                rate_limited_indices: rate_limited_indices.into_iter().collect(),
                requests: Mutex::new(Vec::new()),
                decisions: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl SubagentBackend for RateLimitBackend {
        async fn spawn(
            &self,
            request: SubagentRequest,
        ) -> Result<SubagentResult, xai_tool_runtime::ToolError> {
            let id = request.id.clone();
            let swarm = request.swarm.as_ref().expect("swarm metadata");
            let index = swarm.index;
            let status_tx = swarm.status_tx.as_ref().expect("scheduler status channel");
            self.requests.lock().unwrap().push((index, id.clone()));
            status_tx
                .send(SubagentStatusEvent::ProviderRequestStarted {
                    subagent_id: id.clone(),
                })
                .unwrap();
            if self.rate_limited_indices.contains(&index) {
                let (decision_tx, mut decision_rx) = mpsc::unbounded_channel();
                status_tx
                    .send(SubagentStatusEvent::RateLimitWaiting {
                        subagent_id: id.clone(),
                        attempt: 1,
                        decision_tx,
                    })
                    .unwrap();
                let decision = decision_rx.recv().await.expect("scheduler decision");
                self.decisions.lock().unwrap().push((id.clone(), decision));
                if decision == SubagentRateLimitDecision::Fail {
                    return Ok(SubagentResult {
                        success: false,
                        subagent_id: id.clone(),
                        child_session_id: id,
                        error: Some("rate limited".to_string()),
                        ..Default::default()
                    });
                }
                status_tx
                    .send(SubagentStatusEvent::RateLimitRetrying {
                        subagent_id: id.clone(),
                        attempt: 1,
                    })
                    .unwrap();
                status_tx
                    .send(SubagentStatusEvent::ProviderRequestStarted {
                        subagent_id: id.clone(),
                    })
                    .unwrap();
            }
            Ok(SubagentResult {
                success: true,
                subagent_id: id.clone(),
                child_session_id: id,
                output: Arc::from("ok"),
                ..Default::default()
            })
        }

        async fn query(&self, _: &str, _: bool, _: Option<u64>) -> Option<SubagentSnapshot> {
            None
        }

        async fn cancel(&self, _: &str) -> SubagentCancelOutcome {
            SubagentCancelOutcome::NotFound
        }

        async fn validate_type(&self, _: &str, _: &str) -> SubagentValidateTypeOutcome {
            SubagentValidateTypeOutcome::Ok
        }

        async fn describe_subagent_type(
            &self,
            _: &str,
            _: Option<&str>,
            _: &str,
        ) -> SubagentDescribeOutcome {
            SubagentDescribeOutcome::Unavailable
        }
    }

    fn item_members(count: usize) -> Vec<PlannedMember> {
        (0..count)
            .map(|index| PlannedMember {
                index: index as u32,
                item: Some(index.to_string()),
                prompt: index.to_string(),
                resume_from: None,
                mode: MemberMode::New,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_initial_burst_and_sixth_launch_are_ramped() {
        let backend = Arc::new(ImmediateBackend::default());
        let task = tokio::spawn(run_scheduler(
            backend.clone(),
            item_members(6),
            context(),
            None,
            None,
        ));
        tokio::task::yield_now().await;
        assert_eq!(backend.requests.lock().unwrap().len(), 5);
        tokio::time::advance(Duration::from_millis(699)).await;
        tokio::task::yield_now().await;
        assert_eq!(backend.requests.lock().unwrap().len(), 5);
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(backend.requests.lock().unwrap().len(), 6);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_honors_configured_active_cap() {
        let backend = Arc::new(HoldingBackend::default());
        let task = tokio::spawn(run_scheduler(
            backend.clone(),
            item_members(7),
            context(),
            Some(2),
            None,
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(backend.requests.lock().unwrap().len(), 2);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_retries_same_agent_before_launching_new_work() {
        let backend = Arc::new(RateLimitBackend::new([0]));
        let task = tokio::spawn(run_scheduler(
            backend.clone(),
            item_members(6),
            context(),
            None,
            None,
        ));
        tokio::task::yield_now().await;
        assert_eq!(backend.requests.lock().unwrap().len(), 5);
        assert!(backend.decisions.lock().unwrap().is_empty());

        tokio::time::advance(Duration::from_millis(2_999)).await;
        tokio::task::yield_now().await;
        assert_eq!(backend.requests.lock().unwrap().len(), 5);
        assert!(backend.decisions.lock().unwrap().is_empty());

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        let retry_id = backend.decisions.lock().unwrap()[0].0.clone();
        let first_id = backend
            .requests
            .lock()
            .unwrap()
            .iter()
            .find(|(index, _)| *index == 0)
            .unwrap()
            .1
            .clone();
        assert_eq!(retry_id, first_id);
        assert_eq!(backend.requests.lock().unwrap().len(), 5);

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert_eq!(backend.requests.lock().unwrap().len(), 6);
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_starts_only_one_rate_phase_task_per_global_gate() {
        let backend = Arc::new(RateLimitBackend::new([0, 1]));
        let task = tokio::spawn(run_scheduler(
            backend.clone(),
            item_members(6),
            context(),
            None,
            None,
        ));
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(backend.decisions.lock().unwrap().len(), 1);

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert_eq!(backend.decisions.lock().unwrap().len(), 2);

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_fails_the_only_unfinished_rate_limited_member() {
        let backend = Arc::new(RateLimitBackend::new([0]));
        let results = run_scheduler(backend.clone(), item_members(1), context(), None, None).await;
        let request_id = backend.requests.lock().unwrap()[0].1.clone();
        assert_eq!(
            backend.decisions.lock().unwrap().as_slice(),
            &[(request_id, SubagentRateLimitDecision::Fail)]
        );
        assert_eq!(results[0].outcome, MemberOutcome::Failed);
        assert_eq!(results[0].body, "rate limited");
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_drains_pending_work_after_active_set_empties() {
        let backend = Arc::new(ImmediateBackend::default());
        let task = tokio::spawn(run_scheduler(
            backend.clone(),
            item_members(6),
            context(),
            None,
            None,
        ));
        tokio::task::yield_now().await;
        tokio::time::advance(RAMP_INTERVAL).await;
        tokio::task::yield_now().await;
        let results = task.await.unwrap();
        assert_eq!(backend.requests.lock().unwrap().len(), 6);
        assert_eq!(results.len(), 6);
    }
}
