//! Feedback, remember-note, btw, and recap dispatchers.

use super::ctx::{NO_SESSION_NOTICE, with_active_agent};
use crate::app::actions::{Effect, FeedbackSendOrigin};
use crate::app::agent::AgentId;
use crate::app::agent_view::{AgentView, PromptInputMode};
use crate::app::app_view::{ActiveView, AppView};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::{SessionEvent, ToolCallBlock};
use crate::views::question_view::{LocalQuestionKind, QuestionViewState};
use std::sync::atomic::{AtomicU64, Ordering};
use xai_grok_feedback::{FeedbackSource, FeedbackTaxonomy, structured_feedback};
use xai_grok_tools::implementations::grok_build::ask_user_question::Question;

/// Monotonic counter for correlating async rewrite responses with the modal
/// that requested them. Prevents stale results from populating a different
/// note's review modal when the user closes and re-opens quickly.
static REWRITE_NONCE: AtomicU64 = AtomicU64::new(0);

fn next_rewrite_nonce() -> u64 {
    REWRITE_NONCE.fetch_add(1, Ordering::Relaxed)
}

/// Bare `/feedback` pane label (first paragraph of the question chrome).
pub(crate) const FEEDBACK_QUESTION_LABEL: &str = "How can we improve Open Grok?";

/// One copy of the send-time thank-you, shared by the immediate and modal commit paths.
pub(crate) const FEEDBACK_THANKS_NOTICE: &str =
    "Thanks for the feedback! The Open Grok team is on it.";

/// Minimal mode has no toast surface, so the notice goes to the transcript instead.
fn feedback_notice(app: &mut AppView, message: &str) {
    if app.screen_mode.is_minimal() {
        with_active_agent(app, |agent| {
            agent
                .scrollback
                .push_block(RenderBlock::system(message.to_string()));
        });
    } else {
        app.show_toast(message);
    }
}

/// Why the bare `/feedback` pane refuses to open, if anything blocks it.
fn feedback_pane_blocked(agent: &AgentView) -> Option<&'static str> {
    if agent.active_subagent.is_some() {
        // A fullscreen subagent view hides the prompt, so the pane would have nowhere to draw while still swallowing every key.
        Some("Close the subagent view before sending feedback")
    } else if agent.question_view.is_some() {
        Some("Finish answering the current question first")
    } else if !agent.no_input_overlay_pending()
        || agent.key_owner() != crate::app::agent_view::KeyOwner::Pane
    {
        // Two ways the pane cannot work here. A permission or plan approval holds the composer, even parked in the scrollback, so the
        // pane would hand it the wrong draft on the way out. A viewer outranks every card for keys, so the box would be untypeable.
        Some("Close or answer what's open before sending feedback")
    } else if agent.session.session_id.is_none() {
        Some(NO_SESSION_NOTICE)
    } else {
        None
    }
}

/// Open the freeform report pane for bare `/feedback`. Inline text never uses this.
pub(super) fn dispatch_open_feedback_pane(
    app: &mut AppView,
    prefill: Option<String>,
    mut images: crate::views::prompt_widget::FeedbackImages,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };

    let blocked = {
        let Some(agent) = app.agents.get(&id) else {
            return vec![];
        };
        feedback_pane_blocked(agent)
    };
    if let Some(message) = blocked {
        feedback_notice(app, message);
        return vec![];
    }

    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let question = Question {
        question: FEEDBACK_QUESTION_LABEL.to_string(),
        options: vec![],
        multi_select: Some(false),
        id: None,
    };
    let stashed = agent.prompt.stash();
    let mut state = QuestionViewState::new(
        format!("feedback-{}", uuid::Uuid::new_v4()),
        vec![question],
        stashed,
    )
    .with_local_kind(LocalQuestionKind::Feedback);
    if let Some(prefill) = prefill
        && let Some(slot) = state.per_question_freeform.get_mut(0)
    {
        *slot = prefill;
    }
    // Freeform-only: start typing immediately.
    let freeform = state.activate_freeform_input();
    agent.prompt.set_text_preserving(&freeform);
    agent.prompt.adopt_images(images.take());
    agent.question_view = Some(state);
    vec![]
}

/// Open the feedback modal (every screen mode). Every refusal is visible.
pub(super) fn dispatch_open_feedback_modal(
    app: &mut AppView,
    open: crate::views::feedback_modal::OpenFeedbackModal,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        if matches!(app.active_view, ActiveView::AgentDashboard)
            && let Some(dashboard) = app.dashboard.as_mut()
        {
            dashboard.dispatch.set_text("");
            dashboard.set_error_toast(NO_SESSION_NOTICE);
        }
        return vec![];
    };
    let blocked = {
        let Some(agent) = app.agents.get(&id) else {
            return vec![];
        };
        agent.feedback_modal_open_blocker().or_else(|| {
            agent
                .session
                .session_id
                .is_none()
                .then_some(NO_SESSION_NOTICE)
        })
    };
    if let Some(message) = blocked {
        feedback_notice(app, message);
        return vec![];
    }
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let draft_id = open.draft_id.clone();
    let modal = crate::views::feedback_modal::FeedbackModalState::new(open);
    let modal_id = modal.id();
    let rehydrations = modal.image_rehydration_requests();
    agent.feedback_modal = Some(modal);
    let mut effects = rehydrations
        .into_iter()
        .map(|(image_identity, path)| Effect::RehydrateFeedbackImage {
            agent_id: id,
            modal_id,
            image_identity,
            path,
        })
        .collect::<Vec<_>>();
    if draft_id.is_none()
        && let Some(modal) = agent.feedback_modal.as_mut()
    {
        modal.start_open_draft_list();
        if let Some(request) = modal.take_pending_request()
            && let Some(session_id) = agent.session.session_id.clone()
        {
            effects.push(Effect::FeedbackDraftRequest {
                agent_id: id,
                session_id,
                request,
            });
        }
    }
    if let Some(draft_id) = draft_id
        && let Some(modal) = agent.feedback_modal.as_mut()
    {
        modal.start_external_draft_load(draft_id);
        if let Some(request) = modal.take_pending_request() {
            let Some(session_id) = agent.session.session_id.clone() else {
                return effects;
            };
            effects.push(Effect::FeedbackDraftRequest {
                agent_id: id,
                session_id,
                request,
            });
        }
    }
    effects
}

/// Submit the open feedback modal. Trace upload stays off until the shell
/// mints a one-shot token; this path still posts taxonomy metadata.
pub(super) fn dispatch_submit_feedback_modal(
    app: &mut AppView,
    modal_id: crate::views::feedback_modal::FeedbackModalId,
) -> Vec<Effect> {
    if !app.uses_xai_access_controls() {
        if let ActiveView::Agent(id) = app.active_view
            && let Some(agent) = app.agents.get(&id)
            && let Some(modal) = agent.feedback_modal.as_ref()
            && modal.matches_id(modal_id)
            && !modal.images().is_empty()
        {
            feedback_notice(
                app,
                "Feedback attachments are unavailable for this provider.",
            );
            return vec![];
        }
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    let session_id = agent.session.session_id.clone();
    let Some(modal) = agent.feedback_modal.as_mut() else {
        return vec![];
    };
    if !modal.matches_id(modal_id) {
        return vec![];
    }
    if modal.in_trace_step() && modal.decided_trace_choice().is_none() {
        return vec![];
    }
    let Some(session_id) = session_id else {
        modal.set_error(NO_SESSION_NOTICE.to_string());
        return vec![];
    };
    if !modal.is_sendable() {
        modal.set_error(crate::views::feedback_modal::FEEDBACK_EMPTY_SUBMIT_ERROR.to_string());
        return vec![];
    }
    let text = modal.submitted_text().trim().to_string();
    modal.reconcile_feedback_images();
    let (encoded_images, dropped) = encode_feedback_image_slice(modal.images());
    if text.is_empty() && encoded_images.is_empty() {
        if let Some(notice) = dropped {
            modal.set_error(notice);
        } else {
            modal.set_error(crate::views::feedback_modal::FEEDBACK_EMPTY_SUBMIT_ERROR.to_string());
        }
        return vec![];
    }
    let draft_id = modal.draft_id().cloned();
    let draft_fields = modal.draft_body();
    if draft_id.is_some() && draft_fields.is_none() {
        modal.cancel_draft_submit_pending("Choose a type before sending this draft.".to_owned());
        return vec![];
    }
    let draft = draft_id
        .clone()
        .zip(draft_fields)
        .map(
            |(draft_id, fields)| crate::app::actions::DraftFeedbackBody {
                draft_id,
                title: fields.title,
                details: text.clone(),
                area: fields.area,
                r#type: fields.r#type,
                task_category: fields.task_category,
                failure_mode: fields.failure_mode,
                images: encoded_images.clone(),
            },
        );
    let metadata = Some(modal.structured_feedback_metadata());
    let images = if draft.is_some() {
        Default::default()
    } else {
        modal.take_images()
    };
    let submission_id = crate::views::feedback_modal::FeedbackSubmissionId::next();
    if draft_id.is_some() {
        modal.mark_draft_submit_pending();
    } else {
        agent.feedback_modal = None;
    }
    if let Some(notice) = dropped {
        agent.scrollback.push_block(RenderBlock::system(notice));
    }
    if draft_id.is_none() {
        agent
            .scrollback
            .push_block(RenderBlock::system(FEEDBACK_THANKS_NOTICE.to_string()));
    }
    vec![feedback_send_effect(
        id,
        session_id,
        text,
        images,
        metadata,
        false,
        draft,
        FeedbackSendOrigin::Modal {
            submission_id,
            modal_id,
            is_draft: draft_id.is_some(),
        },
    )]
}

pub(super) fn dispatch_request_feedback_draft(
    app: &mut AppView,
    request: crate::views::feedback_modal::FeedbackDraftRequest,
) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get(&id) else {
        return vec![];
    };
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    vec![Effect::FeedbackDraftRequest {
        agent_id: id,
        session_id,
        request,
    }]
}

fn encode_feedback_image_slice(
    images: &[crate::prompt_images::PastedImage],
) -> (Vec<xai_grok_shell::session::FeedbackImage>, Option<String>) {
    use base64::Engine as _;

    let loaded: Vec<Option<(Vec<u8>, String)>> = images
        .iter()
        .map(crate::prompt_images::load_for_send)
        .collect();
    let (accepted, notice) = super::inline_feedback::select_feedback_images(&loaded);
    let encoded = accepted
        .into_iter()
        .filter_map(|index| {
            let (bytes, mime_type) = loaded.get(index).and_then(Option::as_ref)?;
            Some(xai_grok_shell::session::FeedbackImage {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                mime_type: mime_type.clone(),
                file_name: images
                    .get(index)
                    .and_then(|img| img.source_path.as_deref())
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned()),
            })
        })
        .collect();
    (encoded, notice)
}

fn feedback_send_effect(
    agent_id: AgentId,
    session_id: agent_client_protocol::SessionId,
    text: String,
    images: crate::views::prompt_widget::FeedbackImages,
    metadata: Option<serde_json::Value>,
    request_trace_upload_token: bool,
    draft: Option<crate::app::actions::DraftFeedbackBody>,
    origin: FeedbackSendOrigin,
) -> Effect {
    crate::unified_log::info(
        "feedback.send",
        Some(session_id.0.as_ref()),
        Some(serde_json::json!({
            "chars": text.chars().count(),
            "images": images.len(),
            "modal": matches!(origin, FeedbackSendOrigin::Modal { .. }),
        })),
    );
    Effect::SendFeedback {
        agent_id,
        session_id,
        feedback_text: text,
        images,
        metadata,
        request_trace_upload_token,
        draft,
        origin,
    }
}

/// Enter remember mode: visual change to prompt bar (remember accent, `#` prefix).
/// No side effects — the user types a memory note and presses Enter to send.
pub(super) fn dispatch_enter_remember_mode(app: &mut AppView) -> Vec<Effect> {
    with_active_agent(app, |agent| {
        agent.prompt_input_mode = PromptInputMode::Remember;
        agent.prompt.set_text("");
    });
    vec![]
}

/// Thank-you is shown immediately; POST is a background effect. The composer is not cleared: the text arrives with the action, not from the prompt.
pub(super) fn dispatch_send_feedback(
    app: &mut AppView,
    text: String,
    images: crate::views::prompt_widget::FeedbackImages,
) -> Vec<Effect> {
    if !images.is_empty() && !app.uses_xai_access_controls() {
        feedback_notice(
            app,
            "Feedback attachments are unavailable for this provider.",
        );
        return vec![];
    }
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    agent.ephemeral_tip.clear_on_submit();

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() && images.is_empty() {
        agent.scrollback.push_block(RenderBlock::system(
            "Please provide feedback text.".to_string(),
        ));
        return vec![];
    }

    let Some(session_id) = agent.session.session_id.clone() else {
        agent
            .scrollback
            .push_block(RenderBlock::system(NO_SESSION_NOTICE.to_string()));
        return vec![];
    };

    agent
        .scrollback
        .push_block(RenderBlock::system(FEEDBACK_THANKS_NOTICE.to_string()));

    vec![feedback_send_effect(
        id,
        session_id,
        trimmed,
        images,
        Some(structured_feedback(
            FeedbackSource::Write,
            FeedbackTaxonomy::default(),
        )),
        false,
        None,
        FeedbackSendOrigin::Immediate,
    )]
}

/// Send a raw remember note for LLM-powered rewriting via `x.ai/memory/rewrite`.
/// Clears remember mode and prompts the LLM to reformat the note with session
/// context. Falls back to direct `SaveMemoryNote` when no session is available.
pub(super) fn dispatch_send_remember_note(app: &mut AppView, text: String) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    agent.prompt_input_mode = PromptInputMode::Normal;
    agent.prompt.set_text("");
    // Submitting a memory note retires any edit-contextual ephemeral tip.
    agent.ephemeral_tip.clear_on_submit();

    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        agent.scrollback.push_block(RenderBlock::system(
            "Please provide a memory note.".to_string(),
        ));
        return vec![];
    }

    agent.note_draft_consumed();
    agent.record_prompt_in_history(&trimmed);

    let cwd = agent.session.cwd.clone();

    let Some(session_id) = agent.session.session_id.clone() else {
        // No session — open modal with raw content only (no LLM rewrite).
        agent.active_modal = Some(ActiveModal::RememberNoteReview {
            raw_content: trimmed.clone(),
            enhanced_content: None, // no session → no LLM rewrite, Tab disabled
            showing_enhanced: false,
            scroll: 0,
            window: crate::views::modal_window::ModalWindowState::new(),
            cached_lines: None,
            cwd,
            agent_id: id,
            rewrite_nonce: 0, // no rewrite in flight, nonce unused
        });
        return vec![];
    };

    // Open modal with raw content, LLM rewrite in flight.
    let nonce = next_rewrite_nonce();
    agent.active_modal = Some(ActiveModal::RememberNoteReview {
        raw_content: trimmed.clone(),
        enhanced_content: None,
        showing_enhanced: false,
        scroll: 0,
        window: crate::views::modal_window::ModalWindowState::new(),
        cached_lines: None,
        cwd: cwd.clone(),
        agent_id: id,
        rewrite_nonce: nonce,
    });

    let context_summary = extract_session_context(agent);

    vec![Effect::RewriteMemoryNote {
        agent_id: id,
        session_id,
        raw_text: trimmed,
        context_summary,
        nonce,
    }]
}

/// Save the currently displayed remember note from the review modal.
pub(super) fn dispatch_save_remember_note_from_modal(app: &mut AppView) -> Vec<Effect> {
    use crate::views::modal::ActiveModal;

    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };

    let (content, cwd) = if let Some(ActiveModal::RememberNoteReview {
        ref raw_content,
        ref enhanced_content,
        showing_enhanced,
        ref cwd,
        ..
    }) = agent.active_modal
    {
        let text = if showing_enhanced {
            enhanced_content.as_deref().unwrap_or(raw_content)
        } else {
            raw_content
        };
        (text.trim().to_string(), cwd.clone())
    } else {
        return vec![];
    };

    agent.active_modal = None;
    agent
        .scrollback
        .push_block(RenderBlock::system("Saving memory note...".to_string()));

    vec![Effect::SaveMemoryNote {
        agent_id: id,
        text: content,
        cwd,
    }]
}

/// Extract session context for the LLM memory rewrite request.
///
/// Walks scrollback in reverse, collecting:
/// - Last 5 user prompts
/// - File paths from recent tool calls (Read, Edit, ListDir)
/// - CWD and git branch
fn extract_session_context(agent: &AgentView) -> String {
    let mut user_prompts: Vec<String> = Vec::new();
    let mut file_paths: Vec<String> = Vec::new();

    // Walk scrollback entries in reverse to collect recent context.
    let len = agent.scrollback.len();
    for i in (0..len).rev() {
        let Some(entry) = agent.scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            RenderBlock::UserPrompt(prompt) => {
                if user_prompts.len() < 5 {
                    let text = if prompt.text.len() > 200 {
                        let end = prompt
                            .text
                            .char_indices()
                            .map(|(i, _)| i)
                            .take_while(|&i| i <= 200)
                            .last()
                            .unwrap_or(0);
                        format!("{}...", &prompt.text[..end])
                    } else {
                        prompt.text.clone()
                    };
                    user_prompts.push(text);
                }
            }
            RenderBlock::ToolCall(tc) => {
                if file_paths.len() < 20 {
                    match tc {
                        ToolCallBlock::Read(b) => {
                            file_paths.push(b.path.clone());
                        }
                        ToolCallBlock::Edit(b) => {
                            file_paths.push(b.path.clone());
                        }
                        ToolCallBlock::ListDir(b) => {
                            file_paths.push(b.path.clone());
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        // Stop early once we have enough context.
        if user_prompts.len() >= 5 && file_paths.len() >= 20 {
            break;
        }
    }

    let mut parts: Vec<String> = Vec::new();

    // CWD
    parts.push(format!("CWD: {}", agent.session.cwd.display()));

    // Git branch
    if let Some(ref branch) = agent.current_branch {
        parts.push(format!("Branch: {branch}"));
    }

    // Recent prompts (chronological order)
    if !user_prompts.is_empty() {
        user_prompts.reverse();
        parts.push("Recent prompts:".to_string());
        for p in &user_prompts {
            parts.push(format!("- {p}"));
        }
    }

    // Recent file paths (deduplicated, preserving first-seen order)
    if !file_paths.is_empty() {
        let mut seen = std::collections::HashSet::new();
        file_paths.retain(|p| seen.insert(p.clone()));
        parts.push("Recent files:".to_string());
        for p in &file_paths {
            parts.push(format!("- {p}"));
        }
    }

    parts.join("\n")
}

/// Send a /btw side question. Bypasses the prompt queue — works even while
/// the agent is mid-turn. Fires an ACP ext method and shows a loading overlay.
pub(super) fn dispatch_send_btw(app: &mut AppView, question: String) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let minimal = app.screen_mode.is_minimal();
    let (session_id, minimal_request_id) = {
        let Some(agent) = app.agents.get_mut(&id) else {
            return vec![];
        };
        let Some(session_id) = agent.session.session_id.clone() else {
            if minimal {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(
                        NO_SESSION_NOTICE,
                    ));
            } else {
                agent.show_toast(NO_SESSION_NOTICE);
            }
            return vec![];
        };

        // Composer clearing belongs to the submit funnel: `dispatch_send_prompt_inner` clears it
        // when `consume_input` is set, so draft-preserving callers (palette, edited
        // queue row) keep theirs.
        let minimal_request_id = if minimal {
            Some(crate::minimal_api::start_minimal_btw(
                agent,
                question.clone(),
            ))
        } else {
            agent.btw_state = Some(crate::views::btw_overlay::BtwOverlayState::Loading {
                question: question.clone(),
            });
            // Prompt keeps focus while the answer is in flight (panel focuses on Done).
            agent.btw_focused = false;
            None
        };
        (session_id, minimal_request_id)
    };

    vec![Effect::SendBtw {
        agent_id: id,
        session_id,
        question,
        minimal_request_id,
    }]
}

/// Toast when a manual `/recap` produces no summary. Empty sessions get a clear
/// empty-state message; anything else (model failure, empty summary, etc.) keeps
/// the generic failure toast.
pub(crate) fn recap_unavailable_toast(has_user_messages: bool) -> &'static str {
    if has_user_messages {
        "Couldn't generate recap"
    } else {
        "No messages yet"
    }
}

/// Whether scrollback already has a user prompt. Scans entries (not
/// `turn_count`) so it stays correct during `begin_batch`/`end_batch` session
/// load, when `push` defers `rebuild_turns` and `turn_count` can stay 0 while
/// replayed prompts are already present.
pub(crate) fn scrollback_has_user_messages(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    scrollback
        .iter_entries()
        .any(|(_, entry)| entry.block.is_user_prompt())
}

/// Request a session recap. Bypasses the prompt queue — works even while the
/// agent is mid-turn. Fires the `x.ai/recap` ext method; the recap arrives
/// asynchronously as a `SessionRecap` notification (rendered in scrollback).
///
/// `auto` is `false` for an explicit `/recap` and `true` for the automatic
/// return-from-away recap. For the manual path we clear the prompt and, when
/// no session exists yet, surface a toast; the auto path is best-effort and
/// silently no-ops without an active session.
pub(super) fn dispatch_send_recap(app: &mut AppView, auto: bool) -> Vec<Effect> {
    let ActiveView::Agent(id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&id) else {
        return vec![];
    };
    if auto && agent.wake_turn_active() {
        return vec![];
    }

    // Shell is authoritative (remote settings / config / env). Skip client requests
    // entirely when the feature is off so we never hit `x.ai/recap`.
    if !app.session_recap_available {
        if !auto {
            agent.show_toast("Session recap is not enabled");
        }
        return vec![];
    }

    let Some(session_id) = agent.session.session_id.clone() else {
        if !auto {
            agent.show_toast(NO_SESSION_NOTICE);
        }
        return vec![];
    };

    if !auto {
        agent.prompt.set_text("");
        // Nothing to summarize yet — show a clear empty-state toast instead of
        // a spinner that ends in "Couldn't generate recap".
        //
        // Skip the short-circuit while session replay is still loading (prompts
        // may not have arrived yet). Prefer an entry scan over `turn_count()`
        // so mid-batch resume (deferred `rebuild_turns`) still sees history.
        if !agent.session.loading_replay && !scrollback_has_user_messages(&agent.scrollback) {
            agent.show_toast(recap_unavailable_toast(false));
            return vec![];
        }
        // Show an immediate loading block with the animated "running" sidebar so
        // the user has feedback that a recap is being generated. The
        // `SessionRecap` handler fills this entry in and stops the animation.
        // Reuse an existing in-flight loading block instead of stacking spinners
        // when `/recap` is pressed repeatedly.
        let already_loading = agent.pending_recap_entry.is_some_and(|eid| {
            agent
                .scrollback
                .get_by_id(eid)
                .is_some_and(|entry| entry.is_running)
        });
        if !already_loading {
            let entry_id =
                agent
                    .scrollback
                    .push(crate::scrollback::entry::ScrollbackEntry::running(
                        RenderBlock::session_event(SessionEvent::Recap {
                            summary: String::new(),
                            auto: false,
                        }),
                    ));
            agent.pending_recap_entry = Some(entry_id);
        }
    } else {
        // Retry backoff only — do not consume the away period on dispatch.
        // The shell often no-ops auto recap until ≥3 min since the last main
        // turn; mark_recap_shown runs when any SessionRecap arrives (auto or
        // manual `/recap`).
        app.notification_service
            .focus_tracker
            .note_auto_recap_attempt();
    }

    vec![Effect::SendRecap { session_id, auto }]
}

// TaskResult handlers.

pub(super) fn handle_memory_note_saved(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<(), String>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        match result {
            Ok(()) => {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Memory saved to {}",
                        crate::util::display_user_grok_path("memory/MEMORY.md")
                    )));
            }
            Err(error) => {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't save memory note: {error}"
                    )));
            }
        }
    }
    vec![]
}

pub(super) fn handle_btw_response(
    app: &mut AppView,
    agent_id: AgentId,
    result: Result<String, String>,
    minimal_request_id: Option<uuid::Uuid>,
) -> Vec<Effect> {
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        use crate::views::btw_overlay::BtwOverlayState;
        if let Some(request_id) = minimal_request_id {
            crate::minimal_api::finish_minimal_btw(agent, request_id, result);
            return vec![];
        }
        let question = match &agent.btw_state {
            Some(BtwOverlayState::Loading { question }) => question.clone(),
            _ => String::new(),
        };
        match result {
            Ok(response) => {
                // Answer arrived: show it (until Esc) and focus the panel
                // so Up/Down scroll it until the user returns to the prompt.
                agent.btw_state = Some(BtwOverlayState::done(question, response));
                agent.btw_focused = true;
            }
            Err(error) => {
                // Error stays until Esc; nothing to scroll, keep prompt focus.
                agent.btw_state = Some(BtwOverlayState::Error { question, error });
                agent.btw_focused = false;
            }
        }
    }
    vec![]
}
