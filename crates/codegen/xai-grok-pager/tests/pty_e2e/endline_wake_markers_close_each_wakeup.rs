//! PTY: every wake turn ends with a real marker — a turn ends with three
//! flag-gated background commands running ("3 commands still running…"), and
//! each released flag lands a completion chip, the auto-wake response, then a
//! FRESH wake-end marker snapshotting the remaining counts ("2 …", "1 …"),
//! while every earlier line stays unchanged above (nothing mutates). The last
//! wake's marker is the plain form ("Worked for X." — zero left), and
//! no after-chip work-only status lines appear anywhere: the shell stamps
//! `will_wake` on each completion, so the wake markers carry the counts.
//!
//! Positional chain asserted at the end: marker(3) < chip < wake reply <
//! marker(2) < chip < reply < marker(1) < chip < reply < plain final marker.
#[allow(unused_imports)]
use super::common::*;

/// One flag-gated background command per released flag file.
#[cfg(unix)]
const TASKS: usize = 3;

/// Taller than [`DEFAULT_ROWS`]: the full chain (initial turn + three wake
/// turns) must stay on screen at once for the positional asserts.
#[cfg(unix)]
const ROWS: u16 = 70;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run with cargo test -p xai-grok-pager --test pty_e2e -- --ignored"]
async fn endline_wake_markers_close_each_wakeup() {
    let content = ContentController::start().await.expect("start content");
    let flags: Vec<std::path::PathBuf> = (0..TASKS)
        .map(|i| content.home().join(format!("endline_status_flag_{i}")))
        .collect();

    // The turn backgrounds one flag-gated command per tool call…
    for (i, flag) in flags.iter().enumerate() {
        let args = json!({
            "command": format!(
                "while [ ! -e {} ]; do /bin/sleep 0.2; done",
                flag.display()
            ),
            "description": format!("flag-gated command {i}"),
            "is_background": true
        })
        .to_string();
        let call_id = format!("call_endline_status_{i}");
        content.enqueue_response(
            "/v1/responses",
            ScriptedResponse::sse(responses_api_tool_call_events(
                &call_id,
                "run_terminal_command",
                &args,
            )),
        );
        content.enqueue_response(
            "/v1/chat/completions",
            ScriptedResponse::sse(chat_completions_tool_call_events_with_id(
                &call_id,
                "run_terminal_command",
                &args,
            )),
        );
    }
    // …then a text response ends it with all three still running, and each
    // auto-wake turn consumes one distinct scripted reply (FIFO per path; the
    // stage gating below keeps the consumption order deterministic).
    for text in [
        "STATUS_TURN_SETTLED",
        "WAKE_REPLY_ONE",
        "WAKE_REPLY_TWO",
        "WAKE_REPLY_THREE",
    ] {
        content.enqueue_response(
            "/v1/responses",
            ScriptedResponse::sse(responses_api_message_events(text)),
        );
        content.enqueue_response(
            "/v1/chat/completions",
            ScriptedResponse::sse(chat_completions_message_events(text)),
        );
    }
    content.set_response("STATUS_FALLBACK");

    let binary = pager_binary().expect("resolve pager binary");
    // --yolo skips the bash permission prompt; --trust skips the folder-trust gate.
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        ROWS,
        DEFAULT_COLS,
        &content,
        &["--yolo", "--trust"],
        Some(content.home()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");

    // The turn ends with all three commands running: the final marker
    // carries the snapshot count.
    harness
        .wait_for_text("STATUS_TURN_SETTLED", Duration::from_secs(60))
        .unwrap_or_else(|_| {
            panic!(
                "turn never settled; screen:\n{}\n--- non-system messages ---\n{}",
                harness.screen_contents(),
                dump_non_system_messages(&content.request_bodies())
            )
        });
    harness
        .wait_for_text("3 commands still running", Duration::from_secs(30))
        .unwrap_or_else(|_| {
            panic!(
                "marker never showed the snapshot count; screen:\n{}",
                harness.screen_contents()
            )
        });
    assert!(
        harness.screen_contents().contains("Worked for"),
        "the marker keeps the completion prefix; screen:\n{}",
        harness.screen_contents()
    );

    // Release flag 0: chip → wake reply → a fresh "2 commands" wake-end
    // marker below, with the original "3 commands" marker intact above
    // (screen text is row-major, so find offsets order the lines).
    std::fs::write(&flags[0], b"done").expect("release flag 0");
    let wake_one = wait_until(Duration::from_secs(45), || {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        matches!(
            (
                screen.find("3 commands still running"),
                screen.find("WAKE_REPLY_ONE"),
                screen.find("2 commands still running"),
            ),
            (Some(three), Some(reply), Some(two)) if three < reply && reply < two
        )
    });
    assert!(
        wake_one,
        "expected chip → wake reply → fresh '2 commands' marker below the intact '3 commands' one; screen:\n{}",
        harness.screen_contents()
    );

    // Release flag 1: the "1 command" wake marker joins below its reply.
    std::fs::write(&flags[1], b"done").expect("release flag 1");
    let wake_two = wait_until(Duration::from_secs(45), || {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        matches!(
            (
                screen.find("2 commands still running"),
                screen.find("WAKE_REPLY_TWO"),
                screen.find("1 command still running"),
            ),
            (Some(two), Some(reply), Some(one)) if two < reply && reply < one
        )
    });
    assert!(
        wake_two,
        "expected the second wake chain below the earlier lines; screen:\n{}",
        harness.screen_contents()
    );

    // Release flag 2: zero left — the last wake ends with the PLAIN marker
    // (fourth "Worked for", no new "still running" suffix).
    std::fs::write(&flags[2], b"done").expect("release flag 2");
    let wake_three = wait_until(Duration::from_secs(45), || {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        screen.contains("WAKE_REPLY_THREE") && screen.matches("Worked for").count() == 4
    });
    assert!(
        wake_three,
        "the final plain wake marker never landed; screen:\n{}",
        harness.screen_contents()
    );

    // Full chain, positional: marker(3) < chip < reply < marker(2) < chip <
    // reply < marker(1) < chip < reply < plain final marker — and exactly
    // three "still running" lines total (the markers'), i.e. the stamped
    // `will_wake` suppressed every after-chip work-only status line.
    let screen = harness.screen_contents();
    let chips: Vec<usize> = screen
        .match_indices("Task completed")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        chips.len(),
        TASKS,
        "one completion chip per task; screen:\n{screen}"
    );
    let m3 = screen.find("3 commands still running").expect("marker 3");
    let w1 = screen.find("WAKE_REPLY_ONE").expect("wake reply 1");
    let m2 = screen.find("2 commands still running").expect("marker 2");
    let w2 = screen.find("WAKE_REPLY_TWO").expect("wake reply 2");
    let m1 = screen.find("1 command still running").expect("marker 1");
    let w3 = screen.find("WAKE_REPLY_THREE").expect("wake reply 3");
    let final_marker = screen
        .match_indices("Worked for")
        .map(|(i, _)| i)
        .last()
        .expect("final marker");
    assert!(
        m3 < chips[0]
            && chips[0] < w1
            && w1 < m2
            && m2 < chips[1]
            && chips[1] < w2
            && w2 < m1
            && m1 < chips[2]
            && chips[2] < w3
            && w3 < final_marker,
        "chain out of order; screen:\n{screen}"
    );
    assert_eq!(
        screen.matches("still running").count(),
        3,
        "wake-bound completions must not add work-only status lines; screen:\n{screen}"
    );

    write_cast_if_requested(&harness, "endline_wake_markers_close_each_wakeup.cast");
}
