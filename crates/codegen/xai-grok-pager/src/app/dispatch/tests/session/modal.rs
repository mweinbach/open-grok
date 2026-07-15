//! Tests for session-related modals (extensions, /new worktree question)
//! and session close helpers shared with the dashboard.

use super::*;

#[test]
fn open_extensions_modal_no_session_sets_flag_no_fetches() {
    use crate::views::extensions_modal::ExtensionsTab;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.session_id = None;
    let effects = dispatch(
        Action::OpenExtensionsModal {
            tab: ExtensionsTab::Hooks,
            trigger: xai_grok_telemetry::events::ExtensionsModalTrigger::SlashCommand,
        },
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 0);
    assert!(app.agents[&id].pending_extensions_fetch);
    assert!(app.agents[&id].extensions_modal.is_some());
}

#[test]
fn open_extensions_modal_with_session_emits_fetches_no_flag() {
    use crate::views::extensions_modal::ExtensionsTab;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    let effects = dispatch(
        Action::OpenExtensionsModal {
            tab: ExtensionsTab::Hooks,
            trigger: xai_grok_telemetry::events::ExtensionsModalTrigger::SlashCommand,
        },
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 5);
    assert!(!app.agents[&id].pending_extensions_fetch);
}

#[test]
fn open_extensions_modal_with_session_resets_stale_flag() {
    use crate::views::extensions_modal::ExtensionsTab;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().pending_extensions_fetch = true;
    let effects = dispatch(
        Action::OpenExtensionsModal {
            tab: ExtensionsTab::Hooks,
            trigger: xai_grok_telemetry::events::ExtensionsModalTrigger::SlashCommand,
        },
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 5);
    assert!(!app.agents[&id].pending_extensions_fetch);
}

#[test]
fn session_created_with_flag_but_modal_closed_clears_flag_no_fetches() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let a = app.agents.get_mut(&id).unwrap();
        a.session.session_id = None;
        a.pending_extensions_fetch = true;
        a.extensions_modal = None;
    }
    let effects = dispatch(
        Action::TaskComplete(TaskResult::SessionCreated {
            agent_id: id,
            session_id: acp::SessionId::new("s"),
            models: None,
        }),
        &mut app,
    );
    assert_eq!(count_extension_fetches(&effects), 0);
    assert!(!app.agents[&id].pending_extensions_fetch);
}

// ── /new dispatcher tests ─────────────────────────────────────────────

#[test]
fn dispatch_new_session_opens_question_modal_in_git_repo() {
    let mut app = new_session_test_app();
    app.new_session_worktree_mode = crate::app::app_view::WorktreeMode::Ask;
    let effects = dispatch(Action::NewSession, &mut app);
    assert!(effects.is_empty(), "no effects until modal answered");
    // No new agent yet (creation is deferred until modal answered).
    assert_eq!(app.agents.len(), 1);
    let qv = app.agents[&AgentId(0)]
        .question_view
        .as_ref()
        .expect("modal must be open");
    match qv.local_kind.as_ref().expect("local_kind must be set") {
        crate::views::question_view::LocalQuestionKind::NewSession => {}
        other => panic!("expected NewSession, got {other:?}"),
    }
    assert_eq!(
        qv.questions[0].options.len(),
        4,
        "modal must offer exactly 4 options (Yes/No/Always/Never)"
    );
    let labels: Vec<&str> = qv.questions[0]
        .options
        .iter()
        .map(|o| o.label.as_str())
        .collect();
    assert_eq!(
        labels,
        vec!["Yes", "No", "Always worktree", "Never worktree"]
    );
}

#[test]
fn dispatch_new_session_skips_modal_in_non_git_repo() {
    // current_branch stays None (no git repo) → no modal, straight
    // to dispatch_new_session_inner.
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::NewSession, &mut app);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "non-git path must emit CreateSession, got {effects:?}"
    );
    assert!(
        app.agents.values().all(|a| a.question_view.is_none()),
        "non-git path must not open the modal"
    );
}

// ── Session close (shared with dashboard) ─────────────────────────────

#[test]
fn close_inactive_agent_drops_it() {
    let mut app = three_agent_app();
    let effects = dispatch_sessions_confirm_close(&mut app, AgentId(2));
    assert!(
        effects
            .iter()
            .all(|e| matches!(e, Effect::UnregisterActiveSession { .. }))
    );
    assert!(!app.agents.contains_key(&AgentId(2)));
    assert_eq!(app.agents.len(), 2);
}

#[test]
fn close_agent_releases_retained_memory() {
    use crate::memory_release::test_support;
    test_support::install_counting_hook();

    let mut app = three_agent_app();

    // Dropping a real AgentView (scrollback + caches + child views) → purge.
    let before = test_support::calls();
    dispatch_sessions_confirm_close(&mut app, AgentId(2));
    assert!(!app.agents.contains_key(&AgentId(2)));
    assert_eq!(
        test_support::calls(),
        before + 1,
        "dropping the closed AgentView must purge retained pages"
    );

    // Closing an unknown agent drops nothing → no purge.
    let before = test_support::calls();
    dispatch_sessions_confirm_close(&mut app, AgentId(999));
    assert_eq!(
        test_support::calls(),
        before,
        "a no-op close must not purge"
    );
}

#[test]
fn close_clears_forked_from_on_surviving_children() {
    let mut app = three_agent_app();
    set_forked_from(&mut app, AgentId(2), AgentId(1));
    dispatch_sessions_confirm_close(&mut app, AgentId(1));
    assert!(
        app.agents[&AgentId(2)].session.forked_from.is_none(),
        "stale forked_from pointer must be cleared after parent close"
    );
}

#[test]
fn close_only_agent_is_refused_with_toast() {
    let mut app = test_app_with_agent();
    let agents_before = app.agents.len();
    dispatch_sessions_confirm_close(&mut app, AgentId(0));
    assert_eq!(
        app.agents.len(),
        agents_before,
        "the only agent must NOT be closed"
    );
}

#[test]
fn close_unknown_agent_is_silent_noop() {
    let mut app = three_agent_app();
    let agents_before = app.agents.len();
    dispatch_sessions_confirm_close(&mut app, AgentId(999));
    assert_eq!(app.agents.len(), agents_before);
}

#[test]
fn close_only_agent_short_circuits_before_reaching_welcome_fallback() {
    let mut app = test_app_with_agent();
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == AgentId(0)));
    dispatch_sessions_confirm_close(&mut app, AgentId(0));
    assert!(matches!(app.active_view, ActiveView::Agent(id) if id == AgentId(0)));
    assert!(app.agents.contains_key(&AgentId(0)));
}

#[test]
fn close_does_not_disturb_unrelated_forked_from_pointers() {
    let mut app = three_agent_app();
    set_forked_from(&mut app, AgentId(1), AgentId(0));
    set_forked_from(&mut app, AgentId(2), AgentId(0));
    dispatch_sessions_confirm_close(&mut app, AgentId(1));
    assert_eq!(
        app.agents[&AgentId(2)].session.forked_from,
        Some(AgentId(0)),
        "unrelated forked_from must NOT be cleared"
    );
}

#[test]
fn extensions_modal_in_non_project_dir_creates_session() {
    let mut app = project_picker_app();
    dispatch(Action::NewSession, &mut app);
    let id = AgentId(0);

    let effects = dispatch(
        Action::OpenExtensionsModal {
            tab: crate::views::extensions_modal::ExtensionsTab::McpServers,
            trigger: xai_grok_telemetry::events::ExtensionsModalTrigger::SlashCommand,
        },
        &mut app,
    );

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CreateSession { .. })),
        "session-less modal open must create the deferred session"
    );
    assert!(app.agents[&id].pending_extensions_fetch);
}
