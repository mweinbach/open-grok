use super::common::*;
use anyhow::Result;
use chrono::{TimeDelta, Utc};
use serde_json::Value;
use std::fs;

const OLDEST_TITLE: &str = "Archived conversation";
const SAVED_REPLY: &str = "PAGINATEDREPLAY";

fn seed_picker_pages(saved_session: &Path) -> Result<()> {
    let summary_path = saved_session.join("summary.json");
    let mut summary: Value = serde_json::from_slice(&fs::read(&summary_path)?)?;
    summary["last_active_at"] = json!("2025-01-01T00:00:00Z");
    summary["created_at"] = summary["last_active_at"].clone();
    summary["updated_at"] = summary["last_active_at"].clone();
    summary["session_summary"] = json!(OLDEST_TITLE);
    summary["generated_title"] = json!(OLDEST_TITLE);
    fs::write(summary_path, serde_json::to_vec(&summary)?)?;

    // Only the oldest row needs a replayable transcript. A local list entry
    // is fully described by its summary, so no extra model turns are needed.
    let project_sessions = saved_session.parent().expect("project sessions directory");
    let yesterday = Utc::now() - TimeDelta::days(1);
    for i in 1..=31 {
        let id = format!("10000000-0000-4000-8000-{i:012}");
        let dir = project_sessions.join(&id);
        fs::create_dir(&dir)?;
        summary["info"]["id"] = json!(id);
        summary["last_active_at"] = json!(yesterday + TimeDelta::seconds(i));
        summary["created_at"] = summary["last_active_at"].clone();
        summary["updated_at"] = summary["last_active_at"].clone();
        summary["session_summary"] = json!(format!("Recent session {i:02}"));
        summary["generated_title"] = summary["session_summary"].clone();
        fs::write(dir.join("summary.json"), serde_json::to_vec(&summary)?)?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the open-grok binary and a PTY"]
async fn resume_browses_older_sessions() -> Result<()> {
    let project = tempfile::tempdir()?;
    fs::create_dir(project.path().join(".git"))?;
    let content = ContentController::start().await?;
    content.set_response(SAVED_REPLY);
    let binary = pager_binary()?;
    let spawn = || {
        PtyHarness::spawn_with_content_in_dir(
            &binary,
            80, // The first and second page both fit on screen.
            DEFAULT_COLS,
            &content,
            &["--no-leader"],
            Some(project.path()),
        )
    };

    let mut original = spawn()?;
    original.wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)?;
    original.inject_keys(b"remember this reply\r")?;
    original.wait_for_text(SAVED_REPLY, RESUME_TIMEOUT)?;
    let saved_session = session_dir(&content, &mut original);
    original.inject_keys(b"\x11")?;
    original.update(Duration::from_millis(200));
    original.inject_keys(b"\x11")?;
    assert_eq!(
        wait_for_exit_status(&mut original, WELCOME_TIMEOUT)?,
        PtyExitPoll::Exited(0)
    );
    seed_picker_pages(&saved_session)?;
    content.set_response("Fresh conversation reply");

    let mut picker = spawn()?;
    picker.wait_for_text(WELCOME_SCREEN_SENTINEL, WELCOME_TIMEOUT)?;
    inject_keys_paced(&mut picker, b"/resume");
    picker.inject_keys(keys::ENTER)?;
    picker.wait_for_text("Recent session 31", WELCOME_TIMEOUT)?;
    // The shell can include the current session in its 30-row response.
    // The modal hides it, leaving either 29 or 30 visible fixture rows.
    picker.wait_for_text("Recent session 03", WELCOME_TIMEOUT)?;
    assert!(!picker.contains_text("Recent session 01"));
    assert!(!picker.contains_text(OLDEST_TITLE));

    picker.inject_keys(b"\x1b[15~")?; // F5 loads the next page.
    picker.wait_for_text("Recent session 01", WELCOME_TIMEOUT)?;
    picker.wait_for_text(OLDEST_TITLE, WELCOME_TIMEOUT)?;
    assert!(picker.contains_text("Recent session 31"));
    assert!(!picker.contains_text(SAVED_REPLY));
    picker.inject_keys(keys::DOWN)?; // Move focus from the search field into the list.
    picker.inject_keys(b"\x1b[F")?; // End selects the last session.
    picker.inject_keys(keys::ENTER)?;
    picker.wait_for_text(SAVED_REPLY, RESUME_TIMEOUT)?;
    picker.quit()
}
