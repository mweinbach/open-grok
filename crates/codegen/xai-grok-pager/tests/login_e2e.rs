//! Focused integration coverage for `/login` provider completion.

use std::path::PathBuf;

use xai_grok_pager::acp::model_state::ModelState;
use xai_grok_pager::slash::{SlashController, SlashState};

#[test]
fn login_argument_completion_lists_xai_codex_and_kimi() {
    let mut controller = SlashController::with_builtins(PathBuf::from("."));
    let state = SlashState::default();
    let models = ModelState::default();

    controller.refresh(&state, "/login ", "/login ".len(), &models);

    let snapshot = state.snapshot();
    assert!(
        snapshot.open,
        "/login arguments should open the provider list"
    );
    assert!(!snapshot.cursor_in_command);
    assert_eq!(
        snapshot
            .matches
            .iter()
            .map(|row| (row.display.as_str(), row.insert_text.as_str()))
            .collect::<Vec<_>>(),
        [
            ("xAI Grok", "xai"),
            ("ChatGPT Codex", "codex"),
            ("Kimi", "kimi"),
        ]
    );
    assert!(snapshot.matches[2].description.contains("API key"));
}

#[test]
fn login_provider_completion_filters_by_provider_aliases() {
    let mut controller = SlashController::with_builtins(PathBuf::from("."));
    let state = SlashState::default();
    let models = ModelState::default();

    for (query, expected) in [("moonshot", "kimi"), ("openai", "codex"), ("grok", "xai")] {
        let text = format!("/login {query}");
        controller.refresh(&state, &text, text.len(), &models);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.matches.len(), 1, "query {query:?}");
        assert_eq!(snapshot.matches[0].insert_text, expected, "query {query:?}");
    }
}
