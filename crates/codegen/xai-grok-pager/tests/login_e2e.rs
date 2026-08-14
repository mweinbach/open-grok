//! Focused integration coverage for `/login` provider completion.

use std::path::PathBuf;

use xai_grok_pager::acp::model_state::ModelState;
use xai_grok_pager::slash::{SlashController, SlashState};

#[test]
fn login_argument_completion_lists_all_providers() {
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
            ("Fireworks AI", "fireworks"),
            ("DeepSeek", "deepseek"),
            ("Meta API", "meta"),
            ("OpenCode Go", "opencode-go"),
            ("Wafer AI", "wafer"),
            ("Z AI", "zai"),
        ]
    );
    for row in &snapshot.matches[2..] {
        assert!(
            row.description.contains("API key"),
            "{} should describe API-key setup, got {:?}",
            row.display,
            row.description
        );
    }
}

#[test]
fn login_provider_completion_filters_by_provider_aliases() {
    let mut controller = SlashController::with_builtins(PathBuf::from("."));
    let state = SlashState::default();
    let models = ModelState::default();

    // Exact login aliases must rank their provider first. Generic fuzzy queries
    // may still surface additional secondary matches (e.g. "openai" also hits
    // OpenCode Go / Wafer), so do not require a singleton result set.
    for (query, expected) in [
        ("moonshot", "kimi"),
        ("openai", "codex"),
        ("chatgpt", "codex"),
        ("grok", "xai"),
        ("deepseek", "deepseek"),
        ("meta", "meta"),
        ("go", "opencode-go"),
        ("opencode", "opencode-go"),
        ("wafer", "wafer"),
        ("zai", "zai"),
        ("fireworks", "fireworks"),
    ] {
        let text = format!("/login {query}");
        controller.refresh(&state, &text, text.len(), &models);
        let snapshot = state.snapshot();
        assert!(
            !snapshot.matches.is_empty(),
            "query {query:?} should match at least one provider"
        );
        assert_eq!(
            snapshot.matches[0].insert_text,
            expected,
            "query {query:?} should rank {expected:?} first, got {:?}",
            snapshot
                .matches
                .iter()
                .map(|row| row.insert_text.as_str())
                .collect::<Vec<_>>()
        );
    }
}
