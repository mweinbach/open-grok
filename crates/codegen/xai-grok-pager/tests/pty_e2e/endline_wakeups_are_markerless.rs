//! PTY: auto-wake turns close MARKERLESS — a turn ends with three flag-gated
//! background commands running (one plain "Worked for" marker), and each
//! released flag lands a completion chip and the auto-wake response with NO
//! wake-end marker after it, while every earlier line stays unchanged above
//! (nothing mutates). The persistent "watching · N commands" status row above
//! the prompt counts the remaining work down between wakes and disappears
//! once nothing is left; no "still running" copy appears anywhere.
//!
//! Positional chain asserted at the end: marker < chip < wake reply < chip <
//! reply < chip < reply — exactly ONE "Worked for" total (the user turn's).
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
#[ignore = "PTY e2e; run the owning pty_e2e_* Cargo test with --ignored (see Cargo.toml)"]
async fn endline_wakeups_are_markerless() {
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

    // The turn ends with all three commands running: one plain marker, and
    // the status row's watching cue carrying the count.
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
        .wait_for_text("Worked for", Duration::from_secs(30))
        .unwrap_or_else(|_| {
            panic!(
                "the end marker never appeared; screen:\n{}",
                harness.screen_contents()
            )
        });
    harness
        .wait_for_text("watching · 3 commands", Duration::from_secs(30))
        .unwrap_or_else(|_| {
            panic!(
                "the watching cue never showed the running count; screen:\n{}",
                harness.screen_contents()
            )
        });

    // Release flag 0: chip → wake reply, NO wake marker; the watching cue
    // counts down to 2 while the one user-turn marker stays intact above
    // (screen text is row-major, so find offsets order the lines).
    std::fs::write(&flags[0], b"done").expect("release flag 0");
    let wake_one = wait_until(Duration::from_secs(45), || {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        screen.contains("WAKE_REPLY_ONE")
            && screen.matches("Worked for").count() == 1
            && screen.contains("watching · 2 commands")
    });
    assert!(
        wake_one,
        "expected chip → wake reply with no wake marker, watching cue at 2; screen:\n{}",
        harness.screen_contents()
    );

    // Release flag 1: the second wake chain joins below; cue counts 1.
    std::fs::write(&flags[1], b"done").expect("release flag 1");
    let wake_two = wait_until(Duration::from_secs(45), || {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        screen.contains("WAKE_REPLY_TWO")
            && screen.matches("Worked for").count() == 1
            && screen.contains("watching · 1 command")
    });
    assert!(
        wake_two,
        "expected the second markerless wake chain below the earlier lines; screen:\n{}",
        harness.screen_contents()
    );

    // Release flag 2: zero left — still exactly one marker, and the watching
    // cue disappears entirely.
    std::fs::write(&flags[2], b"done").expect("release flag 2");
    let wake_three = wait_until(Duration::from_secs(45), || {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        screen.contains("WAKE_REPLY_THREE")
            && screen.matches("Worked for").count() == 1
            && !screen.contains("watching ·")
    });
    assert!(
        wake_three,
        "the last wake must stay markerless and retire the watching cue; screen:\n{}",
        harness.screen_contents()
    );

    // Full chain, positional: marker < chip < reply < chip < reply < chip <
    // reply — one marker total, and ZERO "still running" lines anywhere.
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
    let markers: Vec<usize> = screen.match_indices("Worked for").map(|(i, _)| i).collect();
    assert_eq!(
        markers.len(),
        1,
        "exactly one marker — the user turn's; screen:\n{screen}"
    );
    let w1 = screen.find("WAKE_REPLY_ONE").expect("wake reply 1");
    let w2 = screen.find("WAKE_REPLY_TWO").expect("wake reply 2");
    let w3 = screen.find("WAKE_REPLY_THREE").expect("wake reply 3");
    assert!(
        markers[0] < chips[0]
            && chips[0] < w1
            && w1 < chips[1]
            && chips[1] < w2
            && w2 < chips[2]
            && chips[2] < w3,
        "chain out of order; screen:\n{screen}"
    );
    assert_eq!(
        screen.matches("still running").count(),
        0,
        "no still-running copy may appear in the transcript; screen:\n{screen}"
    );

    write_cast_if_requested(&harness, "endline_wakeups_are_markerless.cast");
}
