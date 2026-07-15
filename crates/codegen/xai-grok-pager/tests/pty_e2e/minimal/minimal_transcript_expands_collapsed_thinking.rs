// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// Reasoning text streamed by the mock. Must never appear in the answer text
/// so screen assertions can tell the two apart.
const REASONING_SENTINEL: &str = "REASONINGSENTINEL";

/// Dogfood bug: "I don't see thoughts in the transcript". With thinking
/// enabled (`[ui] show_thinking_blocks` — the default, set
/// explicitly here so the test doesn't depend on the rollout default),
/// minimal commits reasoning as a **collapsed** `Thought for Xs` header
/// (print-once display policy) — the body is intentionally not in the live
/// scrollback. The advertised full-fidelity `/transcript` view must therefore
/// render the thinking body **expanded**, or the reasoning is unreachable.
///
/// Flow: stream a reasoning+text turn → the answer commits, the reasoning
/// collapses to its header (body nowhere on screen) → `/transcript` with
/// `PAGER=cat` dumps the full view → the reasoning body appears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_transcript_expands_collapsed_thinking() {
    // The model must run on the Responses backend — reasoning summary deltas
    // are a Responses-API stream shape (the scripted events below).
    let content = ContentController::start_with_models(vec![
        MockModel::new("test-model").with_api_backend("responses"),
    ])
    .await
    .expect("start content");
    // The scripted turn streams reasoning deltas before the visible answer.
    // Two copies so an auxiliary request can't starve the prompt turn
    // (consumed FIFO; unconsumed scripts are dropped with the server).
    let reasoning = format!("{REASONING_SENTINEL} pondering syllables quietly");
    let answer = format!("{MOCK_RESPONSE_SENTINEL} the answer body.");
    for _ in 0..2 {
        content.enqueue_response(
            "/v1/responses",
            ScriptedResponse::sse(sse::responses_api_reasoning_and_text_events(
                &reasoning,
                &answer,
                "test-model",
            )),
        );
    }
    // Fallback mode for any further auxiliary traffic.
    content.set_response(answer.clone());

    // Thinking blocks explicitly ON (ingestion is gated on this toggle; the
    // sandbox `$HOME` starts with no config at all).
    std::fs::create_dir_all(content.home().join(".grok")).expect("mk .grok");
    std::fs::write(
        content.home().join(".grok/config.toml"),
        "[ui]\nshow_thinking_blocks = true\n",
    )
    .expect("write config");

    // Minimal env + PAGER=cat (non-interactive dump, same as
    // `minimal_transcript_opens_in_pager`).
    let mut env = content.env_for_pager();
    env.push(("PAGER".to_string(), "cat".to_string()));
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::new(&binary, DEFAULT_ROWS, DEFAULT_COLS, MINIMAL_ARGS, &env_refs)
        .expect("spawn minimal pager");
    harness.set_respond_to_queries(true);

    wait_minimal_ready(&mut harness);

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_full_text(MOCK_RESPONSE_SENTINEL, Duration::from_secs(30))
        .expect("turn committed");

    // The reasoning committed as its collapsed header: the body is NOT in the
    // live view (that's the print-once display policy, not a bug)…
    harness
        .wait_for_full_text("Thought for", Duration::from_secs(10))
        .expect("collapsed thinking header committed");
    assert!(
        !harness.full_text().contains(REASONING_SENTINEL),
        "reasoning body must be collapsed in the live view\nfull:\n{}",
        harness.full_text()
    );

    // …so the transcript is the only way to read it. cat dumps the full view.
    inject_keys_paced(&mut harness, b"/transcript");
    harness.inject_keys(b"\r").expect("submit /transcript");

    harness
        .wait_for_full_text(REASONING_SENTINEL, Duration::from_secs(15))
        .unwrap_or_else(|e| {
            panic!(
                "transcript must expand the collapsed thinking body: {e}\nfull:\n{}",
                harness.full_text()
            )
        });

    // And the inline TUI survives the suspend/restore round trip.
    harness
        .wait_for_text(MINIMAL_IDLE_SENTINEL, Duration::from_secs(10))
        .expect("inline TUI restored after the pager exited");
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
