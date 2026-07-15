//! End-to-end coverage for the stall-triggered strategist integration
//! in `drain_goal_updates` → `apply_classifier_outcome`. Each test drives
//! `update_goal(completed: true)` through a real `SessionActor` against a
//! stub subagent coordinator that answers BOTH the verifier skeptic spawns
//! (per a configurable per-round verdict queue) and the strategist spawn
//! (writes the strategy note / fails). Pins:
//!   * the trigger fires at N and 2N consecutive failures, NOT at N+1;
//!   * it is skip-robust (fires even when the streak jumps past N);
//!   * cap / stall pauses take precedence (no strategist that round);
//!   * a strategist failure is fail-OPEN (the goal keeps running);
//!   * Achieved / Blocked verdicts reset the streak AND clear the note;
//!   * the persisted recommendation reaches the rendered continuation
//!     directive via the real `run_goal_round_end` seam.
//!
//! Tests mutate `GROK_GOAL_CLASSIFIER` so they carry `serial`.

use super::support::*;
use super::*;
use crate::session::goal_strategist::GOAL_STRATEGIST_SUBAGENT_DESCRIPTION;
use serial_test::serial;
use std::collections::VecDeque;
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicUsize, Ordering as SeqOrd};
use xai_grok_tools::implementations::grok_build::task::types::{
    SubagentCancelOutcome, SubagentEvent, SubagentResult,
};
use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput;

const ENV_FLAG: &str = "GROK_GOAL_CLASSIFIER";

/// How the stub answers a strategist spawn.
#[derive(Clone, Copy)]
enum StrategistBehaviour {
    /// Parse the strategy-file path, write a note, return `Done`.
    WriteNoteThenDone,
    /// Reply with a runtime failure (exercises the fail-open path).
    RuntimeFailure,
    /// Never reply — keeps the strategist await pending so the test can
    /// drop the drain future mid-run (turn-cancel simulation).
    NeverReply,
}

/// What a single skeptic spawn votes (skeptic_count = 1 in these tests,
/// so one vote per verification round = the aggregate verdict).
#[derive(Clone, Copy)]
enum SkepticVerdict {
    /// Refuted with DISTINCT evidence per spawn (avoids the stall exit).
    Refuted,
    /// Refuted with IDENTICAL evidence (drives the stall early-exit).
    RefutedSame,
    /// Not Refuted ⇒ aggregate Achieved.
    Achieved,
    /// Refuted + a non-model-fixable `contradiction` ⇒ Blocked outcome.
    Blocked,
}

/// Counters the test reads after driving the drain.
#[derive(Clone)]
struct Counters {
    skeptic_spawns: StdArc<AtomicUsize>,
    strategist_spawns: StdArc<AtomicUsize>,
}

/// Coordinator stub that distinguishes strategist spawns (by description)
/// from skeptic spawns and answers each. Skeptic spawns pop the next
/// [`SkepticVerdict`] from `verdicts` (FIFO, one per round).
fn spawn_coordinator(
    strategist: StrategistBehaviour,
    verdicts: VecDeque<SkepticVerdict>,
) -> (tokio::sync::mpsc::UnboundedSender<SubagentEvent>, Counters) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubagentEvent>();
    let counters = Counters {
        skeptic_spawns: StdArc::new(AtomicUsize::new(0)),
        strategist_spawns: StdArc::new(AtomicUsize::new(0)),
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
                        if req.description == GOAL_STRATEGIST_SUBAGENT_DESCRIPTION {
                            counters.strategist_spawns.fetch_add(1, SeqOrd::SeqCst);
                            answer_strategist(strategist, req).await;
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

async fn answer_strategist(
    behaviour: StrategistBehaviour,
    req: Box<xai_grok_tools::implementations::grok_build::task::types::SubagentRequest>,
) {
    match behaviour {
        StrategistBehaviour::WriteNoteThenDone => {
            if let Some(p) = parse_strategy_path(&req.prompt) {
                let _ = tokio::fs::write(&p, b"## Diagnosis\n\nMonolith. Split into pure units.\n")
                    .await;
            }
            let _ = req.result_tx.send(SubagentResult {
                success: true,
                output: StdArc::from("Done"),
                subagent_id: req.id.clone(),
                child_session_id: req.id.clone(),
                ..Default::default()
            });
        }
        StrategistBehaviour::RuntimeFailure => {
            let _ = req.result_tx.send(SubagentResult {
                success: false,
                error: Some("strategist crashed".into()),
                subagent_id: req.id.clone(),
                child_session_id: req.id.clone(),
                ..Default::default()
            });
        }
        StrategistBehaviour::NeverReply => {
            // Keep `result_tx` alive forever so the spawner await pends.
            futures::future::pending::<()>().await;
        }
    }
}

async fn answer_skeptic(
    verdict: SkepticVerdict,
    spawn_idx: usize,
    req: Box<xai_grok_tools::implementations::grok_build::task::types::SubagentRequest>,
) {
    if let Some(p) = parse_details_path(&req.prompt) {
        let _ = tokio::fs::write(&p, b"# mock skeptic details\n").await;
    }
    let (token, json) = match verdict {
        SkepticVerdict::Refuted => (
            "Refuted",
            format!(
                "{{\"refuted\":true,\"evidence\":\"src/round{spawn_idx}.rs:1 missing\",\"confidence\":\"high\",\"details_md\":\"# refuted\"}}"
            ),
        ),
        SkepticVerdict::RefutedSame => (
            "Refuted",
            "{\"refuted\":true,\"evidence\":\"src/same.rs:1 missing\",\"confidence\":\"high\",\"details_md\":\"# refuted\"}".to_string(),
        ),
        SkepticVerdict::Achieved => (
            "Not Refuted",
            "{\"refuted\":false,\"evidence\":\"diff ok\",\"confidence\":\"high\",\"details_md\":\"# ok\"}".to_string(),
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

/// Pull the per-skeptic details path out of the rendered verifier prompt.
fn parse_details_path(prompt: &str) -> Option<String> {
    crate::session::goal_classifier::parse_skeptic_details_path_from_prompt(prompt)
}

/// Pull the absolute `.../strategy.md` path out of the strategist prompt
/// (walk left from `/strategy.md` to the path start).
fn parse_strategy_path(prompt: &str) -> Option<String> {
    let end_idx = prompt.find("/strategy.md")?;
    let end = end_idx + "/strategy.md".len();
    let start = prompt[..end_idx]
        .rfind(|c: char| !c.is_ascii_graphic() || c == '`')
        .map(|i| i + 1)
        .unwrap_or(0);
    Some(prompt[start..end].to_string())
}

/// Build an actor with an active goal, the classifier enabled, the
/// strategist N pinned, cwd pointed at an isolated (non-git) tempdir so
/// the strategist's change-capture stays empty + fast, and the
/// coordinator plumbed in. Returns the tempdir so the caller can scan
/// `events.jsonl`.
async fn make_actor(
    coordinator_tx: Option<tokio::sync::mpsc::UnboundedSender<SubagentEvent>>,
    strategist_every: u32,
    max_runs: u32,
) -> (StdArc<SessionActor>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.events = crate::session::events::EventTracker::new(tmp.path());
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_classifier_enabled = true;
    actor.goal_classifier_max_runs = max_runs;
    actor.goal_strategist_every = strategist_every;
    actor.goal_verifier_skeptic_count = 1;
    actor.tool_context.subagent_event_tx = coordinator_tx;
    // Isolated cwd for a hermetic, fast harness run.
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
    (StdArc::new(actor), tmp)
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
    read_events(tmp, ty).len()
}

/// All events of `ty` from `events.jsonl`, in file (emission) order.
fn read_events(tmp: &tempfile::TempDir, ty: &str) -> Vec<serde_json::Value> {
    let log = std::fs::read_to_string(tmp.path().join("events.jsonl")).unwrap_or_default();
    log.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("type").and_then(|t| t.as_str()) == Some(ty))
        .collect()
}

/// Drive `rounds` verification rounds, one `update_goal(completed)` per drain.
async fn drive_rounds(actor: &SessionActor, rounds: usize) {
    for _ in 0..rounds {
        seed_channel(actor, vec![make_completed()]);
        actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
    }
}

/// `n` distinct-evidence refutations.
fn refuted(n: usize) -> VecDeque<SkepticVerdict> {
    std::iter::repeat_n(SkepticVerdict::Refuted, n).collect()
}

// ── Trigger fires at N and 2N, never N+1 ────────────────────────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn strategist_fires_at_n_and_2n_not_at_n_plus_one() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 2, cap = 10 (distinct gaps avoid the stall early-exit).
            let (tx, counters) =
                spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, refuted(4));
            let (actor, tmp) = make_actor(Some(tx), 2, 10).await;

            // Round 1: consecutive=1 → no fire.
            drive_rounds(&actor, 1).await;
            assert_eq!(
                counters.strategist_spawns.load(SeqOrd::SeqCst),
                0,
                "not at < N"
            );

            // Round 2: consecutive=2 == N → fire once.
            drive_rounds(&actor, 1).await;
            assert_eq!(
                counters.strategist_spawns.load(SeqOrd::SeqCst),
                1,
                "fire at N=2"
            );
            {
                let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
                assert!(
                    snap.last_strategy_recommendation
                        .as_deref()
                        .is_some_and(|r| r.contains("Split into pure units")),
                    "recommendation persisted: {:?}",
                    snap.last_strategy_recommendation,
                );
                assert!(snap.last_strategy_path.is_some());
                assert_eq!(
                    snap.strategist_cap_bonus,
                    crate::session::goal_tracker::GOAL_STRATEGIST_CAP_BONUS,
                    "a successful fire keeps its cap bonus",
                );
            }

            // Round 3: consecutive=3 → N+1, must NOT fire again.
            drive_rounds(&actor, 1).await;
            assert_eq!(
                counters.strategist_spawns.load(SeqOrd::SeqCst),
                1,
                "not at N+1"
            );

            // Round 4: consecutive=4 == 2N → fire again.
            drive_rounds(&actor, 1).await;
            assert_eq!(
                counters.strategist_spawns.load(SeqOrd::SeqCst),
                2,
                "fire at 2N=4"
            );

            assert_eq!(count_event(&tmp, "goal_strategist_fired"), 2);
            assert_eq!(count_event(&tmp, "goal_strategist_completed"), 2);
            assert_eq!(count_event(&tmp, "goal_strategist_failed"), 0);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Telemetry: GoalStrategistFired reports the resolved cadence ─────

/// The `acp_session` glue wires `every: self.goal_strategist_every` into
/// `GoalStrategistFired`. A streak to 2N=4 pins `every` (2) as the resolved
/// cadence, distinct from `consecutive_failures` (4).
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn strategist_fired_event_reports_resolved_cadence() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 2, cap = 10: fires at consecutive=2 and 2N=4.
            let (tx, counters) =
                spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, refuted(4));
            let (actor, tmp) = make_actor(Some(tx), 2, 10).await;

            drive_rounds(&actor, 4).await;
            assert_eq!(counters.strategist_spawns.load(SeqOrd::SeqCst), 2);

            let fired = read_events(&tmp, "goal_strategist_fired");
            assert_eq!(fired.len(), 2, "two strategist fires");
            for ev in &fired {
                assert_eq!(
                    ev["every"], 2,
                    "every must report self.goal_strategist_every"
                );
            }
            // every (2) stays distinct from the failure streak (4).
            assert_eq!(fired[1]["consecutive_failures"], 4);
            assert_eq!(fired[1]["every"], 2);
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Skip-robustness: a streak that jumps PAST N still fires ──────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn strategist_fires_after_streak_skips_past_n() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 2. Simulate a synthetic concurrent-in-flight bump that already
            // advanced the streak to 2 WITHOUT firing (the synthetic path
            // increments but never fires): pre-seed consecutive=2, last_fired=0.
            let (tx, counters) =
                spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, refuted(1));
            let (actor, _tmp) = make_actor(Some(tx), 2, 10).await;
            {
                let mut tracker = actor.goal_tracker.lock();
                let o = tracker.snapshot_mut().unwrap();
                o.consecutive_not_achieved = 2;
                o.last_strategist_fired_at = 0;
            }

            // One real round → streak 2→3 (skips the == 2 landing). A strict
            // `% N == 0` would miss it; the `>= last_fired + N` form fires.
            drive_rounds(&actor, 1).await;

            assert_eq!(
                counters.strategist_spawns.load(SeqOrd::SeqCst),
                1,
                "must fire when the streak skips past the == N landing",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Cap takes precedence: no strategist the round the cap pauses ─────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn strategist_does_not_fire_when_cap_pauses_same_round() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 2 AND cap = 2: round 2 hits the cap (returns before the
            // strategist trigger). Distinct gaps so the stall doesn't fire.
            let (tx, counters) =
                spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, refuted(2));
            let (actor, tmp) = make_actor(Some(tx), 2, 2).await;

            drive_rounds(&actor, 2).await;

            assert_eq!(
                counters.strategist_spawns.load(SeqOrd::SeqCst),
                0,
                "cap precedence"
            );
            assert_eq!(count_event(&tmp, "goal_strategist_fired"), 0);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BackOffPaused),
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Stall takes precedence: no strategist the round the stall pauses ─

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn strategist_does_not_fire_when_stall_pauses_same_round() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 2, cap = 10, but IDENTICAL gaps ⇒ the stall early-exit
            // (threshold 2) pauses at round 2 before the strategist trigger.
            let stall: VecDeque<_> =
                VecDeque::from([SkepticVerdict::RefutedSame, SkepticVerdict::RefutedSame]);
            let (tx, counters) = spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, stall);
            let (actor, tmp) = make_actor(Some(tx), 2, 10).await;

            drive_rounds(&actor, 2).await;

            assert_eq!(
                counters.strategist_spawns.load(SeqOrd::SeqCst),
                0,
                "stall precedence"
            );
            assert_eq!(count_event(&tmp, "goal_strategist_fired"), 0);
            assert!(actor.goal_tracker.lock().status().unwrap().is_paused());
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Strategist failure is fail-open: the goal keeps running ──────────

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn strategist_failure_is_fail_open_goal_keeps_going() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 1 ⇒ fires every round; cap = 10. Strategist always fails.
            let (tx, counters) = spawn_coordinator(StrategistBehaviour::RuntimeFailure, refuted(1));
            let (actor, tmp) = make_actor(Some(tx), 1, 10).await;

            drive_rounds(&actor, 1).await;

            assert_eq!(counters.strategist_spawns.load(SeqOrd::SeqCst), 1);
            // Fail-open: the goal stays Active despite the strategist failure.
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "strategist failure must NOT pause the goal",
            );
            assert_eq!(count_event(&tmp, "goal_strategist_fired"), 1);
            assert_eq!(count_event(&tmp, "goal_strategist_failed"), 1);
            assert_eq!(count_event(&tmp, "goal_strategist_completed"), 0);
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(snap.last_strategy_recommendation.is_none());
            // No restructure was delivered: the up-front cap bonus (and with
            // it the relaxed stall threshold) must be revoked, while the fire
            // stays claimed so the trigger waits a full window to retry.
            assert_eq!(
                snap.strategist_cap_bonus, 0,
                "failed fire must revoke the cap bonus",
            );
            assert_eq!(snap.last_strategist_fired_at, 1, "fire claim retained");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Turn cancel mid-strategist must also revoke the bonus ────────────

/// A turn cancel dropping the drain future mid-strategist delivers no
/// restructure: the cap bonus must be revoked, the fire claim retained.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn strategist_cancel_mid_run_revokes_cap_bonus() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 1 ⇒ fires on the first refute; the strategist never replies.
            let (tx, counters) = spawn_coordinator(StrategistBehaviour::NeverReply, refuted(1));
            let (actor, _tmp) = make_actor(Some(tx), 1, 10).await;

            seed_channel(&actor, vec![make_completed()]);
            let drain_actor = StdArc::clone(&actor);
            let drain = tokio::task::spawn_local(async move {
                drain_actor
                    .drain_goal_updates(0, DrainPurpose::TurnEnd)
                    .await;
            });
            for _ in 0..10_000 {
                if counters.strategist_spawns.load(SeqOrd::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(counters.strategist_spawns.load(SeqOrd::SeqCst), 1);
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .strategist_cap_bonus,
                crate::session::goal_tracker::GOAL_STRATEGIST_CAP_BONUS,
                "claim grants the bonus before the strategist resolves",
            );

            drain.abort();
            let _ = drain.await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.strategist_cap_bonus, 0,
                "cancel mid-strategist must revoke the unearned bonus",
            );
            assert_eq!(snap.last_strategist_fired_at, 1, "fire claim retained");
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── No-coordinator early return also revokes the bonus ──────────────

/// The no-coordinator early return delivers no restructure and must
/// revoke the bonus just like the FailOpen path.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn strategist_no_coordinator_revokes_cap_bonus() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, _tmp) = make_actor(None, 1, 10).await;
            // Real claim path: the trigger fires and grants the bonus.
            let _ = actor.goal_tracker.lock().record_not_achieved_streak();
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .claim_strategist_fire(|consecutive, last| {
                        crate::session::goal_strategist::strategist_should_fire(
                            consecutive,
                            last,
                            1,
                        )
                    })
                    .is_some(),
            );

            actor.maybe_run_goal_strategist(1, 1).await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.strategist_cap_bonus, 0,
                "no-coordinator early return must revoke the bonus",
            );
            assert!(snap.last_strategy_recommendation.is_none());
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Achieved verdict resets the streak AND clears the recommendation ─

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn achieved_verdict_resets_streak_and_clears_recommendation() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 1: round 1 refutes (fires strategist, persists note), round 2
            // is Achieved → goal Complete, streak + note cleared.
            let q = VecDeque::from([SkepticVerdict::Refuted, SkepticVerdict::Achieved]);
            let (tx, _counters) = spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, q);
            let (actor, _tmp) = make_actor(Some(tx), 1, 10).await;

            drive_rounds(&actor, 1).await;
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .last_strategy_recommendation
                    .is_some(),
                "round 1 must persist a recommendation",
            );

            drive_rounds(&actor, 1).await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Complete
            );
            assert_eq!(snap.consecutive_not_achieved, 0, "streak reset on Achieved");
            assert_eq!(snap.last_strategist_fired_at, 0);
            assert!(
                snap.last_strategy_recommendation.is_none(),
                "Achieved must clear the stale recommendation",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Blocked verdict resets the streak AND clears the recommendation ──

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn blocked_verdict_resets_streak_and_clears_recommendation() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // N = 1: round 1 refutes (persists note), round 2 is Blocked
            // (all-refuters non-fixable) → goal paused, streak + note cleared.
            let q = VecDeque::from([SkepticVerdict::Refuted, SkepticVerdict::Blocked]);
            let (tx, _counters) = spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, q);
            let (actor, _tmp) = make_actor(Some(tx), 1, 10).await;

            drive_rounds(&actor, 2).await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert!(
                snap.status.is_paused(),
                "Blocked must pause; got {:?}",
                snap.status
            );
            assert_eq!(snap.consecutive_not_achieved, 0, "streak reset on Blocked");
            assert_eq!(snap.last_strategist_fired_at, 0);
            assert!(
                snap.last_strategy_recommendation.is_none(),
                "Blocked must clear the stale recommendation",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Persisted recommendation reaches the rendered continuation directive ─

#[tokio::test(flavor = "current_thread")]
#[serial]
async fn persisted_recommendation_renders_into_continuation_directive() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // No coordinator needed — we persist the recommendation directly
            // and exercise the real `run_goal_round_end` → `prepare_goal_continuation`
            // seam (no completions seeded ⇒ the drain is a no-op, goal stays Active).
            let (tx, _c) = spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, refuted(0));
            let (actor, _tmp) = make_actor(Some(tx), 5, 10).await;
            {
                let mut tracker = actor.goal_tracker.lock();
                tracker.record_strategy_recommendation(
                    "/tmp/goal/strategy.md".into(),
                    "Split the monolith into pure units.".into(),
                );
            }

            let decision = actor.run_goal_round_end().await;
            let GoalRoundDecision::Continue(directive) = decision else {
                panic!("expected Continue (active goal must keep running)");
            };

            assert!(
                directive.contains("A strategist reviewed")
                    && directive.contains("STRATEGIST RECOMMENDATION"),
                "continuation directive must carry the strategist narrative:\n{directive}",
            );
            assert!(
                directive.contains("Split the monolith into pure units."),
                "continuation directive must inline the persisted recommendation:\n{directive}",
            );
            assert!(
                directive.contains("/tmp/goal/strategy.md"),
                "continuation directive must point at the strategy note path:\n{directive}",
            );

            // One-shot: a second round must not replay the consumed note.
            let next = actor.run_goal_round_end().await;
            let GoalRoundDecision::Continue(next_directive) = next else {
                panic!("expected Continue (active goal must keep running)");
            };
            assert!(
                !next_directive.contains("STRATEGIST RECOMMENDATION")
                    && !next_directive.contains("Split the monolith into pure units."),
                "strategist note must not replay after being consumed:\n{next_directive}",
            );
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot_mut()
                    .expect("active goal")
                    .last_strategy_recommendation
                    .is_none(),
                "persisted recommendation must be cleared after one injection",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}

// ── Re-verify escalation: refuted churn that never re-calls update_goal ─

/// A refuted goal that keeps ending rounds without re-firing verification
/// must, once `rounds_since_verify` reaches the threshold, get a forceful
/// re-verify block in the continuation directive (and not before). Drives
/// the real `run_goal_round_end` → `prepare_goal_continuation` seam; uses
/// the default threshold (no env mutation) by pre-seeding the counter.
#[tokio::test(flavor = "current_thread")]
#[serial]
async fn refuted_continuation_escalates_to_reverify_at_threshold() {
    unsafe { std::env::set_var(ENV_FLAG, "1") };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (tx, _c) = spawn_coordinator(StrategistBehaviour::WriteNoteThenDone, refuted(0));
            let (actor, _tmp) = make_actor(Some(tx), 5, 10).await;
            // Arm the gate: one prior refutation, and the counter one round
            // below the threshold so the next two rounds straddle it.
            {
                let mut tracker = actor.goal_tracker.lock();
                let o = tracker.snapshot_mut().expect("active goal");
                o.consecutive_not_achieved = 1;
                o.rounds_since_verify = GOAL_REVERIFY_AFTER_DEFAULT - 2;
            }

            // Round 1: counter reaches threshold-1 ⇒ no escalation yet.
            let GoalRoundDecision::Continue(d1) = actor.run_goal_round_end().await else {
                panic!("expected Continue (active goal must keep running)");
            };
            assert!(
                !d1.contains("Re-verify before continuing.") && !d1.contains("STOP DRIFTING"),
                "no escalation below threshold:\n{d1}",
            );

            // Round 2: counter reaches the threshold ⇒ escalate with the count.
            let GoalRoundDecision::Continue(d2) = actor.run_goal_round_end().await else {
                panic!("expected Continue (active goal must keep running)");
            };
            assert!(
                d2.contains("Re-verify before continuing.")
                    && d2.contains("`update_goal(completed: true)`")
                    && d2.contains(&format!("{GOAL_REVERIFY_AFTER_DEFAULT} rounds")),
                "escalation must appear at threshold with the live count:\n{d2}",
            );
        })
        .await;
    unsafe { std::env::remove_var(ENV_FLAG) };
}
