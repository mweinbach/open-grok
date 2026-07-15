// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// Mid-turn Esc from the SCROLLBACK pane is a swallowed no-op: it must NOT
/// cancel the running turn. Cancel remains on Ctrl+C / palette / etc.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn esc_mid_turn_from_scrollback_is_swallowed() {
    let content = ContentController::start().await.expect("start content");
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

    // Leave the prompt with a SINGLE Tab, then wait for the footer to prove the
    // scrollback owns keys. Tab TOGGLES focus, so re-pressing it could bounce
    // focus back to the prompt — press once and poll the render instead.
    harness.inject_keys(b"\t").expect("tab to scrollback");
    harness
        .wait_for_text("Space:prompt", Duration::from_secs(10))
        .expect("scrollback must own keys before the mid-turn Esc");

    // 1× Esc from scrollback must swallow (not cancel).
    harness.inject_keys(keys::ESC).expect("press esc");
    harness.update(Duration::from_millis(1000));
    let screen = harness.screen_contents();
    assert!(
        !screen.contains("Turn cancelled by user"),
        "mid-turn Esc from scrollback must NOT cancel\nscreen:\n{screen}"
    );

    // Positive tail: prove the turn was still alive at Esc-time (the negative
    // check above would false-pass on an already-finished turn) and that
    // Ctrl+C — the replacement cancel gesture — works from the scrollback pane.
    harness.inject_keys(keys::CTRL_C).expect("press ctrl+c");
    harness
        .wait_for_text("Turn cancelled by user", Duration::from_secs(15))
        .expect("Ctrl+C from scrollback must cancel the still-running turn");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
