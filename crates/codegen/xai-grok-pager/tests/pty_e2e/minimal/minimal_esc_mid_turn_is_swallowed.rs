// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// Mid-turn Esc in minimal mode is a swallowed no-op (the prompt is always
/// focused). Esc must NOT cancel; cancel remains on Ctrl+C.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_esc_mid_turn_is_swallowed() {
    let content = ContentController::start().await.expect("start content");
    // Paced, long stream so the turn is provably still running when Esc lands.
    let long = format!(
        "{MOCK_RESPONSE_SENTINEL} {}",
        "streaming filler words for the cancellation window. ".repeat(120)
    );
    content.set_response(long);
    content.set_chunk_delay(Some(Duration::from_millis(50)));

    let mut harness = spawn_minimal(&content);
    wait_minimal_ready(&mut harness);

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("turn streaming in the live tail");

    harness.inject_keys(keys::ESC).expect("press esc");
    harness.update(Duration::from_millis(1000));

    // Full-text: minimal commits the cancel marker to native scrollback, so it
    // may sit above the pinned viewport — check scrollback + screen.
    assert!(
        !harness.contains_full_text("Turn cancelled by user"),
        "mid-turn Esc must NOT cancel in minimal mode\nfull contents:\n{}",
        harness.full_text()
    );

    // Positive tail: prove the turn was still alive at Esc-time (the negative
    // check above would false-pass on an already-finished turn) and that
    // Ctrl+C — the replacement cancel gesture — works in minimal mode. The
    // prompt is empty and the turn is running, so Ctrl+C cancels (the minimal
    // quit arm applies only to an idle empty prompt).
    harness.inject_keys(keys::CTRL_C).expect("press ctrl+c");
    harness
        .wait_for_full_text("Turn cancelled by user", Duration::from_secs(15))
        .expect("Ctrl+C must cancel the still-running turn in minimal mode");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
