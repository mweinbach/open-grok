use super::*;

#[test]
fn one_shot_swarm_task_orders_mode_before_prompt_without_flipping_manual_state() {
    let mut app = test_app_with_agent();
    let agent_id = AgentId(0);
    app.agents.get_mut(&agent_id).unwrap().swarm_mode_active = false;

    let effects = dispatch(
        Action::StartSwarmTask("audit the auth flow".into()),
        &mut app,
    );

    assert!(matches!(
        effects.as_slice(),
        [Effect::SwarmModeThenPrompt {
            text,
            enabled: true,
            rollback_enabled: Some(false),
            ..
        }] if text == "audit the auth flow"
    ));
    let agent = &app.agents[&agent_id];
    assert!(!agent.swarm_mode_active);
    assert!(agent.session.state.is_turn_running());
    assert_eq!(
        agent
            .session
            .in_flight_prompt
            .as_ref()
            .map(|prompt| prompt.text.as_str()),
        Some("audit the auth flow")
    );
}

#[test]
fn swarm_setup_failure_restores_prompt_attachments_and_manual_state() {
    let mut app = test_app_with_agent();
    let agent_id = AgentId(0);
    app.agents.get_mut(&agent_id).unwrap().swarm_mode_active = true;

    let effects = dispatch(
        Action::StartSwarmTask("inspect the screenshot".into()),
        &mut app,
    );
    let prompt_id = match effects.as_slice() {
        [
            Effect::SwarmModeThenPrompt {
                prompt_id,
                rollback_enabled: Some(true),
                ..
            },
        ] => prompt_id.clone(),
        other => panic!("expected ordered swarm prompt, got {other:?}"),
    };

    let mut image = crate::prompt_images::from_clipboard_data(&crate::clipboard::ImageData {
        data: vec![1, 2, 3],
        mime_type: "image/png".into(),
    });
    image.display_number = 1;
    {
        let agent = app.agents.get_mut(&agent_id).unwrap();
        let in_flight = agent.session.in_flight_prompt.as_mut().unwrap();
        in_flight.text = "inspect [Image #1]".into();
        in_flight.images = vec![image];
        in_flight.chip_elements = vec![crate::app::agent::ChipElement {
            range: 8..18,
            kind: crate::views::prompt_widget::KIND_IMAGE,
            display: None,
        }];
    }

    let effects = dispatch(
        Action::TaskComplete(TaskResult::SwarmPromptSetupFailed {
            agent_id,
            prompt_id,
            text: "inspect the screenshot".into(),
            error: "connection closed".into(),
            pending_rollback_enabled: None,
        }),
        &mut app,
    );

    assert!(effects.is_empty());
    let agent = &app.agents[&agent_id];
    assert!(agent.swarm_mode_active);
    assert!(agent.session.state.is_idle());
    assert!(agent.session.current_prompt_id.is_none());
    assert!(agent.session.in_flight_prompt.is_none());
    assert_eq!(agent.scrollback.len(), 0);
    assert_eq!(agent.prompt.text(), "inspect [Image #1]");
    assert_eq!(agent.prompt.images.len(), 1);
}

#[test]
fn failed_one_shot_rollback_is_retried_before_the_next_ordinary_prompt() {
    let mut app = test_app_with_agent();
    let agent_id = AgentId(0);

    let effects = dispatch(Action::StartSwarmTask("inspect auth".into()), &mut app);
    let prompt_id = match effects.as_slice() {
        [
            Effect::SwarmModeThenPrompt {
                enabled: true,
                rollback_enabled: Some(false),
                prompt_id,
                ..
            },
        ] => prompt_id.clone(),
        other => panic!("expected one-shot swarm setup, got {other:?}"),
    };

    let effects = dispatch(
        Action::TaskComplete(TaskResult::SwarmPromptSetupFailed {
            agent_id,
            prompt_id,
            text: "inspect auth".into(),
            error: "prompt transport failed; rollback transport failed".into(),
            pending_rollback_enabled: Some(false),
        }),
        &mut app,
    );
    assert!(effects.is_empty());
    assert_eq!(
        app.agents[&agent_id].pending_swarm_mode_rollback,
        Some(false)
    );
    assert!(!app.agents[&agent_id].swarm_mode_active);

    let effects = dispatch(Action::SendPrompt("ordinary follow-up".into()), &mut app);
    assert!(matches!(
        effects.as_slice(),
        [Effect::SwarmModeThenPrompt {
            text,
            enabled: false,
            trigger: "task_rollback",
            rollback_enabled: None,
            ..
        }] if text == "ordinary follow-up"
    ));
}
