// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use super::common::*;

/// 15. **Feature off => no trust question.**
/// With `GROK_FOLDER_TRUST=0` (explicit opt-out) the feature is off, so the repo
/// boots straight to the welcome (the default is now on).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn folder_trust_feature_off_shows_no_question() {
    let content = ContentController::start().await.expect("start content");
    let repo = git_repo_with_mcp_json();
    let env = trust_env(&content, false);
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let cwd = repo.path().to_str().expect("utf8 repo path");

    let binary = pager_binary().expect("resolve pager binary");
    let mut harness = PtyHarness::new(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &["--cwd", cwd],
        &env_refs,
    )
    .expect("spawn pager");

    // Normal welcome boots; the question never appears.
    harness
        .wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)
        .expect("normal welcome renders");
    assert!(
        !harness.contains_text(TRUST_QUESTION_SENTINEL),
        "feature off must not render the trust question\nscreen:\n{}",
        harness.screen_contents()
    );

    harness.quit().expect("clean quit");
}
