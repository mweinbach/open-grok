//! `x.ai/feedback/drafts/*` extension handlers over the session's `FeedbackDraftStore`.

use agent_client_protocol as acp;
use xai_grok_feedback::{DeleteOutcome, FeedbackStoreError, UpdateOutcome};
use xai_grok_telemetry::events::{FeedbackDraftOp, FeedbackDraftOpError, FeedbackDraftOpKind};
use xai_grok_telemetry::session_ctx::log_event_dual;

use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::FeedbackDraftUpdateRequest;

#[derive(serde::Deserialize)]
struct FeedbackDraftSessionRequest {
    session_id: String,
}

#[derive(serde::Deserialize)]
struct FeedbackDraftRequest {
    session_id: String,
    draft_id: xai_grok_feedback::FeedbackDraftId,
}

pub(super) fn feedback_store(
    agent: &MvpAgent,
    session_id: &str,
) -> Result<xai_grok_feedback::FeedbackDraftStore, acp::Error> {
    let session_id = acp::SessionId::new(session_id.to_owned());
    let handle = agent.resident_handle(&session_id).ok_or_else(|| {
        acp::Error::invalid_params().data(format!("session not found: {session_id}"))
    })?;
    Ok(xai_grok_feedback::FeedbackDraftStore::new(
        crate::session::persistence::session_dir(&handle.info),
    ))
}

/// Variant-only: `Display` embeds the session path. Exhaustive so a new variant is a compile error.
pub fn draft_op_error(error: &FeedbackStoreError) -> FeedbackDraftOpError {
    match error {
        FeedbackStoreError::Busy => FeedbackDraftOpError::Busy,
        FeedbackStoreError::DraftNotFound { .. } => FeedbackDraftOpError::NotFound,
        FeedbackStoreError::Decode { .. }
        | FeedbackStoreError::UnsupportedSchema { .. }
        | FeedbackStoreError::InvalidDocument { .. }
        | FeedbackStoreError::EmptyDraftId
        | FeedbackStoreError::DuplicateDraftId { .. }
        | FeedbackStoreError::InvalidRevision { .. }
        | FeedbackStoreError::TooLarge { .. }
        | FeedbackStoreError::DraftCapacityExceeded { .. } => FeedbackDraftOpError::InvalidDocument,
        FeedbackStoreError::InvalidSessionDirectory { .. }
        | FeedbackStoreError::Inspect { .. }
        | FeedbackStoreError::OpenLock { .. }
        | FeedbackStoreError::Lock(_)
        | FeedbackStoreError::Read { .. }
        | FeedbackStoreError::SymlinkPath { .. }
        | FeedbackStoreError::NonFilePath { .. }
        | FeedbackStoreError::CreateTemp { .. }
        | FeedbackStoreError::Write { .. }
        | FeedbackStoreError::Persist { .. }
        | FeedbackStoreError::SyncDirectory { .. } => FeedbackDraftOpError::Io,
        FeedbackStoreError::Encode { .. }
        | FeedbackStoreError::BlankTitle
        | FeedbackStoreError::BlankDetails
        | FeedbackStoreError::TitleTooLarge { .. }
        | FeedbackStoreError::DetailsTooLarge { .. }
        | FeedbackStoreError::AreaTooLarge { .. }
        | FeedbackStoreError::ClockBeforeUnixEpoch(_)
        | FeedbackStoreError::ClockOutOfRange => FeedbackDraftOpError::Other,
    }
}

/// `Ok` carries the `list` row count; every other op reports `Ok(None)` on success.
type DraftOpOutcome = Result<Option<u32>, FeedbackDraftOpError>;

pub(super) fn draft_op_event(
    session_id: &str,
    op: FeedbackDraftOpKind,
    outcome: DraftOpOutcome,
) -> FeedbackDraftOp {
    FeedbackDraftOp {
        session_id: session_id.to_owned(),
        op,
        ok: outcome.is_ok(),
        error: outcome.err(),
        draft_count: outcome.ok().flatten(),
        skipped: None,
    }
}

/// Runs one store op and builds the response before it emits `feedback_draft_op`, so telemetry
/// cannot alter what the client gets.
async fn draft_op<T: Send + 'static>(
    agent: &MvpAgent,
    session_id: &str,
    op: FeedbackDraftOpKind,
    store_op: impl FnOnce() -> xai_grok_feedback::Result<T> + Send + 'static,
    respond: impl FnOnce(T) -> (ExtResult, DraftOpOutcome),
) -> ExtResult {
    let result = tokio::task::spawn_blocking(store_op)
        .await
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
    let (response, outcome) = match result {
        Ok(value) => respond(value),
        Err(error) => (
            Err(acp::Error::internal_error().data(error.to_string())),
            Err(draft_op_error(&error)),
        ),
    };
    log_event_dual(
        agent.product_analytics_enabled(),
        draft_op_event(session_id, op, outcome),
    );
    response
}

pub(super) async fn list_feedback_drafts(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let request: FeedbackDraftSessionRequest = parse_params(args)?;
    let store = feedback_store(agent, &request.session_id)?;
    draft_op(
        agent,
        &request.session_id,
        FeedbackDraftOpKind::List,
        move || store.list(),
        |drafts| {
            (
                super::to_raw_response(&serde_json::json!({ "drafts": drafts })),
                Ok(Some(drafts.len() as u32)),
            )
        },
    )
    .await
}

pub(super) async fn get_feedback_draft(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let request: FeedbackDraftRequest = parse_params(args)?;
    let store = feedback_store(agent, &request.session_id)?;
    let draft_id = request.draft_id;
    draft_op(
        agent,
        &request.session_id,
        FeedbackDraftOpKind::Load,
        move || store.get(&draft_id),
        |draft| match draft {
            Some(draft) => (
                super::to_raw_response(&serde_json::json!({ "draft": draft })),
                Ok(None),
            ),
            None => (
                Err(acp::Error::invalid_params().data("feedback draft not found")),
                Err(FeedbackDraftOpError::NotFound),
            ),
        },
    )
    .await
}

pub(super) async fn update_feedback_draft(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let request: FeedbackDraftUpdateRequest = parse_params(args)?;
    let store = feedback_store(agent, &request.session_id)?;
    draft_op(
        agent,
        &request.session_id,
        FeedbackDraftOpKind::Recover,
        move || store.update_from_input(&request.draft_id, request.input),
        |updated| {
            (
                super::to_raw_response(&serde_json::json!({
                    "updated": matches!(updated, UpdateOutcome::Updated),
                })),
                match updated {
                    UpdateOutcome::Updated => Ok(None),
                    UpdateOutcome::NotFound => Err(FeedbackDraftOpError::NotFound),
                },
            )
        },
    )
    .await
}

pub(super) async fn delete_feedback_draft(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let request: FeedbackDraftRequest = parse_params(args)?;
    let store = feedback_store(agent, &request.session_id)?;
    let draft_id = request.draft_id;
    draft_op(
        agent,
        &request.session_id,
        FeedbackDraftOpKind::Delete,
        move || store.delete(&draft_id),
        |deleted| {
            (
                super::to_raw_response(&serde_json::json!({
                    "deleted": matches!(deleted, DeleteOutcome::Deleted),
                })),
                match deleted {
                    DeleteOutcome::Deleted => Ok(None),
                    DeleteOutcome::NotFound => Err(FeedbackDraftOpError::NotFound),
                },
            )
        },
    )
    .await
}
