use std::time::Instant;

use xai_grok_status_line::{ResolvedStatusLine, StatusLineContext, StatusLineItem};

use super::app_view::{AppView, TickDemand};
use super::status_line::{
    AfterSupersede, ClientOwnedFields, FinishDisposition, ForcePolicy, RunId, RunOutcome, RunSlot,
    draws_a_row,
};
use crate::views::status_line::RowSize;

enum StatusLineWork {
    Clear,
    Problem(String),
    Builtin(Vec<StatusLineItem>),
    Command(String),
}

impl AppView {
    fn draws_a_row(&self) -> bool {
        draws_a_row(&self.current_ui.status_line)
    }

    fn a_subagent_owns_the_frame(&self) -> bool {
        self.active_agent()
            .is_some_and(|agent| agent.active_subagent.is_some())
    }

    pub(crate) fn status_line_tick_demand(&self) -> TickDemand {
        self.status_line_tick_demand_at(Instant::now())
    }

    pub(crate) fn status_line_tick_demand_at(&self, now: Instant) -> TickDemand {
        let source = self.active_view.agent_id();
        status_line_tick_demand(TickInputs {
            row_is_drawn: self.draws_a_row() && !self.a_subagent_owns_the_frame(),
            settled: self.status_line.is_settled(),
            source_changed: self.status_line.source() != source,
            client_fields_changed: *self.status_line.client_fields() != self.client_owned_fields(),
            turn_timer_running: self.local_turn_elapsed().is_some()
                && self.current_ui.status_line.changes_during_a_turn(),
            has_agent: source.is_some(),
            forced: self.status_line.force_pending(),
            refresh_due: self.status_line.refresh_due(),
            run: self.status_line.run_slot(now),
        })
    }

    pub(crate) fn update_status_line(&mut self) {
        self.update_status_line_at(Instant::now());
    }

    pub(crate) fn update_status_line_at(&mut self, now: Instant) {
        let source = self.active_view.agent_id();
        self.status_line.abandon_if_past_deadline(now);

        if self.current_ui.status_line.refresh_interval().is_none() {
            self.status_line.cancel_refresh_request();
        }

        if source.is_none() {
            self.status_line.set_source(source);
            self.status_line.invalidate();
            return;
        }

        if self.a_subagent_owns_the_frame() {
            return;
        }

        if self.status_line.set_source(source) {
            self.status_line.force_next_run();
        }

        if self
            .status_line
            .set_client_fields(self.client_owned_fields())
        {
            self.status_line.force_next_run();
        }

        if let Some(work) = self.status_line_work(now) {
            self.apply_status_line(now, work);
        }
    }

    fn status_line_work(&self, now: Instant) -> Option<StatusLineWork> {
        let config = &self.current_ui.status_line;
        let Some(resolved) = config.resolve() else {
            return Some(match config.problem_to_paint() {
                Some(problem) => StatusLineWork::Problem(problem.to_string()),
                None => StatusLineWork::Clear,
            });
        };

        if !self.status_line.is_due(now) {
            return None;
        }

        Some(match resolved {
            ResolvedStatusLine::Builtin { items } => StatusLineWork::Builtin(items.to_vec()),
            ResolvedStatusLine::Command { command } => StatusLineWork::Command(command.to_string()),
        })
    }

    fn apply_status_line(&mut self, now: Instant, work: StatusLineWork) {
        use crate::views::status_line::compose_builtin;

        match work {
            StatusLineWork::Clear => self.status_line.invalidate(),
            StatusLineWork::Problem(text) => {
                self.status_line.set_problem(&text);
                self.status_line.stamp(now, ForcePolicy::Clear);
            }
            StatusLineWork::Builtin(items) => {
                let Some(ctx) = self.shell_status_context() else {
                    self.status_line.settle_empty();
                    return;
                };
                let segments = compose_builtin(&ctx, self.local_turn_elapsed(), &items);
                self.status_line.set_segments(segments);
                self.status_line.stamp(now, ForcePolicy::Clear);
            }
            StatusLineWork::Command(command) => {
                let Some(ctx) = self.shell_status_context() else {
                    if !self.status_line.command_in_flight(now) {
                        self.status_line.settle_empty();
                    }
                    return;
                };
                let term_size = self.status_line_term_size();
                if let Some(effect) =
                    self.status_line
                        .begin_command_run(now, command, Box::new(ctx), term_size)
                {
                    self.pending_effects.push(effect);
                }
            }
        }
    }

    pub(crate) fn on_status_line_command_finished(&mut self, id: RunId, outcome: RunOutcome) {
        self.on_status_line_command_finished_at(Instant::now(), id, outcome);
    }

    pub(crate) fn on_status_line_command_finished_at(
        &mut self,
        now: Instant,
        id: RunId,
        outcome: RunOutcome,
    ) {
        let disposition = self.status_line.finish_command_run(now, id, outcome);
        let Some(line) = refresh_failure_log(disposition) else {
            return;
        };
        let session_id = self.status_line_session_id();
        let context = Some(serde_json::json!({
            "error": line.error,
            "consecutive_failures": line.failures,
        }));
        match line.level {
            RefreshFailureLogLevel::Warn => {
                crate::unified_log::warn(line.message, session_id.as_deref(), context);
            }
            RefreshFailureLogLevel::Debug => {
                crate::unified_log::debug(line.message, session_id.as_deref(), context);
            }
        }
    }

    fn status_line_session_id(&self) -> Option<String> {
        let source = self.status_line.source()?;
        let agent = self.agents.get(&source)?;
        Some(agent.session.session_id.as_ref()?.0.to_string())
    }

    pub(crate) fn refresh_status_line_for(&mut self, agent_id: crate::app::agent::AgentId) {
        if self.active_view.agent_id() == Some(agent_id) {
            self.refresh_status_line_now();
        }
    }

    pub(crate) fn queue_status_line_resize(&mut self) {
        if !self.draws_a_row() {
            return;
        }
        self.status_line
            .supersede_command_run(AfterSupersede::Rerun);
    }

    pub(crate) fn status_line_refresh_interval(&self) -> Option<std::time::Duration> {
        self.current_ui.status_line.refresh_interval()
    }

    pub(crate) fn note_status_line_refresh_due(&mut self) {
        self.note_status_line_refresh_due_at(Instant::now());
    }

    pub(crate) fn note_status_line_refresh_due_at(&mut self, now: Instant) {
        if self.status_line_refresh_interval().is_none() {
            return;
        }
        self.status_line.request_refresh();
        self.update_status_line_at(now);
    }

    pub(crate) fn refresh_status_line_now(&mut self) {
        self.refresh_status_line_now_at(Instant::now());
    }

    pub(crate) fn refresh_status_line_now_at(&mut self, now: Instant) {
        if !self.draws_a_row() {
            return;
        }
        self.status_line.force_next_run();
        self.update_status_line_at(now);
    }

    fn client_owned_fields(&self) -> ClientOwnedFields {
        let Some(agent) = self.active_agent() else {
            return ClientOwnedFields::default();
        };
        ClientOwnedFields {
            session_name: agent
                .display_name
                .clone()
                .or_else(|| agent.generated_session_title.clone()),
        }
    }

    fn shell_status_context(&self) -> Option<StatusLineContext> {
        let mut ctx = self.active_agent()?.status_context.clone()?;
        let ClientOwnedFields { session_name } = self.client_owned_fields();
        ctx.session_name = session_name;
        Some(ctx)
    }

    fn local_turn_elapsed(&self) -> Option<std::time::Duration> {
        let agent = self.active_agent()?;
        (!agent.renders_parked())
            .then(|| agent.turn_elapsed())
            .flatten()
    }

    pub(crate) fn status_line_frame(&self) -> crate::views::status_line::StatusLineFrame {
        use crate::views::status_line::StatusLineFrame;

        if !self.draws_a_row() {
            return StatusLineFrame::Off;
        }
        let padding = self.current_ui.status_line.padding();
        if self.status_line.source() != self.active_view.agent_id() {
            return StatusLineFrame::Reserved { padding };
        }
        match self.status_line.display() {
            Some(display) => StatusLineFrame::On { display, padding },
            None if self.status_line.is_settled() => StatusLineFrame::Off,
            None => StatusLineFrame::Reserved { padding },
        }
    }

    fn status_line_term_size(&self) -> RowSize {
        self.active_agent()
            .and_then(|agent| agent.last_status_line_size)
            .unwrap_or(RowSize::FALLBACK)
    }
}

struct TickInputs {
    row_is_drawn: bool,
    settled: bool,
    source_changed: bool,
    client_fields_changed: bool,
    turn_timer_running: bool,
    has_agent: bool,
    forced: bool,
    refresh_due: bool,
    run: RunSlot,
}

fn status_line_tick_demand(
    TickInputs {
        row_is_drawn,
        settled,
        source_changed,
        client_fields_changed,
        turn_timer_running,
        has_agent,
        forced,
        refresh_due,
        run,
    }: TickInputs,
) -> TickDemand {
    if !has_agent || !row_is_drawn {
        return TickDemand::None;
    }
    match run {
        RunSlot::PastDeadline => return TickDemand::Slow,
        RunSlot::WithinDeadline => return TickDemand::None,
        RunSlot::Free => {}
    }
    if turn_timer_running || source_changed || client_fields_changed || !settled || forced {
        return TickDemand::Slow;
    }
    if refresh_due {
        return TickDemand::Slow;
    }
    TickDemand::None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshFailureLogLevel {
    Warn,
    Debug,
}

struct RefreshFailureLogLine {
    level: RefreshFailureLogLevel,
    message: &'static str,
    error: String,
    failures: u32,
}

fn refresh_failure_log(disposition: FinishDisposition) -> Option<RefreshFailureLogLine> {
    match disposition {
        FinishDisposition::Applied => None,
        FinishDisposition::RefreshFailureKept { error, failures } => Some(RefreshFailureLogLine {
            level: RefreshFailureLogLevel::Debug,
            message: "status_line: refresh run failed; keeping the last output",
            error,
            failures,
        }),
        FinishDisposition::RefreshFailurePainted { error, failures } => {
            Some(RefreshFailureLogLine {
                level: if failures <= super::status_line::REFRESH_FAILURES_TO_PAINT {
                    RefreshFailureLogLevel::Warn
                } else {
                    RefreshFailureLogLevel::Debug
                },
                message: "status_line: refresh run failed; painting the error",
                error,
                failures,
            })
        }
    }
}

#[cfg(test)]
#[path = "status_line_policy_tests.rs"]
mod tests;
