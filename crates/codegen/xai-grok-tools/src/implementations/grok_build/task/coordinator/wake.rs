//! Completed-child wake: continue a finished non-workflow identity in place.

use tokio_util::sync::CancellationToken;

use super::super::admission::{AdmissionDecision, AdmissionError};
use super::queue::{QueuedCaller, QueuedSpawn, StartOrigin};
use super::{ChildRunner, SubagentCoordinator, SubagentLimitDecision};
use crate::implementations::grok_build::task::coordinator_state::WakeOrigin;
use crate::implementations::grok_build::task::types::ActiveAgentMessageOutcome;

pub(super) enum WakeAdmit {
    Started { message_id: String },
    Queued { message_id: String },
}

impl<R: ChildRunner> SubagentCoordinator<R> {
    pub(super) fn wake_completed_child(
        &mut self,
        subagent_id: &str,
        parent_session_id: &str,
        prompt: String,
    ) -> Result<WakeAdmit, ActiveAgentMessageOutcome> {
        if !self.runner.supports_wake() {
            return Err(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        }
        let Some(completed) = self.completed.get(subagent_id) else {
            return Err(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
        };
        if completed.request.parent_session_id != parent_session_id {
            return Err(ActiveAgentMessageOutcome::NotFoundOrNotOwned);
        }
        if completed.request.owner.is_workflow() {
            return Err(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        }
        if self
            .spawn_blocked_sessions
            .contains(&completed.request.parent_session_id)
        {
            return Err(ActiveAgentMessageOutcome::NotActiveOrFinalizing);
        }

        let resume_id = if completed.child_session_id.is_empty() {
            completed.request.id.clone()
        } else {
            completed.child_session_id.clone()
        };
        let mut wake_request = completed.request.clone();
        wake_request.prompt = prompt;
        wake_request.resume_from = Some(resume_id);
        wake_request.parent_prompt_id = None;
        wake_request.run_in_background = true;
        wake_request.surface_completion = false;
        wake_request.await_to_completion = false;
        wake_request.cancel_token = CancellationToken::new();
        let wake_origin = WakeOrigin {
            agent_id: subagent_id.to_owned(),
            message_id: uuid::Uuid::now_v7().to_string(),
        };

        let running = self.session_running_count(&wake_request.parent_session_id);
        match self.admission.admit(&wake_request, running) {
            AdmissionDecision::Start => {
                self.displace_completed(subagent_id);
                let message_id = wake_origin.message_id.clone();
                self.start_child(wake_request, None, StartOrigin::Direct, Some(wake_origin));
                Ok(WakeAdmit::Started { message_id })
            }
            AdmissionDecision::Enqueue => {
                self.notify_limit(
                    &wake_request,
                    SubagentLimitDecision::QueuedAtConcurrentLimit {
                        limit: self.admission.max_concurrent(),
                    },
                );
                self.displace_completed(subagent_id);
                let message_id = wake_origin.message_id.clone();
                self.queued.push_back(QueuedSpawn {
                    request: Box::new(wake_request),
                    queued_at: tokio::time::Instant::now(),
                    caller: QueuedCaller::Backgrounded,
                    wake_origin: Some(wake_origin),
                });
                Ok(WakeAdmit::Queued { message_id })
            }
            AdmissionDecision::Reject(error) => {
                self.notify_limit(
                    &wake_request,
                    match error {
                        AdmissionError::ConcurrentLimitReached { limit } => {
                            SubagentLimitDecision::RejectedAtConcurrentLimit { limit }
                        }
                    },
                );
                Err(ActiveAgentMessageOutcome::NotActiveOrFinalizing)
            }
        }
    }

    fn displace_completed(&mut self, subagent_id: &str) {
        self.completed.remove(subagent_id);
        self.completed_order.retain(|id| id != subagent_id);
    }
}
