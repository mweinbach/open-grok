use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use xai_grok_telemetry::TelemetryCtx;
use xai_grok_telemetry::events::{
    ActiveAgentMessageCompleted, ActiveAgentMessageLimitHit, ActiveAgentMessageOutcome,
    ActiveAgentMessageSettled, ActiveAgentMessageSettlementDisposition,
};
use xai_grok_tools::implementations::grok_build::send_subagent_message::SendSubagentMessageOutput;
use xai_grok_tools::types::output::ToolOutput;

use crate::session::persistence::ProviderBoundary;

#[derive(Clone)]
pub(crate) struct ActiveAgentMessageTelemetrySource {
    parent_session_id: String,
    prompt_index: Arc<AtomicUsize>,
    provider_boundary: ProviderBoundary,
    enabled: bool,
}

impl ActiveAgentMessageTelemetrySource {
    pub(crate) fn new(
        parent_session_id: String,
        prompt_index: Arc<AtomicUsize>,
        provider_boundary: ProviderBoundary,
        enabled: bool,
    ) -> Self {
        Self {
            parent_session_id,
            prompt_index,
            provider_boundary,
            enabled,
        }
    }

    pub(crate) fn capture(&self) -> ActiveAgentMessageParentTelemetry {
        ActiveAgentMessageParentTelemetry {
            parent_ctx: TelemetryCtx::new(
                self.parent_session_id.clone(),
                Arc::new(tokio::sync::Mutex::new(
                    self.prompt_index.load(Ordering::Acquire),
                )),
            ),
            provider_boundary: self.provider_boundary.clone(),
            enabled: self.enabled,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActiveAgentMessageParentTelemetry {
    parent_ctx: TelemetryCtx,
    provider_boundary: ProviderBoundary,
    enabled: bool,
}

#[derive(Clone)]
pub(crate) struct ActiveAgentMessageAdmissionTelemetry {
    admitted_at: Instant,
    parent: ActiveAgentMessageParentTelemetry,
    child_boundary: ProviderBoundary,
}

#[cfg(test)]
impl ActiveAgentMessageAdmissionTelemetry {
    pub(crate) fn parent_prompt_index(&self) -> usize {
        *self
            .parent
            .parent_ctx
            .prompt_index
            .try_lock()
            .expect("immutable telemetry snapshot")
    }
}

impl ActiveAgentMessageParentTelemetry {
    pub(crate) fn admitted(
        self,
        admitted_at: Instant,
        child_boundary: ProviderBoundary,
    ) -> ActiveAgentMessageAdmissionTelemetry {
        ActiveAgentMessageAdmissionTelemetry {
            admitted_at,
            parent: self,
            child_boundary,
        }
    }
}

pub(crate) type ActiveAgentMessageSettlementStatus = ActiveAgentMessageSettlementDisposition;

pub(crate) fn classify_completed_settlement(
    is_result_success: bool,
    is_result_cancelled: bool,
    is_final_receipt_closed: bool,
) -> ActiveAgentMessageSettlementStatus {
    if is_result_cancelled {
        ActiveAgentMessageSettlementStatus::Cancelled
    } else if is_final_receipt_closed {
        ActiveAgentMessageSettlementStatus::ReceiptClosed
    } else if is_result_success {
        ActiveAgentMessageSettlementStatus::Completed
    } else {
        ActiveAgentMessageSettlementStatus::Failed
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ActiveAgentMessageEvent {
    Completed(ActiveAgentMessageCompleted),
    LimitHit(ActiveAgentMessageLimitHit),
    Settled(ActiveAgentMessageSettled),
}

fn emit(event: ActiveAgentMessageEvent) {
    #[cfg(test)]
    let Some(event) = TEST_EVENTS.with_borrow_mut(|captured| {
        if let Some(captured) = captured {
            captured.push(event);
            None
        } else {
            Some(event)
        }
    }) else {
        return;
    };
    match event {
        ActiveAgentMessageEvent::Completed(event) => xai_grok_telemetry::log_event(event),
        ActiveAgentMessageEvent::LimitHit(event) => xai_grok_telemetry::log_event(event),
        ActiveAgentMessageEvent::Settled(event) => xai_grok_telemetry::log_event(event),
    }
}

fn immediate_events(output: &ToolOutput, duration_ms: u64) -> Vec<ActiveAgentMessageEvent> {
    let ToolOutput::SendSubagentMessage(output) = output else {
        return Vec::new();
    };
    let (outcome, limit) = match output {
        SendSubagentMessageOutput::Accepted { .. } => (ActiveAgentMessageOutcome::Accepted, None),
        SendSubagentMessageOutput::NotFoundOrNotOwned => {
            (ActiveAgentMessageOutcome::NotFoundOrNotOwned, None)
        }
        SendSubagentMessageOutput::NotActiveOrFinalizing => {
            (ActiveAgentMessageOutcome::NotActiveOrFinalizing, None)
        }
        SendSubagentMessageOutput::Saturated { .. } => (ActiveAgentMessageOutcome::Saturated, None),
        SendSubagentMessageOutput::AdmissionUncertain => {
            (ActiveAgentMessageOutcome::AdmissionUncertain, None)
        }
        SendSubagentMessageOutput::NotAcceptedBeforeDeadline => {
            (ActiveAgentMessageOutcome::NotAcceptedBeforeDeadline, None)
        }
        SendSubagentMessageOutput::Unsupported => (ActiveAgentMessageOutcome::Unsupported, None),
        SendSubagentMessageOutput::Limit {
            max_bytes,
            observed_bytes,
        } if observed_bytes > max_bytes => (
            ActiveAgentMessageOutcome::Limit,
            Some(ActiveAgentMessageLimitHit {
                max_bytes: u64::try_from(*max_bytes).unwrap_or(u64::MAX),
                observed_bytes: u64::try_from(*observed_bytes).unwrap_or(u64::MAX),
            }),
        ),
        SendSubagentMessageOutput::Limit { .. } => (ActiveAgentMessageOutcome::Invalid, None),
        SendSubagentMessageOutput::ChannelClosed => {
            (ActiveAgentMessageOutcome::ChannelClosed, None)
        }
        _ => (ActiveAgentMessageOutcome::AdmissionUncertain, None),
    };
    let mut events = vec![ActiveAgentMessageEvent::Completed(
        ActiveAgentMessageCompleted {
            outcome,
            duration_ms,
        },
    )];
    if let Some(limit) = limit {
        events.push(ActiveAgentMessageEvent::LimitHit(limit));
    }
    events
}

pub(crate) fn record_completed_tool_output(output: &ToolOutput, duration_ms: u64, enabled: bool) {
    if enabled {
        for event in immediate_events(output, duration_ms) {
            emit(event);
        }
    }
}

fn project_settlement(
    admission: Option<ActiveAgentMessageAdmissionTelemetry>,
    disposition: ActiveAgentMessageSettlementStatus,
    settled_at: Instant,
) -> Option<(TelemetryCtx, ActiveAgentMessageSettled)> {
    let admission = admission?;
    if !admission.parent.enabled
        || !admission.parent.provider_boundary.allows_xai_export()
        || !admission.child_boundary.allows_xai_export()
    {
        return None;
    }
    Some((
        admission.parent.parent_ctx,
        ActiveAgentMessageSettled {
            disposition,
            duration_ms: u64::try_from(
                settled_at
                    .saturating_duration_since(admission.admitted_at)
                    .as_millis(),
            )
            .unwrap_or(u64::MAX),
        },
    ))
}

pub(crate) async fn record_settlement(
    admission: Option<ActiveAgentMessageAdmissionTelemetry>,
    status: ActiveAgentMessageSettlementStatus,
    enabled: bool,
) {
    if !enabled {
        return;
    }
    let Some((parent_ctx, event)) = project_settlement(admission, status, Instant::now()) else {
        return;
    };
    xai_grok_telemetry::with_session_ctx(parent_ctx, async {
        emit(ActiveAgentMessageEvent::Settled(event));
    })
    .await;
}

#[cfg(test)]
thread_local! {
    static TEST_EVENTS: std::cell::RefCell<Option<Vec<ActiveAgentMessageEvent>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
#[path = "active_agent_message_tests.rs"]
mod tests;
