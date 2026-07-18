//! Login, logout, account switching, and auth-code submission dispatchers.

use super::ctx::{get_visible_agent_mut, restore_auth_return_view, show_welcome};
use super::queue::{maybe_drain_queue, note_peek_page_flip_after_drain};
use super::router::dispatch;
use super::session::lifecycle::{clear_startup_actions, drain_startup_actions};
use crate::app::actions::{Action, CodexLoginPurpose, Effect, ProviderSessionTarget};
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::{
    ActiveView, AppView, AuthMode, AuthState, CODEX_STARTUP_MODEL_ID, PrimaryProvider, TrustState,
    XAI_STARTUP_MODEL_ID,
};
use crate::scrollback::block::RenderBlock;
use crate::scrollback::blocks::SessionEvent;

// ---------------------------------------------------------------------------
// Auth dispatch
// ---------------------------------------------------------------------------

/// Open the provider chooser for a bare `/login` without starting any auth
/// flow. Startup and re-auth callers continue to dispatch concrete `Login`
/// directly. The session-less dashboard reopens the shared slash provider
/// completion so the same xAI, Codex, and Kimi choices remain available before
/// an agent exists.
pub(super) fn dispatch_open_login_provider_picker(app: &mut AppView) -> Vec<Effect> {
    let kimi_status = super::settings::ui::kimi_api_key_status();
    let items = crate::slash::commands::login::provider_items(Some(kimi_status));
    if let Some(agent) = get_visible_agent_mut(app) {
        agent.active_modal = Some(crate::views::modal::ActiveModal::ArgPicker {
            command: "login".to_owned(),
            args_query: String::new(),
            items: items.clone(),
            original_items: items,
            state: crate::views::picker::PickerState::input_active(),
            previous_palette: None,
            window: crate::views::modal_window::ModalWindowState::new(),
        });
        vec![]
    } else if let Some(dashboard) = app.dashboard.as_mut()
        && matches!(app.active_view, ActiveView::AgentDashboard)
    {
        dashboard.search_mode = false;
        dashboard.list_focused = false;
        dashboard.error_toast = None;
        dashboard.dispatch.set_text("/login ");
        let end = dashboard.dispatch.text().len();
        dashboard.dispatch.textarea.set_cursor(end);
        dashboard.dispatch.refresh_slash(&dashboard.models);
        vec![]
    } else {
        dispatch_login(app)
    }
}

fn snapshot_provider_sessions(
    app: &AppView,
    provider: PrimaryProvider,
) -> Vec<ProviderSessionTarget> {
    app.agents
        .iter()
        .filter_map(|(id, agent)| {
            (PrimaryProvider::for_current_model(&agent.session.models) == Some(provider)).then(
                || ProviderSessionTarget {
                    agent_id: *id,
                    session_id: agent.session.session_id.clone(),
                },
            )
        })
        .collect()
}

/// `/logout` revokes xAI only. Snapshot both providers before the shell's
/// model-update broadcast so completion deterministically closes xAI tabs and
/// preserves authenticated Codex tabs regardless of notification ordering.
pub(super) fn dispatch_logout(app: &mut AppView) -> Vec<Effect> {
    let mut xai_targets = snapshot_provider_sessions(app, PrimaryProvider::Xai);
    let codex_targets = snapshot_provider_sessions(app, PrimaryProvider::Codex);
    if app.primary_provider == PrimaryProvider::Xai
        && let ActiveView::Agent(id) = app.active_view
        && !xai_targets.iter().any(|target| target.agent_id == id)
    {
        xai_targets.push(ProviderSessionTarget {
            agent_id: id,
            session_id: app
                .agents
                .get(&id)
                .and_then(|agent| agent.session.session_id.clone()),
        });
    }
    vec![Effect::Logout {
        xai_targets,
        codex_targets,
    }]
}

/// `/login codex` -- connect the independent OpenAI account without changing
/// the xAI ACP authentication state or leaving the current session.
pub(super) fn dispatch_login_codex(app: &mut AppView) -> Vec<Effect> {
    let agent_id = match app.active_view {
        ActiveView::Agent(id) => Some(id),
        _ => None,
    };
    if let Some(agent_id) = agent_id
        && let Some(agent) = app.agents.get_mut(&agent_id)
    {
        agent.scrollback.push_block(RenderBlock::system(
            "Opening your browser to connect OpenAI Codex…",
        ));
    }
    vec![Effect::LoginCodex {
        agent_id,
        purpose: CodexLoginPurpose::Independent,
    }]
}

/// Select and persist a visible model paired with a first-run provider.
///
/// The ACP catalog is already filtered for authentication, hidden models, and
/// user allow/deny rules. Never synthesize a preferred ID that is absent from
/// that catalog, and never cross the provider boundary as a fallback.
fn select_startup_model(
    app: &mut AppView,
    provider: PrimaryProvider,
    preferred: &str,
    allow_provider_fallback: bool,
) -> Result<Option<Effect>, String> {
    let preferred_id = agent_client_protocol::ModelId::new(preferred.to_owned());
    let model_id = if PrimaryProvider::for_model(&app.models, &preferred_id) == Some(provider) {
        Some(preferred_id)
    } else if allow_provider_fallback {
        provider.startup_model(&app.models, preferred)
    } else {
        None
    };
    let Some(model_id) = model_id else {
        app.startup_model_override = None;
        let provider_name = match provider {
            PrimaryProvider::Codex => "ChatGPT Codex",
            PrimaryProvider::Xai => "xAI Grok",
            PrimaryProvider::Kimi => "Kimi",
        };
        return Err(if allow_provider_fallback {
            format!("No visible {provider_name} model is available. Check model filters and retry.")
        } else {
            format!(
                "Requested {provider_name} model '{preferred}' is not visible. Check model filters and retry."
            )
        });
    };
    let previous = app
        .models
        .current
        .as_ref()
        .map(|id| id.0.to_string())
        .unwrap_or_default();
    app.models.set_current(model_id.clone(), None);
    app.startup_model_override = Some(model_id.clone());
    // A post-auth catalog refresh may already report this model as current,
    // but the provider choice still needs to become the persisted default.
    Ok(Some(Effect::PersistSetting {
        key: "default_model",
        value: crate::settings::SettingValue::String(model_id.0.to_string()),
        rollback_value: crate::settings::SettingValue::String(previous),
    }))
}

/// First-run ChatGPT choice. This uses the pager-owned Codex OAuth flow and
/// deliberately does not mutate the xAI ACP authentication state.
pub(super) fn dispatch_choose_startup_codex(app: &mut AppView) -> Vec<Effect> {
    if !matches!(app.auth_state, AuthState::ProviderChoice { .. }) {
        return vec![];
    }
    if app.codex_resume_auth_pending {
        return dispatch_codex_session_resume_auth(app);
    }
    let request_seq = app.next_auth_request_seq;
    app.next_auth_request_seq += 1;
    app.startup_provider_selection = Some(PrimaryProvider::Codex);
    app.primary_provider = PrimaryProvider::Codex;
    app.clear_xai_access_controls();
    app.auth_code_input.clear();
    app.welcome_menu_index = None;
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: AuthMode::Command,
    };
    if let Some(account) = app.startup_codex_account.clone() {
        return handle_codex_startup_complete(app, request_seq, Ok(account), None);
    }
    vec![Effect::LoginCodex {
        agent_id: None,
        purpose: CodexLoginPurpose::Startup { request_seq },
    }]
}

/// Authenticate only to resume an existing Codex session. Unlike first-run
/// onboarding, this path must not select or persist a new default model.
pub(in crate::app::dispatch) fn dispatch_codex_session_resume_auth(
    app: &mut AppView,
) -> Vec<Effect> {
    let request_seq = app.next_auth_request_seq;
    app.next_auth_request_seq += 1;
    app.codex_resume_auth_pending = true;
    // The minimal TUI uses this explicit flow owner for its auth label; the
    // distinct SessionResume purpose still prevents startup model selection.
    app.startup_provider_selection = Some(PrimaryProvider::Codex);
    app.primary_provider = PrimaryProvider::Codex;
    app.clear_xai_access_controls();
    app.auth_code_input.clear();
    app.welcome_menu_index = None;
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: AuthMode::Command,
    };
    vec![Effect::LoginCodex {
        agent_id: None,
        purpose: CodexLoginPurpose::SessionResume { request_seq },
    }]
}

/// First-run xAI choice. Authentication itself remains on the existing ACP
/// path. Model selection is intentionally deferred until `AuthComplete`, whose
/// response carries the post-auth provider-filtered catalog; before login the
/// visible ACP catalog may contain no xAI models at all.
pub(super) fn dispatch_choose_startup_xai(app: &mut AppView) -> Vec<Effect> {
    if !matches!(app.auth_state, AuthState::ProviderChoice { .. }) {
        return vec![];
    }
    if app.codex_resume_auth_pending {
        // Choosing xAI explicitly abandons the pending Codex-only resume
        // instead of retrying that same session after xAI auth completes.
        app.codex_resume_auth_pending = false;
        app.deferred_startup.session = None;
    }
    app.startup_provider_selection = Some(PrimaryProvider::Xai);
    app.primary_provider = PrimaryProvider::Xai;
    app.welcome_menu_index = None;

    // ACP may already have a valid xAI credential when the chooser was shown
    // solely because the persisted Codex default was not authenticated. Reuse
    // that live xAI auth instead of needlessly opening another browser flow.
    if app.startup_xai_ready {
        let request_seq = app.next_auth_request_seq;
        app.next_auth_request_seq += 1;
        app.is_api_key_auth = app.auth_methods.iter().any(|method| {
            method.id().0.as_ref() == xai_grok_shell::agent::auth_method::XAI_API_KEY_METHOD_ID
        });
        if app.is_api_key_auth {
            app.apply_usage_visibility(false);
            app.ensure_voice_for_api_key();
        }
        app.auth_state = AuthState::Authenticating {
            request_seq,
            handle: None,
            auth_url: None,
            mode: app.auth_start_mode,
        };
        let meta = app.startup_xai_auth_meta.clone();
        return handle_auth_complete(app, request_seq, meta);
    }

    app.auth_state = AuthState::Pending { error: None };
    let login_effects = dispatch_login(app);
    if login_effects.is_empty()
        && let AuthState::Pending { error: Some(error) } = &app.auth_state
    {
        app.auth_state = AuthState::ProviderChoice {
            error: Some(format!("xAI Grok login unavailable: {error}")),
        };
        app.startup_provider_selection = None;
        app.welcome_menu_index = Some(0);
    }
    login_effects
}

/// Complete the startup-only Codex OAuth path without entering any xAI
/// subscription, billing, ZDR, or account gates.
pub(super) fn handle_codex_startup_complete(
    app: &mut AppView,
    request_seq: u64,
    result: Result<xai_grok_shell::codex_auth::CodexAccountSummary, String>,
    models: Option<agent_client_protocol::SessionModelState>,
) -> Vec<Effect> {
    if !matches!(
        app.auth_state,
        AuthState::Authenticating {
            request_seq: current,
            ..
        } if current == request_seq
    ) {
        return vec![];
    }

    let account = match result {
        Ok(account) => account,
        Err(error) => {
            app.startup_provider_selection = None;
            app.auth_state = AuthState::ProviderChoice {
                error: Some(format!("ChatGPT Codex login failed: {error}")),
            };
            app.welcome_prompt_focused = false;
            app.welcome_menu_index = Some(0);
            return vec![];
        }
    };

    if let Some(models) = models {
        app.models = Some(models).into();
    }

    // The post-login ACP refresh publishes an auth-filtered catalog before
    // this completion is normally handled. If the selected provider still has
    // no visible model (for example because an allowlist filtered all of them),
    // return to the chooser instead of fabricating Sol or selecting xAI.
    let requested_model = app
        .cli_model_override
        .as_ref()
        .or(app.startup_model_override.as_ref())
        .map(|model| model.0.to_string());
    let preferred_model = requested_model.as_deref().unwrap_or(CODEX_STARTUP_MODEL_ID);
    let startup_model_effect = match select_startup_model(
        app,
        PrimaryProvider::Codex,
        preferred_model,
        app.cli_model_override.is_none(),
    ) {
        Ok(effect) => effect,
        Err(error) => {
            app.startup_codex_account = Some(account);
            app.sync_usage_command_visibility();
            app.startup_provider_selection = None;
            app.primary_provider = PrimaryProvider::Codex;
            app.clear_xai_access_controls();
            app.auth_state = AuthState::ProviderChoice { error: Some(error) };
            app.welcome_prompt_focused = false;
            app.welcome_menu_index = Some(0);
            return vec![];
        }
    };

    app.startup_codex_account = Some(account.clone());
    app.sync_usage_command_visibility();
    app.primary_provider = PrimaryProvider::Codex;
    app.startup_provider_selection = None;
    app.clear_xai_access_controls();
    app.auth_state = AuthState::Done;
    app.auth_show_raw_url = false;
    app.auth_code_input.clear();
    app.welcome_prompt_focused = matches!(app.trust_state, TrustState::Done);
    app.welcome_menu_index = None;

    let mut details = Vec::new();
    if let Some(email) = account.email.filter(|value| !value.trim().is_empty()) {
        details.push(email);
    }
    if let Some(plan) = account.plan_type.filter(|value| !value.trim().is_empty()) {
        details.push(format!("{plan} plan"));
    }
    let connected = if details.is_empty() {
        "ChatGPT Codex connected.".to_string()
    } else {
        format!("ChatGPT Codex connected: {}.", details.join(" · "))
    };
    app.show_toast(&connected);

    let mut effects = startup_model_effect.into_iter().collect::<Vec<_>>();
    effects.extend(dispatch(Action::RequestBundleStatus, app));
    effects.push(Effect::FetchChangelog);
    if app.session_startup_allowed() {
        effects.extend(drain_startup_actions(app));
    }
    effects
}

/// Finish auth for a deferred persisted-session load without changing the
/// configured/default model. The refreshed catalog is installed only so the
/// shell and pager agree when the exact session is retried.
pub(super) fn handle_codex_session_resume_complete(
    app: &mut AppView,
    request_seq: u64,
    result: Result<xai_grok_shell::codex_auth::CodexAccountSummary, String>,
    models: Option<agent_client_protocol::SessionModelState>,
) -> Vec<Effect> {
    if !matches!(
        app.auth_state,
        AuthState::Authenticating {
            request_seq: current,
            ..
        } if current == request_seq
    ) {
        return vec![];
    }

    let account = match result {
        Ok(account) => account,
        Err(error) => {
            app.auth_state = AuthState::ProviderChoice {
                error: Some(format!("ChatGPT Codex login failed: {error}")),
            };
            app.welcome_prompt_focused = false;
            app.welcome_menu_index = Some(0);
            return vec![];
        }
    };

    if let Some(models) = models {
        app.models = Some(models).into();
    }
    app.startup_codex_account = Some(account);
    app.sync_usage_command_visibility();
    app.startup_provider_selection = None;
    app.startup_model_override = None;
    app.codex_resume_auth_pending = false;
    app.primary_provider = PrimaryProvider::Codex;
    app.clear_xai_access_controls();
    app.auth_state = AuthState::Done;
    app.auth_show_raw_url = false;
    app.auth_code_input.clear();
    app.welcome_prompt_focused = matches!(app.trust_state, TrustState::Done);
    app.welcome_menu_index = None;
    app.show_toast("ChatGPT Codex connected. Resuming session…");

    let mut effects = dispatch(Action::RequestBundleStatus, app);
    effects.push(Effect::FetchChangelog);
    if app.session_startup_allowed() {
        effects.extend(drain_startup_actions(app));
    }
    effects
}

/// `/logout codex` -- revoke only the independent OpenAI account.
pub(super) fn dispatch_logout_codex(app: &mut AppView) -> Vec<Effect> {
    let primary_was_codex = app.primary_provider == PrimaryProvider::Codex;
    let agent_id = match app.active_view {
        ActiveView::Agent(id) => Some(id),
        _ => None,
    };
    let mut targets = snapshot_provider_sessions(app, PrimaryProvider::Codex);
    if primary_was_codex
        && let Some(id) = agent_id
        && !targets.iter().any(|target| target.agent_id == id)
    {
        targets.push(ProviderSessionTarget {
            agent_id: id,
            session_id: app
                .agents
                .get(&id)
                .and_then(|agent| agent.session.session_id.clone()),
        });
    }
    vec![Effect::LogoutCodex {
        agent_id,
        targets,
        primary_was_codex,
    }]
}

/// Ensure `login_method_id` is populated from stored auth methods.
/// On the eager-auth path (cached token), login_method_id is never set
/// because the user skipped the login screen.
///
/// Does **not** invent `grok.com` when no interactive method is advertised
/// (e.g. `preferred_method=api_key` with no key — empty `auth_methods`).
/// Callers already surface "No login method available" when this leaves
/// `login_method_id` unset.
pub(super) fn ensure_login_method(app: &mut AppView) {
    if app.login_method_id.is_some() {
        return;
    }
    let (label, method_id, start_mode) =
        crate::acp::find_interactive_login_method(&app.auth_methods);
    if let Some(id) = method_id {
        app.login_label = label;
        app.login_method_id = Some(id);
        app.auth_start_mode = match start_mode {
            crate::acp::AuthStartMode::Pending => AuthMode::Pending,
            crate::acp::AuthStartMode::Command => AuthMode::Command,
        };
    }
    // No interactive method: leave login_method_id unset (fail-closed).
}

/// Error when no interactive login method is available (empty auth_methods,
/// e.g. `preferred_method=api_key` with no credentials). Prefer the shell's
/// pin-unavailable copy when the list is empty.
fn no_login_method_error(app: &AppView) -> String {
    if app.auth_methods.is_empty() {
        xai_grok_shell::agent::auth_method::PREFERRED_API_KEY_UNAVAILABLE.to_string()
    } else {
        "No login method available".to_string()
    }
}

/// Abort any shell-owned xAI Authenticate/SwitchAccount task and its URL poll
/// so a new xAI login cannot stack device-code mints or let a stale poll steal
/// its successor's URL. Codex OAuth never installs either of these handles.
fn abort_prior_xai_auth(app: &mut AppView) {
    if let AuthState::Authenticating {
        handle,
        request_seq,
        ..
    } = &mut app.auth_state
        && let Some(handle) = handle.take()
    {
        tracing::debug!(
            request_seq,
            "aborting prior in-flight xAI auth task for single-flight"
        );
        handle.abort();
    }
    if let Some((request_seq, handle)) = app.auth_url_poll_handle.take() {
        tracing::debug!(
            request_seq,
            "aborting prior xAI auth URL poll for single-flight"
        );
        handle.abort();
    }
}

/// Log out, then start a new login flow in a single sequential task.
pub(super) fn dispatch_switch_account(app: &mut AppView) -> Vec<Effect> {
    ensure_login_method(app);

    let Some(method_id) = app.login_method_id.clone() else {
        app.auth_state = AuthState::Pending {
            error: Some(no_login_method_error(app)),
        };
        return vec![];
    };

    abort_prior_xai_auth(app);

    let request_seq = app.next_auth_request_seq;
    app.next_auth_request_seq += 1;
    app.auth_code_input.clear();
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: app.auth_start_mode,
    };

    vec![
        Effect::SwitchAccount {
            request_seq,
            method_id,
            use_oauth: app.auth_use_oauth,
        },
        Effect::PollAuthUrl { request_seq },
    ]
}

/// Scan the trailing run of session-event / system blocks for a
/// [`SessionEvent::ReAuthRequired`] prompt. Used by the `PromptResponse`
/// handler to suppress the redundant "Turn failed" block after a 401 — the
/// re-auth prompt is pushed by the `RetryState` handler, which runs first.
pub(super) fn scrollback_has_recent_reauth_prompt(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    use crate::scrollback::block::RenderBlock;
    for idx in (0..scrollback.len()).rev() {
        match scrollback.entry(idx).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev)) => {
                if matches!(ev.event, SessionEvent::ReAuthRequired) {
                    return true;
                }
            }
            // Tolerate interleaved system messages in the trailing run.
            Some(RenderBlock::System(_)) => {}
            // Stop at the first substantive block: any re-auth prompt for
            // this turn lives in the trailing events pushed just before the
            // PromptResponse arrived.
            _ => break,
        }
    }
    false
}

/// True if the trailing run of session/system blocks contains a terminal
/// context-overflow block ([`SessionEvent::ContextTooLarge`] or `CompactionFailed`).
/// Lets `PromptResponse` suppress the redundant `TurnFailed`, mirroring reauth.
pub(super) fn scrollback_has_recent_context_too_large(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    use crate::scrollback::block::RenderBlock;
    for idx in (0..scrollback.len()).rev() {
        match scrollback.entry(idx).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev)) => {
                if matches!(
                    ev.event,
                    SessionEvent::ContextTooLarge | SessionEvent::CompactionFailed { .. }
                ) {
                    return true;
                }
            }
            // Tolerate interleaved system messages in the trailing run.
            Some(RenderBlock::System(_)) => {}
            // Stop at the first substantive block.
            _ => break,
        }
    }
    false
}

/// Strip the trailing run of auth-error blocks — the `ReAuthRequired`
/// prompt plus any stale `RetryFailed` / `TurnFailed` — from an agent's
/// scrollback. Called after a successful mid-session re-auth so the prompt
/// disappears once the user returns to the session. Mirrors the
/// credit-limit upsell's stale-block strip.
pub(super) fn strip_trailing_auth_error_blocks(agent: &mut AgentView) {
    use crate::scrollback::block::RenderBlock;
    let mut to_remove = Vec::new();
    for idx in (0..agent.scrollback.len()).rev() {
        match agent.scrollback.entry(idx).map(|e| &e.block) {
            Some(RenderBlock::SessionEvent(ev))
                if matches!(
                    &ev.event,
                    SessionEvent::ReAuthRequired
                        | SessionEvent::RetryFailed { .. }
                        | SessionEvent::TurnFailed { .. }
                ) =>
            {
                to_remove.push(idx);
            }
            // Skip over other trailing session-event / system blocks.
            Some(RenderBlock::SessionEvent(_) | RenderBlock::System(_)) => continue,
            // Stop at the first substantive block.
            _ => break,
        }
    }
    for idx in to_remove {
        agent.scrollback.remove_from(idx);
    }
}

/// Start an interactive login flow. Triggered by pressing 'l' on the
/// welcome screen or by the `/login` slash command.
///
/// When invoked mid-session (the active view is an agent/dashboard rather
/// than the welcome screen), the auth UI — including the external auth
/// provider's sign-in URL and status — is only rendered by the welcome
/// view. We therefore stash the caller's view in `auth_return_view` and
/// switch to `Welcome` so the flow is actually visible; the prior view is
/// restored once auth completes or is cancelled. Without this, `/login`
/// with an external auth provider configured appeared to do nothing.
pub(super) fn dispatch_login(app: &mut AppView) -> Vec<Effect> {
    ensure_login_method(app);
    let Some(method_id) = app.login_method_id.clone() else {
        app.auth_state = AuthState::Pending {
            error: Some(no_login_method_error(app)),
        };
        return vec![];
    };

    // Surface the auth UI when triggered from inside a session. `show_welcome`
    // resets ephemeral state here, covering the AuthComplete / cancel-login
    // fallbacks too (`auth_return_view` is only ever set here).
    if !matches!(app.active_view, ActiveView::Welcome) {
        app.auth_return_view = Some(app.active_view);
        show_welcome(app);
    }

    abort_prior_xai_auth(app);

    let request_seq = app.next_auth_request_seq;
    app.next_auth_request_seq += 1;
    app.auth_code_input.clear();
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: app.auth_start_mode,
    };

    vec![
        Effect::Authenticate {
            request_seq,
            method_id,
            use_oauth: app.auth_use_oauth,
            force_interactive: true,
        },
        Effect::PollAuthUrl { request_seq },
    ]
}

/// Cancel a login that was started from inside a session and restore the
/// caller's view. Only meaningful when `auth_return_view` is set (a
/// mid-session xAI `/login` or 401 re-auth prompt). Aborts the in-flight xAI
/// task and tells the shell to cancel its device/loopback flow so a retry does
/// not race a still-polling prior mint. Codex OAuth never sets
/// `auth_return_view`, so it cannot emit the xAI-only cancellation effect.
pub(super) fn dispatch_cancel_login(app: &mut AppView) -> Vec<Effect> {
    let Some(return_view) = app.auth_return_view.take() else {
        return vec![];
    };
    // Capture the attempt before aborting it so the shell cancel is scoped to
    // this xAI attempt only. A delayed RPC must not cancel a fast re-login.
    let cancel_seq = match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => Some(*request_seq),
        _ => None,
    };
    abort_prior_xai_auth(app);
    app.next_auth_request_seq += 1;
    app.auth_state = AuthState::Done;
    app.auth_show_raw_url = false;
    app.auth_code_input.clear();
    restore_auth_return_view(app, return_view);
    // The user bailed out of re-auth — drop stashed prompts and strip the
    // stale re-auth prompt from scrollback (on all agents: the login may
    // have been started from the dashboard). Clearing the stash alone is
    // not enough: a leftover `ReAuthRequired` block would let a later
    // `PromptResponse` re-detect it via `scrollback_has_recent_reauth_prompt`
    // and re-stash the prompt, so a subsequent unrelated login could
    // silently resubmit it. Mirrors the strip in the `AuthComplete` path.
    for agent in app.agents.values_mut() {
        agent.reauth_stashed_prompt = None;
        strip_trailing_auth_error_blocks(agent);
    }
    match cancel_seq {
        Some(request_seq) => vec![Effect::CancelAuth { request_seq }],
        None => vec![],
    }
}

/// User submitted a manually-pasted auth token in loopback mode.
pub(super) fn dispatch_submit_auth_code(app: &mut AppView, code: String) -> Vec<Effect> {
    let request_seq = match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => *request_seq,
        _ => return vec![],
    };

    vec![Effect::SubmitAuthCode { request_seq, code }]
}

// TaskResult handlers.

pub(super) fn handle_auth_complete(
    app: &mut AppView,
    request_seq: u64,
    meta: Option<serde_json::Value>,
) -> Vec<Effect> {
    if let AuthState::Authenticating {
        request_seq: current_seq,
        ..
    } = &app.auth_state
        && *current_seq == request_seq
    {
        app.startup_xai_ready = true;
        let mut auth_meta_value = meta;
        let response_models = auth_meta_value
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|meta| meta.remove("models"))
            .and_then(|models| {
                serde_json::from_value::<agent_client_protocol::SessionModelState>(models).ok()
            });
        if let Some(meta_val) = auth_meta_value.as_ref() {
            app.startup_xai_auth_meta = Some(meta_val.clone());
            if app.uses_xai_access_controls()
                && let Ok(auth_meta) =
                    serde_json::from_value::<xai_grok_shell::auth::AuthMeta>(meta_val.clone())
            {
                app.apply_auth_meta(&auth_meta);
            }
        }

        let startup_model_effect = if app.startup_provider_selection == Some(PrimaryProvider::Xai) {
            if let Some(models) = response_models {
                app.models = Some(models).into();
            }
            let requested_model = app
                .cli_model_override
                .as_ref()
                .or(app.startup_model_override.as_ref())
                .map(|model| model.0.to_string());
            let preferred_model = requested_model.as_deref().unwrap_or(XAI_STARTUP_MODEL_ID);
            match select_startup_model(
                app,
                PrimaryProvider::Xai,
                preferred_model,
                app.cli_model_override.is_none(),
            ) {
                Ok(effect) => {
                    app.startup_provider_selection = None;
                    effect
                }
                Err(error) => {
                    app.startup_provider_selection = None;
                    app.clear_xai_access_controls();
                    app.auth_state = AuthState::ProviderChoice { error: Some(error) };
                    app.welcome_prompt_focused = false;
                    app.welcome_menu_index = Some(1);
                    return vec![];
                }
            }
        } else {
            None
        };

        app.auth_state = AuthState::Done;
        app.auth_show_raw_url = false;
        app.welcome_prompt_focused = !app.is_access_blocked();
        app.auth_code_input.clear();

        // Mid-session re-auth (`/login` or a 401 prompt): restore the
        // view the user was on instead of running the startup
        // load-session flow. The session state lives in `app.agents`,
        // independent of `active_view`, so it is preserved across the
        // auth detour.
        if let Some(return_view) = app.auth_return_view.take() {
            restore_auth_return_view(app, return_view);
            // Mid-session re-auth returns to the existing session, NOT
            // the startup flow, so discard any deferred startup stash
            // (e.g. an incidental `Ctrl+N` pressed during /login that the
            // chokepoint deferred) rather than leaving it to fire later.
            clear_startup_actions(app);
            // Re-auth succeeded — hide the now-stale re-auth prompt
            // (and any trailing error blocks) so the user returns to
            // a clean session. Mirrors the credit-limit upsell's
            // stale-block strip.
            // Auth is global, so handle every agent (the login may
            // have been started from the dashboard, not the agent
            // that 401'd).
            let mut retry_effects = Vec::new();
            let mut drained_ids = Vec::new();
            for agent in app.agents.values_mut() {
                strip_trailing_auth_error_blocks(agent);
                // Auto-resubmit the prompt that failed on the expired
                // login so the user doesn't have to retype it. The
                // user couldn't have queued another prompt during the
                // auth detour, so a plain front-enqueue + drain is safe.
                if let Some(prompt) = agent.reauth_stashed_prompt.take() {
                    agent.scrollback.push_block(RenderBlock::system(
                        "Re-authenticated. Retrying\u{2026}".to_string(),
                    ));
                    agent.session.enqueue_in_flight_prompt_front(prompt);
                    retry_effects.extend(maybe_drain_queue(agent));
                    drained_ids.push(agent.session.id);
                }
            }
            for id in drained_ids {
                note_peek_page_flip_after_drain(app, id);
            }
            let mut effects = startup_model_effect.into_iter().collect::<Vec<_>>();
            effects.extend(dispatch(Action::RequestBundleStatus, app));
            if app.uses_xai_access_controls() && app.usage_visible {
                effects.push(Effect::FetchAppBilling);
            }
            effects.extend(retry_effects);
            return effects;
        }

        // status only; shell auto-syncs post-auth
        let mut effects = startup_model_effect.into_iter().collect::<Vec<_>>();
        effects.extend(dispatch(Action::RequestBundleStatus, app));

        // Start auto-checking subscription if gated.
        // Check immediately (don't wait 5s) then schedule the timer.
        if app.uses_xai_access_controls() && !app.has_access() {
            app.paywall_check_started = Some(std::time::Instant::now());
            effects.push(Effect::CheckSubscription { verify: None });
            effects.push(Effect::SchedulePaywallCheck);
        }
        // Fetch billing so the welcome screen can show a credit warning.
        if app.uses_xai_access_controls() && app.usage_visible {
            effects.push(Effect::FetchAppBilling);
        }
        // Fetch changelog (mirrors startup path for interactive login).
        effects.push(Effect::FetchChangelog);

        // ZDR-blocked users stay on the welcome screen — discard any
        // deferred startup (they cannot start a session).
        if app.is_zdr_blocked() {
            clear_startup_actions(app);
            return effects;
        }

        // Replay deferred session startup once BOTH gates are open. Auth
        // is now Done, so `session_startup_allowed()` here means "is trust
        // also resolved?" -- if trust is still Pending its question renders
        // next and its answer drains instead. Same predicate the trust
        // handlers use, so the deferred startup runs exactly once after
        // whichever gate resolves last.
        if app.session_startup_allowed() {
            effects.extend(drain_startup_actions(app));
        }
        return effects;
    }
    vec![]
}

pub(super) fn handle_auth_url_ready(
    app: &mut AppView,
    request_seq: u64,
    auth_url: Option<String>,
    external: bool,
    mode: Option<String>,
) -> Vec<Effect> {
    if let AuthState::Authenticating {
        request_seq: current_seq,
        auth_url: current_url,
        mode: current_mode,
        ..
    } = &mut app.auth_state
        && *current_seq == request_seq
    {
        *current_url = auth_url;
        // Prefer `mode`; fall back to `external` for older agents. An
        // old-agent device login lands on Loopback (harmless paste box;
        // the background poll still completes).
        *current_mode = match mode.as_deref() {
            Some("device") => AuthMode::Device,
            Some("command") => AuthMode::Command,
            Some("loopback") => AuthMode::Loopback,
            _ if external => AuthMode::Command,
            _ => AuthMode::Loopback,
        };
    }
    vec![]
}

pub(super) fn handle_mcp_auth_trigger_done(
    app: &mut AppView,
    agent_id: AgentId,
    server_name: String,
    result: Result<crate::app::actions::McpAuthTriggerOutcome, String>,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    if let Some(ref mut modal) = agent.extensions_modal {
        modal.pending_action = None;
        modal.pending_entry_index = None;
        match result {
            Ok(crate::app::actions::McpAuthTriggerOutcome::Authenticated) => {}
            Ok(crate::app::actions::McpAuthTriggerOutcome::SetupRequired(setup)) => {
                let setup_values = match &modal.mcps_data {
                    crate::views::extensions_modal::TabDataState::Loaded(servers) => servers
                        .iter()
                        .find(|server| server.name == server_name)
                        .map(|server| server.setup_values.clone())
                        .unwrap_or_default(),
                    _ => std::collections::HashMap::new(),
                };
                if let Some(form) = crate::views::extensions_modal::McpSetupFormState::from_setup(
                    server_name.clone(),
                    setup,
                    setup_values,
                ) {
                    modal.mcp_setup = Some(form);
                } else {
                    modal.modal_message =
                        Some(crate::views::extensions_modal::ModalMessage::Error(
                            format!("{server_name}: setup schema is not supported in this UI"),
                        ));
                }
                return vec![];
            }
            Err(error) => {
                let message = if error.starts_with("To authenticate") {
                    format!("{server_name}: {error}")
                } else if error.contains(&server_name) {
                    format!("Auth failed: {error}")
                } else {
                    format!("{server_name} auth failed: {error}")
                };
                modal.modal_message =
                    Some(crate::views::extensions_modal::ModalMessage::Error(message));
                if let Some(session_id) = agent.session.session_id.clone() {
                    return vec![Effect::FetchMcpsList {
                        agent_id,
                        session_id,
                        cache: false,
                    }];
                }
                return vec![];
            }
        }
    }
    // No toast on success: the row transition from the FetchMcpsList
    // refresh below is the confirmation.
    let Some(session_id) = agent.session.session_id.clone() else {
        return vec![];
    };
    vec![Effect::FetchMcpsList {
        agent_id,
        session_id,
        cache: false,
    }]
}

pub(super) fn handle_mcp_setup_submit_done(
    app: &mut AppView,
    agent_id: AgentId,
    server_name: String,
    result: Result<(), String>,
) -> Vec<Effect> {
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    if let Some(ref mut modal) = agent.extensions_modal {
        if let Err(error) = result {
            modal.pending_action = None;
            modal.pending_entry_index = None;
            modal.modal_message = Some(crate::views::extensions_modal::ModalMessage::Error(
                format!("{server_name} setup failed: {error}"),
            ));
            return vec![];
        }
        modal.pending_action = Some(format!("Authenticating {server_name}..."));
        modal.pending_entry_index = None;
    }
    let Some(session_id) = agent.session.session_id.clone() else {
        if let Some(ref mut modal) = agent.extensions_modal {
            modal.pending_action = None;
            modal.modal_message = Some(crate::views::extensions_modal::ModalMessage::Error(
                format!("{server_name}: no active session for authentication"),
            ));
        }
        return vec![];
    };
    vec![Effect::McpAuthTrigger {
        agent_id,
        session_id,
        server_name,
    }]
}
