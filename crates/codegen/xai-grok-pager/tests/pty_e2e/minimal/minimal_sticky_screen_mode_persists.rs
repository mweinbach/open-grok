// Per-test-case module for the `pty_e2e` integration test crate.
#[allow(unused_imports)]
use crate::common::*;

/// Sticky screen mode: an explicit `--minimal` persists
/// `[ui] screen_mode = "minimal"` to the isolated `config.toml`, and a later
/// plain launch (no flag) in the same GROK_HOME reopens in minimal mode. The
/// fullscreen direction of the sticky write is covered by
/// `minimal_slash_switches_to_fullscreen`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn minimal_sticky_screen_mode_persists() {
    let content = ContentController::start().await.expect("start content");
    content.set_response(format!("{} sticky payload.", turn_sentinel(1)));

    // Sessions are keyed by cwd: both runs must share a stable project dir.
    let project = tempfile::tempdir().expect("create project dir");
    std::fs::create_dir_all(project.path().join(".git")).expect("create .git");

    // First run: explicit `--minimal` — this is the write that must stick.
    let mut first = spawn_minimal_in_dir(&content, DEFAULT_ROWS, DEFAULT_COLS, &[], project.path());
    wait_minimal_ready(&mut first);

    // The startup persist is fire-and-forget; poll the isolated config.toml
    // (pumping the PTY so the pager never blocks on a full buffer).
    let config_path = content.home().join(".grok").join("config.toml");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let body = std::fs::read_to_string(&config_path).unwrap_or_default();
        if body.contains("screen_mode = \"minimal\"") {
            break;
        }
        if Instant::now() >= deadline {
            panic!("--minimal never persisted [ui] screen_mode; config.toml:\n{body}");
        }
        first.update(Duration::from_millis(100));
    }
    quit_minimal(&mut first);

    // Second run: NO mode flag. The sticky preference alone must select
    // minimal (query forwarding on, or the inline probe would downgrade the
    // run to full-screen inline and mask a regression as a pass-through).
    let binary = pager_binary().expect("resolve pager binary");
    let mut second = PtyHarness::spawn_with_content_in_dir(
        &binary,
        DEFAULT_ROWS,
        DEFAULT_COLS,
        &content,
        &["--no-leader"],
        Some(project.path()),
    )
    .expect("spawn plain pager");
    second.set_respond_to_queries(true);

    // Minimal has no welcome screen; its idle status line is the readiness
    // sentinel and proves the sticky preference selected minimal.
    wait_minimal_ready(&mut second);

    assert!(
        !second.contains_text("panicked"),
        "pager panicked\nscreen:\n{}",
        second.screen_contents()
    );

    quit_minimal(&mut second);
}
