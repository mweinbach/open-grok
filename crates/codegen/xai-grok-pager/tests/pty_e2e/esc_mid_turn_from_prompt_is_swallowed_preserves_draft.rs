// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Mid-turn Esc from the PROMPT pane is a swallowed no-op: it must NOT cancel
/// the turn and must NOT arm idle clear/rewind, even with a non-empty draft.
/// Draft text stays in the composer; cancel remains on Ctrl+C / palette / etc.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn esc_mid_turn_from_prompt_is_swallowed_preserves_draft() {
    let content = ContentController::start().await.expect("start content");
    // Long paced stream so the turn is still visibly running when Esc lands.
    let long_response = format!(
        "{MOCK_RESPONSE_SENTINEL} {}",
        "streaming filler words for the cancellation window. ".repeat(120)
    );
    content.set_response(long_response);
    content.set_chunk_delay(Some(Duration::from_millis(50)));

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness =
        PtyHarness::spawn_with_content(&binary, DEFAULT_ROWS, DEFAULT_COLS, &content, &[])
            .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("stream started");

    // Type a draft into the prompt WHILE the turn streams (prompt stays focused
    // after submit). A distinctive single token avoids any wrapping ambiguity.
    let draft = "DRAFTKEEPME";
    harness.inject_keys(draft.as_bytes()).expect("type draft");
    harness
        .wait_for_text(draft, Duration::from_secs(10))
        .expect("draft renders in the composer");

    // 1× Esc mid-turn must swallow (not cancel, not arm clear).
    harness.inject_keys(keys::ESC).expect("press esc");
    harness.update(Duration::from_millis(1000));
    let screen = harness.screen_contents();

    assert!(
        !screen.contains("Turn cancelled by user"),
        "mid-turn Esc must NOT cancel the turn\nscreen:\n{screen}"
    );
    assert!(
        screen.contains(draft),
        "mid-turn Esc must preserve the draft\nscreen:\n{screen}"
    );
    assert!(
        !screen.contains("press again to clear"),
        "running-turn Esc must not arm the idle clear\nscreen:\n{screen}"
    );

    // Positive tail: prove the turn was still alive at Esc-time (the negative
    // check above would false-pass on an already-finished turn) and that
    // Ctrl+C — the replacement cancel gesture — works from this pane. With a
    // non-empty draft the first Ctrl+C clears the draft and keeps the turn;
    // the second (now on an empty prompt) cancels it.
    harness.inject_keys(keys::CTRL_C).expect("first ctrl+c");
    wait_for_labels_absent(&mut harness, &[draft], Duration::from_secs(10));
    assert!(
        !harness.contains_text(draft),
        "first Ctrl+C must clear the draft, not cancel\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.inject_keys(keys::CTRL_C).expect("second ctrl+c");
    harness
        .wait_for_text("Turn cancelled by user", Duration::from_secs(15))
        .expect("Ctrl+C on the empty prompt must cancel the still-running turn");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
