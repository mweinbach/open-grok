use super::*;
use crate::session::FeedbackDraftUpdateRequest;
use xai_grok_feedback::{
    FeedbackDraft, FeedbackDraftStore, FeedbackFailureMode, FeedbackStoreError,
    FeedbackTaskCategory, FeedbackType, UpdateOutcome,
};
use xai_grok_telemetry::events::FeedbackDraftOpKind;

/// Wire spelling of the `FeedbackDraftUpdateRequest` the pager sends on `drafts/update`.
fn pager_update_body(draft_id: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": "sess-1",
        "draft_id": draft_id,
        "details": "edited body",
        "title": "edited title",
        "area": "editing",
        "type": "bug",
        "task_category": "debug",
        "failure_mode": "hallucinated",
    })
}

/// Wire spelling of the `FeedbackDraftSendRequest` the pager sends on `x.ai/feedback`.
fn pager_send_body() -> serde_json::Value {
    serde_json::json!({
        "session_id": "sess-1",
        "draft_id": "01931111-aaaa-7bbb-8ccc-ddddeeeeffff",
        "request_trace_upload_token": true,
        "edited_body": {
            "title": "edited title",
            "details": "edited body",
            "area": "editing",
            "type": "bug",
            "task_category": "debug",
            "failure_mode": "hallucinated",
            "images": [{ "data": "aGk=", "mimeType": "image/png" }],
            "client_version": "1.2.3",
            "terminal_info": {
                "brand": "Ghostty",
                "multiplexer": "tmux",
                "isSsh": false,
                "isByobu": false,
                "termVar": "xterm-ghostty",
            },
        },
    })
}

fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(
    body: &serde_json::Value,
) -> serde_json::Value {
    serde_json::to_value(serde_json::from_value::<T>(body.clone()).unwrap()).unwrap()
}

#[test]
fn drafts_send_request_round_trips_the_pager_body() {
    let full = pager_send_body();
    assert_eq!(round_trip::<FeedbackDraftSendRequest>(&full), full);

    // Absent optionals must round-trip as absent, not `null`.
    let mut minimal = full;
    let Some(edited_body) = minimal
        .get_mut("edited_body")
        .and_then(|v| v.as_object_mut())
    else {
        panic!("edited_body missing: {minimal:?}");
    };
    for optional in [
        "area",
        "task_category",
        "failure_mode",
        "client_version",
        "terminal_info",
    ] {
        edited_body.remove(optional);
    }
    assert_eq!(round_trip::<FeedbackDraftSendRequest>(&minimal), minimal);
}

#[test]
fn drafts_update_request_round_trips_the_pager_body() {
    let full = pager_update_body("01931111-aaaa-7bbb-8ccc-ddddeeeeffff");
    assert_eq!(round_trip::<FeedbackDraftUpdateRequest>(&full), full);

    let mut minimal = full;
    let body = minimal.as_object_mut().unwrap();
    for optional in ["area", "task_category", "failure_mode"] {
        body.remove(optional);
    }
    assert_eq!(round_trip::<FeedbackDraftUpdateRequest>(&minimal), minimal);
}

#[test]
fn drafts_update_writes_the_full_pager_body() {
    let session = tempfile::tempdir().unwrap();
    let store = FeedbackDraftStore::new(session.path());
    let predraft = store
        .append_predraft("Todo list", "todos are chopped")
        .unwrap();
    let request: FeedbackDraftUpdateRequest =
        serde_json::from_value(pager_update_body(predraft.id.as_str())).unwrap();

    assert_eq!(
        store
            .update_from_input(&request.draft_id, request.input)
            .unwrap(),
        UpdateOutcome::Updated
    );

    let stored = store
        .get(&predraft.id)
        .unwrap()
        .expect("draft still present");
    assert_eq!(
        stored,
        FeedbackDraft {
            title: "edited title".to_owned(),
            details: "edited body".to_owned(),
            area: Some("editing".to_owned()),
            r#type: Some(FeedbackType::Bug),
            task_category: Some(FeedbackTaskCategory::Debug),
            failure_mode: Some(FeedbackFailureMode::Hallucinated),
            revision: predraft.revision + 1,
            ..predraft
        }
    );
}

/// Pins the `feedback_draft_op` builder: the `list` count rides on success and a store failure
/// rides as its variant class only, never its message.
#[test]
fn draft_op_event_places_count_and_error_class() {
    let cases = [
        (
            FeedbackDraftOpKind::List,
            Ok(Some(2)),
            serde_json::json!({
                "session_id": "sess-1", "op": "list", "ok": true, "draft_count": 2
            }),
        ),
        (
            FeedbackDraftOpKind::Recover,
            Err(draft_op_error(&FeedbackStoreError::Busy)),
            serde_json::json!({
                "session_id": "sess-1", "op": "recover", "ok": false, "error": "busy"
            }),
        ),
        (
            FeedbackDraftOpKind::Delete,
            Ok(None),
            serde_json::json!({ "session_id": "sess-1", "op": "delete", "ok": true }),
        ),
    ];
    for (op, outcome, expected) in cases {
        assert_eq!(
            serde_json::to_value(feedback_drafts::draft_op_event("sess-1", op, outcome)).unwrap(),
            expected,
            "{op:?}"
        );
    }
}

#[test]
fn drafts_update_without_type_fails_the_parse_instead_of_half_updating() {
    let mut body = pager_update_body("01931111-aaaa-7bbb-8ccc-ddddeeeeffff");
    body.as_object_mut().unwrap().remove("type");

    let error = serde_json::from_value::<FeedbackDraftUpdateRequest>(body)
        .expect_err("a body without `type` must not parse");
    assert!(
        error.to_string().contains("missing field `type`"),
        "{error}"
    );
}
