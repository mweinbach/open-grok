//! Unit tests for the goal-mode degradation contract — back-off
//! streak, `handle_turn_end` dispatch, auto-pause helper, and the
//! idempotent `/goal resume` slash command.
use super::support::*;
use super::*;
use std::sync::atomic::Ordering;

async fn make_test_actor_with_active_goal() -> SessionActor {
    let (gateway_tx, _gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.goal_enabled = true;
    set_goal_harness_for_tests(&actor);
    actor.goal_tracker.lock().create_goal(
        "test-goal".to_string(),
        "test objective".to_string(),
        None,
        0,
        "2026-01-01T00:00:00Z".to_string(),
        None,
    );
    actor
}

#[tokio::test(flavor = "current_thread")]
async fn goal_backoff_pauses_after_three_consecutive_failed_turns() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            for _ in 0..GOAL_CONTINUATION_BACKOFF_THRESHOLD {
                actor.handle_turn_end(false).await;
            }
            let status = actor.goal_tracker.lock().status();
            assert_eq!(
                status,
                Some(crate::session::goal_tracker::GoalStatus::BackOffPaused)
            );
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_backoff_resets_on_success() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // Seed blocked streak so we can verify turn end leaves it alone.
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);
            actor.handle_turn_end(false).await; // streak = 1
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 1);
            actor.handle_turn_end(false).await; // streak = 2
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 2);
            actor.handle_turn_end(true).await; // streak = 0; continuation enqueued
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
            assert_eq!(
                actor.goal_blocked_streak.load(Ordering::Relaxed),
                2,
                "turn success must NOT reset the blocked streak (a model \
                 blocking once per turn would otherwise never hit 3/3)",
            );
            actor.handle_turn_end(false).await; // streak = 1
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 1);
            let status = actor.goal_tracker.lock().status();
            assert_eq!(
                status,
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "goal should remain Active when success reset the streak"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn auto_pause_goal_if_active_user_reason_transitions_to_user_paused() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .auto_pause_goal_if_active(crate::session::goal_tracker::GoalPauseReason::User)
                .await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::UserPaused)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn auto_pause_noop_when_goal_already_paused() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);
            actor
                .auto_pause_goal_if_active(crate::session::goal_tracker::GoalPauseReason::User)
                .await;
            // Should remain UserPaused — auto_pause is a no-op when not Active.
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::UserPaused)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_skip_increment_when_goal_not_active() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // Simulate the doom-loop branch having paused before handle_turn_end.
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);
            actor.handle_turn_end(false).await;
            // Streak should not increment because goal is not Active.
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::UserPaused)
            );
        })
        .await;
}

/// Seed a single InProgress todo so `has_pending_goal_todos`
/// returns true. Returns nothing; the bridge owns the resource.
async fn seed_one_pending_todo(actor: &SessionActor) {
    use crate::tools::todo::{TodoItem, TodoPriority, TodoState, TodoStatus};
    use xai_grok_tools::types::resources::State;
    let mut todos = TodoState::default();
    todos.push(
        "t1".into(),
        TodoItem {
            content: "finish the implementation".into(),
            priority: TodoPriority::default(),
            status: TodoStatus::InProgress,
            meta: None,
        },
    );
    actor
        .tool_bridge_handle()
        .update_resource(State(todos))
        .await;
}

/// Seed an EMPTY `TodoState` so `has_pending_goal_todos` returns
/// `false` rather than fail-open `true`. Used by the "bail
/// without pending todos" tests to exercise the fall-through path.
async fn seed_empty_todo_state(actor: &SessionActor) {
    use crate::tools::todo::TodoState;
    use xai_grok_tools::types::resources::State;
    actor
        .tool_bridge_handle()
        .update_resource(State(TodoState::default()))
        .await;
}

/// Push a `GoalClassifierNudge` input, standing in for the drain
/// having queued one on a classifier-rejected completion. Lets the
/// precedence test exercise the shared idempotency gate without
/// standing up the classifier sampler.
async fn seed_pending_classifier_nudge(actor: &SessionActor) {
    let (respond_to, _) = tokio::sync::oneshot::channel();
    actor
        .state
        .lock()
        .await
        .pending_inputs
        .push_back(InputItem {
            prompt_id: "test-classifier-nudge".into(),
            prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                "classifier nudge",
            ))],
            prompt_mode: crate::session::plan_mode::PromptMode::Agent,
            trace_gcs_config: None,
            artifact_tracker: None,
            client_identifier: None,
            screen_mode: None,
            verbatim: true,
            json_schema: None,
            origin: crate::session::PromptOrigin::GoalClassifierNudge,
            respond_to,
            persist_ack: None,
            parsed_prompt_tx: None,
            queue_meta: None,
            send_now: false,
        });
}

/// Read `events.jsonl` and return the parsed `Event` records.
/// The test actor writes events to a session-unique events file;
/// we read it as the canonical event sink.
///
/// Synchronous-flush requirement: the live `EventWriter::emit`
/// path takes the file mutex, calls `write_all`, and releases —
/// no internal buffering — so this helper can read immediately
/// after the producer awaits the call site that emits the event.
/// If `EventWriter` ever switches to a buffered or background
/// writer, every caller of this helper will need to flush
/// explicitly (or this helper must grow a `wait_for_flush` arg);
/// guard the contract from drifting silently.
fn read_events_jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    body.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

/// Replace the actor's `EventTracker` with one that writes to a
/// fresh per-test temp dir. Returns the temp dir guard so the
/// caller can read the produced `events.jsonl` and the dir is
/// dropped on test exit.
fn redirect_events_to_tempdir(actor: &mut SessionActor) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir must be creatable");
    actor.events = crate::session::events::EventTracker::new(dir.path());
    dir
}

/// Event-type label written by [`emit_event_sink_sentinel`].
const EVENT_SINK_SENTINEL_TYPE: &str = "turn_ended";

/// Emit a baseline `turn_ended` record into the redirected sink
/// before an action under test. The turn-end path under test
/// (`handle_turn_end` / `maybe_queue_goal_continuation`) never
/// emits `turn_ended` itself, so this is the only such record — it
/// lets a negative "no premature-stop event" assertion prove the
/// sink is live (file written and readable) rather than passing
/// vacuously on a missing or unwired events file.
fn emit_event_sink_sentinel(actor: &SessionActor) {
    actor.emit_turn_ended(
        crate::session::events::TurnOutcomeLabel::Completed,
        None,
        None,
    );
}

/// Assert the redirected sink is live (contains the
/// [`emit_event_sink_sentinel`] baseline) AND carries no
/// `goal_premature_stop_detected` event. Pairing the two ensures the
/// negative assertion can never pass on an empty or missing file.
fn assert_sink_live_without_premature_stop(events: &[serde_json::Value]) {
    assert!(
        events
            .iter()
            .any(|e| e.pointer("/type") == Some(&serde_json::json!(EVENT_SINK_SENTINEL_TYPE))),
        "event sink must contain the baseline sentinel (proves it is live): {events:#?}",
    );
    assert!(
        !events.iter().any(|e| {
            e.pointer("/type") == Some(&serde_json::json!("goal_premature_stop_detected"))
        }),
        "no premature-stop event must be recorded: {events:#?}",
    );
}

/// Bail text on a SUCCESS turn with pending todos. The completion
/// drain leaves the goal Active (no completion claim), so
/// `maybe_queue_goal_continuation` selects the bail-specific
/// nudge flavor. Confirms:
/// * The back-off streak RESETS to 0; the blocked streak is preserved.
/// * The continuation reminder is queued with the bail preface.
/// * `Event::GoalPrematureStopDetected { pattern: "giving_up" }`
///   is emitted exactly once; `Event::GoalAutoPaused` is NOT.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_bail_on_success_with_pending_todos_queues_bail_flavor() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
            .run_until(async {
                let mut actor = make_test_actor_with_active_goal().await;
                let event_dir = redirect_events_to_tempdir(&mut actor);
                actor.goal_continuation_streak.store(2, Ordering::Relaxed);
                actor.goal_blocked_streak.store(1, Ordering::Relaxed);
                seed_one_pending_todo(&actor).await;

                actor
                    .chat_state_handle
                    .push_assistant_response(ConversationItem::assistant("Giving up."));

                actor.handle_turn_end(true).await;

                assert_eq!(
                    actor.goal_continuation_streak.load(Ordering::Relaxed),
                    0,
                    "success branch must reset the back-off streak",
                );
                assert_eq!(
                    actor.goal_blocked_streak.load(Ordering::Relaxed),
                    1,
                    "success branch must NOT touch goal_blocked_streak",
                );
                assert_eq!(
                    actor.goal_tracker.lock().status(),
                    Some(crate::session::goal_tracker::GoalStatus::Active),
                    "still-Active goal must not auto-pause",
                );
                let nudge = {
                    let state = actor.state.lock().await;
                    state
                        .pending_inputs
                        .iter()
                        .find(|i| {
                            matches!(i.origin, crate::session::PromptOrigin::GoalSummary)
                        })
                        .map(|i| match i.prompt_blocks.first() {
                            Some(acp::ContentBlock::Text(t)) => t.text.clone(),
                            other => panic!("expected text block, got {other:?}"),
                        })
                        .expect("bail-on-success must queue a continuation reminder")
                };
                assert!(
                    nudge.contains("You appear to be stopping or handing off"),
                    "queued nudge must carry the bail preface:\n{nudge}",
                );
                assert!(
                    nudge.contains("Goal NOT complete — continue working."),
                    "bail nudge must still carry the generic directive body:\n{nudge}",
                );

                let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
                let detected = events
                    .iter()
                    .filter(|e| {
                        e.pointer("/type")
                            == Some(&serde_json::json!("goal_premature_stop_detected"))
                            && e.pointer("/pattern") == Some(&serde_json::json!("giving_up"))
                    })
                    .count();
                assert_eq!(
                    detected, 1,
                    "GoalPrematureStopDetected{{pattern: \"giving_up\"}} must fire exactly once: {events:#?}",
                );

                // The bail must ALSO record a PrematureStopDetected history
                // entry so it reaches the pager's Recent History via
                // `last_event` (not only the events.jsonl telemetry).
                {
                    let tracker = actor.goal_tracker.lock();
                    let o = tracker.snapshot().expect("active goal snapshot");
                    let last = o.history.last().expect("history must have an entry");
                    assert!(
                        matches!(
                            last.event,
                            crate::session::goal_tracker::GoalEvent::PrematureStopDetected
                        ),
                        "last history event must be PrematureStopDetected, got {:?}",
                        last.event,
                    );
                    assert_eq!(last.detail.as_deref(), Some("giving_up"));
                    match crate::session::goal_orchestrator::build_goal_updated(o, 0, 0) {
                        crate::extensions::notification::SessionUpdate::GoalUpdated {
                            last_event,
                            last_event_detail,
                            ..
                        } => {
                            assert_eq!(last_event.as_deref(), Some("premature_stop_detected"));
                            assert_eq!(last_event_detail.as_deref(), Some("giving_up"));
                        }
                        other => panic!("expected GoalUpdated, got {other:?}"),
                    }
                }

                let auto_paused = events.iter().any(|e| {
                    e.pointer("/type") == Some(&serde_json::json!("goal_auto_paused"))
                });
                assert!(
                    !auto_paused,
                    "bail-on-success path must NOT emit GoalAutoPaused: {events:#?}",
                );
            })
            .await;
}

/// Bail text on a SUCCESS turn but NO pending todos. With no
/// outstanding work the stop-detector is gated out, so the generic
/// continuation flavor renders and no premature-stop event fires.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_bail_on_success_without_pending_todos_uses_generic_flavor() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = make_test_actor_with_active_goal().await;
            let event_dir = redirect_events_to_tempdir(&mut actor);
            emit_event_sink_sentinel(&actor);
            seed_empty_todo_state(&actor).await;

            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant("Giving up."));

            actor.handle_turn_end(true).await;

            let nudge = {
                let state = actor.state.lock().await;
                state
                    .pending_inputs
                    .iter()
                    .find(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary))
                    .map(|i| match i.prompt_blocks.first() {
                        Some(acp::ContentBlock::Text(t)) => t.text.clone(),
                        other => panic!("expected text block, got {other:?}"),
                    })
                    .expect("success path must still queue a continuation reminder")
            };
            assert!(
                !nudge.contains("You appear to be stopping or handing off"),
                "no pending todos ⇒ generic flavor, no bail preface:\n{nudge}",
            );
            assert!(
                nudge.contains("Goal NOT complete — continue working."),
                "generic continuation body must still render:\n{nudge}",
            );

            let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
            assert_sink_live_without_premature_stop(&events);
        })
        .await;
}

/// Bail text on a SUCCESS turn whose completion claim is verified
/// during the turn-end drain. `update_goal(completed: true)`
/// transitions the goal to Complete inside
/// `maybe_queue_goal_continuation`'s drain, so the goal-active
/// gate returns before the stop-detector runs: no continuation is
/// queued and no premature-stop event fires even though the
/// turn-final text bails with pending todos.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_verified_complete_during_drain_skips_bail_nudge() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = make_test_actor_with_active_goal().await;
            let event_dir = redirect_events_to_tempdir(&mut actor);
            emit_event_sink_sentinel(&actor);
            seed_one_pending_todo(&actor).await;
            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant("Giving up."));

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: Some(true),
                        message: None,
                        blocked_reason: None,
                    },
                ),
            )
            .unwrap();
            drop(tx);

            actor.handle_turn_end(true).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
                "completed: true must transition the goal to Complete during the drain",
            );
            let state = actor.state.lock().await;
            assert!(
                !state.pending_inputs.iter().any(|i| {
                    matches!(
                        i.origin,
                        crate::session::PromptOrigin::GoalSummary
                            | crate::session::PromptOrigin::GoalClassifierNudge,
                    )
                }),
                "verified-complete goal must NOT queue a continuation reminder",
            );
            let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
            assert_sink_live_without_premature_stop(&events);
        })
        .await;
}

/// A SUCCESS turn against a non-Active goal skips the whole branch:
/// `maybe_queue_goal_continuation` is never reached, so neither
/// streak moves, nothing queues, and no event fires.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_success_skips_when_goal_not_active() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = make_test_actor_with_active_goal().await;
            let event_dir = redirect_events_to_tempdir(&mut actor);
            emit_event_sink_sentinel(&actor);
            seed_one_pending_todo(&actor).await;
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);
            actor.goal_continuation_streak.store(7, Ordering::Relaxed);
            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant("Giving up."));

            actor.handle_turn_end(true).await;

            assert_eq!(
                actor.goal_continuation_streak.load(Ordering::Relaxed),
                7,
                "non-Active goal must leave the streak untouched",
            );
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::UserPaused),
                "pause cause must be preserved",
            );
            let state = actor.state.lock().await;
            assert!(
                state.pending_inputs.is_empty(),
                "non-Active goal must NOT queue a continuation reminder",
            );
            let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
            assert_sink_live_without_premature_stop(&events);
        })
        .await;
}

/// Bail text on a SUCCESS turn with the `TodoState` resource
/// missing. `has_pending_goal_todos` fails OPEN, so the bail flavor
/// still renders and the premature-stop event still fires —
/// safety-net intent over a precise pending-todo count.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_bail_on_success_fails_open_when_todo_resource_missing() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = make_test_actor_with_active_goal().await;
            let event_dir = redirect_events_to_tempdir(&mut actor);
            // No `seed_one_pending_todo` — the State<TodoState>
            // resource is never inserted, so `read_resource` returns
            // None and `has_pending_goal_todos` fails OPEN.
            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant("Giving up."));

            actor.handle_turn_end(true).await;

            let nudge = {
                let state = actor.state.lock().await;
                state
                    .pending_inputs
                    .iter()
                    .find(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary))
                    .map(|i| match i.prompt_blocks.first() {
                        Some(acp::ContentBlock::Text(t)) => t.text.clone(),
                        other => panic!("expected text block, got {other:?}"),
                    })
                    .expect("fail-open must still queue a continuation reminder")
            };
            assert!(
                nudge.contains("You appear to be stopping or handing off"),
                "missing TodoState must fail OPEN to the bail flavor:\n{nudge}",
            );
            let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
            assert!(
                events.iter().any(|e| {
                    e.pointer("/type") == Some(&serde_json::json!("goal_premature_stop_detected"))
                }),
                "fail-open must still emit the premature-stop event: {events:#?}",
            );
        })
        .await;
}

/// The dominant production path: a clean SUCCESS turn whose
/// final text is ordinary progress narration, with pending todos.
/// The stop-detector must NOT trip, so the generic continuation
/// flavor renders (bail preface absent) and no premature-stop event
/// fires. Guards against the detector false-positiving on routine
/// narration or the generic path spuriously emitting.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_success_ordinary_text_uses_generic_flavor() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = make_test_actor_with_active_goal().await;
            let event_dir = redirect_events_to_tempdir(&mut actor);
            emit_event_sink_sentinel(&actor);
            seed_one_pending_todo(&actor).await;
            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant(
                    "Implemented the helper and ran the tests.",
                ));

            actor.handle_turn_end(true).await;

            let nudge = {
                let state = actor.state.lock().await;
                state
                    .pending_inputs
                    .iter()
                    .find(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary))
                    .map(|i| match i.prompt_blocks.first() {
                        Some(acp::ContentBlock::Text(t)) => t.text.clone(),
                        other => panic!("expected text block, got {other:?}"),
                    })
                    .expect("clean success path must queue a continuation reminder")
            };
            assert!(
                !nudge.contains("You appear to be stopping or handing off"),
                "ordinary narration must render the generic flavor, no bail preface:\n{nudge}",
            );
            assert!(
                nudge.contains("Goal NOT complete — continue working."),
                "generic continuation body must render:\n{nudge}",
            );

            let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
            assert_sink_live_without_premature_stop(&events);
        })
        .await;
}

/// Cross-site exactly-once. Both queue sites
/// (`handle_completion`'s `TurnOutcome::Completed` arm and the
/// `handle_turn_end` safety net) call `maybe_queue_goal_continuation`.
/// Driving two back-to-back calls with bail text + pending todos
/// must push exactly one `GoalSummary` AND emit exactly one
/// `goal_premature_stop_detected` — the second call early-returns
/// at the idempotency gate before the emit. Pins the reason
/// `emit_event` sits BELOW the gate: a regression moving it above
/// would double-emit and fail this count.
#[tokio::test(flavor = "current_thread")]
async fn maybe_queue_goal_continuation_emits_premature_stop_at_most_once_across_sites() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = make_test_actor_with_active_goal().await;
            let event_dir = redirect_events_to_tempdir(&mut actor);
            seed_one_pending_todo(&actor).await;
            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant("Giving up."));

            actor.maybe_queue_goal_continuation().await;
            actor.maybe_queue_goal_continuation().await;

            {
                let state = actor.state.lock().await;
                let goal_summary = state
                    .pending_inputs
                    .iter()
                    .filter(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary))
                    .count();
                assert_eq!(
                    goal_summary, 1,
                    "two calls must push exactly one GoalSummary continuation",
                );
            }
            let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
            let detected = events
                .iter()
                .filter(|e| {
                    e.pointer("/type") == Some(&serde_json::json!("goal_premature_stop_detected"))
                })
                .count();
            assert_eq!(
                detected, 1,
                "premature-stop event must fire exactly once across both calls: {events:#?}",
            );
        })
        .await;
}

/// Classifier-rejection × bail precedence. When a completion claim
/// is classifier-rejected during the drain the goal stays Active
/// and a `GoalClassifierNudge` is queued (here pre-seeded to stand
/// in for that drain outcome). The shared idempotency gate matches
/// `GoalClassifierNudge`, so `maybe_queue_goal_continuation`
/// early-returns before the emit: the bail flavor is suppressed and
/// no premature-stop event fires. The classifier nudge already
/// forces continuation with the gap inlined, so this precedence is
/// intentional — pinned so a future gate change is caught.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_classifier_nudge_pre_empts_bail_nudge_and_event() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut actor = make_test_actor_with_active_goal().await;
            let event_dir = redirect_events_to_tempdir(&mut actor);
            emit_event_sink_sentinel(&actor);
            seed_one_pending_todo(&actor).await;
            actor
                .chat_state_handle
                .push_assistant_response(ConversationItem::assistant("Giving up."));
            seed_pending_classifier_nudge(&actor).await;

            actor.handle_turn_end(true).await;

            {
                let state = actor.state.lock().await;
                let goal_summary = state
                    .pending_inputs
                    .iter()
                    .filter(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary))
                    .count();
                let classifier_nudge = state
                    .pending_inputs
                    .iter()
                    .filter(|i| {
                        matches!(i.origin, crate::session::PromptOrigin::GoalClassifierNudge)
                    })
                    .count();
                assert_eq!(
                    goal_summary, 0,
                    "a pending classifier nudge must suppress the bail continuation",
                );
                assert_eq!(
                    classifier_nudge, 1,
                    "the classifier nudge must remain the sole queued continuation",
                );
            }
            let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
            assert_sink_live_without_premature_stop(&events);
        })
        .await;
}

/// The non-success path always increments the streak, regardless
/// of the turn-final text (bail or ordinary narration) and
/// regardless of pending todos — stop-detection no longer lives
/// here. Pre-load streak = 1 and assert the genuine 1 → 2 tick
/// (not a tautological 0 → 0), no auto-pause below threshold, no
/// continuation queued, no event, and `goal_blocked_streak`
/// untouched.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_non_success_increments_streak_regardless_of_text() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            for text in ["Giving up.", "Implemented the helper and ran the tests."] {
                let mut actor = make_test_actor_with_active_goal().await;
                let event_dir = redirect_events_to_tempdir(&mut actor);
                emit_event_sink_sentinel(&actor);
                seed_one_pending_todo(&actor).await;
                actor.goal_continuation_streak.store(1, Ordering::Relaxed);
                actor.goal_blocked_streak.store(3, Ordering::Relaxed);
                actor
                    .chat_state_handle
                    .push_assistant_response(ConversationItem::assistant(text));

                actor.handle_turn_end(false).await;

                assert_eq!(
                    actor.goal_continuation_streak.load(Ordering::Relaxed),
                    2,
                    "non-success must tick the streak 1 → 2 for text {text:?}",
                );
                assert_eq!(
                    actor.goal_blocked_streak.load(Ordering::Relaxed),
                    3,
                    "non-success must NOT touch goal_blocked_streak for text {text:?}",
                );
                assert_eq!(
                    actor.goal_tracker.lock().status(),
                    Some(crate::session::goal_tracker::GoalStatus::Active),
                    "below-threshold tick must NOT auto-pause for text {text:?}",
                );
                let state = actor.state.lock().await;
                assert!(
                    state.pending_inputs.is_empty(),
                    "non-success must NOT queue a continuation reminder for text {text:?}",
                );
                let events = read_events_jsonl(&event_dir.path().join("events.jsonl"));
                assert_sink_live_without_premature_stop(&events);
            }
        })
        .await;
}

/// The non-success back-off threshold. Consecutive non-success
/// turns increment the streak unconditionally; at
/// `GOAL_CONTINUATION_BACKOFF_THRESHOLD` the goal auto-pauses with
/// `BackOffPaused`. Mixing bail and ordinary text proves the
/// turn-final text no longer influences this path.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_non_success_auto_pauses_at_backoff_threshold() {
    use crate::sampling::ConversationItem;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            seed_one_pending_todo(&actor).await;

            for i in 0..GOAL_CONTINUATION_BACKOFF_THRESHOLD {
                let text = if i % 2 == 0 {
                    "Giving up."
                } else {
                    "Implemented more of the helper."
                };
                actor
                    .chat_state_handle
                    .push_assistant_response(ConversationItem::assistant(text));
                actor.handle_turn_end(false).await;
            }

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BackOffPaused),
                "consecutive non-success turns must auto-pause at the threshold",
            );
            assert_eq!(
                actor.goal_continuation_streak.load(Ordering::Relaxed),
                0,
                "the auto-pause must reset the streak",
            );
        })
        .await;
}

fn sample_turn_infra_err() -> PromptTurnResult {
    Err(
        acp::Error::internal_error().data(crate::sampling::error::error_data_with_status(
            "upstream unavailable".into(),
            Some(503),
        )),
    )
}

fn sample_turn_invalid_request_err() -> PromptTurnResult {
    Err(acp::Error::invalid_request().data("bad prompt shape"))
}

async fn simulate_completion_with_result(actor: &SessionActor, result: PromptTurnResult) {
    let (turn_succeeded, infra_pause_message) =
        SessionActor::post_turn_goal_degradation_plan(&result);
    if let Some(message) = infra_pause_message {
        actor.apply_infra_pause_after_turn_err(message).await;
    }
    actor.handle_turn_end(turn_succeeded).await;
}

fn agent_message_text_from_notification(n: &acp::SessionNotification) -> Option<String> {
    match &n.update {
        acp::SessionUpdate::AgentMessageChunk(chunk) => match &chunk.content {
            acp::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn spawn_gateway_notification_capture(
    mut gateway_rx: tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>,
) -> std::sync::Arc<tokio::sync::Mutex<Vec<acp::SessionNotification>>> {
    let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let sent_for_task = sent.clone();
    tokio::task::spawn_local(async move {
        while let Some(msg) = gateway_rx.recv().await {
            if let xai_acp_lib::AcpClientMessage::SessionNotification(args) = msg {
                sent_for_task.lock().await.push(args.request);
                let _ = args.response_tx.send(Ok(()));
            }
        }
    });
    sent
}

fn spawn_replay_event_drainer(
    actor: std::sync::Arc<SessionActor>,
    mut event_rx: tokio::sync::mpsc::UnboundedReceiver<SessionEvent>,
) {
    let settings = actor.buffering_settings.clone();
    tokio::task::spawn_local(async move {
        let mut replay_buffer = ReplayBuffer::new(settings);
        while let Some(event) = event_rx.recv().await {
            match event {
                SessionEvent::Notification(notification) => {
                    if let Some((primary, secondary)) = replay_buffer.consume_chunk(notification) {
                        actor.emit_buffered(primary).await;
                        if let Some(extra) = secondary {
                            actor.emit_buffered(extra).await;
                        }
                    }
                }
                SessionEvent::FlushReplay { respond_to } => {
                    if let Some(notification) = replay_buffer.flush() {
                        actor.emit_buffered(notification).await;
                    }
                    if let Some(tx) = respond_to {
                        let _ = tx.send(());
                    }
                }
            }
        }
    });
}

async fn make_test_actor_with_active_goal_and_gateway_capture() -> (
    std::sync::Arc<SessionActor>,
    std::sync::Arc<tokio::sync::Mutex<Vec<acp::SessionNotification>>>,
) {
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let sent = spawn_gateway_notification_capture(gateway_rx);
    let (persistence_tx, _persistence_rx) =
        tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let (actor, event_rx) = create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
    actor.goal_tracker.lock().create_goal(
        "test-goal".to_string(),
        "test objective".to_string(),
        None,
        0,
        "2026-01-01T00:00:00Z".to_string(),
        None,
    );
    let actor = std::sync::Arc::new(actor);
    spawn_replay_event_drainer(actor.clone(), event_rx);
    (actor, sent)
}

#[test]
fn is_infra_turn_error_allowlist() {
    let cases: &[(&str, acp::Error, bool)] = &[
        ("internal", acp::Error::internal_error(), true),
        (
            "rate_limit",
            acp::Error::new(
                crate::sampling::error::RATE_LIMITED_ERROR_CODE,
                "too many requests".to_string(),
            ),
            true,
        ),
        ("auth", acp::Error::auth_required(), true),
        ("invalid_request", acp::Error::invalid_request(), false),
        (
            "unmapped_code",
            acp::Error::new(-99999, "custom failure".to_string()),
            false,
        ),
    ];
    for (label, err, expected) in cases {
        assert_eq!(SessionActor::is_infra_turn_error(err), *expected, "{label}");
    }
}

#[test]
fn format_turn_error_message_uses_message_and_classifies() {
    let err = acp::Error::new(
        crate::sampling::error::RATE_LIMITED_ERROR_CODE,
        "too many requests".to_string(),
    );
    assert_eq!(
        SessionActor::format_turn_error_message(&err),
        "Turn failed: too many requests"
    );
}

#[test]
fn format_turn_error_message_falls_back_to_data_detail() {
    let err = acp::Error::internal_error().data(serde_json::json!({
        "detail": "connection reset"
    }));
    assert_eq!(
        SessionActor::format_turn_error_message(&err),
        "Turn failed: connection reset"
    );
}

#[test]
fn format_turn_error_message_reads_error_data_with_status() {
    let err = acp::Error::internal_error().data(crate::sampling::error::error_data_with_status(
        "upstream unavailable".into(),
        Some(503),
    ));
    assert_eq!(
        SessionActor::format_turn_error_message(&err),
        "Turn failed: upstream unavailable"
    );
}

#[test]
fn format_turn_error_message_falls_back_to_classify_when_no_detail() {
    let err = acp::Error::new(acp::ErrorCode::InternalError.into(), String::new());
    assert_eq!(
        SessionActor::format_turn_error_message(&err),
        "Turn failed: internal"
    );
}

#[test]
fn format_turn_error_message_prefers_data_message_over_err_message() {
    let err = acp::Error::new(
        acp::ErrorCode::InternalError.into(),
        "Internal error".to_string(),
    )
    .data(Some(crate::sampling::error::error_data_with_status(
        "upstream unavailable".into(),
        Some(503),
    )));
    assert_eq!(
        SessionActor::format_turn_error_message(&err),
        "Turn failed: upstream unavailable"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn infra_err_pauses_active_goal_immediately_with_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            simulate_completion_with_result(&actor, sample_turn_infra_err()).await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::InfraPaused)
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .and_then(|o| o.pause_message.clone()),
                Some("Turn failed: upstream unavailable".into())
            );
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn infra_err_does_not_require_three_strikes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            simulate_completion_with_result(&actor, sample_turn_infra_err()).await;
            assert_ne!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BackOffPaused)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_turn_uses_backoff_streak_not_infra_pause() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let cancelled: PromptTurnResult = Ok(crate::session::commands::PromptTurnOk {
                stop_reason: acp::StopReason::EndTurn,
                total_tokens: 0,
                turn_snapshot: None,
                completion_kind: crate::session::commands::PromptCompletionKind::Cancelled {
                    category: None,
                    context: None,
                },
                structured_output: None,
                usage: None,
            });
            simulate_completion_with_result(&actor, cancelled).await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active)
            );
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_skip_increment_when_infra_paused_first() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            simulate_completion_with_result(&actor, sample_turn_infra_err()).await;
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::InfraPaused)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_request_err_uses_backoff_not_infra_pause() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            simulate_completion_with_result(&actor, sample_turn_invalid_request_err()).await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active)
            );
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn infra_auto_pause_noop_when_not_active() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            simulate_completion_with_result(&actor, sample_turn_infra_err()).await;
            let first_message = actor
                .goal_tracker
                .lock()
                .snapshot()
                .and_then(|o| o.pause_message.clone());
            let noop = actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Infra,
                    "Turn failed: upstream unavailable".into(),
                )
                .await;
            assert!(
                !noop,
                "second infra auto-pause must be no-op when not Active"
            );
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::InfraPaused)
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .and_then(|o| o.pause_message.clone()),
                first_message
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn infra_auto_pause_sends_slash_command_output() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, sent) = make_test_actor_with_active_goal_and_gateway_capture().await;
            simulate_completion_with_result(actor.as_ref(), sample_turn_infra_err()).await;
            tokio::task::yield_now().await;
            let texts: Vec<String> = sent
                .lock()
                .await
                .iter()
                .filter_map(agent_message_text_from_notification)
                .collect();
            let combined = texts.join("\n");
            assert!(
                combined.contains("Goal paused due to turn error"),
                "slash output must announce infra auto-pause, got:\n{combined}"
            );
            assert!(
                combined.contains("upstream unavailable"),
                "slash output must include error detail, got:\n{combined}"
            );
            assert!(
                combined.contains("/goal resume"),
                "slash output must mention resume, got:\n{combined}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_turn_without_infra_error_does_not_auto_pause_goal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let cancelled: PromptTurnResult = Ok(crate::session::commands::PromptTurnOk {
                stop_reason: acp::StopReason::EndTurn,
                total_tokens: 0,
                turn_snapshot: None,
                completion_kind: crate::session::commands::PromptCompletionKind::Cancelled {
                    category: Some(
                        xai_file_utils::events::types::CancellationCategory::MidTurnAbort,
                    ),
                    context: None,
                },
                structured_output: None,
                usage: None,
            });
            simulate_completion_with_result(&actor, cancelled).await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "non-infra cancellation must not auto-pause the goal"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_from_infra_paused_transitions_to_active() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_continuation_streak.store(2, Ordering::Relaxed);
            actor.goal_tracker.lock().pause_with_message(
                crate::session::goal_tracker::GoalPauseReason::Infra,
                "Turn failed: upstream unavailable".into(),
            );
            let actor = Arc::new(actor);
            let _ = actor.resume_goal().await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active)
            );
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .pause_message
                    .is_none()
            );
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_from_infra_paused_reminder_uses_infra_copy() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_tracker.lock().pause_with_message(
                crate::session::goal_tracker::GoalPauseReason::Infra,
                "Turn failed: upstream unavailable".into(),
            );
            let actor = Arc::new(actor);
            let GoalResumeOutcome::Inference { reminder: text, .. } = actor.resume_goal().await
            else {
                panic!("resumed infra-paused goal must flow through to inference");
            };
            assert!(
                text.contains("Continue working now."),
                "the located item must be the resume reminder:\n{text}"
            );
            assert!(
                text.contains("Previous state: Paused (infrastructure error)"),
                "resume reminder must use infra copy:\n{text}"
            );
            assert!(
                text.contains("Previous error: Turn failed: upstream unavailable"),
                "resume reminder must surface infra pause_message:\n{text}"
            );
            assert!(
                !text.contains("Previous state: Blocked"),
                "infra resume must not use Blocked recap:\n{text}"
            );
            assert_resume_recap_discipline_tracking_order(
                &text,
                "Previous state: Paused (infrastructure error)",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_from_active_nudges_and_resets_streak() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_continuation_streak.store(2, Ordering::Relaxed);
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);
            let actor = Arc::new(actor);
            let GoalResumeOutcome::Inference { reminder: text, .. } = actor.resume_goal().await
            else {
                panic!("nudged active goal must flow through to inference");
            };
            // Both streaks reset on resume.
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
            assert_eq!(actor.goal_blocked_streak.load(Ordering::Relaxed), 0);
            // Status remains Active.
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active)
            );
            assert!(
                text.contains("TRACKING:") || text.contains("goal"),
                "active-nudge path must build the continuation reminder:\n{text}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn maybe_queue_goal_continuation_is_idempotent() {
    // Two back-to-back calls against an Active goal must enqueue
    // exactly one `GoalSummary` InputItem. This is the dedup
    // contract that lets the pre-`emit_turn_ended` queue site and
    // the `handle_turn_end` safety net coexist without doubling up
    // the continuation reminder.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;

            actor.maybe_queue_goal_continuation().await;
            actor.maybe_queue_goal_continuation().await;

            let state = actor.state.lock().await;
            let goal_summary_count = state
                .pending_inputs
                .iter()
                .filter(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary))
                .count();
            assert_eq!(
                goal_summary_count, 1,
                "two consecutive calls must push exactly one GoalSummary item"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_from_user_paused_transitions_to_active_and_resets_streak() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);
            actor.goal_continuation_streak.store(2, Ordering::Relaxed);
            let actor = Arc::new(actor);
            let _ = actor.resume_goal().await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active)
            );
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_from_back_off_paused_transitions_to_active() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::BackOff);
            let actor = Arc::new(actor);
            let _ = actor.resume_goal().await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active)
            );
        })
        .await;
}

// -- Verification / Blocked path --

#[tokio::test(flavor = "current_thread")]
async fn auto_pause_goal_if_active_with_message_transitions_to_blocked_and_stashes_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Verification,
                    "no windows sdk".into(),
                )
                .await;
            let tracker = actor.goal_tracker.lock();
            assert_eq!(
                tracker.status(),
                Some(crate::session::goal_tracker::GoalStatus::Blocked)
            );
            assert_eq!(
                tracker.snapshot().unwrap().pause_message.as_deref(),
                Some("no windows sdk"),
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_blocked_reason_transitions_after_three_attempts() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // With the 3-turn threshold, first two blocked attempts are rejected.
            // Only the third transitions to Blocked.
            for i in 0..3 {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                *actor.goal_update_rx.borrow_mut() = Some(rx);
                tx.send(
                    xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                        xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                            completed: None,
                            message: Some("longer body".into()),
                            blocked_reason: Some("short label".into()),
                        },
                    ),
                )
                .unwrap();
                drop(tx);
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
                if i < 2 {
                    assert_eq!(
                        actor.goal_tracker.lock().status(),
                        Some(crate::session::goal_tracker::GoalStatus::Active),
                        "attempt {} should not block yet",
                        i + 1,
                    );
                }
            }

            let tracker = actor.goal_tracker.lock();
            assert_eq!(
                tracker.status(),
                Some(crate::session::goal_tracker::GoalStatus::Blocked)
            );
            assert_eq!(
                tracker.snapshot().unwrap().pause_message.as_deref(),
                Some("short label\nlonger body"),
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_blocked_reason_rejected_below_threshold() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: None,
                        message: None,
                        blocked_reason: Some("only label".into()),
                    },
                ),
            )
            .unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            let tracker = actor.goal_tracker.lock();
            // First blocked attempt is rejected (threshold = 3).
            assert_eq!(
                tracker.status(),
                Some(crate::session::goal_tracker::GoalStatus::Active)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn auto_pause_with_message_returns_false_when_goal_already_paused() {
    // The new `bool` return value is the gate that suppresses the
    // user-visible chat notification on a no-op pause.
    // Test it directly: seed UserPaused, attempt a Verification
    // pause, expect (i) `false`, (ii) status stays UserPaused,
    // (iii) `pause_message` stays None.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);

            let applied = actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Verification,
                    "would-block".into(),
                )
                .await;

            assert!(!applied, "no-op pause must return false");
            let tracker = actor.goal_tracker.lock();
            assert_eq!(
                tracker.status(),
                Some(crate::session::goal_tracker::GoalStatus::UserPaused)
            );
            assert!(tracker.snapshot().unwrap().pause_message.is_none());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn auto_pause_with_message_returns_true_when_goal_was_active() {
    // Sibling positive case: the successful path must return true.
    // Pinning both branches keeps the chat-notification gate
    // honest (the matching contract).
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;

            let applied = actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Verification,
                    "real-block".into(),
                )
                .await;

            assert!(applied, "Active → Blocked pause must return true");
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Blocked)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_blocked_reason_against_non_active_does_not_stash_pause_message() {
    // Wire-level coverage: a blocked update against a
    // non-Active goal (here UserPaused) must NOT transition status,
    // must NOT stash a pause_message, and the subsequent drain
    // semantics behave as if no blocked signal had been sent.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: None,
                        message: Some("body".into()),
                        blocked_reason: Some("would-block".into()),
                    },
                ),
            )
            .unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            let tracker = actor.goal_tracker.lock();
            assert_eq!(
                tracker.status(),
                Some(crate::session::goal_tracker::GoalStatus::UserPaused),
                "no-op blocked signal must not transition status"
            );
            assert!(
                tracker.snapshot().unwrap().pause_message.is_none(),
                "no-op blocked signal must not stash a pause_message"
            );
            drop(tracker);
            // The streak still increments even against a non-Active
            // goal — the streak counter is independent of the pause
            // state. This is intentional: the model's blocked
            // attempts count regardless of the goal's current status.
            assert_eq!(actor.goal_blocked_streak.load(Ordering::Relaxed), 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_completes_after_blocked_does_not_leak_pause_message() {
    let local = tokio::task::LocalSet::new();
    local
            .run_until(async {
                let actor = make_test_actor_with_active_goal().await;
                // Pre-set streak to 2 so the next blocked attempt triggers.
                actor
                    .goal_blocked_streak
                    .store(2, Ordering::Relaxed);
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                *actor.goal_update_rx.borrow_mut() = Some(rx);
                tx.send(xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: None,
                        message: None,
                        blocked_reason: Some("blk".into()),
                    }
)).unwrap();
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
                assert_eq!(
                    actor.goal_tracker.lock().status(),
                    Some(crate::session::goal_tracker::GoalStatus::Blocked)
                );

                // Now in a fresh drain, send completed: true. The shell
                // accepts complete() from any paused variant (including
                // Blocked), and the pause_message
                // is cleared during the transition.
                tx.send(xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {

                        completed: Some(true),
                        message: None,
                        blocked_reason: None,
                    }
)).unwrap();
                drop(tx);
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

                let tracker = actor.goal_tracker.lock();
                assert_eq!(
                    tracker.status(),
                    Some(crate::session::goal_tracker::GoalStatus::Complete)
                );
                assert!(
                    tracker.snapshot().unwrap().pause_message.is_none(),
                    "complete() must clear pause_message — otherwise a stale Reason: line leaks to the pager"
                );
            })
            .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_skips_subsequent_completed_after_block() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // Pre-set streak to 2 so the blocked_reason triggers on the 3rd.
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: None,
                        message: None,
                        blocked_reason: Some("X".into()),
                    },
                ),
            )
            .unwrap();
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: Some(true),
                        message: None,
                        blocked_reason: None,
                    },
                ),
            )
            .unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            let tracker = actor.goal_tracker.lock();
            assert_eq!(
                tracker.status(),
                Some(crate::session::goal_tracker::GoalStatus::Blocked),
                "blocked transition must absorb the follow-up `completed: true`"
            );
            assert_eq!(
                tracker.snapshot().unwrap().pause_message.as_deref(),
                Some("X"),
                "pause_message from the block must survive the would-be Complete"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_from_blocked_transitions_to_active_and_clears_pause_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);
            actor.goal_tracker.lock().pause_with_message(
                crate::session::goal_tracker::GoalPauseReason::Verification,
                "previous reason".into(),
            );
            let actor = Arc::new(actor);
            let _ = actor.resume_goal().await;
            let tracker = actor.goal_tracker.lock();
            assert_eq!(
                tracker.status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "resume from Blocked must reach Active"
            );
            assert!(
                tracker.snapshot().unwrap().pause_message.is_none(),
                "resume must clear pause_message",
            );
            drop(tracker);
            assert_eq!(
                actor.goal_blocked_streak.load(Ordering::Relaxed),
                0,
                "resume must reset blocked streak"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_reminder_includes_previous_block_reason() {
    let local = tokio::task::LocalSet::new();
    local
            .run_until(async {
                let actor = make_test_actor_with_active_goal().await;
                actor.goal_tracker.lock().pause_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Verification,
                    "previous reason".into(),
                );
                let actor = Arc::new(actor);
                let GoalResumeOutcome::Inference { reminder: text, .. } = actor.resume_goal().await
                else {
                    panic!("resumed blocked goal must flow through to inference");
                };
                assert!(
                    text.contains("Continue working now."),
                    "the located item must be the resume reminder:\n{text}"
                );
                // The block recap leads with `Previous state: Blocked.` so the
                // model has unambiguous context about the prior status.
                assert!(
                    text.contains("Previous state: Blocked"),
                    "resume reminder must lead with the literal `Previous state: Blocked` clause:\n{text}"
                );
                assert!(
                    text.contains("Previous block reason: previous reason"),
                    "resume reminder must surface the previous pause_message verbatim:\n{text}"
                );
                assert!(
                    text.contains("Re-evaluate whether the blocker has been addressed"),
                    "resume reminder must prompt re-evaluation:\n{text}"
                );
                assert_resume_recap_discipline_tracking_order(&text, "Previous state: Blocked");
            })
            .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_reminder_omits_block_recap_when_no_pause_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // A non-Blocked pause (UserPaused) carries no pause_message,
            // so the resume reminder must NOT mention "Previous block
            // reason" — the recap is gated on pause_message presence.
            let actor = make_test_actor_with_active_goal().await;
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);
            let actor = Arc::new(actor);
            let GoalResumeOutcome::Inference { reminder: text, .. } = actor.resume_goal().await
            else {
                panic!("resumed user-paused goal must flow through to inference");
            };
            assert!(
                !text.contains("Previous block reason"),
                "user-pause resume must not surface a block-reason recap:\n{text}"
            );
        })
        .await;
}

#[test]
fn format_blocked_chat_notification_with_detail_has_full_layout() {
    let text = super::format_blocked_chat_notification("short label", Some("long body"));
    // Exact layout: header / Reason row / detail body / blank /
    // resume hint. Pinned so future copy edits cannot drift without
    // an explicit update here.
    assert_eq!(
        text,
        "Goal paused — verification blocked.\n\
             Reason: short label\n\
             long body\n\
             \n\
             Type /goal resume to continue after you've addressed it.",
    );
}

#[test]
fn format_blocked_chat_notification_without_detail_skips_body() {
    let text = super::format_blocked_chat_notification("only label", None);
    // The "Reason:" row is immediately followed by the blank-line +
    // resume hint, with no empty body row between the two — the
    // body block is conditionally rendered. Pin the exact layout to
    // guard against either side gaining or losing a stray newline.
    assert_eq!(
        text,
        "Goal paused — verification blocked.\n\
             Reason: only label\n\
             \n\
             Type /goal resume to continue after you've addressed it.",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn goal_pause_from_blocked_is_already_paused() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // /goal pause on a Blocked goal should yield the "Goal is
            // already paused" branch — Blocked is one of the paused
            // variants in the GoalPause match.
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_tracker.lock().pause_with_message(
                crate::session::goal_tracker::GoalPauseReason::Verification,
                "X".into(),
            );
            let actor = Arc::new(actor);
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GoalPause)
                .await;
            // Status remains Blocked — pause is a no-op.
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Blocked)
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_auto_paused_event_emits_verification_reason() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.events = crate::session::events::EventTracker::new(tmp.path());
            actor.goal_tracker.lock().create_goal(
                "test-goal".to_string(),
                "test objective".to_string(),
                None,
                0,
                "2026-01-01T00:00:00Z".to_string(),
                None,
            );

            actor
                .auto_pause_goal_if_active_with_message(
                    crate::session::goal_tracker::GoalPauseReason::Verification,
                    "no windows sdk".into(),
                )
                .await;
            drop(actor);

            let log = std::fs::read_to_string(tmp.path().join("events.jsonl"))
                .expect("events.jsonl must exist after auto_pause_with_message");
            assert!(
                log.lines().any(|line| {
                    let val: serde_json::Value = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(_) => return false,
                    };
                    val["type"] == "goal_auto_paused" && val["reason"] == "verification"
                }),
                "expected goal_auto_paused with reason=verification:\n{log}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_resume_on_complete_returns_informational_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_tracker.lock().complete();
            let actor = Arc::new(actor);
            // A completed goal is a terminal case: resume returns a message
            // and the caller ends the turn (no inference).
            let outcome = actor.resume_goal().await;
            assert!(
                matches!(outcome, GoalResumeOutcome::Message(_)),
                "resume on a complete goal must end the turn with a message"
            );
            // Status should remain Complete (no transition).
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete)
            );
        })
        .await;
}

/// New-contract guard: resuming a paused goal must flow through to a
/// normal inference turn (same path as the initial `/goal`), returning
/// the goal system-reminder as the turn's prompt content rather than a
/// terminal message. Mirrors how `setup_goal` seeds the initial turn.
#[tokio::test(flavor = "current_thread")]
async fn goal_resume_paused_flows_through_to_inference_with_reminder() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .goal_tracker
                .lock()
                .pause(crate::session::goal_tracker::GoalPauseReason::User);
            let actor = Arc::new(actor);

            let GoalResumeOutcome::Inference { reminder, user_msg } = actor.resume_goal().await
            else {
                panic!("resuming a paused goal must run a real inference turn");
            };

            assert!(
                reminder.starts_with("<system-reminder>"),
                "resume must seed the goal system-reminder as prompt content:\n{reminder}"
            );
            assert!(
                reminder.contains("test objective"),
                "resume reminder must carry the objective:\n{reminder}"
            );
            assert!(
                reminder.contains("Continue working now."),
                "resume reminder must close with the continuation directive:\n{reminder}"
            );
            assert_eq!(user_msg, "Goal resumed.");
        })
        .await;
}

/// Terminal case: `/goal resume` with no goal set ends the turn with an
/// informational message (no inference).
#[tokio::test(flavor = "current_thread")]
async fn goal_resume_with_no_goal_returns_terminal_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            let actor = Arc::new(actor);

            match actor.resume_goal().await {
                GoalResumeOutcome::Message(msg) => {
                    assert!(
                        msg.contains("No goal set"),
                        "no-goal resume must surface the no-goal message:\n{msg}"
                    );
                }
                GoalResumeOutcome::Inference { .. } => {
                    panic!("no-goal resume must not run inference")
                }
            }
        })
        .await;
}

/// Terminal case: `/goal resume` on a budget-limited goal ends the turn
/// with an informational message (no inference, no status transition).
#[tokio::test(flavor = "current_thread")]
async fn goal_resume_budget_limited_returns_terminal_message() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            assert!(actor.goal_tracker.lock().budget_limit());
            let actor = Arc::new(actor);

            match actor.resume_goal().await {
                GoalResumeOutcome::Message(msg) => {
                    assert!(
                        msg.contains("budget-limited"),
                        "budget-limited resume must surface the budget message:\n{msg}"
                    );
                }
                GoalResumeOutcome::Inference { .. } => {
                    panic!("budget-limited resume must not run inference")
                }
            }
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BudgetLimited),
                "terminal resume must not transition status"
            );
        })
        .await;
}

/// Build an actor wired to drive a real `/goal resume` turn through
/// `handle_prompt`: `update_goal` is registered (so the `goal` command
/// gate is satisfied and the slash resolves to `GoalResume`), gateway +
/// replay drainer are spawned so slash output flushes, and the
/// persistence receiver is returned so the test can observe the
/// turn's persisted user-message content. With `paused_goal`, a
/// UserPaused goal named "test objective" is seeded.
async fn make_goal_resume_turn_actor(
    paused_goal: bool,
) -> (
    std::sync::Arc<SessionActor>,
    std::sync::Arc<tokio::sync::Mutex<Vec<acp::SessionNotification>>>,
    tokio::sync::mpsc::UnboundedReceiver<PersistenceMsg>,
) {
    let (gateway_tx, gateway_rx) =
        tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let sent = spawn_gateway_notification_capture(gateway_rx);
    let (persistence_tx, persistence_rx) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
    let (mut actor, event_rx) =
        create_test_actor_ex(0, 256_000, 85, gateway_tx, persistence_tx).await;
    *actor.agent.borrow_mut() = test_agent_with_goal_tool().await;
    actor.goal_enabled = true;
    if paused_goal {
        let mut tracker = actor.goal_tracker.lock();
        tracker.create_goal(
            "test-goal".to_string(),
            "test objective".to_string(),
            None,
            0,
            "2026-01-01T00:00:00Z".to_string(),
            None,
        );
        assert!(
            tracker.pause(crate::session::goal_tracker::GoalPauseReason::User),
            "seed goal must pause"
        );
    }
    let actor = std::sync::Arc::new(actor);
    spawn_replay_event_drainer(actor.clone(), event_rx);
    (actor, sent, persistence_rx)
}

fn goal_resume_prompt_blocks() -> Vec<acp::ContentBlock> {
    vec![acp::ContentBlock::Text(acp::TextContent::new(
        "/goal resume".to_string(),
    ))]
}

/// End-to-end interception (Message arm): `/goal resume` with no goal set,
/// driven through the real `handle_prompt` entry, ends the turn promptly
/// with the informational message and runs no inference (no user-message
/// content is persisted for the turn).
#[tokio::test(flavor = "current_thread")]
async fn goal_resume_no_goal_through_handle_prompt_ends_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, sent, mut persistence_rx) = make_goal_resume_turn_actor(false).await;

            // The Message arm returns `ok_end_turn` before any sampler
            // interaction, so the turn completes well within the timeout.
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.handle_prompt(
                    "goal-resume-no-goal",
                    goal_resume_prompt_blocks(),
                    PromptMode::Agent,
                    None,
                    None,
                    None,
                    None,
                    false,
                    None,
                    None,
                    None,
                ),
            )
            .await
            .expect("no-goal resume must end the turn, not run inference");
            assert!(result.is_ok(), "turn should end cleanly: {result:?}");

            let texts: Vec<String> = sent
                .lock()
                .await
                .iter()
                .filter_map(agent_message_text_from_notification)
                .collect();
            assert!(
                texts.iter().any(|t| t.contains("No goal set")),
                "no-goal resume must surface the terminal message, got:\n{texts:?}"
            );

            // No inference ran: the turn never persisted a user-message
            // chunk (the Message arm returns before prompt-block persistence).
            let mut persisted_user_text = false;
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Acp(n)) = msg
                    && matches!(n.update, acp::SessionUpdate::UserMessageChunk(_))
                {
                    persisted_user_text = true;
                }
            }
            assert!(
                !persisted_user_text,
                "terminal resume must not persist turn user content (no inference)"
            );
        })
        .await;
}

/// End-to-end interception (Inference arm): `/goal resume` on a paused
/// goal, driven through the real `handle_prompt` entry, falls through to
/// the inference turn — the goal system-reminder becomes the turn's
/// persisted user-message content (the Message arm would have returned
/// before this), and "Goal resumed." is surfaced to the user.
#[tokio::test(flavor = "current_thread")]
async fn goal_resume_paused_through_handle_prompt_runs_inference() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (actor, sent, mut persistence_rx) = make_goal_resume_turn_actor(true).await;

            // Drive the turn in the background: the Inference arm seeds the
            // reminder and falls through to the sampler loop, which blocks on
            // the noop sampler — so we observe the persisted reminder (emitted
            // before sampling) and then abort.
            let actor_for_turn = actor.clone();
            let turn = tokio::task::spawn_local(async move {
                actor_for_turn
                    .handle_prompt(
                        "goal-resume-paused",
                        goal_resume_prompt_blocks(),
                        PromptMode::Agent,
                        None,
                        None,
                        None,
                        None,
                        false,
                        None,
                        None,
                        None,
                    )
                    .await
            });

            let mut reminder_persisted = false;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline && !reminder_persisted {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    persistence_rx.recv(),
                )
                .await
                {
                    Ok(Some(PersistenceMsg::Update(
                        crate::session::storage::SessionUpdate::Acp(n),
                    ))) => {
                        if let acp::SessionUpdate::UserMessageChunk(chunk) = &n.update
                            && let acp::ContentBlock::Text(t) = &chunk.content
                            && t.text.contains("Continue working now.")
                        {
                            reminder_persisted = true;
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
            turn.abort();

            assert!(
                reminder_persisted,
                "resumed paused goal must flow through to inference, persisting the \
                 goal system-reminder as the turn's user content"
            );

            let texts: Vec<String> = sent
                .lock()
                .await
                .iter()
                .filter_map(agent_message_text_from_notification)
                .collect();
            assert!(
                texts.iter().any(|t| t.contains("Goal resumed.")),
                "resume must surface the 'Goal resumed.' acknowledgement, got:\n{texts:?}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_clear_resets_streak() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // Simulate two failed turns to seed the continuation streak.
            actor.handle_turn_end(false).await;
            actor.handle_turn_end(false).await;
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 2);
            // Seed blocked streak too.
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);

            let actor = Arc::new(actor);
            let _ = actor
                .execute_builtin_slash_command(BuiltinAction::GoalClear)
                .await;
            // `/goal clear` must reset both streaks so they can't leak
            // into the next goal.
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
            assert_eq!(actor.goal_blocked_streak.load(Ordering::Relaxed), 0);
            assert!(
                actor.goal_tracker.lock().status().is_none(),
                "/goal clear must drop the goal tracker state"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_message_only_does_not_change_status() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_blocked_streak.store(1, Ordering::Relaxed);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: None,
                        message: Some("Running tests...".into()),
                        blocked_reason: None,
                    },
                ),
            )
            .unwrap();
            drop(tx);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "message-only update must not change status"
            );
            // blocked streak must not be touched by a message-only update.
            assert_eq!(actor.goal_blocked_streak.load(Ordering::Relaxed), 1);
        })
        .await;
}

/// Repro for the "harness dropped the response channel" bug. `update_goal`
/// is in the standard toolset and its `GoalUpdateHandle` is always registered,
/// so a model can call it in a session that is NOT a `/goal` run
/// (`goal_harness_enabled() == false`, the `create_test_actor` default — the
/// state for every plain eval/coding rollout). When the goal drainer hands such
/// an envelope to `drain_goal_updates_with_extra` (see the spawn-time drainer
/// task in `spawn_session_actor`), the function early-returns and the envelope's
/// ack oneshot is dropped without a response, so the tool surfaces the alarming
/// `harness_no_ack` ("Goal-update harness dropped the response channel before
/// producing an ack"). The drain must instead reply with a clean ack.
#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_harness_disabled_does_not_drop_ack() {
    use xai_grok_tools::implementations::grok_build::update_goal::{
        RejectReason, UpdateGoalAck, UpdateGoalInput,
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // A non-goal session: harness disabled (create_test_actor default).
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            assert!(
                !actor.goal_harness_enabled.load(Ordering::Relaxed),
                "precondition: goal harness must be disabled for this repro",
            );

            // Path 1 (production): the drainer task hands each channel envelope
            // to the drain as `extra`.
            let (extra_ack_tx, extra_ack_rx) = tokio::sync::oneshot::channel();
            let extra_env = (
                UpdateGoalInput {
                    completed: Some(true),
                    message: Some("Implemented and ran the test; PNG saved.".into()),
                    blocked_reason: None,
                },
                extra_ack_tx,
            );

            // Path 2 (test-driver / defensive): an envelope buffered on the
            // channel must also be answered, not left to hang forever on an
            // ack sender that never drops.
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            let (chan_ack_tx, chan_ack_rx) = tokio::sync::oneshot::channel();
            tx.send((
                UpdateGoalInput {
                    completed: None,
                    message: Some("progress".into()),
                    blocked_reason: None,
                },
                chan_ack_tx,
            ))
            .unwrap();
            drop(tx);

            actor
                .drain_goal_updates_with_extra(0, DrainPurpose::MidTurn, vec![extra_env])
                .await;

            // Both ack receivers must resolve to a clean HarnessDisabled
            // rejection — never a dropped channel (which the tool reports as
            // the misleading `harness_no_ack`).
            for ack_rx in [extra_ack_rx, chan_ack_rx] {
                match ack_rx.await {
                    Ok(UpdateGoalAck::Rejected { reason, .. }) => {
                        assert_eq!(reason, RejectReason::HarnessDisabled);
                    }
                    Ok(other) => panic!("expected Rejected(HarnessDisabled), got {other:?}"),
                    Err(_) => panic!(
                        "goal-update ack channel dropped without a response \
                         (harness disabled early-return)"
                    ),
                }
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_blocked_reason_takes_precedence_over_completed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // Pre-seed to 2 so the blocked_reason triggers.
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: Some(true),
                        message: None,
                        blocked_reason: Some("stuck".into()),
                    },
                ),
            )
            .unwrap();
            drop(tx);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Blocked),
                "blocked_reason must take precedence over completed"
            );
        })
        .await;
}

// ── Goal classifier integration into `drain_goal_updates` ────────
//
// These tests pin the integration's gate semantics — disabled
// fallthrough, Active-only guard, mid-turn deferral, in-flight
// re-entry guard, TurnEnd FIFO drain of `pending_classifier_completions`,
// and the idempotency-matcher edit. They are unit-level (no real
// classifier sampler invoked); the full Achieved/NotAchieved/cap
// E2E suite using `MockSpawner` lives separately.

fn make_completed_cmd()
-> xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalEnvelope {
    let input = xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
        completed: Some(true),
        message: None,
        blocked_reason: None,
    };
    let (ack_tx, _ack_rx) = tokio::sync::oneshot::channel();
    (input, ack_tx)
}

#[tokio::test(flavor = "current_thread")]
async fn drain_classifier_disabled_calls_tracker_complete_as_today() {
    // Gate-off path: with `goal_classifier_enabled = false`,
    // `update_goal(completed: true)` must transition the goal to
    // Complete immediately (identical to the pre-classifier behaviour).
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            assert!(!actor.goal_classifier_enabled);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(make_completed_cmd()).unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .classifier_runs_attempted,
                0,
                "disabled gate must not reserve a classifier attempt slot",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_classifier_enabled_status_not_active_drops_silently_and_resets_blocked_streak() {
    // Guard 1: a `completed: true` against a non-Active goal must
    // drop silently (no tracker.complete(), no verification-stage
    // fire) and still reset the blocked-streak counter so the
    // invariant holds across guard short-circuits.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // Force the verification stage on via the field
            // directly — we don't go through the resolver here,
            // the gate machinery reads only
            // `self.goal_classifier_enabled`.
            let actor = SessionActor {
                goal_classifier_enabled: true,
                ..actor
            };
            // Transition to UserPaused so Guard 1 fires.
            assert!(
                actor
                    .goal_tracker
                    .lock()
                    .pause(crate::session::goal_tracker::GoalPauseReason::User),
            );
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(make_completed_cmd()).unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::UserPaused),
                "Guard 1 must NOT transition status",
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .classifier_runs_attempted,
                0,
                "Guard 1 must NOT reserve a classifier attempt slot",
            );
            assert_eq!(
                actor.goal_blocked_streak.load(Ordering::Relaxed),
                0,
                "classifier-eligible completion must reset blocked_streak \
                     regardless of which guard fires (R2-5)",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_classifier_enabled_mid_turn_defers_to_pending() {
    // Guard 2: `DrainPurpose::MidTurn` defers verification-eligible
    // completions into `pending_classifier_completions` instead of
    // firing the verification stage.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let actor = SessionActor {
                goal_classifier_enabled: true,
                ..actor
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(make_completed_cmd()).unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "mid-turn defer must NOT change status",
            );
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                1,
                "mid-turn drain must push the completion into pending",
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .classifier_runs_attempted,
                0,
                "mid-turn defer must NOT reserve an attempt slot",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_classifier_enabled_in_flight_short_circuits_to_synthetic_not_achieved() {
    // Guard 3: a second `completed: true` that arrives while a
    // classifier is already in flight short-circuits through
    // `account_not_achieved_without_sampler` — attempt slot is
    // still reserved and the synthetic details file is written. No
    // nudge turn is queued; feedback rides the in-turn continuation
    // directive.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let actor = SessionActor {
                goal_classifier_enabled: true,
                ..actor
            };
            // Simulate an in-flight classifier owned by some other
            // path. We never clear the flag inside the guard, so
            // verify that contract too.
            actor
                .goal_classifier_in_flight
                .store(true, Ordering::SeqCst);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(make_completed_cmd()).unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            assert!(
                actor.goal_classifier_in_flight.load(Ordering::SeqCst),
                "Guard 3 must NOT clear in-flight (the in-flight \
                     owner still holds it)",
            );
            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.classifier_runs_attempted, 1,
                "synthetic NotAchieved must reserve an attempt slot",
            );
            assert_eq!(
                snap.classifier_max_runs,
                Some(crate::session::goal_classifier::GOAL_CLASSIFIER_MAX_RUNS_DEFAULT),
            );
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved),
            );
            assert!(
                snap.last_classifier_details_path.as_ref().is_some_and(|p| {
                    p == &crate::session::goal_classifier::format_details_path(
                        &snap.verifier_id,
                        snap.classifier_runs_attempted,
                    )
                }),
                "synthetic verdict must record the scratch-rooted details path",
            );
            // Root intact ⇒ the synthetic details file is actually written.
            let details = snap.last_classifier_details_path.as_deref().unwrap();
            let body = std::fs::read_to_string(details)
                .expect("synthetic details file must be written under the intact root");
            assert!(
                !body.trim().is_empty(),
                "synthetic details body must be non-empty",
            );
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::Active,
                "attempt 1 of 3 must NOT pause — only cap-reached does",
            );
            let state = actor.state.lock().await;
            assert!(
                !state
                    .pending_inputs
                    .iter()
                    .any(|i| matches!(i.origin, crate::session::PromptOrigin::GoalClassifierNudge)),
                "synthetic NotAchieved must not queue a nudge turn",
            );
        })
        .await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn drain_in_flight_synthetic_verdict_skips_details_write_under_squatted_root() {
    // Symlink-squatted root: the Guard-3 short-circuit must still
    // account the attempt + verdict but skip the details write — an
    // unguarded write would land THROUGH the link in the attacker dir.
    struct SquatGuard(std::path::PathBuf);
    impl Drop for SquatGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let actor = SessionActor {
                goal_classifier_enabled: true,
                ..actor
            };
            actor
                .goal_classifier_in_flight
                .store(true, Ordering::SeqCst);
            let vid = actor
                .goal_tracker
                .lock()
                .snapshot()
                .unwrap()
                .verifier_id
                .clone();
            let root = crate::session::goal_tracker::goal_scratch_root(&vid);
            let _ = std::fs::remove_dir_all(&root);
            let attacker = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(attacker.path(), &root).unwrap();
            let _guard = SquatGuard(root.clone());

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(make_completed_cmd()).unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.classifier_runs_attempted, 1,
                "accounting still lands under a squatted root",
            );
            assert_eq!(
                snap.last_classifier_verdict,
                Some(crate::session::goal_tracker::GoalClassifierVerdict::NotAchieved),
            );
            assert!(
                std::fs::symlink_metadata(&root)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "the squat symlink must be left in place, not replaced",
            );
            assert_eq!(
                std::fs::read_dir(attacker.path()).unwrap().count(),
                0,
                "nothing may be written through the symlink squat",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_classifier_turn_end_drains_pending_completions_first() {
    // TurnEnd drains `pending_classifier_completions` ahead of the
    // regular channel. We verify this by pre-seeding a deferred
    // completion, holding the in-flight guard so Guard 3 fires (no
    // sampler call), and asserting the pending queue empties + a
    // classifier attempt is accounted.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let actor = SessionActor {
                goal_classifier_enabled: true,
                ..actor
            };
            actor
                .goal_classifier_in_flight
                .store(true, Ordering::SeqCst);

            actor
                .pending_classifier_completions
                .lock()
                .push_back(make_completed_cmd().0);

            // Empty regular channel: nothing else to drain.
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "TurnEnd drain must consume the pending queue",
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .classifier_runs_attempted,
                1,
                "deferred completion must be processed (synthetic Guard 3 path)",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_turn_end_preserves_fifo_order_across_deferred_and_channel() {
    // Pin the deferred-then-channel FIFO contract by seeding 2
    // deferred + 2 channel items. With the in-flight guard held,
    // every completion routes through `account_not_achieved_without_sampler`
    // and increments `classifier_runs_attempted` until the cap.
    // Sequence under FIFO drain:
    //   - deferred[0] → attempt 1, nudge queued
    //   - deferred[1] → attempt 2, nudge already pending (idempotent)
    //   - channel[0]  → attempt 3 == cap → BackOff pause
    //   - channel[1]  → Guard 1 (status != Active) → drop
    // Final: classifier_runs_attempted = 3, status = BackOffPaused,
    // both queues empty.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let actor = SessionActor {
                goal_classifier_enabled: true,
                goal_classifier_max_runs: 3,
                ..actor
            };
            actor
                .goal_classifier_in_flight
                .store(true, Ordering::SeqCst);

            actor
                .pending_classifier_completions
                .lock()
                .push_back(make_completed_cmd().0);
            actor
                .pending_classifier_completions
                .lock()
                .push_back(make_completed_cmd().0);

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(make_completed_cmd()).unwrap();
            tx.send(make_completed_cmd()).unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            let snap = actor.goal_tracker.lock().snapshot().cloned().unwrap();
            assert_eq!(
                snap.classifier_runs_attempted, 3,
                "FIFO drain must process deferred-then-channel until cap",
            );
            assert_eq!(
                snap.status,
                crate::session::goal_tracker::GoalStatus::BackOffPaused,
                "cap-reached on the 3rd attempt must pause with BackOff",
            );
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                0,
                "deferred queue must be empty",
            );
            let mut rx_slot = actor.goal_update_rx.borrow_mut();
            let rx = rx_slot.as_mut().expect("rx present in test fixture");
            assert!(
                rx.try_recv().is_err(),
                "channel must be empty (all channel items consumed)",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_pending_queue_cap_overflows_drop_oldest_with_telemetry() {
    // Cap is `GOAL_CLASSIFIER_PENDING_QUEUE_CAP`. Push CAP+1 mid-turn
    // completions; on the (CAP+1)-th, the oldest entry is dropped
    // and the queue size stays at CAP.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let actor = SessionActor {
                goal_classifier_enabled: true,
                ..actor
            };
            for _ in 0..(GOAL_CLASSIFIER_PENDING_QUEUE_CAP + 1) {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                *actor.goal_update_rx.borrow_mut() = Some(rx);
                tx.send(make_completed_cmd()).unwrap();
                drop(tx);
                actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            }
            assert_eq!(
                actor.pending_classifier_completions.lock().len(),
                GOAL_CLASSIFIER_PENDING_QUEUE_CAP,
                "queue size must stay at cap on overflow",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn reserve_classifier_attempt_slot_returns_none_without_orchestration() {
    // Defense-in-depth coverage for the snapshot-disappeared bail-out
    // branch in the drain body. The path is structurally unreachable
    // in production (no `.await` between Guard 1 and slot reservation
    // under the LocalSet) but the helper must return None when no
    // orchestration exists so the bail-out fires telemetry.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_tracker.lock().clear();
            let policy = actor.resolve_goal_classifier_policy();
            assert!(actor.goal_tracker.lock().snapshot().is_none());
            assert_eq!(actor.reserve_classifier_attempt_slot(&policy), None);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_mid_turn_then_turn_end_processes_deferred_completion() {
    // Sibling positive case for the deferred-FIFO contract: a
    // mid-turn drain defers; a follow-up turn-end drain processes
    // the deferred cmd. Holds in-flight so the actual sampler is
    // not invoked.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let actor = SessionActor {
                goal_classifier_enabled: true,
                ..actor
            };
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(make_completed_cmd()).unwrap();
            drop(tx);

            actor.drain_goal_updates(0, DrainPurpose::MidTurn).await;
            assert_eq!(actor.pending_classifier_completions.lock().len(), 1);

            actor
                .goal_classifier_in_flight
                .store(true, Ordering::SeqCst);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            assert_eq!(actor.pending_classifier_completions.lock().len(), 0);
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .classifier_runs_attempted,
                1,
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn idempotency_matcher_suppresses_goal_summary_when_classifier_nudge_pending() {
    // A pending `GoalClassifierNudge` must suppress a
    // subsequent `GoalSummary` push from `maybe_queue_goal_continuation`.
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let (respond_to, _rx) = tokio::sync::oneshot::channel();
            {
                let mut state = actor.state.lock().await;
                state.pending_inputs.push_back(InputItem {
                    prompt_id: "goal-classifier-nudge-fixture".into(),
                    prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                        "<system-reminder>fixture</system-reminder>",
                    ))],
                    prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                    trace_gcs_config: None,
                    artifact_tracker: None,
                    client_identifier: None,
                    screen_mode: None,
                    verbatim: true,
                    json_schema: None,
                    origin: crate::session::PromptOrigin::GoalClassifierNudge,
                    respond_to,
                    persist_ack: None,
                    parsed_prompt_tx: None,
                    queue_meta: None,
                    send_now: false,
                });
            }

            actor.maybe_queue_goal_continuation().await;

            let state = actor.state.lock().await;
            let summary_count = state
                .pending_inputs
                .iter()
                .filter(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary))
                .count();
            assert_eq!(
                summary_count, 0,
                "pending GoalClassifierNudge must suppress GoalSummary push",
            );
            // The classifier nudge fixture must still be there.
            let nudge_count = state
                .pending_inputs
                .iter()
                .filter(|i| matches!(i.origin, crate::session::PromptOrigin::GoalClassifierNudge))
                .count();
            assert_eq!(nudge_count, 1);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn drain_goal_updates_completed_resets_blocked_streak() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: Some(true),
                        message: None,
                        blocked_reason: None,
                    },
                ),
            )
            .unwrap();
            drop(tx);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Complete),
            );
            assert_eq!(
                actor.goal_blocked_streak.load(Ordering::Relaxed),
                0,
                "completed must reset blocked_streak"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_auto_paused_event_emits_infra_reason() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.events = crate::session::events::EventTracker::new(tmp.path());
            actor.goal_tracker.lock().create_goal(
                "test-goal".to_string(),
                "test objective".to_string(),
                None,
                0,
                "2026-01-01T00:00:00Z".to_string(),
                None,
            );

            simulate_completion_with_result(&actor, sample_turn_infra_err()).await;
            drop(actor);

            let log = std::fs::read_to_string(tmp.path().join("events.jsonl"))
                .expect("events.jsonl must exist after auto_pause");
            assert!(
                log.lines().any(|line| {
                    let val: serde_json::Value = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(_) => return false,
                    };
                    val["type"] == "goal_auto_paused" && val["reason"] == "infra"
                }),
                "expected a goal_auto_paused event with reason=infra:\n{log}"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_auto_paused_event_is_emitted() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            // Route the events.jsonl into a per-test temp dir so we can
            // read it back after the trigger. The actor's events field
            // is owned, so we construct it directly here (rather than
            // through `make_test_actor_with_active_goal`).
            let tmp = tempfile::TempDir::new().expect("tempdir");
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.events = crate::session::events::EventTracker::new(tmp.path());
            actor.goal_tracker.lock().create_goal(
                "test-goal".to_string(),
                "test objective".to_string(),
                None,
                0,
                "2026-01-01T00:00:00Z".to_string(),
                None,
            );

            actor
                .auto_pause_goal_if_active(crate::session::goal_tracker::GoalPauseReason::User)
                .await;
            // Drop the actor so the EventWriter file handle flushes.
            drop(actor);

            let log = std::fs::read_to_string(tmp.path().join("events.jsonl"))
                .expect("events.jsonl must exist after auto_pause");
            assert!(
                log.lines().any(|line| {
                    let val: serde_json::Value = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(_) => return false,
                    };
                    val["type"] == "goal_auto_paused" && val["reason"] == "user"
                }),
                "expected a goal_auto_paused event with reason=user:\n{log}"
            );
        })
        .await;
}

#[test]
fn build_goal_updated_emits_new_paused_strings() {
    use crate::extensions::notification::SessionUpdate;
    use crate::session::goal_orchestrator::build_goal_updated;
    use crate::session::goal_tracker::{GoalStatus, make_base_orchestration};

    for (status, expected_str) in [
        (GoalStatus::Active, "active"),
        (GoalStatus::UserPaused, "user_paused"),
        (GoalStatus::UserPaused, "user_paused"),
        (GoalStatus::BackOffPaused, "back_off_paused"),
        (GoalStatus::NoProgressPaused, "no_progress_paused"),
        (GoalStatus::InfraPaused, "infra_paused"),
        (GoalStatus::Blocked, "blocked"),
        (GoalStatus::BudgetLimited, "budget_limited"),
        (GoalStatus::Complete, "complete"),
    ] {
        let mut o = make_base_orchestration();
        o.status = status;
        match build_goal_updated(&o, 0, 0) {
            // Bind the wire-format `String` to a distinct name so the
            // outer `status: GoalStatus` is still in scope for the
            // failure-message format string.
            SessionUpdate::GoalUpdated {
                status: wire_status,
                ..
            } => {
                assert_eq!(
                    wire_status, expected_str,
                    "GoalStatus::{status:?} must serialize to {expected_str:?}",
                );
            }
            other => panic!("expected GoalUpdated, got {other:?}"),
        }
    }
}

// ---- Unified subagent-token registry: goal_tokens_used helper ----

fn insert_record(
    actor: &SessionActor,
    subagent_id: &str,
    goal_id: Option<&str>,
    anchor: u64,
    last: u64,
) {
    actor.subagent_token_records.lock().insert(
        subagent_id.to_string(),
        SubagentTokenRecord {
            goal_id: goal_id.map(str::to_string),
            resume_anchor_cumulative: anchor,
            last_cumulative_reported: last,
            model: None,
            finished: false,
        },
    );
}

fn insert_record_with_model(
    actor: &SessionActor,
    subagent_id: &str,
    goal_id: Option<&str>,
    anchor: u64,
    last: u64,
    model: Option<&str>,
) {
    actor.subagent_token_records.lock().insert(
        subagent_id.to_string(),
        SubagentTokenRecord {
            goal_id: goal_id.map(str::to_string),
            resume_anchor_cumulative: anchor,
            last_cumulative_reported: last,
            model: model.map(str::to_string),
            finished: false,
        },
    );
}

/// `spawn_notif` carrying an explicit effective model id on the wire.
fn spawn_notif_with_model(
    subagent_id: &str,
    resumed_from: Option<&str>,
    model: Option<&str>,
) -> XaiSessionNotification {
    let mut notif = spawn_notif(subagent_id, resumed_from);
    if let XaiSessionUpdate::SubagentSpawned { model: m, .. } = &mut notif.update {
        *m = model.map(str::to_string);
    }
    notif
}

fn spawn_notif(subagent_id: &str, resumed_from: Option<&str>) -> XaiSessionNotification {
    XaiSessionNotification {
        session_id: acp::SessionId::new("test-actor"),
        update: XaiSessionUpdate::SubagentSpawned {
            subagent_id: subagent_id.into(),
            parent_session_id: "test-actor".into(),
            parent_prompt_id: None,
            child_session_id: subagent_id.into(),
            subagent_type: "general-purpose".into(),
            description: "task".into(),
            effective_context_source: resumed_from.map(|_| "resumed".into()),
            context_normalized: false,
            capability_mode: None,
            persona: None,
            role: None,
            model: None,
            resumed_from: resumed_from.map(str::to_string),
        },
        meta: None,
    }
}

fn finish_notif(subagent_id: &str, tokens_used: u64) -> XaiSessionNotification {
    XaiSessionNotification {
        session_id: acp::SessionId::new("test-actor"),
        update: XaiSessionUpdate::SubagentFinished {
            subagent_id: subagent_id.into(),
            child_session_id: subagent_id.into(),
            status: "completed".into(),
            error: None,
            tool_calls: 0,
            turns: 1,
            duration_ms: 100,
            tokens_used,
            output: None,
            will_wake: false,
        },
        meta: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_fresh_spawn_records_full_cumulative() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record(&actor, "a", Some("test-goal"), 0, 50_000);
            // baseline=0; current=10_000 → parent_delta=10k + 50k = 60k.
            assert_eq!(actor.goal_tokens_used(10_000), 60_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_sequential_subagents_sums_marginal_costs() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record(&actor, "a", Some("test-goal"), 0, 50_000);
            insert_record(&actor, "b", Some("test-goal"), 0, 80_000);
            assert_eq!(actor.goal_tokens_used(0), 130_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_resume_chain_counts_only_marginal_per_link() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // A: 0→50_000 (marginal 50k)
            insert_record(&actor, "a", Some("test-goal"), 0, 50_000);
            // A' resumed from A: anchor 50k → last 80k (marginal 30k)
            insert_record(&actor, "a1", Some("test-goal"), 50_000, 80_000);
            // A'' resumed from A': anchor 80k → last 110k (marginal 30k)
            insert_record(&actor, "a2", Some("test-goal"), 80_000, 110_000);
            // Sum of marginals = 50k + 30k + 30k = 110k.
            assert_eq!(actor.goal_tokens_used(0), 110_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_parallel_resumes_from_same_parent_count_each_marginal_uniquely() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record(&actor, "a", Some("test-goal"), 0, 50_000);
            // Two parallel resumes both anchor at 50k.
            insert_record(&actor, "a1", Some("test-goal"), 50_000, 70_000);
            insert_record(&actor, "a2", Some("test-goal"), 50_000, 90_000);
            // 50 + 20 + 40 = 110k.
            assert_eq!(actor.goal_tokens_used(0), 110_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_cross_session_resume_anchors_at_zero() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // Parent id is unknown to this process; anchor falls back to 0.
            actor
                .handle_xai_session_notification(spawn_notif("x", Some("from-prior-session")))
                .await;
            let anchor = actor
                .subagent_token_records
                .lock()
                .get("x")
                .map(|r| r.resume_anchor_cumulative);
            assert_eq!(anchor, Some(0));
            if let Some(r) = actor.subagent_token_records.lock().get_mut("x") {
                r.last_cumulative_reported = 25_000;
            }
            assert_eq!(actor.goal_tokens_used(0), 25_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_crashed_subagent_retains_last_live_value() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record(&actor, "a", Some("test-goal"), 0, 30_000);
            assert_eq!(actor.goal_tokens_used(0), 30_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_monotonic_under_out_of_order_events() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .handle_xai_session_notification(spawn_notif("a", None))
                .await;
            actor
                .handle_xai_session_notification(finish_notif("a", 50_000))
                .await;
            // Stale event with smaller cumulative arrives second.
            actor
                .handle_xai_session_notification(finish_notif("a", 40_000))
                .await;
            assert_eq!(
                actor
                    .subagent_token_records
                    .lock()
                    .get("a")
                    .unwrap()
                    .last_cumulative_reported,
                50_000,
                "monotonic guard must keep the higher value"
            );
            assert_eq!(actor.goal_tokens_used(0), 50_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn integration_subagent_spawn_resume_uses_parent_anchor() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .handle_xai_session_notification(spawn_notif("a", None))
                .await;
            actor
                .handle_xai_session_notification(finish_notif("a", 50_000))
                .await;
            actor
                .handle_xai_session_notification(spawn_notif("a1", Some("a")))
                .await;
            let rec = actor
                .subagent_token_records
                .lock()
                .get("a1")
                .map(|r| (r.resume_anchor_cumulative, r.last_cumulative_reported))
                .unwrap();
            assert_eq!(rec.0, 50_000, "child anchor = parent.last");
            assert_eq!(rec.1, 50_000, "child last seeded to anchor at spawn");
            actor
                .handle_xai_session_notification(finish_notif("a1", 80_000))
                .await;
            // Total marginal: 50k (a) + 30k (a1) = 80k.
            assert_eq!(actor.goal_tokens_used(0), 80_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn subagent_spawn_captures_effective_model_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .handle_xai_session_notification(spawn_notif_with_model(
                    "a",
                    None,
                    Some("grok-4.5"),
                ))
                .await;
            let model = actor
                .subagent_token_records
                .lock()
                .get("a")
                .and_then(|r| r.model.clone());
            assert_eq!(model.as_deref(), Some("grok-4.5"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn subagent_spawn_absent_model_captured_as_none() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .handle_xai_session_notification(spawn_notif_with_model("a", None, None))
                .await;
            let model = actor
                .subagent_token_records
                .lock()
                .get("a")
                .and_then(|r| r.model.clone());
            assert_eq!(model, None);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_by_model_breaks_down_active_goal_records() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record_with_model(&actor, "a", Some("test-goal"), 0, 100, Some("grok-4"));
            insert_record_with_model(&actor, "b", Some("test-goal"), 0, 400, Some("grok-3"));
            // No captured model → folds under the supplied current model.
            insert_record_with_model(&actor, "c", Some("test-goal"), 0, 50, None);
            // A record from another goal must be excluded.
            insert_record_with_model(&actor, "d", Some("other-goal"), 0, 999, Some("grok-3"));
            // A FINISHED record under the active goal must be excluded from the
            // LIVE active-window breakdown (the per-model analogue of the
            // finished/in-flight split in goal_tokens). If it leaked, grok-4
            // would be 800 and sort first.
            insert_record_with_model(&actor, "e", Some("test-goal"), 0, 700, Some("grok-4"));
            actor
                .subagent_token_records
                .lock()
                .get_mut("e")
                .unwrap()
                .finished = true;
            let out = actor.goal_tokens_by_model("cur-model");
            assert_eq!(
                out,
                vec![
                    ("grok-3".to_owned(), 400),
                    ("grok-4".to_owned(), 100),
                    ("cur-model".to_owned(), 50),
                ]
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_by_model_empty_without_active_goal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record_with_model(&actor, "a", Some("test-goal"), 0, 100, Some("grok-4"));
            // Drop the orchestration: with no active goal the breakdown is
            // empty regardless of any lingering records.
            actor.goal_tracker.lock().clear();
            assert!(actor.goal_tokens_by_model("cur-model").is_empty());
        })
        .await;
}

/// Budget enforcement must run on a FAILED / cancelled turn end, not only on
/// the successful continuation path. A goal whose parent + subagent spend
/// crossed the `--budget` during a failed turn must trip to `BudgetLimited`
/// immediately, instead of staying Active until a later successful turn end.
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_trips_budget_on_failed_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.goal_enabled = true;
            set_goal_harness_for_tests(&actor);
            actor.goal_tracker.lock().create_goal(
                "test-goal".to_string(),
                "test objective".to_string(),
                Some(50_000), // token budget
                0,
                "2026-01-01T00:00:00Z".to_string(),
                None,
            );
            // Subagent spend already crossed the cap (60k > 50k budget).
            insert_record(&actor, "a", Some("test-goal"), 0, 60_000);

            // Turn FAILED (turn_succeeded = false) — the path that previously
            // skipped budget enforcement entirely.
            actor.handle_turn_end(false).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::BudgetLimited),
                "a failed turn that crossed the budget must trip BudgetLimited",
            );
        })
        .await;
}

/// Control: a failed turn UNDER budget must NOT trip — the goal stays Active
/// (the back-off streak handles repeated non-completion separately).
#[tokio::test(flavor = "current_thread")]
async fn handle_turn_end_keeps_goal_active_under_budget_on_failed_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.goal_enabled = true;
            set_goal_harness_for_tests(&actor);
            actor.goal_tracker.lock().create_goal(
                "test-goal".to_string(),
                "test objective".to_string(),
                Some(50_000),
                0,
                "2026-01-01T00:00:00Z".to_string(),
                None,
            );
            insert_record(&actor, "a", Some("test-goal"), 0, 10_000); // under budget

            actor.handle_turn_end(false).await;

            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "an under-budget failed turn must not trip BudgetLimited",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn integration_subagent_spawn_captures_active_goal_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .handle_xai_session_notification(spawn_notif("a", None))
                .await;
            let goal_id = actor
                .subagent_token_records
                .lock()
                .get("a")
                .and_then(|r| r.goal_id.clone());
            assert_eq!(goal_id.as_deref(), Some("test-goal"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_saturates_on_u64_overflow() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record(&actor, "big", Some("test-goal"), 0, u64::MAX);
            assert_eq!(actor.goal_tokens_used(0), i64::MAX);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_saturates_on_fold_overflow() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let near = (i64::MAX / 2 + 100) as u64;
            insert_record(&actor, "a", Some("test-goal"), 0, near);
            insert_record(&actor, "b", Some("test-goal"), 0, near);
            assert_eq!(actor.goal_tokens_used(0), i64::MAX);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_ratchets_high_water_across_compactions() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_tracker.lock().create_goal(
                "test-goal".into(),
                "obj".into(),
                None,
                10_000,
                "2026-01-01T00:00:00Z".into(),
                None,
            );
            insert_record(&actor, "a", Some("test-goal"), 0, 0);
            // Pre-compaction: parent total = 60k → delta = 50k.
            let (peak, _) = actor.goal_tokens(60_000);
            assert_eq!(peak, 50_000);
            // Compaction shrinks parent total to 5k (< baseline);
            // parent_delta clamps to 0 but ratchet preserves 50k.
            let (after, _) = actor.goal_tokens(5_000);
            assert_eq!(
                after, 50_000,
                "high-water mark must not drop after compaction"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn prune_subagent_records_drops_records_for_active_goal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record(&actor, "a", Some("test-goal"), 0, 50_000);
            insert_record(&actor, "b", Some("other-goal"), 0, 80_000);
            actor.prune_subagent_records_for_active_goal();
            let keys: Vec<String> = actor
                .subagent_token_records
                .lock()
                .keys()
                .cloned()
                .collect();
            assert_eq!(
                keys,
                vec!["b".to_string()],
                "only active-goal record pruned"
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_above_budget_threshold_reports_via_helper() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_tracker.lock().create_goal(
                "test-goal".into(),
                "obj".into(),
                Some(100),
                0,
                "2026-01-01T00:00:00Z".into(),
                None,
            );
            insert_record(&actor, "a", Some("test-goal"), 0, 150);
            let used = actor.goal_tokens_used(0);
            let budget = actor.goal_tracker.lock().token_budget();
            assert_eq!(used, 150);
            assert_eq!(budget, Some(100));
            assert!(budget.is_some_and(|b| used >= b));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_filters_records_by_goal_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record(&actor, "a", Some("test-goal"), 0, 50_000);
            insert_record(&actor, "b", Some("other-goal"), 0, 80_000);
            assert_eq!(actor.goal_tokens_used(0), 50_000);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_excludes_subagents_with_no_goal_id() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            insert_record(&actor, "a", None, 0, 50_000);
            assert_eq!(actor.goal_tokens_used(0), 0);
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_used_returns_zero_when_no_active_goal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _g) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _p) = tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            insert_record(&actor, "a", Some("phantom"), 0, 50_000);
            assert_eq!(actor.goal_tokens_used(100_000), 0);
        })
        .await;
}

/// A compaction shrink re-anchors the spend counter, so post-compaction
/// regrowth keeps counting instead of freezing below the old high-water.
#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_keeps_counting_after_compaction_shrink() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            assert_eq!(actor.goal_tokens(60_000).0, 60_000);
            // Compaction shrinks the context total to 5k.
            assert_eq!(actor.goal_tokens(5_000).0, 60_000, "ratchet holds");
            // Post-compaction growth is new spend and must count right
            // away, not freeze until the total regrows past 60k.
            assert_eq!(actor.goal_tokens(10_000).0, 65_000);
            // Idempotent under a stable total.
            assert_eq!(actor.goal_tokens(10_000).0, 65_000);
            // Second shrink-grow cycle keeps accumulating.
            assert_eq!(actor.goal_tokens(2_000).0, 65_000, "ratchet holds again");
            assert_eq!(actor.goal_tokens(9_000).0, 72_000);
        })
        .await;
}

/// A legacy snapshot (no spend-accumulator fields) must seed the anchor
/// from `token_baseline` on the first `goal_tokens` call, not from 0.
#[tokio::test(flavor = "current_thread")]
async fn goal_tokens_legacy_snapshot_seeds_anchor_from_baseline() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            {
                let mut tracker = actor.goal_tracker.lock();
                let o = tracker.snapshot_mut().unwrap();
                o.token_baseline = 10_000;
                o.last_session_tokens_seen = None;
                o.parent_tokens_spent = 0;
            }
            assert_eq!(
                actor.goal_tokens(15_000).0,
                5_000,
                "first call after a legacy restore must anchor at token_baseline",
            );
        })
        .await;
}

/// Blocked attempts interleaved with successful turn ends must still
/// accumulate to the 3-attempt pause.
#[tokio::test(flavor = "current_thread")]
async fn blocked_streak_reaches_pause_across_successful_turns() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            let blocked = || {
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: None,
                        message: None,
                        blocked_reason: Some("cannot reach service".into()),
                    },
                )
            };
            for attempt in 1..=3u32 {
                tx.send(blocked()).unwrap();
                actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
                if attempt < 3 {
                    assert_eq!(
                        actor.goal_tracker.lock().status(),
                        Some(crate::session::goal_tracker::GoalStatus::Active),
                        "attempt {attempt}/3 must not pause yet",
                    );
                    // The blocked attempt ends its turn successfully.
                    actor.handle_turn_end(true).await;
                    assert_eq!(
                        actor.goal_blocked_streak.load(Ordering::Relaxed),
                        attempt,
                        "successful turn end must not reset the streak",
                    );
                }
            }
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Blocked),
                "third consecutive blocked attempt must pause the goal",
            );
        })
        .await;
}

fn progress_notif(subagent_id: &str, tokens_used: u64) -> XaiSessionNotification {
    XaiSessionNotification {
        session_id: acp::SessionId::new("test-actor"),
        update: XaiSessionUpdate::SubagentProgress {
            subagent_id: subagent_id.into(),
            parent_session_id: "test-actor".into(),
            child_session_id: subagent_id.into(),
            duration_ms: 2_000,
            turn_count: 3,
            tool_call_count: 7,
            tokens_used,
            context_window_tokens: 256_000,
            context_usage_pct: 12,
            tools_used: Vec::new(),
            error_count: 0,
        },
        meta: None,
    }
}

/// Progress ticks advance the goal token count + live fields mid-run and
/// emit to the gateway ONLY (never persisted to the session JSONL — they
/// recur every couple of seconds per subagent), and the eventual finish
/// must not double-count them.
#[tokio::test(flavor = "current_thread")]
async fn subagent_progress_advances_goal_tokens_live_without_double_count() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, mut persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.goal_enabled = true;
            set_goal_harness_for_tests(&actor);
            actor.goal_tracker.lock().create_goal(
                "test-goal".to_string(),
                "test objective".to_string(),
                None,
                0,
                "2026-01-01T00:00:00Z".to_string(),
                None,
            );
            actor
                .handle_xai_session_notification(spawn_notif("a", None))
                .await;
            // Count (and drain) goal_updated notifications delivered to the
            // gateway. Progress-tick emits are gateway-only now, so coalescing
            // is observed here rather than on the persistence channel.
            let count_gateway_goal_updated =
                |rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>| {
                    let mut n = 0usize;
                    while let Ok(msg) = rx.try_recv() {
                        let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
                            continue;
                        };
                        if args.request.method.as_ref() != "x.ai/session_notification" {
                            continue;
                        }
                        let Ok(v) =
                            serde_json::from_str::<serde_json::Value>(args.request.params.get())
                        else {
                            continue;
                        };
                        if v.get("update")
                            .and_then(|u| u.get("sessionUpdate"))
                            .and_then(|s| s.as_str())
                            == Some("goal_updated")
                        {
                            n += 1;
                        }
                    }
                    n
                };
            // Drop the spawn-transition emit + persisted entries so the
            // measurements below isolate the progress ticks.
            let _ = count_gateway_goal_updated(&mut gateway_rx);
            while persistence_rx.try_recv().is_ok() {}

            actor
                .handle_xai_session_notification(progress_notif("a", 30_000))
                .await;
            assert_eq!(
                actor.goal_tokens_used(0),
                30_000,
                "a progress tick must move the goal token count mid-run",
            );
            {
                let tracker = actor.goal_tracker.lock();
                let o = tracker.snapshot().unwrap();
                assert_eq!(o.live_subagent_tokens, 30_000);
                assert_eq!(o.live_turn_count, 3);
                assert_eq!(o.live_tool_call_count, 7);
                assert_eq!(o.live_context_pct, 12);
            }
            assert_eq!(
                count_gateway_goal_updated(&mut gateway_rx),
                1,
                "an advancing progress tick emits exactly one gateway GoalUpdated",
            );
            // ...and it is gateway-only: the progress tick persists nothing.
            assert!(
                persistence_rx.try_recv().is_err(),
                "an advancing progress tick must not be persisted (recurring → \
                 unbounded log growth); the durable total lives in GoalModeState",
            );

            // Coalescing: a replayed (non-advancing) tick must emit nothing.
            actor
                .handle_xai_session_notification(progress_notif("a", 30_000))
                .await;
            assert_eq!(
                count_gateway_goal_updated(&mut gateway_rx),
                0,
                "a same-value tick must not emit another GoalUpdated",
            );
            assert!(
                persistence_rx.try_recv().is_err(),
                "a coalesced tick persists nothing either",
            );

            actor
                .handle_xai_session_notification(finish_notif("a", 50_000))
                .await;
            assert_eq!(
                actor.goal_tokens_used(0),
                50_000,
                "finish must converge on the cumulative total, not add the live ticks again",
            );
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .live_subagent_tokens,
                0,
                "finish clears the live fields",
            );
            // The SubagentProgress notification itself must never persist.
            while let Ok(msg) = persistence_rx.try_recv() {
                if let PersistenceMsg::Update(crate::session::storage::SessionUpdate::Xai(n)) = msg
                {
                    assert!(
                        !matches!(n.update, XaiSessionUpdate::SubagentProgress { .. }),
                        "SubagentProgress must never be persisted",
                    );
                }
            }
        })
        .await;
}

/// Child context compaction makes a progress tick report a LOWER cumulative
/// than the record's high-water. `live_subagent_tokens` (the live display
/// value) must track the ratcheted high-water — consistent with the budget-
/// enforcement total in `goal_tokens().0` — not the raw post-compaction tick,
/// or the live display would fall below the enforced spend.
#[tokio::test(flavor = "current_thread")]
async fn subagent_progress_live_tokens_monotonic_across_child_compaction() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, _gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let mut actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            actor.goal_enabled = true;
            set_goal_harness_for_tests(&actor);
            actor.goal_tracker.lock().create_goal(
                "test-goal".to_string(),
                "test objective".to_string(),
                None,
                0,
                "2026-01-01T00:00:00Z".to_string(),
                None,
            );
            actor
                .handle_xai_session_notification(spawn_notif("a", None))
                .await;

            // Climb to a 100k high-water.
            actor
                .handle_xai_session_notification(progress_notif("a", 100_000))
                .await;
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .live_subagent_tokens,
                100_000,
            );

            // Child compaction: the next tick reports a LOWER cumulative.
            actor
                .handle_xai_session_notification(progress_notif("a", 60_000))
                .await;
            assert_eq!(
                actor
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .unwrap()
                    .live_subagent_tokens,
                100_000,
                "live_subagent_tokens must hold the ratcheted high-water, not the \
                 post-compaction tick",
            );
            // The budget-enforcement total holds at the high-water too, so the
            // live display can never sit below it.
            assert_eq!(actor.goal_tokens_used(0), 100_000);
        })
        .await;
}

/// A fresh `/goal` must zero both streaks so a previous goal's blocked
/// streak can't pause the new goal's first blocked attempt.
#[tokio::test(flavor = "current_thread")]
async fn setup_goal_resets_streaks_from_previous_goal() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor.goal_blocked_streak.store(2, Ordering::Relaxed);
            actor.goal_continuation_streak.store(2, Ordering::Relaxed);
            let _ = actor.setup_goal("fresh objective", None).await;
            assert_eq!(actor.goal_blocked_streak.load(Ordering::Relaxed), 0);
            assert_eq!(actor.goal_continuation_streak.load(Ordering::Relaxed), 0);
            // The new goal's first blocked attempt must report 1/3, not pause.
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *actor.goal_update_rx.borrow_mut() = Some(rx);
            tx.send(
                xai_grok_tools::implementations::grok_build::update_goal::envelope_for_test(
                    xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalInput {
                        completed: None,
                        message: None,
                        blocked_reason: Some("blk".into()),
                    },
                ),
            )
            .unwrap();
            drop(tx);
            actor.drain_goal_updates(0, DrainPurpose::TurnEnd).await;
            assert_eq!(actor.goal_blocked_streak.load(Ordering::Relaxed), 1);
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "attempt 1/3 on the fresh goal must not pause",
            );
        })
        .await;
}

/// Hostile / out-of-order tick handling: a decreasing tick can't lower
/// the ratchet, an unknown subagent id is dropped, and a tick after
/// `SubagentFinished` is ignored (sealed record).
#[tokio::test(flavor = "current_thread")]
async fn subagent_progress_edge_ticks_cannot_corrupt_token_records() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .handle_xai_session_notification(spawn_notif("a", None))
                .await;
            actor
                .handle_xai_session_notification(progress_notif("a", 30_000))
                .await;
            // Decreasing tick: ratchet holds.
            actor
                .handle_xai_session_notification(progress_notif("a", 10_000))
                .await;
            assert_eq!(actor.goal_tokens_used(0), 30_000, "ratchet must hold");
            // Unknown subagent id: dropped, nothing recorded.
            actor
                .handle_xai_session_notification(progress_notif("ghost", 99_000))
                .await;
            assert_eq!(actor.goal_tokens_used(0), 30_000);
            assert!(!actor.subagent_token_records.lock().contains_key("ghost"));
            // Record tagged to a DIFFERENT goal: tick ignored entirely.
            insert_record(&actor, "foreign", Some("other-goal"), 0, 1_000);
            actor
                .handle_xai_session_notification(progress_notif("foreign", 70_000))
                .await;
            assert_eq!(
                actor
                    .subagent_token_records
                    .lock()
                    .get("foreign")
                    .unwrap()
                    .last_cumulative_reported,
                1_000,
                "a foreign-goal record must not be ratcheted by a tick",
            );
            assert_eq!(actor.goal_tokens_used(0), 30_000);
            // Tick after finish: sealed record ignores it.
            actor
                .handle_xai_session_notification(finish_notif("a", 35_000))
                .await;
            actor
                .handle_xai_session_notification(progress_notif("a", 80_000))
                .await;
            assert_eq!(
                actor.goal_tokens_used(0),
                35_000,
                "a post-finish tick must not move the sealed record",
            );
        })
        .await;
}

/// Spend reaching the budget (inclusive boundary) ends the goal as
/// `BudgetLimited` with a `BudgetExceeded` history entry and no continuation.
#[tokio::test(flavor = "current_thread")]
async fn goal_budget_reached_stops_goal_at_turn_end() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            // Exact boundary: tokens_used == budget.
            actor
                .goal_tracker
                .lock()
                .snapshot_mut()
                .unwrap()
                .token_budget = Some(50_000);
            insert_record(&actor, "a", Some("test-goal"), 0, 50_000);
            actor.handle_turn_end(true).await;
            {
                let tracker = actor.goal_tracker.lock();
                let o = tracker.snapshot().unwrap();
                assert_eq!(
                    o.status,
                    crate::session::goal_tracker::GoalStatus::BudgetLimited,
                    "spend == budget must end the goal at turn end (inclusive)",
                );
                assert!(
                    o.history.iter().any(|e| matches!(
                        e.event,
                        crate::session::goal_tracker::GoalEvent::BudgetExceeded
                    )),
                    "a BudgetExceeded history entry must be recorded",
                );
            }
            let state = actor.state.lock().await;
            assert!(
                !state
                    .pending_inputs
                    .iter()
                    .any(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary)),
                "no continuation may be queued for a budget-ended goal",
            );
        })
        .await;
}

/// Control: an under-budget goal continues normally.
#[tokio::test(flavor = "current_thread")]
async fn goal_under_budget_continues_normally() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let actor = make_test_actor_with_active_goal().await;
            actor
                .goal_tracker
                .lock()
                .snapshot_mut()
                .unwrap()
                .token_budget = Some(1_000_000);
            insert_record(&actor, "a", Some("test-goal"), 0, 50_000);
            actor.handle_turn_end(true).await;
            assert_eq!(
                actor.goal_tracker.lock().status(),
                Some(crate::session::goal_tracker::GoalStatus::Active),
                "an under-budget goal must stay Active",
            );
            let state = actor.state.lock().await;
            assert!(
                state
                    .pending_inputs
                    .iter()
                    .any(|i| matches!(i.origin, crate::session::PromptOrigin::GoalSummary)),
                "continuation must be queued for an under-budget goal",
            );
        })
        .await;
}
