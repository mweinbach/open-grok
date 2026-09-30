use std::sync::Arc;

use agent_client_protocol as acp;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::actions::ActionRegistry;
use crate::app::agent_view::AgentPane;
use crate::app::agent_view::test_fixtures::make_agent;
use crate::views::modal::ActiveModal;

/// Ctrl+M, then Enter on a reasoning model: the effort sub-menu opens on the model's default effort row.
#[test]
fn arg_picker_effort_phase_opens_on_default_row() {
    let mut agent = make_agent();
    let id = acp::ModelId::new(Arc::from("reasoning-x"));
    agent.session.models.available.insert(
        id.clone(),
        acp::ModelInfo::new(id, "Reasoning X").meta(
            serde_json::json!({ "supportsReasoningEffort": true, "reasoningEffort": "high" })
                .as_object()
                .cloned(),
        ),
    );
    // Ctrl+M is the multiline toggle while the prompt is focused; the picker binding lives on the agent screen
    agent.set_active_pane(AgentPane::Scrollback, true);

    let registry = ActionRegistry::defaults();
    agent.handle_input(
        &Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)),
        &registry,
    );
    assert!(
        matches!(
            agent.active_modal.as_ref(),
            Some(ActiveModal::ArgPicker { command, args_query, .. })
                if command == "model" && args_query.is_empty()
        ),
        "Ctrl+M must open the /model picker in the model phase"
    );

    agent.handle_input(
        &Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &registry,
    );
    let Some(ActiveModal::ArgPicker {
        args_query,
        items,
        state,
        ..
    }) = agent.active_modal.as_ref()
    else {
        panic!("expected the /model picker to chain into the effort phase");
    };
    assert_eq!("Reasoning X ", args_query);
    assert_eq!(1, state.selected);
    assert_eq!(
        Some("Reasoning X high"),
        items.get(1).map(|item| item.insert_text.as_str())
    );
}
