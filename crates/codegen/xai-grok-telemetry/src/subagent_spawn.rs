#![deny(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentSpawnPhase {
    QueueWait,
    SpawnPrepare,
    SessionBootstrap,
    AgentBuild,
    ToolSetup,
    ReadyToFirstTurn,
}

pub fn phase_region(phase: SubagentSpawnPhase) -> crate::region::Region {
    crate::region::Region::from_span(phase_span(phase))
}

pub fn phase_region_under(
    phase: SubagentSpawnPhase,
    parent: &tracing::Span,
) -> crate::region::Region {
    crate::region::Region::from_span(phase_span_under(phase, parent))
}

crate::startup::span_table!(fn phase_span, fn phase_span_under(SubagentSpawnPhase) {
    QueueWait => "subagent_spawn.queue_wait",
    SpawnPrepare => "subagent_spawn.spawn_prepare",
    SessionBootstrap => "subagent_spawn.session_bootstrap",
    AgentBuild => "subagent_spawn.agent_build",
    ToolSetup => "subagent_spawn.tool_setup",
    ReadyToFirstTurn => "subagent_spawn.ready_to_first_turn",
});

pub struct SpawnPhaseContext {
    pub timer: SharedSubagentSpawnTimer,
    pub parent: tracing::Span,
}

#[derive(Debug, Default)]
pub struct SubagentSpawnTimer {
    phases: Mutex<Vec<(SubagentSpawnPhase, u64)>>,
}

pub type SharedSubagentSpawnTimer = Arc<SubagentSpawnTimer>;

impl SubagentSpawnTimer {
    pub fn new_shared() -> SharedSubagentSpawnTimer {
        Arc::new(Self::default())
    }

    pub fn record(&self, phase: SubagentSpawnPhase, elapsed: Duration) {
        let ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        let mut phases = self
            .phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = phases.iter_mut().find(|(path, _)| *path == phase) {
            slot.1 = ms;
        } else {
            phases.push((phase, ms));
        }
    }

    #[cfg(test)]
    fn ms(&self, phase: SubagentSpawnPhase) -> Option<u64> {
        self.phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(path, _)| *path == phase)
            .map(|(_, ms)| *ms)
    }

    pub fn write_event_phases(&self, event: &mut crate::events::SubagentCompleted) {
        for (phase, ms) in self
            .phases
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            *phase_event_slot(event, *phase) = Some(*ms);
        }
    }
}

fn phase_event_slot(
    event: &mut crate::events::SubagentCompleted,
    phase: SubagentSpawnPhase,
) -> &mut Option<u64> {
    match phase {
        SubagentSpawnPhase::QueueWait => &mut event.queue_wait_ms,
        SubagentSpawnPhase::SpawnPrepare => &mut event.spawn_prepare_ms,
        SubagentSpawnPhase::SessionBootstrap => &mut event.session_bootstrap_ms,
        SubagentSpawnPhase::AgentBuild => &mut event.agent_build_ms,
        SubagentSpawnPhase::ToolSetup => &mut event.tool_setup_ms,
        SubagentSpawnPhase::ReadyToFirstTurn => &mut event.ready_to_first_turn_ms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_last_write_wins_and_absent_reads_none() {
        let timer = SubagentSpawnTimer::default();
        assert_eq!(timer.ms(SubagentSpawnPhase::SpawnPrepare), None);
        timer.record(SubagentSpawnPhase::SpawnPrepare, Duration::from_millis(5));
        timer.record(SubagentSpawnPhase::SpawnPrepare, Duration::from_millis(9));
        assert_eq!(timer.ms(SubagentSpawnPhase::SpawnPrepare), Some(9));
        assert_eq!(timer.ms(SubagentSpawnPhase::AgentBuild), None);
    }
}
