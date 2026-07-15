//! PTY: a parked wait produces two static markers — the park pushes "Turn
//! completed in X. 1 command still running…" and the turn that follows ends
//! with its own marker below. A prompt typed mid-park is cancel-and-send:
//! the shell silently cancels the parked turn (no "Turn cancelled by user"
//! marker) and runs the message as its OWN next turn, whose completion pushes
//! the second marker; the park line is never edited, so the transcript holds
//! BOTH markers with the park text intact, in order.
//!
//! Wire journey, fully flag-file driven — no timing windows: the model
//! backgrounds a flag-gated command, then runs a flag-gated foreground hold
//! while the test extracts the runtime task id from the request bodies
//! (`<task-id>` envelope; a UUID minted by the terminal actor, so it cannot
//! be scripted statically) and enqueues the blocking
//! `get_command_or_subagent_output(timeout)` on the real id — the pager
//! parks. Typing mid-park cancels-and-sends and the fixed-text reply ends
//! the new turn with the second, final marker.
#[allow(unused_imports)]
use super::common::*;

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "PTY e2e; run with cargo test -p xai-grok-pager --test pty_e2e -- --ignored"]
async fn endline_park_two_static_markers() {
    let content = ContentController::start().await.expect("start content");
    // Gates the background command the markers count (released at the end).
    let park_flag = content.home().join("endline_park_flag");
    // Gates the id-extraction hold: created once the wait script is enqueued.
    let id_ready_flag = content.home().join("endline_id_ready_flag");

    let gated_loop = |flag: &std::path::Path| {
        format!("while [ ! -e {} ]; do /bin/sleep 0.2; done", flag.display())
    };

    // Tool call 1: a flag-gated background command — the work both markers
    // snapshot ("1 command still running…").
    let bg_args = json!({
        "command": gated_loop(&park_flag),
        "description": "flag-gated command",
        "is_background": true
    })
    .to_string();
    content.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_tool_call_events(
            "call_endline_bg",
            "run_terminal_command",
            &bg_args,
        )),
    );
    content.enqueue_response(
        "/v1/chat/completions",
        ScriptedResponse::sse(chat_completions_tool_call_events_with_id(
            "call_endline_bg",
            "run_terminal_command",
            &bg_args,
        )),
    );

    // Tool call 2: the flag-gated foreground hold — the turn idles here (no
    // deadline) until the test has extracted the task id and enqueued the
    // wait script.
    let id_hold_args = json!({
        "command": gated_loop(&id_ready_flag),
        "description": "hold for id extraction"
    })
    .to_string();
    content.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_tool_call_events(
            "call_endline_id_hold",
            "run_terminal_command",
            &id_hold_args,
        )),
    );
    content.enqueue_response(
        "/v1/chat/completions",
        ScriptedResponse::sse(chat_completions_tool_call_events_with_id(
            "call_endline_id_hold",
            "run_terminal_command",
            &id_hold_args,
        )),
    );

    // Fallback for the cancel-and-sent prompt's turn: plain text ends it.
    content.set_response("ENDLINE_FINAL_ANSWER");

    let binary = pager_binary().expect("resolve pager binary");
    // --yolo skips the bash permission prompt; --trust skips the folder-trust gate.
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
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

    // The runtime task id rides in the tool result of the follow-up request
    // (the same request the id-hold script answers), inside a
    // <task-id>…</task-id> envelope.
    let task_id = poll_for(Duration::from_secs(60), || {
        content
            .request_bodies()
            .iter()
            .find_map(|b| extract_task_id(&b.to_string()))
    })
    .unwrap_or_else(|| {
        panic!(
            "no <task-id> in any request body\n--- non-system messages ---\n{}\n--- screen ---\n{}",
            dump_non_system_messages(&content.request_bodies()),
            harness.screen_contents()
        )
    });

    // Tool call 3: block on the REAL task — the interruptible wait the pager
    // parks on (600s survives the wait cap; the send-now cancel aborts it).
    let wait_args = json!({
        "task_ids": [task_id],
        "timeout_ms": 600_000
    })
    .to_string();
    content.enqueue_response(
        "/v1/responses",
        ScriptedResponse::sse(responses_api_tool_call_events(
            "call_endline_wait",
            "get_command_or_subagent_output",
            &wait_args,
        )),
    );
    content.enqueue_response(
        "/v1/chat/completions",
        ScriptedResponse::sse(chat_completions_tool_call_events_with_id(
            "call_endline_wait",
            "get_command_or_subagent_output",
            &wait_args,
        )),
    );

    // Everything downstream is scripted — let the id-extraction hold finish.
    std::fs::write(&id_ready_flag, b"ready").expect("release id-extraction hold");

    // Park: the first static marker reads as a completion with the count.
    harness
        .wait_for_text("1 command still running", Duration::from_secs(90))
        .unwrap_or_else(|_| {
            panic!(
                "parked marker never appeared; screen:\n{}\n--- non-system messages ---\n{}",
                harness.screen_contents(),
                dump_non_system_messages(&content.request_bodies())
            )
        });
    assert!(
        harness.screen_contents().contains("Worked for"),
        "the parked marker keeps the completion prefix; screen:\n{}",
        harness.screen_contents()
    );

    // Type mid-park: Enter is cancel-and-send (the wait makes it a sendable
    // parked turn) — the parked turn is cancelled silently and the message
    // runs as its own next turn.
    harness
        .inject_keys(b"hurry up please")
        .expect("type mid-park");
    harness.update(Duration::from_millis(300));
    harness.inject_keys(b"\r").expect("submit mid-park prompt");

    harness
        .wait_for_text("ENDLINE_FINAL_ANSWER", Duration::from_secs(90))
        .unwrap_or_else(|_| {
            panic!(
                "cancel-and-sent turn never settled; screen:\n{}",
                harness.screen_contents()
            )
        });

    // The turn-start adoption scrolls the promoted "❯ hurry up please" block
    // to the viewport top, pushing the park marker above the screen. Scroll
    // the transcript back to its head so both markers are inspectable.
    harness.inject_keys(b"\t").expect("focus scrollback (tab)");
    harness.update(Duration::from_millis(300));
    harness.inject_keys(b"g").expect("goto transcript top");

    // Two static markers: the park line unchanged above the promoted prompt
    // and the new turn's final marker (also counting the still-gated command)
    // below it — with NO cancelled marker anywhere (silent send-now cancel).
    let two_markers = wait_until(Duration::from_secs(90), || {
        harness.update(Duration::from_millis(100));
        let screen = harness.screen_contents();
        // Positional: park marker ABOVE the promoted prompt ABOVE the final
        // marker (screen text is row-major), both markers intact.
        screen.matches("Worked for").count() == 2
            && screen.matches("1 command still running").count() == 2
            && !screen.contains("Turn cancelled by user")
            && matches!(
                (
                    screen.find("Worked for"),
                    screen.find("hurry up please"),
                    screen.rfind("Worked for"),
                ),
                (Some(park), Some(prompt), Some(fin)) if park < prompt && prompt < fin
            )
    });
    assert!(
        two_markers,
        "expected park marker, promoted prompt, then the fresh final marker in order \
         (and no cancelled marker); screen:\n{}",
        harness.screen_contents()
    );

    write_cast_if_requested(&harness, "endline_park_two_static_markers.cast");

    // Release the flag-gated command so nothing outlives the harness teardown.
    std::fs::write(&park_flag, b"done").expect("release flag");
}
