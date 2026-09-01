use std::sync::Arc;
use std::time::{Duration, Instant};

use xai_grok_status_line::{StatusLineConfig, StatusLineContext, StatusLineTrigger};

use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::views::status_line::{RowSize, SanitizedText, StatusLineDisplay, StatusSegment};

mod command;
pub(crate) mod metrics;

pub(crate) const EVENT_DEBOUNCE: Duration = Duration::from_millis(300);

pub(crate) const MIN_REFRESH_INTERVAL_MS: Duration = Duration::from_millis(100);

pub(crate) const ABANDON_AFTER: Duration = Duration::from_secs(30);

pub(crate) const REFRESH_FAILURES_TO_PAINT: u32 = 3;

const _: () = assert!(
    ABANDON_AFTER.as_secs() >= command::COMMAND_TIMEOUT.as_secs() * 2,
    "the watchdog must only fire for a task that never answers, never for a slow script"
);

pub(crate) fn draws_a_row(config: &StatusLineConfig) -> bool {
    config.reserves_a_row()
}

fn display_for(text: &str) -> Option<StatusLineDisplay> {
    (!text.is_empty()).then(|| StatusLineDisplay::Text(SanitizedText::new(text)))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClientOwnedFields {
    pub(crate) session_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunId(pub(crate) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    id: RunId,
    started: Instant,
    trigger: StatusLineTrigger,
}

impl Run {
    fn past_deadline(self, now: Instant) -> bool {
        now.duration_since(self.started) >= ABANDON_AFTER
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum RunState {
    #[default]
    Idle,
    Running(Run),
    Superseded(Run),
    Abandoned(Run),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunSlot {
    Free,
    WithinDeadline,
    PastDeadline,
}

impl RunState {
    fn slot(self, now: Instant) -> RunSlot {
        match self {
            RunState::Running(run) | RunState::Superseded(run) if run.past_deadline(now) => {
                RunSlot::PastDeadline
            }
            RunState::Running(_) | RunState::Superseded(_) => RunSlot::WithinDeadline,
            RunState::Idle | RunState::Abandoned(_) => RunSlot::Free,
        }
    }

    fn abandon_if_past_deadline(&mut self, now: Instant) -> Option<StatusLineTrigger> {
        let (next, run, abandoned_trigger) = match *self {
            RunState::Superseded(run) if run.past_deadline(now) => (RunState::Idle, run, None),
            RunState::Running(run) if run.past_deadline(now) => {
                (RunState::Abandoned(run), run, Some(run.trigger))
            }
            RunState::Idle
            | RunState::Running(_)
            | RunState::Superseded(_)
            | RunState::Abandoned(_) => return None,
        };
        tracing::warn!(run_id = run.id.0, "status_line: run abandoned, no result");
        metrics::global().record_abandoned();
        *self = next;
        abandoned_trigger
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForcePolicy {
    Clear,
    Keep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterSupersede {
    Rerun,
    NoRun,
}

#[derive(Debug)]
pub struct StatusLineRun {
    id: RunId,
    command: String,
    ctx: Box<StatusLineContext>,
    term_size: RowSize,
}

#[derive(Debug)]
pub enum RunOutcome {
    Output(String),
    Failed { text: String, error: String },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FinishDisposition {
    Applied,
    RefreshFailureKept { error: String, failures: u32 },
    RefreshFailurePainted { error: String, failures: u32 },
}

#[derive(Default)]
pub(crate) struct StatusLineState {
    content: Option<Arc<StatusLineDisplay>>,
    settled: bool,
    answered: bool,
    last_update: Option<Instant>,
    forced: bool,
    refresh_due: bool,
    refresh_failures: u32,
    run: RunState,
    next_run_id: RunId,
    changed: bool,
    source: Option<AgentId>,
    built_from: ClientOwnedFields,
}

impl StatusLineState {
    pub(crate) fn source(&self) -> Option<AgentId> {
        self.source
    }

    pub(crate) fn client_fields(&self) -> &ClientOwnedFields {
        &self.built_from
    }

    #[must_use = "a change needs the rebuild the caller was going to force"]
    pub(crate) fn set_client_fields(&mut self, current: ClientOwnedFields) -> bool {
        if self.built_from == current {
            return false;
        }
        self.built_from = current;
        true
    }

    pub(crate) fn set_source(&mut self, source: Option<AgentId>) -> bool {
        if self.source == source {
            return false;
        }
        self.source = source;
        self.invalidate();
        true
    }

    pub(crate) fn display(&self) -> Option<Arc<StatusLineDisplay>> {
        self.content.clone()
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.settled
    }

    pub(crate) fn settle_empty(&mut self) {
        self.settled = true;
        self.clear_force();
        self.refresh_due = false;
    }

    #[must_use = "dropping the flag loses the redraw it was asking for"]
    pub(crate) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    pub(crate) fn is_due(&self, now: Instant) -> bool {
        let interval = if self.forced || self.refresh_due {
            MIN_REFRESH_INTERVAL_MS
        } else {
            EVENT_DEBOUNCE
        };
        self.last_update
            .is_none_or(|at| now.duration_since(at) >= interval)
    }

    pub(crate) fn stamp(&mut self, now: Instant, force: ForcePolicy) {
        self.last_update = Some(now);
        match force {
            ForcePolicy::Clear => self.clear_force(),
            ForcePolicy::Keep => {}
        }
    }

    fn clear_force(&mut self) {
        self.forced = false;
    }

    pub(crate) fn force_next_run(&mut self) {
        self.forced = true;
    }

    pub(crate) fn force_pending(&self) -> bool {
        self.forced
    }

    pub(crate) fn request_refresh(&mut self) {
        self.refresh_due = true;
    }

    pub(crate) fn cancel_refresh_request(&mut self) {
        self.refresh_due = false;
    }

    pub(crate) fn refresh_due(&self) -> bool {
        self.refresh_due
    }

    pub(crate) fn abandon_if_past_deadline(&mut self, now: Instant) {
        if self.run.abandon_if_past_deadline(now) == Some(StatusLineTrigger::RefreshInterval) {
            self.refresh_due = true;
        }
    }

    pub(crate) fn run_slot(&self, now: Instant) -> RunSlot {
        self.run.slot(now)
    }

    pub(crate) fn command_in_flight(&self, now: Instant) -> bool {
        self.run_slot(now) != RunSlot::Free
    }

    #[must_use = "the effect must be dispatched, or the row waits forever on a run that never started"]
    pub(crate) fn begin_command_run(
        &mut self,
        now: Instant,
        command: String,
        mut ctx: Box<StatusLineContext>,
        term_size: RowSize,
    ) -> Option<Effect> {
        if self.command_in_flight(now) {
            return None;
        }
        let trigger = if std::mem::take(&mut self.refresh_due) {
            StatusLineTrigger::RefreshInterval
        } else {
            StatusLineTrigger::State
        };
        ctx.trigger = Some(trigger);
        let id = self.next_run_id;
        self.next_run_id.0 += 1;
        self.run = RunState::Running(Run {
            id,
            started: now,
            trigger,
        });
        self.stamp(now, ForcePolicy::Clear);
        Some(Effect::RunStatusLineCommand(StatusLineRun {
            id,
            command,
            ctx,
            term_size,
        }))
    }

    #[must_use = "the disposition carries the refresh failure the caller must log"]
    pub(crate) fn finish_command_run(
        &mut self,
        now: Instant,
        id: RunId,
        outcome: RunOutcome,
    ) -> FinishDisposition {
        let disposition = match self.run {
            RunState::Running(run) if run.id == id => {
                self.run = RunState::Idle;
                self.apply_run_outcome(run.trigger, outcome)
            }
            RunState::Abandoned(run) if run.id == id => {
                self.run = RunState::Idle;
                if run.trigger == StatusLineTrigger::RefreshInterval {
                    self.refresh_due = false;
                }
                self.apply_run_outcome(run.trigger, outcome)
            }
            RunState::Superseded(run) if run.id == id => {
                self.run = RunState::Idle;
                FinishDisposition::Applied
            }
            RunState::Idle
            | RunState::Abandoned(_)
            | RunState::Running(_)
            | RunState::Superseded(_) => return FinishDisposition::Applied,
        };
        self.stamp(now, ForcePolicy::Keep);
        disposition
    }

    fn apply_run_outcome(
        &mut self,
        trigger: StatusLineTrigger,
        outcome: RunOutcome,
    ) -> FinishDisposition {
        match outcome {
            RunOutcome::Output(line) => {
                self.refresh_failures = 0;
                self.answered = true;
                self.settle_with_session_content(display_for(&line));
                FinishDisposition::Applied
            }
            RunOutcome::Failed { text, error } => match trigger {
                StatusLineTrigger::State => {
                    self.settle_with_session_content(display_for(&text));
                    FinishDisposition::Applied
                }
                StatusLineTrigger::RefreshInterval => {
                    self.refresh_failures = self.refresh_failures.saturating_add(1);
                    let failures = self.refresh_failures;
                    if failures >= REFRESH_FAILURES_TO_PAINT || !self.answered {
                        self.settle_with_session_content(display_for(&text));
                        FinishDisposition::RefreshFailurePainted { error, failures }
                    } else {
                        self.settled = true;
                        FinishDisposition::RefreshFailureKept { error, failures }
                    }
                }
            },
        }
    }

    pub(crate) fn supersede_command_run(&mut self, after: AfterSupersede) {
        self.run = match self.run {
            RunState::Running(run) => {
                if run.trigger == StatusLineTrigger::RefreshInterval {
                    self.refresh_due = true;
                }
                RunState::Superseded(run)
            }
            RunState::Abandoned(_) => RunState::Idle,
            state @ (RunState::Superseded(_) | RunState::Idle) => state,
        };
        match after {
            AfterSupersede::Rerun => self.force_next_run(),
            AfterSupersede::NoRun => self.clear_force(),
        }
    }

    pub(crate) fn set_segments(&mut self, segments: Vec<StatusSegment>) {
        self.settle_with_session_content(
            (!segments.is_empty()).then_some(StatusLineDisplay::Segments(segments)),
        );
    }

    pub(crate) fn set_problem(&mut self, text: &str) {
        self.settled = true;
        self.write_content(Some(StatusLineDisplay::Segments(vec![
            StatusSegment::warn(text),
        ])));
    }

    fn settle_with_session_content(&mut self, next: Option<StatusLineDisplay>) {
        if next.is_some() {
            metrics::global().note_content();
        }
        self.settled = true;
        self.write_content(next);
    }

    fn write_content(&mut self, next: Option<StatusLineDisplay>) {
        if self.content.as_deref() != next.as_ref() {
            self.content = next.map(Arc::new);
            self.changed = true;
        }
    }

    pub(crate) fn invalidate(&mut self) {
        self.supersede_command_run(AfterSupersede::NoRun);
        self.write_content(None);
        self.settled = false;
        self.answered = false;
    }
}

#[cfg(test)]
pub(crate) fn test_context(cwd: &str) -> StatusLineContext {
    use xai_grok_status_line::StatusLineWorkspace;

    StatusLineContext {
        cwd: cwd.to_string(),
        workspace: StatusLineWorkspace {
            current_dir: cwd.to_string(),
            repo_root: Some(cwd.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
#[path = "status_line_tests.rs"]
mod tests;
