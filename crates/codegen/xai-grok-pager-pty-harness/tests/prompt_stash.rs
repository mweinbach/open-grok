use std::time::Duration;

use anyhow::{Context, Result};
use xai_grok_pager_pty_harness::{ContentController, PtyHarness, pager_binary};

const ROWS: u16 = 50;
const COLS: u16 = 120;
const CANARY: &str = "STASHCANARY99";
const CTRL_CANARY: &str = "CTRLSTASHCANARY7";
const STASH_CAPTION: &str = "Stashed";
const ALT_S: &[u8] = b"\x1bs";
const CTRL_S: &[u8] = b"\x13";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn chord_bytes_stash_and_restore_the_draft() {
    run().await.expect("stash/pop e2e");
}

async fn run() -> Result<()> {
    let content = ContentController::start()
        .await
        .context("start mock server")?;

    let project = tempfile::tempdir().context("project dir")?;
    std::fs::create_dir_all(project.path().join(".git")).context("create .git")?;

    let binary = pager_binary().context("resolve pager binary")?;
    let mut pager = PtyHarness::spawn_with_content_in_dir(
        &binary,
        ROWS,
        COLS,
        &content,
        &[],
        Some(project.path()),
    )
    .context("spawn pager")?;
    pager
        .wait_for_text("Quit", Duration::from_secs(20))
        .context("welcome screen")?;

    pager.inject_keys(CANARY.as_bytes()).context("type draft")?;
    pager
        .wait_for_text(CANARY, Duration::from_secs(10))
        .context("draft rendered in composer")?;

    pager.inject_keys(ALT_S).context("alt+s stash")?;
    pager
        .wait_for_text(STASH_CAPTION, Duration::from_secs(10))
        .context("caption painted on the prompt border")?;
    pager
        .wait_for_text_absent(CANARY, Duration::from_secs(10))
        .context("composer cleared by stash")?;

    assert!(
        !pager.contains_text("again to clear"),
        "bare-Esc clear arm fired, so ESC+s split-decoded:\n{}",
        pager.screen_contents()
    );

    pager.inject_keys(ALT_S).context("alt+s pop")?;
    pager
        .wait_for_text(CANARY, Duration::from_secs(10))
        .context("draft restored by pop")?;
    pager
        .wait_for_text_absent(STASH_CAPTION, Duration::from_secs(10))
        .context("caption clears once the slot is empty")?;

    pager.inject_keys(CTRL_S).context("ctrl+s stash")?;
    pager
        .wait_for_text_absent(CANARY, Duration::from_secs(10))
        .context("first draft re-stashed by ctrl+s")?;

    pager
        .inject_keys(CTRL_CANARY.as_bytes())
        .context("type second draft")?;
    pager
        .wait_for_text(CTRL_CANARY, Duration::from_secs(10))
        .context("second draft rendered")?;

    pager.inject_keys(CTRL_S).context("ctrl+s stash second")?;
    pager
        .wait_for_text_absent(CTRL_CANARY, Duration::from_secs(10))
        .context("second draft stashed by ctrl+s (replacing the first)")?;

    assert!(
        !pager.contains_text("Resume session"),
        "session picker must not open while a draft is stashable:\n{}",
        pager.screen_contents()
    );

    pager.inject_keys(CTRL_S).context("ctrl+s pop second")?;
    pager
        .wait_for_text(CTRL_CANARY, Duration::from_secs(10))
        .context("second draft restored by ctrl+s pop")?;

    assert!(
        !pager.contains_text("panicked"),
        "pager panicked:\n{}",
        pager.screen_contents()
    );

    pager.quit().context("quit pager")?;
    Ok(())
}
