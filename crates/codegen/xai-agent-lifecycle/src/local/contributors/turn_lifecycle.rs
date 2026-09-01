use async_trait::async_trait;

use crate::send::contributors::turn_lifecycle::{
    InputPolicy, TurnAbortInput, TurnDoneInput, TurnErrorInput, TurnLifecycleContributor,
    TurnStartInput,
};

/// `?Send` twin of [`TurnLifecycleContributor`] for single-threaded hosts like grok build's TUI
/// agent, whose session state is `Rc`/`RefCell`-based and can never satisfy the `Send` bounds the
/// send flavor bakes into its boxed hook futures.
#[async_trait(?Send)]
pub trait LocalTurnLifecycleContributor {
    async fn on_turn_start(&self, _input: &TurnStartInput) {}

    async fn on_turn_start_with_policy(&self, input: &TurnStartInput, _policy: InputPolicy) {
        self.on_turn_start(input).await;
    }
    async fn on_turn_done(&self, _input: &TurnDoneInput) {}

    async fn on_turn_abort(&self, _input: &TurnAbortInput) {}

    async fn on_turn_error(&self, _input: &TurnErrorInput<'_>) {}
}

/// Send contributors are usable in single-threaded hosts as-is, so shared logic implements
/// [`TurnLifecycleContributor`] once and both hosts can register it.
#[async_trait(?Send)]
impl<T: TurnLifecycleContributor> LocalTurnLifecycleContributor for T {
    async fn on_turn_start(&self, input: &TurnStartInput) {
        TurnLifecycleContributor::on_turn_start(self, input).await;
    }
    async fn on_turn_start_with_policy(&self, input: &TurnStartInput, policy: InputPolicy) {
        TurnLifecycleContributor::on_turn_start_with_policy(self, input, policy).await;
    }

    async fn on_turn_done(&self, input: &TurnDoneInput) {
        TurnLifecycleContributor::on_turn_done(self, input).await;
    }

    async fn on_turn_abort(&self, input: &TurnAbortInput) {
        TurnLifecycleContributor::on_turn_abort(self, input).await;
    }

    async fn on_turn_error(&self, input: &TurnErrorInput<'_>) {
        TurnLifecycleContributor::on_turn_error(self, input).await;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{
        AnalyticsClass, CompactionClass, InputAuthority, QueuePolicy, ShutdownPolicy, TurnBoundary,
    };

    fn parent_agent_policy() -> InputPolicy {
        InputPolicy {
            authority: InputAuthority::ModelAuthoredUntrusted,
            turn_boundary: TurnBoundary::Conversational,
            analytics: AnalyticsClass::AgentMessage,
            compaction: CompactionClass::ConversationalAgentAnchor,
            queue: QueuePolicy::VisibleProtected,
            shutdown: ShutdownPolicy::Drain,
        }
    }

    struct LegacyLocalCounter(Rc<Cell<usize>>);

    #[async_trait(?Send)]
    impl LocalTurnLifecycleContributor for LegacyLocalCounter {
        async fn on_turn_start(&self, _input: &TurnStartInput) {
            self.0.set(self.0.get() + 1);
        }
    }

    struct TypedLocalCounter(Rc<Cell<Option<InputPolicy>>>);

    #[async_trait(?Send)]
    impl LocalTurnLifecycleContributor for TypedLocalCounter {
        async fn on_turn_start_with_policy(&self, _input: &TurnStartInput, policy: InputPolicy) {
            self.0.set(Some(policy));
        }
    }

    struct SendPolicyCounter {
        legacy_calls: AtomicUsize,
        typed_calls: AtomicUsize,
    }

    #[async_trait]
    impl TurnLifecycleContributor for SendPolicyCounter {
        async fn on_turn_start(&self, _input: &TurnStartInput) {
            self.legacy_calls.fetch_add(1, Ordering::SeqCst);
        }

        async fn on_turn_start_with_policy(&self, _input: &TurnStartInput, policy: InputPolicy) {
            assert_eq!(policy, parent_agent_policy());
            self.typed_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn local_policy_dispatch_preserves_legacy_and_non_send_contributors() {
        let legacy_calls = Rc::new(Cell::new(0));
        let legacy = LegacyLocalCounter(Rc::clone(&legacy_calls));
        let legacy_contributor: &dyn LocalTurnLifecycleContributor = &legacy;
        legacy_contributor
            .on_turn_start_with_policy(&TurnStartInput::new(true), parent_agent_policy())
            .await;
        assert_eq!(legacy_calls.get(), 1);

        let received = Rc::new(Cell::new(None));
        let typed = TypedLocalCounter(Rc::clone(&received));
        let typed_contributor: &dyn LocalTurnLifecycleContributor = &typed;
        typed_contributor
            .on_turn_start_with_policy(&TurnStartInput::new(true), parent_agent_policy())
            .await;
        assert_eq!(received.get(), Some(parent_agent_policy()));
    }

    #[tokio::test]
    async fn send_to_local_adapter_preserves_policy_without_legacy_downgrade() {
        let counter = SendPolicyCounter {
            legacy_calls: AtomicUsize::new(0),
            typed_calls: AtomicUsize::new(0),
        };
        let contributor: &dyn LocalTurnLifecycleContributor = &counter;
        contributor
            .on_turn_start_with_policy(&TurnStartInput::new(true), parent_agent_policy())
            .await;
        assert_eq!(counter.typed_calls.load(Ordering::SeqCst), 1);
        assert_eq!(counter.legacy_calls.load(Ordering::SeqCst), 0);
    }
}
