//! System-reminder injection concern for `SessionActor`: reminder policy,
//! the TodoGate, date/interrupt reminders, and between-turn completion
//! reminders.
use super::*;
/// Owned snapshot returned by [`SessionActor::collect_todo_gate_input`].
///
/// The borrowed `TodoGateInput<'_>` consumed by [`evaluate_todo_gate`]
/// is built from this owned data via [`Self::as_input`], which performs
/// the "first N in_progress are backed" insertion-order partition.
///
/// Exposed as `pub` solely so the replay-trace integration test in
/// `tests/trace_replay.rs` can drive the gate against synthetic JSON
/// fixtures. Not part of the public API.
#[doc(hidden)]
pub struct CollectedTodoGateInput {
    /// Pairs of `(id, content, status)` in `TodoState.todo_items_with_ids()`
    /// (insertion) order — `IndexMap` preserves this so the partition
    /// between backed and unbacked in-progress items is deterministic.
    pub todos: Vec<(String, String, crate::tools::todo::TodoStatus)>,
    /// `|outstanding subagents| + |incomplete bash/monitor tasks|` at
    /// the moment of gate evaluation.
    pub backing_task_count: usize,
}
impl CollectedTodoGateInput {
    /// Borrowed view used by [`evaluate_todo_gate`]. Pure transformation —
    /// no I/O, no clones (besides the borrow into `&str`).
    ///
    /// Allocates exactly two `Vec<&str>`s: one for pending, one for
    /// in-progress (which is then split in place via `split_off` —
    /// the leading slice is reused as `in_progress_backed`, no extra
    /// allocation).
    pub fn as_input(&self) -> TodoGateInput<'_> {
        use crate::tools::todo::TodoStatus;
        let mut pending = Vec::new();
        let mut in_progress: Vec<&str> = Vec::new();
        for (_, content, status) in &self.todos {
            match status {
                TodoStatus::Pending => pending.push(content.as_str()),
                TodoStatus::InProgress => in_progress.push(content.as_str()),
                TodoStatus::Completed | TodoStatus::Cancelled => {}
            }
        }
        let backed_count = in_progress.len().min(self.backing_task_count);
        let in_progress_unbacked = in_progress.split_off(backed_count);
        let in_progress_backed = in_progress;
        TodoGateInput {
            pending,
            in_progress_unbacked,
            in_progress_backed,
            backing_task_count: self.backing_task_count,
        }
    }
}
/// Inputs to `evaluate_todo_gate`. All fields are deliberately owned
/// borrows from the gate's call-site so the helper is a pure function.
///
/// The struct itself is `pub` (with `#[doc(hidden)]`) only so the
/// replay-trace integration test in `tests/trace_replay.rs` can name
/// the type as `&TodoGateInput<'_>` when calling `evaluate_todo_gate`.
/// Fields stay crate-private — the test never constructs the struct
/// directly; it obtains an instance via `CollectedTodoGateInput::as_input()`.
#[doc(hidden)]
pub struct TodoGateInput<'a> {
    pub(super) pending: Vec<&'a str>,
    pub(super) in_progress_unbacked: Vec<&'a str>,
    pub(super) in_progress_backed: Vec<&'a str>,
    pub(super) backing_task_count: usize,
}
impl TodoGateReason {
    /// Wire-string form, byte-identical to a `TODO_GATE_*` const in
    /// `crate::session::events`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InFlight => crate::session::events::TODO_GATE_IN_FLIGHT,
        }
    }
}
/// Pure decision function: does the gate fire, and with what reminder?
///
/// The function does NOT consult the cap — the caller folds the cap check
/// in around this function. Keeping cap logic out makes `evaluate_todo_gate`
/// trivially testable.
///
/// Exposed as `pub` solely so the replay-trace integration test in
/// `tests/trace_replay.rs` can call the gate directly. Not part of the
/// public API.
#[doc(hidden)]
pub fn evaluate_todo_gate(input: &TodoGateInput<'_>) -> TodoGateDecision {
    if input.pending.is_empty() && input.in_progress_unbacked.is_empty() {
        return TodoGateDecision::Continue;
    }
    TodoGateDecision::Nudge {
        reminder: build_todo_gate_reminder(&input.pending, &input.in_progress_unbacked),
        reason: TodoGateReason::InFlight,
    }
}
/// Build the in-flight TodoGate reminder text.
///
/// Uses the doubled-`${{{{ tools.by_kind.* }}}}` convention so the
/// caller's `format!` pass leaves a single `${{ tools.by_kind.* }}`
/// for `TemplateRenderer` / `render_prompt` to resolve into the
/// model-facing tool name.
pub(super) fn build_todo_gate_reminder(pending: &[&str], unbacked_in_progress: &[&str]) -> String {
    use std::fmt::Write as _;
    let mut buf =
        String::from("You have outstanding todos but ended your turn without a tool call.\n\n");
    if !unbacked_in_progress.is_empty() {
        buf.push_str("In-progress (no backing background task):\n");
        for c in unbacked_in_progress {
            let _ = writeln!(buf, "- {c}");
        }
        buf.push('\n');
    }
    if !pending.is_empty() {
        buf.push_str("Pending:\n");
        for c in pending {
            let _ = writeln!(buf, "- {c}");
        }
        buf.push('\n');
    }
    let _ = write!(
        buf,
        "Per <task_completion_discipline>, advance the next pending todo \
         with the appropriate tool call NOW. If you have a genuine external \
         blocker (missing credential, denied permission, network unreachable), \
         state it explicitly AND mark the affected todos `cancelled` via \
         ${{{{ tools.by_kind.plan }}}} with a reason in the same turn."
    );
    buf
}
/// Resolve the runtime `ReminderPolicy` from the resolved inputs.
///
/// Precedence: CLI `--todo-gate` > remote `/settings` > built-in default
/// (which is disabled). Extracted from `spawn_session_actor` so the
/// precedence rules are unit-testable. Named `resolve_*` to match the
/// sibling precedence helpers in `crate::util::config`
/// (`resolve_zdr_access_enabled`, `resolve_restore_code`, …).
pub(crate) fn resolve_reminder_policy(
    remote: Option<&crate::util::config::RemoteSettings>,
    todo_gate: bool,
) -> xai_grok_agent::ReminderPolicy {
    let mut policy = xai_grok_agent::ReminderPolicy::default();
    if let Some(remote) = remote {
        if let Some(enabled) = remote.todo_gate_enabled {
            policy.todo_gate.enabled = enabled;
        }
        if let Some(cap) = remote.todo_gate_max_fires_per_prompt {
            policy.todo_gate.max_fires_per_prompt = cap;
        }
    }
    if todo_gate {
        policy.todo_gate.enabled = true;
    }
    policy
}
/// Build the date-rollover reminder when the local calendar
/// date has advanced past the date last surfaced to the model.
///
/// Returns `None` when the date is unchanged (already announced) or has moved
/// backwards (e.g. a manual clock adjustment), so the caller injects nothing
/// in the common case. Pure (no `self`, no clock access) so the rollover
/// boundary logic is unit-testable; see `reminder_policy_tests`.
pub(crate) fn date_rollover_reminder(
    today: chrono::NaiveDate,
    last_announced: chrono::NaiveDate,
) -> Option<String> {
    if today <= last_announced {
        return None;
    }
    Some(format!(
        "The local date has changed since this session started. Today's date is now \
         {today}. The \"Today's date\" value shown in the <user_info> block above was set \
         earlier in the session and is now stale; use {today} as the current date."
    ))
}
/// Body of the one-shot interrupt `<system-reminder>` injected on the next real
/// user turn after a mid-stream abort that left the model with no other signal.
/// Wrapped in grok's `<system-reminder>` shape by [`SessionActor::push_system_reminder`].
/// See [`SessionActor::maybe_inject_interrupt_reminder`].
pub(crate) const INTERRUPT_REMINDER: &str = "[Request interrupted by user]";
/// TodoGate when enabled and the prompt carries `<task_completion_discipline>`
/// (`{DISCIPLINE_BLOCK}`), but NOT while the goal loop is active — the
/// continuation directive drives the loop there (see the body).
pub(super) fn todo_gate_active(
    policy: &xai_grok_agent::system_reminder::ReminderPolicy,
    audience: xai_grok_agent::prompt::context::PromptAudience,
    definition: &AgentDefinition,
    goal_harness_enabled: bool,
    goal_status: Option<crate::session::goal_tracker::GoalStatus>,
) -> bool {
    if !policy.todo_gate.enabled {
        return false;
    }
    if laziness_injection_active(goal_harness_enabled, goal_status) {
        return false;
    }
    definition.carries_task_completion_discipline(audience)
}
impl SessionActor {
    /// Date rollover for long-running sessions. When a session crosses a
    /// local-midnight boundary the `Today's date` value stamped into the cached
    /// `<user_info>` prefix goes stale (the prefix is only re-stamped on
    /// compaction / resume, to preserve the prompt cache). Detect the change
    /// and inject a one-shot `<system-reminder>` announcing the new date.
    ///
    /// Self-dedupes via `last_announced_local_date`, so it fires at most once
    /// per calendar day regardless of how many turns occur. Skipped when the
    /// active template manages this surface elsewhere.
    pub(super) async fn maybe_inject_date_rollover_reminder(&self) {
        let today = chrono::Local::now().date_naive();
        let last = self.last_announced_local_date.get();
        let Some(reminder) = date_rollover_reminder(today, last) else {
            return;
        };
        self.last_announced_local_date.set(today);
        self.push_system_reminder(&reminder);
        tracing::debug!(
            previous = % last, today = % today, "Injected date rollover reminder"
        );
    }
    /// Inject a one-shot `<system-reminder>` telling the model its previous turn
    /// was interrupted mid-stream, when nothing else will (no in-flight tool to
    /// repair into a "cancelled" tool-result, no permission tool-result). The
    /// flag is armed by [`Self::cancel_running_task`] only on the no-active-tool
    /// abort path, and is consumed exactly once (caller gates to real user
    /// prompts). Skipped when the active template manages this surface
    /// elsewhere, matching [`Self::maybe_inject_date_rollover_reminder`].
    pub(super) async fn maybe_inject_interrupt_reminder(&self) {
        if !self.events.take_pending_interrupt_reminder() {
            return;
        }
        self.push_system_reminder(INTERRUPT_REMINDER);
        tracing::debug!("Injected prior-turn interrupt reminder");
    }
    /// Push a `<system-reminder>`-wrapped user message into the conversation.
    pub(super) fn push_system_reminder(&self, content: &str) {
        self.push_system_reminder_with_tag(content, "system-reminder");
    }
    /// The active reminder wrapper tag, backed by the canonical tag constants
    /// in `xai_grok_tools::reminders`.
    pub(super) fn reminder_wrapper_tag(&self) -> &'static str {
        xai_grok_tools::reminders::DEFAULT_REMINDER_TAG
    }
    /// Push a `<{tag}>`-wrapped user message.
    pub(super) fn push_system_reminder_with_tag(&self, content: &str, tag: &str) {
        let message = ConversationItem::system_reminder(format!("<{tag}>\n{content}\n</{tag}>"));
        self.chat_state_handle.push_user_message(message);
    }
    /// Mark completion IDs as reported in the shared
    /// `ReportedTaskCompletions` state so the per-tool-call
    /// `TaskCompletionReminder` won't (re-)surface them. Used both to dedupe
    /// completions the model already saw (notification-drain / auto-wake
    /// prompts) and to drop them during the goal loop (between-turn drain).
    /// No-op on an empty list.
    pub(super) async fn mark_completions_reported(&self, ids: &[&str]) {
        if ids.is_empty() {
            return;
        }
        use xai_grok_tools::reminders::task_completion::ReportedTaskCompletions;
        use xai_grok_tools::types::resources::State;
        let bridge = self.agent.borrow().tool_bridge().clone();
        let resources = bridge.shared_resources().await;
        let mut res = resources.lock().await;
        let reported = res.get_or_default::<State<ReportedTaskCompletions>>();
        for id in ids {
            reported.mark_reported(id);
        }
    }
    /// Drain background completions (bash tasks + subagents) that arrived
    /// while the model was idle and inject them as system-reminders so the
    /// model sees them at turn start.
    ///
    /// While the goal loop is active the completions are DROPPED instead:
    /// the continuation directive is the sole driver, and an async "task /
    /// subagent completed" reminder can derail a weak model (e.g.
    /// relaunching a killed server). They are still drained (consumed) and
    /// marked reported so nothing accumulates to surface on a later turn.
    ///
    /// Surfacing (the `push_system_reminder` calls) is suppressed when the
    /// active template handles this elsewhere, but the goal-loop drain is not:
    /// with the goal loop active, completions are still consumed and marked
    /// reported so nothing surfaces later.
    pub(super) async fn drain_between_turn_completions(&self) {
        let goal_loop_active = self.goal_loop_active();
        let bridge = self.agent.borrow().tool_bridge().clone();
        let bash_completions = bridge.drain_between_turn_bash_completions().await;
        if !bash_completions.is_empty() {
            let ids: Vec<&str> = bash_completions
                .iter()
                .map(|t| t.task_id.as_str())
                .collect();
            if goal_loop_active {
                tracing::info!(
                    count = bash_completions.len(), task_ids = ? ids,
                    "dropping between-turn bash task completions (goal loop active)"
                );
                self.mark_completions_reported(&ids).await;
            } else {
                tracing::info!(
                    count = bash_completions.len(), task_ids = ? ids,
                    "draining between-turn bash task completions"
                );
                let task_output_name =
                    xai_grok_tools::reminders::task_completion::resolve_task_output_tool_name(
                        &bridge,
                    )
                    .await;
                let read_tool_name =
                    xai_grok_tools::reminders::task_completion::resolve_read_tool_name(&bridge)
                        .await;
                let reminder = xai_grok_tools::reminders::task_completion::format_between_turn_bash_completions(
                    &bash_completions,
                    task_output_name.as_deref(),
                    read_tool_name.as_deref(),
                );
                self.push_system_reminder(&reminder);
            }
        }
        let Some(tx) = &self.tool_context.subagent_event_tx else {
            return;
        };
        use xai_grok_tools::implementations::grok_build::task::types::{
            SubagentCompletionsRequest, SubagentEvent,
        };
        let suppress_ids = self
            .tool_context
            .auto_wake_delivered
            .as_ref()
            .map(|a| a.snapshot())
            .unwrap_or_default();
        let (respond_to, rx) = tokio::sync::oneshot::channel();
        if tx
            .send(SubagentEvent::Completions(SubagentCompletionsRequest {
                suppress_ids,
                respond_to,
            }))
            .is_err()
        {
            return;
        }
        let Ok(completions) = rx.await else {
            return;
        };
        if completions.is_empty() {
            return;
        }
        let ids: Vec<&str> = completions.iter().map(|c| c.subagent_id.as_str()).collect();
        if goal_loop_active {
            tracing::info!(
                count = completions.len(), subagent_ids = ? ids,
                "dropping between-turn subagent completions (goal loop active)"
            );
            self.mark_completions_reported(&ids).await;
            return;
        }
        tracing::info!(
            count = completions.len(), subagent_ids = ? ids,
            "draining between-turn subagent completions"
        );
        let reminder =
            xai_grok_tools::reminders::task_completion::format_between_turn_completion_reminder(
                &completions,
                &bridge,
            )
            .await;
        self.push_system_reminder(&reminder);
    }
    /// Persist a manifest of running background tasks to the session directory.
    ///
    /// Called during session shutdown (both explicit and channel-closed paths)
    /// so a resumed session can inform the model about processes that were
    /// still alive when the session ended.
    pub(super) async fn persist_background_task_manifest(&self) {
        let tasks = self
            .agent
            .borrow()
            .tool_bridge()
            .list_background_tasks()
            .await;
        let entries: Vec<crate::terminal::BackgroundTaskManifestEntry> = tasks
            .into_iter()
            .filter(|t| !t.completed)
            .map(|t| crate::terminal::BackgroundTaskManifestEntry {
                task_id: t.task_id,
                command: t.command,
                display_command: t.display_command,
                output_file: t.output_file,
                start_time: t.start_time,
                cwd: t.cwd,
                kind: t.kind,
            })
            .collect();
        if !entries.is_empty() {
            tracing::info!(
                count = entries.len(),
                "persisting background task manifest for session resume"
            );
        }
        let session_dir = crate::session::persistence::session_dir(&self.session_info);
        crate::terminal::persist_manifest(&session_dir, entries);
    }
    /// Load the background task manifest from a prior session and inject a
    /// system-reminder so the model knows about orphaned tasks.
    ///
    /// The manifest file is deleted after loading so it is only shown once.
    pub(super) fn inject_resumed_tasks_reminder(&self) {
        let session_dir = crate::session::persistence::session_dir(&self.session_info);
        let entries = crate::terminal::load_and_clear_manifest(&session_dir);
        if entries.is_empty() {
            return;
        }
        tracing::info!(
            count = entries.len(),
            "injecting resumed background tasks reminder"
        );
        let reminder = crate::terminal::format_resumed_tasks_reminder(&entries);
        self.push_system_reminder(&reminder);
    }
    /// Turn-end TodoGate config, or `None` when [`todo_gate_active`] is false.
    pub(super) fn todo_gate_policy(
        &self,
    ) -> Option<xai_grok_agent::system_reminder::TodoGateConfig> {
        let goal_status = self.goal_tracker.lock().status();
        let agent = self.agent.borrow();
        let policy = agent.reminder_policy();
        let active = todo_gate_active(
            policy,
            agent.prompt_audience(),
            agent.definition(),
            self.goal_harness_enabled(),
            goal_status,
        );
        tracing::debug!(
            enabled = policy.todo_gate.enabled,
            goal_harness_enabled = self.goal_harness_enabled(),
            ?goal_status,
            active,
            "todo_gate_policy"
        );
        if !active {
            return None;
        }
        Some(policy.todo_gate)
    }
    /// Gather the inputs needed by `evaluate_todo_gate` from live session
    /// state.
    ///
    /// Each `.await` is preceded by an owned `Arc<ToolBridge>` clone
    /// from `tool_bridge_handle()` — no `RefCell::Ref<Agent>` guard is
    /// held across a suspension point.
    pub(super) async fn collect_todo_gate_input(&self, prompt_id: &str) -> CollectedTodoGateInput {
        use crate::tools::todo::{TodoState, TodoStatus};
        use xai_grok_tools::types::resources::State;
        let bridge = self.tool_bridge_handle();
        let todos: Vec<(String, String, TodoStatus)> = bridge
            .read_resource::<State<TodoState>>()
            .await
            .map(|state| {
                state
                    .0
                    .todo_items_with_ids()
                    .map(|(id, item)| (id.clone(), item.content.clone(), item.status))
                    .collect()
            })
            .unwrap_or_default();
        let outstanding_live = self
            .outstanding_reply_for_prompt(prompt_id)
            .await
            .map(|r| r.live_ids.len())
            .unwrap_or(0);
        let incomplete_terminal_tasks = bridge
            .list_background_tasks()
            .await
            .into_iter()
            .filter(xai_grok_tools::computer::types::TaskSnapshot::is_outstanding)
            .count();
        let backing_task_count = outstanding_live + incomplete_terminal_tasks;
        CollectedTodoGateInput {
            todos,
            backing_task_count,
        }
    }
}
