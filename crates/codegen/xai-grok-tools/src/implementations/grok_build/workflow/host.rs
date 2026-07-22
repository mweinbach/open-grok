//! Workflow host — the code-mode session delegate that services `agent()`
//! calls from the workflow script by spawning real subagents through the
//! shared [`SubagentBackend`], plus the `notify()` side channel that carries
//! `log()`/`phase()` progress out of the script.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use xai_grok_code_mode_protocol::{
    CellId, CodeModeNestedToolCall, CodeModeSessionDelegate, NotificationFuture,
    ToolInvocationFuture,
};
use xai_tool_types::SubagentIsolationMode;

use crate::implementations::grok_build::task::backend::SubagentBackend;
use crate::implementations::grok_build::task::types::{
    ModelOverrideProvenance, SubagentRequest, SubagentResult, SubagentRuntimeOverrides,
    SwarmMemberMeta, TaskModelValidator,
};
use crate::notification::handle::ToolNotificationHandle;
use crate::notification::types::WorkflowProgress;

/// Wire name of the nested tool the prelude's `agent()` resolves to.
pub const WORKFLOW_AGENT_NESTED_TOOL: &str = "__wf_agent";

/// Lifetime cap on `agent()` calls per workflow run — a runaway-loop backstop.
pub const MAX_WORKFLOW_AGENTS: u32 = 1000;

/// Cap on retained progress-log lines.
const PROGRESS_LOG_CAP: usize = 400;

/// Rendered progress tail sent with each notification.
const PROGRESS_TAIL_LINES: usize = 40;

/// One `agent()` invocation as encoded by the prelude.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentCallInput {
    pub index: u32,
    pub prompt: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub schema: Option<JsonValue>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub isolation: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
}

/// Journal line persisted per completed `agent()` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub index: u32,
    pub key: String,
    pub ok: bool,
    pub value: JsonValue,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub tokens_used: u64,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Done,
    Failed,
    Cached,
}

#[derive(Debug, Clone)]
pub struct AgentProgressEntry {
    pub index: u32,
    pub label: String,
    pub phase: Option<String>,
    pub status: AgentStatus,
    pub detail: Option<String>,
}

#[derive(Debug, Default)]
pub struct ProgressState {
    pub current_phase: Option<String>,
    pub agents: Vec<AgentProgressEntry>,
    pub log: VecDeque<String>,
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    pub cached: usize,
}

impl ProgressState {
    fn push_line(&mut self, line: String) {
        if self.log.len() >= PROGRESS_LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn upsert_agent(&mut self, entry: AgentProgressEntry) {
        match self
            .agents
            .iter_mut()
            .find(|existing| existing.index == entry.index)
        {
            Some(existing) => *existing = entry,
            None => self.agents.push(entry),
        }
        self.running = 0;
        self.done = 0;
        self.failed = 0;
        self.cached = 0;
        for agent in &self.agents {
            match agent.status {
                AgentStatus::Running => self.running += 1,
                AgentStatus::Done => self.done += 1,
                AgentStatus::Failed => self.failed += 1,
                AgentStatus::Cached => self.cached += 1,
            }
        }
    }

    pub fn render_tail(&self) -> String {
        let mut out = String::new();
        let skip = self.log.len().saturating_sub(PROGRESS_TAIL_LINES);
        for line in self.log.iter().skip(skip) {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    pub fn summary_line(&self) -> String {
        let mut parts = vec![format!("{} running", self.running)];
        parts.push(format!("{} done", self.done));
        if self.cached > 0 {
            parts.push(format!("{} cached", self.cached));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        let phase = self
            .current_phase
            .as_deref()
            .map(|phase| format!(" · phase: {phase}"))
            .unwrap_or_default();
        format!("agents: {}{phase}", parts.join(", "))
    }
}

/// How journaled results from a prior run are matched to this run's calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResumeMode {
    /// Replay a call only when its `(prompt, behavioral opts)` key is
    /// unchanged. Safe default: any semantic edit re-runs the call.
    #[default]
    Exact,
    /// Replay by call position (the prelude's sequence index), trusting the
    /// operator that the script's call structure is unchanged even where the
    /// wording was edited. This is how "go back to a specific point" works
    /// after editing a script: earlier positions replay, later ones re-run.
    Positional,
}

/// Replay boundary: the last prior-run point that is still trusted.
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeBoundary {
    /// Replay journal entries with `index <= n`.
    Index(u32),
    /// Replay through the last journaled entry of the named phase (falling
    /// back to an agent-label match), resolved against the source journal.
    Point(String),
}

/// Journal entries from a prior run, indexed for the chosen resume mode.
#[derive(Debug, Default)]
pub struct ReplayPlan {
    mode: ResumeMode,
    by_key: HashMap<String, VecDeque<JournalEntry>>,
    by_index: HashMap<u32, JournalEntry>,
}

impl ReplayPlan {
    /// Build a plan from a prior run's successful entries. `boundary`
    /// restricts which entries are trusted for replay; entries past it are
    /// dropped so their calls run fresh.
    pub fn build(
        entries: Vec<JournalEntry>,
        mode: ResumeMode,
        boundary: Option<ResumeBoundary>,
    ) -> Result<Self, String> {
        let max_index = match boundary {
            None => None,
            Some(ResumeBoundary::Index(index)) => Some(index),
            Some(ResumeBoundary::Point(ref point)) => {
                let wanted = point.trim().to_lowercase();
                let matches = |candidate: &Option<String>| {
                    candidate
                        .as_deref()
                        .is_some_and(|value| value.trim().to_lowercase() == wanted)
                };
                let phase_max = entries
                    .iter()
                    .filter(|entry| matches(&entry.phase))
                    .map(|entry| entry.index)
                    .max();
                let label_max = entries
                    .iter()
                    .filter(|entry| matches(&entry.label))
                    .map(|entry| entry.index)
                    .max();
                let resolved = phase_max.or(label_max);
                let Some(resolved) = resolved else {
                    let mut known: Vec<String> = entries
                        .iter()
                        .flat_map(|entry| entry.phase.iter().chain(entry.label.iter()).cloned())
                        .collect();
                    known.sort();
                    known.dedup();
                    return Err(format!(
                        "resume_through `{point}` matches no phase or agent label in the \
                         source journal. Known points: {}",
                        if known.is_empty() {
                            "(none journaled)".to_string()
                        } else {
                            known.join(", ")
                        }
                    ));
                };
                Some(resolved)
            }
        };

        let mut plan = Self {
            mode,
            ..Self::default()
        };
        for entry in entries {
            if !entry.ok {
                continue;
            }
            if let Some(max_index) = max_index
                && entry.index > max_index
            {
                continue;
            }
            let migrated_key = migrate_journal_key(&entry.key);
            plan.by_index.entry(entry.index).or_insert_with(|| {
                let mut indexed = entry.clone();
                indexed.key = migrated_key.clone();
                indexed
            });
            let mut keyed = entry;
            keyed.key = migrated_key.clone();
            plan.by_key
                .entry(migrated_key)
                .or_default()
                .push_back(keyed);
        }
        Ok(plan)
    }

    fn take(&mut self, call: &AgentCallInput, key: &str) -> Option<JournalEntry> {
        match self.mode {
            ResumeMode::Exact => self.by_key.get_mut(key).and_then(VecDeque::pop_front),
            ResumeMode::Positional => self.by_index.remove(&call.index),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.by_index.is_empty()
    }
}

/// Re-key a journal entry written by an older binary whose keys still carried
/// the display-only `label`/`phase` fields. Produces exactly the serialization
/// of [`WorkflowHost::journal_key`] for any serde_json configuration.
fn migrate_journal_key(old: &str) -> String {
    let Ok(value) = serde_json::from_str::<JsonValue>(old) else {
        return old.to_string();
    };
    let field = |name: &str| value.get(name).cloned().unwrap_or(JsonValue::Null);
    json!({
        "prompt": field("prompt"),
        "schema": field("schema"),
        "model": field("model"),
        "effort": field("effort"),
        "isolation": field("isolation"),
        "agent_type": field("agent_type"),
    })
    .to_string()
}

/// Everything the workflow tool wires into the host at run start.
pub struct WorkflowHostConfig {
    pub backend: Arc<dyn SubagentBackend>,
    pub model_validator: Option<TaskModelValidator>,
    pub parent_session_id: String,
    pub parent_prompt_id: Option<String>,
    pub run_id: String,
    pub workflow_name: String,
    pub tool_call_id: String,
    pub notifications: ToolNotificationHandle,
    pub concurrency: usize,
    pub per_agent_timeout: Option<Duration>,
    pub token_budget: Option<u64>,
    pub journal_path: Option<std::path::PathBuf>,
    /// When set (background runs), every progress line is also appended
    /// here so the run reads like any other background task's output file.
    pub progress_path: Option<std::path::PathBuf>,
    pub replay: ReplayPlan,
}

pub struct WorkflowHost {
    backend: Arc<dyn SubagentBackend>,
    model_validator: Option<TaskModelValidator>,
    parent_session_id: String,
    parent_prompt_id: Option<String>,
    run_id: String,
    workflow_name: String,
    tool_call_id: String,
    notifications: ToolNotificationHandle,
    semaphore: Arc<Semaphore>,
    per_agent_timeout: Option<Duration>,
    token_budget: Option<u64>,
    tokens_spent: AtomicU64,
    agents_started: AtomicU32,
    validated_types: parking_lot::Mutex<HashSet<String>>,
    state: parking_lot::Mutex<ProgressState>,
    journal: parking_lot::Mutex<Option<std::fs::File>>,
    progress_file: parking_lot::Mutex<Option<std::fs::File>>,
    replay: parking_lot::Mutex<ReplayPlan>,
}

impl WorkflowHost {
    pub fn new(config: WorkflowHostConfig) -> Arc<Self> {
        let append = |path: &std::path::PathBuf| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        };
        let journal = config.journal_path.as_ref().and_then(append);
        let progress_file = config.progress_path.as_ref().and_then(append);
        Arc::new(Self {
            backend: config.backend,
            model_validator: config.model_validator,
            parent_session_id: config.parent_session_id,
            parent_prompt_id: config.parent_prompt_id,
            run_id: config.run_id,
            workflow_name: config.workflow_name,
            tool_call_id: config.tool_call_id,
            notifications: config.notifications,
            semaphore: Arc::new(Semaphore::new(config.concurrency.max(1))),
            per_agent_timeout: config.per_agent_timeout,
            token_budget: config.token_budget,
            tokens_spent: AtomicU64::new(0),
            agents_started: AtomicU32::new(0),
            validated_types: parking_lot::Mutex::new(HashSet::new()),
            state: parking_lot::Mutex::new(ProgressState::default()),
            journal: parking_lot::Mutex::new(journal),
            progress_file: parking_lot::Mutex::new(progress_file),
            replay: parking_lot::Mutex::new(config.replay),
        })
    }

    pub fn tokens_spent(&self) -> u64 {
        self.tokens_spent.load(Ordering::Relaxed)
    }

    pub fn agents_started(&self) -> u32 {
        self.agents_started.load(Ordering::Relaxed)
    }

    pub fn snapshot<T>(&self, read: impl FnOnce(&ProgressState) -> T) -> T {
        read(&self.state.lock())
    }

    fn emit_progress(&self, line: Option<String>) {
        let text = {
            let mut state = self.state.lock();
            if let Some(line) = &line {
                state.push_line(line.clone());
            }
            format!("{}\n{}", state.summary_line(), state.render_tail())
        };
        if let Some(line) = &line {
            use std::io::Write as _;
            let mut progress_file = self.progress_file.lock();
            if let Some(file) = progress_file.as_mut() {
                let _ = writeln!(file, "{line}");
            }
        }
        self.notifications.send_workflow_progress(WorkflowProgress {
            tool_call_id: self.tool_call_id.clone(),
            run_id: self.run_id.clone(),
            workflow_name: self.workflow_name.clone(),
            text,
        });
    }

    fn record_phase(&self, title: String) {
        self.state.lock().current_phase = Some(title.clone());
        self.emit_progress(Some(format!("── phase: {title}")));
    }

    fn record_log(&self, message: String) {
        self.emit_progress(Some(format!("• {message}")));
    }

    fn record_agent(&self, entry: AgentProgressEntry, line: String) {
        self.state.lock().upsert_agent(entry);
        self.emit_progress(Some(line));
    }

    fn journal_entry(&self, entry: &JournalEntry) {
        use std::io::Write as _;
        let mut journal = self.journal.lock();
        if let Some(file) = journal.as_mut()
            && let Ok(line) = serde_json::to_string(entry)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    /// Canonical journal key for one `agent()` call: prompt + every behavioral
    /// option, serialized with a fixed field order. `index` is deliberately
    /// excluded so reordered-but-unchanged calls still replay, and `label` /
    /// `phase` are excluded because they are display-only — renaming a label
    /// or regrouping phases must not invalidate journaled results.
    fn journal_key(call: &AgentCallInput) -> String {
        json!({
            "prompt": call.prompt,
            "schema": call.schema,
            "model": call.model,
            "effort": call.effort,
            "isolation": call.isolation,
            "agent_type": call.agent_type,
        })
        .to_string()
    }

    fn display_label(call: &AgentCallInput) -> String {
        match &call.label {
            Some(label) if !label.trim().is_empty() => label.clone(),
            _ => {
                let head: String = call.prompt.chars().take(48).collect();
                if call.prompt.chars().count() > 48 {
                    format!("agent#{}: {head}…", call.index)
                } else {
                    format!("agent#{}: {head}", call.index)
                }
            }
        }
    }

    async fn handle_agent(
        &self,
        input: Option<JsonValue>,
        cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        let input = input.ok_or_else(|| "agent(): missing call payload".to_string())?;
        let call: AgentCallInput = serde_json::from_value(input)
            .map_err(|error| format!("agent(): invalid call payload: {error}"))?;

        if let Some(total) = self.token_budget {
            let spent = self.tokens_spent();
            if spent >= total {
                return Err(format!(
                    "workflow token budget exhausted ({spent}/{total} tokens)"
                ));
            }
        }
        let started = self.agents_started.fetch_add(1, Ordering::Relaxed);
        if started >= MAX_WORKFLOW_AGENTS {
            return Err(format!(
                "workflow agent cap reached ({MAX_WORKFLOW_AGENTS} agents per run)"
            ));
        }

        let label = Self::display_label(&call);
        let key = Self::journal_key(&call);

        // Journal replay: a trusted prior result (unchanged key, or same
        // position under positional resume) resolves instantly instead of
        // spawning.
        let replayed = self.replay.lock().take(&call, &key);
        if let Some(entry) = replayed {
            self.record_agent(
                AgentProgressEntry {
                    index: call.index,
                    label: label.clone(),
                    phase: call.phase.clone(),
                    status: AgentStatus::Cached,
                    detail: None,
                },
                format!("↺ {label} (replayed from journal)"),
            );
            // Re-journal the replayed result under this run so resume chains:
            // resuming from THIS run must replay everything it served, not
            // only the calls that ran fresh here.
            self.journal_entry(&JournalEntry {
                index: call.index,
                key,
                ..entry.clone()
            });
            return Ok(json!({
                "ok": true,
                "value": entry.value,
                "error": JsonValue::Null,
                "budget_spent": self.tokens_spent(),
                "agent_id": entry.agent_id,
                "cached": true,
            }));
        }

        // Fail fast on unknown agent types and rejected model slugs — these
        // are programmer errors in the script, so they throw in JS.
        let subagent_type = call
            .agent_type
            .clone()
            .unwrap_or_else(xai_tool_types::default_subagent_type);
        self.validate_agent_type(&subagent_type).await?;
        if let Some(requested) = call.model.as_deref() {
            match &self.model_validator {
                Some(validator) => {
                    if let Some(error) = validator.error_for(requested) {
                        return Err(error);
                    }
                }
                None => {
                    return Err(format!(
                        "agent(): cannot validate model `{requested}`: model catalog validator \
                         is unavailable"
                    ));
                }
            }
        }
        let isolation = match call.isolation.as_deref() {
            None | Some("none") => None,
            Some("worktree") => Some(SubagentIsolationMode::Worktree),
            Some(other) => {
                return Err(format!(
                    "agent(): invalid isolation `{other}` (expected \"worktree\")"
                ));
            }
        };

        let permit = tokio::select! {
            permit = self.semaphore.clone().acquire_owned() => {
                permit.map_err(|_| "workflow host shut down".to_string())?
            }
            _ = cancellation_token.cancelled() => {
                return Err("workflow cancelled".to_string());
            }
        };

        self.record_agent(
            AgentProgressEntry {
                index: call.index,
                label: label.clone(),
                phase: call.phase.clone(),
                status: AgentStatus::Running,
                detail: None,
            },
            format!("▶ {label}"),
        );

        let prompt = match &call.schema {
            Some(schema) => prompt_with_output_contract(&call.prompt, schema),
            None => call.prompt.clone(),
        };

        let first = self
            .spawn_once(
                &call,
                &subagent_type,
                isolation,
                prompt,
                &cancellation_token,
            )
            .await;
        let outcome = match first {
            Ok((agent_id, result)) => {
                self.settle_agent(&call, &key, &label, agent_id, result, &cancellation_token)
                    .await
            }
            Err(error) => AgentOutcome::failed(None, error),
        };
        drop(permit);

        match &outcome.error {
            None => self.record_agent(
                AgentProgressEntry {
                    index: call.index,
                    label: label.clone(),
                    phase: call.phase.clone(),
                    status: AgentStatus::Done,
                    detail: None,
                },
                format!(
                    "✓ {label} ({}, {} tok)",
                    format_duration_ms(outcome.duration_ms),
                    outcome.tokens_used
                ),
            ),
            Some(error) => self.record_agent(
                AgentProgressEntry {
                    index: call.index,
                    label: label.clone(),
                    phase: call.phase.clone(),
                    status: AgentStatus::Failed,
                    detail: Some(error.clone()),
                },
                format!("✗ {label}: {error}"),
            ),
        }

        if outcome.error.is_none() {
            self.journal_entry(&JournalEntry {
                index: call.index,
                key,
                ok: true,
                value: outcome.value.clone(),
                error: None,
                label: call.label.clone(),
                phase: call.phase.clone(),
                tokens_used: outcome.tokens_used,
                duration_ms: outcome.duration_ms,
                agent_id: outcome.agent_id.clone(),
            });
        }

        Ok(json!({
            "ok": outcome.error.is_none(),
            "value": outcome.value,
            "error": outcome.error,
            "budget_spent": self.tokens_spent(),
            "agent_id": outcome.agent_id,
            "cached": false,
        }))
    }

    async fn validate_agent_type(&self, subagent_type: &str) -> Result<(), String> {
        if self.validated_types.lock().contains(subagent_type) {
            return Ok(());
        }
        use crate::implementations::grok_build::task::types::SubagentValidateTypeOutcome;
        match self
            .backend
            .validate_type(subagent_type, &self.parent_session_id)
            .await
        {
            SubagentValidateTypeOutcome::Ok => {
                self.validated_types
                    .lock()
                    .insert(subagent_type.to_string());
                Ok(())
            }
            SubagentValidateTypeOutcome::Unknown { available } => {
                let suffix = if available.is_empty() {
                    String::new()
                } else {
                    format!(". Available types: {}", available.join(", "))
                };
                Err(format!(
                    "agent(): unknown agentType `{subagent_type}`{suffix}"
                ))
            }
            SubagentValidateTypeOutcome::Disabled => {
                Err(format!("agent(): agentType `{subagent_type}` is disabled"))
            }
            SubagentValidateTypeOutcome::NotAllowed { allowed } => Err(format!(
                "agent(): this agent can only spawn: {}; `{subagent_type}` not allowed",
                allowed.join(", ")
            )),
            SubagentValidateTypeOutcome::ValidationUnavailable => Err(
                "agent(): cannot validate agentType: the subagent coordinator is unreachable"
                    .to_string(),
            ),
        }
    }

    async fn spawn_once(
        &self,
        call: &AgentCallInput,
        subagent_type: &str,
        isolation: Option<SubagentIsolationMode>,
        prompt: String,
        cancellation_token: &CancellationToken,
    ) -> Result<(String, SubagentResult), String> {
        self.spawn_request(
            call,
            subagent_type,
            isolation,
            prompt,
            None,
            cancellation_token,
        )
        .await
    }

    async fn spawn_request(
        &self,
        call: &AgentCallInput,
        subagent_type: &str,
        isolation: Option<SubagentIsolationMode>,
        prompt: String,
        resume_from: Option<String>,
        cancellation_token: &CancellationToken,
    ) -> Result<(String, SubagentResult), String> {
        let agent_id = uuid::Uuid::now_v7().to_string();
        let (placeholder_tx, _placeholder_rx) = tokio::sync::oneshot::channel();
        let request = SubagentRequest {
            id: agent_id.clone(),
            prompt,
            description: Self::display_label(call),
            subagent_type: subagent_type.to_string(),
            parent_session_id: self.parent_session_id.clone(),
            parent_prompt_id: self.parent_prompt_id.clone(),
            // Swarm metadata groups workflow agents into one cohort in the UI
            // and opts them out of the foreground await-budget auto-background.
            swarm: Some(SwarmMemberMeta {
                swarm_id: self.run_id.clone(),
                description: self.workflow_name.clone(),
                index: call.index,
                item: call.label.clone(),
                expected_members: 0,
                status_tx: None,
            }),
            resume_from,
            cwd: None,
            runtime_overrides: SubagentRuntimeOverrides {
                model: call.model.clone(),
                model_override_provenance: ModelOverrideProvenance::Tool,
                reasoning_effort: call.effort.clone(),
                persona: None,
                capability_mode: None,
                isolation,
                harness_agent_type: None,
                completion_output_cap: None,
                spawn_depth: None,
            },
            run_in_background: false,
            surface_completion: false,
            fork_context: false,
            result_tx: placeholder_tx,
        };

        let spawn = self.backend.spawn(request);
        tokio::pin!(spawn);
        let result = match self.per_agent_timeout {
            Some(timeout) => {
                tokio::select! {
                    result = &mut spawn => result,
                    _ = cancellation_token.cancelled() => {
                        let _ = self.backend.cancel(&agent_id).await;
                        return Err("workflow cancelled".to_string());
                    }
                    _ = tokio::time::sleep(timeout) => {
                        let _ = self.backend.cancel(&agent_id).await;
                        return Ok((
                            agent_id.clone(),
                            timed_out_result(agent_id.clone(), timeout),
                        ));
                    }
                }
            }
            None => {
                tokio::select! {
                    result = &mut spawn => result,
                    _ = cancellation_token.cancelled() => {
                        let _ = self.backend.cancel(&agent_id).await;
                        return Err("workflow cancelled".to_string());
                    }
                }
            }
        };
        result
            .map(|result| (agent_id, result))
            .map_err(|error| format!("agent spawn failed: {error}"))
    }

    /// Fold a completed spawn into an outcome, applying token accounting and
    /// the schema output contract (with one corrective resume retry).
    async fn settle_agent(
        &self,
        call: &AgentCallInput,
        _key: &str,
        label: &str,
        agent_id: String,
        result: SubagentResult,
        cancellation_token: &CancellationToken,
    ) -> AgentOutcome {
        self.tokens_spent
            .fetch_add(result.tokens_used, Ordering::Relaxed);

        if result.cancelled {
            return AgentOutcome::failed_with(
                Some(agent_id),
                "agent cancelled".to_string(),
                &result,
            );
        }
        if !result.success {
            let error = result
                .error
                .clone()
                .unwrap_or_else(|| "agent failed".to_string());
            return AgentOutcome::failed_with(Some(agent_id), error, &result);
        }

        let Some(schema) = &call.schema else {
            return AgentOutcome {
                agent_id: Some(agent_id),
                value: JsonValue::String(result.output.to_string()),
                error: None,
                tokens_used: result.tokens_used,
                duration_ms: result.duration_ms,
            };
        };

        if let Some(value) = extract_schema_value(&result.output, schema) {
            return AgentOutcome {
                agent_id: Some(agent_id),
                value,
                error: None,
                tokens_used: result.tokens_used,
                duration_ms: result.duration_ms,
            };
        }

        // One corrective retry: resume the same child and ask for JSON only.
        self.record_log(format!(
            "{label}: output did not match the schema; asking the agent to reformat"
        ));
        let corrective_prompt = format!(
            "Your previous reply was not a single JSON value matching the required schema. \
             Reply now with ONLY that JSON value — no prose, no code fences.\n\nSchema:\n{}",
            serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
        );
        let retry = self
            .spawn_request(
                call,
                &call
                    .agent_type
                    .clone()
                    .unwrap_or_else(xai_tool_types::default_subagent_type),
                None,
                corrective_prompt,
                Some(agent_id.clone()),
                cancellation_token,
            )
            .await;
        match retry {
            Ok((retry_id, retry_result)) => {
                self.tokens_spent
                    .fetch_add(retry_result.tokens_used, Ordering::Relaxed);
                let total_tokens = result.tokens_used + retry_result.tokens_used;
                let total_duration = result.duration_ms + retry_result.duration_ms;
                if retry_result.success
                    && let Some(value) = extract_schema_value(&retry_result.output, schema)
                {
                    return AgentOutcome {
                        agent_id: Some(retry_id),
                        value,
                        error: None,
                        tokens_used: total_tokens,
                        duration_ms: total_duration,
                    };
                }
                AgentOutcome {
                    agent_id: Some(retry_id),
                    value: JsonValue::Null,
                    error: Some("agent output did not match the required schema".to_string()),
                    tokens_used: total_tokens,
                    duration_ms: total_duration,
                }
            }
            Err(error) => AgentOutcome {
                agent_id: Some(agent_id),
                value: JsonValue::Null,
                error: Some(format!("schema retry failed: {error}")),
                tokens_used: result.tokens_used,
                duration_ms: result.duration_ms,
            },
        }
    }
}

struct AgentOutcome {
    agent_id: Option<String>,
    value: JsonValue,
    error: Option<String>,
    tokens_used: u64,
    duration_ms: u64,
}

impl AgentOutcome {
    fn failed(agent_id: Option<String>, error: String) -> Self {
        Self {
            agent_id,
            value: JsonValue::Null,
            error: Some(error),
            tokens_used: 0,
            duration_ms: 0,
        }
    }

    fn failed_with(agent_id: Option<String>, error: String, result: &SubagentResult) -> Self {
        Self {
            agent_id,
            value: JsonValue::Null,
            error: Some(error),
            tokens_used: result.tokens_used,
            duration_ms: result.duration_ms,
        }
    }
}

fn timed_out_result(agent_id: String, timeout: Duration) -> SubagentResult {
    SubagentResult {
        success: false,
        output: Arc::from(""),
        error: Some(format!("agent timed out after {}s", timeout.as_secs())),
        cancelled: false,
        subagent_id: agent_id.clone(),
        child_session_id: agent_id,
        tool_calls: 0,
        turns: 0,
        duration_ms: timeout.as_millis() as u64,
        tokens_used: 0,
        worktree_path: None,
        backgrounded: false,
    }
}

fn format_duration_ms(duration_ms: u64) -> String {
    if duration_ms >= 60_000 {
        format!(
            "{}m{}s",
            duration_ms / 60_000,
            (duration_ms % 60_000) / 1000
        )
    } else {
        format!("{:.1}s", duration_ms as f64 / 1000.0)
    }
}

fn prompt_with_output_contract(prompt: &str, schema: &JsonValue) -> String {
    format!(
        "{prompt}\n\n<output_contract>\nYour FINAL message must be exactly one JSON value that \
         validates against this JSON Schema — no prose before or after it, no code fences:\n{}\n\
         </output_contract>",
        serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
    )
}

/// Extract the first JSON value from agent output text and shallow-check it
/// against the requested schema's top-level `type`.
pub fn extract_schema_value(output: &str, schema: &JsonValue) -> Option<JsonValue> {
    let value = extract_json_value(output)?;
    match schema.get("type").and_then(JsonValue::as_str) {
        Some("object") if !value.is_object() => None,
        Some("array") if !value.is_array() => None,
        Some("string") if !value.is_string() => None,
        Some("number") if !value.is_number() => None,
        Some("integer") if !value.is_i64() && !value.is_u64() => None,
        Some("boolean") if !value.is_boolean() => None,
        _ => Some(value),
    }
}

fn extract_json_value(text: &str) -> Option<JsonValue> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<JsonValue>(trimmed) {
        return Some(value);
    }
    // Fenced block: take the contents of the first code fence.
    if let Some(start) = trimmed.find("```") {
        let after = &trimmed[start + 3..];
        let body_start = after.find('\n').map(|nl| nl + 1).unwrap_or(0);
        let body = &after[body_start..];
        let body = body.split("```").next().unwrap_or(body).trim();
        if let Ok(value) = serde_json::from_str::<JsonValue>(body) {
            return Some(value);
        }
    }
    // First parseable JSON value starting at any `{` or `[`.
    for (index, ch) in trimmed.char_indices() {
        if ch == '{' || ch == '[' {
            let mut stream =
                serde_json::Deserializer::from_str(&trimmed[index..]).into_iter::<JsonValue>();
            if let Some(Ok(value)) = stream.next() {
                return Some(value);
            }
        }
    }
    None
}

/// Envelope pushed through `notify()` by the prelude.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum NotifyEnvelope {
    Log { message: String },
    Phase { title: String },
}

impl CodeModeSessionDelegate for WorkflowHost {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            match invocation.tool_name.name.as_str() {
                WORKFLOW_AGENT_NESTED_TOOL => {
                    self.handle_agent(invocation.input, cancellation_token)
                        .await
                }
                other => Err(format!(
                    "workflow scripts can only call the built-in hooks (unknown tool `{other}`)"
                )),
            }
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            match serde_json::from_str::<NotifyEnvelope>(&text) {
                Ok(NotifyEnvelope::Log { message }) => self.record_log(message),
                Ok(NotifyEnvelope::Phase { title }) => self.record_phase(title),
                Err(_) => self.record_log(text),
            }
            Ok(())
        })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

/// Load a prior run's journal entries for `resume_from_run_id`.
pub fn load_journal_entries(path: &std::path::Path) -> Vec<JournalEntry> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            serde_json::from_str::<JournalEntry>(line).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(prompt: &str) -> AgentCallInput {
        AgentCallInput {
            index: 0,
            prompt: prompt.to_string(),
            label: None,
            phase: None,
            schema: None,
            model: None,
            effort: None,
            isolation: None,
            agent_type: None,
        }
    }

    #[test]
    fn journal_key_ignores_index_label_and_phase_but_not_prompt_or_opts() {
        let mut a = call("find bugs");
        a.index = 1;
        let mut b = call("find bugs");
        b.index = 7;
        b.label = Some("renamed".to_string());
        b.phase = Some("Regrouped".to_string());
        assert_eq!(
            WorkflowHost::journal_key(&a),
            WorkflowHost::journal_key(&b),
            "display-only fields must not invalidate journaled results"
        );

        let mut c = call("find bugs");
        c.model = Some("some-model".to_string());
        assert_ne!(WorkflowHost::journal_key(&a), WorkflowHost::journal_key(&c));
        let d = call("find other bugs");
        assert_ne!(WorkflowHost::journal_key(&a), WorkflowHost::journal_key(&d));
    }

    fn entry(index: u32, prompt: &str, phase: Option<&str>, label: Option<&str>) -> JournalEntry {
        JournalEntry {
            index,
            key: WorkflowHost::journal_key(&AgentCallInput {
                prompt: prompt.to_string(),
                ..call(prompt)
            }),
            ok: true,
            value: json!(format!("result-{index}")),
            error: None,
            label: label.map(str::to_string),
            phase: phase.map(str::to_string),
            tokens_used: 1,
            duration_ms: 1,
            agent_id: Some(format!("agent-{index}")),
        }
    }

    #[test]
    fn positional_plan_replays_by_index_despite_reworded_prompts() {
        let entries = vec![
            entry(0, "old wording A", Some("P1"), None),
            entry(1, "old wording B", Some("P2"), None),
        ];
        let mut plan =
            ReplayPlan::build(entries, ResumeMode::Positional, None).expect("plan builds");

        let mut reworded = call("completely new wording");
        reworded.index = 0;
        let key = WorkflowHost::journal_key(&reworded);
        let hit = plan.take(&reworded, &key).expect("index 0 replays");
        assert_eq!(hit.value, json!("result-0"));

        let mut gap = call("whatever");
        gap.index = 5;
        assert!(
            plan.take(&gap, &key).is_none(),
            "unjournaled index runs fresh"
        );
    }

    #[test]
    fn exact_plan_requires_matching_key() {
        let source = call("stable prompt");
        let mut journaled = entry(0, "stable prompt", None, None);
        journaled.key = WorkflowHost::journal_key(&source);
        let mut plan =
            ReplayPlan::build(vec![journaled], ResumeMode::Exact, None).expect("plan builds");

        let mut reworded = call("different prompt");
        reworded.index = 0;
        let miss_key = WorkflowHost::journal_key(&reworded);
        assert!(plan.take(&reworded, &miss_key).is_none());

        let mut same = call("stable prompt");
        same.index = 3;
        let hit_key = WorkflowHost::journal_key(&same);
        assert!(
            plan.take(&same, &hit_key).is_some(),
            "key match replays at any index"
        );
    }

    #[test]
    fn resume_boundary_by_phase_label_and_index() {
        let entries = || {
            vec![
                entry(0, "plan", Some("Plan"), Some("sol-plan")),
                entry(1, "bootstrap", Some("Bootstrap"), Some("glm-bootstrap")),
                entry(2, "wave", Some("Waves"), Some("glm-w0")),
            ]
        };

        let plan = ReplayPlan::build(
            entries(),
            ResumeMode::Positional,
            Some(ResumeBoundary::Point("bootstrap".to_string())),
        )
        .expect("phase boundary resolves case-insensitively");
        assert_eq!(plan.by_index.len(), 2, "entries past the boundary drop");

        let plan = ReplayPlan::build(
            entries(),
            ResumeMode::Positional,
            Some(ResumeBoundary::Point("sol-plan".to_string())),
        )
        .expect("label fallback resolves");
        assert_eq!(plan.by_index.len(), 1);

        let plan = ReplayPlan::build(
            entries(),
            ResumeMode::Positional,
            Some(ResumeBoundary::Index(1)),
        )
        .expect("index boundary");
        assert_eq!(plan.by_index.len(), 2);

        let err = ReplayPlan::build(
            entries(),
            ResumeMode::Positional,
            Some(ResumeBoundary::Point("no-such-point".to_string())),
        )
        .unwrap_err();
        assert!(err.contains("Known points"), "{err}");
        assert!(err.contains("Bootstrap"), "{err}");
    }

    #[test]
    fn old_format_keys_with_label_and_phase_migrate() {
        // A .20/.21-era journal key that still embeds label/phase.
        let old_key = json!({
            "prompt": "stable prompt",
            "label": "old-label",
            "phase": "Old Phase",
            "schema": null,
            "model": null,
            "effort": null,
            "isolation": null,
            "agent_type": null,
        })
        .to_string();
        let mut journaled = entry(0, "stable prompt", Some("Old Phase"), Some("old-label"));
        journaled.key = old_key;
        let mut plan =
            ReplayPlan::build(vec![journaled], ResumeMode::Exact, None).expect("plan builds");

        let mut same = call("stable prompt");
        same.index = 0;
        let new_key = WorkflowHost::journal_key(&same);
        assert!(
            plan.take(&same, &new_key).is_some(),
            "migrated key must match the new key format"
        );
    }

    #[test]
    fn extracts_bare_fenced_and_embedded_json() {
        let schema = json!({"type": "object"});
        assert_eq!(
            extract_schema_value("{\"a\":1}", &schema),
            Some(json!({"a":1}))
        );
        assert_eq!(
            extract_schema_value("Here you go:\n```json\n{\"a\": 1}\n```\nDone.", &schema),
            Some(json!({"a":1}))
        );
        assert_eq!(
            extract_schema_value("The result is {\"a\": [1, 2]} as requested.", &schema),
            Some(json!({"a":[1,2]}))
        );
        assert_eq!(extract_schema_value("no json here", &schema), None);
    }

    #[test]
    fn schema_shallow_type_check_applies() {
        assert_eq!(
            extract_schema_value("[1,2]", &json!({"type": "object"})),
            None
        );
        assert_eq!(
            extract_schema_value("[1,2]", &json!({"type": "array"})),
            Some(json!([1, 2]))
        );
    }

    #[test]
    fn journal_load_and_plan_skip_failures_and_garbage() {
        let dir = std::env::temp_dir().join(format!("wf-journal-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("journal.jsonl");
        let ok = entry(0, "good", None, None);
        let failed = JournalEntry {
            index: 1,
            key: "k2".into(),
            ok: false,
            value: JsonValue::Null,
            error: Some("boom".into()),
            label: None,
            phase: None,
            tokens_used: 0,
            duration_ms: 0,
            agent_id: None,
        };
        let lines = format!(
            "{}\n{}\nnot json\n",
            serde_json::to_string(&ok).unwrap(),
            serde_json::to_string(&failed).unwrap()
        );
        std::fs::write(&path, lines).unwrap();
        let entries = load_journal_entries(&path);
        assert_eq!(entries.len(), 2, "loader keeps parseable entries");
        let plan = ReplayPlan::build(entries, ResumeMode::Positional, None).expect("plan builds");
        assert_eq!(plan.by_index.len(), 1, "failed entries never replay");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn progress_state_counts_and_tail() {
        let mut state = ProgressState::default();
        state.upsert_agent(AgentProgressEntry {
            index: 0,
            label: "a".into(),
            phase: None,
            status: AgentStatus::Running,
            detail: None,
        });
        state.upsert_agent(AgentProgressEntry {
            index: 1,
            label: "b".into(),
            phase: None,
            status: AgentStatus::Done,
            detail: None,
        });
        state.upsert_agent(AgentProgressEntry {
            index: 0,
            label: "a".into(),
            phase: None,
            status: AgentStatus::Failed,
            detail: Some("x".into()),
        });
        assert_eq!(state.running, 0);
        assert_eq!(state.done, 1);
        assert_eq!(state.failed, 1);
        for i in 0..500 {
            state.push_line(format!("line {i}"));
        }
        assert!(state.log.len() <= 400);
        assert!(state.render_tail().contains("line 499"));
    }
}
