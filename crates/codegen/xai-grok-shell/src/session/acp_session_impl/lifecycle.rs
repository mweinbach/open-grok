//! Between-turn lifecycle mutation gate.
//!
//! Model/harness switches and committed rewinds both change state that a turn
//! must observe as one coherent timeline. Inputs may continue to queue while a
//! mutation runs, but every scheduler path funnels through
//! `maybe_start_running_task`, which consults this gate before promotion.

use super::*;

/// Panic/abort-safe ownership of a detached maintenance mutation.
///
/// Manual compaction and history repair must stay detached so the session
/// actor can still accept cancellation and queue input. If their task exits
/// unexpectedly, dropping this lease releases the start gate and wakes any
/// queued work instead of stranding the session permanently.
struct LifecycleMutationLease {
    session: std::sync::Weak<SessionActor>,
    kind: LifecycleMutationKind,
    completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    armed: bool,
}

impl LifecycleMutationLease {
    fn new(
        session: &Arc<SessionActor>,
        kind: LifecycleMutationKind,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) -> Self {
        Self {
            session: Arc::downgrade(session),
            kind,
            completion_tx,
            armed: true,
        }
    }

    async fn finish(mut self) {
        self.armed = false;
        if let Some(session) = self.session.upgrade() {
            release_lifecycle_mutation_and_resume(session, self.kind, self.completion_tx.clone())
                .await;
        }
    }
}

impl Drop for LifecycleMutationLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let kind = self.kind;
        let completion_tx = self.completion_tx.clone();
        tokio::task::spawn_local(async move {
            release_lifecycle_mutation_and_resume(session, kind, completion_tx).await;
        });
    }
}

async fn release_lifecycle_mutation_and_resume(
    session: Arc<SessionActor>,
    kind: LifecycleMutationKind,
    completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
) {
    session.end_lifecycle_mutation(kind).await;
    SessionActor::maybe_start_running_task(session.clone(), completion_tx.clone()).await;
    SessionActor::maybe_drain_notifications(session, completion_tx).await;
}

impl SessionActor {
    pub(super) async fn begin_lifecycle_mutation(
        &self,
        kind: LifecycleMutationKind,
    ) -> Result<(), LifecycleMutationBlock> {
        // `maybe_start_running_task` takes this same lock before checking the
        // gate and installing `running_task`, so checking the active slot and
        // claiming the gate are one atomic transition.
        let mut state = self.state.lock().await;
        let turn_future_active = self
            .session_turn_active
            .load(std::sync::atomic::Ordering::Acquire);
        if state.running_task.is_some() || turn_future_active {
            return Err(LifecycleMutationBlock::ActiveTurn);
        }
        if let Some(active) = state.lifecycle_mutation {
            return Err(LifecycleMutationBlock::MutationInProgress(active));
        }
        state.lifecycle_mutation = Some(kind);
        tracing::debug!(
            session_id = %self.session_info.id.0,
            mutation = kind.as_str(),
            "claimed between-turn lifecycle mutation gate"
        );
        Ok(())
    }

    pub(super) async fn end_lifecycle_mutation(&self, kind: LifecycleMutationKind) {
        let mut state = self.state.lock().await;
        match state.lifecycle_mutation {
            Some(active) if active == kind => {
                state.lifecycle_mutation = None;
                tracing::debug!(
                    session_id = %self.session_info.id.0,
                    mutation = kind.as_str(),
                    "released between-turn lifecycle mutation gate"
                );
            }
            Some(active) => {
                tracing::warn!(
                    session_id = %self.session_info.id.0,
                    requested = kind.as_str(),
                    active = active.as_str(),
                    "ignored mismatched lifecycle mutation release"
                );
            }
            None => {
                tracing::debug!(
                    session_id = %self.session_info.id.0,
                    mutation = kind.as_str(),
                    "lifecycle mutation gate was already released"
                );
            }
        }
    }

    pub(super) async fn start_manual_compaction(
        self: &Arc<Self>,
        user_context: Option<String>,
        respond_to: tokio::sync::oneshot::Sender<acp::Result<()>>,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        let kind = LifecycleMutationKind::ManualCompaction;
        if let Err(blocked) = self.begin_lifecycle_mutation(kind).await {
            let _ = respond_to.send(Err(acp::Error::invalid_request()
                .data(format!("Cannot compact while {}.", blocked.message()))));
            return;
        }

        let session = self.clone();
        let lease = LifecycleMutationLease::new(self, kind, completion_tx);
        tokio::task::spawn_local(async move {
            let result = session.run_compact(user_context).await;
            lease.finish().await;
            let _ = respond_to.send(result);
        });
    }

    pub(super) async fn start_history_repair(
        self: &Arc<Self>,
        dry_run: bool,
        respond_to: tokio::sync::oneshot::Sender<
            anyhow::Result<xai_chat_state::compaction_utils::HistoryRepairReport>,
        >,
        completion_tx: mpsc::UnboundedSender<(String, PromptTurnResult)>,
    ) {
        if dry_run {
            let session = self.clone();
            tokio::task::spawn_local(async move {
                let result = session.handle_repair_history(true).await;
                let _ = respond_to.send(result);
            });
            return;
        }

        let kind = LifecycleMutationKind::HistoryRepair;
        if let Err(blocked) = self.begin_lifecycle_mutation(kind).await {
            let _ = respond_to.send(Err(anyhow::anyhow!(
                "Cannot repair history while {}.",
                blocked.message()
            )));
            return;
        }

        let session = self.clone();
        let lease = LifecycleMutationLease::new(self, kind, completion_tx);
        tokio::task::spawn_local(async move {
            let result = session.handle_repair_history(false).await;
            lease.finish().await;
            let _ = respond_to.send(result);
        });
    }
}
