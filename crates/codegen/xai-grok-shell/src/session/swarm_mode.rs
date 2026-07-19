//! Session-local swarm mode state.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmModeTrigger {
    Manual,
    Task,
    Tool,
}

impl SwarmModeTrigger {
    pub(crate) const fn injects_reminder(self) -> bool {
        matches!(self, Self::Manual | Self::Task)
    }

    pub(crate) const fn survives_turn(self) -> bool {
        matches!(self, Self::Manual)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct SwarmModeTracker {
    #[serde(default)]
    trigger: Option<SwarmModeTrigger>,
    #[serde(default)]
    reminder_pending: bool,
    #[serde(default)]
    exit_reminder_pending: bool,
}

impl SwarmModeTracker {
    pub(crate) fn enter(&mut self, trigger: SwarmModeTrigger) -> SwarmModeTrigger {
        if self.trigger == Some(SwarmModeTrigger::Manual) && trigger != SwarmModeTrigger::Manual {
            return SwarmModeTrigger::Manual;
        }
        self.trigger = Some(trigger);
        self.reminder_pending = trigger.injects_reminder();
        trigger
    }

    pub(crate) fn exit(&mut self) {
        if self
            .trigger
            .is_some_and(|trigger| trigger.injects_reminder())
        {
            self.exit_reminder_pending = true;
        }
        self.trigger = None;
        self.reminder_pending = false;
    }

    pub(crate) fn take_reminder(&mut self) -> bool {
        let pending = self.reminder_pending;
        self.reminder_pending = false;
        pending
    }

    pub(crate) fn take_exit_reminder(&mut self) -> bool {
        let pending = self.exit_reminder_pending;
        self.exit_reminder_pending = false;
        pending
    }

    pub(crate) const fn trigger(&self) -> Option<SwarmModeTrigger> {
        self.trigger
    }

    pub(crate) const fn enabled(&self) -> bool {
        self.trigger.is_some()
    }

    pub(crate) fn exit_if_trigger(&mut self, trigger: SwarmModeTrigger) -> bool {
        if self.trigger != Some(trigger) {
            return false;
        }
        self.exit();
        true
    }

    /// Clears one-shot task/tool state at the turn boundary and returns whether it changed.
    pub(crate) fn auto_exit_turn(&mut self) -> bool {
        match self.trigger {
            Some(trigger) if !trigger.survives_turn() => {
                self.exit();
                true
            }
            _ => false,
        }
    }

    pub(crate) fn restored_manual_only(&mut self) {
        if self.trigger != Some(SwarmModeTrigger::Manual) {
            self.exit();
        }
    }
}

pub(crate) const SWARM_MODE_REMINDER: &str = concat!(
    "Swarm mode is active. Do a small amount of exploratory work first, then decide whether ",
    "the request actually benefits from parallel subagents. If no swarm is warranted, say so ",
    "and wait for the user instead of forcing one. Once you have enough context and a swarm is ",
    "warranted, do not do the main work yourself: make one exclusive agent_swarm call with a ",
    "prompt_template containing literal {{item}} and an items array. Partition work into ",
    "distinct, independent scopes with no duplicate or conflicting ownership; read-only ",
    "exploration may overlap slightly. Unless the user limits scope, decompose as finely as ",
    "useful up to 128 members, combining only genuinely inseparable work. Use ordinary task ",
    "calls instead for a few heterogeneous tasks. Do not mix agent_swarm with any other tool ",
    "call in the same batch. Keep the subagent tree flat: do not instruct swarm members to ",
    "launch additional task or agent_swarm calls; each member should return a complete handoff."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_persists_while_one_shot_modes_auto_exit() {
        let mut tracker = SwarmModeTracker::default();
        tracker.enter(SwarmModeTrigger::Manual);
        assert!(!tracker.auto_exit_turn());
        assert!(tracker.enabled());

        assert_eq!(
            tracker.enter(SwarmModeTrigger::Task),
            SwarmModeTrigger::Manual
        );
        assert!(!tracker.auto_exit_turn());
        assert_eq!(tracker.trigger(), Some(SwarmModeTrigger::Manual));
        assert!(!tracker.exit_if_trigger(SwarmModeTrigger::Task));

        tracker.exit();
        tracker.enter(SwarmModeTrigger::Task);
        assert!(tracker.exit_if_trigger(SwarmModeTrigger::Task));
        assert!(!tracker.enabled());
        tracker.enter(SwarmModeTrigger::Tool);
        assert!(tracker.auto_exit_turn());
        assert!(!tracker.enabled());
    }

    #[test]
    fn restore_keeps_only_manual() {
        let mut tracker = SwarmModeTracker::default();
        tracker.enter(SwarmModeTrigger::Task);
        tracker.restored_manual_only();
        assert!(!tracker.enabled());
        tracker.enter(SwarmModeTrigger::Manual);
        tracker.restored_manual_only();
        assert_eq!(tracker.trigger(), Some(SwarmModeTrigger::Manual));
    }

    #[test]
    fn reminder_requires_exploration_exclusive_call_and_flat_tree() {
        assert!(SWARM_MODE_REMINDER.contains("exploratory work first"));
        assert!(SWARM_MODE_REMINDER.contains("exclusive agent_swarm call"));
        assert!(SWARM_MODE_REMINDER.contains("up to 128 members"));
        assert!(SWARM_MODE_REMINDER.contains("Keep the subagent tree flat"));
        assert!(SWARM_MODE_REMINDER.contains("complete handoff"));
    }
}
