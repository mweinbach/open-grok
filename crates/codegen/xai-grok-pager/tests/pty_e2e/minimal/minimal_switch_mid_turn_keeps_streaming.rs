#[allow(unused_imports)]
use crate::common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_switch_mid_turn_keeps_streaming() {
    let content = ContentController::start().await.expect("start content");
    let turn = content.expect_agent_turn_blocked(
        "turn running across the mode switch",
        slow_turn_text("MIDTURN"),
    );

    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--no-leader"],
        Some(project.path()),
    )
    .expect("spawn fullscreen pager");
    harness.set_respond_to_queries(true);

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");

    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit turn");
    harness
        .wait_for_text("MIDTURN", Duration::from_secs(30))
        .expect("streamed content visible mid-turn");

    let requests_before_switch = content.request_bodies().len();

    inject_keys_paced(&mut harness, b"/minimal");
    harness
        .wait_for_text(
            "Switch this session to minimal (scrollback-native) mode",
            Duration::from_secs(5),
        )
        .expect("slash dropdown offers /minimal mid-turn");
    harness.update(Duration::from_millis(150));
    harness.inject_keys(b"\r").expect("submit /minimal");

    harness
        .wait_for_full_text("MIDTURN", Duration::from_secs(30))
        .expect("streamed content survives the switch");

    turn.release();
    harness
        .wait_for_full_text("streaming29", Duration::from_secs(45))
        .unwrap_or_else(|error| {
            panic!(
                "gated turn did not complete after the mid-turn switch: {error}\nfull:\n{}",
                harness.full_text()
            )
        });
    harness
        .wait_for_text(MINIMAL_SWITCH_BACK_IDLE_SENTINEL, Duration::from_secs(30))
        .unwrap_or_else(|error| {
            panic!(
                "minimal idle status missing after mid-turn switch: {error}\nscreen:\n{}",
                harness.screen_contents()
            )
        });
    assert!(
        harness.full_text().contains("streaming29"),
        "streamed tail missing from committed transcript after idle\nfull:\n{}",
        harness.full_text()
    );

    assert!(
        !harness.full_text().contains("Reopening session"),
        "mid-turn /minimal must not re-exec; found reopen text:\n{}",
        harness.full_text()
    );
    assert_eq!(
        content.request_bodies().len(),
        requests_before_switch,
        "the switch must not issue new model requests (no killed + resumed turn)\nrequests:\n{}",
        dump_non_system_messages(&content.request_bodies())
    );
    assert!(
        !harness.contains_text("panicked"),
        "pager panicked after mid-turn /minimal\nscreen:\n{}",
        harness.screen_contents()
    );

    quit_minimal(&mut harness);
}
