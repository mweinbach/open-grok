//! Tests for login, logout, account switching, and auth-code dispatchers.

use super::*;
use crate::app::actions::ProviderSessionTarget;

fn add_provider_model(app: &mut AppView, model: &str, provider: &str) -> acp::ModelId {
    let model_id = acp::ModelId::new(model.to_string());
    let mut meta = acp::Meta::new();
    meta.insert("provider".to_string(), serde_json::json!(provider));
    app.models.available.insert(
        model_id.clone(),
        acp::ModelInfo::new(model_id.clone(), model.to_string()).meta(Some(meta)),
    );
    model_id
}

fn set_agent_provider(app: &mut AppView, agent_id: AgentId, model: &str, provider: &str) {
    let model_id = acp::ModelId::new(model.to_string());
    let mut meta = acp::Meta::new();
    meta.insert("provider".to_string(), serde_json::json!(provider));
    let models = acp::SessionModelState::new(
        model_id.clone(),
        vec![acp::ModelInfo::new(model_id, model.to_string()).meta(Some(meta))],
    );
    app.agents.get_mut(&agent_id).unwrap().session.models = Some(models).into();
}

#[test]
fn bare_login_opens_provider_picker_without_starting_auth() {
    use crate::views::modal::ActiveModal;

    let mut app = test_app_with_agent();
    let before_auth = format!("{:?}", app.auth_state);
    let effects = dispatch(Action::OpenLoginProviderPicker, &mut app);

    assert!(effects.is_empty());
    assert_eq!(app.active_view, ActiveView::Agent(AgentId(0)));
    assert_eq!(format!("{:?}", app.auth_state), before_auth);
    let Some(ActiveModal::ArgPicker {
        command,
        items,
        original_items,
        ..
    }) = app.agents[&AgentId(0)].active_modal.as_ref()
    else {
        panic!("bare /login should open the provider ArgPicker");
    };
    assert_eq!(command, "login");
    assert_eq!(items.len(), 4);
    assert_eq!(original_items.len(), 4);
    assert_eq!(
        items
            .iter()
            .map(|item| item.insert_text.as_str())
            .collect::<Vec<_>>(),
        ["xai", "codex", "kimi", "fireworks"]
    );
}

#[test]
fn dashboard_bare_login_opens_inline_provider_picker_without_starting_auth() {
    let mut app = test_app_with_agent();
    app.dashboard = Some(crate::views::dashboard::DashboardState::new());
    app.active_view = ActiveView::AgentDashboard;
    let before_auth = format!("{:?}", app.auth_state);

    let effects = dispatch(Action::OpenLoginProviderPicker, &mut app);

    assert!(effects.is_empty());
    assert_eq!(app.active_view, ActiveView::AgentDashboard);
    assert_eq!(format!("{:?}", app.auth_state), before_auth);
    let dashboard = app.dashboard.as_ref().unwrap();
    assert_eq!(dashboard.dispatch.text(), "/login ");
    let snapshot = dashboard.dispatch.slash_snapshot();
    assert!(snapshot.open);
    assert_eq!(
        snapshot
            .matches
            .iter()
            .map(|item| item.insert_text.as_str())
            .collect::<Vec<_>>(),
        ["xai", "codex", "kimi", "fireworks"]
    );
}

#[test]
fn attached_dashboard_login_opens_picker_on_visible_agent() {
    use crate::app::app_view::InputOutcome;
    use crate::views::modal::ActiveModal;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app_with_agent();
    let mut dashboard = crate::views::dashboard::DashboardState::new();
    dashboard.attached_agent = Some(AgentId(0));
    app.dashboard = Some(dashboard);
    app.active_view = ActiveView::AgentDashboard;

    let effects = dispatch(Action::OpenLoginProviderPicker, &mut app);

    assert!(effects.is_empty());
    assert!(matches!(
        app.agents[&AgentId(0)].active_modal,
        Some(ActiveModal::ArgPicker { ref command, .. }) if command == "login"
    ));
    assert_eq!(app.dashboard.as_ref().unwrap().dispatch.text(), "");

    if let Some(ActiveModal::ArgPicker { state, .. }) =
        app.agents[&AgentId(0)].active_modal.as_mut()
    {
        state.selected = 2;
    }
    let outcome = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .handle_modal_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let InputOutcome::Action(action) = outcome else {
        panic!("attached Kimi selection should emit an action, got {outcome:?}");
    };
    assert!(matches!(&action, Action::OpenKimiApiKeyEditor));

    let effects = dispatch(action, &mut app);
    assert!(effects.is_empty());
    assert!(matches!(
        app.agents[&AgentId(0)].active_modal,
        Some(ActiveModal::Settings { .. })
    ));
}

#[test]
fn login_picker_kimi_code_selection_routes_through_matching_secure_save_flow() {
    use crate::app::app_view::InputOutcome;
    use crate::views::modal::ActiveModal;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app_with_agent();
    let effects = dispatch(Action::OpenLoginProviderPicker, &mut app);
    assert!(effects.is_empty());
    {
        let Some(ActiveModal::ArgPicker { state, .. }) =
            app.agents[&AgentId(0)].active_modal.as_mut()
        else {
            panic!("expected provider picker");
        };
        state.selected = 2;
    }

    let picker_outcome = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .handle_modal_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let InputOutcome::Action(action) = picker_outcome else {
        panic!("Kimi selection should emit a provider action, got {picker_outcome:?}");
    };
    assert!(matches!(&action, Action::OpenKimiApiKeyEditor));

    let effects = dispatch(action, &mut app);
    assert!(effects.is_empty());
    assert!(matches!(
        app.agents[&AgentId(0)].active_modal,
        Some(ActiveModal::Settings { .. })
    ));

    let service_nav = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .handle_modal_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert!(matches!(service_nav, InputOutcome::Changed));
    let service_outcome = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .handle_modal_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let InputOutcome::Action(service_action) = service_outcome else {
        panic!("Kimi service choice should emit a typed action, got {service_outcome:?}");
    };
    assert!(matches!(
        &service_action,
        Action::SetKimiApiEndpoint(endpoint) if endpoint == "code"
    ));
    let service_effects = dispatch(service_action, &mut app);
    assert!(matches!(
        service_effects.as_slice(),
        [Effect::UpdateKimiApiEndpoint {
            endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint::Code,
            previous: xai_grok_shell::kimi_models::KimiApiEndpoint::Platform,
            ..
        }]
    ));

    for c in "sk-kimi-routed".chars() {
        let outcome = app
            .agents
            .get_mut(&AgentId(0))
            .unwrap()
            .handle_modal_key(&KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        assert!(matches!(outcome, InputOutcome::Changed));
    }
    let save_outcome = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .handle_modal_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let InputOutcome::Action(save_action) = save_outcome else {
        panic!("Kimi save should emit a typed action, got {save_outcome:?}");
    };
    assert!(app.agents[&AgentId(0)].active_modal.is_none());

    let effects = dispatch(save_action, &mut app);
    assert!(matches!(
        effects.as_slice(),
        [Effect::UpdateKimiApiKey {
            endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint::Code,
            active: true,
            key: Some(key),
            ..
        }]
            if key.expose() == "sk-kimi-routed"
    ));
}

#[test]
fn failed_kimi_service_write_returns_focused_login_to_service_picker() {
    use crate::app::app_view::InputOutcome;
    use crate::views::modal::ActiveModal;
    use crate::views::settings_modal::SettingsModalMode;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app_with_agent();
    let _ = dispatch(Action::OpenKimiApiKeyEditor, &mut app);
    let _ = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .handle_modal_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    let outcome = app
        .agents
        .get_mut(&AgentId(0))
        .unwrap()
        .handle_modal_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let InputOutcome::Action(action) = outcome else {
        panic!("Kimi Code selection should emit an action");
    };
    let _ = dispatch(action, &mut app);
    assert_eq!(app.kimi_api_endpoint, "code");
    let generation = app.kimi_active_operation_generation;

    let _ = dispatch(
        Action::TaskComplete(TaskResult::KimiApiEndpointUpdated {
            endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint::Code,
            effective_endpoint: xai_grok_shell::kimi_models::KimiApiEndpoint::Code,
            previous: xai_grok_shell::kimi_models::KimiApiEndpoint::Platform,
            generation,
            stale: false,
            credential_configured: false,
            warning: None,
            error: Some("config unavailable".to_owned()),
            models: None,
        }),
        &mut app,
    );

    assert_eq!(app.kimi_api_endpoint, "platform");
    let Some(ActiveModal::Settings { state }) = app.agents[&AgentId(0)].active_modal.as_ref()
    else {
        panic!("focused Kimi login should remain open");
    };
    assert!(matches!(
        state.mode(),
        SettingsModalMode::PickingEnum {
            key: "kimi_api_endpoint",
            original_value: crate::settings::SettingValue::Enum("platform"),
            ..
        }
    ));
}

#[test]
fn codex_login_keeps_xai_session_active_and_emits_independent_effect() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::LoginCodex, &mut app);
    assert_eq!(app.active_view, ActiveView::Agent(AgentId(0)));
    assert!(matches!(
        effects.as_slice(),
        [Effect::LoginCodex {
            agent_id: Some(AgentId(0)),
            purpose: CodexLoginPurpose::Independent,
        }]
    ));
    assert!(last_system_text(&app, AgentId(0)).contains("OpenAI Codex"));
}

#[test]
fn codex_logout_keeps_xai_session_active_and_emits_independent_effect() {
    let mut app = test_app_with_agent();
    let effects = dispatch(Action::LogoutCodex, &mut app);
    assert_eq!(app.active_view, ActiveView::Agent(AgentId(0)));
    assert!(matches!(
        effects.as_slice(),
        [Effect::LogoutCodex {
            agent_id: Some(AgentId(0)),
            targets,
            primary_was_codex: false,
        }]
            if targets.is_empty()
    ));
}

#[test]
fn codex_auth_completion_does_not_change_xai_auth_state() {
    let mut app = test_app_with_agent();
    app.startup_codex_account = Some(xai_grok_shell::codex_auth::CodexAccountSummary {
        email: Some("codex@example.com".into()),
        account_id: Some("acct".into()),
        plan_type: Some("Pro".into()),
    });
    let before = format!("{:?}", app.auth_state);
    dispatch(
        Action::TaskComplete(TaskResult::CodexLogoutComplete {
            agent_id: Some(AgentId(0)),
            targets: vec![],
            primary_was_codex: false,
            result: Ok(true),
        }),
        &mut app,
    );
    assert_eq!(format!("{:?}", app.auth_state), before);
    assert!(app.startup_codex_account.is_none());
    assert_eq!(
        last_system_text(&app, AgentId(0)),
        "OpenAI Codex disconnected."
    );
}

#[test]
fn codex_login_keeps_combined_usage_available_when_xai_usage_is_hidden() {
    let mut app = test_app_with_agent();
    app.dashboard = Some(crate::views::dashboard::DashboardState::new());
    app.apply_usage_visibility(false);
    assert!(
        app.agents[&AgentId(0)]
            .prompt
            .slash_controller
            .registry()
            .get("usage")
            .is_none()
    );
    assert!(
        app.welcome_prompt
            .slash_controller
            .registry()
            .get("usage")
            .is_none()
    );

    dispatch(
        Action::TaskComplete(TaskResult::CodexLoginComplete {
            agent_id: Some(AgentId(0)),
            purpose: CodexLoginPurpose::Independent,
            result: Ok(xai_grok_shell::codex_auth::CodexAccountSummary {
                email: Some("codex@example.com".into()),
                account_id: Some("acct".into()),
                plan_type: Some("Pro".into()),
            }),
            models: None,
        }),
        &mut app,
    );

    assert!(!app.usage_visible, "xAI billing visibility stays disabled");
    assert!(app.startup_codex_account.is_some());
    assert!(
        app.agents[&AgentId(0)]
            .prompt
            .slash_controller
            .registry()
            .get("usage")
            .is_some()
    );
    assert!(
        app.welcome_prompt
            .slash_controller
            .registry()
            .get("usage")
            .is_some()
    );
    let dashboard = app.dashboard.as_ref().unwrap();
    assert!(
        dashboard
            .dispatch
            .slash_controller
            .registry()
            .get("usage")
            .is_some()
    );
    assert!(
        dashboard
            .peek_reply
            .slash_controller
            .registry()
            .get("usage")
            .is_some()
    );

    dispatch(
        Action::TaskComplete(TaskResult::CodexLogoutComplete {
            agent_id: Some(AgentId(0)),
            targets: vec![],
            primary_was_codex: false,
            result: Ok(true),
        }),
        &mut app,
    );
    assert!(app.startup_codex_account.is_none());
    assert!(
        app.agents[&AgentId(0)]
            .prompt
            .slash_controller
            .registry()
            .get("usage")
            .is_none()
    );
    assert!(
        app.welcome_prompt
            .slash_controller
            .registry()
            .get("usage")
            .is_none()
    );
    let dashboard = app.dashboard.as_ref().unwrap();
    assert!(
        dashboard
            .dispatch
            .slash_controller
            .registry()
            .get("usage")
            .is_none()
    );
    assert!(
        dashboard
            .peek_reply
            .slash_controller
            .registry()
            .get("usage")
            .is_none()
    );
}

#[test]
fn codex_primary_logout_without_xai_fallback_returns_to_provider_choice() {
    let mut app = test_app_with_agent();
    app.primary_provider = PrimaryProvider::Codex;
    app.startup_xai_ready = false;
    app.startup_codex_account = Some(xai_grok_shell::codex_auth::CodexAccountSummary {
        email: Some("codex@example.com".into()),
        account_id: Some("acct".into()),
        plan_type: Some("Pro".into()),
    });

    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodexLogoutComplete {
            agent_id: Some(AgentId(0)),
            targets: vec![ProviderSessionTarget {
                agent_id: AgentId(0),
                session_id: Some(acp::SessionId::new("test-session")),
            }],
            primary_was_codex: true,
            result: Ok(true),
        }),
        &mut app,
    );

    assert!(app.startup_codex_account.is_none());
    assert!(matches!(app.active_view, ActiveView::Welcome));
    assert!(!app.agents.contains_key(&AgentId(0)));
    assert!(matches!(app.auth_state, AuthState::ProviderChoice { .. }));
    assert_eq!(app.welcome_menu_index, Some(0));
    assert!(
        effects
            .iter()
            .any(|effect| matches!(effect, Effect::UnregisterActiveSession { .. }))
    );
}

#[test]
fn codex_logout_removes_all_codex_tabs_and_preserves_xai_tab() {
    for model_update_first in [false, true] {
        let mut app = test_app_with_agent();
        let codex_one = AgentId(0);
        set_agent_provider(&mut app, codex_one, "gpt-5.6-sol", "codex");
        for (id, sid, model, provider) in [
            (AgentId(1), "codex-two", "gpt-5.6-terra", "codex"),
            (AgentId(2), "xai-one", "grok-build", "xai"),
        ] {
            let session = make_test_agent_session(&app, id, sid);
            app.agents
                .insert(id, AgentView::new(session, ScrollbackState::new()));
            set_agent_provider(&mut app, id, model, provider);
        }
        app.next_agent_id = 3;
        app.primary_provider = PrimaryProvider::Codex;
        app.startup_xai_ready = true;
        app.active_view = ActiveView::Agent(codex_one);

        let logout_effects = dispatch(Action::LogoutCodex, &mut app);
        let (targets, primary_was_codex) = match logout_effects.as_slice() {
            [
                Effect::LogoutCodex {
                    targets,
                    primary_was_codex,
                    ..
                },
            ] => (targets.clone(), *primary_was_codex),
            other => panic!("expected one Codex logout effect, got {other:?}"),
        };
        if model_update_first {
            // Simulate catalog clear winning the race and rewriting both Codex
            // tabs/global provider to the visible xAI fallback.
            set_agent_provider(&mut app, AgentId(0), "grok-build", "xai");
            set_agent_provider(&mut app, AgentId(1), "grok-build", "xai");
            app.primary_provider = PrimaryProvider::Xai;
        }
        let effects = dispatch(
            Action::TaskComplete(TaskResult::CodexLogoutComplete {
                agent_id: Some(codex_one),
                targets,
                primary_was_codex,
                result: Ok(true),
            }),
            &mut app,
        );

        assert!(!app.agents.contains_key(&AgentId(0)));
        assert!(!app.agents.contains_key(&AgentId(1)));
        assert!(app.agents.contains_key(&AgentId(2)));
        assert_eq!(app.active_view, ActiveView::Agent(AgentId(2)));
        assert_eq!(app.primary_provider, PrimaryProvider::Xai);
        let unregistered = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::UnregisterActiveSession { session_id } => Some(session_id.0.as_ref()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            unregistered,
            std::collections::HashSet::from(["test-session", "codex-two"])
        );
        assert!(!unregistered.contains("xai-one"));
    }
}

#[test]
fn xai_logout_preserves_valid_codex_regardless_of_model_update_order() {
    for model_update_first in [false, true] {
        let mut app = test_app_with_agent();
        let xai = AgentId(0);
        let codex = AgentId(1);
        set_agent_provider(&mut app, xai, "grok-build", "xai");
        let session = make_test_agent_session(&app, codex, "codex-session");
        app.agents
            .insert(codex, AgentView::new(session, ScrollbackState::new()));
        set_agent_provider(&mut app, codex, "gpt-5.6-sol", "codex");
        app.next_agent_id = 2;
        app.primary_provider = PrimaryProvider::Xai;
        app.active_view = ActiveView::Agent(xai);

        let plan = dispatch(Action::Logout, &mut app);
        let (xai_targets, codex_targets) = match plan.as_slice() {
            [
                Effect::Logout {
                    xai_targets,
                    codex_targets,
                },
            ] => (xai_targets.clone(), codex_targets.clone()),
            other => panic!("expected xAI logout plan, got {other:?}"),
        };
        if model_update_first {
            // Simulate xAI disappearing from the catalog before the task result;
            // the foreground model/provider now looks Codex even though this
            // exact tab was xAI-owned when logout began.
            set_agent_provider(&mut app, xai, "gpt-5.6-sol", "codex");
            app.primary_provider = PrimaryProvider::Codex;
        }
        let effects = dispatch(
            Action::TaskComplete(TaskResult::LogoutComplete {
                xai_targets,
                codex_targets,
                codex_account: Some(xai_grok_shell::codex_auth::CodexAccountSummary {
                    email: None,
                    account_id: Some("acct".into()),
                    plan_type: Some("Plus".into()),
                }),
            }),
            &mut app,
        );

        assert!(
            !app.agents.contains_key(&xai),
            "ordering={model_update_first}"
        );
        assert!(
            app.agents.contains_key(&codex),
            "ordering={model_update_first}"
        );
        assert_eq!(app.active_view, ActiveView::Agent(codex));
        assert_eq!(app.primary_provider, PrimaryProvider::Codex);
        assert!(matches!(app.auth_state, AuthState::Done));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::UnregisterActiveSession { session_id }
                if session_id.0.as_ref() == "test-session"
        )));
        assert!(!effects.iter().any(|effect| matches!(
            effect,
            Effect::UnregisterActiveSession { session_id }
                if session_id.0.as_ref() == "codex-session"
        )));
    }
}

#[test]
fn independent_codex_login_completion_does_not_open_xai_auth_gate() {
    let mut app = test_app_with_agent();
    app.auth_state = AuthState::Pending { error: None };
    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodexLoginComplete {
            agent_id: Some(AgentId(0)),
            purpose: CodexLoginPurpose::Independent,
            result: Ok(xai_grok_shell::codex_auth::CodexAccountSummary {
                email: Some("codex@example.com".into()),
                account_id: Some("acct".into()),
                plan_type: Some("Pro".into()),
            }),
            models: None,
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert!(matches!(app.auth_state, AuthState::Pending { .. }));
    assert!(last_system_text(&app, AgentId(0)).contains("codex@example.com"));
}

#[test]
fn independent_codex_login_applies_fallback_catalog_for_model_picker() {
    let mut app = test_app_with_agent();
    let agent_id = AgentId(0);
    let xai_id = acp::ModelId::new("grok-build");
    let codex_id = acp::ModelId::new("gpt-5.6-sol");
    let mut xai_meta = acp::Meta::new();
    xai_meta.insert("provider".to_string(), serde_json::json!("xai"));
    let mut codex_meta = acp::Meta::new();
    codex_meta.insert("provider".to_string(), serde_json::json!("codex"));
    let models = acp::SessionModelState::new(
        xai_id.clone(),
        vec![
            acp::ModelInfo::new(xai_id.clone(), "Grok Build".to_string()).meta(Some(xai_meta)),
            acp::ModelInfo::new(codex_id.clone(), "GPT-5.6 Sol".to_string()).meta(Some(codex_meta)),
        ],
    );
    app.agents.get_mut(&agent_id).unwrap().session.models = Some(models.clone()).into();

    dispatch(
        Action::TaskComplete(TaskResult::CodexLoginComplete {
            agent_id: Some(agent_id),
            purpose: CodexLoginPurpose::Independent,
            result: Ok(xai_grok_shell::codex_auth::CodexAccountSummary {
                email: None,
                account_id: Some("acct".into()),
                plan_type: Some("Plus".into()),
            }),
            models: Some(models),
        }),
        &mut app,
    );

    assert!(app.models.available.contains_key(&codex_id));
    assert!(
        app.agents[&agent_id]
            .session
            .models
            .available
            .contains_key(&codex_id),
        "the /model catalog must include the embedded Codex fallback"
    );
    assert_eq!(app.primary_provider, PrimaryProvider::Xai);
}

#[test]
fn stale_xai_subscription_result_does_not_gate_codex_provider() {
    let mut app = test_app_with_agent();
    app.primary_provider = PrimaryProvider::Codex;
    app.clear_xai_access_controls();
    let meta = serde_json::to_value(xai_grok_shell::auth::AuthMeta {
        gate: Some(xai_grok_shell::auth::GateInfo {
            message: "xAI subscription required".into(),
            url: None,
            label: None,
        }),
        subscription_tier: Some("Free".into()),
        ..Default::default()
    })
    .unwrap();

    let effects = dispatch(
        Action::TaskComplete(TaskResult::CheckSubscriptionComplete {
            verify: None,
            meta: Some(meta),
        }),
        &mut app,
    );

    assert!(effects.is_empty());
    assert!(app.gate.is_none());
    assert!(app.subscription_tier.is_none());
    assert!(app.startup_xai_auth_meta.is_some());
}

#[test]
fn startup_codex_choice_uses_separate_oauth_purpose_and_clears_xai_gate() {
    let mut app = test_app();
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.gate = Some(xai_grok_shell::auth::GateInfo {
        message: "xAI subscription required".into(),
        url: None,
        label: None,
    });

    let effects = dispatch(Action::ChooseStartupCodex, &mut app);

    assert!(matches!(
        effects.as_slice(),
        [Effect::LoginCodex {
            agent_id: None,
            purpose: CodexLoginPurpose::Startup { request_seq: 1 },
        }]
    ));
    assert_eq!(app.primary_provider, PrimaryProvider::Codex);
    assert!(app.gate.is_none());
    assert!(matches!(
        app.auth_state,
        AuthState::Authenticating { request_seq: 1, .. }
    ));
}

#[test]
fn startup_codex_choice_reuses_fresh_cached_account_without_browser_login() {
    let mut app = test_app();
    add_provider_model(&mut app, "gpt-5.6-sol", "codex");
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.startup_codex_account = Some(xai_grok_shell::codex_auth::CodexAccountSummary {
        email: Some("codex@example.com".into()),
        account_id: Some("acct".into()),
        plan_type: Some("Plus".into()),
    });
    app.deferred_startup.new_session = true;

    let effects = dispatch(Action::ChooseStartupCodex, &mut app);

    assert!(matches!(app.auth_state, AuthState::Done));
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::LoginCodex { .. } | Effect::Authenticate { .. }
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::CreateSession {
            model_id: Some(model),
            ..
        } if model.0.as_ref() == "gpt-5.6-sol"
    )));
}

#[test]
fn startup_codex_success_selects_sol_and_drains_without_xai_effects() {
    let mut app = test_app();
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.deferred_startup.new_session = true;
    let _ = dispatch(Action::ChooseStartupCodex, &mut app);
    let model_id = acp::ModelId::new("gpt-5.6-sol");
    let mut meta = acp::Meta::new();
    meta.insert("provider".to_string(), serde_json::json!("codex"));
    let refreshed_models = acp::SessionModelState::new(
        model_id.clone(),
        vec![acp::ModelInfo::new(model_id, "GPT-5.6 Sol".to_string()).meta(Some(meta))],
    );

    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodexLoginComplete {
            agent_id: None,
            purpose: CodexLoginPurpose::Startup { request_seq: 1 },
            result: Ok(xai_grok_shell::codex_auth::CodexAccountSummary {
                email: None,
                account_id: Some("acct".into()),
                plan_type: Some("Plus".into()),
            }),
            models: Some(refreshed_models),
        }),
        &mut app,
    );

    assert!(matches!(app.auth_state, AuthState::Done));
    assert_eq!(app.primary_provider, PrimaryProvider::Codex);
    assert_eq!(
        app.startup_model_override.as_ref().map(|id| id.0.as_ref()),
        Some("gpt-5.6-sol")
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::PersistSetting {
            key: "default_model",
            value: crate::settings::SettingValue::String(value),
            ..
        } if value == "gpt-5.6-sol"
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::CreateSession {
            model_id: Some(model),
            ..
        } if model.0.as_ref() == "gpt-5.6-sol"
    )));
    assert!(
        !effects.iter().any(|effect| matches!(
            effect,
            Effect::CheckSubscription { .. } | Effect::FetchAppBilling
        )),
        "Codex startup must not enter xAI subscription or billing gates: {effects:?}"
    );
}

#[test]
fn startup_codex_preserves_configured_default_after_oauth() {
    let mut app = test_app();
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.startup_model_override = Some(acp::ModelId::new("gpt-5.6-terra"));
    let _ = dispatch(Action::ChooseStartupCodex, &mut app);
    let mut meta = acp::Meta::new();
    meta.insert("provider".to_string(), serde_json::json!("codex"));
    let terra = acp::ModelId::new("gpt-5.6-terra");
    let sol = acp::ModelId::new("gpt-5.6-sol");
    let models = acp::SessionModelState::new(
        sol.clone(),
        vec![
            acp::ModelInfo::new(sol, "GPT-5.6 Sol".to_string()).meta(Some(meta.clone())),
            acp::ModelInfo::new(terra.clone(), "GPT-5.6 Terra".to_string()).meta(Some(meta)),
        ],
    );

    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodexLoginComplete {
            agent_id: None,
            purpose: CodexLoginPurpose::Startup { request_seq: 1 },
            result: Ok(xai_grok_shell::codex_auth::CodexAccountSummary {
                email: None,
                account_id: Some("acct".into()),
                plan_type: Some("Plus".into()),
            }),
            models: Some(models),
        }),
        &mut app,
    );

    assert_eq!(app.startup_model_override, Some(terra));
}

#[test]
fn stale_configured_codex_default_falls_back_within_codex() {
    let mut app = test_app();
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.startup_model_override = Some(acp::ModelId::new("codex-no-longer-listed"));
    let _ = dispatch(Action::ChooseStartupCodex, &mut app);
    let visible = acp::ModelId::new("codex-visible");
    let mut meta = acp::Meta::new();
    meta.insert("provider".to_string(), serde_json::json!("codex"));
    let models = acp::SessionModelState::new(
        visible.clone(),
        vec![acp::ModelInfo::new(visible.clone(), "Codex Visible".to_string()).meta(Some(meta))],
    );

    let _ = dispatch(
        Action::TaskComplete(TaskResult::CodexLoginComplete {
            agent_id: None,
            purpose: CodexLoginPurpose::Startup { request_seq: 1 },
            result: Ok(xai_grok_shell::codex_auth::CodexAccountSummary {
                email: None,
                account_id: Some("acct".into()),
                plan_type: Some("Plus".into()),
            }),
            models: Some(models),
        }),
        &mut app,
    );

    assert!(matches!(app.auth_state, AuthState::Done));
    assert_eq!(app.startup_model_override, Some(visible));
}

#[test]
fn startup_codex_falls_back_only_to_a_visible_codex_model() {
    let mut app = test_app();
    add_provider_model(&mut app, "gpt-5.6-sol", "xai");
    add_provider_model(&mut app, "codex-visible", "codex");
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.startup_codex_account = Some(xai_grok_shell::codex_auth::CodexAccountSummary {
        email: None,
        account_id: Some("acct".into()),
        plan_type: Some("Plus".into()),
    });

    let effects = dispatch(Action::ChooseStartupCodex, &mut app);

    assert!(matches!(app.auth_state, AuthState::Done));
    assert_eq!(
        app.startup_model_override.as_ref().map(|id| id.0.as_ref()),
        Some("codex-visible")
    );
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::PersistSetting {
            key: "default_model",
            value: crate::settings::SettingValue::String(value),
            ..
        } if value == "codex-visible"
    )));
}

#[test]
fn startup_codex_without_a_visible_codex_model_returns_to_chooser() {
    let mut app = test_app();
    add_provider_model(&mut app, "grok-visible", "xai");
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.startup_codex_account = Some(xai_grok_shell::codex_auth::CodexAccountSummary {
        email: None,
        account_id: Some("acct".into()),
        plan_type: Some("Plus".into()),
    });
    app.deferred_startup.new_session = true;

    let effects = dispatch(Action::ChooseStartupCodex, &mut app);

    assert!(effects.is_empty());
    assert!(app.startup_model_override.is_none());
    assert!(matches!(
        &app.auth_state,
        AuthState::ProviderChoice { error: Some(error) }
            if error.contains("No visible ChatGPT Codex model")
    ));
    assert!(app.deferred_startup.new_session);
}

#[test]
fn startup_codex_failure_returns_to_provider_choice() {
    let mut app = test_app();
    app.auth_state = AuthState::ProviderChoice { error: None };
    let _ = dispatch(Action::ChooseStartupCodex, &mut app);

    let effects = dispatch(
        Action::TaskComplete(TaskResult::CodexLoginComplete {
            agent_id: None,
            purpose: CodexLoginPurpose::Startup { request_seq: 1 },
            result: Err("browser cancelled".into()),
            models: None,
        }),
        &mut app,
    );

    assert!(effects.is_empty());
    assert!(matches!(
        &app.auth_state,
        AuthState::ProviderChoice { error: Some(error) }
            if error.contains("browser cancelled")
    ));
    assert_eq!(app.welcome_menu_index, Some(0));
}

#[test]
fn startup_xai_authenticates_before_selecting_from_post_auth_catalog() {
    let mut app = test_app();
    app.auth_state = AuthState::ProviderChoice { error: None };

    let login_effects = dispatch(Action::ChooseStartupXai, &mut app);

    assert_eq!(app.primary_provider, PrimaryProvider::Xai);
    assert!(app.startup_model_override.is_none());
    assert!(
        login_effects
            .iter()
            .any(|effect| matches!(effect, Effect::Authenticate { .. }))
    );
    let model_id = acp::ModelId::new("grok-visible");
    let mut model_meta = acp::Meta::new();
    model_meta.insert("provider".to_string(), serde_json::json!("xai"));
    let models = acp::SessionModelState::new(
        model_id.clone(),
        vec![acp::ModelInfo::new(model_id, "Grok Visible".to_string()).meta(Some(model_meta))],
    );
    let complete_effects = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: Some(serde_json::json!({ "models": models })),
        }),
        &mut app,
    );

    assert!(matches!(app.auth_state, AuthState::Done));
    assert_eq!(
        app.startup_model_override.as_ref().map(|id| id.0.as_ref()),
        Some("grok-visible")
    );
    assert!(complete_effects.iter().any(|effect| matches!(
        effect,
        Effect::PersistSetting {
            key: "default_model",
            value: crate::settings::SettingValue::String(value),
            ..
        } if value == "grok-visible"
    )));
}

#[test]
fn startup_xai_preserves_configured_default_after_auth() {
    let mut app = test_app();
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.startup_model_override = Some(acp::ModelId::new("grok-configured"));
    let _ = dispatch(Action::ChooseStartupXai, &mut app);
    let configured = acp::ModelId::new("grok-configured");
    let fallback = acp::ModelId::new("grok-build");
    let mut meta = acp::Meta::new();
    meta.insert("provider".to_string(), serde_json::json!("xai"));
    let models = acp::SessionModelState::new(
        fallback.clone(),
        vec![
            acp::ModelInfo::new(fallback, "Grok Build".to_string()).meta(Some(meta.clone())),
            acp::ModelInfo::new(configured.clone(), "Grok Configured".to_string()).meta(Some(meta)),
        ],
    );

    let _ = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: Some(serde_json::json!({ "models": models })),
        }),
        &mut app,
    );

    assert_eq!(app.startup_model_override, Some(configured));
}

#[test]
fn startup_xai_choice_reuses_already_authenticated_acp_without_browser_login() {
    let mut app = test_app();
    add_provider_model(&mut app, "grok-build", "xai");
    app.auth_state = AuthState::ProviderChoice { error: None };
    app.startup_xai_ready = true;
    app.deferred_startup.new_session = true;
    app.models
        .set_current(acp::ModelId::new("gpt-5.6-sol"), None);

    let effects = dispatch(Action::ChooseStartupXai, &mut app);

    assert!(matches!(app.auth_state, AuthState::Done));
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        Effect::Authenticate { .. } | Effect::PollAuthUrl { .. }
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        Effect::CreateSession {
            model_id: Some(model),
            ..
        } if model.0.as_ref() == "grok-build"
    )));
}

#[test]
fn startup_xai_without_a_visible_xai_model_stays_on_provider_chooser() {
    let mut app = test_app();
    app.auth_state = AuthState::ProviderChoice { error: None };

    let login_effects = dispatch(Action::ChooseStartupXai, &mut app);
    assert!(
        login_effects
            .iter()
            .any(|effect| matches!(effect, Effect::Authenticate { .. }))
    );
    let codex_id = acp::ModelId::new("codex-visible");
    let mut model_meta = acp::Meta::new();
    model_meta.insert("provider".to_string(), serde_json::json!("codex"));
    let models = acp::SessionModelState::new(
        codex_id.clone(),
        vec![acp::ModelInfo::new(codex_id, "Codex Visible".to_string()).meta(Some(model_meta))],
    );

    let effects = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: Some(serde_json::json!({ "models": models })),
        }),
        &mut app,
    );

    assert!(effects.is_empty());
    assert!(app.startup_model_override.is_none());
    assert!(matches!(
        &app.auth_state,
        AuthState::ProviderChoice { error: Some(error) }
            if error.contains("No visible xAI Grok model")
    ));
    assert_eq!(app.welcome_menu_index, Some(1));
}

#[test]
fn cta_mcps_loaded_needs_auth_opens_modal_and_seeds() {
    use crate::app::agent_view::CtaPhase;
    use crate::views::extensions_modal::{ExtensionsTab, TabDataState};
    use crate::views::mcps_modal::{McpSectionId, McpServerDisplayStatus, section_key};
    let mut app = test_app_with_agent();
    app.team_id = Some("team-uuid".into());
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().plugin_cta.phase = CtaPhase::AwaitingMcps {
        name: "figma".into(),
    };
    let servers = vec![
        cta_mcp_server("grok_com_managed", None, McpServerDisplayStatus::Ready),
        cta_mcp_server("local-srv", None, McpServerDisplayStatus::Ready),
        cta_mcp_server("other-srv", Some("slack"), McpServerDisplayStatus::Ready),
        cta_mcp_server(
            "figma-srv",
            Some("figma"),
            McpServerDisplayStatus::NeedsAuth,
        ),
    ];
    let effects = dispatch(
        Action::TaskComplete(TaskResult::PluginCtaMcpsLoaded {
            agent_id: id,
            plugin_name: "figma".into(),
            result: Ok(servers),
        }),
        &mut app,
    );
    // Handoff complete: CTA settles to Hidden.
    assert_eq!(app.agents[&id].plugin_cta.phase, CtaPhase::Hidden);
    // Modal opened to the MCP Servers tab.
    let modal = app.agents[&id]
        .extensions_modal
        .as_ref()
        .expect("extensions modal should be open");
    assert_eq!(modal.active_tab, ExtensionsTab::McpServers);
    // Session team id seeded so the Managed subtitle deep link matches Ctrl+O.
    assert_eq!(modal.session_team_id.as_deref(), Some("team-uuid"));
    // MCP tab seeded directly from the read we already have (no flash).
    match &modal.mcps_data {
        TabDataState::Loaded(servers) => assert_eq!(servers.len(), 4),
        other => panic!("expected mcps_data Loaded, got {other:?}"),
    }
    // Managed + Local + other plugins collapsed; only target expanded.
    let collapsed = &modal.mcps_collapsed_sections;
    assert!(collapsed.contains(&section_key(&McpSectionId::Managed)));
    assert!(collapsed.contains(&section_key(&McpSectionId::Local)));
    assert!(collapsed.contains(&section_key(&McpSectionId::Plugin("slack".into()))));
    assert!(!collapsed.contains(&section_key(&McpSectionId::Plugin("figma".into()))));
    assert!(modal.mcps_section_collapse_initialized);
    // Emits the SAME full tab fetch-set as a manual open so no tab is stuck
    // Loading, plus the candidate refresh.
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::FetchHooksList { .. }))
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::FetchPluginsList { .. }))
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::FetchMarketplaceList { .. }))
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::FetchMcpsList { .. }))
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|e| matches!(e, Effect::FetchSkillsList { .. }))
            .count(),
        1
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPluginCtaCatalog { .. }))
    );
}

#[test]
fn cta_mcps_loaded_no_needs_auth_terminal_sets_installed() {
    use crate::app::agent_view::CtaPhase;
    use crate::views::mcps_modal::McpServerDisplayStatus;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let cta = &mut app.agents.get_mut(&id).unwrap().plugin_cta;
        cta.phase = CtaPhase::AwaitingMcps {
            name: "figma".into(),
        };
        cta.expects_mcp = true;
    }
    // Plugin server present and Ready (terminal, no auth) -> settle now.
    let servers = vec![cta_mcp_server(
        "figma-srv",
        Some("figma"),
        McpServerDisplayStatus::Ready,
    )];
    let effects = dispatch(
        Action::TaskComplete(TaskResult::PluginCtaMcpsLoaded {
            agent_id: id,
            plugin_name: "figma".into(),
            result: Ok(servers),
        }),
        &mut app,
    );
    assert_eq!(
        app.agents[&id].plugin_cta.phase,
        CtaPhase::Installed {
            name: "figma".into()
        }
    );
    assert!(app.agents[&id].extensions_modal.is_none());
    // No modal repopulation; settle emits the auto-dismiss timer + candidate
    // refresh, and never re-probes.
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMcpsList { .. }))
    );
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RetryPluginCtaMcps { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::DismissCtaInstalled { .. }))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchPluginCtaCatalog { .. }))
    );
}

#[test]
fn cta_mcps_loaded_later_needs_auth_opens_handoff() {
    use crate::app::agent_view::CtaPhase;
    use crate::views::mcps_modal::McpServerDisplayStatus;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let cta = &mut app.agents.get_mut(&id).unwrap().plugin_cta;
        cta.phase = CtaPhase::AwaitingMcps {
            name: "figma".into(),
        };
        cta.expects_mcp = true;
        // Several polls already elapsed before the server reached NeedsAuth.
        cta.mcp_attempt = 5;
    }
    let effects = dispatch(
        Action::TaskComplete(TaskResult::PluginCtaMcpsLoaded {
            agent_id: id,
            plugin_name: "figma".into(),
            result: Ok(vec![cta_mcp_server(
                "figma-srv",
                Some("figma"),
                McpServerDisplayStatus::NeedsAuth,
            )]),
        }),
        &mut app,
    );
    // NeedsAuth is terminal: hand off immediately even mid-poll.
    assert_eq!(app.agents[&id].plugin_cta.phase, CtaPhase::Hidden);
    assert!(app.agents[&id].extensions_modal.is_some());
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::RetryPluginCtaMcps { .. }))
    );
}

// ── agent-bound kinds (bash) ─────────

/// A bash command typed while a turn is RUNNING takes the
/// server-authoritative immediate path (Effect + optimistic echo, no local
/// queue entry).
#[test]
fn bash_while_running_is_server_authoritative() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().session.state = AgentState::TurnRunning;

    let effects = dispatch(Action::SendBashCommand("ls -la".into()), &mut app);
    let pid = match &effects[0] {
        Effect::SendBashCommand {
            command, prompt_id, ..
        } => {
            assert_eq!(command, "ls -la");
            prompt_id.clone()
        }
        other => panic!("expected immediate SendBashCommand, got {other:?}"),
    };
    // Not in the local queue.
    assert_eq!(app.agents[&id].session.queue_len(), 0);
    // Optimistic echo present with kind="bash".
    let q = app
        .shared_prompt_queue("test-session")
        .expect("echo present");
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].id, pid);
    assert_eq!(q[0].kind, "bash");
    assert_eq!(q[0].text, "ls -la");
}

#[test]
fn auth_complete_triggers_bundle_status_fetch() {
    let mut app = test_app();
    app.auth_state = AuthState::Authenticating {
        request_seq: 1,
        handle: None,
        auth_url: None,
        mode: AuthMode::Pending,
    };

    let effects = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: None,
        }),
        &mut app,
    );

    assert!(matches!(app.auth_state, AuthState::Done));
    // Pager only refreshes the on-disk catalog snapshot; the actual
    // bundle download now runs inside the shell post-auth.
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchBundleStatus))
    );
}

#[test]
fn auth_complete_with_deferred_load_also_fetches_status() {
    let mut app = test_app();
    app.auth_state = AuthState::Authenticating {
        request_seq: 1,
        handle: None,
        auth_url: None,
        mode: AuthMode::Pending,
    };
    app.deferred_startup.session =
        Some(crate::app::session_startup::DeferredSessionStartup::Load {
            session_id: "test-session".into(),
            session_cwd: None,
            chat_kind: false,
        });

    let effects = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: None,
        }),
        &mut app,
    );

    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchBundleStatus))
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadSession { .. }))
    );
    assert!(app.deferred_startup.session.is_none());
}

/// `/login` from the welcome screen (startup / logged-out) must NOT
/// stash a return view — the normal login-then-load flow is preserved.
#[test]
fn login_from_welcome_does_not_stash_return_view() {
    let mut app = test_app();
    assert_eq!(app.active_view, ActiveView::Welcome);

    dispatch(Action::Login, &mut app);

    assert_eq!(app.active_view, ActiveView::Welcome);
    assert_eq!(app.auth_return_view, None);
}

/// Compact-auth recovery: hold prompt across auto-compact 401, stash on
/// PromptResponse, resubmit on mid-session AuthComplete.
#[test]
fn e2e_compact_auth_failure_holds_prompt_and_resubmits_after_login() {
    use crate::app::acp_handler::apply_session_event_for_test;
    use crate::app::agent::{AgentState, InFlightPrompt};
    use crate::scrollback::EntryId;
    use crate::scrollback::block::RenderBlock;
    use xai_grok_shell::extensions::notification::{RetryState, SessionUpdate as XaiSessionUpdate};

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent.session.session_id = Some(acp::SessionId::new("sess-compact-auth-e2e"));
        agent.session.current_prompt_id = Some("prompt-1".into());
        agent.session.in_flight_prompt = Some(InFlightPrompt {
            text: "please continue after login".into(),
            images: Vec::new(),
            scrollback_entry: EntryId::new(1),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });

        apply_session_event_for_test(
            &XaiSessionUpdate::AutoCompactStarted {
                tokens_used: 180_000,
                context_window: 200_000,
                percentage: 90,
                reason: "threshold".into(),
            },
            &mut agent.session,
            &mut agent.scrollback,
        );
        assert!(
            agent.session.in_flight_prompt.is_none(),
            "cancel rewind must still be blocked mid-compact"
        );
        assert_eq!(
            agent
                .session
                .compact_held_prompt
                .as_ref()
                .map(|p| p.text.as_str()),
            Some("please continue after login"),
            "must hold the prompt text for reauth auto-resubmit"
        );

        apply_session_event_for_test(
            &XaiSessionUpdate::AutoCompactFailed {
                error: "authentication problem — re-authenticate using /login and retry.".into(),
            },
            &mut agent.session,
            &mut agent.scrollback,
        );
        assert!(agent.session.compact_held_prompt.is_some());

        apply_session_event_for_test(
            &XaiSessionUpdate::RetryState(RetryState::Failed {
                error_type: "auth".into(),
                message: "Unauthorized (401): compaction failed".into(),
            }),
            &mut agent.session,
            &mut agent.scrollback,
        );
        let has_reauth = (0..agent.scrollback.len()).any(|i| {
            matches!(
                agent.scrollback.entry(i).map(|e| &e.block),
                Some(RenderBlock::SessionEvent(ev))
                    if matches!(ev.event, SessionEvent::ReAuthRequired)
            )
        });
        assert!(has_reauth, "RetryState auth must show ReAuthRequired");
    }

    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("Unauthorized (401)".to_string()),
            http_status: Some(401),
            prompt_id: Some("prompt-1".into()),
        }),
        &mut app,
    );
    assert_eq!(
        app.agents[&id]
            .reauth_stashed_prompt
            .as_ref()
            .map(|p| p.text.as_str()),
        Some("please continue after login"),
        "PromptResponse must stash the compact-held prompt for AuthComplete"
    );

    dispatch(Action::Login, &mut app);
    let seq = authenticating_seq(&app);
    let effects = dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: seq,
            meta: None,
        }),
        &mut app,
    );
    assert!(
        app.agents[&id].reauth_stashed_prompt.is_none(),
        "stash consumed on AuthComplete"
    );
    assert!(
        effects.iter().any(|e| matches!(
            e,
            Effect::SendPrompt { .. } | Effect::SendPromptBlocks { .. }
        )),
        "AuthComplete must resubmit the prompt so compact runs again with valid auth, got: {effects:?}"
    );
}

/// Without compact_held, clearing in_flight on compact start leaves reauth empty.
#[test]
fn pre_fix_compact_start_without_hold_cannot_stash_for_reauth() {
    use crate::app::agent::AgentState;
    use crate::scrollback::block::RenderBlock;

    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent.session.session_id = Some(acp::SessionId::new("sess-pre-fix"));
        agent.session.current_prompt_id = Some("p1".into());
        agent.session.in_flight_prompt = None;
        agent.session.compact_held_prompt = None;
        agent
            .scrollback
            .push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
    }
    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("Unauthorized (401)".to_string()),
            http_status: Some(401),
            prompt_id: Some("p1".into()),
        }),
        &mut app,
    );
    assert!(
        app.agents[&id].reauth_stashed_prompt.is_none(),
        "without compact_held / in_flight, reauth cannot stash — the pre-fix bug"
    );
}

/// A second auth-failed turn with no rewindable prompt
/// (`in_flight_prompt == None`) must not clobber the stash from an
/// earlier 401.
#[test]
fn second_auth_failure_does_not_clobber_reauth_stash() {
    use crate::scrollback::block::RenderBlock;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.reauth_stashed_prompt = Some(crate::app::agent::InFlightPrompt {
            text: "first prompt".into(),
            images: Vec::new(),
            scrollback_entry: crate::scrollback::EntryId::new(0),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        agent
            .scrollback
            .push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
        agent.session.state = AgentState::TurnRunning;
        agent.turn_started_at = Some(std::time::Instant::now());
        agent.session.in_flight_prompt = None;
    }

    dispatch(
        Action::TaskComplete(TaskResult::PromptResponse {
            agent_id: id,
            result: Err("Unauthorized (401)".to_string()),
            http_status: Some(401),
            prompt_id: None,
        }),
        &mut app,
    );

    assert_eq!(
        app.agents[&id]
            .reauth_stashed_prompt
            .as_ref()
            .map(|prompt| prompt.text.as_str()),
        Some("first prompt"),
        "a None in_flight_prompt must not wipe an earlier stash"
    );
}

/// Cancelling a mid-session re-auth drops the stashed prompt so it is
/// not silently resubmitted on a later, unrelated login.
#[test]
fn cancel_login_drops_reauth_stashed_prompt() {
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    app.agents.get_mut(&id).unwrap().reauth_stashed_prompt =
        Some(crate::app::agent::InFlightPrompt {
            text: "stale".into(),
            images: Vec::new(),
            scrollback_entry: crate::scrollback::EntryId::new(0),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });

    dispatch(Action::Login, &mut app);
    dispatch(Action::CancelLogin, &mut app);

    assert!(
        app.agents[&id].reauth_stashed_prompt.is_none(),
        "cancelling re-auth must drop the stashed prompt"
    );
}

/// Cancelling a mid-session re-auth strips the stale `ReAuthRequired`
/// prompt from scrollback so a later `PromptResponse` cannot re-detect
/// it and re-stash the prompt for silent resubmission.
#[test]
fn cancel_login_strips_reauth_prompt_from_scrollback() {
    use crate::scrollback::block::RenderBlock;
    let mut app = test_app_with_agent();
    let id = AgentId(0);
    {
        let agent = app.agents.get_mut(&id).unwrap();
        agent.reauth_stashed_prompt = Some(crate::app::agent::InFlightPrompt {
            text: "stale".into(),
            images: Vec::new(),
            scrollback_entry: crate::scrollback::EntryId::new(0),
            combined_scrollback_entries: Vec::new(),
            chip_elements: Vec::new(),
        });
        agent
            .scrollback
            .push_block(RenderBlock::session_event(SessionEvent::ReAuthRequired));
    }

    dispatch(Action::Login, &mut app);
    dispatch(Action::CancelLogin, &mut app);

    let sb = &app.agents[&id].scrollback;
    let has_reauth = (0..sb.len()).any(|i| {
        matches!(
            sb.entry(i).map(|e| &e.block),
            Some(RenderBlock::SessionEvent(ev)) if matches!(ev.event, SessionEvent::ReAuthRequired)
        )
    });
    assert!(
        !has_reauth,
        "cancelling re-auth must strip the stale re-auth prompt from scrollback"
    );
}

/// Empty `auth_methods` (preferred_method pin unavailable) must not invent
/// `grok.com` or start an OIDC flow the agent did not advertise.
#[test]
fn login_with_empty_auth_methods_fails_closed() {
    let mut app = test_app_with_agent();
    app.auth_methods.clear();
    app.login_method_id = None;

    let effects = dispatch(Action::Login, &mut app);

    assert!(
        effects.is_empty(),
        "must not start Authenticate without an advertised method"
    );
    assert_eq!(
        app.active_view,
        ActiveView::Agent(AgentId(0)),
        "must stay on the session view"
    );
    assert!(
        matches!(
            &app.auth_state,
            AuthState::Pending { error: Some(msg) }
                if msg.contains("preferred_method=api_key")
        ),
        "must surface pin-unavailable error, got {:?}",
        app.auth_state
    );
    assert!(app.login_method_id.is_none());
}

/// Puts the app in `Authenticating` with a live task's abort handle installed
/// (as the event loop would), returning the task's JoinHandle and the seq.
/// Callers assert the task actually gets aborted (`unwrap_err().is_cancelled()`),
/// not merely that the handle slot was cleared.
fn install_live_auth_task(
    app: &mut AppView,
    rt: &tokio::runtime::Runtime,
) -> (tokio::task::JoinHandle<()>, u64) {
    dispatch(Action::Login, app);
    let task = rt.spawn(std::future::pending::<()>());
    match &mut app.auth_state {
        AuthState::Authenticating {
            handle,
            request_seq,
            ..
        } => {
            *handle = Some(task.abort_handle());
            (task, *request_seq)
        }
        other => panic!("expected Authenticating after Login, got {other:?}"),
    }
}

fn test_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
}

/// A second `/login` while already authenticating must abort the prior auth
/// task and bump the seq (single-flight: no stacked device-code mints).
#[test]
fn login_while_authenticating_aborts_prior_task() {
    let rt = test_runtime();
    let mut app = test_app_with_agent();
    let (prior_task, first_seq) = install_live_auth_task(&mut app, &rt);

    let effects = dispatch(Action::Login, &mut app);

    rt.block_on(async {
        assert!(
            prior_task.await.unwrap_err().is_cancelled(),
            "prior auth task must be aborted"
        );
    });
    match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => {
            assert!(
                *request_seq > first_seq,
                "re-login must bump request_seq for single-flight"
            );
        }
        other => panic!("expected Authenticating after re-Login, got {other:?}"),
    }
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Authenticate { .. })),
        "re-login must emit a new Authenticate"
    );
}

/// The URL poll is a sibling task to Authenticate and must be part of the same
/// single-flight boundary; otherwise a stale poll can publish the prior
/// attempt's URL after the successor has started.
#[test]
fn login_while_authenticating_aborts_prior_url_poll() {
    let rt = test_runtime();
    let mut app = test_app_with_agent();
    dispatch(Action::Login, &mut app);
    let request_seq = match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => *request_seq,
        other => panic!("expected Authenticating after Login, got {other:?}"),
    };
    let poll_task = rt.spawn(std::future::pending::<()>());
    app.auth_url_poll_handle = Some((request_seq, poll_task.abort_handle()));

    dispatch(Action::Login, &mut app);

    rt.block_on(async {
        assert!(
            poll_task.await.unwrap_err().is_cancelled(),
            "prior auth URL poll must be aborted"
        );
    });
    assert!(
        app.auth_url_poll_handle.is_none(),
        "the stale URL poll handle must be cleared"
    );
}

/// A stale `AuthComplete` (from an attempt whose abort lost the race because
/// the task had already finished) must not complete the new attempt: the
/// request-seq guard is the only protection here.
#[test]
fn stale_auth_complete_after_relogin_is_ignored() {
    let mut app = test_app_with_agent();
    dispatch(Action::Login, &mut app);
    let first_seq = match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => *request_seq,
        other => panic!("expected Authenticating after Login, got {other:?}"),
    };
    dispatch(Action::Login, &mut app); // re-login bumps to seq2

    dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: first_seq,
            meta: None,
        }),
        &mut app,
    );

    match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => {
            assert!(
                *request_seq > first_seq,
                "stale AuthComplete must leave the new attempt authenticating"
            );
        }
        other => panic!("stale AuthComplete must be ignored, got {other:?}"),
    }
}

/// Switch-account while authenticating goes through the same single-flight
/// abort as `/login` (sibling entry point).
#[test]
fn switch_account_while_authenticating_aborts_prior_task() {
    let rt = test_runtime();
    let mut app = test_app_with_agent();
    let (prior_task, first_seq) = install_live_auth_task(&mut app, &rt);

    dispatch(Action::SwitchAccount, &mut app);

    rt.block_on(async {
        assert!(
            prior_task.await.unwrap_err().is_cancelled(),
            "prior auth task must be aborted on switch-account"
        );
    });
    match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => {
            assert!(*request_seq > first_seq, "switch must bump request_seq");
        }
        other => panic!("expected Authenticating after SwitchAccount, got {other:?}"),
    }
}

/// Cancelling a mid-session login aborts the in-flight auth task (not just
/// restores the view) so a retry cannot race a still-polling prior mint.
#[test]
fn cancel_login_aborts_prior_task() {
    let rt = test_runtime();
    let mut app = test_app_with_agent();
    // Login from a session view stashes `auth_return_view`, making CancelLogin live.
    let (prior_task, _) = install_live_auth_task(&mut app, &rt);

    dispatch(Action::CancelLogin, &mut app);

    rt.block_on(async {
        assert!(
            prior_task.await.unwrap_err().is_cancelled(),
            "cancel must abort the in-flight auth task"
        );
    });
}

/// Cancelling a mid-session login returns to the session rather than
/// quitting the app, and clears the stashed view + auth state.
#[test]
fn cancel_login_restores_view() {
    let mut app = test_app_with_agent();
    dispatch(Action::Login, &mut app);
    assert_eq!(app.active_view, ActiveView::Welcome);
    let prior_seq = match &app.auth_state {
        AuthState::Authenticating { request_seq, .. } => *request_seq,
        other => panic!("expected Authenticating after Login, got {other:?}"),
    };

    let effects = dispatch(Action::CancelLogin, &mut app);

    assert!(
        matches!(
            effects.as_slice(),
            [Effect::CancelAuth { request_seq }] if *request_seq == prior_seq
        ),
        "cancel must tell the shell to stop the in-flight auth poll for this attempt"
    );
    assert_eq!(app.active_view, ActiveView::Agent(AgentId(0)));
    assert_eq!(app.auth_return_view, None);
    assert!(matches!(app.auth_state, AuthState::Done));
}

/// `CancelLogin` outside a mid-session login is a no-op (must not move
/// off the welcome screen or panic).
#[test]
fn cancel_login_noop_without_stashed_view() {
    let mut app = test_app();
    let effects = dispatch(Action::CancelLogin, &mut app);
    assert!(effects.is_empty());
    assert_eq!(app.active_view, ActiveView::Welcome);
    assert_eq!(app.auth_return_view, None);
}

/// Codex OAuth is pager-owned and never stashes an xAI auth return view, so
/// cancelling from its startup screen must not emit `x.ai/auth/cancel` or
/// mutate the live Codex attempt.
#[test]
fn cancel_login_during_codex_oauth_never_emits_xai_cancel() {
    let mut app = test_app();
    let request_seq = 41;
    app.primary_provider = PrimaryProvider::Codex;
    app.startup_provider_selection = Some(PrimaryProvider::Codex);
    app.auth_state = AuthState::Authenticating {
        request_seq,
        handle: None,
        auth_url: None,
        mode: AuthMode::Command,
    };
    app.auth_return_view = None;
    let next_auth_request_seq = app.next_auth_request_seq;

    let effects = dispatch(Action::CancelLogin, &mut app);

    assert!(
        effects.is_empty(),
        "Codex OAuth must not emit xAI auth cancel"
    );
    assert!(matches!(
        app.auth_state,
        AuthState::Authenticating {
            request_seq: current,
            ..
        } if current == request_seq
    ));
    assert_eq!(app.next_auth_request_seq, next_auth_request_seq);
}

#[test]
fn auth_complete_extracts_show_resolved_model_from_meta() {
    let mut app = test_app();
    app.auth_state = AuthState::Authenticating {
        request_seq: 1,
        handle: None,
        auth_url: None,
        mode: AuthMode::Pending,
    };
    assert!(app.show_resolved_model);

    dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: Some(serde_json::json!({ "show_resolved_model": false })),
        }),
        &mut app,
    );

    assert!(!app.show_resolved_model);
}

#[test]
fn auth_complete_preserves_show_resolved_model_when_absent() {
    let mut app = test_app();
    app.show_resolved_model = false;
    app.auth_state = AuthState::Authenticating {
        request_seq: 1,
        handle: None,
        auth_url: None,
        mode: AuthMode::Pending,
    };

    dispatch(
        Action::TaskComplete(TaskResult::AuthComplete {
            request_seq: 1,
            meta: Some(serde_json::to_value(xai_grok_shell::auth::AuthMeta::default()).unwrap()),
        }),
        &mut app,
    );

    assert!(!app.show_resolved_model);
}
