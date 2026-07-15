//! End-to-end coverage for the one-shot goal summarizer integration in
//! `drain_goal_updates` → `apply_classifier_outcome`. Each test drives
//! `update_goal(completed: true)` through a real `SessionActor` against a stub
//! subagent coordinator that answers BOTH the verifier skeptic spawn (a
//! configurable verdict) and the summarizer spawn (returns a summary / fails).
//! Pins:
//!   * the summarizer fires ONCE on a real `Achieved` verdict and the goal
//!     completes;
//!   * it does NOT fire on NotAchieved, Blocked, a cap pause, or the infra
//!     `FailOpenAchieved` path;
//!   * it fires exactly once per achievement;
//!   * a summarizer failure is fail-OPEN — the goal still completes;
//!   * the remote kill-switch (`goal_summary_enabled = false`) suppresses it.
//!
//! Tests mutate `GROK_GOAL_CLASSIFIER` so they carry `serial`.

use super::support::*;
use super::*;
use crate::session::goal_summarizer::GOAL_SUMMARIZER_SUBAGENT_DESCRIPTION;
use serial_test::serial;
use std::collections::VecDeque;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering as SeqOrd};
use xai_grok_tools::implementations::grok_build::task::types::{
    SubagentCancelOutcome, SubagentEvent, SubagentResult,
};
use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput;

const ENV_FLAG: &str = "GROK_GOAL_CLASSIFIER";

/// How the stub answers a summarizer spawn.
#[derive(Clone, Copy)]
enum SummarizerBehaviour {
    /// Return a non-empty summary as the subagent output.
    ReturnSummary,
    /// Reply with a runtime failure (exercises the fail-open path).
    RuntimeFailure,
}

/// What the single skeptic votes (skeptic_count = 1, so one vote = the
/// aggregate verdict for the round).
#[derive(Clone, Copy)]
enum SkepticVerdict {
    /// Not Refuted ⇒ aggregate Achieved.
    Achieved,
    /// Refuted with distinct evidence ⇒ NotAchieved.
    Refuted,
    /// Refuted + non-model-fixable `contradiction` ⇒ Blocked outcome.
    Blocked,
}

#[derive(Clone)]
struct Counters {
    skeptic_spawns: StdArc<AtomicUsize>,
    summarizer_spawns: StdArc<AtomicUsize>,
}

/// Coordinator stub distinguishing summarizer spawns (by description) from
/// skeptic spawns and answering each. Skeptic spawns pop the next
/// [`SkepticVerdict`] from `verdicts` (FIFO, one per round).
fn spawn_coordinator(
    summarizer: SummarizerBehaviour,
    verdicts: VecDeque<SkepticVerdict>,
) -> (tokio::sync::mpsc::UnboundedSender<SubagentEvent>, Counters) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
    let counters = Counters {
        skeptic_spawns: StdArc::new(AtomicUsize::new(0)),
        summarizer_spawns: StdArc::new(AtomicUsize::new(0)),
    };
    let queue = StdArc::new(parking_lot::Mutex::new(verdicts));
    let task_counters = counters.clone();
    tokio::task::spawn_local(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                SubagentEvent::Spawn(req) => {
                    let counters = task_counters.clone();
                    let queue = StdArc::clone(&queue);
                    tokio::task::spawn_local(async move {
                        if req.description == GOAL_SUMMARIZER_SUBAGENT_DESCRIPTION {
                            counters.summarizer_spawns.fetch_add(1, SeqOrd::SeqCst);
                            answer_summarizer(summarizer, req).await;
                            return;
                        }
                        let n = counters.skeptic_spawns.fetch_add(1, SeqOrd::SeqCst);
                        let verdict = queue.lock().pop_front().unwrap_or(SkepticVerdict::Refuted);
                        answer_skeptic(verdict, n, req).await;
                    });
                }
                SubagentEvent::Cancel(c) => {
                    let _ = c.respond_to.send(SubagentCancelOutcome::Cancelled);
                }
                _ => {}
            }
        }
    });
    (tx, counters)
}

async fn answer_summarizer(
    behaviour: SummarizerBehaviour,
    req: Box<xai_grok_tools::implementations::grok_build::task::types::SubagentRequest>,
) {
    match behaviour {
        SummarizerBehaviour::ReturnSummary => {
            let _ = req.result_tx.send(SubagentResult {
                success: true,
                output: StdArc::from(
                    "Shipped the feature.\n\n- Added the widget\n- Wired the route\n\nVerified by the panel.",
                ),
                subagent_id: req.id.clone(),
                child_session_id: req.id.clone(),
                ..Default::default()
            });
        }
        SummarizerBehaviour::RuntimeFailure => {
            let _ = req.result_tx.send(SubagentResult {
                success: false,
                error: Some("summarizer crashed".into()),
                subagent_id: req.id.clone(),
                child_session_id: req.id.clone(),
                ..Default::default()
            });
        }
    }
}

async fn answer_skeptic(
    verdict: SkepticVerdict,
    spawn_idx: usize,
    req: Box<xai_grok_tools::implementations::grok_build::task::types::SubagentRequest>,
) {
    if let Some(p) =
        crate::session::goal_classifier::parse_skeptic_details_path_from_prompt(&req.prompt)
    {
        let _ = tokio::fs::write(&p, b"# mock skeptic details\n").await;
    }
    let (token, json) = match verdict {
        SkepticVerdict::Achieved => (
            "Not Refuted",
            "{\"refuted\":false,\"evidence\":\"diff ok\",\"confidence\":\"high\",\"details_md\":\"# ok\"}".to_string(),
        ),
        SkepticVerdict::Refuted => (
            "Refuted",
            format!(
                "{{\"refuted\":true,\"evidence\":\"src/round{spawn_idx}.rs:1 missing\",\"confidence\":\"high\",\"details_md\":\"# refuted\"}}"
            ),
        ),
        SkepticVerdict::Blocked => (
            "Refuted",
            "{\"refuted\":true,\"evidence\":\"objective conflict\",\"confidence\":\"high\",\"blocking\":\"contradiction\",\"details_md\":\"# blocked\"}".to_string(),
        ),
    };
    if let Some(p) = crate::session::goal_classifier::parse_verdict_path_from_prompt(&req.prompt) {
        let _ = tokio::fs::write(&p, json).await;
    }
    let _ = req.result_tx.send(SubagentResult {
        success: true,
        output: StdArc::from(token),
        subagent_id: req.id.clone(),
        child_session_id: req.id.clone(),
        ..Default::default()
    });
}

/// Shared goal-mode wiring for the actor: classifier on, summarizer flag,
/// skeptic count, isolated (non-git) cwd, coordinator, and an active goal.
fn configure_goal_actor(
    actor: &mut SessionActor,
    tmp: &tempfile::TempDir,
    summary_enabled: bool,
    max_runs: u32,
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
) {
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(actor);
    actor.goal_classifier_enabled = true;
    actor.goal_summary_enabled = summary_enabled;
    actor.goal_classifier_max_runs = max_runs;
    actor.goal_verifier_skeptic_count = 1;
    actor.tool_context.subagent_event_tx = coordinator_tx;
    actor.tool_context.cwd =
        xai_grok_paths::AbsPathBuf::new(tmp.path().to_path_buf()).expect("abs cwd");
    actor.goal_tracker.lock().create_goal(
        "test-goal".to_string(),
        "test objective".to_string(),
        None,
        0,
        "2026-01-01T00:00:00Z".to_string(),
        None,
    );
}

/// Build an actor with an active goal (no notification capture). Returns the
/// tempdir so the caller can scan `events.jsonl`.
async fn make_actor(
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
    summary_enabled: bool,
    max_runs: u32,
) -> (StdArc<SessionActor>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    configure_goal_actor(&mut actor, &tmp, summary_enabled, max_runs, coordinator_tx);
    (StdArc::new(actor), tmp)
}

/// Like [`make_actor`], but retains `event_rx` and drains it on the `LocalSet`:
/// `AgentMessageChunk` text is collected into the returned sink and
/// `FlushReplay` is acked (so `send_slash_command_output`'s flush completes).
/// Lets a test assert the summary text was actually surfaced to the user.
async fn make_capturing_actor(
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
    summary_enabled: bool,
    max_runs: u32,
) -> (
    StdArc<SessionActor>,
    tempfile::TempDir,
    StdArc<parking_lot::Mutex<Vec<String>>>,
) {
    use crate::session::replay_events::{SessionEvent, SessionNotification};
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let (mut actor, mut event_rx) =
        create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
    configure_goal_actor(&mut actor, &tmp, summary_enabled, max_runs, coordinator_tx);

    let sink: StdArc<parking_lot::Mutex<Vec<String>>> =
        StdArc::new(parking_lot::Mutex::new(vec![]));
    let sink_task = StdArc::clone(&sink);
    tokio::task::spawn_local(async move {
        while let Some(ev) = event_rx.recv().await {
            match ev {
                SessionEvent::Notification(SessionNotification::Acp(n)) => {
                    if let acp::SessionUpdate::AgentMessageChunk(chunk) = &n.update
                        && let acp::ContentBlock::Text(t) = &chunk.content
                    {
                        sink_task.lock().push(t.text.clone());
                    }
                }
                SessionEvent::Notification(_) => {}
                SessionEvent::FlushReplay { respond_to } => {
                    if let Some(tx) = respond_to {
                        let _ = tx.send(());
                    }
                }
            }
        }
    });
    (StdArc::new(actor), tmp, sink)
}

fn make_completed() -> UpdateGoalInput {
    UpdateGoalInput {
        completed: Some(true),
        message: None,
        blocked_reason: None,
    }
}

fn seed_channel(actor: &SessionActor, cmds: Vec<UpdateGoalInput>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *actor.goal_update_rx.borrow_mut() = Some(rx);
    for cmd in cmds {
        tx.send(xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(cmd))
            .unwrap();
    }
    drop(tx);
}

fn count_event(tmp: &tempfile::TempDir, ty: &str) -> usize {
    let log = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap_or_default();
    log.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some(ty))
        .count()
}

async fn drive_round(actor: &SessionActor) {
    seed_channel(actor, vec![make_completed()]);
    actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
}

// ── Fires once on a real Achieved verdict, goal completes ────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_fires_on_real_achieved_and_completes() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Achieved]),
            );
            let (actor, tmp) = make_actor(Some(tx), true, 3).await;

            drive_round(&actor).await;

            assert_eq!(
                counters.summarizer_spawns.load(SeqOrd::SeqCst),
                1,
                "summarizer fires once on a real Achieved verdict",
            );
            assert_eq!(count_event(&tmp, "goal_summarizer_fired"), 1);
            // `completed` only marks a successful run; that the text actually
            // reaches the user is pinned by `summarizer_surfaces_summary_text_to_user`.
            assert_eq!(count_event(&tmp, "goal_summarizer_completed"), 1);
            assert_eq!(count_event(&tmp, "goal_summarizer_fail_open"), 0);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── The summary TEXT is actually surfaced to the user ───────────────

/// Pins that the summary reaches the user as an `AgentMessageChunk` — not just
/// that `GoalSummarizerCompleted` fired. Deleting the
/// `send_slash_command_output(&summary)` call must fail THIS test.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_surfaces_summary_text_to_user() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Achieved]),
            );
            let (actor, _tmp, surfaced) = make_capturing_actor(Some(tx), true, 3).await;

            drive_round(&actor).await;
            // Let the event-drain task observe any trailing chunk.
            tokio::task::yield_now().await;

            let chunks = surfaced.lock();
            assert!(
                chunks.iter().any(|t| t.contains("Shipped the feature.")),
                "the summary text must reach the user as an AgentMessageChunk; got {chunks:?}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Surfacing starts a NEW message block (fresh stream boundary) ─────

/// The closing summary must render as its own block, not glued to the model's
/// last turn message. Surfacing bumps `stream_start_ms`, the boundary the
/// client uses to start a new agent message; without the bump the summary
/// chunk coalesces into the preceding model message.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_surfacing_bumps_stream_start_for_new_block() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Achieved]),
            );
            let (actor, _tmp, _surfaced) = make_capturing_actor(Some(tx), true, 3).await;

            // Stand in for the model's turn-message stream.
            const MODEL_STREAM_START: i64 = 1;
            actor
                .chat_state_handle
                .record_stream_start(MODEL_STREAM_START);

            drive_round(&actor).await;

            let start = actor
                .chat_state_handle
                .get_notification_meta()
                .await
                .and_then(|m| m.stream_start_ms);
            assert!(start.is_some(), "a stream start must be recorded");
            assert_ne!(
                start,
                Some(MODEL_STREAM_START),
                "surfacing must bump stream_start_ms so the summary renders as a \
                 new block, not appended to the model's last message",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Exactly once: a second completion against the Complete goal no-ops ─

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_fires_exactly_once_per_achievement() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Achieved, SkepticVerdict::Achieved]),
            );
            let (actor, tmp) = make_actor(Some(tx), true, 3).await;

            drive_round(&actor).await;
            // Second completion: the goal is already Complete (non-Active
            // guard short-circuits before the Achieved arm).
            drive_round(&actor).await;

            assert_eq!(
                counters.summarizer_spawns.load(SeqOrd::SeqCst),
                1,
                "summarizer must fire exactly once per achievement",
            );
            assert_eq!(count_event(&tmp, "goal_summarizer_fired"), 1);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Does NOT fire on NotAchieved (goal stays Active) ─────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_does_not_fire_on_not_achieved() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Refuted]),
            );
            let (actor, tmp) = make_actor(Some(tx), true, 3).await;

            drive_round(&actor).await;

            assert_eq!(counters.summarizer_spawns.load(SeqOrd::SeqCst), 0);
            assert_eq!(count_event(&tmp, "goal_summarizer_fired"), 0);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Does NOT fire on Blocked (goal pauses) ──────────────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_does_not_fire_on_blocked() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Blocked]),
            );
            let (actor, tmp) = make_actor(Some(tx), true, 3).await;

            drive_round(&actor).await;

            assert_eq!(counters.summarizer_spawns.load(SeqOrd::SeqCst), 0);
            assert_eq!(count_event(&tmp, "goal_summarizer_fired"), 0);
            assert!(actor.goal_tracker.lock().status().unwrap().is_paused());
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Does NOT fire when the cap pauses the round ─────────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_does_not_fire_on_cap_pause() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // max_runs = 1: the first NotAchieved hits the cap and BackOff-pauses.
            let (tx, counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Refuted]),
            );
            let (actor, tmp) = make_actor(Some(tx), true, 1).await;

            drive_round(&actor).await;

            assert_eq!(counters.summarizer_spawns.load(SeqOrd::SeqCst), 0);
            assert_eq!(count_event(&tmp, "goal_summarizer_fired"), 0);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BackOffPaused),
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Does NOT fire on the infra FailOpenAchieved path (non-confounded) ─

/// Drives `FailOpenAchieved` WITH a coordinator present so the assertion is
/// non-confounded: if the summarizer were (wrongly) invoked from the
/// FailOpenAchieved arm it COULD spawn, but it isn't, so `summarizer_spawns`
/// stays 0. A regular file planted at the goal's scratch-root path makes the
/// stage's `ensure_goal_scratch_root` fail → `FailOpenAchieved{FileWriteFailed}`
/// (a missing/garbage skeptic verdict would be fail-CLOSED → NotAchieved, not
/// fail-open, so that route can't be used here).
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_does_not_fire_on_fail_open_achieved() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Achieved]),
            );
            let (actor, tmp) = make_actor(Some(tx), true, 3).await;

            // Plant a file where the scratch root (a dir) must be created, so
            // the stage fails open before any skeptic spawns.
            let verifier_id = actor
                .goal_tracker
                .lock()
                .snapshot()
                .unwrap()
                .verifier_id
                .clone();
            let scratch = crate::session::goal_tracker::goal_scratch_root(&verifier_id);
            // The root is created (as a dir) at goal setup; swap it for a file
            // so `ensure_goal_scratch_root` rejects it (not a real directory).
            let _ = std::fs::remove_dir_all(&scratch);
            std::fs::write(&scratch, b"not a dir").unwrap();

            drive_round(&actor).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
                "infra failure fails open to Achieved",
            );
            assert!(
                count_event(&tmp, "goal_classifier_fail_open") >= 1,
                "completion must be via the FailOpenAchieved (infra) path",
            );
            assert_eq!(
                counters.summarizer_spawns.load(SeqOrd::SeqCst),
                0,
                "the FailOpenAchieved arm must NOT run the summarizer (coordinator present)",
            );
            assert_eq!(count_event(&tmp, "goal_summarizer_fired"), 0);

            let _ = std::fs::remove_file(&scratch);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Fail-open: a summarizer failure still completes the goal ─────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_failure_is_fail_open_goal_still_completes() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, counters) = spawn_coordinator(
                SummarizerBehaviour::RuntimeFailure,
                VecDeque::from([SkepticVerdict::Achieved]),
            );
            let (actor, tmp) = make_actor(Some(tx), true, 3).await;

            drive_round(&actor).await;

            assert_eq!(counters.summarizer_spawns.load(SeqOrd::SeqCst), 1);
            // The goal completed BEFORE the summarizer ran; a failure must
            // never un-complete or pause it.
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
                "summarizer failure must NOT block completion",
            );
            assert_eq!(count_event(&tmp, "goal_summarizer_fired"), 1);
            assert_eq!(count_event(&tmp, "goal_summarizer_fail_open"), 1);
            assert_eq!(count_event(&tmp, "goal_summarizer_completed"), 0);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Kill-switch: disabled flag ⇒ no summarizer spawn ────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn summarizer_disabled_flag_suppresses_spawn() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, counters) = spawn_coordinator(
                SummarizerBehaviour::ReturnSummary,
                VecDeque::from([SkepticVerdict::Achieved]),
            );
            // goal_summary_enabled = false ⇒ kill-switch.
            let (actor, tmp) = make_actor(Some(tx), false, 3).await;

            drive_round(&actor).await;

            assert_eq!(
                counters.summarizer_spawns.load(SeqOrd::SeqCst),
                0,
                "disabled summarizer must not spawn",
            );
            assert_eq!(count_event(&tmp, "goal_summarizer_fired"), 0);
            // The achievement itself is unaffected.
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
