// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// Esc does not cancel a running turn in minimal mode. It commits one
/// "Press Ctrl+c to cancel the turn" system line and leaves the stream
/// running. The prompt is always focused, so the turn-running Esc branch
/// wins over idle clear/rewind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_esc_mid_turn_hints_instead_of_cancelling() {
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

    harness
        .wait_for_full_text("Press Ctrl+c to cancel the turn", Duration::from_secs(10))
        .expect("minimal mid-turn Esc must commit the Ctrl+C hint");
    assert!(
        !harness.contains_text("Turn cancelled by user"),
        "minimal Esc must not cancel\nscreen:\n{}",
        harness.screen_contents()
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
