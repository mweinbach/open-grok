//! Repro + regression test: leader process dies → connected clients must
//! re-elect a leader and transparently restore their sessions.
//!
//! Scenario (mirrors the field report "clients see `unknown session id` after
//! the leader dies"):
//!
//! 1. Two stdio clients (`grok agent --leader stdio`) share one leader.
//! 2. Each creates its own session and completes a prompt round-trip.
//! 3. The leader is killed with SIGKILL (crash, no graceful shutdown).
//! 4. Each client's bridge must reconnect (re-electing / spawning a fresh
//!    leader), replay `initialize` + `session/load`, and then prompts against
//!    the ORIGINAL session IDs must succeed again.
//!
//! Tests are `#[ignore]`d by default — they require a pre-built binary:
//!
//! ```bash
//! cargo test -p xai-grok-shell --test test_leader_death_repro -- --ignored --nocapture
//! ```

#![cfg(unix)]

use std::time::Duration;

use agent_client_protocol::{self as acp, Agent as _};

use xai_grok_test_support::leader::{
    LeaderStdioClient, leader_log, wait_for_live_leader, wait_for_new_leader,
    wait_for_replay_notifications,
};
use xai_grok_test_support::*;

/// THE repro. Kill the shared leader with SIGKILL while two clients are
/// connected; both must recover their sessions on the re-elected leader.
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn test_leader_sigkill_clients_recover_sessions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let workdir = git_workdir();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".grok")).unwrap();

            // ── Phase 1: two clients, one leader, two sessions ────────────
            let client_a = LeaderStdioClient::spawn(&server, workdir.path(), home.path()).await;
            client_a.initialize().await;
            let session_a = client_a.create_session(workdir.path()).await;
            let r = client_a.prompt(&session_a, "hello from A").await;
            assert!(
                r.is_ok(),
                "pre-crash prompt A failed: {:?}\nstderr:\n{}\nleader log:\n{}",
                r.err(),
                client_a.stderr_text(),
                leader_log(home.path()),
            );

            let client_b = LeaderStdioClient::spawn(&server, workdir.path(), home.path()).await;
            client_b.initialize().await;
            let session_b = client_b.create_session(workdir.path()).await;
            let r = client_b.prompt(&session_b, "hello from B").await;
            assert!(
                r.is_ok(),
                "pre-crash prompt B failed: {:?}\nstderr:\n{}\nleader log:\n{}",
                r.err(),
                client_b.stderr_text(),
                leader_log(home.path()),
            );

            let leader_pid = wait_for_live_leader(home.path(), Duration::from_secs(5))
                .await
                .expect("no live leader PID in lock file");
            assert_ne!(leader_pid, client_a.child.id().unwrap_or(0));
            assert_ne!(leader_pid, client_b.child.id().unwrap_or(0));

            // ── Phase 2: SIGKILL the leader (simulated crash) ─────────────
            let base_a = client_a.notification_count();
            let base_b = client_b.notification_count();
            eprintln!("killing leader pid {leader_pid}");
            unsafe {
                libc::kill(leader_pid as i32, libc::SIGKILL);
            }

            // ── Phase 3: clients must re-elect a leader and reconnect ─────
            let new_pid = wait_for_new_leader(home.path(), leader_pid, Duration::from_secs(60))
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "no new leader was elected after SIGKILL\n\
                         client A stderr:\n{}\nclient B stderr:\n{}\nleader log:\n{}",
                        client_a.stderr_text(),
                        client_b.stderr_text(),
                        leader_log(home.path()),
                    )
                });
            eprintln!("new leader elected: pid {new_pid}");

            let a_reconnected =
                wait_for_replay_notifications(&client_a, base_a, Duration::from_secs(60)).await;
            let b_reconnected =
                wait_for_replay_notifications(&client_b, base_b, Duration::from_secs(60)).await;
            eprintln!("replay evidence: A={a_reconnected} B={b_reconnected}");

            // ── Phase 4: prompts on the ORIGINAL session IDs must work ────
            let res_a = client_a.prompt(&session_a, "after crash A").await;
            let res_b = client_b.prompt(&session_b, "after crash B").await;

            assert!(
                res_a.is_ok(),
                "client A prompt after leader crash failed: {:?}\n\
                 stderr:\n{}\nleader log:\n{}",
                res_a.err(),
                client_a.stderr_text(),
                leader_log(home.path()),
            );
            assert!(
                res_b.is_ok(),
                "client B prompt after leader crash failed: {:?}\n\
                 stderr:\n{}\nleader log:\n{}",
                res_b.err(),
                client_b.stderr_text(),
                leader_log(home.path()),
            );
        })
        .await;
}

/// Single-client variant: kill -9 the leader, the lone client must re-elect
/// and restore. Narrower failure surface than the two-client test.
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn test_leader_sigkill_single_client_recovers() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let workdir = git_workdir();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".grok")).unwrap();

            let client = LeaderStdioClient::spawn(&server, workdir.path(), home.path()).await;
            client.initialize().await;
            let session = client.create_session(workdir.path()).await;
            client
                .prompt(&session, "hello")
                .await
                .expect("pre-crash prompt failed");

            let leader_pid = wait_for_live_leader(home.path(), Duration::from_secs(5))
                .await
                .expect("no live leader PID in lock file");
            let base = client.notification_count();
            eprintln!("killing leader pid {leader_pid}");
            unsafe {
                libc::kill(leader_pid as i32, libc::SIGKILL);
            }

            let new_pid = wait_for_new_leader(home.path(), leader_pid, Duration::from_secs(60))
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "no new leader was elected after SIGKILL\nstderr:\n{}\nleader log:\n{}",
                        client.stderr_text(),
                        leader_log(home.path()),
                    )
                });
            eprintln!("new leader elected: pid {new_pid}");

            let reconnected =
                wait_for_replay_notifications(&client, base, Duration::from_secs(60)).await;
            eprintln!("replay evidence: {reconnected}");

            let res = client.prompt(&session, "after crash").await;
            assert!(
                res.is_ok(),
                "prompt after leader crash failed: {:?}\nstderr:\n{}\nleader log:\n{}",
                res.err(),
                client.stderr_text(),
                leader_log(home.path()),
            );
        })
        .await;
}

/// One client driving TWO sessions over a single stdio bridge (the IDE
/// shape). After a leader SIGKILL, BOTH sessions must be replayed onto the
/// re-elected leader — restoring only the most recent one left the other
/// failing with "unknown session id".
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn test_leader_sigkill_multi_session_client_recovers_all_sessions() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let workdir = git_workdir();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".grok")).unwrap();

            let client = LeaderStdioClient::spawn(&server, workdir.path(), home.path()).await;
            client.initialize().await;
            let session_one = client.create_session(workdir.path()).await;
            client
                .prompt(&session_one, "hello one")
                .await
                .expect("pre-crash prompt on session one failed");
            let session_two = client.create_session(workdir.path()).await;
            client
                .prompt(&session_two, "hello two")
                .await
                .expect("pre-crash prompt on session two failed");
            assert_ne!(session_one.0, session_two.0);

            let leader_pid = wait_for_live_leader(home.path(), Duration::from_secs(5))
                .await
                .expect("no live leader PID in lock file");
            let base = client.notification_count();
            eprintln!("killing leader pid {leader_pid}");
            unsafe {
                libc::kill(leader_pid as i32, libc::SIGKILL);
            }

            wait_for_new_leader(home.path(), leader_pid, Duration::from_secs(60))
                .await
                .unwrap_or_else(|| {
                    panic!(
                        "no new leader was elected after SIGKILL\nstderr:\n{}\nleader log:\n{}",
                        client.stderr_text(),
                        leader_log(home.path()),
                    )
                });
            wait_for_replay_notifications(&client, base, Duration::from_secs(60)).await;

            // BOTH sessions must work on the new leader.
            let res_one = client.prompt(&session_one, "after crash one").await;
            let res_two = client.prompt(&session_two, "after crash two").await;
            assert!(
                res_one.is_ok(),
                "session one prompt after crash failed: {:?}\nstderr:\n{}\nleader log:\n{}",
                res_one.err(),
                client.stderr_text(),
                leader_log(home.path()),
            );
            assert!(
                res_two.is_ok(),
                "session two prompt after crash failed: {:?}\nstderr:\n{}\nleader log:\n{}",
                res_two.err(),
                client.stderr_text(),
                leader_log(home.path()),
            );
        })
        .await;
}

/// Prompt sent DURING the outage (after the bridge noticed the dead leader
/// but before the new one is ready). The stdio bridge must hold and deliver
/// it once the session is restored — not silently drop it (which left the
/// client's request hanging forever).
#[tokio::test]
#[ignore] // requires pre-built binary; run with --ignored
async fn test_prompt_sent_during_outage_is_delivered_after_recovery() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = MockInferenceServer::start().await.unwrap();
            let workdir = git_workdir();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".grok")).unwrap();

            let client = LeaderStdioClient::spawn(&server, workdir.path(), home.path()).await;
            client.initialize().await;
            let session = client.create_session(workdir.path()).await;
            client
                .prompt(&session, "hello")
                .await
                .expect("pre-crash prompt failed");

            let leader_pid = wait_for_live_leader(home.path(), Duration::from_secs(5))
                .await
                .expect("no live leader PID in lock file");
            eprintln!("killing leader pid {leader_pid}");
            unsafe {
                libc::kill(leader_pid as i32, libc::SIGKILL);
            }
            // Give the bridge a moment to observe the dead socket (its send
            // channel closes), then prompt mid-outage: re-election + session
            // restore are still seconds away.
            tokio::time::sleep(Duration::from_millis(300)).await;

            let res = tokio::time::timeout(
                Duration::from_secs(90),
                client.conn.prompt(acp::PromptRequest::new(session.clone(), vec![acp::ContentBlock::Text(acp::TextContent::new("sent during outage".to_string()))])),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "prompt sent during outage never completed (dropped by bridge?)\n\
                     stderr:\n{}\nleader log:\n{}",
                    client.stderr_text(),
                    leader_log(home.path()),
                )
            });
            assert!(
                res.is_ok(),
                "prompt sent during outage failed: {:?}\nstderr:\n{}\nleader log:\n{}",
                res.err(),
                client.stderr_text(),
                leader_log(home.path()),
            );

            // A session-scoped request other than prompt (model switch) must
            // also survive — same "unknown session id" class.
            let set_model = tokio::time::timeout(
                Duration::from_secs(30),
                client.conn.set_session_model(acp::SetSessionModelRequest::new(session.clone(), acp::ModelId::new("test-model"))),
            )
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "set_session_model after recovery never completed\nstderr:\n{}\nleader log:\n{}",
                    client.stderr_text(),
                    leader_log(home.path()),
                )
            });
            assert!(
                set_model.is_ok(),
                "set_session_model after recovery failed: {:?}\nstderr:\n{}\nleader log:\n{}",
                set_model.err(),
                client.stderr_text(),
                leader_log(home.path()),
            );
        })
        .await;
}
