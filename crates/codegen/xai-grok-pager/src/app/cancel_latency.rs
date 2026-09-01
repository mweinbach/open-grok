use std::time::Instant;

use xai_grok_telemetry::events::CancellationScope;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CancelOrigin {
    UserGesture,
    #[allow(dead_code)]
    Programmatic,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TurnEnd {
    Completed,
    Aborted,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct CancelLatency {
    pub(crate) requested_at: Instant,
    pub(crate) scope: CancellationScope,
}

impl CancelLatency {
    pub(crate) fn new(requested_at: Instant, scope: CancellationScope) -> Self {
        Self {
            requested_at,
            scope,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::agent_view::test_fixtures::make_agent;

    #[test]
    fn repeated_user_cancel_measures_first_gesture_only_once() {
        let mut agent = make_agent();
        agent.cancel_and_arm(CancellationScope::Turn, CancelOrigin::UserGesture);
        let requested_at = agent.cancel_latency.unwrap().requested_at;
        agent.cancel_and_arm(CancellationScope::Turn, CancelOrigin::UserGesture);
        assert_eq!(agent.cancel_latency.unwrap().requested_at, requested_at);
        let settled_at = requested_at + std::time::Duration::from_millis(42);
        let event = agent.settle_cancel(TurnEnd::Completed, settled_at).unwrap();
        assert_eq!(event.latency_ms, 42);
        assert!(matches!(event.scope, CancellationScope::Turn));
        assert!(
            agent
                .settle_cancel(TurnEnd::Completed, settled_at)
                .is_none()
        );
    }

    #[test]
    fn aborted_turn_discards_cancel_anchor() {
        let mut agent = make_agent();
        agent.cancel_and_arm(CancellationScope::Turn, CancelOrigin::UserGesture);
        assert!(
            agent
                .settle_cancel(TurnEnd::Aborted, Instant::now())
                .is_none()
        );
        assert!(agent.cancel_latency.is_none());
    }

    #[test]
    fn programmatic_and_read_only_child_cancels_do_not_arm_latency() {
        let mut agent = make_agent();
        agent.cancel_and_arm(CancellationScope::Turn, CancelOrigin::Programmatic);
        assert!(agent.cancel_latency.is_none());
        agent.mark_as_subagent_view();
        agent.cancel_and_arm(CancellationScope::Turn, CancelOrigin::UserGesture);
        assert!(agent.cancel_latency.is_none());
    }
}
