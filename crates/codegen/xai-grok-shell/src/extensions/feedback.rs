//! `x.ai/feedback`, `x.ai/feedback/dismiss`, `x.ai/btw`, and `x.ai/review/*`
//! extension handlers.
//!
//! - `feedback`/`feedback/dismiss`: persist user ratings/text locally and
//!   forward to cli-chat-proxy.
//! - `btw`: dispatch a side question to the active session via
//!   `SessionCommand::SideQuestion` and return the answer.
//! - `review/comment` and `review/comment/delete`: record inline code review
//!   events to cloud storage.

use std::sync::Arc;

use agent_client_protocol as acp;
use tokio::sync::oneshot;

use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::persistence::{LocalFeedbackEntry, UserFeedbackEntry};
use crate::session::{
    ClientFeedbackInput, CommentDeleteRequest, CommentDeleteResponse, CommentRequest,
    CommentResponse, FeedbackRequestDismiss, FeedbackResponse, SessionCommand, SideQuestionError,
};
use crate::upload::gcs::WithAuth as _;
use xai_file_utils::gcs::upload_bytes;
use xai_grok_telemetry::id::agent_id;

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/btw" => {
            tracing::info!("handling /btw side question");
            handle_btw(agent, args).await
        }
        "x.ai/feedback" | "x.ai/feedback/dismiss" => {
            tracing::info!("handling user feedback");
            handle_feedback(agent, args).await
        }
        m if m.starts_with("x.ai/review") => {
            tracing::info!("handling review comment");
            handle_review(agent, args).await
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

/// Handle `x.ai/btw` -- a side question that doesn't interrupt the current turn.
async fn handle_btw(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct BtwRequest {
        session_id: String,
        question: String,
    }

    let req: BtwRequest = parse_params(args)?;
    let sid: acp::SessionId = req.session_id.clone().into();
    let session_handle = agent.resident_handle(&sid);
    let Some(session) = session_handle else {
        return Err(
            acp::Error::invalid_params().data(format!("session not found: {}", req.session_id))
        );
    };
    let (tx, rx) = oneshot::channel();
    let _ = session.cmd_tx.send(SessionCommand::SideQuestion {
        question: req.question,
        respond_to: tx,
    });
    let result = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session failed to respond"))?;
    match result {
        Ok(answer) => super::to_ext_response(Ok(serde_json::json!({
            "answer": answer,
        }))),
        // Model errors take the canonical mapping: overload gets its short
        // display copy there, rate limits keep the typed code + upgrade
        // copy, auth failures surface as auth_required.
        Err(SideQuestionError::Sampling(e)) => {
            Err(crate::sampling::error::map_sampling_err_to_acp(e))
        }
        // Non-model failures are already readable sentences. Set `message`
        // and leave `data` unset — `Display` appends JSON-encoded `data`,
        // and `internal_error().data(e)` rendered as `Internal error: "…"`,
        // which made capacity failures look like client bugs in the TUI.
        Err(e) => Err(acp::Error::new(
            acp::ErrorCode::InternalError.into(),
            e.to_string(),
        )),
    }
}

fn parse_feedback_input(params: &str) -> Result<ClientFeedbackInput, acp::Error> {
    let max_request_bytes =
        crate::session::feedback::MAX_FEEDBACK_IMAGE_TOTAL_BYTES.div_ceil(3) * 4 + 64 * 1024;
    if params.len() > max_request_bytes {
        return Err(acp::Error::invalid_params()
            .data("Feedback request exceeds the attachment size budget"));
    }
    match serde_json::from_str::<ClientFeedbackInput>(params) {
        Ok(input) => Ok(input),
        Err(_) => {
            let raw_input: serde_json::Value = serde_json::from_str(params)
                .map_err(|_| acp::Error::invalid_params().data("Invalid feedback request"))?;
            if raw_input.get("images").is_some() {
                return Err(acp::Error::invalid_params().data("Invalid feedback image request"));
            }
            let simple: crate::session::FeedbackRequest = serde_json::from_value(raw_input)
                .map_err(|_| acp::Error::invalid_params().data("Invalid feedback request"))?;
            Ok(ClientFeedbackInput {
                session_id: simple.session_id,
                client_type: prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui,
                rating_type: None,
                rating_value: None,
                feedback_text: Some(simple.feedback_text),
                images: vec![],
                feedback_categories: vec![],
                context_type: None,
                turn_number: None,
                request_id: None,
                client_version: None,
                metadata: None,
                terminal_info: None,
            })
        }
    }
}

#[cfg(test)]
mod feedback_input_tests {
    use super::*;

    #[test]
    fn legacy_text_feedback_remains_compatible() {
        let input =
            parse_feedback_input(r#"{"session_id":"session","feedback_text":"feedback"}"#).unwrap();
        assert!(input.images.is_empty());
        assert_eq!(input.feedback_text.as_deref(), Some("feedback"));
        assert_eq!(
            input.client_type,
            prod_mc_cli_chat_proxy_types::feedback_types::ClientType::Tui
        );
    }

    #[test]
    fn malformed_images_cannot_fall_back_to_legacy_text() {
        assert!(parse_feedback_input(r#"{"session_id":"session","client_type":"tui","feedback_text":"feedback","images":"bad"}"#).is_err());
        assert!(
            parse_feedback_input(
                r#"{"session_id":"session","feedback_text":"feedback","images":[]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn oversized_feedback_is_rejected_before_json_parsing() {
        let oversized = " ".repeat(
            crate::session::feedback::MAX_FEEDBACK_IMAGE_TOTAL_BYTES.div_ceil(3) * 4
                + 64 * 1024
                + 1,
        );
        let error = parse_feedback_input(&oversized).unwrap_err();
        assert!(error.to_string().contains("attachment size budget"));
    }
}

async fn handle_feedback(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    if !agent.cfg.borrow().is_feedback_enabled() {
        return Err(acp::Error::internal_error().data(
            "Feedback is disabled. To enable, set GROK_FEEDBACK_ENABLED=true or \
             [features] feedback = true in config.toml.",
        ));
    }

    match args.method.as_ref() {
        "x.ai/feedback" => {
            let feedback_input = parse_feedback_input(args.params.get())?;

            let session_id = acp::SessionId::new(feedback_input.session_id.clone());
            let session_handle = agent.resident_handle(&session_id);

            if !feedback_input.images.is_empty() {
                if !session_handle
                    .as_ref()
                    .is_some_and(|handle| handle.feedback_manager.allows_xai_export())
                {
                    return Err(acp::Error::invalid_params().data(
                        "Feedback image attachments are unavailable for this provider or session",
                    ));
                }
                crate::session::feedback::validate_feedback_image_payloads(&feedback_input.images)
                    .map_err(|error| acp::Error::invalid_params().data(error.to_string()))?;
            }

            let (model_id, model_metadata) = if let Some(ref session) = session_handle {
                let (tx1, rx1) = tokio::sync::oneshot::channel();
                let _ = session
                    .cmd_tx
                    .send(SessionCommand::GetCurrentModel { responds_to: tx1 });
                let model_id = rx1.await.ok();

                let model_metadata = session.get_model_metadata().await;

                (model_id, model_metadata)
            } else {
                let sampling_config = agent.sampling_config.borrow().clone();
                (Some(sampling_config.model.clone()), Default::default())
            };

            let turn_number = feedback_input.turn_number.or_else(|| {
                agent
                    .session_turn_number(&session_id)
                    .map(|t| t.saturating_sub(1) as i64)
            });

            let mut submission = feedback_input.to_submission(
                model_id.clone(),
                model_metadata.resolved_model_id,
                model_metadata.model_fingerprint,
                turn_number,
            );
            let turn_number = submission.turn_number;

            // Enrich with session context for Slack notifications (best-effort).
            if let Some(ref session_handle) = session_handle {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = session_handle
                    .cmd_tx
                    .send(SessionCommand::GetFeedbackContext {
                        turn_number,
                        responds_to: tx,
                    });
                if let Ok(ctx) = rx.await {
                    submission.tool_outcomes = ctx.tool_outcomes;
                    submission.session_cwd = Some(ctx.session_cwd);
                    submission.compaction_count = Some(ctx.compaction_count);
                    submission.context_window_usage = Some(ctx.context_window_usage);
                    submission.context_tokens_used = Some(ctx.context_tokens_used);
                    submission.context_window_tokens = Some(ctx.context_window_tokens);
                }
            }

            // Track rating in session signals
            if let (Some(session_handle), Some(rating_value)) =
                (&session_handle, feedback_input.rating_value)
            {
                use prod_mc_cli_chat_proxy_types::feedback_types::RatingType;
                let (is_positive, is_negative) = match feedback_input.rating_type {
                    // Thumbs: -1 = down, 0 = neutral, 1 = up
                    Some(RatingType::Thumbs) | None => (rating_value > 0, rating_value < 0),
                    // Stars (1-5): >= 4 positive, <= 2 negative, 3 neutral
                    Some(RatingType::Stars) => (rating_value >= 4, rating_value <= 2),
                    // NPS (0-10): 9-10 promoter, 0-6 detractor, 7-8 passive
                    Some(RatingType::Nps) => (rating_value >= 9, rating_value <= 6),
                };
                if is_positive {
                    session_handle.signals_handle.record_positive_rating();
                } else if is_negative {
                    session_handle.signals_handle.record_negative_rating();
                }
            }

            // Log feedback type for debugging
            if feedback_input.is_solicited() {
                tracing::info!(
                    session_id = %feedback_input.session_id,
                    request_id = ?feedback_input.request_id(),
                    turn_number = ?turn_number,
                    "Solicited feedback received (response to feedback request)"
                );
            } else {
                tracing::info!(
                    session_id = %feedback_input.session_id,
                    turn_number = ?turn_number,
                    "Spontaneous user feedback received"
                );
            }

            // Point to the per-turn unified log already uploaded by
            // complete_prompt_trace. Only set the URL when trace uploads
            // are active — otherwise the cloud storage object won't exist.
            let allows_xai_export = session_handle
                .as_ref()
                .is_some_and(|handle| handle.feedback_manager.allows_xai_export());
            if allows_xai_export
                && agent.trace_upload_config().await.is_some()
                && let Some(tn) = turn_number
            {
                let bucket_url = {
                    let cfg = agent.cfg.borrow();
                    cfg.endpoints.resolve_trace_bucket_url().map(|r| r.value)
                };
                submission.unified_log_url = crate::upload::gcs::unified_log_url(
                    bucket_url.as_deref(),
                    &feedback_input.session_id,
                    tn,
                );
            }

            let telemetry_enabled = {
                let cfg = agent.cfg.borrow();
                cfg.is_telemetry_enabled()
                    && allows_xai_export
                    && !agent
                        .auth_manager
                        .current_or_expired()
                        .is_some_and(|a| a.is_zdr_team())
            };
            let client = session_handle
                .as_ref()
                .and_then(|handle| handle.feedback_manager.feedback_client().cloned());
            if client.is_none() {
                tracing::warn!(
                    "no feedback client available (missing proxy credentials); feedback saved locally only"
                );
            }
            // Read the live feedback.user config (the session-actor path uses its
            // spawn-time snapshot); both dedupe through the same process-wide
            // identity cache, so a stable config resolves identically either way.
            // Clone out so the RefCell borrow doesn't span an await.
            let user_cfg = agent.cfg.borrow().feedback.user.clone();
            let author_identity =
                crate::util::user_identity::cached_identity(user_cfg.as_ref()).await;
            let outcome = crate::session::feedback_manager::submit_feedback_workflow(
                &mut submission,
                client.as_ref(),
                session_handle.as_ref().map(|h| &h.persistence_tx),
                crate::session::feedback_manager::SubmitFeedbackOptions {
                    solicited: feedback_input.is_solicited(),
                    telemetry_enabled,
                    author_identity,
                },
            )
            .await;

            match &outcome {
                crate::session::feedback_manager::SubmitOutcome::Submitted => {
                    tracing::info!("feedback submitted to proxy successfully");
                }
                crate::session::feedback_manager::SubmitOutcome::LocalOnly => {
                    tracing::warn!("feedback saved locally only (no proxy client)");
                }
                crate::session::feedback_manager::SubmitOutcome::Failed(e) => {
                    tracing::error!(error = %e, "feedback submission to proxy failed");
                    return Err(acp::Error::internal_error()
                        .data(format!("Feedback submission failed: {e}")));
                }
            }

            let value = serde_json::to_value(FeedbackResponse { success: true })
                .map(|value| serde_json::value::to_raw_value(&value).map(Arc::from))
                .expect("to work")
                .expect("to work");
            Ok(acp::ExtResponse::new(value))
        }
        "x.ai/feedback/dismiss" => {
            let dismiss_input: FeedbackRequestDismiss = parse_params(args)?;
            let session_id = acp::SessionId::new(dismiss_input.session_id.clone());
            let session_handle = agent.resident_handle(&session_id);
            let allows_xai_export = session_handle
                .as_ref()
                .is_some_and(|handle| handle.feedback_manager.allows_xai_export());

            tracing::info!(
                session_id = %dismiss_input.session_id,
                request_id = %dismiss_input.request_id,
                "Feedback request dismissed by user"
            );

            // Count dismissals too (else event_type is always "responded" and
            // response-rate is unknowable), gated like the responded path so a
            // ZDR team emits no survey data and the ratio stays comparable.
            let telemetry_enabled = {
                let cfg = agent.cfg.borrow();
                cfg.is_telemetry_enabled()
                    && allows_xai_export
                    && !agent
                        .auth_manager
                        .current_or_expired()
                        .is_some_and(|a| a.is_zdr_team())
            };
            if telemetry_enabled {
                tracing::info_span!(
                    "feedback.survey",
                    survey_type = "session",
                    event_type = "dismissed",
                    appearance_id = %dismiss_input.request_id,
                    has_feedback_text = false,
                    is_solicited = true,
                )
                .in_scope(|| {});
            }

            // Persist dismiss locally; flushed before storage CopyFile by the persistence actor.
            if let Some(session_handle) = &session_handle {
                session_handle.persist_feedback(LocalFeedbackEntry::UserFeedback(
                    UserFeedbackEntry {
                        submitted_at: chrono::Utc::now(),
                        session_id: dismiss_input.session_id.clone(),
                        turn_number: None,
                        solicited: true,
                        request_id: Some(dismiss_input.request_id.clone()),
                        dismissed: true,
                        submission: None,
                    },
                ));
            }

            let request_id = dismiss_input.request_id.clone();
            let client = session_handle
                .as_ref()
                .and_then(|handle| handle.feedback_manager.feedback_client().cloned())
                .ok_or_else(|| acp::Error::internal_error().data("No credentials for feedback"))?;
            let feedback_base_url = agent.cfg.borrow().endpoints.resolve_feedback_base_url();
            match client.dismiss_request(&request_id).await {
                Ok(response) => {
                    tracing::info!(
                        request_id = %response.request_id,
                        status = %response.status,
                        feedback_url = %feedback_base_url,
                        "Feedback request dismissed"
                    );
                    let value = serde_json::to_value(&response)
                        .map(|value| serde_json::value::to_raw_value(&value).map(Arc::from))
                        .expect("to work")
                        .expect("to work");
                    Ok(acp::ExtResponse::new(value))
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        request_id = %request_id,
                        feedback_url = %feedback_base_url,
                        "Failed to dismiss feedback request"
                    );
                    Err(acp::Error::internal_error()
                        .data(format!("Failed to dismiss feedback request: {e}")))
                }
            }
        }
        _ => Err(acp::Error::method_not_found()),
    }
}

/// Record inline code review events.
///
/// Methods:
/// - `x.ai/review/comment`: record a new inline code comment to cloud storage
/// - `x.ai/review/comment/delete`: record a tombstone event for a deleted comment
async fn handle_review(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/review/comment" => {
            let request: CommentRequest = parse_params(args)?;
            let session_handle =
                agent.resident_handle(&acp::SessionId::new(request.session_id.clone()));
            let provider_boundary = session_handle
                .as_ref()
                .map(|handle| handle.feedback_manager.provider_boundary());

            let comment_id = uuid::Uuid::now_v7().to_string();

            tracing::info!(
                comment_id = %comment_id,
                session_id = %request.session_id,
                prompt_index = request.prompt_index,
                path = %request.citation.path,
                lines = %format!("{}-{}", request.citation.start_line, request.citation.end_line),
                "Comment received"
            );

            let record = serde_json::json!({
                "event": "create",
                "commentId": comment_id,
                "sessionId": request.session_id,
                "promptIndex": request.prompt_index,
                "comment": null,
                "citation": request.citation,
                "agentId": agent_id().to_string(),
                "clientType": format!("{:?}", agent.client_type()),
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });

            if provider_boundary.as_ref().is_some_and(
                |boundary: &crate::session::persistence::ProviderBoundary| {
                    boundary.allows_xai_export()
                },
            ) && let Some(gcs_config) = agent
                .build_gcs_config(format!("{}/comments", request.session_id))
                .await
            {
                let json_bytes = serde_json::to_vec_pretty(&record)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                let gcs_path = format!(
                    "{}/{}.json",
                    gcs_config.gcs_prefix.as_deref().unwrap_or("comments"),
                    comment_id
                );

                let auth_manager = Some(agent.auth_manager.clone());
                let provider_boundary = provider_boundary.expect("checked above");
                tokio::spawn(async move {
                    if !provider_boundary.allows_xai_export() {
                        return;
                    }
                    match upload_bytes(
                        &gcs_config.with_auth(auth_manager),
                        &gcs_path,
                        &json_bytes,
                        "application/json",
                    )
                    .await
                    {
                        Ok(gcs_url) => {
                            tracing::info!(gcs_url = %gcs_url, "Comment uploaded to GCS");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, gcs_path, "Failed to upload comment to GCS");
                        }
                    }
                });
            }

            let value = serde_json::to_value(CommentResponse {
                comment_id,
                recorded: true,
            })
            .map(|value| serde_json::value::to_raw_value(&value).map(Arc::from))
            .expect("to work")
            .expect("to work");
            Ok(acp::ExtResponse::new(value))
        }
        "x.ai/review/comment/delete" => {
            let request: CommentDeleteRequest = parse_params(args)?;
            let session_handle =
                agent.resident_handle(&acp::SessionId::new(request.session_id.clone()));
            let provider_boundary = session_handle
                .as_ref()
                .map(|handle| handle.feedback_manager.provider_boundary());

            tracing::info!(
                comment_id = %request.comment_id,
                session_id = %request.session_id,
                "Comment delete received"
            );

            let record = serde_json::json!({
                "event": "delete",
                "commentId": request.comment_id,
                "sessionId": request.session_id,
                "agentId": agent_id().to_string(),
                "clientType": format!("{:?}", agent.client_type()),
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });

            if provider_boundary.as_ref().is_some_and(
                |boundary: &crate::session::persistence::ProviderBoundary| {
                    boundary.allows_xai_export()
                },
            ) && let Some(gcs_config) = agent
                .build_gcs_config(format!("{}/comments", request.session_id))
                .await
            {
                let json_bytes = serde_json::to_vec_pretty(&record)
                    .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
                let event_id = uuid::Uuid::now_v7().to_string();
                let gcs_path = format!(
                    "{}/{}.json",
                    gcs_config.gcs_prefix.as_deref().unwrap_or("comments"),
                    event_id
                );

                let auth_manager = Some(agent.auth_manager.clone());
                let provider_boundary = provider_boundary.expect("checked above");
                tokio::spawn(async move {
                    if !provider_boundary.allows_xai_export() {
                        return;
                    }
                    match upload_bytes(
                        &gcs_config.with_auth(auth_manager),
                        &gcs_path,
                        &json_bytes,
                        "application/json",
                    )
                    .await
                    {
                        Ok(gcs_url) => {
                            tracing::info!(gcs_url = %gcs_url, "Comment delete event uploaded to GCS");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, gcs_path, "Failed to upload comment delete event to GCS");
                        }
                    }
                });
            }

            let value = serde_json::to_value(CommentDeleteResponse {
                comment_id: request.comment_id,
                deleted: true,
            })
            .map(|value| serde_json::value::to_raw_value(&value).map(Arc::from))
            .expect("to work")
            .expect("to work");
            Ok(acp::ExtResponse::new(value))
        }
        _ => Err(acp::Error::method_not_found()),
    }
}
