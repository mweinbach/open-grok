//! Dashboard dispatchers: attach, overlays, rows, renames, and permissions.

use super::ctx::{
    show_welcome, surface_yolo_launch_block_notice, sync_active_permission_mode_mirror,
};
use super::dashboard_telemetry::{
    log_dashboard_attached, log_dashboard_closed, log_dashboard_launched, log_dashboard_opened,
};
use super::modes::{dispatch_cycle_mode_and_sync, set_yolo_mode, yolo_enable_blocked};
use super::permissions::resolve_permission_queue_transition;
use super::queue::{maybe_drain_queue, note_peek_page_flip};
use super::router::dispatch;
use super::session::lifecycle::{
    dispatch_new_session_inner_with_id, dispatch_new_worktree_session,
};
use super::session::load::dispatch_load_session;
use super::session::load::focus_if_session_already_open;
use super::session::modal::dispatch_sessions_confirm_close;
use super::turn::dispatch_cancel_turn;
use super::voice::{merge_prompt_with_voice_interim, voice_stop_on_submit};
use crate::app::actions::{Action, Effect, PermissionModeKind};
use crate::app::agent::{AgentId, DeferredModelSwitch};
use crate::app::agent_view::AgentView;
use crate::app::app_view::{ActiveView, AppView, DashboardReturn, TrustState};
use crate::app::cancel_latency::CancelOrigin;
use agent_client_protocol as acp;
use xai_grok_telemetry::events::CancellationScope;

// ---------------------------------------------------------------------------
// Agent Dashboard dispatchers
// ---------------------------------------------------------------------------

/// Build a `DashboardState` from the persisted layout (pins, reorder, grouping), loading and caching `app.dashboard_persisted` on first use.
/// Callers use it both to build the real dashboard and to compute the cycle order before the dashboard has been opened.
fn dashboard_state_from_persisted(app: &mut AppView) -> crate::views::dashboard::DashboardState {
    use crate::views::dashboard::{DashboardState, load_persisted};
    if app.dashboard_persisted.is_none() {
        app.dashboard_persisted = load_persisted();
    }
    let persisted = app
        .dashboard_persisted
        .clone()
        .unwrap_or_else(crate::views::dashboard::PersistedDashboard::defaults);
    let resolver = crate::views::dashboard::SessionIdResolver::from_agents(&app.agents);
    DashboardState::from_persisted(&persisted, &resolver)
}

pub(super) fn ensure_dashboard_state(app: &mut AppView) {
    if app.dashboard.is_some() {
        return;
    }
    let mut state = dashboard_state_from_persisted(app);
    state.gc_stale_refs(&dashboard_alive_fn(&app.agents));
    state.adopt_slash_mru(app.slash_mru.clone());
    state.adopt_command_tags(app.command_tags.clone());
    state.set_screen_mode(app.screen_mode);
    state.set_recap_visible(app.session_recap_available);
    state.set_voice_visible(app.voice_mode_enabled);
    state.set_restricted_commands(&app.tier_restricted_commands);
    let billing = app.usage_visible;
    state
        .dispatch
        .slash_controller
        .set_billing_surface_visible(billing);
    state
        .peek_reply
        .slash_controller
        .set_billing_surface_visible(billing);
    app.dashboard = Some(state);
}

/// Configure the dashboard for display: snapshot app-wide state (cwd, models, plugins, permission mode) and clear the staged dispatch settings.
/// Shared by `dispatch_open_dashboard` and the overlay-cycle path; a no-op when the dashboard is unallocated.
fn configure_dashboard_state(app: &mut AppView) {
    clear_overlay_login_or_secret_modal(app);
    let bootstrap_commands = app.bootstrap_acp_commands.clone();
    let models = app.models.clone();
    let disable_plugins = app.appearance.disable_plugins;
    let default_yolo = app.default_yolo;
    let default_auto = app.auto_mode_gate
        && !default_yolo
        && app.current_ui.permission_mode.as_deref() == Some("auto");
    let cwd = app.cwd.clone();
    let cwd_has_git_ancestor = app.cwd_has_git_ancestor;
    let has_agents = !app.agents.is_empty();
    if let Some(d) = app.dashboard.as_mut() {
        d.close_popup();
        // Provider credential drafts are ephemeral. Dropping the Settings
        // state zeroizes any partially entered key before a dashboard reopen.
        d.settings_modal = None;
        d.location_picker = None;
        d.cwd = cwd.clone();
        d.cwd_has_git_ancestor = cwd_has_git_ancestor;
        d.dispatch_worktree = false;
        d.worktree_dialog = None;
        d.pending_worktree_prompt = None;
        d.pending_worktree_attach = false;
        d.focus_new_agent_button();
        d.list_focused = has_agents;
        d.dispatch.file_search.retarget(&cwd);
        // Tool gating disabled (None): the dashboard has no agent toolset.
        d.dispatch
            .slash_controller
            .registry_mut()
            .set_plugins_visible(!disable_plugins);
        d.dispatch
            .sync_acp_commands(&bootstrap_commands, None, &models);
        d.models = models;
        d.pending_model = None;
        d.pending_mode = if default_yolo {
            crate::views::dashboard::DashboardDispatchMode::AlwaysApprove
        } else if default_auto {
            crate::views::dashboard::DashboardDispatchMode::Auto
        } else {
            crate::views::dashboard::DashboardDispatchMode::Normal
        };
    }
}

/// Open the dashboard view. Respects the [`crate::views::dashboard::dashboard_enabled`] feature flag (env var override and persisted setting).
/// The dashboard is independent of leader mode: it renders local sessions from `app.agents`.
/// When connected via a leader it also polls the leader roster (see the roster-poll gate in the event loop).
pub(super) fn dispatch_open_dashboard(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::dashboard_enabled;

    if !dashboard_enabled() {
        app.show_toast("Agent dashboard is disabled in this configuration");
        return vec![];
    }

    if !matches!(app.auth_state, crate::app::app_view::AuthState::Done) {
        app.show_toast("Sign in to open the dashboard");
        return vec![];
    }
    // Same rationale for folder trust: opening the dashboard would visually
    // dismiss the trust question with the folder still unanswered. Toast and
    // stay put (mirrors the auth gate above) so the question is resolved first.
    if matches!(app.trust_state, TrustState::Pending { .. }) {
        app.show_toast("Answer the folder-trust question to open the dashboard");
        return vec![];
    }

    if matches!(app.active_view, ActiveView::AgentDashboard) {
        return dispatch_exit_dashboard(app);
    }
    // Stamp return target for this visit (clears any prior leftover).
    app.dashboard_return = match app.active_view {
        ActiveView::Agent(id) => Some(DashboardReturn::Agent(id)),
        _ => None,
    };

    if app.dashboard.is_none() {
        ensure_dashboard_state(app);
    } else if let Some(d) = app.dashboard.as_mut() {
        d.gc_stale_refs(&dashboard_alive_fn(&app.agents));
        d.set_recap_visible(app.session_recap_available);
        d.set_voice_visible(app.voice_mode_enabled);
        d.set_restricted_commands(&app.tier_restricted_commands);
    }

    let agent_cwds: Vec<(AgentId, std::path::PathBuf)> = app
        .agents
        .iter()
        .map(|(id, a)| (*id, a.session.cwd.clone()))
        .collect();
    for (id, cwd) in agent_cwds {
        if let Some(info) = crate::git_info::compute_cwd_git_info(&cwd)
            && let Some(agent) = app.agents.get_mut(&id)
        {
            agent.current_branch = info.branch;
            agent.is_worktree = info.is_worktree || agent.session.is_worktree;
            agent.main_repo = info.main_repo;
            agent.worktree_label = info.worktree_label;
        }
    }

    //

    configure_dashboard_state(app);
    app.active_view = ActiveView::AgentDashboard;
    log_dashboard_opened(app);
    if app.workspace_dashboard_enabled {
        app.dashboard_sessions_loading = app.workspace_snapshot.is_none();
        crate::app::workspace_sync::request(app);
        if app.workspace_store.is_some()
            || app.workspace_store_loading
            || app.workspace_write_in_flight
        {
            return vec![];
        }
        app.workspace_store_loading = true;
        let db_path = xai_grok_dashboard_store::default_db_path(&xai_grok_config::grok_home());
        return vec![Effect::LoadWorkspaceSnapshot { db_path }];
    }
    app.dashboard_sessions_loading = true;
    if app.leader_mode {
        return vec![Effect::FetchRoster];
    }
    vec![Effect::FetchDashboardSessions]
}

/// Produce a closure that answers "does this DashboardRowId still exist in `agents`?".
/// A static lifetime is not possible because the closure borrows, so callers pass `&app.agents`.
fn dashboard_alive_fn(
    agents: &indexmap::IndexMap<AgentId, AgentView>,
) -> impl Fn(&crate::views::dashboard::DashboardRowId) -> bool + '_ {
    move |id| match id {
        crate::views::dashboard::DashboardRowId::TopLevel(a) => agents.contains_key(a),
        crate::views::dashboard::DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => agents
            .get(parent)
            .is_some_and(|a| a.subagent_sessions.contains_key(child_session_id)),

        crate::views::dashboard::DashboardRowId::Roster { .. }
        | crate::views::dashboard::DashboardRowId::Workspace { .. } => false,
    }
}

pub(super) fn dispatch_exit_dashboard(app: &mut AppView) -> Vec<Effect> {
    clear_overlay_login_or_secret_modal(app);
    // Also clear any popup attachment so a fresh
    // reopen lands on the row list, not on a stale popup
    // (`close_popup()` atomically clears the hit
    // rects too.)
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.close_popup();
        // Do not retain a partially typed provider credential in the
        // in-memory dashboard state after the user leaves the surface.
        d.settings_modal = None;
        // Dashboard state is preserved across reopen; clear a leftover exit
        // alias so the next Enter does not quit.
        if crate::slash::commands::exit::is_exit_alias(d.dispatch.text()) {
            d.dispatch.set_text("");
            d.error_toast = None;
        }
    }
    log_dashboard_closed(app);
    let preferred = app
        .dashboard_return
        .take()
        .filter(|t| app.agents.contains_key(&t.agent_id()));

    let (return_id, rearm_overlay) = match preferred {
        Some(t) => (Some(t.agent_id()), t.is_overlay()),
        None => (app.agents.keys().next().copied(), false),
    };
    let effects = if let Some(id) = return_id {
        app.active_view = ActiveView::Agent(id);
        if rearm_overlay {
            rearm_session_overlay(app, id);
        }
        let effects = app.sync_primary_provider_from_active_agent();
        surface_yolo_launch_block_notice(app, id);
        effects
    } else {
        show_welcome(app);
        Vec::new()
    };
    effects
}

/// Restore session-overlay chrome (`attached_agent` and the row cursor).
/// Keeps a live subagent takeover; otherwise clears it and selects TopLevel.
fn rearm_session_overlay(app: &mut AppView, id: AgentId) {
    use crate::views::dashboard::DashboardRowId;
    let live_child = app.agents.get(&id).and_then(|a| {
        a.active_subagent
            .as_ref()
            .filter(|c| a.subagent_sessions.contains_key(*c))
            .cloned()
    });
    let row = match live_child {
        Some(child_session_id) => DashboardRowId::Subagent {
            parent: id,
            child_session_id,
        },
        None => {
            if let Some(agent) = app.agents.get_mut(&id) {
                agent.active_subagent = None;
            }
            DashboardRowId::TopLevel(id)
        }
    };
    if let Some(d) = app.dashboard.as_mut() {
        d.focus_row(row);
        d.attached_agent = Some(id);
    }
}

pub(super) fn dispatch_dashboard_attach(
    app: &mut AppView,
    id: crate::views::dashboard::DashboardRowId,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    //

    clear_pending_overlay_stop(app);
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
    }
    let mut provider_effects = Vec::new();
    match id {
        DashboardRowId::TopLevel(agent_id) => {
            if !app.agents.contains_key(&agent_id) {
                if let Some(d) = app.dashboard.as_mut() {
                    d.set_error_toast("Session no longer exists");
                }
                return vec![];
            }
            if let Some(agent) = app.agents.get_mut(&agent_id) {
                agent.close_subagent_fullscreen();
            }
            if let Some(d) = app.dashboard.as_mut() {
                d.focus_row(DashboardRowId::TopLevel(agent_id));

                d.attached_agent = Some(agent_id);
            }
            app.active_view = ActiveView::Agent(agent_id);
            provider_effects.extend(app.sync_primary_provider_from_active_agent());
            log_dashboard_attached(&DashboardRowId::TopLevel(agent_id));
            surface_yolo_launch_block_notice(app, agent_id);
        }
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => {
            let alive = app
                .agents
                .get(&parent)
                .is_some_and(|a| a.subagent_sessions.contains_key(&child_session_id));
            if !alive {
                if let Some(d) = app.dashboard.as_mut() {
                    d.set_error_toast("Subagent no longer running");
                }
                return vec![];
            }
            if let Some(agent) = app.agents.get_mut(&parent) {
                agent.open_subagent_fullscreen(child_session_id.clone());
            }
            let row_id = DashboardRowId::Subagent {
                parent,
                child_session_id,
            };
            if let Some(d) = app.dashboard.as_mut() {
                d.focus_row(row_id.clone());
                d.attached_agent = Some(parent);
            }
            app.active_view = ActiveView::Agent(parent);
            provider_effects.extend(app.sync_primary_provider_from_active_agent());
            log_dashboard_attached(&row_id);
            surface_yolo_launch_block_notice(app, parent);
        }
        DashboardRowId::Roster { session_id } => {
            let (session_cwd, conversation_entry) = app
                .leader_roster
                .iter()
                .chain(app.dashboard_local_sessions.iter())
                .find(|e| e.session_id == session_id)
                .map(|e| {
                    let is_conversation = e.origin.kind == "conversation";
                    (
                        // Conversation rows have no cwd to re-home into.
                        (!is_conversation).then(|| std::path::PathBuf::from(&e.cwd)),
                        is_conversation,
                    )
                })
                .unwrap_or((None, false));

            // Already local (e.g. double-click after the row converted): focus only.
            if let Some(existing_id) =
                focus_if_session_already_open(app, session_id.as_str(), conversation_entry)
            {
                log_dashboard_attached(&DashboardRowId::TopLevel(existing_id));
                return vec![];
            }

            let effects = dispatch_load_session(app, session_id, session_cwd, conversation_entry);
            if let Some(new_id) = effects.iter().find_map(|e| match e {
                Effect::LoadSession { agent_id, .. } => Some(*agent_id),
                _ => None,
            }) {
                if let Some(d) = app.dashboard.as_mut() {
                    d.focus_row(DashboardRowId::TopLevel(new_id));
                    d.attached_agent = Some(new_id);
                }
                log_dashboard_attached(&DashboardRowId::TopLevel(new_id));
            }
            return effects;
        }
        DashboardRowId::Workspace { .. } => return vec![],
    }
    provider_effects
}

/// Exit the dashboard's session-overlay: dismiss the bordered chrome and return to the dashboard view.
pub(super) fn dispatch_dashboard_overlay_exit(app: &mut AppView) -> Vec<Effect> {
    clear_overlay_login_or_secret_modal(app);
    // Capture before close_popup() clears attached_agent.
    if let ActiveView::Agent(id) = app.active_view {
        app.dashboard_return = Some(DashboardReturn::Overlay(id));
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.close_popup();
    }

    clear_pending_overlay_stop(app);
    app.active_view = ActiveView::AgentDashboard;
    vec![]
}

/// Drop provider-login UI before a dashboard transition can hide its owning
/// agent. Secret buffers zeroize on drop; the picker is also cleared so it
/// cannot reappear against a different overlay context.
fn clear_overlay_login_or_secret_modal(app: &mut AppView) {
    let agent_id = match app.active_view {
        ActiveView::Agent(id) => Some(id),
        ActiveView::AgentDashboard => app
            .dashboard
            .as_ref()
            .and_then(|dashboard| dashboard.attached_agent),
        ActiveView::Welcome => None,
    };
    if let Some(agent_id) = agent_id
        && let Some(agent) = app.agents.get_mut(&agent_id)
    {
        while agent.close_login_or_secret_modal() {}
    }
}

/// Disarm a pending overlay stop-confirm (see
/// [`dispatch_dashboard_overlay_stop`]). Called from every overlay
/// navigation that can happen WITHOUT a key press (mouse clicks on
/// `[Dashboard]` / `[‹]` / `[›]`); key presses already disarm via the
/// pending-action fast path in `AppView::handle_input`.
fn clear_pending_overlay_stop(app: &mut AppView) {
    if app
        .pending_action
        .as_ref()
        .is_some_and(|p| matches!(p.action, Action::DashboardOverlayStop))
    {
        app.pending_action = None;
    }
}

/// Confirmed stop from inside the dashboard's session-overlay: the second Ctrl+X press within the confirm window.
/// Canonical state machine for overlay Ctrl+X (the intercept in `app_view::handle_input` and the `DashboardOverlayStop` def both point here):
///
/// - First press while stoppable work runs (turn, `/compact`, streaming wake turn; `arm_dashboard_stop`) becomes `Action::CancelTurn`.
///   That is the agent view's Ctrl+C behaviour (keep-subagents prompt, prompt rewind).
///   It never arms, so mashing Ctrl+X to stop a turn can't close the session.
/// - First press in any other state (idle, cancel pending) arms `AppView::pending_action` with the dashboard's 2s `CONFIRM_WINDOW`.
///   The shortcuts bar paints "press Ctrl+x again to close this session".
///   Cancel can't help here: `dispatch_cancel_turn` no-ops without a running turn, and command cancellation is a `CancelTurn` TODO.
///   So the two-press close is the only termination the user can reach, matching the dashboard list's Ctrl+X which arms even while busy.
/// - Second press inside the window lands here via the pending-action fast path; any other key disarms via that same path.
///   A turn started inside the window (queued prompt drained, user sent one) downgrades the press to a cancel instead of closing work in flight.
///
/// Mirrors `dispatch_dashboard_stop`'s second press, except the user is INSIDE the session being closed.
/// The view returns to the dashboard instead of falling back to another agent.
pub(super) fn dispatch_dashboard_overlay_stop(app: &mut AppView) -> Vec<Effect> {
    let Some(id) = app.dashboard.as_ref().and_then(|d| d.attached_agent) else {
        return vec![];
    };
    // Confirmed press: cancel any stoppable activity, including `/compact`
    // (first-press overlay Ctrl+X intentionally arms instead — see
    // `arm_dashboard_stop`).
    if let Some(agent) = app.agents.get_mut(&id)
        && agent.stoppable_activity_running()
    {
        agent.cancel_trigger_hint = Some(crate::app::actions::CancelTrigger::DashboardStop);
        return dispatch_cancel_turn(app);
    }

    let neighbor =
        dashboard_neighbor_row(app, &crate::views::dashboard::DashboardRowId::TopLevel(id));
    if let Some(d) = app.dashboard.as_mut() {
        d.close_popup();
    }
    app.active_view = ActiveView::AgentDashboard;
    let effects = dispatch_sessions_confirm_close(app, id);
    if !app.agents.contains_key(&id)
        && let Some(d) = app.dashboard.as_mut()
    {
        match neighbor {
            Some(n) => d.focus_row(n),
            None => d.focus_new_agent_button(),
        }

        // Don't clobber a refusal toast planted by the close path above.
        if d.error_toast.is_none() {
            d.error_toast = Some(format!("{} Session closed", crate::glyphs::check_mark()));
        }
    }
    effects
}

/// Toggle worktree-dispatch mode for the dashboard (bound to Ctrl+W).
///
/// When the mode is on, the next dispatch spawns the agent in a fresh git worktree and the `[+ New Agent]` button reads `[+ New Worktree]`.
/// Worktrees require a git repo, so outside one the toggle no-ops with a toast and never leaves the dashboard in worktree mode.
pub(super) fn dispatch_dashboard_toggle_worktree(app: &mut AppView) -> Vec<Effect> {
    let has_git = app.cwd_has_git_ancestor;
    if let Some(d) = app.dashboard.as_mut() {
        if has_git {
            d.dispatch_worktree = !d.dispatch_worktree;
        } else {
            d.dispatch_worktree = false;
            d.set_error_toast("Not a git repository: worktrees need one");
        }
    }
    vec![]
}

/// Toggle auto-approve (YOLO mode) on the selected dashboard row's owning agent.
/// Subagents inherit their parent's mode, so a subagent selection routes to the parent.
///
/// Reuses `set_yolo_mode` (which reads `active_view` to target the agent) by temporarily switching the active view to the selected agent.
/// This keeps the drain / persist / toast logic in a single code path instead of duplicating it.
pub(super) fn dispatch_dashboard_toggle_auto_approve(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let Some(d) = app.dashboard.as_ref() else {
        return vec![];
    };
    let Some(selected) = d.selected.as_ref() else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Select a session first");
        }
        return vec![];
    };
    let agent_id = match selected {
        DashboardRowId::TopLevel(id) => *id,
        DashboardRowId::Subagent { parent, .. } => *parent,
        DashboardRowId::Roster { .. } | DashboardRowId::Workspace { .. } => return vec![],
    };
    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }
    let agent = app.agents.get(&agent_id).expect("checked above");
    let new = !agent.session.yolo_mode;

    if let Some(warning) = yolo_enable_blocked(app, new) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast(warning);
        }
        return vec![];
    }

    let saved_view = app.active_view;
    app.active_view = ActiveView::Agent(agent_id);
    let effects = set_yolo_mode(app, new);
    app.active_view = saved_view;
    effects
}

fn snapshot_prompt_widget(
    prompt: &mut crate::views::prompt_widget::PromptWidget,
    text: String,
) -> crate::views::prompt_widget::StashedPrompt {
    if prompt.text() == text
        || !prompt.images.is_empty()
        || !prompt.textarea().elements().is_empty()
    {
        prompt.stash().with_transformed_text(text)
    } else {
        crate::views::prompt_widget::StashedPrompt::from_submission(text, Vec::new(), Vec::new())
    }
}

/// Open the worktree-label dialog and stash the dispatch prompt until confirm.
fn open_dashboard_worktree_dialog(
    app: &mut AppView,
    prompt: Option<String>,
    attach: bool,
) -> Vec<Effect> {
    if let Some(d) = app.dashboard.as_mut() {
        d.pending_worktree_prompt =
            prompt.map(|text| snapshot_prompt_widget(&mut d.dispatch, text));
        d.pending_worktree_attach = attach;
        d.worktree_dialog = Some(crate::app::app_view::NewWorktreeDialogState::new());
        d.dispatch.set_text("");
        d.error_toast = None;
    }
    vec![]
}

fn resolve_pending_dispatch_mode(
    app: &mut AppView,
) -> (
    crate::views::dashboard::DashboardDispatchMode,
    Option<&'static str>,
) {
    use crate::views::dashboard::DashboardDispatchMode;

    let staged = app
        .dashboard
        .as_ref()
        .map(|dashboard| dashboard.pending_mode)
        .unwrap_or_default();
    let (resolved, warning) = match staged {
        DashboardDispatchMode::Auto if !app.auto_mode_gate => (DashboardDispatchMode::Normal, None),
        DashboardDispatchMode::AlwaysApprove if app.yolo_policy_block.is_some() => {
            (DashboardDispatchMode::Normal, app.yolo_policy_block)
        }
        _ => (staged, None),
    };
    if resolved != staged
        && let Some(dashboard) = app.dashboard.as_mut()
    {
        dashboard.pending_mode = resolved;
    }
    (resolved, warning)
}

fn permission_mode_for_dispatch(
    mode: crate::views::dashboard::DashboardDispatchMode,
) -> PermissionModeKind {
    use crate::views::dashboard::DashboardDispatchMode;
    match mode {
        DashboardDispatchMode::Normal | DashboardDispatchMode::Plan => PermissionModeKind::Ask,
        DashboardDispatchMode::Auto => PermissionModeKind::Auto,
        DashboardDispatchMode::AlwaysApprove => PermissionModeKind::AlwaysApprove,
    }
}

fn set_create_permission_mode(
    effects: &mut [Effect],
    mode: crate::views::dashboard::DashboardDispatchMode,
) {
    let mode = Some(permission_mode_for_dispatch(mode));
    for effect in effects {
        match effect {
            Effect::CreateSession {
                permission_mode_override,
                ..
            }
            | Effect::CreateWorktreeSession {
                permission_mode_override,
                ..
            } => *permission_mode_override = mode,
            _ => {}
        }
    }
}

/// Create a new session AND switch into its detail view.
/// Routed from the `[+ New Agent]` button, or Enter on an empty prompt while the button is focused.
/// Mirrors `dispatch_dashboard_dispatch`'s new-session arm with `attach=true`, minus the prompt enqueue.
pub(super) fn dispatch_dashboard_create_new_agent_with_detail(app: &mut AppView) -> Vec<Effect> {
    // Creating/switching consumes the dispatch surface — stop voice and drop the
    // target so a late final can't refill the box after the view switch.
    let _ = voice_stop_on_submit(app);

    if app.cwd_has_git_ancestor && app.dashboard.as_ref().is_some_and(|d| d.dispatch_worktree) {
        return open_dashboard_worktree_dialog(app, None, /* attach */ true);
    }
    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    let (pending_mode, policy_block) = resolve_pending_dispatch_mode(app);
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    log_dashboard_launched("new_agent_button");
    let (new_id, mut effects) = dispatch_new_session_inner_with_id(app, model_id);
    set_create_permission_mode(&mut effects, pending_mode);
    if let Some(agent) = app.agents.get_mut(&new_id) {
        apply_pending_dispatch_config(agent, pending_model.as_ref(), pending_mode, policy_block);
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);

        d.dispatch.set_text("");
        d.error_toast = None;
        d.filter = crate::views::dashboard::Filter::None;

        d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
        d.attached_agent = Some(new_id);
    }
    app.active_view = ActiveView::Agent(new_id);
    sync_active_permission_mode_mirror(app);
    surface_yolo_launch_block_notice(app, new_id);
    effects
}

/// Open the dashboard's shortcuts cheatsheet modal.
///
/// Builds the entry list from the registry, scoped to the `DashboardFocused` and `Always` contexts.
/// Mirrors `ActionId::ShortcutsHelp`'s agent-view handler.
pub(super) fn dispatch_dashboard_open_shortcuts_help(app: &mut AppView) {
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    if d.shortcuts_modal.is_some() {
        return;
    }
    use crate::actions::When;
    let contexts = [When::DashboardFocused, When::Always];
    let entries = crate::views::shortcuts_help::build_entries(
        &contexts,
        &app.registry,
        /* vim_mode */ false,
    );
    let state = crate::views::shortcuts_help::build_initial_picker_state(&entries);
    d.shortcuts_modal = Some(Box::new(crate::views::dashboard::ShortcutsModalState {
        entries,
        state,
        window: Default::default(),
        filter_active: false,
        collapsed_sections: crate::views::shortcuts_help::default_collapsed(),
        expanded_ids: std::collections::HashSet::new(),
        mode: crate::views::shortcuts_help::ShortcutsHelpMode::Browse,
    }));
}

/// Short display label for a directory in the location picker: the basename (truncated), or `~` for the home directory itself.
fn location_picker_label(path: &std::path::Path) -> String {
    if xai_dirs::home_dir().is_some_and(|h| h == path) {
        return "~".to_string();
    }
    let raw = path.file_name().and_then(|n| n.to_str()).unwrap_or("/");
    crate::render::line_utils::truncate_str(raw, 30)
}

/// Resolve a raw location-picker / `/cd` path string to an absolute path, expanding a leading `~` and joining relative paths against `cwd`.
/// Returns `None` for empty input or when `~` can't be expanded. The caller validates that the result is a directory.
pub(super) fn resolve_location_input(
    input: &str,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded: std::path::PathBuf = if trimmed == "~" {
        xai_dirs::home_dir()?
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        xai_dirs::home_dir()?.join(rest)
    } else {
        std::path::PathBuf::from(trimmed)
    };
    if expanded.is_absolute() {
        Some(expanded)
    } else {
        Some(cwd.join(expanded))
    }
}

/// Open the dashboard's location picker.
/// Seeds the candidate list with the current cwd (marked `(current)`) followed by recent project directories from session history.
/// Idempotent: a no-op if the picker is already open or the dashboard isn't active.
pub(super) fn dispatch_dashboard_open_location_picker(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::{LocationCandidate, LocationPickerState};

    if !matches!(app.active_view, ActiveView::AgentDashboard) {
        app.show_toast("Open the dashboard (/dashboard) to change location");
        return vec![];
    }

    if app
        .dashboard
        .as_ref()
        .is_some_and(|d| d.location_picker.is_some())
    {
        return vec![];
    }

    let cwd = app.cwd.clone();
    // The recent-dirs source is async; block the current runtime thread briefly to collect it.
    let recent = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(crate::recent_dirs::collect_recent_dirs(10))
    });

    let worktrees = crate::git_info::worktree_label_index();
    let worktree_label = |path: &std::path::Path| -> Option<String> {
        let key = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        worktrees.get(&key).cloned()
    };

    let mut candidates: Vec<LocationCandidate> = Vec::new();
    candidates.push(LocationCandidate {
        label: location_picker_label(&cwd),
        detail: format!("{}  (current)", crate::recent_dirs::display_path(&cwd)),
        worktree: worktree_label(&cwd),
        path: cwd.clone(),
    });
    for (path, ts) in recent.into_iter().filter(|(p, _)| p != &cwd) {
        let detail = format!(
            "{}  ({})",
            crate::recent_dirs::display_path(&path),
            crate::views::session_title::format_relative_time(
                (chrono::Utc::now() - ts).to_std().unwrap_or_default()
            ),
        );
        candidates.push(LocationCandidate {
            label: location_picker_label(&path),
            detail,
            worktree: worktree_label(&path),
            path,
        });
    }

    if let Some(d) = app.dashboard.as_mut() {
        let mut lp = LocationPickerState::new(candidates, cwd, worktrees);

        lp.worktree_mode = d.dispatch_worktree;
        d.location_picker = Some(lp);
    }
    crate::unified_log::info("dashboard.location_picker.opened", None, None);
    vec![]
}

/// Apply a location-picker / `/cd` selection.
/// Resolves and validates the path; on success updates `app.cwd` and the process cwd (so newly dispatched sessions spawn there) and closes the modal.
/// On failure the modal stays open with an inline error and the cwd is unchanged.
pub(super) fn dispatch_dashboard_change_location(app: &mut AppView, input: String) -> Vec<Effect> {
    // `app.dashboard` stays `Some` for the rest of the session once opened.
    if !matches!(app.active_view, ActiveView::AgentDashboard) {
        app.show_toast("Open the dashboard (/dashboard) to change location");
        return vec![];
    }
    let path = match resolve_location_input(&input, &app.cwd).filter(|p| p.is_dir()) {
        Some(p) => p,
        None => {
            if let Some(lp) = app
                .dashboard
                .as_mut()
                .and_then(|d| d.location_picker.as_mut())
            {
                lp.error = Some(format!("Not a directory: {}", input.trim()));
            } else if let Some(d) = app.dashboard.as_mut() {
                d.set_error_toast(&format!("Not a directory: {}", input.trim()));
            }
            return vec![];
        }
    };

    crate::unified_log::info(
        "dashboard.location_picker.changed",
        None,
        Some(serde_json::json!({ "path": path.display().to_string() })),
    );

    let changed = app.cwd != path;
    let display = crate::recent_dirs::display_path(&path);
    app.cwd = path.clone();

    app.cwd_has_git_ancestor = path.ancestors().any(|p| p.join(".git").exists());

    crate::git_info::populate_from_cwd_async(path.clone());

    let has_git = app.cwd_has_git_ancestor;
    if let Some(d) = app.dashboard.as_mut() {
        d.cwd = path.clone();
        d.cwd_has_git_ancestor = has_git;

        if let Some(wt) = d.location_picker.as_ref().map(|lp| lp.worktree_mode) {
            d.dispatch_worktree = wt && has_git;
        } else if !has_git {
            d.dispatch_worktree = false;
        }
        d.location_picker = None;
        if changed {
            d.dispatch.file_search.retarget(&path);
            d.error_toast = Some(format!("\u{2192} {display}"));
        }
    }

    vec![Effect::SetWorkingDir { path }]
}

/// Confirm the dashboard worktree-label dialog: create the agent in a fresh worktree at `app.cwd`, replaying any prompt stashed at dialog open.
/// The dialog itself was already cleared by the input handler.
/// Shows a dashboard toast (instead of creating) when the cwd isn't a git repository.
pub(super) fn dispatch_dashboard_confirm_worktree(
    app: &mut AppView,
    label: Option<String>,
) -> Vec<Effect> {
    // Apply the prompt, attach choice, and staged model/mode together.

    let (mut prompt, attach) = match app.dashboard.as_mut() {
        Some(d) => (
            d.pending_worktree_prompt.take(),
            std::mem::replace(&mut d.pending_worktree_attach, false),
        ),
        None => (None, false),
    };
    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    if !app.cwd_has_git_ancestor {
        if let Some(d) = app.dashboard.as_mut() {
            // Restore the typed prompt if the cwd stopped being a repo.
            if let Some(p) = prompt {
                d.dispatch.restore(p);
            }
            d.set_error_toast("Not a git repository: can't create a worktree here");
        }
        return vec![];
    }
    let (pending_mode, policy_block) = resolve_pending_dispatch_mode(app);
    let (prompt_text, mut images, chip_elements) = if let Some(stashed) = prompt.take() {
        let (text, images, chip_elements) = stashed.into_submission();
        (Some(text), images, chip_elements)
    } else {
        (None, Vec::new(), Vec::new())
    };
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    let mut effects =
        dispatch_new_worktree_session(app, None, label, prompt_text, model_id, None, None);
    set_create_permission_mode(&mut effects, pending_mode);
    if let Some(new_id) = effects.iter().find_map(|e| match e {
        Effect::CreateWorktreeSession { agent_id, .. } => Some(*agent_id),
        _ => None,
    }) {
        if let Some(agent) = app.agents.get_mut(&new_id) {
            apply_pending_dispatch_config(
                agent,
                pending_model.as_ref(),
                pending_mode,
                policy_block,
            );
            if let Some(entry) = agent.session.pending_prompts.back_mut() {
                entry.images = std::mem::take(&mut images);
                entry.chip_elements = chip_elements;
            }
        }
        if attach {
            if let Some(d) = app.dashboard.as_mut() {
                d.restore_peek_viewport(&mut app.agents);
                d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
                d.attached_agent = Some(new_id);
            }
            sync_active_permission_mode_mirror(app);
        } else {
            app.active_view = ActiveView::AgentDashboard;
            if let Some(warning) = policy_block
                && let Some(dashboard) = app.dashboard.as_mut()
            {
                dashboard.set_error_toast(warning);
            }
        }
    }
    crate::prompt_images::drain_and_cleanup(&mut images);
    effects
}

/// Cycle the dashboard overlay to the prev (-1) / next (+1) agent in the visible row order, wrapping at the ends.
/// Attaches overlay chrome on the first cycle from a session not opened via the dashboard.
pub(super) fn dispatch_dashboard_overlay_cycle(app: &mut AppView, delta: i32) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let ActiveView::Agent(current) = app.active_view else {
        return vec![];
    };
    if app.agents.len() <= 1 {
        return vec![];
    }

    let order = if app.workspace_dashboard_enabled {
        app.workspace_snapshot
            .as_ref()
            .map(|snapshot| {
                crate::views::dashboard::build_rows_with_workspace(
                    &app.agents,
                    snapshot,
                    crate::views::dashboard::render::cached_home(),
                )
                .into_iter()
                .filter_map(|row| match row.id {
                    DashboardRowId::TopLevel(id) if !row.is_more_placeholder => Some(id),
                    _ => None,
                })
                .collect()
            })
            .unwrap_or_default()
    } else {
        match app.dashboard.as_ref() {
            Some(d) => crate::views::dashboard::overlay_cycle_order(d, &app.agents),
            None => {
                if !crate::views::dashboard::dashboard_enabled()
                    || !matches!(app.auth_state, crate::app::app_view::AuthState::Done)
                {
                    return vec![];
                }
                let transient = dashboard_state_from_persisted(app);
                crate::views::dashboard::overlay_cycle_order(&transient, &app.agents)
            }
        }
    };
    if order.len() <= 1 {
        return vec![];
    }
    let Some(idx) = order.iter().position(|id| *id == current) else {
        return vec![];
    };
    let n = order.len() as i32;
    let next_idx = (((idx as i32) + delta).rem_euclid(n)) as usize;
    let next_id = order[next_idx];
    if next_id == current {
        return vec![];
    }
    clear_overlay_login_or_secret_modal(app);
    // Materialize + configure only on a real switch — otherwise a
    // cycle-created dashboard renders bare on back-out (default cwd, empty
    // `/model`, wrong auto-approve).
    if app.dashboard.is_none() {
        ensure_dashboard_state(app);
        configure_dashboard_state(app);
    }

    if let Some(agent) = app.agents.get_mut(&next_id) {
        agent.active_subagent = None;
    }

    clear_pending_overlay_stop(app);
    if let Some(d) = app.dashboard.as_mut() {
        d.restore_peek_viewport(&mut app.agents);
        d.attached_agent = Some(next_id);
        d.focus_row(DashboardRowId::TopLevel(next_id));
    }
    app.active_view = ActiveView::Agent(next_id);
    let effects = app.sync_primary_provider_from_active_agent();
    surface_yolo_launch_block_notice(app, next_id);
    effects
}

pub(super) fn dispatch_dashboard_dispatch(
    app: &mut AppView,
    text: String,
    attach: bool,
) -> Vec<Effect> {
    // Enter is a submit attempt — promote interim, hard-reset voice, merge into
    // the payload so even a rejected send (empty / over-cap) can't leave a hot
    // mic or let a late final refill the box.
    let text = merge_prompt_with_voice_interim(text, voice_stop_on_submit(app));
    // Paste-then-immediate-send: a Cmd+V image probe is still off-thread

    if let Some(d) = app.dashboard.as_mut()
        && d.paste_probe_in_flight > 0
    {
        d.deferred_dispatch_send =
            Some(crate::views::dashboard::state::DeferredDispatchSend { attach });
        return vec![];
    }
    let trimmed = text.trim().to_string();

    if crate::slash::commands::exit::is_exit_alias(&trimmed) {
        if let Some(d) = app.dashboard.as_mut() {
            d.dispatch.set_text("");
            d.error_toast = None;
        }
        return dispatch(Action::Quit, app);
    }

    if trimmed.is_empty() {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Type a prompt to dispatch a session");
        }
        return vec![];
    }

    const MAX_DISPATCH_BYTES: usize = 64 * 1024;
    if text.len() > MAX_DISPATCH_BYTES {
        let chars = text.chars().count();
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast(&format!(
                "Prompt too long ({chars} chars / {} bytes; max ~64 KiB)",
                text.len()
            ));
        }
        return vec![];
    }

    if app.cwd_has_git_ancestor && app.dashboard.as_ref().is_some_and(|d| d.dispatch_worktree) {
        return open_dashboard_worktree_dialog(app, Some(text), attach);
    }

    //
    // New-session path.
    //

    //

    let pending_model = app.dashboard.as_ref().and_then(|d| d.pending_model.clone());
    let (pending_mode, policy_block) = resolve_pending_dispatch_mode(app);
    let model_id = pending_model.as_ref().map(|m| m.id.clone());
    let prompt_state = app
        .dashboard
        .as_mut()
        .map(|dashboard| snapshot_prompt_widget(&mut dashboard.dispatch, text.clone()))
        .unwrap_or_else(|| {
            crate::views::prompt_widget::StashedPrompt::from_submission(
                text,
                Vec::new(),
                Vec::new(),
            )
        });
    let (prompt_text, mut pasted_images, chip_elements) = prompt_state.into_submission();
    log_dashboard_launched("prompt");
    let (new_id, mut effects) = dispatch_new_session_inner_with_id(app, model_id);
    set_create_permission_mode(&mut effects, pending_mode);
    if let Some(agent) = app.agents.get_mut(&new_id) {
        agent.session.enqueue_prompt(prompt_text);
        if let Some(entry) = agent.session.pending_prompts.back_mut() {
            entry.images = std::mem::take(&mut pasted_images);
            entry.chip_elements = chip_elements;
        }
        apply_pending_dispatch_config(agent, pending_model.as_ref(), pending_mode, policy_block);
    }
    crate::prompt_images::drain_and_cleanup(&mut pasted_images);

    if let Some(d) = app.dashboard.as_mut() {
        d.dispatch.set_text("");
        d.error_toast = None;
        d.filter = crate::views::dashboard::Filter::None;
    }
    if attach {
        if let Some(d) = app.dashboard.as_mut() {
            d.restore_peek_viewport(&mut app.agents);
            d.focus_row(crate::views::dashboard::DashboardRowId::TopLevel(new_id));
            d.attached_agent = Some(new_id);
        }
        app.active_view = ActiveView::Agent(new_id);
        sync_active_permission_mode_mirror(app);
        surface_yolo_launch_block_notice(app, new_id);
    } else {
        // Plain Enter (Send) stays on the dashboard, no auto-select.

        app.active_view = ActiveView::AgentDashboard;

        if let Some(warning) = policy_block
            && let Some(d) = app.dashboard.as_mut()
        {
            d.set_error_toast(warning);
        }
    }
    effects
}

/// Resolve a slash command typed into the dashboard's dispatch input.
///
/// The dashboard has no session context, so the execution path is more limited than the agent view's:
///
///   - Builtin commands returning `CommandResult::Action(...)` run as in the agent path (`/exit`/`/quit` quit the CLI; `/home` leaves the dashboard).
///     Examples: `/dashboard`, `/exit`, `/quit`, `/theme`, `/settings`, `/help`, `/model`, `/mcps`, `/plugin`.
///   - `CommandResult::Message` / `Error` surface as an `error_toast` on the dashboard (no scrollback to push into).
///     `Error` strings get the `✗` prefix via `set_error_toast`; `Message` strings are stored verbatim (they carry their own glyph).
///   - `CommandResult::Handled` clears the input.
///   - `CommandResult::PassThrough` (unknown or ACP-advertised commands), `QueueCommand`, and `InjectSkill` become a bare free-text dispatch.
///     The text spawns a new session as its first prompt, so a plugin or skill invoked from the dashboard is never silently dropped.
///
/// Offer / execute tri-state (matches completion's [`command_offered`]):
///   - **Unknown** token → [`dispatch_dashboard_dispatch`] (new session prompt).
///   - **Registered, not offered** (session-scoped hidden on this surface,
///     or `dashboard_only` off-dashboard) → clear dispatch + error toast;
///     do **not** spawn with the slash text as the prompt.
///   - **Registered, offered** → MRU + `command.run` (e.g. `/model` /
///     `/plan` stage the next spawn).
pub(super) fn dispatch_dashboard_dispatch_slash(app: &mut AppView, text: String) -> Vec<Effect> {
    use crate::slash::command::{CommandExecCtx, CommandResult};
    use crate::slash::parse_invocation;

    // Enter is a submit attempt — promote interim, hard-reset, merge into payload.
    let text = merge_prompt_with_voice_interim(text, voice_stop_on_submit(app));
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return vec![];
    }

    let coding_data_sharing_opt_out_from_app = app.coding_data_retention_opt_out;
    let coding_data_sharing_lock_from_app = app.coding_data_sharing_lock();
    let show_tips_from_app = app.show_tips;
    let auto_update_from_app = app.auto_update;
    let respect_manual_folds_from_app = app.appearance.scrollback.scroll.respect_manual_folds;
    let auto_mode_gate_from_app = app.auto_mode_gate;
    let ask_user_question_timeout_enabled_from_app = app.ask_user_question_timeout_enabled;
    let voice_stt_language_from_app = app.voice_config.language.clone();

    let scheduler_background_loops_seed = app.scheduler_background_loops_seed;

    let result = {
        let Some(invocation) = parse_invocation(trimmed.as_str()) else {
            return vec![];
        };

        // Get the slash registry from the dashboard's prompt widget.

        let Some(dashboard) = app.dashboard.as_ref() else {
            return vec![];
        };
        let reg = dashboard.dispatch.slash_controller.registry();

        {
            use xai_grok_telemetry::events::{PagerCommandSource, PagerSlashCommand};
            use xai_grok_telemetry::session_ctx::log_event;
            let source = if reg.is_builtin(invocation.token) {
                PagerCommandSource::Builtin
            } else {
                PagerCommandSource::NonBuiltin
            };
            log_event(PagerSlashCommand {
                command_name: invocation.token.to_string(),
                source,
            });
        }

        if reg.is_restricted(invocation.token) {
            let token = invocation.token.to_string();
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast(&format!(
                    "/{token} requires SuperGrok: upgrade at {}",
                    super::billing::UPSELL_URL_UPGRADE
                ));
            }
            return vec![];
        }

        let Some(command) = reg.get(invocation.token).cloned() else {
            return dispatch_dashboard_dispatch(app, text, /* attach */ false);
        };
        // Registered but not offered on this surface (session-scoped
        // hidden from the dropdown, or non-dashboard `dashboard_only`):
        // error toast — never spawn a session whose first prompt is the
        // slash text (that was worse than the old loud Action toasts).
        if !dashboard
            .dispatch
            .slash_controller
            .is_command_offered(command.as_ref(), &app.models)
        {
            let name = command.name();
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast(&format!("/{name} only works in a session"));
            }
            return vec![];
        }
        if let Some(dashboard) = app.dashboard.as_mut() {
            // Records MRU and queues an off-thread persist internally.
            dashboard
                .dispatch
                .slash_controller
                .record_command_use(invocation.token, invocation.token);
        }

        let dashboard_multiline = app.dashboard.as_ref().is_some_and(|d| d.multiline_mode);
        let mut ctx = CommandExecCtx {
            models: &app.models,
            session_id: None,
            bundle_state: &app.bundle_state,
            screen_mode: app.screen_mode,
            billing_surface_visible: app.usage_visible,
            pager_state: crate::settings::PagerLocalSnapshot {
                multiline_mode: dashboard_multiline,
                yolo_mode: app.default_yolo,
                auto_mode: app.current_ui.permission_mode.as_deref() == Some("auto")
                    && !app.default_yolo,
                current_model_name: app.models.current_model_name(),
                available_models: app
                    .models
                    .available
                    .iter()
                    .map(|(id, info)| (info.name.clone(), id.clone()))
                    .collect(),
                recap_model: app.recap_model.clone(),
                kimi_api_key_status: crate::settings::SecretStatus::Missing,
                kimi_code_api_key_status: crate::settings::SecretStatus::Missing,
                fireworks_api_key_status: crate::settings::SecretStatus::Missing,
                deepseek_api_key_status:
                    crate::app::dispatch::settings::ui::deepseek_api_key_status(),
                meta_api_key_status: crate::app::dispatch::settings::ui::meta_api_key_status(),
                opencode_go_api_key_status:
                    crate::app::dispatch::settings::ui::opencode_go_api_key_status(),
                wafer_api_key_status: crate::app::dispatch::settings::ui::wafer_api_key_status(),
                zai_api_key_status: crate::app::dispatch::settings::ui::zai_api_key_status(),
                runinfra_api_key_status:
                    crate::app::dispatch::settings::ui::runinfra_api_key_status(),
                gemini_api_key_status: crate::app::dispatch::settings::ui::gemini_api_key_status(),
                openrouter_api_key_status:
                    crate::app::dispatch::settings::ui::openrouter_api_key_status(),
                opencode_go_models: app.opencode_go_models.clone(),
                opencode_go_enabled_models: app.opencode_go_enabled_models.clone(),
                openrouter_models: app.openrouter_models.clone(),
                openrouter_enabled_models: app.openrouter_enabled_models.clone(),
                custom_models: crate::settings::cached_custom_models(),
                custom_model_id: String::new(),
                custom_model_slug: String::new(),
                custom_model_name: String::new(),
                custom_model_provider: String::new(),
                custom_model_base_url: String::new(),
                custom_model_context_window:
                    crate::settings::defs::CUSTOM_MODEL_CONTEXT_WINDOW_DEFAULT,
                custom_model_backend: "chat_completions".to_owned(),
                custom_model_env_key: String::new(),
                custom_model_save: false,
                perplexity_web_search_enabled: app.perplexity_web_search_enabled,
                web_search_source: xai_grok_shell::util::config::load_web_search_source_sync(),
                x_search_enabled: xai_grok_shell::util::config::load_x_search_config_sync().enabled,
                antigravity_skip_permissions:
                    xai_grok_shell::util::config::load_antigravity_skip_permissions_sync(),
                perplexity_api_key_status:
                    crate::app::dispatch::settings::ui::perplexity_api_key_status(),
                kimi_api_endpoint: app.kimi_api_endpoint.clone(),
                memory_model: app.memory_model.clone(),
                coding_data_sharing_opt_out: coding_data_sharing_opt_out_from_app,
                coding_data_sharing_lock: coding_data_sharing_lock_from_app,
                plan_mode_active: false,
                swarm_mode: app.current_ui.swarm_mode.unwrap_or(false),
                show_tips: show_tips_from_app,
                auto_update: auto_update_from_app,
                vim_mode: crate::appearance::cache::load_vim_mode(),
                scroll_speed: crate::appearance::cache::load_scroll_speed(),
                respect_manual_folds: respect_manual_folds_from_app,
                auto_mode_gate: auto_mode_gate_from_app,
                ask_user_question_timeout_enabled: ask_user_question_timeout_enabled_from_app,
                voice_stt_language: voice_stt_language_from_app,
                scheduler_background_loops: scheduler_background_loops_seed,
                local_feature_flags: xai_grok_shell::util::config::load_local_feature_flags_sync(),
            },
        };
        command.run(&mut ctx, invocation.args)
    };

    match result {
        CommandResult::Handled | CommandResult::HandledNoOp => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
            }
            vec![]
        }
        CommandResult::Error(msg) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");

                d.set_error_toast(&msg);
            }
            vec![]
        }
        CommandResult::Message(msg) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = Some(msg);
            }
            vec![]
        }
        CommandResult::Action(Action::ExitSession) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
            }
            dispatch(Action::ExitDashboard, app)
        }
        // `/model` on the session-less dashboard stages the model for the
        // NEXT spawned agent instead of switching a (nonexistent) session.
        // Both the effort-bearing (`SwitchModel`) and bare
        // (`SetDefaultModel`) forms map to the same per-spawn staging — we
        // deliberately do NOT persist a global default here.
        CommandResult::Action(Action::SwitchModel {
            model_id,
            effort,
            service_tier: _,
        }) => {
            stage_dashboard_model(app, model_id, effort);
            vec![]
        }
        CommandResult::Action(Action::SetDefaultModel(model_id)) => {
            stage_dashboard_model(app, model_id, None);
            vec![]
        }

        CommandResult::Action(Action::SetPlanMode(_)) => {
            use crate::views::dashboard::DashboardDispatchMode;
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
                d.pending_mode = if d.pending_mode == DashboardDispatchMode::Plan {
                    DashboardDispatchMode::Normal
                } else {
                    DashboardDispatchMode::Plan
                };
            }
            vec![]
        }

        CommandResult::Action(Action::EnterPlanMode { description }) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.pending_mode = crate::views::dashboard::DashboardDispatchMode::Plan;
            }
            match description {
                Some(desc) => {
                    dispatch_dashboard_dispatch(app, desc, /* attach */ false)
                }
                None => {
                    if let Some(d) = app.dashboard.as_mut() {
                        d.dispatch.set_text("");
                        d.error_toast = None;
                    }
                    vec![]
                }
            }
        }

        CommandResult::Action(Action::ShowPlan) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast("No plan to show on the dashboard");
            }
            vec![]
        }
        CommandResult::Action(action) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.error_toast = None;
            }
            dispatch(action, app)
        }
        CommandResult::Doctor(_) => {
            if let Some(d) = app.dashboard.as_mut() {
                d.dispatch.set_text("");
                d.set_error_toast("Open a session to run /doctor.");
            }
            vec![]
        }
        CommandResult::QueueCommand(_)
        | CommandResult::InjectSkill { .. }
        | CommandResult::PassThrough(_) => {
            dispatch_dashboard_dispatch(app, text, /* attach */ false)
        }
    }
}

/// Stage a model (and optional reasoning effort) for the next agent the dashboard spawns.
/// Resolves the display name from the app's model catalog (or the raw id) so the renderer can show the indicator without a live `ModelState`.
/// Clears the dispatch input and any error toast.
fn stage_dashboard_model(
    app: &mut AppView,
    model_id: acp::ModelId,
    effort: Option<xai_grok_shell::sampling::types::ReasoningEffort>,
) {
    let display = app
        .models
        .available
        .get(&model_id)
        .map(|info| info.name.clone())
        .unwrap_or_else(|| model_id.0.to_string());
    if let Some(d) = app.dashboard.as_mut() {
        d.dispatch.set_text("");
        d.error_toast = None;

        d.models.set_current(model_id.clone(), effort);
        d.pending_model = Some(crate::views::dashboard::PendingDispatchModel {
            id: model_id,
            effort,
            display,
        });
    }
}

/// Apply the dashboard's staged model effort and plan mode to a freshly spawned agent.
/// The base model is already seeded via `CreateSession`'s `model_id`.
/// The reasoning effort is stashed here and pushed to the shell once the session exists, mirroring the agent-view flow.
/// The deferred plan `SessionMode` is consumed in the `SessionCreated` handlers.
pub(super) fn apply_pending_dispatch_config(
    agent: &mut AgentView,
    pending_model: Option<&crate::views::dashboard::PendingDispatchModel>,
    pending_mode: crate::views::dashboard::DashboardDispatchMode,
    policy_block: Option<&'static str>,
) {
    use crate::views::dashboard::DashboardDispatchMode;

    if let Some(m) = pending_model {
        agent.session.deferred_model_switch = m.effort.map(|e| DeferredModelSwitch {
            model_id: m.id.clone(),
            effort: Some(e),
            // Effort-only push; no display change to roll back.
            prev_model_id: None,
        });
    }
    let pending_mode =
        if policy_block.is_some() && pending_mode == DashboardDispatchMode::AlwaysApprove {
            DashboardDispatchMode::Normal
        } else {
            pending_mode
        };
    agent.session.yolo_mode = pending_mode == DashboardDispatchMode::AlwaysApprove;
    agent.session.auto_mode = pending_mode == DashboardDispatchMode::Auto;
    match pending_mode {
        DashboardDispatchMode::Normal
        | DashboardDispatchMode::Auto
        | DashboardDispatchMode::AlwaysApprove => {}
        DashboardDispatchMode::Plan => {
            agent.deferred_session_mode = Some(xai_grok_tools::types::SessionMode::Plan);
            // Optimistic so the agent view reflects plan mode immediately when
            // opened via Ctrl+S, before the ACP round-trip confirms it.
            agent.plan_mode_pending = Some(true);
        }
    }
    if let Some(warning) = policy_block {
        agent.show_toast(warning);
    }
}

/// Cycle the peeked agent's live mode using the agent prompt's gated rotation, the peek-panel counterpart to `DashboardCycleMode`.
/// Reuses `dispatch_cycle_mode_and_sync` via the same temporary `active_view` swap as `dispatch_dashboard_toggle_auto_approve`.
/// The peek then behaves exactly like Shift+Tab inside that agent's chat view; the bottom-border badge reflects the new mode on the next frame.
/// Only top-level agents have a mode to cycle; subagents are parent-driven.
pub(super) fn dispatch_dashboard_peek_cycle_mode(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    let Some(row) = app
        .dashboard
        .as_ref()
        .and_then(|d| d.peek.as_ref().map(|p| p.row.clone()))
    else {
        return vec![];
    };
    let agent_id = match row {
        DashboardRowId::TopLevel(id) => id,
        DashboardRowId::Subagent { .. } => {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_error_toast("Can't change a subagent's mode");
            }
            return vec![];
        }
        DashboardRowId::Roster { .. } | DashboardRowId::Workspace { .. } => return vec![],
    };
    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }

    let saved_view = app.active_view;
    app.active_view = ActiveView::Agent(agent_id);
    let effects = dispatch_cycle_mode_and_sync(app);
    app.active_view = saved_view;
    effects
}

/// Send or queue a reply typed into the peek panel's `❯ reply` input.
///
/// The reply is enqueued on the row's owning top-level agent and [`maybe_drain_queue`] decides the rest.
/// An idle agent sends it immediately (a turn starts); a mid-turn agent keeps it queued so it drains after the current turn finishes.
/// This is the same queue and drain pipeline the agent view's own prompt input uses, so the two surfaces behave identically.
///
/// Subagent rows can't be replied to (they're driven by their parent), so they surface a toast and leave the peek open.
///
/// `attach` (Ctrl+S) additionally walks into the agent's detail view, mirroring the dispatch input's send+open affordance.
pub(super) fn dispatch_dashboard_peek_reply(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    text: String,
    attach: bool,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    // Enter is a submit attempt — promote interim, hard-reset, merge into payload
    // so a rejected reply can't leave a hot mic or let a late final refill the box.
    let text = merge_prompt_with_voice_interim(text, voice_stop_on_submit(app));

    // Paste-then-immediate-send: a Cmd+V image probe is still off-thread

    if let Some(d) = app.dashboard.as_mut()
        && d.paste_probe_in_flight > 0
    {
        d.deferred_peek_send =
            Some(crate::views::dashboard::state::DeferredPeekSend { row, attach });
        return vec![];
    }

    let DashboardRowId::TopLevel(agent_id) = row else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_error_toast("Can't reply to a subagent");
        }
        return vec![];
    };

    if !app.agents.contains_key(&agent_id) {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Session no longer exists");
        }
        return vec![];
    }

    let prompt_state = app
        .dashboard
        .as_mut()
        .map(|dashboard| snapshot_prompt_widget(&mut dashboard.peek_reply, text.clone()))
        .unwrap_or_else(|| {
            crate::views::prompt_widget::StashedPrompt::from_submission(
                text,
                Vec::new(),
                Vec::new(),
            )
        });
    let (text, images, chip_elements) = prompt_state.into_submission();

    if text.trim().is_empty() && images.is_empty() {
        return vec![];
    }

    let drain = {
        let Some(agent) = app.agents.get_mut(&agent_id) else {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_peek(None);
                d.set_error_toast("Session no longer exists");
            }
            return vec![];
        };

        // Untrimmed so `chip_elements` byte ranges stay aligned with the stored text.
        agent.session.enqueue_prompt(text);
        if let Some(entry) = agent.session.pending_prompts.back_mut() {
            entry.chip_elements = chip_elements;
            if !images.is_empty() {
                entry.images = images;
            }
        }
        maybe_drain_queue(agent)
    };
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    let mut effects = drain.effects;

    if let Some(d) = app.dashboard.as_mut() {
        d.clear_peek_reply();
        d.error_toast = None;
    }

    if attach {
        if let Some(d) = app.dashboard.as_mut() {
            d.restore_peek_viewport(&mut app.agents);
            d.focus_row(DashboardRowId::TopLevel(agent_id));
            d.attached_agent = Some(agent_id);
        }
        app.active_view = ActiveView::Agent(agent_id);
        effects.extend(app.sync_primary_provider_from_active_agent());
        surface_yolo_launch_block_notice(app, agent_id);
    }

    effects
}

pub(super) fn dispatch_dashboard_toggle_pin(app: &mut AppView) -> Vec<Effect> {
    if let Some(d) = app.dashboard.as_mut() {
        let _ = d.toggle_pin_selected();
    }
    dispatch_dashboard_persist(app)
}

pub(super) fn dispatch_dashboard_begin_rename(app: &mut AppView) {
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    let Some(sel) = d.selected.clone() else {
        return;
    };
    let crate::views::dashboard::DashboardRowId::TopLevel(agent_id) = &sel else {
        let message = if sel.is_subagent() {
            "Subagent rows can't be renamed"
        } else {
            "Load the session before renaming"
        };
        d.set_error_toast(message);
        return;
    };
    let prefill = app
        .agents
        .get(agent_id)
        .map(rename_prefill_title)
        .unwrap_or_default();
    if let Some(d) = app.dashboard.as_mut() {
        d.rename = Some(crate::views::dashboard::state::RenameDraft::new(
            sel, prefill,
        ));
    }
}

fn rename_prefill_title(agent: &AgentView) -> String {
    if let Some(name) = agent.display_name.as_deref() {
        let trimmed = name.trim();
        if !trimmed.is_empty() {
            return crate::views::session_title::sanitize_display_text(trimmed).into_owned();
        }
    }
    if let Some(title) = agent.generated_session_title.as_deref() {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            return crate::views::session_title::sanitize_display_text(trimmed).into_owned();
        }
    }
    String::new()
}

pub(super) fn dispatch_dashboard_commit_rename(app: &mut AppView) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_mut() else {
        return vec![];
    };
    let Some(rn) = d.rename.take() else {
        return vec![];
    };

    let trimmed = rn.text().trim();
    if trimmed.is_empty() {
        return vec![];
    }
    let title: String =
        xai_grok_shell::session::persistence::sanitize_rename_title(trimmed).into_owned();
    if title.is_empty() {
        return vec![];
    }
    let crate::views::dashboard::DashboardRowId::TopLevel(agent_id) = rn.row else {
        return vec![];
    };
    let mut effects = Vec::new();
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        let kind = agent.rename_kind();
        if let Some(session_id) = agent.session.session_id.clone() {
            let cwd = agent.session.cwd.clone();
            agent.display_name = Some(title.clone());
            effects.push(Effect::RenameSession {
                agent_id,
                session_id,
                title,
                cwd,
                kind,
            });
        } else {
            agent.display_name = Some(title);
        }
    }
    crate::app::workspace_sync::request(app);
    effects
}

/// Pick the dashboard row the cursor should land on after `closed` is removed.
/// Computed against the CURRENT (pre-removal) display order so it mirrors what the user sees.
/// The target is the next visible row below `closed` (the cursor stays put while the rows below shift up), or the previous row when `closed` is last.
/// Section headers are skipped so the cursor always lands on an agent row.
/// Returns `None` when `closed` is the only row; the caller clears the selection and `reanchor_selection` falls back to the `[+ New Agent]` button.
///
/// Without this, closing the selected agent leaves a stale cursor that `reanchor_selection` drops to `None`, and the next ↑/↓ restarts from the top.
pub(super) fn dashboard_neighbor_row(
    app: &AppView,
    closed: &crate::views::dashboard::DashboardRowId,
) -> Option<crate::views::dashboard::DashboardRowId> {
    use crate::views::dashboard::Focusable;
    let d = app.dashboard.as_ref()?;
    let home = crate::views::dashboard::render::cached_home();
    let roster: &[crate::app::roster::RosterEntry] = if app.leader_mode {
        &app.leader_roster
    } else {
        &app.dashboard_local_sessions
    };
    let rows = if app.workspace_dashboard_enabled {
        app.workspace_snapshot
            .as_ref()
            .map(|snapshot| {
                crate::views::dashboard::build_rows_with_workspace(&app.agents, snapshot, home)
            })
            .unwrap_or_default()
    } else {
        crate::views::dashboard::build_rows_with_roster(
            &app.agents,
            &d.pinned,
            &d.reorder,
            d.grouping,
            &d.filter,
            home,
            roster,
        )
    };
    let focusables = crate::views::dashboard::render::focusables(
        &rows,
        d.grouping,
        &d.filter,
        &d.collapsed_sections,
        d.idle_show_all,
        d.search_mode,
    );
    let cur = focusables
        .iter()
        .position(|f| matches!(f, Focusable::Row(id) if id == closed))?;

    let next = focusables[cur + 1..].iter().find_map(|f| match f {
        Focusable::Row(id) => Some(id.clone()),
        Focusable::Section(_) | Focusable::IdleOverflow => None,
    });
    next.or_else(|| {
        focusables[..cur].iter().rev().find_map(|f| match f {
            Focusable::Row(id) => Some(id.clone()),
            Focusable::Section(_) | Focusable::IdleOverflow => None,
        })
    })
}

/// Ctrl+X on the selected dashboard row, keyed off the row's `RowState` (the same `allows_delete` the renderer paints `[✗]` with):
/// - Deletable row: first press arms, a second within the window deletes.
/// - Busy top-level row: stop what keeps it busy (running turn, background tasks/monitors/`/loop`s, or queued prompts), never arm.
///   A busy roster row has no local work to stop, so it just reports it must be stopped first.
/// - Subagent row: kill the subagent.
///
/// Delete only ever runs on an idle row, so it is never queued alongside a `CancelTurn`.
pub(super) fn dispatch_dashboard_stop(app: &mut AppView) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;
    use std::time::Instant;

    let Some(sel) = app.dashboard.as_ref().and_then(|d| d.selected.clone()) else {
        return vec![];
    };
    match &sel {
        DashboardRowId::TopLevel(id) => {
            let id = *id;
            let Some(agent) = app.agents.get_mut(&id) else {
                return vec![];
            };
            if !crate::views::dashboard::classify_top_level(agent).allows_delete() {
                let stopped = stop_top_level_activity(agent);
                if let Some(d) = app.dashboard.as_mut() {
                    d.delete_confirm = None;
                }
                return match stopped {
                    Some(effects) => effects,
                    None => {
                        app.show_toast("Stop the session before deleting");
                        vec![]
                    }
                };
            }
            arm_or_delete(app, sel)
        }
        DashboardRowId::Subagent {
            parent,
            child_session_id,
        } => {
            let Some(agent) = app.agents.get_mut(parent) else {
                return vec![];
            };
            let Some(info) = agent.subagent_sessions.get_mut(child_session_id) else {
                return vec![];
            };
            let subagent_id = info.subagent_id.to_string();
            info.pending_kill = true;
            info.kill_requested_at = Some(Instant::now());
            let session_id = agent.session.session_id.clone();
            session_id
                .map(|sid| Effect::KillSubagent {
                    session_id: sid,
                    subagent_id,
                })
                .into_iter()
                .collect()
        }
        DashboardRowId::Roster { session_id } => {
            let entry = app
                .leader_roster
                .iter()
                .chain(app.dashboard_local_sessions.iter())
                .find(|e| e.session_id == session_id.as_str());
            match entry {
                None => {
                    app.show_toast("Session is no longer in the list");
                    vec![]
                }

                Some(e) if e.origin.kind == "conversation" => {
                    app.show_toast("Deleting chat conversations isn't supported yet");
                    vec![]
                }
                // No local turn to cancel, so a busy roster row can't delete.
                Some(e)
                    if !crate::views::dashboard::roster_activity_to_state(e.activity)
                        .allows_delete() =>
                {
                    app.show_toast("Stop the session before deleting");
                    vec![]
                }
                Some(_) => arm_or_delete(app, sel),
            }
        }
        DashboardRowId::Workspace { .. } => vec![],
    }
}

/// Stop what keeps a busy top-level row out of Idle: a running turn, background tasks/monitors, scheduled `/loop`s, and queued (unsent) prompts.
/// Marks local state optimistically (mirroring the agent view's own kill paths).
/// Returns `Some(effects)` when it stopped something (empty if only the local prompt queue was dropped); `None` when nothing was stoppable.
fn stop_top_level_activity(agent: &mut crate::app::agent_view::AgentView) -> Option<Vec<Effect>> {
    let session_id = agent.session.session_id.clone();
    let mut effects = Vec::new();

    // Turn / background work need a session id to reach the backend.
    if let Some(session_id) = session_id {
        if !agent.session.state.is_idle() || agent.wake_turn_active() {
            if agent.session.state.is_compact_running() {
                agent.cancel_and_arm(CancellationScope::Compaction, CancelOrigin::UserGesture);
            } else if agent.running_wake_turn.is_some() {
                agent.mark_wake_cancel_sent();
            } else if agent.session.state.is_turn_running() {
                agent.cancel_and_arm(CancellationScope::Turn, CancelOrigin::UserGesture);
            }
            agent.cancel_trigger_hint = Some(crate::app::actions::CancelTrigger::DashboardStop);

            effects.push(super::turn::emit_cancel_turn(
                agent,
                session_id.clone(),
                /* cancel_subagents */ true,
                /* rewind_if_no_output */ false,
            ));
        }
        let running: Vec<String> = agent
            .session
            .bg_tasks
            .values()
            .filter(|t| t.status == crate::app::agent::BgTaskStatus::Running)
            .map(|t| t.task_id.clone())
            .collect();
        for task_id in running {
            if let Some(task) = agent.session.bg_tasks.get_mut(&task_id) {
                task.pending_kill = true;
                task.kill_requested_at = Some(std::time::Instant::now());
            }
            effects.push(Effect::KillBgTask {
                session_id: session_id.clone(),
                task_id,
                source: xai_grok_shell::extensions::task::TaskKillSource::Teardown,
            });
        }
        let scheduled: Vec<String> = agent.session.scheduled_tasks.keys().cloned().collect();
        for task_id in scheduled {
            agent.session.scheduled_tasks.remove(&task_id);
            effects.push(Effect::DeleteScheduledTask {
                session_id: session_id.clone(),
                task_id,
            });
        }
    }

    let dropped_queue = !agent.session.pending_prompts.is_empty();
    if dropped_queue {
        agent.session.pending_prompts.clear();
        agent.sync_queue_pane();
    }

    (!effects.is_empty() || dropped_queue).then_some(effects)
}

/// A live arm on `sel` confirms and deletes; otherwise (re)arm.
fn arm_or_delete(app: &mut AppView, sel: crate::views::dashboard::DashboardRowId) -> Vec<Effect> {
    let armed = app
        .dashboard
        .as_mut()
        .and_then(|d| d.armed_delete_row())
        .as_ref()
        == Some(&sel);
    if armed {
        return delete_dashboard_row(app, sel);
    }
    if let Some(d) = app.dashboard.as_mut() {
        d.arm_delete(sel);
    }
    vec![]
}

pub(super) fn dispatch_dashboard_delete(app: &mut AppView) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_mut() else {
        return vec![];
    };

    let Some(sel) = d.armed_delete_row() else {
        return vec![];
    };
    if d.selected.as_ref() != Some(&sel) {
        d.delete_confirm = None;
        return vec![];
    }
    delete_dashboard_row(app, sel)
}

/// Delete `row`, which the caller has confirmed is idle and armed.
/// Takes `row` as a parameter (not read back off `delete_confirm`) and never cancels a turn or kills a task; delete is a settled-row operation.
fn delete_dashboard_row(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
) -> Vec<Effect> {
    use crate::views::dashboard::DashboardRowId;

    if let Some(d) = app.dashboard.as_mut() {
        d.delete_confirm = None;
    }
    match row {
        DashboardRowId::TopLevel(id) => {
            let Some(agent) = app.agents.get(&id) else {
                return vec![];
            };

            if !crate::views::dashboard::classify_top_level(agent).allows_delete() {
                app.show_toast("Stop the session before deleting");
                return vec![];
            }
            let Some(session_id) = agent.session.session_id.clone() else {
                app.show_toast("No session history to delete");
                return vec![];
            };
            let cwd = agent.session.cwd.display().to_string();
            app.show_toast("Deleting session\u{2026}");
            vec![Effect::DeleteSession {
                source: "current".into(),
                session_id: session_id.to_string(),
                cwd,
                after: crate::app::actions::AfterSessionDelete::Dashboard,
            }]
        }
        DashboardRowId::Subagent { .. } => {
            app.show_toast("Subagent rows can't be deleted from the dashboard");
            vec![]
        }
        DashboardRowId::Roster { session_id } => {
            let Some(entry) = app
                .leader_roster
                .iter()
                .chain(app.dashboard_local_sessions.iter())
                .find(|e| e.session_id == session_id)
                .cloned()
            else {
                app.show_toast("Session is no longer in the list");
                return vec![];
            };
            if entry.origin.kind == "conversation" {
                app.show_toast("Deleting chat conversations isn't supported yet");
                return vec![];
            }
            if !crate::views::dashboard::roster_activity_to_state(entry.activity).allows_delete() {
                app.show_toast("Stop the session before deleting");
                return vec![];
            }
            app.show_toast("Deleting session\u{2026}");
            vec![Effect::DeleteSession {
                source: "local".into(),
                session_id,
                cwd: entry.cwd,
                after: crate::app::actions::AfterSessionDelete::Dashboard,
            }]
        }
        DashboardRowId::Workspace { .. } => vec![],
    }
}

pub(super) fn dispatch_dashboard_toggle_grouping(app: &mut AppView) -> Vec<Effect> {
    if let Some(d) = app.dashboard.as_mut() {
        d.toggle_grouping();
    }
    dispatch_dashboard_persist(app)
}

pub(super) fn dispatch_dashboard_select(app: &mut AppView, next: bool) {
    let Some(d) = app.dashboard.as_mut() else {
        return;
    };
    // We don't have rows cached here; reconstruct from agents.

    let home = crate::views::dashboard::render::cached_home();

    // Disjoint field borrows (`app.dashboard` is held mutably via `d`).
    let roster: &[crate::app::roster::RosterEntry] = if app.leader_mode {
        &app.leader_roster
    } else {
        &app.dashboard_local_sessions
    };
    let rows = if app.workspace_dashboard_enabled {
        app.workspace_snapshot
            .as_ref()
            .map(|snapshot| {
                crate::views::dashboard::build_rows_with_workspace(&app.agents, snapshot, home)
            })
            .unwrap_or_default()
    } else {
        crate::views::dashboard::build_rows_with_roster(
            &app.agents,
            &d.pinned,
            &d.reorder,
            d.grouping,
            &d.filter,
            home,
            roster,
        )
    };

    let focusables = crate::views::dashboard::render::focusables(
        &rows,
        d.grouping,
        &d.filter,
        &d.collapsed_sections,
        d.idle_show_all,
        d.search_mode,
    );
    let set_cursor = |d: &mut crate::views::dashboard::DashboardState,
                      f: &crate::views::dashboard::Focusable| {
        match f {
            crate::views::dashboard::Focusable::Section(key) => d.focus_section(*key),
            crate::views::dashboard::Focusable::Row(id) => d.focus_row(id.clone()),
            crate::views::dashboard::Focusable::IdleOverflow => d.focus_idle_overflow(),
        }
    };
    // Button-focused navigation contract:

    if d.new_agent_button_focused {
        if next && !focusables.is_empty() {
            set_cursor(d, &focusables[0]);
            d.clear_manual_scroll();
        }
        return;
    }
    if focusables.is_empty() {
        d.focus_new_agent_button();
        return;
    }
    // Current index from the active cursor (section header or row).
    let cur = focusables
        .iter()
        .position(|f| match f {
            crate::views::dashboard::Focusable::Section(key) => d.selected_section == Some(*key),
            crate::views::dashboard::Focusable::Row(id) => d.selected.as_ref() == Some(id),
            crate::views::dashboard::Focusable::IdleOverflow => d.selected_idle_overflow,
        })
        .unwrap_or(0);

    if !next && cur == 0 {
        d.focus_new_agent_button();
        d.clear_manual_scroll();
        return;
    }
    let new = if next {
        (cur + 1).min(focusables.len() - 1)
    } else {
        cur.saturating_sub(1)
    };
    set_cursor(d, &focusables[new]);

    d.clear_manual_scroll();
}

pub(super) fn dispatch_dashboard_reorder(app: &mut AppView, up: bool) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_mut() else {
        return vec![];
    };
    let Some(sel) = d.selected.clone() else {
        return vec![];
    };

    let pos = d.reorder.iter().position(|r| *r == sel);
    if up {
        match pos {
            Some(0) => {
                d.reorder.remove(0);
            }
            Some(i) => {
                d.reorder.swap(i, i - 1);
            }
            None => {
                d.reorder.insert(0, sel);
            }
        }
    } else {
        match pos {
            Some(i) if i + 1 < d.reorder.len() => {
                d.reorder.swap(i, i + 1);
            }
            Some(_) => {
                // Already at the bottom — append to end.
            }
            None => {
                d.reorder.push(sel);
            }
        }
    }
    dispatch_dashboard_persist(app)
}

fn dispatch_dashboard_persist(app: &mut AppView) -> Vec<Effect> {
    let Some(d) = app.dashboard.as_ref() else {
        return vec![];
    };
    // Don't hardcode `enabled = true`

    let enabled = app
        .dashboard_persisted
        .as_ref()
        .map(|p| p.enabled)
        .unwrap_or(true);
    let resolver = crate::views::dashboard::SessionIdResolver::from_agents(&app.agents);
    let persisted = d.to_persisted(enabled, &resolver);
    app.dashboard_persisted = Some(persisted.clone());
    vec![Effect::PersistDashboard(persisted)]
}

/// Answer a permission request from the dashboard peek panel without going through `PermissionSelect`, which needs `active_view == Agent(_)`.
/// Routes directly to the row's owning agent and verifies the request_id has not rotated since the peek snapshot was taken.
pub(super) fn dispatch_dashboard_permission_select(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    request_id: usize,
    option_id: acp::PermissionOptionId,
) -> Vec<Effect> {
    // Determine the owning AgentId.
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. }
        | crate::views::dashboard::DashboardRowId::Workspace { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };
    // Stale-snapshot guard.
    let front_matches = agent
        .permission_queue
        .front()
        .is_some_and(|p| p.id == request_id);
    if !front_matches {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Permission has changed: re-open peek");
        }
        return vec![];
    }
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };

    let edited_pattern = super::permissions::take_edited_pattern(agent, &perm);
    let meta = super::permissions::build_selection_meta(&perm, &option_id, edited_pattern);

    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id)),
        )
        .meta(meta)))
        .ok();

    resolve_permission_queue_transition(agent);

    // Refresh the peek (it likely no longer has a question).
    if let Some(d) = app.dashboard.as_mut() {
        d.set_peek(None);
    }
    vec![]
}

/// Reject the peeked agent's pending permission with a typed feedback message: the peek panel's "No, type to add feedback" path.
///
/// Mirrors [`super::permissions::dispatch_permission_followup`]: resolve the front request with `RejectOnce` and the `followup_message` meta.
/// Targets the dashboard row's agent instead of the active view, with the same stale-request guard as [`dispatch_dashboard_permission_select`].
pub(super) fn dispatch_dashboard_permission_followup(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    request_id: usize,
    text: String,
) -> Vec<Effect> {
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. }
        | crate::views::dashboard::DashboardRowId::Workspace { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };

    let front_matches = agent
        .permission_queue
        .front()
        .is_some_and(|p| p.id == request_id);
    if !front_matches {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Permission has changed: re-open peek");
        }
        return vec![];
    }
    let Some(perm) = agent.permission_queue.pop_front() else {
        return vec![];
    };

    let option_id = perm
        .options
        .iter()
        .find(|o| o.kind == acp::PermissionOptionKind::RejectOnce)
        .map(|o| o.option_id.clone());
    let outcome = match option_id {
        Some(option_id) => {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(option_id))
        }
        None => acp::RequestPermissionOutcome::Cancelled,
    };
    let meta = if !text.trim().is_empty() {
        serde_json::json!({ "followup_message": text })
            .as_object()
            .cloned()
    } else {
        None
    };
    perm.request
        .response_tx
        .send(Ok(acp::RequestPermissionResponse::new(outcome).meta(meta)))
        .ok();
    resolve_permission_queue_transition(agent);
    if let Some(d) = app.dashboard.as_mut() {
        d.set_peek(None);
    }
    vec![]
}

/// Answer the peeked agent's pending `AskUserQuestion` (the Ask tool) from the dashboard peek panel.
/// `option_idx` selects an option; `None` with a non-empty `freeform` submits the "Other" free-text answer.
/// Delegates to [`AgentView::dashboard_answer_question`], which sends the ext-response; the peek closes once an answer is actually submitted.
pub(super) fn dispatch_dashboard_question_answer(
    app: &mut AppView,
    row: crate::views::dashboard::DashboardRowId,
    option_idx: Option<usize>,
    freeform: String,
) -> Vec<Effect> {
    let target_id = match &row {
        crate::views::dashboard::DashboardRowId::TopLevel(id) => *id,
        crate::views::dashboard::DashboardRowId::Subagent { parent, .. } => *parent,
        crate::views::dashboard::DashboardRowId::Roster { .. }
        | crate::views::dashboard::DashboardRowId::Workspace { .. } => return vec![],
    };
    let Some(agent) = app.agents.get_mut(&target_id) else {
        if let Some(d) = app.dashboard.as_mut() {
            d.set_peek(None);
            d.set_error_toast("Row no longer exists");
        }
        return vec![];
    };
    match agent.dashboard_answer_question(option_idx, freeform) {
        crate::app::agent_view::PeekAnswerOutcome::Submitted => {
            if let Some(d) = app.dashboard.as_mut() {
                d.set_peek(None);
            }
        }

        crate::app::agent_view::PeekAnswerOutcome::Advanced => {
            if let Some(d) = app.dashboard.as_mut() {
                if let Some(p) = d.peek.as_mut() {
                    p.selected_option = None;
                }
                d.clear_peek_reply();
            }
        }

        crate::app::agent_view::PeekAnswerOutcome::NoOp => {}
    }
    vec![]
}
