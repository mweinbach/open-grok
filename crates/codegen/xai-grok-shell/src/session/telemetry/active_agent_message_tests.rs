use super::*;
use std::time::Duration;
use xai_grok_sampling_types::ModelProvider;

fn source(enabled: bool) -> ActiveAgentMessageTelemetrySource {
    ActiveAgentMessageTelemetrySource::new(
        "parent-session".to_owned(),
        Arc::new(AtomicUsize::new(3)),
        ProviderBoundary::default(),
        enabled,
    )
}

#[test]
fn parent_turn_snapshot_does_not_drift_while_message_waits() {
    let source = source(true);
    let captured = source.capture();
    source.prompt_index.store(9, Ordering::Release);
    let admitted_at = Instant::now();
    let admission = captured.admitted(admitted_at, ProviderBoundary::default());
    assert_eq!(admission.parent_prompt_index(), 3);
    let (parent_ctx, event) = project_settlement(
        Some(admission),
        ActiveAgentMessageSettlementStatus::Completed,
        admitted_at + Duration::from_millis(15),
    )
    .expect("allowed settlement");
    assert_eq!(parent_ctx.session_id, "parent-session");
    assert_eq!(*parent_ctx.prompt_index.try_lock().unwrap(), 3);
    assert_eq!(event.duration_ms, 15);
    assert_eq!(
        source
            .capture()
            .admitted(admitted_at, ProviderBoundary::default())
            .parent_prompt_index(),
        9
    );
}

#[test]
fn settlement_rechecks_both_live_export_boundaries() {
    for close_parent in [true, false] {
        let source = source(true);
        let child_boundary = ProviderBoundary::default();
        let admission = source
            .capture()
            .admitted(Instant::now(), child_boundary.clone());
        if close_parent {
            source.provider_boundary.observe(ModelProvider::Codex);
        } else {
            child_boundary.observe(ModelProvider::Kimi);
        }
        assert!(
            project_settlement(
                Some(admission),
                ActiveAgentMessageSettlementStatus::Completed,
                Instant::now()
            )
            .is_none()
        );
    }
    let disabled = source(false)
        .capture()
        .admitted(Instant::now(), ProviderBoundary::default());
    assert!(
        project_settlement(
            Some(disabled),
            ActiveAgentMessageSettlementStatus::Completed,
            Instant::now()
        )
        .is_none()
    );
    assert!(
        project_settlement(
            None,
            ActiveAgentMessageSettlementStatus::Completed,
            Instant::now()
        )
        .is_none()
    );
}

#[test]
fn immediate_events_are_content_free_and_limits_are_not_double_counted() {
    let wrapper = ToolOutput::Dynamic(
        serde_json::json!({
            "outcome": "accepted",
            "message_id": "SECRET-MESSAGE-CONTENT",
        })
        .into(),
    );
    assert!(immediate_events(&wrapper, 12).is_empty());
    let output = ToolOutput::SendSubagentMessage(SendSubagentMessageOutput::Accepted {
        message_id: "SECRET-MESSAGE-CONTENT".to_owned(),
    });
    let events = immediate_events(&output, 12);
    assert_eq!(
        events,
        vec![ActiveAgentMessageEvent::Completed(
            ActiveAgentMessageCompleted {
                outcome: ActiveAgentMessageOutcome::Accepted,
                duration_ms: 12,
            }
        )]
    );
    let ActiveAgentMessageEvent::Completed(event) = &events[0] else {
        panic!("completed event")
    };
    assert_eq!(
        serde_json::to_value(event).unwrap(),
        serde_json::json!({"outcome": "accepted", "duration_ms": 12})
    );
    for (observed_bytes, expected_count, outcome) in [
        (11, 2, ActiveAgentMessageOutcome::Limit),
        (0, 1, ActiveAgentMessageOutcome::Invalid),
    ] {
        let output = ToolOutput::SendSubagentMessage(SendSubagentMessageOutput::Limit {
            max_bytes: 10,
            observed_bytes,
        });
        let events = immediate_events(&output, 5);
        assert_eq!(events.len(), expected_count);
        assert_eq!(
            events[0],
            ActiveAgentMessageEvent::Completed(ActiveAgentMessageCompleted {
                outcome,
                duration_ms: 5
            })
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn settlement_emits_once_and_disabled_emissions_stay_silent() {
    TEST_EVENTS.with_borrow_mut(|events| *events = Some(Vec::new()));
    let admission = source(true)
        .capture()
        .admitted(Instant::now(), ProviderBoundary::default());
    record_settlement(
        Some(admission.clone()),
        ActiveAgentMessageSettlementStatus::Completed,
        true,
    )
    .await;
    record_settlement(
        Some(admission),
        ActiveAgentMessageSettlementStatus::Completed,
        false,
    )
    .await;
    record_settlement(None, ActiveAgentMessageSettlementStatus::Completed, true).await;
    let output = ToolOutput::SendSubagentMessage(SendSubagentMessageOutput::Unsupported);
    record_completed_tool_output(&output, 1, false);
    let events = TEST_EVENTS
        .with_borrow_mut(Option::take)
        .expect("capture installed");
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        ActiveAgentMessageEvent::Settled(ActiveAgentMessageSettled {
            disposition: ActiveAgentMessageSettlementStatus::Completed,
            ..
        })
    ));
}

#[test]
fn immediate_producer_emits_one_outcome_and_one_limit() {
    TEST_EVENTS.with_borrow_mut(|events| *events = Some(Vec::new()));
    let output = ToolOutput::SendSubagentMessage(SendSubagentMessageOutput::Limit {
        max_bytes: 8,
        observed_bytes: 9,
    });
    record_completed_tool_output(&output, 2, true);
    let events = TEST_EVENTS
        .with_borrow_mut(Option::take)
        .expect("capture installed");
    assert_eq!(
        events,
        vec![
            ActiveAgentMessageEvent::Completed(ActiveAgentMessageCompleted {
                outcome: ActiveAgentMessageOutcome::Limit,
                duration_ms: 2,
            }),
            ActiveAgentMessageEvent::LimitHit(ActiveAgentMessageLimitHit {
                max_bytes: 8,
                observed_bytes: 9,
            }),
        ]
    );
}

#[test]
fn settlement_classification_preserves_cancel_and_receipt_failure_precedence() {
    assert_eq!(
        classify_completed_settlement(true, true, true),
        ActiveAgentMessageSettlementStatus::Cancelled
    );
    assert_eq!(
        classify_completed_settlement(true, false, true),
        ActiveAgentMessageSettlementStatus::ReceiptClosed
    );
    assert_eq!(
        classify_completed_settlement(true, false, false),
        ActiveAgentMessageSettlementStatus::Completed
    );
    assert_eq!(
        classify_completed_settlement(false, false, false),
        ActiveAgentMessageSettlementStatus::Failed
    );
}
