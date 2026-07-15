// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// A queue-pane edit of a queued `!` row keeps bash semantics: the edited
/// command executes at drain and never reaches the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
#[cfg(unix)]
async fn verify_bashq_claim3_edit_keeps_bash() {
    let content = ContentController::start().await.expect("start content");
    content.set_chunk_delay(Some(Duration::from_millis(150)));
    let step_one = {
        let mut s = String::from("STEPONE");
        for i in 0..150 {
            s.push_str(&format!(" streaming{i}"));
        }
        s
    };
    content.set_turns([
        step_one,
        // Consumed only on an unfixed binary (the demoted-to-prompt drain).
        "STEPTHREE edited continuation.".to_owned(),
    ]);
    // Hold turn 1 open so the edit lands while the row is still queued.
    content.hold_agent_completions();

    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");
    let cwd = dunce::canonicalize(project.path()).expect("canonicalize project");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &[],
        Some(cwd.as_path()),
    )
    .expect("spawn pager");

    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("welcome text");
    harness
        .inject_keys(format!("{PROMPT}\r").as_bytes())
        .expect("submit prompt");
    harness
        .wait_for_text("STEPONE", Duration::from_secs(30))
        .expect("turn 1 streaming");

    harness
        .inject_keys(b"!printf 'CLAIMTHREE_%s_OK\\n' ORIG\r")
        .expect("submit bash-mode command mid-turn");
    harness
        .wait_for_text("CLAIMTHREE_%s_OK", Duration::from_secs(10))
        .expect("bash command visible as a queued row");

    harness
        .inject_keys(CTRL_SEMICOLON)
        .expect("focus queue pane");
    harness.update(Duration::from_millis(300));
    harness.inject_keys(b"e").expect("edit queued row");
    // A bash-row edit shows the bash info override, not "editing queued #N".
    harness
        .wait_for_text("Run shell command", Duration::from_secs(10))
        .expect("bash edit mode entered");

    // The cursor sits at 0, so the edit prepends and comments out the original.
    harness
        .inject_keys(b"printf 'CLAIMTHREE_%s_OK\\n' EDITED # ")
        .expect("type the edit");
    harness
        .wait_for_text("EDITED", Duration::from_secs(10))
        .expect("edited text echoes in the composer");
    harness.inject_keys(b"\r").expect("save the edit");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while harness.contains_text("Run shell command") {
        assert!(
            std::time::Instant::now() < deadline,
            "edit mode never exited after save\nscreen:\n{}",
            harness.screen_contents()
        );
        harness.update(Duration::from_millis(100));
    }
    harness
        .wait_for_text("EDITED", Duration::from_secs(10))
        .expect("queued row shows the edited text after the rebroadcast");

    harness.update(Duration::from_millis(500));
    content.release_agent_completions();
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    while !harness.contains_text("CLAIMTHREE_EDITED_OK") && !harness.contains_text("STEPTHREE") {
        assert!(
            std::time::Instant::now() < deadline,
            "neither bash execution nor model drain appeared \
             (ORIG ran instead: {})\nscreen:\n{}",
            harness.contains_text("CLAIMTHREE_ORIG_OK"),
            harness.screen_contents()
        );
        harness.update(Duration::from_millis(200));
    }

    let users = all_user_message_blobs(&content);
    assert!(
        !users.iter().any(|u| u.contains("CLAIMTHREE")),
        "EDIT LOST BASH SEMANTICS: edited bash row reached the model as a \
         plain prompt (and never executed): {users:#?}"
    );

    assert!(
        harness.contains_text("CLAIMTHREE_EDITED_OK"),
        "edited bash command never executed\nscreen:\n{}",
        harness.screen_contents()
    );
    harness
        .wait_for_text("Run (user)", Duration::from_secs(15))
        .expect("Run (user) chrome for the drained bash turn");

    assert!(
        !harness.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        harness.screen_contents()
    );
    harness.quit().expect("clean quit");
}
