//! Async task-result application: routes task results into state.
use super::auth::{
    ensure_login_method, handle_auth_complete, handle_auth_url_ready,
    handle_codex_session_resume_complete, handle_codex_startup_complete,
    handle_mcp_auth_trigger_done, handle_mcp_setup_submit_done,
};
use super::billing::{
    PAYWALL_AUTO_CHECK_TIMEOUT, apply_auto_topup, handle_billing_fetched,
    handle_check_subscription_complete, handle_credit_limit_recheck_complete,
    handle_gate_refreshed, handle_gate_verify_timeout, handle_usage_fetched,
};
use super::cta::{
    handle_cta_plugin_install_done, handle_cta_plugin_reload_done,
    handle_plugin_cta_catalog_loaded, handle_plugin_cta_debounce_expired,
    handle_plugin_cta_mcps_loaded,
};
use super::ctx::{
    SwitchCause, find_agent_by_session_id, get_active_agent_mut, show_welcome, switch_to_agent,
};
use super::notes::{handle_btw_response, handle_memory_note_saved};
use super::prompt::{
    defer_to_open_reload_window, handle_compact_complete, handle_prompt_response,
    handle_suggestion_debounce_expired,
};
use super::rewind::{
    dispatch_rewind_success, handle_rewind_execute_failed, handle_rewind_points_loaded,
    handle_rewind_preview_complete, handle_rewind_preview_failed,
};
use super::router::{dispatch, dispatch_action_result};
use super::session::foreign::{
    handle_foreign_sessions_scanned, handle_session_list_failed, handle_session_list_loaded,
};
use super::session::fork::{
    handle_fork_session_failed, handle_fork_session_ready, handle_worktree_forked,
};
use super::session::lifecycle::{
    dispatch_exit_session, handle_session_created, handle_session_failed,
    handle_switch_model_complete, handle_worktree_session_created, handle_worktree_session_failed,
};
use super::session::load::{
    handle_card_detail_loaded, handle_codex_session_load_auth_required, handle_deep_search_results,
    handle_session_load_failed, handle_session_loaded, handle_session_restore_failed,
    handle_session_restored, handle_session_search_debounce_expired, remove_session_from_pickers,
};
use super::settings::ui::apply_setting_rollback;
use super::status::{
    commit_session_usage_block, handle_coding_data_sharing_failed,
    handle_coding_data_sharing_updated, handle_context_info_complete, scrub_error_for_toast,
};
use super::transcript::{
    handle_hooks_list_loaded, handle_marketplace_list_loaded, handle_marketplace_updates_available,
    handle_mcp_toggle_done, handle_plugins_list_loaded, handle_skills_toggle_done,
};
use super::turn::handle_bg_task_killed;
use crate::app::actions::{
    ClipboardPasteCompletion, ClipboardPasteContext, ClipboardPasteFailure, ClipboardPasteTarget,
    DoctorFixTarget, DoctorPlanningOutcome, Effect, ProbedAttachment, SubagentKillOutcome,
    TaskResult,
};
use crate::app::agent::AgentId;
use crate::app::app_view::{ActiveView, AppView, AuthState, PrimaryProvider};
use crate::scrollback::block::RenderBlock;
use agent_client_protocol as acp;
pub(super) fn unregister_session_effect(session_id: Option<acp::SessionId>) -> Vec<Effect> {
    session_id
        .map(|sid| Effect::UnregisterActiveSession { session_id: sid })
        .into_iter()
        .collect()
}
pub(super) fn unregister_all_active_sessions(app: &AppView) -> Vec<Effect> {
    app.agents
        .values()
        .filter_map(|a| {
            a.session
                .session_id
                .as_ref()
                .map(|sid| Effect::UnregisterActiveSession {
                    session_id: sid.clone(),
                })
        })
        .collect()
}

fn push_codex_auth_result(
    app: &mut AppView,
    agent_id: Option<crate::app::agent::AgentId>,
    message: String,
) {
    if let Some(agent_id) = agent_id
        && let Some(agent) = app.agents.get_mut(&agent_id)
    {
        agent.scrollback.push_block(RenderBlock::system(message));
    } else {
        app.show_toast(&message);
    }
}

/// Apply the catalog returned synchronously with independent Codex login. A
/// successful live refresh normally broadcasts the same state, but the
/// cached/embedded fallback path deliberately has no broadcast of its own.
fn apply_codex_login_models(app: &mut AppView, model_state: acp::SessionModelState) -> Vec<Effect> {
    let new_models = crate::acp::model_state::ModelState::from(Some(model_state));
    let shell_fallback_current = new_models.current.clone();
    let mut app_models = new_models.clone();
    if let ActiveView::Agent(id) = app.active_view
        && let Some(agent) = app.agents.get(&id)
        && let Some(agent_model) = agent.session.models.current.as_ref()
        && app_models.available.contains_key(agent_model)
    {
        app_models.current = Some(agent_model.clone());
    }
    app.models = app_models;
    for agent in app.agents.values_mut() {
        agent
            .session
            .models
            .update_catalog(new_models.available.clone(), shell_fallback_current.clone());
    }
    app.sync_primary_provider_from_active_agent()
}

pub(super) const X11_PRIMARY_PASTE_HINT: &str = "Try Shift+Insert to paste selected text";
fn show_clipboard_toast(target: &ClipboardPasteTarget, message: &str, app: &mut AppView) {
    match target {
        ClipboardPasteTarget::AgentPrompt { agent_id, .. } => {
            if let Some(agent) = app.agents.get_mut(agent_id) {
                agent.show_toast(message);
            }
        }
        ClipboardPasteTarget::DashboardDispatch | ClipboardPasteTarget::DashboardPeek { .. } => {
            if let Some(dashboard) = app.dashboard.as_mut() {
                dashboard.error_toast = Some(message.to_owned());
            }
        }
    }
}
pub(super) fn maybe_show_x11_primary_paste_hint(
    eligible: bool,
    completion: ClipboardPasteCompletion,
    target: &ClipboardPasteTarget,
    app: &mut AppView,
) {
    if !eligible || completion != ClipboardPasteCompletion::FullMiss {
        return;
    }
    show_clipboard_toast(target, X11_PRIMARY_PASTE_HINT, app);
}
/// Whether a completed clipboard probe should fall through to the `grok wrap`
/// host-image request. A clean `FullMiss` always qualifies; a remote read
/// *error* (`AttachmentRead`) also qualifies because inside `grok wrap` the
/// authoritative pasteboard is the local host's, not the (absent) remote one, so
/// the error is recoverable over the wrap OSC path. Every other failure
/// (`TextRead`, `TargetInsertion`, `AlreadyReported`) is a real dead end and
/// must keep toasting. The request itself still self-gates on
/// `osc52_sink_active()`, so this is inert outside `grok wrap`.
pub(super) fn wrap_host_image_request_eligible(completion: ClipboardPasteCompletion) -> bool {
    matches!(
        completion,
        ClipboardPasteCompletion::FullMiss
            | ClipboardPasteCompletion::Failed(ClipboardPasteFailure::AttachmentRead)
    )
}
pub(super) fn show_clipboard_failure(
    target: &ClipboardPasteTarget,
    failure: ClipboardPasteFailure,
    app: &mut AppView,
) {
    let message = match failure {
        ClipboardPasteFailure::AlreadyReported => return,
        ClipboardPasteFailure::TextRead => "Couldn't read clipboard text",
        ClipboardPasteFailure::AttachmentRead => "Couldn't read clipboard contents",
        ClipboardPasteFailure::TargetInsertion => "Couldn't paste clipboard contents",
    };
    show_clipboard_toast(target, message, app);
}
fn apply_clipboard_paste_result(
    ctx: ClipboardPasteContext,
    image: ProbedAttachment,
    file_urls: Option<String>,
    app: &mut AppView,
) -> ClipboardPasteCompletion {
    match ctx.target.clone() {
        ClipboardPasteTarget::AgentPrompt { agent_id, .. } => app
            .agents
            .get_mut(&agent_id)
            .map_or(ClipboardPasteCompletion::Dropped, |agent| {
                agent.complete_clipboard_attachment_paste(ctx, image, file_urls)
            }),
        ClipboardPasteTarget::DashboardDispatch | ClipboardPasteTarget::DashboardPeek { .. } => app
            .dashboard
            .as_mut()
            .map_or(ClipboardPasteCompletion::Dropped, |dashboard| {
                dashboard.complete_clipboard_attachment_paste(ctx, image, file_urls)
            }),
    }
}
fn drain_clipboard_target(target: &ClipboardPasteTarget, app: &mut AppView) -> Vec<Effect> {
    match target {
        ClipboardPasteTarget::AgentPrompt { agent_id, .. } => {
            let is_active = app.active_view == ActiveView::Agent(*agent_id);
            let Some(agent) = app.agents.get_mut(agent_id) else {
                return vec![];
            };
            let resend = agent.take_deferred_send_after_paste();
            let action = if is_active {
                resend.and_then(|kind| agent.build_deferred_send_action(kind))
            } else {
                None
            };
            let mut effects = std::mem::take(&mut agent.pending_effects);
            if let Some(action) = action {
                effects.extend(dispatch(action, app));
            }
            effects
        }
        ClipboardPasteTarget::DashboardDispatch | ClipboardPasteTarget::DashboardPeek { .. } => {
            let Some(dashboard) = app.dashboard.as_mut() else {
                return vec![];
            };
            let resends = dashboard.take_deferred_sends_after_paste();
            let mut effects = std::mem::take(&mut dashboard.pending_effects);
            if matches!(app.active_view, ActiveView::AgentDashboard) {
                for action in resends {
                    effects.extend(dispatch(action, app));
                }
            }
            effects
        }
    }
}
fn apply_kimi_catalog(app: &mut AppView, model_state: acp::SessionModelState) {
    let new_models = crate::acp::model_state::ModelState::from(Some(model_state));
    let fallback_current = new_models.current.clone();
    let mut app_models = new_models.clone();
    if let ActiveView::Agent(agent_id) = app.active_view
        && let Some(current) = app
            .agents
            .get(&agent_id)
            .and_then(|agent| agent.session.models.current.as_ref())
        && app_models.available.contains_key(current)
    {
        app_models.current = Some(current.clone());
    }
    app.models = app_models;
    for agent in app.agents.values_mut() {
        agent
            .session
            .models
            .update_catalog(new_models.available.clone(), fallback_current.clone());
    }
}

fn capture_kimi_sessions_created_during_update(app: &mut AppView) {
    let mut targets = Vec::new();
    for (&agent_id, agent) in &mut app.agents {
        if PrimaryProvider::for_current_model(&agent.session.models) == Some(PrimaryProvider::Kimi)
        {
            agent.session.provider_rebind_pending = true;
            targets.push(agent_id);
        }
    }
    app.pending_kimi_rebind_agents.extend(targets);
}

fn pending_kimi_model(
    models: &crate::acp::model_state::ModelState,
    endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint,
) -> Option<acp::ModelId> {
    let is_kimi = |model_id: &acp::ModelId| {
        PrimaryProvider::for_model(models, model_id) == Some(PrimaryProvider::Kimi)
    };
    let exact = |slug: &str| {
        let id = acp::ModelId::new(slug);
        (models.available.contains_key(&id) && is_kimi(&id)).then_some(id)
    };
    let code = models
        .current
        .clone()
        .filter(|id| is_kimi(id) && id.0.as_ref().starts_with("kimi-for-coding"))
        .or_else(|| exact("kimi-for-coding"))
        .or_else(|| exact("kimi-for-coding-highspeed"));
    let platform = models
        .current
        .clone()
        .filter(|id| is_kimi(id) && !id.0.as_ref().starts_with("kimi-for-coding"))
        .or_else(|| exact("kimi-k3"))
        .or_else(|| {
            models
                .available
                .keys()
                .find(|id| is_kimi(id) && !id.0.as_ref().starts_with("kimi-for-coding"))
                .cloned()
        });
    match endpoint {
        xai_grok_shell::kimi_models::KimiApiEndpoint::Code => code,
        xai_grok_shell::kimi_models::KimiApiEndpoint::Platform => platform,
    }
}

fn rebind_pending_kimi_sessions(
    app: &mut AppView,
    endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint,
    generation: u64,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    let targets = app
        .pending_kimi_rebind_agents
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let mut completed = Vec::new();
    for agent_id in targets {
        let Some(agent) = app.agents.get_mut(&agent_id) else {
            completed.push(agent_id);
            continue;
        };
        if !agent.session.provider_rebind_pending {
            completed.push(agent_id);
            continue;
        }
        if agent.session.model_switch_pending {
            continue;
        }
        let Some(session_id) = agent.session.session_id.clone() else {
            // Creation/load may already be racing the runtime update. Keep the
            // hold and retry from the session-ready result once it has an ID.
            continue;
        };
        let Some(model_id) = pending_kimi_model(&agent.session.models, endpoint) else {
            tracing::warn!(?agent_id, %endpoint, "Kimi sampler rebind has no matching model");
            agent.scrollback.push_block(RenderBlock::system(format!(
                "No {} model is available for this session; queued prompts are paused. Adjust the model allowlist or switch this tab to another provider.",
                kimi_endpoint_label(endpoint),
            )));
            continue;
        };
        let effort = (agent.session.models.current.as_ref() == Some(&model_id))
            .then_some(agent.session.models.reasoning_effort)
            .flatten();

        agent.session.model_switch_pending = true;
        effects.push(Effect::RebindKimiModel {
            agent_id,
            session_id,
            model_id,
            effort,
            generation,
            effective_endpoint: endpoint,
        });
    }
    for agent_id in completed {
        app.pending_kimi_rebind_agents.remove(&agent_id);
    }
    effects
}

fn after_kimi_session_ready(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    mut effects: Vec<Effect>,
) -> Vec<Effect> {
    if app.kimi_runtime_update_pending {
        if let Some(agent) = app.agents.get_mut(&agent_id)
            && PrimaryProvider::for_current_model(&agent.session.models)
                == Some(PrimaryProvider::Kimi)
        {
            agent.session.provider_rebind_pending = true;
            app.pending_kimi_rebind_agents.insert(agent_id);
        }
        return effects;
    }
    if app.pending_kimi_rebind_agents.contains(&agent_id)
        && app.agents.get(&agent_id).is_some_and(|agent| {
            agent.session.provider_rebind_pending && !agent.session.model_switch_pending
        })
    {
        let endpoint = app.kimi_effective_endpoint;
        let generation = app.kimi_active_operation_generation;
        effects.extend(rebind_pending_kimi_sessions(app, endpoint, generation));
    }
    effects
}

fn mark_runtime_pending_perplexity_session(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    incoming_models: Option<&acp::SessionModelState>,
) {
    if !app.perplexity_web_search_update_pending {
        return;
    }
    let incoming = incoming_models
        .cloned()
        .map(|models| crate::acp::model_state::ModelState::from(Some(models)));
    let is_kimi = incoming.as_ref().map_or_else(
        || {
            app.agents.get(&agent_id).is_some_and(|agent| {
                PrimaryProvider::for_current_model(&agent.session.models)
                    == Some(PrimaryProvider::Kimi)
            })
        },
        |models| PrimaryProvider::for_current_model(models) == Some(PrimaryProvider::Kimi),
    );
    if is_kimi && let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.provider_rebind_pending = true;
        app.pending_perplexity_rebuild_agents.insert(agent_id);
    }
}

fn finish_perplexity_web_search_update(app: &mut AppView) -> Vec<Effect> {
    let agent_ids = app
        .pending_perplexity_rebuild_agents
        .drain()
        .collect::<Vec<_>>();
    let mut effects = Vec::new();
    for agent_id in agent_ids {
        if app.pending_kimi_rebind_agents.contains(&agent_id) {
            continue;
        }
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            agent.session.provider_rebind_pending = false;
            effects.extend(crate::app::dispatch::maybe_drain_queue_and_note_peek(
                app, agent_id,
            ));
        }
    }
    effects
}

fn reconcile_pending_perplexity_session_after_model_switch(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
) -> Vec<Effect> {
    if !app.perplexity_web_search_update_pending {
        return vec![];
    }
    let is_kimi = app.agents.get(&agent_id).is_some_and(|agent| {
        PrimaryProvider::for_current_model(&agent.session.models) == Some(PrimaryProvider::Kimi)
    });
    if is_kimi {
        if let Some(agent) = app.agents.get_mut(&agent_id) {
            agent.session.provider_rebind_pending = true;
            app.pending_perplexity_rebuild_agents.insert(agent_id);
        }
        return vec![];
    }

    app.pending_perplexity_rebuild_agents.remove(&agent_id);
    if app.pending_kimi_rebind_agents.contains(&agent_id) {
        return vec![];
    }
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    agent.session.provider_rebind_pending = false;
    crate::app::dispatch::maybe_drain_queue_and_note_peek(app, agent_id)
}

fn mark_runtime_pending_kimi_session(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    incoming_models: Option<&acp::SessionModelState>,
) {
    // A load/create response can race the completion of a Kimi runtime
    // mutation. The shell may have captured the old sampler before the
    // mutation completed even though the pager observes this result after the
    // global pending flag was cleared. Once this process has performed a Kimi
    // mutation, conservatively rebind every subsequently-ready Kimi session.
    if !app.kimi_runtime_update_pending && app.kimi_active_operation_generation == 0 {
        return;
    }
    let incoming = incoming_models
        .cloned()
        .map(|models| crate::acp::model_state::ModelState::from(Some(models)));
    let is_kimi = incoming.as_ref().map_or_else(
        || {
            app.agents.get(&agent_id).is_some_and(|agent| {
                PrimaryProvider::for_current_model(&agent.session.models)
                    == Some(PrimaryProvider::Kimi)
            })
        },
        |models| PrimaryProvider::for_current_model(models) == Some(PrimaryProvider::Kimi),
    );
    if incoming.is_some() && !is_kimi {
        app.cancel_pending_kimi_rebind(agent_id);
        return;
    }
    if is_kimi && let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.provider_rebind_pending = true;
        app.pending_kimi_rebind_agents.insert(agent_id);
    }
}

fn finish_kimi_rebind(app: &mut AppView, agent_id: crate::app::agent::AgentId) {
    app.pending_kimi_rebind_agents.remove(&agent_id);
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.provider_rebind_pending = false;
    }
}

fn handle_kimi_model_rebind_complete(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    session_id: acp::SessionId,
    model_id: acp::ModelId,
    effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
    generation: u64,
    effective_endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint,
    result: Result<(), crate::app::actions::SwitchModelError>,
) -> Vec<Effect> {
    let still_owned = app.pending_kimi_rebind_agents.contains(&agent_id);
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        app.pending_kimi_rebind_agents.remove(&agent_id);
        return vec![];
    };
    if agent.session.session_id.as_ref() != Some(&session_id) {
        if !agent.session.provider_rebind_pending {
            app.pending_kimi_rebind_agents.remove(&agent_id);
            return vec![];
        }
        if agent.session.model_switch_pending {
            return vec![];
        }
        return rebind_pending_kimi_sessions(
            app,
            app.kimi_effective_endpoint,
            app.kimi_active_operation_generation,
        );
    }
    if !still_owned || !agent.session.provider_rebind_pending {
        // An authoritative local/remote switch canceled this automatic
        // refresh while its ACP request was in flight. The shell may have
        // accepted the late Kimi request, so reconcile it back to the model
        // the pager now considers current before releasing the queue.
        agent.session.model_switch_pending = false;
        let Some(target_model) = agent.session.models.current.clone() else {
            return crate::app::dispatch::maybe_drain_queue(agent).effects;
        };
        if target_model == model_id {
            return crate::app::dispatch::maybe_drain_queue(agent).effects;
        }
        let Some(current_session_id) = agent.session.session_id.clone() else {
            agent.session.provider_rebind_pending = true;
            return vec![];
        };
        let target_effort = agent.session.models.reasoning_effort;
        // This is a reconciliation-only hold: the automatic Kimi operation no
        // longer owns the tab, so keep it out of `pending_kimi_rebind_agents`.
        // If the corrective switch fails, queue draining must remain blocked
        // because the late Kimi request may now own the shell sampler.
        agent.session.provider_rebind_pending = true;
        agent.session.model_switch_pending = true;
        return vec![Effect::SwitchModel {
            agent_id,
            session_id: current_session_id,
            model_id: target_model,
            effort: target_effort,
            prev_model_id: None,
        }];
    }
    agent.session.model_switch_pending = false;

    if generation != app.kimi_active_operation_generation
        || effective_endpoint != app.kimi_effective_endpoint
    {
        // The shell accepted (or rejected) an obsolete rebind. Keep the
        // provider hold and immediately reconcile to the latest catalog.
        return rebind_pending_kimi_sessions(
            app,
            app.kimi_effective_endpoint,
            app.kimi_active_operation_generation,
        );
    }

    match result {
        Ok(()) => {
            // This is a sampler refresh, not a user preference change. Update
            // only the live session mirror: never overwrite models.default or
            // user_model_preference for background tabs.
            agent.session.models.set_current(model_id, effort);
        }
        Err(error) => {
            agent.scrollback.push_block(RenderBlock::system(format!(
                "Couldn't refresh the Kimi session after its service changed; queued prompts are paused. Update the selected Kimi key or switch this tab to another provider. ({})",
                match error {
                    crate::app::actions::SwitchModelError::Other(message) => {
                        scrub_error_for_toast(&message)
                    }
                    crate::app::actions::SwitchModelError::IncompatibleAgent { .. } => {
                        "the current agent is incompatible with the selected Kimi model".to_owned()
                    }
                },
            )));
            // Fail closed: the old sampler may still hold the previous
            // service/key. A later Kimi settings operation retries; an
            // explicit switch to a non-Kimi provider cancels this hold.
            return vec![];
        }
    }
    finish_kimi_rebind(app, agent_id);

    let mut effects = crate::app::dispatch::maybe_drain_queue_and_note_peek(app, agent_id);
    if matches!(app.active_view, ActiveView::Agent(active) if active == agent_id) {
        effects.extend(app.sync_primary_provider_from_active_agent());
    }
    effects
}

fn capture_fireworks_sessions_created_during_update(app: &mut AppView) {
    let mut targets = Vec::new();
    for (&agent_id, agent) in &mut app.agents {
        if PrimaryProvider::for_current_model(&agent.session.models)
            == Some(PrimaryProvider::Fireworks)
        {
            agent.session.provider_rebind_pending = true;
            targets.push(agent_id);
        }
    }
    app.pending_fireworks_rebind_agents.extend(targets);
}

fn pending_fireworks_model(models: &crate::acp::model_state::ModelState) -> Option<acp::ModelId> {
    let is_fireworks = |model_id: &acp::ModelId| {
        PrimaryProvider::for_model(models, model_id) == Some(PrimaryProvider::Fireworks)
    };
    models
        .current
        .clone()
        .filter(|id| is_fireworks(id))
        .or_else(|| models.available.keys().find(|id| is_fireworks(id)).cloned())
}

fn rebind_pending_fireworks_sessions(app: &mut AppView, generation: u64) -> Vec<Effect> {
    let mut effects = Vec::new();
    let targets = app
        .pending_fireworks_rebind_agents
        .iter()
        .copied()
        .collect::<Vec<_>>();
    let mut completed = Vec::new();
    for agent_id in targets {
        let Some(agent) = app.agents.get_mut(&agent_id) else {
            completed.push(agent_id);
            continue;
        };
        if !agent.session.provider_rebind_pending {
            completed.push(agent_id);
            continue;
        }
        if agent.session.model_switch_pending {
            continue;
        }
        let Some(session_id) = agent.session.session_id.clone() else {
            // Creation/load may already be racing the runtime update. Keep the
            // hold and retry from the session-ready result once it has an ID.
            continue;
        };
        let Some(model_id) = pending_fireworks_model(&agent.session.models) else {
            tracing::warn!(?agent_id, "Fireworks sampler rebind has no matching model");
            agent.scrollback.push_block(RenderBlock::system(
                "No Fireworks AI model is available for this session; queued prompts are paused. Adjust the model allowlist or switch this tab to another provider.".to_owned(),
            ));
            continue;
        };
        let effort = (agent.session.models.current.as_ref() == Some(&model_id))
            .then_some(agent.session.models.reasoning_effort)
            .flatten();

        agent.session.model_switch_pending = true;
        effects.push(Effect::RebindFireworksModel {
            agent_id,
            session_id,
            model_id,
            effort,
            generation,
        });
    }
    for agent_id in completed {
        app.pending_fireworks_rebind_agents.remove(&agent_id);
    }
    effects
}

fn after_fireworks_session_ready(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    mut effects: Vec<Effect>,
) -> Vec<Effect> {
    if app.fireworks_runtime_update_pending {
        if let Some(agent) = app.agents.get_mut(&agent_id)
            && PrimaryProvider::for_current_model(&agent.session.models)
                == Some(PrimaryProvider::Fireworks)
        {
            agent.session.provider_rebind_pending = true;
            app.pending_fireworks_rebind_agents.insert(agent_id);
        }
        return effects;
    }
    if app.pending_fireworks_rebind_agents.contains(&agent_id)
        && app.agents.get(&agent_id).is_some_and(|agent| {
            agent.session.provider_rebind_pending && !agent.session.model_switch_pending
        })
    {
        let generation = app.fireworks_operation_generation;
        effects.extend(rebind_pending_fireworks_sessions(app, generation));
    }
    effects
}

fn mark_runtime_pending_fireworks_session(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    incoming_models: Option<&acp::SessionModelState>,
) {
    // A load/create response can race the completion of a Fireworks runtime
    // mutation. The shell may have captured the old sampler before the
    // mutation completed even though the pager observes this result after the
    // global pending flag was cleared. Once this process has performed a
    // Fireworks mutation, conservatively rebind every subsequently-ready
    // Fireworks session.
    if !app.fireworks_runtime_update_pending && app.fireworks_operation_generation == 0 {
        return;
    }
    let incoming = incoming_models
        .cloned()
        .map(|models| crate::acp::model_state::ModelState::from(Some(models)));
    let is_fireworks = incoming.as_ref().map_or_else(
        || {
            app.agents.get(&agent_id).is_some_and(|agent| {
                PrimaryProvider::for_current_model(&agent.session.models)
                    == Some(PrimaryProvider::Fireworks)
            })
        },
        |models| PrimaryProvider::for_current_model(models) == Some(PrimaryProvider::Fireworks),
    );
    if incoming.is_some() && !is_fireworks {
        app.cancel_pending_fireworks_rebind(agent_id);
        return;
    }
    if is_fireworks && let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.provider_rebind_pending = true;
        app.pending_fireworks_rebind_agents.insert(agent_id);
    }
}

fn finish_fireworks_rebind(app: &mut AppView, agent_id: crate::app::agent::AgentId) {
    app.pending_fireworks_rebind_agents.remove(&agent_id);
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        agent.session.provider_rebind_pending = false;
    }
}

fn handle_fireworks_model_rebind_complete(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    session_id: acp::SessionId,
    model_id: acp::ModelId,
    effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
    generation: u64,
    result: Result<(), crate::app::actions::SwitchModelError>,
) -> Vec<Effect> {
    let still_owned = app.pending_fireworks_rebind_agents.contains(&agent_id);
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        app.pending_fireworks_rebind_agents.remove(&agent_id);
        return vec![];
    };
    if agent.session.session_id.as_ref() != Some(&session_id) {
        if !agent.session.provider_rebind_pending {
            app.pending_fireworks_rebind_agents.remove(&agent_id);
            return vec![];
        }
        if agent.session.model_switch_pending {
            return vec![];
        }
        return rebind_pending_fireworks_sessions(app, app.fireworks_operation_generation);
    }
    if !still_owned || !agent.session.provider_rebind_pending {
        // An authoritative local/remote switch canceled this automatic
        // refresh while its ACP request was in flight. The shell may have
        // accepted the late Fireworks request, so reconcile it back to the
        // model the pager now considers current before releasing the queue.
        agent.session.model_switch_pending = false;
        let Some(target_model) = agent.session.models.current.clone() else {
            return crate::app::dispatch::maybe_drain_queue(agent).effects;
        };
        if target_model == model_id {
            return crate::app::dispatch::maybe_drain_queue(agent).effects;
        }
        let Some(current_session_id) = agent.session.session_id.clone() else {
            agent.session.provider_rebind_pending = true;
            return vec![];
        };
        let target_effort = agent.session.models.reasoning_effort;
        // Reconciliation-only hold: the automatic Fireworks operation no
        // longer owns the tab, so keep it out of
        // `pending_fireworks_rebind_agents`. If the corrective switch fails,
        // queue draining must remain blocked because the late Fireworks
        // request may now own the shell sampler.
        agent.session.provider_rebind_pending = true;
        agent.session.model_switch_pending = true;
        return vec![Effect::SwitchModel {
            agent_id,
            session_id: current_session_id,
            model_id: target_model,
            effort: target_effort,
            prev_model_id: None,
        }];
    }
    agent.session.model_switch_pending = false;

    if generation != app.fireworks_operation_generation {
        // The shell accepted (or rejected) an obsolete rebind. Keep the
        // provider hold and immediately reconcile to the latest catalog.
        return rebind_pending_fireworks_sessions(app, app.fireworks_operation_generation);
    }

    match result {
        Ok(()) => {
            // This is a sampler refresh, not a user preference change. Update
            // only the live session mirror: never overwrite models.default or
            // user_model_preference for background tabs.
            agent.session.models.set_current(model_id, effort);
        }
        Err(error) => {
            agent.scrollback.push_block(RenderBlock::system(format!(
                "Couldn't refresh the Fireworks AI session after its credential changed; queued prompts are paused. Update the Fireworks AI key or switch this tab to another provider. ({})",
                match error {
                    crate::app::actions::SwitchModelError::Other(message) => {
                        scrub_error_for_toast(&message)
                    }
                    crate::app::actions::SwitchModelError::IncompatibleAgent { .. } => {
                        "the current agent is incompatible with the selected Fireworks AI model"
                            .to_owned()
                    }
                },
            )));
            // Fail closed: the old sampler may still hold the previous key. A
            // later Fireworks settings operation retries; an explicit switch
            // to another provider cancels this hold.
            return vec![];
        }
    }
    finish_fireworks_rebind(app, agent_id);

    let mut effects = crate::app::dispatch::maybe_drain_queue_and_note_peek(app, agent_id);
    if matches!(app.active_view, ActiveView::Agent(active) if active == agent_id) {
        effects.extend(app.sync_primary_provider_from_active_agent());
    }
    effects
}

fn fireworks_credential_configured() -> bool {
    xai_grok_shell::fireworks_models::environment_api_key_is_configured()
        || xai_grok_shell::auth::provider_api_key_is_configured(
            &xai_grok_tools::util::grok_home::grok_home(),
            xai_grok_shell::sampling::types::ModelProvider::Fireworks,
        )
}

fn kimi_endpoint_label(endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint) -> &'static str {
    match endpoint {
        xai_grok_shell::kimi_models::KimiApiEndpoint::Platform => "Kimi Platform",
        xai_grok_shell::kimi_models::KimiApiEndpoint::Code => "Kimi Code",
    }
}

fn kimi_credential_configured(endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint) -> bool {
    xai_grok_shell::kimi_models::environment_api_key_is_configured(endpoint)
        || xai_grok_shell::auth::kimi_api_key_is_configured(
            &xai_grok_tools::util::grok_home::grok_home(),
            endpoint,
        )
}

fn handle_swarm_prompt_setup_failed(
    app: &mut AppView,
    agent_id: crate::app::agent::AgentId,
    prompt_id: String,
    text: String,
    error: String,
    pending_rollback_enabled: Option<bool>,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    if agent.session.current_prompt_id.as_deref() != Some(prompt_id.as_str()) {
        tracing::debug!(
            agent = ?agent_id,
            prompt_id,
            current_prompt_id = ?agent.session.current_prompt_id,
            "Ignoring stale swarm prompt setup failure"
        );
        return vec![];
    }

    if let Some(enabled) = pending_rollback_enabled {
        agent.pending_swarm_mode_rollback = Some(enabled);
    }

    if let Some(stashed) = agent.session.in_flight_prompt.take() {
        let cursor = stashed.text.len();
        agent.prompt.set_text(&stashed.text);
        agent.prompt.restore_chip_elements(&stashed.chip_elements);
        agent.prompt.set_images(stashed.images);
        agent.prompt.set_cursor(cursor);
        agent.scrollback.remove_entry(stashed.scrollback_entry);
    } else {
        let cursor = text.len();
        agent.prompt.set_text(&text);
        agent.prompt.set_cursor(cursor);
    }

    agent.session.finish_turn(&mut agent.scrollback);
    agent.mark_turn_finished();
    agent.activity_started_at = None;
    agent.last_activity = None;
    agent.show_toast(&format!("Swarm setup failed — prompt restored: {error}"));
    vec![]
}

pub(crate) fn current_doctor_target(
    app: &AppView,
    target: &DoctorFixTarget,
) -> Option<DoctorFixTarget> {
    let agent = app.agents.get(&target.agent_id)?;
    if agent.session.cwd != target.cwd {
        return None;
    }
    match (&target.session_id, &agent.session.session_id) {
        (Some(expected), Some(current))
            if expected == current
                && target.session_binding_epoch == agent.session_binding_epoch =>
        {
            Some(target.clone())
        }
        (None, Some(current))
            if agent.session_binding_epoch == target.session_binding_epoch.wrapping_add(1) =>
        {
            Some(DoctorFixTarget {
                session_id: Some(current.clone()),
                session_binding_epoch: agent.session_binding_epoch,
                ..target.clone()
            })
        }
        (None, None) if target.session_binding_epoch == agent.session_binding_epoch => {
            Some(target.clone())
        }
        _ => None,
    }
}
pub(crate) fn deliver_doctor_message(app: &mut AppView, preferred: AgentId, message: String) {
    let destination = app
        .agents
        .contains_key(&preferred)
        .then_some(preferred)
        .or_else(|| match app.active_view {
            ActiveView::Agent(id) if app.agents.contains_key(&id) => Some(id),
            _ => app.agents.keys().next().copied(),
        });
    if let Some(destination) = destination
        && let Some(agent) = app.agents.get_mut(&destination)
    {
        agent.scrollback.push_block(RenderBlock::system(message));
        return;
    }
    app.startup_warnings.push(crate::startup::StartupWarning {
        severity: crate::startup::WarningSeverity::Info,
        message,
        action: None,
    });
}
/// Handle a completed async task result.
pub(super) fn dispatch_task_result(result: TaskResult, app: &mut AppView) -> Vec<Effect> {
    match result {
        TaskResult::PerplexityWebSearchUpdated {
            enabled,
            api_key_configured,
            generation,
            error,
            reconciled,
        } => {
            if generation != app.perplexity_web_search_generation {
                return vec![];
            }
            app.perplexity_web_search_enabled = enabled;
            app.perplexity_web_search_update_pending = !reconciled;
            super::settings::ui::refresh_open_settings_modals(app);
            if let Some(error) = error {
                app.show_toast(&format!(
                    "✗ Could not update Perplexity web search: {}{}",
                    scrub_error_for_toast(&error),
                    if reconciled {
                        ""
                    } else {
                        "; queued Kimi prompts remain paused"
                    }
                ));
            } else {
                app.show_toast(&format!(
                    "✓ Perplexity web search: {}; API key {}",
                    if enabled { "on" } else { "off" },
                    if api_key_configured {
                        "configured"
                    } else {
                        "required"
                    }
                ));
            }
            if reconciled {
                finish_perplexity_web_search_update(app)
            } else {
                vec![]
            }
        }
        TaskResult::KimiApiKeyUpdated {
            endpoint,
            effective_endpoint,
            generation,
            configured,
            active,
            warning,
            error,
            models,
            stale,
        } => {
            if stale || (active && generation != app.kimi_active_operation_generation) {
                return vec![];
            }
            if active {
                capture_kimi_sessions_created_during_update(app);
            }
            let storage_succeeded = error.is_none();
            super::settings::ui::refresh_open_settings_modals(app);
            let credential_status = match endpoint {
                xai_grok_shell::kimi_models::KimiApiEndpoint::Platform => {
                    super::settings::ui::kimi_api_key_status()
                }
                xai_grok_shell::kimi_models::KimiApiEndpoint::Code => {
                    super::settings::ui::kimi_code_api_key_status()
                }
            };
            let label = kimi_endpoint_label(endpoint);
            let runtime_apply_unconfirmed = active && warning.is_some() && models.is_none();
            if let Some(error) = error {
                app.show_toast(&format!(
                    "✗ Could not {} {label} API key: {}{}",
                    if configured { "save" } else { "remove" },
                    scrub_error_for_toast(&error),
                    if active {
                        "; queued Kimi prompts remain paused"
                    } else {
                        ""
                    },
                ));
            } else {
                let message = if configured {
                    if credential_status == crate::settings::SecretStatus::EnvironmentOverride {
                        format!(
                            "✓ {label} API key saved to UI storage; environment key remains active"
                        )
                    } else if warning.is_some() || !active {
                        format!("✓ {label} API key saved")
                    } else {
                        format!("✓ {label} API key saved; models refreshed")
                    }
                } else if credential_status == crate::settings::SecretStatus::EnvironmentOverride {
                    format!("✓ UI-stored {label} API key cleared; environment key remains active")
                } else {
                    format!("✓ UI-stored {label} API key cleared")
                };
                if let Some(warning) = warning {
                    let operation = if configured
                        || credential_status == crate::settings::SecretStatus::EnvironmentOverride
                    {
                        "model query"
                    } else {
                        "catalog clear"
                    };
                    app.show_toast(&format!(
                        "{message}; {operation} warning: {}{}",
                        scrub_error_for_toast(&warning),
                        if runtime_apply_unconfirmed {
                            "; queued Kimi prompts remain paused"
                        } else {
                            ""
                        },
                    ));
                } else {
                    app.show_toast(&message);
                }
            }

            if !storage_succeeded {
                // A prior serialized endpoint operation may already have
                // changed the runtime before this credential write failed.
                // Keep active Kimi tabs fail-closed until retry/cancellation.
                return vec![];
            }
            if !active {
                return vec![];
            }
            if runtime_apply_unconfirmed {
                // No ACP catalog means the runtime apply itself was not
                // confirmed (as opposed to a Platform `/models` warning,
                // which still returns the rebuilt fallback catalog). Keep the
                // old sampler fail-closed until a retry can reconcile it.
                return vec![];
            }

            app.kimi_effective_endpoint = effective_endpoint;
            app.kimi_confirmed_endpoint =
                xai_grok_shell::kimi_models::KimiApiEndpoint::from_canonical(
                    &app.kimi_api_endpoint,
                )
                .unwrap_or_default();
            app.kimi_runtime_update_pending = false;
            if let Some(models) = models {
                apply_kimi_catalog(app, models);
                super::settings::ui::refresh_open_settings_modals(app);
            }
            if !configured && !kimi_credential_configured(effective_endpoint) {
                app.show_toast(&format!(
                    "✓ {} API key {}; API key required and queued Kimi prompts remain paused",
                    kimi_endpoint_label(endpoint),
                    if configured { "saved" } else { "cleared" },
                ));
                return vec![];
            }

            // A loaded Kimi session owns a sampler whose credential was
            // resolved when the session was created. Re-selecting the same
            // model rebuilds that sampler immediately after a save or clear,
            // so callers never have to restart the pager for the credential
            // change to take effect.
            rebind_pending_kimi_sessions(app, effective_endpoint, generation)
        }
        TaskResult::KimiApiEndpointUpdated {
            endpoint,
            previous,
            effective_endpoint,
            generation,
            credential_configured,
            warning,
            error,
            models,
            stale,
        } => {
            if stale || generation != app.kimi_active_operation_generation {
                return vec![];
            }
            capture_kimi_sessions_created_during_update(app);
            if let Some(error) = error {
                super::settings::setters::set_kimi_api_endpoint_inner(app, previous);
                // `previous` is read from durable config inside the serialized
                // effect, so it may be newer than the pager's last completion.
                // Adopt it as the rollback anchor; otherwise selecting that
                // same value cannot retry the still-fail-closed live apply.
                app.kimi_confirmed_endpoint = previous;
                super::settings::ui::refresh_open_settings_modals(app);
                super::settings::ui::restart_failed_kimi_provider_login(app);
                app.show_toast(&format!(
                    "✗ Could not switch Kimi service: {}; queued Kimi prompts remain paused",
                    scrub_error_for_toast(&error),
                ));
                return vec![];
            }

            if let Some(models) = models {
                apply_kimi_catalog(app, models);
            }
            super::settings::setters::set_kimi_api_endpoint_inner(app, endpoint);
            app.kimi_effective_endpoint = effective_endpoint;
            app.kimi_confirmed_endpoint = endpoint;
            app.kimi_runtime_update_pending = false;
            super::settings::ui::refresh_open_settings_modals(app);
            let message = format!("✓ Kimi service: {}", kimi_endpoint_label(endpoint));
            if let Some(warning) = warning {
                app.show_toast(&format!(
                    "{message}; model refresh warning: {}",
                    scrub_error_for_toast(&warning),
                ));
            } else {
                app.show_toast(&format!(
                    "{message}; {}",
                    if credential_configured {
                        "models refreshed"
                    } else {
                        "API key required"
                    }
                ));
            }
            if !credential_configured {
                return vec![];
            }
            rebind_pending_kimi_sessions(app, effective_endpoint, generation)
        }
        TaskResult::SessionCreated {
            agent_id,
            session_id,
            models: new_models,
        } => {
            mark_runtime_pending_kimi_session(app, agent_id, new_models.as_ref());
            mark_runtime_pending_fireworks_session(app, agent_id, new_models.as_ref());
            mark_runtime_pending_perplexity_session(app, agent_id, new_models.as_ref());
            let effects = handle_session_created(app, agent_id, session_id, new_models);
            let effects = after_kimi_session_ready(app, agent_id, effects);
            after_fireworks_session_ready(app, agent_id, effects)
        }
        TaskResult::SessionFailed { agent_id, error } => {
            handle_session_failed(app, agent_id, error)
        }
        TaskResult::WorktreeSessionCreated {
            agent_id,
            session_id,
            worktree_path,
            session_cwd,
            models: new_models,
        } => {
            mark_runtime_pending_kimi_session(app, agent_id, new_models.as_ref());
            mark_runtime_pending_fireworks_session(app, agent_id, new_models.as_ref());
            mark_runtime_pending_perplexity_session(app, agent_id, new_models.as_ref());
            let effects = handle_worktree_session_created(
                app,
                agent_id,
                session_id,
                worktree_path,
                session_cwd,
                new_models,
            );
            let effects = after_kimi_session_ready(app, agent_id, effects);
            after_fireworks_session_ready(app, agent_id, effects)
        }
        TaskResult::WorktreeForked {
            agent_id,
            session_id,
            worktree_path,
            session_cwd,
            code_restored,
            restore_summary,
            restore_degree,
        } => handle_worktree_forked(
            app,
            agent_id,
            session_id,
            worktree_path,
            session_cwd,
            code_restored,
            restore_summary,
            restore_degree,
        ),
        TaskResult::WorktreeSessionFailed { agent_id, error } => {
            handle_worktree_session_failed(app, agent_id, error)
        }
        TaskResult::ForkSessionReady {
            agent_id,
            new_session_id,
            cwd,
        } => handle_fork_session_ready(app, agent_id, new_session_id, cwd),
        TaskResult::ForkSessionFailed { agent_id, error } => {
            handle_fork_session_failed(app, agent_id, error)
        }
        TaskResult::BillingFetched {
            agent_id,
            balance,
            silent,
            subscription_tier,
            autotopup,
        } => handle_billing_fetched(app, agent_id, balance, silent, subscription_tier, autotopup),
        TaskResult::UsageFetched {
            agent_id,
            xai,
            codex,
        } => handle_usage_fetched(app, agent_id, xai, codex),
        TaskResult::BillingError {
            agent_id,
            error,
            silent,
        } => {
            if app.uses_xai_access_controls()
                && !silent
                && let Some(agent) = app.agents.get_mut(&agent_id)
            {
                agent.scrollback.push_block(RenderBlock::System(
                    crate::scrollback::blocks::SystemMessageBlock::new(format!(
                        "Billing error: {error}"
                    )),
                ));
            }
            vec![]
        }
        TaskResult::AppBillingFetched { balance, autotopup } => {
            if app.uses_xai_access_controls() {
                app.credit_balance = balance;
                apply_auto_topup(&mut app.auto_topup, &autotopup);
            }
            vec![]
        }
        TaskResult::GateRefreshed { settings } => handle_gate_refreshed(app, settings),
        TaskResult::SessionLoaded {
            agent_id,
            session_id,
            models: new_models,
            code_restored,
            restore_summary,
            restore_degree,
            running_prompt_id,
        } => {
            mark_runtime_pending_kimi_session(app, agent_id, new_models.as_ref());
            mark_runtime_pending_fireworks_session(app, agent_id, new_models.as_ref());
            mark_runtime_pending_perplexity_session(app, agent_id, new_models.as_ref());
            let effects = handle_session_loaded(
                app,
                agent_id,
                session_id,
                new_models,
                code_restored,
                restore_summary,
                restore_degree,
                running_prompt_id,
            );
            let effects = after_kimi_session_ready(app, agent_id, effects);
            after_fireworks_session_ready(app, agent_id, effects)
        }
        TaskResult::SessionTitleFromDisk { agent_id, title } => {
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let Some((t, is_manual)) = title.filter(|(s, _)| !s.trim().is_empty())
            {
                if is_manual && agent.display_name.is_none() {
                    agent.display_name = Some(t.clone());
                }
                agent.generated_session_title = Some(t);
            }
            vec![]
        }
        TaskResult::SessionLoadFailed {
            agent_id,
            session_id,
            error,
        } => handle_session_load_failed(app, agent_id, session_id, error),
        TaskResult::CodexSessionLoadAuthRequired {
            agent_id,
            session_id,
        } => handle_codex_session_load_auth_required(app, agent_id, session_id),
        TaskResult::SessionListLoaded {
            sessions,
            partial,
            scope,
            seq,
            query,
        } => handle_session_list_loaded(app, sessions, partial, scope, seq, query),
        TaskResult::ForeignSessionsScanned { entries, seq } => {
            handle_foreign_sessions_scanned(app, entries, seq)
        }
        TaskResult::ForeignResumeCwdCanonicalized {
            requested_cwd,
            canonical_cwd,
            launch_token,
        } => {
            let accepted_cwd = canonical_cwd.clone();
            if app.accept_foreign_resume_canonical_cwd(launch_token, &requested_cwd, canonical_cwd)
                && let Some(canonical_cwd) = accepted_cwd
            {
                vec![Effect::DetectForeignResumeHint {
                    canonical_cwd,
                    compat: app.foreign_session_compat,
                    grok_home: xai_grok_tools::util::grok_home::grok_home(),
                    launch_token,
                }]
            } else {
                vec![]
            }
        }
        TaskResult::ForeignResumeHintDetected {
            canonical_cwd,
            launch_token,
            hint,
        } => {
            app.apply_foreign_resume_detection(launch_token, &canonical_cwd, hint);
            vec![]
        }
        TaskResult::SessionListFailed { error, seq, query } => {
            handle_session_list_failed(app, error, seq, query)
        }
        TaskResult::SessionSearchDebounceExpired { query, seq } => {
            handle_session_search_debounce_expired(app, query, seq)
        }
        TaskResult::RosterLoaded { sessions } => {
            app.leader_roster = sessions;
            app.dashboard_sessions_loading = false;
            vec![]
        }
        TaskResult::RosterFailed { error } => {
            tracing::debug!(error = %error, "leader roster fetch failed");
            app.dashboard_sessions_loading = false;
            vec![]
        }
        TaskResult::DashboardSessionsLoaded { sessions } => {
            app.dashboard_local_sessions = sessions;
            app.dashboard_sessions_loading = false;
            vec![]
        }
        TaskResult::CardDetailLoaded {
            source,
            session_id,
            generation,
            detail,
        } => handle_card_detail_loaded(app, source, session_id, generation, detail),
        TaskResult::SessionRestored {
            agent_id,
            local_session_id,
        } => {
            mark_runtime_pending_kimi_session(app, agent_id, None);
            mark_runtime_pending_fireworks_session(app, agent_id, None);
            let effects = handle_session_restored(app, agent_id, local_session_id);
            let effects = after_kimi_session_ready(app, agent_id, effects);
            after_fireworks_session_ready(app, agent_id, effects)
        }
        TaskResult::SessionRestoreFailed { agent_id, error } => {
            handle_session_restore_failed(app, agent_id, error)
        }
        TaskResult::SessionRestoreProgress { agent_id, message } => {
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && !defer_to_open_reload_window(agent, agent_id, "SessionRestoreProgress")
            {
                agent.scrollback.push_block(RenderBlock::system(message));
            }
            vec![]
        }
        TaskResult::PromptResponse {
            agent_id,
            result,
            http_status,
            prompt_id,
        } => handle_prompt_response(app, agent_id, result, http_status, prompt_id),
        TaskResult::SwarmPromptSetupFailed {
            agent_id,
            prompt_id,
            text,
            error,
            pending_rollback_enabled,
        } => handle_swarm_prompt_setup_failed(
            app,
            agent_id,
            prompt_id,
            text,
            error,
            pending_rollback_enabled,
        ),
        TaskResult::SendPromptNowFailed {
            agent_id,
            session_id,
            prompt_id,
            error,
            blocks,
        } => {
            let sid = session_id.0.to_string();
            super::queue::retire_optimistic_echo(
                &mut app.optimistic_prompt_echoes,
                &mut app.shared_prompt_queues,
                &sid,
                &prompt_id,
            );
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.shared_queue.retain(|e| e.id != prompt_id);
                agent.note_queue_echo_retired(&prompt_id);
                if agent.expect_send_now_cancel.as_deref() == Some(prompt_id.as_str())
                    || agent.follow_without_jump_prompt_id.as_deref() == Some(prompt_id.as_str())
                {
                    agent.clear_send_now_expectation();
                }
                agent.retire_send_now_painted_block(&prompt_id);
                let text = blocks
                    .iter()
                    .find_map(|b| match b {
                        acp::ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let id = agent.session.next_queue_id;
                agent.session.next_queue_id += 1;
                agent
                    .session
                    .pending_prompts
                    .push_front(crate::app::agent::QueuedPrompt {
                        wire_blocks: Some(blocks),
                        ..crate::app::agent::QueuedPrompt::plain(
                            id,
                            &text,
                            crate::app::agent::QueueEntryKind::Prompt,
                        )
                    });
                agent.show_toast(&format!("Send now failed — requeued: {error}"));
            }
            vec![]
        }
        TaskResult::PreferredModelPersisted { result } => {
            if let Err(err) = result
                && let Some(agent) = get_active_agent_mut(app)
            {
                agent.scrollback.push_block(RenderBlock::system(format!(
                    "Couldn't save preferred model: {err} (still active for this session)"
                )));
            }
            vec![]
        }
        TaskResult::CancelComplete => {
            tracing::trace!("Cancel notification sent successfully");
            vec![]
        }
        TaskResult::KillSubagentComplete {
            session_id,
            subagent_id,
            outcome,
        } => {
            if let SubagentKillOutcome::NothingLive { status } = outcome {
                let status = status.as_deref().unwrap_or("cancelled");
                crate::app::acp_handler::finalize_killed_subagent(
                    app,
                    &session_id,
                    &subagent_id,
                    status,
                );
            }
            vec![]
        }
        TaskResult::CompactComplete { agent_id, result } => {
            handle_compact_complete(app, agent_id, result)
        }
        TaskResult::SwitchModelComplete {
            agent_id,
            model_id,
            effort,
            result,
            prev_model_id,
        } => {
            let switch_succeeded = result.is_ok();
            let target_provider = app
                .agents
                .get(&agent_id)
                .and_then(|agent| PrimaryProvider::for_model(&agent.session.models, &model_id));
            // A hold owned by the Fireworks pending set is released on leaving
            // Fireworks; every other hold (Kimi set, or a reconciliation-only
            // session flag) keeps the pre-Fireworks rule: released on leaving
            // Kimi.
            let held_by_fireworks = app.pending_fireworks_rebind_agents.contains(&agent_id);
            let left_kimi = switch_succeeded
                && !held_by_fireworks
                && target_provider != Some(PrimaryProvider::Kimi);
            if left_kimi {
                app.cancel_pending_kimi_rebind(agent_id);
            }
            let left_fireworks = switch_succeeded
                && held_by_fireworks
                && target_provider != Some(PrimaryProvider::Fireworks);
            if left_fireworks {
                app.cancel_pending_fireworks_rebind(agent_id);
            }
            let mut effects = handle_switch_model_complete(
                app,
                agent_id,
                model_id,
                effort,
                result,
                prev_model_id,
            );
            if switch_succeeded {
                effects.extend(reconcile_pending_perplexity_session_after_model_switch(
                    app, agent_id,
                ));
            }
            if app.pending_kimi_rebind_agents.contains(&agent_id)
                && app.agents.get(&agent_id).is_some_and(|agent| {
                    agent.session.provider_rebind_pending && !agent.session.model_switch_pending
                })
            {
                effects.extend(rebind_pending_kimi_sessions(
                    app,
                    app.kimi_effective_endpoint,
                    app.kimi_active_operation_generation,
                ));
            }
            if app.pending_fireworks_rebind_agents.contains(&agent_id)
                && app.agents.get(&agent_id).is_some_and(|agent| {
                    agent.session.provider_rebind_pending && !agent.session.model_switch_pending
                })
            {
                effects.extend(rebind_pending_fireworks_sessions(
                    app,
                    app.fireworks_operation_generation,
                ));
            }
            effects
        }
        TaskResult::KimiModelRebindComplete {
            agent_id,
            session_id,
            model_id,
            effort,
            generation,
            effective_endpoint,
            result,
        } => handle_kimi_model_rebind_complete(
            app,
            agent_id,
            session_id,
            model_id,
            effort,
            generation,
            effective_endpoint,
            result,
        ),
        TaskResult::FireworksApiKeyUpdated {
            configured,
            generation,
            stale,
            warning,
            error,
            models,
        } => {
            if stale || generation != app.fireworks_operation_generation {
                return vec![];
            }
            capture_fireworks_sessions_created_during_update(app);
            let storage_succeeded = error.is_none();
            super::settings::ui::refresh_open_settings_modals(app);
            let credential_status = super::settings::ui::fireworks_api_key_status();
            let runtime_apply_unconfirmed = warning.is_some() && models.is_none();
            if let Some(error) = error {
                app.show_toast(&format!(
                    "✗ Could not {} Fireworks AI API key: {}; queued Fireworks prompts remain paused",
                    if configured { "save" } else { "remove" },
                    scrub_error_for_toast(&error),
                ));
            } else {
                let message = if configured {
                    if credential_status == crate::settings::SecretStatus::EnvironmentOverride {
                        "✓ Fireworks AI API key saved to UI storage; environment key remains active"
                            .to_owned()
                    } else if warning.is_some() {
                        "✓ Fireworks AI API key saved".to_owned()
                    } else {
                        "✓ Fireworks AI API key saved; models refreshed".to_owned()
                    }
                } else if credential_status == crate::settings::SecretStatus::EnvironmentOverride {
                    "✓ UI-stored Fireworks AI API key cleared; environment key remains active"
                        .to_owned()
                } else {
                    "✓ UI-stored Fireworks AI API key cleared".to_owned()
                };
                if let Some(warning) = warning {
                    app.show_toast(&format!(
                        "{message}; model query warning: {}{}",
                        scrub_error_for_toast(&warning),
                        if runtime_apply_unconfirmed {
                            "; queued Fireworks prompts remain paused"
                        } else {
                            ""
                        },
                    ));
                } else {
                    app.show_toast(&message);
                }
            }

            if !storage_succeeded {
                // Keep active Fireworks tabs fail-closed until retry.
                return vec![];
            }
            if runtime_apply_unconfirmed {
                // No ACP catalog means the runtime apply itself was not
                // confirmed. Keep the old sampler fail-closed until a retry
                // can reconcile it.
                return vec![];
            }

            app.fireworks_runtime_update_pending = false;
            if let Some(models) = models {
                apply_kimi_catalog(app, models);
                super::settings::ui::refresh_open_settings_modals(app);
            }
            if !fireworks_credential_configured() {
                app.show_toast(&format!(
                    "✓ Fireworks AI API key {}; API key required and queued Fireworks prompts remain paused",
                    if configured { "saved" } else { "cleared" },
                ));
                return vec![];
            }

            // A loaded Fireworks session owns a sampler whose credential was
            // resolved when the session was created. Re-selecting the same
            // model rebuilds that sampler immediately after a save or clear,
            // so callers never have to restart the pager for the credential
            // change to take effect.
            rebind_pending_fireworks_sessions(app, generation)
        }
        TaskResult::FireworksModelRebindComplete {
            agent_id,
            session_id,
            model_id,
            effort,
            generation,
            result,
        } => handle_fireworks_model_rebind_complete(
            app, agent_id, session_id, model_id, effort, generation, result,
        ),
        TaskResult::BgTaskKilled {
            session_id,
            task_id,
            outcome,
        } => handle_bg_task_killed(app, session_id, task_id, outcome),
        TaskResult::BgTaskKillFailed {
            session_id,
            task_id,
            error,
        } => {
            tracing::warn!(task_id = %task_id, error = %error, "Failed to kill bg task");
            if let Some(agent) = find_agent_by_session_id(&mut app.agents, &session_id)
                && let Some(task) = agent.session.bg_tasks.get_mut(&task_id)
            {
                task.pending_kill = false;
                task.kill_requested_at = None;
            }
            vec![]
        }
        TaskResult::ChangelogFetched { markdown, entries } => {
            app.changelog_markdown = markdown;
            app.changelog_bullets =
                xai_grok_shell::util::changelog::bullets_from_entries(&entries, 3);
            vec![]
        }
        TaskResult::ClipboardAttachmentProbed {
            ctx,
            image,
            file_urls,
        } => {
            let is_clipboard_key = ctx.source.is_clipboard_key();
            let primary_hint_eligible = is_clipboard_key
                && !app.screen_mode.is_minimal()
                && crate::clipboard::x11_primary_guidance_available();
            let target = ctx.target.clone();
            let wrap_text = if is_clipboard_key {
                ctx.source.text().map(str::to_owned)
            } else {
                None
            };
            let completion = apply_clipboard_paste_result(ctx, image, file_urls, app);
            let wrap_request_emitted = wrap_host_image_request_eligible(completion)
                && is_clipboard_key
                && crate::wrap_clipboard_image::maybe_request_wrap_host_image(
                    None,
                    wrap_text.as_deref(),
                    None,
                );
            let effects = drain_clipboard_target(&target, app);
            maybe_show_x11_primary_paste_hint(
                primary_hint_eligible && !wrap_request_emitted,
                completion,
                &target,
                app,
            );
            if let ClipboardPasteCompletion::Failed(failure) = completion
                && !wrap_request_emitted
            {
                show_clipboard_failure(&target, failure, app);
            }
            effects
        }
        TaskResult::PromptImagePreviewPrepared => vec![],
        TaskResult::DoctorFixPlanned { target, result } => {
            let Some(target) = current_doctor_target(app, &target) else {
                deliver_doctor_message(
                    app,
                    target.agent_id,
                    "This fix was cancelled because the session changed. Run `/doctor fix` again."
                        .to_owned(),
                );
                return vec![];
            };
            match result {
                Ok(DoctorPlanningOutcome::Listing(listing)) => {
                    deliver_doctor_message(app, target.agent_id, listing);
                }
                Ok(DoctorPlanningOutcome::Plan(plan)) => {
                    super::prompt::open_doctor_fix_question(app, target, plan);
                }
                Ok(DoctorPlanningOutcome::RunLocally(command)) => {
                    deliver_doctor_message(
                        app,
                        target.agent_id,
                        format!(
                            "This fix configures your local computer, not this SSH session.\nOn your local computer, run: {command}"
                        ),
                    );
                }
                Err(error) => deliver_doctor_message(
                    app,
                    target.agent_id,
                    if error.starts_with("Could not prepare the fix:") {
                        error
                    } else {
                        format!("Could not prepare the fix: {error}")
                    },
                ),
            }
            vec![]
        }
        TaskResult::DoctorFixApplied { target, result } => {
            let message = match result {
                Ok(outcome) => crate::diagnostics::format_fix_success(&outcome),
                Err(error) if error.starts_with("Could not apply the fix:") => error,
                Err(error) => format!("Could not apply the fix: {error}"),
            };
            deliver_doctor_message(app, target.agent_id, message);
            vec![]
        }
        TaskResult::AnnouncementsHiddenPersisted { result } => {
            if let Err(e) = result {
                tracing::warn!("Failed to persist announcements hidden state: {}", e);
            }
            vec![]
        }
        TaskResult::PromptHistoryLoaded { agent_id, prompts } => {
            use xai_grok_tools::implementations::skills::skill::extract_skill_display_text;
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.session.prompt_history_loading = false;
                agent.session.prompt_history = prompts
                    .into_iter()
                    .map(|p| extract_skill_display_text(&p).unwrap_or(p))
                    .collect();
                if agent.prompt.history_search.is_active() {
                    let history = agent.combined_prompt_history();
                    agent.prompt.history_search.refresh_items(&history);
                    if !agent.prompt.history_search.is_browse() {
                        let query = agent.prompt.text().to_owned();
                        agent.prompt.history_search.update_query(&query);
                    }
                }
            }
            vec![]
        }
        TaskResult::AuthComplete { request_seq, meta } => {
            handle_auth_complete(app, request_seq, meta)
        }
        TaskResult::AuthFailed { request_seq, error } => {
            if let AuthState::Authenticating {
                request_seq: current_seq,
                ..
            } = &app.auth_state
                && *current_seq == request_seq
            {
                let startup_xai_choice = app.auth_return_view.is_none()
                    && app.primary_provider == crate::app::app_view::PrimaryProvider::Xai
                    && app.startup_provider_selection
                        == Some(crate::app::app_view::PrimaryProvider::Xai);
                if startup_xai_choice {
                    app.clear_xai_access_controls();
                    app.startup_provider_selection = None;
                    app.auth_state = AuthState::ProviderChoice {
                        error: Some(format!("xAI Grok login failed: {error}")),
                    };
                    app.welcome_menu_index = Some(0);
                } else {
                    app.auth_state = AuthState::Pending { error: Some(error) };
                }
                app.auth_code_input.reset();
            }
            vec![]
        }
        TaskResult::AuthUrlReady {
            request_seq,
            auth_url,
            external,
            mode,
        } => handle_auth_url_ready(app, request_seq, auth_url, external, mode),
        TaskResult::AuthCodeSubmitted { .. } => vec![],
        TaskResult::AuthCancelComplete => vec![],
        TaskResult::McpsListLoaded { agent_id, result } => {
            use crate::views::extensions_modal::TabDataState;
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let Some(ref mut modal) = agent.extensions_modal
            {
                modal.pending_action = None;
                modal.pending_entry_index = None;
                modal.mcps_data = match result {
                    Ok(response) => TabDataState::Loaded(response),
                    Err(e) => TabDataState::Error(e),
                };
            }
            vec![]
        }
        TaskResult::McpAuthTriggerDone {
            agent_id,
            server_name,
            result,
        } => handle_mcp_auth_trigger_done(app, agent_id, server_name, result),
        TaskResult::McpSetupSubmitDone {
            agent_id,
            server_name,
            result,
        } => handle_mcp_setup_submit_done(app, agent_id, server_name, result),
        TaskResult::HooksListLoaded { agent_id, result } => {
            handle_hooks_list_loaded(app, agent_id, result)
        }
        TaskResult::PluginsListLoaded { agent_id, result } => {
            handle_plugins_list_loaded(app, agent_id, result)
        }
        TaskResult::HooksActionResult { agent_id, result }
        | TaskResult::PluginsActionResult { agent_id, result }
        | TaskResult::MarketplaceActionResult { agent_id, result } => {
            dispatch_action_result(app, agent_id, result)
        }
        TaskResult::CtaPluginInstallDone {
            agent_id,
            plugin_name,
            result,
        } => handle_cta_plugin_install_done(app, agent_id, plugin_name, result),
        TaskResult::CtaPluginReloadDone {
            agent_id,
            plugin_name,
            result,
        } => handle_cta_plugin_reload_done(app, agent_id, plugin_name, result),
        TaskResult::PluginCtaMcpsLoaded {
            agent_id,
            plugin_name,
            result,
        } => handle_plugin_cta_mcps_loaded(app, agent_id, plugin_name, result),
        TaskResult::CtaInstalledDismissTimeout {
            agent_id,
            plugin_name,
        } => {
            use crate::app::agent_view::CtaPhase;
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let CtaPhase::Installed { name } = &agent.plugin_cta.phase
                && *name == plugin_name
            {
                agent.plugin_cta.phase = CtaPhase::Hidden;
            }
            vec![]
        }
        TaskResult::McpToggleDone { agent_id, result } => {
            handle_mcp_toggle_done(app, agent_id, result)
        }
        TaskResult::MarketplaceUpdatesAvailable { agent_id, updates } => {
            handle_marketplace_updates_available(app, agent_id, updates)
        }
        TaskResult::MarketplaceListLoaded { agent_id, result } => {
            handle_marketplace_list_loaded(app, agent_id, result)
        }
        TaskResult::PluginCtaCatalogLoaded { agent_id, result } => {
            handle_plugin_cta_catalog_loaded(app, agent_id, result)
        }
        TaskResult::SkillsListLoaded { agent_id, result } => {
            use crate::views::extensions_modal::TabDataState;
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let Some(ref mut modal) = agent.extensions_modal
            {
                modal.skills_data = match result {
                    Ok(skills) => TabDataState::Loaded(skills),
                    Err(e) => TabDataState::Error(e),
                };
                modal.pending_action = None;
                modal.pending_entry_index = None;
            }
            vec![]
        }
        TaskResult::WorkflowsListLoaded {
            agent_id,
            session_id,
            result,
        } => {
            use crate::views::extensions_modal::TabDataState;
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && agent.session.session_id.as_ref() == Some(&session_id)
                && let Some(ref mut modal) = agent.extensions_modal
            {
                modal.workflows_data = match result {
                    Ok(workflows) => TabDataState::Loaded(workflows),
                    Err(e) => TabDataState::Error(e),
                };
            }
            vec![]
        }
        TaskResult::SkillsToggleDone { agent_id, result } => {
            handle_skills_toggle_done(app, agent_id, result)
        }
        TaskResult::ShareSessionComplete {
            agent_id,
            share_url,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Session shared: {share_url}"
                    )));
            }
            vec![]
        }
        TaskResult::ShareSessionFailed { agent_id, error } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't share session: {error}"
                    )));
            }
            vec![]
        }
        TaskResult::SessionAgentNameResolved {
            agent_id,
            agent_name,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.session_agent_name = agent_name.clone();
                if let Some(modal) = agent.agents_modal.as_mut() {
                    modal.active_agent = agent_name;
                }
            }
            vec![]
        }
        TaskResult::SessionInfoComplete {
            agent_id,
            info,
            text,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.session_agent_name = info.data.agent_name.clone();
                if let Some(modal) = agent.agents_modal.as_mut() {
                    modal.active_agent = info.data.agent_name.clone();
                }
                agent.apply_full_context_info(info.data.context);
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(text));
            }
            vec![]
        }
        TaskResult::SessionInfoFailed { agent_id, error } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't load session info: {error}"
                    )));
            }
            vec![]
        }
        TaskResult::CodingDataSharingUpdated { agent_id, opted_in } => {
            handle_coding_data_sharing_updated(app, agent_id, opted_in)
        }
        TaskResult::CodingDataSharingFailed {
            agent_id,
            error,
            rollback_to_opted_in,
        } => handle_coding_data_sharing_failed(app, agent_id, error, rollback_to_opted_in),
        TaskResult::RenameSessionComplete { agent_id, title } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                let safe = crate::views::session_title::sanitize_display_text(&title);
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Session renamed to \"{safe}\""
                    )));
            }
            vec![]
        }
        TaskResult::RenameSessionFailed { agent_id, error } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't rename session: {error}"
                    )));
            }
            vec![]
        }
        TaskResult::DeleteSessionComplete { source, session_id } => {
            remove_session_from_pickers(app, &source, &session_id);
            app.show_toast("Session deleted");
            vec![]
        }
        TaskResult::DeleteSessionFailed {
            source,
            session_id,
            error,
        } => {
            tracing::warn!(source, session_id = %session_id, error = %error, "session delete failed");
            app.show_toast(&format!("Couldn't delete session: {error}"));
            vec![]
        }
        TaskResult::ContextInfoComplete { agent_id, info } => {
            handle_context_info_complete(app, agent_id, info)
        }
        TaskResult::ContextInfoFailed { agent_id, error } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't load context info: {error}"
                    )));
            }
            vec![]
        }
        TaskResult::SessionUsageComplete {
            agent_id,
            session_id,
            usage,
        } => commit_session_usage_block(
            app,
            agent_id,
            &session_id,
            crate::app::status_blocks::session_usage_block_text(&usage),
        ),
        TaskResult::SessionUsageFailed {
            agent_id,
            session_id,
            error,
        } => commit_session_usage_block(
            app,
            agent_id,
            &session_id,
            format!("Couldn't load session usage: {error}"),
        ),
        TaskResult::FeedbackComplete { .. } => vec![],
        TaskResult::FeedbackFailed { agent_id, error } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .scrollback
                    .push_block(crate::scrollback::block::RenderBlock::system(format!(
                        "Couldn't send feedback: {error}"
                    )));
            }
            vec![]
        }
        TaskResult::MemoryNoteSaved { agent_id, result } => {
            handle_memory_note_saved(app, agent_id, result)
        }
        TaskResult::MemoryNoteRewritten {
            agent_id,
            result,
            nonce,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id)
                && let Ok(markdown) = result
                && let Some(crate::views::modal::ActiveModal::RememberNoteReview {
                    ref mut enhanced_content,
                    ref mut cached_lines,
                    rewrite_nonce,
                    ..
                }) = agent.active_modal
                && rewrite_nonce == nonce
            {
                *enhanced_content = Some(markdown);
                *cached_lines = None;
            }
            vec![]
        }
        TaskResult::BundleStatusReady {
            has_cache,
            version,
            personas,
            roles,
            agents,
            skills,
            persona_details,
            role_details,
        } => {
            app.bundle_state.has_cache = has_cache;
            app.bundle_state.version = version.unwrap_or_default();
            app.bundle_state.personas = personas;
            app.bundle_state.roles = roles;
            app.bundle_state.agents = agents;
            app.bundle_state.skills = skills;
            app.bundle_state.persona_details = persona_details;
            app.bundle_state.role_details = role_details;
            vec![]
        }
        TaskResult::BundleStatusFailed { error } => {
            tracing::warn!(error = %error, "bundle status fetch failed");
            vec![]
        }
        TaskResult::CatalogEntryReady {
            kind,
            name,
            content,
        } => {
            if let ActiveView::Agent(id) = app.active_view
                && let Some(agent) = app.agents.get_mut(&id)
            {
                let title = format!("{kind}: {name}");
                agent.block_viewer = Some(
                    crate::views::block_viewer::BlockViewerPane::for_plain_text(&title, &content),
                );
            }
            vec![]
        }
        TaskResult::CatalogEntryFailed { error } => {
            tracing::warn!(error = %error, "catalog entry fetch failed");
            if let ActiveView::Agent(id) = app.active_view
                && let Some(agent) = app.agents.get_mut(&id)
            {
                agent
                    .scrollback
                    .push_block(RenderBlock::system(format!("Couldn't load entry: {error}")));
            }
            vec![]
        }
        TaskResult::BtwResponse {
            agent_id,
            result,
            minimal_request_id,
        } => handle_btw_response(app, agent_id, result, minimal_request_id),
        TaskResult::InterjectQueued { .. } => vec![],
        TaskResult::RecapRequested {
            session_id,
            auto,
            error,
        } => {
            if let Some(error) = error {
                tracing::debug!(%error, "recap request failed");
                if !auto
                    && let Some(agent) = find_agent_by_session_id(&mut app.agents, &session_id.0)
                    && let Some(pending_id) = agent.pending_recap_entry.take()
                {
                    agent.scrollback.remove_entry(pending_id);
                    agent.show_toast(super::recap_unavailable_toast(
                        super::scrollback_has_user_messages(&agent.scrollback),
                    ));
                }
            }
            vec![]
        }
        TaskResult::InterjectFailed {
            agent_id,
            error,
            text,
            blocks,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                let id = agent.session.next_queue_id;
                agent.session.next_queue_id += 1;
                agent
                    .session
                    .pending_prompts
                    .push_front(crate::app::agent::QueuedPrompt {
                        id,
                        text,
                        kind: crate::app::agent::QueueEntryKind::Prompt,
                        wire_blocks: blocks,
                        images: Vec::new(),
                        display_as_skill: false,
                        task_id: None,
                        human_schedule: None,
                        chip_elements: Vec::new(),
                        skill_token_ranges: Vec::new(),
                        combined_texts: Vec::new(),
                    });
                agent.show_toast(&format!("Interjection failed — requeued: {error}"));
            }
            vec![]
        }
        TaskResult::AvailableCommandsRefreshed { agent_id, commands } => {
            if !commands.is_empty()
                && let Some(agent) = app.agents.get_mut(&agent_id)
            {
                agent.session.available_commands = commands;
                agent.session.available_commands_generation += 1;
                super::super::acp_handler::refresh_workflow_run_capabilities(agent);
            }
            vec![]
        }
        TaskResult::AuthCopyFeedbackTimeout { generation } => {
            if generation == app.auth_clipboard_feedback_generation {
                app.auth_clipboard_delivery = None;
            }
            vec![]
        }
        TaskResult::PaywallCheckTick => {
            let timed_out = app
                .paywall_check_started
                .is_some_and(|t| t.elapsed() >= PAYWALL_AUTO_CHECK_TIMEOUT);
            if !app.has_access() && !timed_out {
                vec![
                    Effect::CheckSubscription { verify: None },
                    Effect::SchedulePaywallCheck,
                ]
            } else {
                vec![]
            }
        }
        TaskResult::CheckSubscriptionComplete { verify, meta } => {
            handle_check_subscription_complete(app, verify, meta)
        }
        TaskResult::GateVerifyTimeout { generation } => handle_gate_verify_timeout(app, generation),
        TaskResult::CreditLimitRecheckComplete { agent_id, meta } => {
            handle_credit_limit_recheck_complete(app, agent_id, meta)
        }
        TaskResult::LogoutComplete {
            xai_targets,
            codex_targets,
            codex_account,
        } => {
            app.startup_xai_ready = false;
            app.startup_xai_auth_meta = None;
            app.access_gate_shown_logged = false;
            app.announcement_cta_impressions_logged.clear();
            app.gate = None;
            app.pending_gate_verification = None;
            app.last_subscription_check_at = None;
            app.login_method_id = None;
            ensure_login_method(app);
            app.auth_clipboard_delivery = None;
            let active_agent_id = match app.active_view {
                ActiveView::Agent(id) => Some(id),
                _ => None,
            };
            let active_xai_removed = active_agent_id
                .is_some_and(|id| xai_targets.iter().any(|target| target.agent_id == id));
            let mut effects = xai_targets
                .iter()
                .filter_map(|target| target.session_id.clone())
                .map(|session_id| Effect::UnregisterActiveSession { session_id })
                .collect::<Vec<_>>();
            if active_xai_removed {
                let _ = dispatch_exit_session(app);
            }
            for target in &xai_targets {
                super::session::modal::remove_agent_and_cleanup(app, target.agent_id);
            }

            if let Some(account) = codex_account {
                // The fallback decision was validated before xAI's logout
                // broadcast, so models/update ordering cannot change it.
                app.startup_codex_account = Some(account);
                app.sync_usage_command_visibility();
                app.primary_provider = PrimaryProvider::Codex;
                app.clear_xai_access_controls();
                app.auth_state = AuthState::Done;
                let surviving_codex = codex_targets
                    .iter()
                    .map(|target| target.agent_id)
                    .find(|id| app.agents.contains_key(id));
                if active_xai_removed {
                    if let Some(id) = surviving_codex {
                        switch_to_agent(app, id, SwitchCause::Picker);
                    } else {
                        show_welcome(app);
                    }
                }
                app.welcome_prompt_focused = app.has_access();
                app.show_toast("xAI Grok disconnected. ChatGPT Codex remains connected.");
                return effects;
            }

            // Neither provider is authenticated. Any snapshotted Codex tabs
            // are invalid too; close them and return to the side-by-side
            // provider chooser rather than an xAI-only login state.
            app.startup_codex_account = None;
            app.sync_usage_command_visibility();
            app.codex_resume_auth_pending = false;
            let active_codex_removed = active_agent_id
                .is_some_and(|id| codex_targets.iter().any(|target| target.agent_id == id));
            if active_codex_removed && !active_xai_removed {
                let _ = dispatch_exit_session(app);
            }
            effects.extend(
                codex_targets
                    .iter()
                    .filter_map(|target| target.session_id.clone())
                    .map(|session_id| Effect::UnregisterActiveSession { session_id }),
            );
            for target in &codex_targets {
                super::session::modal::remove_agent_and_cleanup(app, target.agent_id);
            }
            app.primary_provider = PrimaryProvider::Xai;
            app.clear_xai_access_controls();
            app.auth_state = AuthState::ProviderChoice {
                error: Some(
                    "xAI Grok is disconnected. Sign in to ChatGPT Codex or xAI Grok to continue."
                        .to_string(),
                ),
            };
            show_welcome(app);
            app.welcome_menu_index = Some(0);
            app.welcome_prompt_focused = false;
            effects
        }
        TaskResult::CodexLoginComplete {
            agent_id,
            purpose,
            result,
            models,
        } => match purpose {
            crate::app::actions::CodexLoginPurpose::Independent => {
                if let Ok(account) = &result {
                    app.startup_codex_account = Some(account.clone());
                    app.sync_usage_command_visibility();
                }
                let effects = if result.is_ok()
                    && let Some(models) = models
                {
                    apply_codex_login_models(app, models)
                } else {
                    Vec::new()
                };
                let message = match result {
                    Ok(account) => {
                        let mut details = Vec::new();
                        if let Some(email) = account.email.filter(|value| !value.trim().is_empty())
                        {
                            details.push(email);
                        }
                        if let Some(plan) =
                            account.plan_type.filter(|value| !value.trim().is_empty())
                        {
                            details.push(format!("{plan} plan"));
                        }
                        if details.is_empty() {
                            "OpenAI Codex connected.".to_string()
                        } else {
                            format!("OpenAI Codex connected: {}.", details.join(" · "))
                        }
                    }
                    Err(error) => format!("OpenAI Codex login failed: {error}"),
                };
                push_codex_auth_result(app, agent_id, message);
                effects
            }
            crate::app::actions::CodexLoginPurpose::Startup { request_seq } => {
                handle_codex_startup_complete(app, request_seq, result, models)
            }
            crate::app::actions::CodexLoginPurpose::SessionResume { request_seq } => {
                handle_codex_session_resume_complete(app, request_seq, result, models)
            }
        },
        TaskResult::CodexLogoutComplete {
            agent_id,
            targets,
            primary_was_codex,
            result,
        } => {
            let logout_succeeded = result.is_ok();
            let message = match result {
                Ok(true) => "OpenAI Codex disconnected.".to_string(),
                Ok(false) => "OpenAI Codex was not connected.".to_string(),
                Err(error) => format!("OpenAI Codex logout failed: {error}"),
            };
            push_codex_auth_result(app, agent_id, message);
            if !logout_succeeded {
                return vec![];
            }

            app.startup_codex_account = None;
            app.sync_usage_command_visibility();
            app.codex_resume_auth_pending = false;
            if app.startup_provider_selection == Some(PrimaryProvider::Codex) {
                app.startup_provider_selection = None;
            }

            // Codex OAuth is process-global. Remove every Codex-backed tab,
            // not just the one that issued /logout, so dashboard/tab switching
            // cannot resurrect a promptable session with revoked credentials.
            let active_agent_id = match app.active_view {
                ActiveView::Agent(id) => Some(id),
                _ => None,
            };
            let mut codex_agent_ids = targets
                .iter()
                .map(|target| target.agent_id)
                .collect::<Vec<_>>();
            let discovered_codex_agent_ids = app
                .agents
                .iter()
                .filter_map(|(id, agent)| {
                    (PrimaryProvider::for_current_model(&agent.session.models)
                        == Some(PrimaryProvider::Codex))
                    .then_some(*id)
                })
                .collect::<Vec<_>>();
            for id in discovered_codex_agent_ids {
                if !codex_agent_ids.contains(&id) {
                    codex_agent_ids.push(id);
                }
            }
            // A legacy/incomplete model state may not carry provider metadata.
            // If Codex owns the foreground, the invoking/active tab is still
            // invalidated by the global credential removal.
            if primary_was_codex
                && let Some(id) = agent_id.or(active_agent_id)
                && !codex_agent_ids.contains(&id)
            {
                codex_agent_ids.push(id);
            }
            let mut effects = targets
                .iter()
                .filter_map(|target| {
                    target
                        .session_id
                        .clone()
                        .map(|session_id| Effect::UnregisterActiveSession { session_id })
                })
                .collect::<Vec<_>>();
            effects.extend(codex_agent_ids.iter().filter_map(|id| {
                if targets.iter().any(|target| target.agent_id == *id) {
                    return None;
                }
                app.agents
                    .get(id)
                    .and_then(|agent| agent.session.session_id.clone())
                    .map(|session_id| Effect::UnregisterActiveSession { session_id })
            }));
            let active_removed = active_agent_id.is_some_and(|id| codex_agent_ids.contains(&id));
            if active_removed {
                // Reuse the normal welcome cleanup, but registration teardown
                // is already emitted exactly once for every removed Codex tab.
                let _ = dispatch_exit_session(app);
            }
            for id in codex_agent_ids {
                super::session::modal::remove_agent_and_cleanup(app, id);
            }

            if !primary_was_codex {
                return effects;
            }

            if app.startup_xai_ready {
                effects
                    .extend(app.switch_primary_provider_with_effects(PrimaryProvider::Xai, None));
                // This is an already-authenticated provider fallback, not a
                // first-run login. Keep the startup marker clear so a later
                // ordinary xAI /login cannot re-enter startup model selection.
                app.startup_provider_selection = None;
                app.auth_state = AuthState::Done;
                app.welcome_prompt_focused = app.has_access();
                let surviving_xai = app.agents.iter().find_map(|(id, agent)| {
                    (PrimaryProvider::for_current_model(&agent.session.models)
                        == Some(PrimaryProvider::Xai))
                    .then_some(*id)
                });
                if let Some(id) = surviving_xai {
                    switch_to_agent(app, id, SwitchCause::Picker);
                } else {
                    show_welcome(app);
                }
                app.show_toast("ChatGPT Codex disconnected. xAI Grok remains connected.");
            } else {
                show_welcome(app);
                app.clear_xai_access_controls();
                app.auth_state = AuthState::ProviderChoice {
                    error: Some(
                        "ChatGPT Codex is disconnected. Sign in to ChatGPT Codex or xAI Grok to continue."
                            .to_string(),
                    ),
                };
                app.welcome_menu_index = Some(0);
                app.welcome_prompt_focused = false;
            }
            effects
        }
        TaskResult::DeepSearchResults { results, seq } => {
            handle_deep_search_results(app, results, seq)
        }
        TaskResult::RewindPointsLoaded { agent_id, points } => {
            handle_rewind_points_loaded(app, agent_id, points)
        }
        TaskResult::RewindPointsFailed { agent_id, error } => {
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return vec![];
            };
            agent.rewind_state = None;
            app.show_toast(&format!("Undo failed: {error}"));
            vec![]
        }
        TaskResult::RewindPreviewComplete {
            agent_id,
            response,
            target_prompt_index,
            mode,
        } => handle_rewind_preview_complete(app, agent_id, response, target_prompt_index, mode),
        TaskResult::RewindPreviewFailed { agent_id, error } => {
            handle_rewind_preview_failed(app, agent_id, error)
        }
        TaskResult::RewindExecuteComplete { agent_id, response } => {
            dispatch_rewind_success(app, agent_id, response)
        }
        TaskResult::RewindExecuteFailed { agent_id, error } => {
            handle_rewind_execute_failed(app, agent_id, error)
        }
        TaskResult::SuggestionDebounceExpired {
            agent_id,
            generation,
        } => handle_suggestion_debounce_expired(app, agent_id, generation),
        TaskResult::PluginCtaDebounceExpired {
            agent_id,
            generation,
        } => handle_plugin_cta_debounce_expired(app, agent_id, generation),
        TaskResult::ShellSuggestionsLoaded {
            agent_id,
            response,
            request_text,
            request_cursor,
        } => {
            let Some(agent) = app.agents.get_mut(&agent_id) else {
                return vec![];
            };
            if agent.prompt_input_mode != crate::app::agent_view::PromptInputMode::Bash {
                return vec![];
            }
            let generation = response.generation;
            agent
                .prompt
                .suggestions
                .on_suggestions_loaded(response, &request_text, request_cursor);
            let text = agent.prompt.text().to_owned();
            agent.prompt.suggestions.set_last_request_text(&text);
            let mark = agent.pending_effects.len();
            if agent.prompt.suggestions.take_pending_tab(generation) {
                agent.shell_completion_tab();
            }
            agent.pending_effects.split_off(mark)
        }
        TaskResult::PromptSuggestionLoaded {
            agent_id,
            suggestion,
            generation,
        } => {
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent
                    .prompt
                    .prompt_suggestion
                    .on_loaded(suggestion, generation);
                agent.refresh_prompt_suggestion_gate();
                agent.log_prompt_suggestion_shown_if_visible();
            }
            vec![]
        }
        TaskResult::SettingPersisted { key, value } => {
            tracing::trace!(target: "settings", ?key, ?value, "setting persisted");
            vec![]
        }
        TaskResult::SettingPersistFailed {
            key,
            rollback_value,
            error,
        } => {
            let rollback_effects = apply_setting_rollback(app, key, &rollback_value);
            tracing::warn!(target: "settings", ?key, ?rollback_value, %error, "setting persist failed; rolled back");
            let scrubbed = scrub_error_for_toast(&error);
            app.show_toast(&format!("\u{2717} Could not save {key}: {scrubbed}"));
            rollback_effects
        }
        TaskResult::SettingPersistFailedBestEffort { key, error } => {
            tracing::warn!(
                target: "settings",
                ?key, %error,
                "setting persist failed (best-effort); in-memory state stays at optimistic value",
            );
            let scrubbed = scrub_error_for_toast(&error);
            app.show_toast(&format!("\u{2717} Could not save {key}: {scrubbed}"));
            vec![]
        }
    }
}
