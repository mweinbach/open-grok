// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// 19. **Send-now chord delivers the composer text as its own next turn.**
/// (Historical name: the chord used to interject into the SAME turn.)
/// Ctrl+Enter with text mid-stream is cancel-and-send: the running turn is
/// cancelled silently and the text runs as the next turn — a standard
/// `<user_query>` prompt with no interjection preamble, rendered as a "❯ "
/// user block via the turn-start adoption.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn interjection_reaches_model_in_same_turn() {
    let content = ContentController::start().await.expect("start content");
    // Gate turn 1's terminal event so the typed text + chord provably land
    // mid-turn regardless of suite load.
    content.hold_agent_completions();
    content.set_turns([
        slow_turn_text("TURNONE"),
        "TURNTWO reply to the sent-now message.".to_owned(),
    ]);

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
        .wait_for_text("TURNONE", Duration::from_secs(30))
        .expect("turn 1 streaming");

    harness
        .inject_keys(b"please also check the logs")
        .expect("type message");
    harness.inject_keys(CTRL_ENTER).expect("send-now chord");
    content.release_agent_completions();
    // Cancel-and-send: the message commits as a standard "❯ " prompt block
    // and runs as its own turn.
    harness
        .wait_for_text(
            "\u{276F} please also check the logs",
            Duration::from_secs(15),
        )
        .expect("send-now prompt block");
    harness
        .wait_for_text("TURNTWO", Duration::from_secs(40))
        .expect("sent-now message ran as the next turn");

    // The send-now cancel of turn 1 is silent.
    assert!(
        !harness.contains_text("Turn cancelled by user"),
        "send-now cancel must not render a cancelled marker\nscreen:\n{}",
        harness.screen_contents()
    );

    let users = all_user_messages(&content);
    let sent = users
        .iter()
        .find(|u| u.contains("please also check the logs"))
        .unwrap_or_else(|| panic!("sent-now message never reached the wire: {users:#?}"));
    assert!(
        !sent.contains(INTERJECTION_WIRE_PREFIX),
        "send-now must not use the interjection preamble: {sent}"
    );
    assert!(
        sent.contains("<user_query>"),
        "send-now must arrive as a standard user_query prompt: {sent}"
    );

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("clean quit");
}
