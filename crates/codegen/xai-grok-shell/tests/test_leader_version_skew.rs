//! Two-binary version-skew tests: a real OLD released binary and a real NEW
//! binary sharing one leader socket. This is the only harness that exercises
//! cross-version eviction with real processes.
//!
//! Binaries are resolved per role:
//! - `GROK_BINARY_LEADER` — the binary that elects the initial leader
//!   (typically the latest released stable, e.g. fetched from
//!   `https://storage.googleapis.com/grok-build-public-artifacts/cli/grok-<ver>-linux-x86_64`).
//! - `GROK_BINARY_CLIENT` — the second client (typically a freshly built main).
//!
//! All tests are `#[ignore]`d: they need two pre-built binaries and spawn real
//! leader subprocesses. On-demand today — no CI lane runs them; invoke with:
//!
//! ```bash
//! GROK_BINARY_LEADER=/path/to/grok-old GROK_BINARY_CLIENT=/path/to/grok-new \
//!   cargo test -p xai-grok-shell --test test_leader_version_skew -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use xai_grok_shell::leader::{
    ClientCapabilities, ClientMode, ControlCommand, ControlPayload, LeaderClient,
};
use xai_grok_test_support::leader::{
    LeaderStdioClient, client_binary, leader_binary, leader_log, pid_alive, read_leader_pid,
    wait_for_live_leader, wait_for_new_leader, wait_for_replay_notifications,
};
use xai_grok_test_support::*;

/// Skew tests are meaningless when both roles resolve to the same binary
/// (e.g. a local `--ignored` run without the env vars): the version floor
/// never trips. Skip loudly instead of failing.
fn skew_binaries() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let old = leader_binary();
    let new = client_binary();
    if old == new {
        eprintln!(
            "SKIP: GROK_BINARY_LEADER/GROK_BINARY_CLIENT resolve to the same binary ({})",
            old.display()
        );
        return None;
    }
    Some((old, new))
}

async fn wait_for_pid_death(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

fn sandbox_unified_log(home: &Path) -> String {
    std::fs::read_to_string(home.join(".grok").join("logs").join("unified.jsonl"))
        .unwrap_or_default()
}

/// End-to-end version-skew: an old leader is running; a newer client connects,
/// evicts it under the version floor, spawns a replacement from its own
/// binary, and the old client's session survives via reconnect + reload.
#[tokio::test]
#[ignore = "two-binary version-skew test; set GROK_BINARY_LEADER/GROK_BINARY_CLIENT and run with --ignored"]
async fn new_client_evicts_old_leader_and_sessions_reload() {
    let Some((old_bin, new_bin)) = skew_binaries() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let workdir = git_workdir();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".grok")).unwrap();

            // Old binary elects the leader and completes a turn.
            let old_client = LeaderStdioClient::spawn_with_binary(
                &old_bin,
                &server,
                workdir.path(),
                home.path(),
            )
            .await;
            old_client.initialize().await;
            let session = old_client.create_session(workdir.path()).await;
            old_client
                .prompt(&session, "hello from the old world")
                .await
                .expect("pre-skew prompt failed");
            let old_pid = wait_for_live_leader(home.path(), Duration::from_secs(10))
                .await
                .expect("no live old leader");
            let base = old_client.notification_count();

            // New binary connects: version floor → evict → respawn.
            let new_client = LeaderStdioClient::spawn_with_binary(
                &new_bin,
                &server,
                workdir.path(),
                home.path(),
            )
            .await;
            new_client.initialize().await;

            let new_pid = wait_for_new_leader(home.path(), old_pid, Duration::from_secs(60))
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "no replacement leader after version-floor eviction\n\
                         old client stderr:\n{}\nnew client stderr:\n{}\nleader log:\n{}",
                        old_client.stderr_text(),
                        new_client.stderr_text(),
                        leader_log(home.path()),
                    )
                });
            assert_ne!(new_pid, old_pid);

            // The evicted leader must actually exit within the evict grace
            // (EVICT_WAIT_TIMEOUT is 8s; force-kill covers overruns).
            assert!(
                wait_for_pid_death(old_pid, Duration::from_secs(30)).await,
                "old leader pid {old_pid} still alive after eviction\nleader log:\n{}",
                leader_log(home.path()),
            );

            // The old client reconnects and its original session still works.
            wait_for_replay_notifications(&old_client, base, Duration::from_secs(60)).await;
            let res = old_client.prompt(&session, "after the eviction").await;
            assert!(
                res.is_ok(),
                "old client prompt after eviction failed: {:?}\nstderr:\n{}\nleader log:\n{}",
                res.err(),
                old_client.stderr_text(),
                leader_log(home.path()),
            );

            // And the new client works against the leader it spawned.
            let new_session = new_client.create_session(workdir.path()).await;
            new_client
                .prompt(&new_session, "hello from the new world")
                .await
                .expect("new client prompt failed");
        })
        .await;
}

/// New leader + old client: the older client adopts the newer leader (the
/// floor is directional — never downgrade), keeps functioning through
/// serde-default compat, and the leader records the version mismatch.
#[tokio::test]
#[ignore = "two-binary version-skew test; set GROK_BINARY_LEADER/GROK_BINARY_CLIENT and run with --ignored"]
async fn old_client_adopts_new_leader_and_still_functions() {
    let Some((old_bin, new_bin)) = skew_binaries() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let workdir = git_workdir();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".grok")).unwrap();

            // NEW binary elects the leader first.
            let new_client = LeaderStdioClient::spawn_with_binary(
                &new_bin,
                &server,
                workdir.path(),
                home.path(),
            )
            .await;
            new_client.initialize().await;
            let leader_pid = wait_for_live_leader(home.path(), Duration::from_secs(10))
                .await
                .expect("no live new leader");

            // OLD binary connects: must adopt (no downgrade eviction).
            let old_client = LeaderStdioClient::spawn_with_binary(
                &old_bin,
                &server,
                workdir.path(),
                home.path(),
            )
            .await;
            old_client.initialize().await;
            assert_eq!(
                read_leader_pid(home.path()),
                Some(leader_pid),
                "an older client must never evict a newer leader"
            );

            // Old client functions across the skew: session + prompt succeed,
            // exercising serde-default wire compat in anger.
            let session = old_client.create_session(workdir.path()).await;
            old_client
                .prompt(&session, "old client on new leader")
                .await
                .expect("old client prompt on new leader failed");

            // The leader records the client/leader version mismatch (the
            // x.ai/leader/version_mismatch notification's server-side warn).
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            let mut saw_mismatch = false;
            while tokio::time::Instant::now() < deadline {
                if leader_log(home.path()).contains("Version mismatch") {
                    saw_mismatch = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            assert!(
                saw_mismatch,
                "leader never logged the version mismatch\nleader log:\n{}",
                leader_log(home.path()),
            );
        })
        .await;
}

/// `grok update`'s relaunch signal against a REAL old leader: connect,
/// require `relaunch_v1`, send `RelaunchForUpdate`, and the leader exits so
/// the surviving client re-elects. Mirrors the private
/// `signal_leaders_to_relaunch` in `xai-grok-pager-bin/src/main.rs` (which is
/// bin-private, so the per-leader body is replicated here).
#[tokio::test]
#[ignore = "two-binary version-skew test; set GROK_BINARY_LEADER/GROK_BINARY_CLIENT and run with --ignored"]
async fn relaunch_for_update_drives_real_old_leader_to_exit() {
    let Some((old_bin, _new_bin)) = skew_binaries() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let workdir = git_workdir();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".grok")).unwrap();

            let old_client = LeaderStdioClient::spawn_with_binary(
                &old_bin,
                &server,
                workdir.path(),
                home.path(),
            )
            .await;
            old_client.initialize().await;
            let session = old_client.create_session(workdir.path()).await;
            old_client
                .prompt(&session, "before relaunch")
                .await
                .expect("pre-relaunch prompt failed");
            let old_pid = wait_for_live_leader(home.path(), Duration::from_secs(10))
                .await
                .expect("no live old leader");
            let base = old_client.notification_count();

            // The update-signal body, against the sandboxed socket.
            let control = LeaderClient::connect(
                home.path().join(".grok").join("leader.sock"),
                "grok-pager-update",
                ClientMode::Stdio,
                ClientCapabilities::default(),
            )
            .await
            .expect("control connect to old leader failed");

            if !control.registration().supports_relaunch() {
                // Pre-relaunch_v1 releases degrade to the manual-restart
                // message; nothing to drive here.
                eprintln!(
                    "SKIP: old leader {:?} does not advertise relaunch_v1",
                    control.registration().leader_binary_version
                );
                control.cancel();
                return;
            }

            let ack = control
                .send_control(ControlCommand::RelaunchForUpdate {
                    to_version: "999.0.0".to_string(),
                })
                .await;
            control.cancel();
            match ack {
                Ok(Ok(ControlPayload::Relaunching { .. })) => {}
                // The leader may exit before the ack flushes — acceptable.
                Err(_) => {}
                other => panic!("unexpected RelaunchForUpdate reply: {other:?}"),
            }

            assert!(
                wait_for_pid_death(old_pid, Duration::from_secs(30)).await,
                "old leader pid {old_pid} did not exit after accepting relaunch\nleader log:\n{}",
                leader_log(home.path()),
            );

            // The surviving client re-elects and restores its session.
            wait_for_new_leader(home.path(), old_pid, Duration::from_secs(60))
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "no re-elected leader after relaunch\nstderr:\n{}\nleader log:\n{}",
                        old_client.stderr_text(),
                        leader_log(home.path()),
                    )
                });
            wait_for_replay_notifications(&old_client, base, Duration::from_secs(60)).await;
            old_client
                .prompt(&session, "after relaunch")
                .await
                .expect("prompt after relaunch failed");
        })
        .await;
}

/// Single-ownership after eviction: exactly one leader remains (old pid dead,
/// lock names the live replacement), the eviction is attributable in the
/// sandbox unified log, and no second writer touched `auth.json` during the
/// swap (API-key auth here, so any write would be a regression).
#[tokio::test]
#[ignore = "two-binary version-skew test; set GROK_BINARY_LEADER/GROK_BINARY_CLIENT and run with --ignored"]
async fn eviction_leaves_single_leader_and_single_auth_owner() {
    let Some((old_bin, new_bin)) = skew_binaries() else {
        return;
    };
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let workdir = git_workdir();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".grok")).unwrap();

            let old_client = LeaderStdioClient::spawn_with_binary(
                &old_bin,
                &server,
                workdir.path(),
                home.path(),
            )
            .await;
            old_client.initialize().await;
            let old_pid = wait_for_live_leader(home.path(), Duration::from_secs(10))
                .await
                .expect("no live old leader");

            let auth_path = home.path().join(".grok").join("auth.json");
            let auth_before = std::fs::metadata(&auth_path)
                .ok()
                .and_then(|m| m.modified().ok());

            let new_client = LeaderStdioClient::spawn_with_binary(
                &new_bin,
                &server,
                workdir.path(),
                home.path(),
            )
            .await;
            new_client.initialize().await;

            let new_pid = wait_for_new_leader(home.path(), old_pid, Duration::from_secs(60))
                .await
                .expect("no replacement leader after eviction");
            assert!(
                wait_for_pid_death(old_pid, Duration::from_secs(30)).await,
                "evicted leader must exit"
            );
            assert!(pid_alive(new_pid), "replacement leader must stay alive");
            assert_eq!(
                read_leader_pid(home.path()),
                Some(new_pid),
                "the lock file must name exactly the surviving leader"
            );

            // Attribution: the evicting client recorded the vacate/replace in
            // the sandbox unified log.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            let mut attributed = false;
            while tokio::time::Instant::now() < deadline {
                let log = sandbox_unified_log(home.path());
                if log.contains("leader.evict.vacate_requested")
                    || log.contains("leader.spawn.replacement")
                {
                    attributed = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            assert!(
                attributed,
                "eviction must be attributable in unified.jsonl\nlog:\n{}",
                sandbox_unified_log(home.path()),
            );

            // API-key sandbox: neither leader generation may write auth.json
            // during the swap (single auth ownership; a concurrent refresher
            // in the dying leader would show up as a write here).
            let auth_after = std::fs::metadata(&auth_path)
                .ok()
                .and_then(|m| m.modified().ok());
            assert_eq!(
                auth_before, auth_after,
                "auth.json must not be written during an eviction swap"
            );
        })
        .await;
}
