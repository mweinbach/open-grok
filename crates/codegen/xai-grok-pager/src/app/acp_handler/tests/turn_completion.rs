#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    #[test]
    fn driver_prompt_complete_without_prompt_id_arms_reconcile_not_finish() {
        // Driver still owns the turn via PromptResponse — prompt_complete must
        // NOT finish immediately. Missing wire promptId (legacy shells) arms
        // lost-PR reconcile on current_prompt_id so grace teardown
        // can run if the RPC never arrives; turn state stays TurnRunning.
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-local".into());
            agent.turn_started_at = Some(std::time::Instant::now());
            assert!(!agent.attached_as_viewer);
        }

        let affected = handle_ext_notification(&prompt_complete_ext("sess-drive"), &mut app);
        assert!(
            affected,
            "arming reconcile must schedule ticks for background-tab recovery"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            matches!(agent.session.state, AgentState::TurnRunning),
            "driver's running turn must NOT be finished by prompt_complete"
        );
        assert_eq!(
            agent.session.current_prompt_id.as_deref(),
            Some("pid-local"),
            "driver's current_prompt_id must be untouched at arm time"
        );
        assert!(agent.turn_started_at.is_some());
        assert_eq!(
            agent
                .pending_turn_end_reconcile
                .as_ref()
                .map(|p| p.prompt_id.as_str()),
            Some("pid-local"),
        );
    }

    #[test]
    fn driver_prompt_complete_with_matching_prompt_id_arms_reconcile() {
        // Lost-response recovery: when the driver
        // receives the turn-end broadcast for the exact turn it is awaiting,
        // it must ARM the deferred reconcile — without finishing the turn
        // immediately (the RPC response normally lands ms later and carries
        // richer context; finishing here would double-finish every turn).
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-stuck".into());
            agent.session.cancel_turn(&mut agent.scrollback); // CancelTurn → TurnCancelling
            assert!(!agent.attached_as_viewer);
        }

        let affected = handle_ext_notification(
            &prompt_complete_ext_with_prompt_id("sess-drive", "pid-stuck", "cancelled"),
            &mut app,
        );
        assert!(
            affected,
            "arming must report a state change — the event loop only calls \
             schedule_tick on changed ACP batches, and the reconcile sweep \
             runs on the animation tick (a dormant background tab would \
             otherwise never get swept)"
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.session.state.is_cancelling(),
            "turn state must be untouched at arm time (RPC may still arrive)"
        );
        let pending = agent
            .pending_turn_end_reconcile
            .as_ref()
            .expect("reconcile must be armed for the driver's awaited turn");
        assert_eq!(pending.prompt_id, "pid-stuck");
        assert_eq!(pending.stop_reason.as_deref(), Some("cancelled"));
    }

    #[test]
    fn driver_prompt_complete_with_mismatched_prompt_id_does_not_arm() {
        // A broadcast for some OTHER prompt (stale, or a queued prompt that
        // resolved server-side) must not arm a reconcile against the turn
        // this client is actually driving.
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-current".into());
        }

        let _ = handle_ext_notification(
            &prompt_complete_ext_with_prompt_id("sess-drive", "pid-other", "end_turn"),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.pending_turn_end_reconcile.is_none());
        assert!(matches!(agent.session.state, AgentState::TurnRunning));
    }

    #[test]
    fn driver_prompt_complete_without_prompt_id_arms_on_current() {
        // Older shells omit `promptId`; arm reconcile on current_prompt_id when
        // not mid-tool (see arm_driver_turn_end_reconcile). Does not finish.
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-current".into());
        }

        let _ = handle_ext_notification(&prompt_complete_ext("sess-drive"), &mut app);

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .pending_turn_end_reconcile
                .as_ref()
                .map(|p| p.prompt_id.as_str()),
            Some("pid-current"),
        );
        assert!(matches!(agent.session.state, AgentState::TurnRunning));
    }

    #[test]
    fn driver_prompt_complete_pushes_no_marker() {
        // The driver emits its own marker via PromptResponse; prompt_complete
        // must not double-push one for it (or push any block at all).
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-local".into());
            agent.turn_started_at = Some(std::time::Instant::now());
            assert!(!agent.attached_as_viewer);
        }

        let len_before = app.agents.get(&AgentId(0)).unwrap().scrollback.len();
        let _ = handle_ext_notification(&prompt_complete_ext("sess-drive"), &mut app);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "the driver must not get any new block from prompt_complete"
        );
    }

    #[test]
    fn live_turn_completed_finalizes_viewer_turn_and_duplicate_is_noop() {
        // The durable `TurnCompleted` is the viewer's non-interactive exit from
        // TurnRunning on the replayed rail (parallel to the fire-and-forget
        // `prompt_complete`). A viewer adopting the driver's live turn must drop
        // back to Idle with a marker when it arrives.
        let mut app = make_app_with_agent("sess-view");
        app.agents.get_mut(&AgentId(0)).unwrap().attached_as_viewer = true;
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-view", "chunk", "pid-driver", false),
            &mut app,
        );
        assert!(matches!(
            app.agents.get(&AgentId(0)).unwrap().session.state,
            AgentState::TurnRunning
        ));

        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-view", "pid-driver", "end_turn", false),
            &mut app,
        );
        assert!(affected, "finalizing the active viewer turn should redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.session.state.is_idle(),
            "a live TurnCompleted must drop a viewer back to Idle"
        );
        assert!(agent.session.current_prompt_id.is_none());
        assert!(matches!(
            last_session_event(&agent.scrollback),
            Some(SessionEvent::TurnCompleted { .. })
        ));

        // A duplicate/stale terminal for the now-finished turn is a no-op.
        let len_before = app.agents.get(&AgentId(0)).unwrap().scrollback.len();
        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-view", "pid-driver", "end_turn", false),
            &mut app,
        );
        assert!(!affected, "a duplicate TurnCompleted must be a no-op");
        assert_eq!(
            app.agents.get(&AgentId(0)).unwrap().scrollback.len(),
            len_before,
            "a duplicate TurnCompleted must not push a second marker"
        );
    }

    #[test]
    fn live_turn_completed_driver_arms_reconcile() {
        // For the driver the `PromptResponse` RPC owns the lifecycle, so a live
        // TurnCompleted for the turn it is driving arms the lost-RPC reconcile
        // WITHOUT finishing the turn (mirrors the `prompt_complete` driver path).
        let mut app = make_app_with_agent("sess-drive");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-local".into());
            assert!(!agent.attached_as_viewer);
        }

        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-drive", "pid-local", "cancelled", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            matches!(agent.session.state, AgentState::TurnRunning),
            "the driver's turn must NOT be finished by a live TurnCompleted"
        );
        let pending = agent
            .pending_turn_end_reconcile
            .as_ref()
            .expect("the driver's awaited turn must arm a reconcile");
        assert_eq!(pending.prompt_id, "pid-local");
        assert_eq!(pending.stop_reason.as_deref(), Some("cancelled"));
    }

    #[test]
    fn wake_delta_records_wake_turn_start() {
        // The wake turn's deltas are the marker's only timing source: the
        // stamp is pid-scoped so a later real turn's `turnStartMs` cannot
        // masquerade as the wake turn's start.
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "task-completed-bg1", 5_000),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let (pid, _) = agent
            .wake_turn_start
            .as_ref()
            .expect("a wake delta must record its turn start");
        assert_eq!(pid, "task-completed-bg1");

        // A real (user) turn's delta must not overwrite the record.
        let _ = handle(
            make_viewer_chunk_with_turn_start("sess-wake", "pid-user", 1_000),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.wake_turn_start.as_ref().map(|(p, _)| p.as_str()),
            Some("task-completed-bg1"),
            "only wake-turn deltas feed the wake start record"
        );
    }

    #[test]
    fn wake_turn_completed_pushes_end_marker_with_counts() {
        let mut app = make_app_with_agent("sess-wake");
        seed_two_bg_tasks_and_announce(&mut app, "sess-wake");
        {
            // Window closed (e.g. the last marker was workless) — the wake
            // marker must REOPEN it via the shared single assignment.
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.end_work_announced = false;
            agent.wake_turn_start = Some(("task-completed-bg1".into(), 1_700_000_000_000));
        }

        let affected = handle_ext_notification(
            &xai_wake_turn_completed_notif(
                "sess-wake",
                "task-completed-bg1",
                Some(1_700_000_000_000 + 5_000),
            ),
            &mut app,
        );
        assert!(affected, "a wake marker on the active agent redraws");

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.session.state.is_idle(),
            "a wake turn is never adopted — the pager stays idle around it"
        );
        let block = last_marker_block(&agent.scrollback);
        assert_eq!(
            block.marker_text(),
            "Worked for 5.0s. 2 commands still running.",
            "elapsed spans the delta-borne start to the terminal's shell clock"
        );
        assert!(!block.parked);
        assert_eq!(
            block.prompt_id.as_deref(),
            Some("task-completed-bg1"),
            "the marker carries the wake pid for hook attribution"
        );
        assert!(
            agent.end_work_announced,
            "a counted wake marker reopens the between-turns status window"
        );
        assert!(
            agent.wake_turn_start.is_none(),
            "the tracked start is consumed by its marker"
        );
    }

    #[test]
    fn wake_marker_without_tracked_start_omits_elapsed() {
        // Old shells stamp no `turnStartMs` on deltas — the marker renders
        // without a duration rather than lying with "0.0s".
        let mut app = make_app_with_agent("sess-wake");
        let _ = handle_ext_notification(
            &make_task_backgrounded_notif("sess-wake", "tc-1", "task-1", "sleep 98"),
            &mut app,
        );

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_marker_block(&agent.scrollback).marker_text(),
            "Turn completed. 1 command still running."
        );
    }

    #[test]
    fn zero_count_wake_marker_is_plain_and_closes_window() {
        let mut app = make_app_with_agent("sess-wake");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.end_work_announced = true;
            agent.wake_turn_start = Some(("task-completed-bg1".into(), 1_700_000_000_000));
        }

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif(
                "sess-wake",
                "task-completed-bg1",
                Some(1_700_000_000_000 + 2_000),
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let block = last_marker_block(&agent.scrollback);
        assert_eq!(block.marker_text(), "Worked for 2.0s.");
        assert!(
            block.end_work.is_none(),
            "zero counts → legacy plain marker"
        );
        assert!(
            !agent.end_work_announced,
            "a workless wake marker proves nothing is running — window closed"
        );
    }

    #[test]
    fn wake_turn_completed_in_replay_only_records_pid() {
        // The replay arm is untouched: a wake pid seen during a load's replay
        // records adoption state and pushes nothing (markers are client-local
        // and never replayed).
        let mut app = make_app_with_agent("sess-wake");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;
        let len_before = app.agents[&AgentId(0)].scrollback.len();

        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "end_turn", true),
            &mut app,
        );

        assert!(!affected);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent
                .replayed_terminal_prompts
                .contains("task-completed-bg1"),
            "the replay arm must keep recording wake pids"
        );
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "no marker during replay"
        );
    }

    #[test]
    fn scheduler_fired_turn_completed_keeps_adopted_path() {
        // `/loop` turns are synthetic but CLIENT-driven with a real finalize
        // path — they must not take the wake-marker shortcut. Idle driver +
        // scheduler pid → the shared finalize ignores it, no marker.
        let mut app = make_app_with_agent("sess-cron");
        let len_before = app.agents[&AgentId(0)].scrollback.len();

        let affected = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-cron", "scheduler-fired-abc", Some(1_000)),
            &mut app,
        );

        assert!(!affected);
        assert_eq!(
            app.agents[&AgentId(0)].scrollback.len(),
            len_before,
            "a scheduler-fired terminal must not push a wake marker"
        );
    }

    #[test]
    fn failed_wake_turn_keeps_markerless_shape() {
        // "Worked for" would lie about an errored/cancelled wake turn, and
        // the cancel/failure UX is driver-side context this signal lacks —
        // those stop reasons keep today's markerless shape. The status-line
        // re-emit on this leg self-gates silent here (closed window, no work).
        let mut app = make_app_with_agent("sess-wake");
        let len_before = app.agents[&AgentId(0)].scrollback.len();

        for stop_reason in ["error", "cancelled", "rate_limit"] {
            let _ = handle_ext_notification(
                &xai_turn_completed_notif("sess-wake", "task-completed-bg1", stop_reason, false),
                &mut app,
            );
        }

        assert_eq!(
            app.agents[&AgentId(0)].scrollback.len(),
            len_before,
            "non-completion wake terminals push nothing"
        );
    }

    #[test]
    fn dead_wake_reemits_skipped_work_status_line() {
        // The shell's `will_wake` promise made the chip skip its status line;
        // the wake then died markerless (cancelled) — nothing else marks the
        // moment, so the terminal re-emits the work-only line. The window
        // stays open: the line announces the same still-running work.
        let mut app = make_app_with_agent("sess-wake");
        seed_two_bg_tasks_and_announce(&mut app, "sess-wake");

        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-wake", "task-completed-bg1", "cancelled", false),
            &mut app,
        );

        assert!(affected, "the re-emitted status line must redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            work_status_lines(&agent.scrollback),
            vec!["2 commands still running.".to_string()],
            "the dead wake's terminal re-emits the skipped work-only line"
        );
        assert!(
            agent.end_work_announced,
            "a status line is not a marker — the window stays open"
        );
    }

    #[test]
    fn wake_terminal_during_local_turn_pushes_nothing() {
        // Wire interleave: wake turn W streams (pager idle), the user sends a
        // prompt locally (TurnRunning), then FIFO delivers W's terminal
        // before the new turn's deltas. A foreign "Worked for" (or status
        // line) under the fresh prompt would misattribute — the local turn's
        // own marker carries the counts when it ends.
        let mut app = make_app_with_agent("sess-wake");
        seed_two_bg_tasks_and_announce(&mut app, "sess-wake");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.wake_turn_start = Some(("task-completed-bg1".into(), 1_000));
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-local".into());
        }
        let len_before = app.agents[&AgentId(0)].scrollback.len();

        let affected = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", Some(6_000)),
            &mut app,
        );

        assert!(!affected);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "no marker and no status line may land under the fresh local prompt"
        );
        assert!(
            agent.session.state.is_turn_running(),
            "the local turn is untouched"
        );
        assert!(agent.end_work_announced, "the skip leaves the window as-is");
        assert!(
            agent.wake_turn_start.is_some(),
            "the pid-scoped elapsed slot stays; it cannot misfire on other turns"
        );
    }

    #[test]
    fn wake_marker_leaves_real_turn_stash_pending() {
        // Stop-hook stash semantics belong to real turns: a stash stamped
        // with a REAL turn's pid must survive a wake marker untouched — no
        // fold, no standalone flush — and wait for its own marker rail.
        use crate::scrollback::blocks::tool::{HookRunEntry, HookRunStatus};
        let mut app = make_app_with_agent("sess-wake");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.pending_stop_hooks = Some(crate::app::agent_view::PendingStopHooks {
                prompt_id: Some("pid-real".into()),
                groups: vec![(
                    "stop".to_string(),
                    vec![HookRunEntry {
                        name: "global/notify".into(),
                        status: HookRunStatus::Success {
                            elapsed: std::time::Duration::from_millis(12),
                        },
                        output: None,
                    }],
                )],
            });
        }

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake", "task-completed-bg1", None),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_marker_stop_hook_groups(&agent.scrollback),
            Some(0),
            "a real turn's stash must not attach to the wake marker"
        );
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 0);
        assert!(
            agent.pending_stop_hooks.is_some(),
            "the stash stays pending for its own turn's marker"
        );
    }

    #[test]
    fn live_stop_hooks_during_turn_stash_instead_of_standalone_block() {
        // Driver order: the batch lands while the turn is still running
        // (before the PromptResponse) and is held for the turn marker.
        let mut app = make_app_with_agent("sess-stop");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-1".into());
        }
        let len_before = app.agents[&AgentId(0)].scrollback.len();

        let _ = handle_ext_notification(
            &xai_hook_execution_notif("sess-stop", "stop", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent.scrollback.len(),
            len_before,
            "live stop hooks mid-turn must not push a standalone block"
        );
        let pending = agent
            .pending_stop_hooks
            .as_ref()
            .expect("stop hooks must be stashed for the marker");
        assert_eq!(pending.prompt_id.as_deref(), Some("pid-1"));
        assert_eq!(pending.groups.len(), 1);
        assert_eq!(pending.groups[0].0, "stop");
    }

    #[test]
    fn replayed_stop_hooks_render_as_standalone_block() {
        // Replay keeps the legacy standalone block: turn markers are
        // client-local and not reconstructed from the persisted stream,
        // so there is nothing to merge into on resume.
        let mut app = make_app_with_agent("sess-replay");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .session
            .loading_replay = true;

        let _ = handle_ext_notification(
            &xai_hook_execution_notif("sess-replay", "stop", true),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            count_lifecycle_blocks(&agent.scrollback),
            1,
            "replayed stop hooks keep the standalone lifecycle block"
        );
        assert!(agent.pending_stop_hooks.is_none());
    }

    #[test]
    fn foreign_turn_stop_hooks_never_stash_under_running_turn() {
        // A delayed batch from an ended turn (pid-old) lands while a later
        // turn (pid-new) runs — a queued-prompt drain. It renders
        // standalone, not on pid-new's marker.
        let mut app = make_app_with_agent("sess-foreign");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-new".into());
        }

        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt("sess-foreign", "stop", Some("pid-old"), false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.pending_stop_hooks.is_none(),
            "a foreign turn's batch must not stash under the running turn"
        );
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 1);

        // The running turn's own batch (matching wire pid) still stashes.
        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt("sess-foreign", "stop", Some("pid-new"), false),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let pending = agent
            .pending_stop_hooks
            .as_ref()
            .expect("own-turn batch stashes");
        assert_eq!(pending.prompt_id.as_deref(), Some("pid-new"));
        assert_eq!(
            count_lifecycle_blocks(&agent.scrollback),
            1,
            "own-turn batch must not add a standalone block"
        );
    }

    #[test]
    fn foreign_stop_hooks_refused_at_idle_tail_marker() {
        // The delayed foreign batch lands after the later turn also ended:
        // no turn is running, so only the marker's pid stamp keeps the batch
        // off it. A fresh event name proves the refusal is the pid check,
        // not the same-name dedup.
        let mut app = make_app_with_agent("sess-idle-foreign");
        app.agents.get_mut(&AgentId(0)).unwrap().attached_as_viewer = true;
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-idle-foreign", "chunk", "pid-new", false),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-idle-foreign", "pid-new", "end_turn", false),
            &mut app,
        );

        // The marker's own batch (matching pid) merges…
        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt(
                "sess-idle-foreign",
                "stop",
                Some("pid-new"),
                false,
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_marker_stop_hook_groups(&agent.scrollback),
            Some(1),
            "the marker's own batch (matching pid) merges"
        );
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 0);

        // …a foreign-pid batch is refused even with a fresh event name.
        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt(
                "sess-idle-foreign",
                "stop_failure",
                Some("pid-old"),
                false,
            ),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_marker_stop_hook_groups(&agent.scrollback),
            Some(1),
            "a foreign-pid batch must not merge into another turn's marker"
        );
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 1);
    }

    #[test]
    fn stamped_stop_hooks_merge_past_interleaved_tail_block() {
        // Viewer/race order with a block (compaction, recap, …) landing
        // between the marker and the batch: an exact pid match still merges
        // into the marker instead of degrading to the standalone block.
        let mut app = make_app_with_agent("sess-interleaved");
        app.agents.get_mut(&AgentId(0)).unwrap().attached_as_viewer = true;
        let _ = handle(
            make_agent_chunk_message_with_prompt("sess-interleaved", "chunk", "pid-new", false),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-interleaved", "pid-new", "end_turn", false),
            &mut app,
        );
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .scrollback
            .push_block(RenderBlock::session_event(
                crate::scrollback::blocks::SessionEvent::CompactionCompleted {
                    tokens_before: Some(100),
                    tokens_after: 10,
                    elapsed_ms: Some(5),
                },
            ));

        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt(
                "sess-interleaved",
                "stop",
                Some("pid-new"),
                false,
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_marker_stop_hook_groups(&agent.scrollback),
            Some(1),
            "the stamped batch merges into its marker across the interleaved block"
        );
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 0);
    }

    #[test]
    fn same_name_stash_repeat_goes_standalone() {
        // A second batch with an already-stashed event name (a session-end
        // `stop` landing mid-turn) renders standalone instead of duplicating
        // the marker's `stop` group.
        let mut app = make_app_with_agent("sess-stash-dup");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-1".into());
        }
        let _ = handle_ext_notification(
            &xai_hook_execution_notif("sess-stash-dup", "stop", false),
            &mut app,
        );
        let _ = handle_ext_notification(
            &xai_hook_execution_notif("sess-stash-dup", "stop", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let pending = agent
            .pending_stop_hooks
            .as_ref()
            .expect("first batch stays");
        assert_eq!(pending.groups.len(), 1, "no duplicate group in the stash");
        assert_eq!(
            count_lifecycle_blocks(&agent.scrollback),
            1,
            "the repeat renders as the standalone block"
        );
    }

    #[test]
    fn stash_key_prefers_wire_prompt_id() {
        // A stamped batch stashed while the client-side pid is missing keys
        // the stash by the wire pid, so the marker-push stale check can still
        // tell whether the stash belongs to the ending turn.
        let mut app = make_app_with_agent("sess-wire-key");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = None;
        }
        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt("sess-wire-key", "stop", Some("pid-a"), false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let pending = agent.pending_stop_hooks.as_ref().expect("batch stashes");
        assert_eq!(pending.prompt_id.as_deref(), Some("pid-a"));
    }

    #[test]
    fn session_end_stop_hooks_without_live_turn_stay_standalone() {
        // The session-end Stop batch fires with no turn running and no fresh
        // marker in the tail — legacy standalone block.
        let mut app = make_app_with_agent("sess-end");
        let _ = handle_ext_notification(
            &xai_hook_execution_notif("sess-end", "stop", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 1);
        assert!(agent.pending_stop_hooks.is_none());
    }

    #[test]
    fn non_stop_lifecycle_hooks_keep_standalone_block() {
        // session_start & co are untouched by the stop-hook inlining.
        let mut app = make_app_with_agent("sess-ls");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-1".into());
        }
        let _ = handle_ext_notification(
            &xai_hook_execution_notif("sess-ls", "session_start", false),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 1);
        assert!(agent.pending_stop_hooks.is_none());
    }

    #[test]
    fn between_turns_completion_reemits_work_status_until_zero() {
        let mut app = make_app_with_agent("sess-reemit");
        seed_two_bg_tasks_and_announce(&mut app, "sess-reemit");
        assert!(app.agents[&AgentId(0)].session.state.is_idle());

        // First completion: chip, then a fresh status line with the rest.
        let _ = handle_ext_notification(
            &make_task_completed_notif("sess-reemit", "task-1", "sleep 98", Some(0)),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            work_status_lines(&agent.scrollback),
            vec!["1 command still running.".to_string()],
            "the completion re-emits the remaining-work status line"
        );
        let tail_is_status = matches!(
            agent.scrollback.get(agent.scrollback.len() - 1).map(|e| &e.block),
            Some(RenderBlock::System(b)) if b.text.contains("still running")
        );
        assert!(tail_is_status, "the status line lands AFTER the chip");

        // Last completion: chip only — zero left closes the story.
        let _ = handle_ext_notification(
            &make_task_completed_notif("sess-reemit", "task-2", "sleep 99", Some(0)),
            &mut app,
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            work_status_lines(&agent.scrollback).len(),
            1,
            "zero remaining work must not add a status line"
        );
    }

    #[test]
    fn mid_turn_completion_pushes_chip_only() {
        let mut app = make_app_with_agent("sess-midturn");
        seed_two_bg_tasks_and_announce(&mut app, "sess-midturn");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("p1".into());
        }

        let _ = handle_ext_notification(
            &make_task_completed_notif("sess-midturn", "task-1", "sleep 98", Some(0)),
            &mut app,
        );
        assert!(
            work_status_lines(&app.agents[&AgentId(0)].scrollback).is_empty(),
            "a completion inside an active turn pushes its chip only"
        );
    }

    #[test]
    fn unannounced_completion_pushes_chip_only() {
        // No turn-end marker announced work (e.g. a fresh attach): the
        // between-turns window is closed and completions stay chip-only.
        let mut app = make_app_with_agent("sess-unannounced");
        seed_two_bg_tasks_and_announce(&mut app, "sess-unannounced");
        app.agents.get_mut(&AgentId(0)).unwrap().end_work_announced = false;

        let _ = handle_ext_notification(
            &make_task_completed_notif("sess-unannounced", "task-1", "sleep 98", Some(0)),
            &mut app,
        );
        assert!(
            work_status_lines(&app.agents[&AgentId(0)].scrollback).is_empty(),
            "an unannounced window must not spawn status lines"
        );
    }

    #[test]
    fn subagent_finished_between_turns_reemits_work_status() {
        let mut app = make_app_with_parent_and_child("sess-sub-reemit", "child-1");
        let _ = handle_ext_notification(
            &make_task_backgrounded_notif("sess-sub-reemit", "tc-1", "task-1", "sleep 98"),
            &mut app,
        );
        app.agents.get_mut(&AgentId(0)).unwrap().end_work_announced = true;

        let _ = handle(
            make_ext_session_notification("sess-sub-reemit", test_subagent_finished("child-1")),
            &mut app,
        );

        assert_eq!(
            work_status_lines(&app.agents[&AgentId(0)].scrollback),
            vec!["1 command still running.".to_string()],
            "a finished subagent re-emits the remaining-work status line"
        );
    }

    #[test]
    fn will_wake_completion_skips_work_status_line() {
        // The shell stamped `will_wake`: a wake response follows the chip and
        // its end marker carries the fresh counts — the after-chip work-only
        // line would duplicate them.
        let mut app = make_app_with_agent("sess-wake-skip");
        seed_two_bg_tasks_and_announce(&mut app, "sess-wake-skip");

        let _ = handle_ext_notification(
            &task_completed_notif("sess-wake-skip", "task-1", "sleep 98", Some(0), None, true),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            work_status_lines(&agent.scrollback).is_empty(),
            "a wake-bound completion pushes its chip only"
        );
        assert!(
            agent.end_work_announced,
            "the skip leaves the window to the wake marker"
        );
    }

    #[test]
    fn legacy_task_completed_without_will_wake_field_emits_status() {
        // Old shells don't stamp the field — missing reads as `false`, so the
        // skew degrades to a transient duplicate line at worst, never a lost
        // status.
        let mut app = make_app_with_agent("sess-legacy");
        seed_two_bg_tasks_and_announce(&mut app, "sess-legacy");

        let notif = make_task_completed_notif("sess-legacy", "task-1", "sleep 98", Some(0));
        let mut v: serde_json::Value = serde_json::from_str(notif.params.get()).unwrap();
        v["update"]
            .as_object_mut()
            .unwrap()
            .remove("will_wake")
            .expect("the typed builder stamps the field");
        let legacy = acp::ExtNotification::new(
            "x.ai/task_completed",
            std::sync::Arc::from(serde_json::value::to_raw_value(&v).unwrap()),
        );
        let _ = handle_ext_notification(&legacy, &mut app);

        assert_eq!(
            work_status_lines(&app.agents[&AgentId(0)].scrollback),
            vec!["1 command still running.".to_string()],
            "a stamp-less completion keeps the no-wake fallback line"
        );
    }

    #[test]
    fn will_wake_subagent_finished_skips_work_status_line() {
        let mut app = make_app_with_parent_and_child("sess-sub-skip", "child-1");
        let _ = handle_ext_notification(
            &make_task_backgrounded_notif("sess-sub-skip", "tc-1", "task-1", "sleep 98"),
            &mut app,
        );
        app.agents.get_mut(&AgentId(0)).unwrap().end_work_announced = true;

        let _ = handle(
            make_ext_session_notification(
                "sess-sub-skip",
                test_subagent_finished_with_wake("child-1", true),
            ),
            &mut app,
        );

        assert!(
            work_status_lines(&app.agents[&AgentId(0)].scrollback).is_empty(),
            "a wake-bound subagent completion pushes no status line"
        );
    }

    #[test]
    fn child_session_completions_never_spam_root_status() {
        // A background subagent's own task traffic routes to the CHILD view;
        // it never counts toward the root marker, so its completions must
        // not push root status lines — no matter how many land in the open
        // between-turns window.
        let mut app = make_app_with_parent_and_child("sess-child-quiet", "child-1");
        let _ = handle_ext_notification(
            &make_task_backgrounded_notif("child-1", "tc-c1", "task-c1", "sleep 97"),
            &mut app,
        );
        let _ = handle_ext_notification(
            &make_task_backgrounded_notif("child-1", "tc-c2", "task-c2", "sleep 98"),
            &mut app,
        );
        app.agents.get_mut(&AgentId(0)).unwrap().end_work_announced = true;
        assert!(app.agents[&AgentId(0)].session.state.is_idle());

        let _ = handle_ext_notification(
            &make_task_completed_notif("child-1", "task-c1", "sleep 97", Some(0)),
            &mut app,
        );
        let _ = handle_ext_notification(
            &make_task_completed_notif("child-1", "task-c2", "sleep 98", Some(0)),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            work_status_lines(&agent.scrollback).is_empty(),
            "child-session completions must not spawn root status lines"
        );
        let child = agent.subagent_views.get("child-1").unwrap();
        assert!(
            work_status_lines(&child.scrollback).is_empty(),
            "and none in the child view either (chips only)"
        );

        // Nested analogue: a SubagentFinished carrying a CHILD session id
        // routes to the child handler, which has no status site at all.
        let _ = handle(
            make_ext_session_notification("child-1", test_subagent_finished("grandchild-1")),
            &mut app,
        );
        assert!(
            work_status_lines(&app.agents[&AgentId(0)].scrollback).is_empty(),
            "nested subagent traffic must not spawn root status lines"
        );
    }

    /// The core reattach-finalization: a `TurnCompleted` seen during a load's
    /// replay window records its prompt id (the running turn isn't adopted yet),
    /// and the post-replay `SessionLoaded` adoption then SKIPS that same id — so
    /// a viewer that re-attached after the turn ended does not re-strand on
    /// "Waiting…".
    #[test]
    fn replayed_turn_completed_blocks_session_loaded_adoption() {
        use crate::app::dispatch::dispatch;
        use crate::app::actions::{Action, TaskResult};

        let mut app = make_app_with_agent("sess-1");
        let id = AgentId(0);
        app.agents.get_mut(&id).unwrap().session.loading_replay = true;

        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-1", "p-run", "end_turn", true),
            &mut app,
        );
        assert!(
            !affected,
            "a replayed terminal records adoption state, not a redraw"
        );
        assert!(
            app.agents[&id].replayed_terminal_prompts.contains("p-run"),
            "a replayed TurnCompleted must record its prompt id"
        );

        dispatch(
            Action::TaskComplete(TaskResult::SessionLoaded {
                agent_id: id,
                session_id: acp::SessionId::new("sess-1"),
                models: None,
                code_restored: false,
                restore_summary: None,
                restore_degree: None,
                running_prompt_id: Some("p-run".to_string()),
            }),
            &mut app,
        );

        let agent = &app.agents[&id];
        assert!(
            agent.session.current_prompt_id.is_none(),
            "a terminal-in-replay prompt must NOT be adopted on load"
        );
        assert!(
            agent.session.state.is_idle(),
            "adopting an already-ended turn would re-strand the viewer on Waiting…"
        );
    }

    /// BUG 1 pin: a BACKGROUND-tab driver (`is_active == false`) that arms the
    /// lost-RPC reconcile from a live `TurnCompleted` must STILL report a change.
    /// Otherwise `event_loop` skips `schedule_tick` and `reconcile_overdue_turn_ends`
    /// never fires, stranding the turn on "Waiting…". The reconcile-arm return must
    /// NOT be gated on `is_active`. (This test fails if the live arm routes the arm
    /// through `changed && is_active`.)
    #[test]
    fn background_driver_live_turn_completed_arms_reconcile_and_reports_change() {
        let mut app = make_app_with_agent("sess-bg");
        let id = AgentId(0);
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-bg".into());
            assert!(!agent.attached_as_viewer);
        }
        // Make the driver a background tab: the active view is elsewhere.
        app.active_view = ActiveView::Welcome;
        assert!(!is_matched_agent_active(&app, id));

        let affected = handle_ext_notification(
            &xai_turn_completed_notif("sess-bg", "pid-bg", "cancelled", false),
            &mut app,
        );
        assert!(
            affected,
            "a background driver's reconcile-arm must report a change so the tick is scheduled"
        );
        let agent = app.agents.get(&id).unwrap();
        assert!(
            agent.pending_turn_end_reconcile.is_some(),
            "the lost-RPC reconcile must be armed"
        );
        assert!(
            matches!(agent.session.state, AgentState::TurnRunning),
            "arming must NOT finish the driver's turn"
        );
    }

    /// The replay set never leaks across loads: a second load enters a fresh
    /// replay window via `begin_replay_window`, which resets ALL coupled fields
    /// (the terminal set AND `unexpected_replay_drops`) together.
    #[test]
    fn second_load_does_not_inherit_first_loads_replay_window_state() {
        let mut app = make_app_with_agent("sess-1");
        let id = AgentId(0);
        // First load replay records a terminal; also seed a prior stray-replay
        // drop count so the reset of every coupled field is observable.
        {
            let agent = app.agents.get_mut(&id).unwrap();
            agent.session.loading_replay = true;
            agent.unexpected_replay_drops = 3;
        }
        let _ = handle_ext_notification(
            &xai_turn_completed_notif("sess-1", "p-first", "end_turn", true),
            &mut app,
        );
        assert!(
            app.agents[&id]
                .replayed_terminal_prompts
                .contains("p-first")
        );

        // A second load (reconnect) enters a fresh replay window.
        app.agents.get_mut(&id).unwrap().begin_session_reload(1);
        let agent = &app.agents[&id];
        assert!(
            agent.replayed_terminal_prompts.is_empty(),
            "the second load must not inherit the first load's terminal set"
        );
        assert_eq!(
            agent.unexpected_replay_drops, 0,
            "begin_replay_window must reset every replay-coupled field together"
        );
        assert!(agent.session.loading_replay);
    }

    #[test]
    fn wake_stop_hooks_during_local_turn_stash_under_wake_pid() {
        // A wake batch while a local turn runs keys to its OWN wake pid, never
        // the local turn — else a late wake stop would fold onto an unrelated
        // turn's marker.
        let mut app = make_app_with_agent("sess-wake-park");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-main".into());
        }

        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt(
                "sess-wake-park",
                "stop",
                Some("task-completed-bg1"),
                false,
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 0);
        let pending = agent
            .pending_stop_hooks
            .as_ref()
            .expect("wake batch stashes for its own end marker");
        assert_eq!(
            pending.prompt_id.as_deref(),
            Some("task-completed-bg1"),
            "keyed to the wake pid, not the running local turn"
        );
        assert_eq!(pending.groups.len(), 1);
        assert_eq!(pending.groups[0].0, "stop");
    }

    #[test]
    fn wake_stop_hooks_idle_stash_for_wake_marker() {
        // Idle wake turn (non-adopted) whose hook beats its own TurnCompleted →
        // stashes under the wake pid for `push_wake_end_marker`, not standalone.
        let mut app = make_app_with_agent("sess-wake-idle");

        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt(
                "sess-wake-idle",
                "stop",
                Some("notifications-019f-abc"),
                false,
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            count_lifecycle_blocks(&agent.scrollback),
            0,
            "an idle wake stop batch must stash, not flush a standalone block"
        );
        let pending = agent
            .pending_stop_hooks
            .as_ref()
            .expect("wake batch stashes for its own end marker");
        assert_eq!(
            pending.prompt_id.as_deref(),
            Some("notifications-019f-abc"),
            "keyed to the wake pid so push_wake_end_marker folds it"
        );
        assert_eq!(pending.groups.len(), 1);
        assert_eq!(pending.groups[0].0, "stop");
    }

    #[test]
    fn wake_stop_hooks_marker_first_attach() {
        // Idle wake turn whose end marker lands before its hook → the hook
        // attaches to the marker, not stash (guards the marker-arm-before-stash
        // routing order).
        let mut app = make_app_with_agent("sess-wake-marker1st");

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake-marker1st", "task-completed-bg1", None),
            &mut app,
        );
        assert_eq!(
            last_marker_stop_hook_groups(&app.agents[&AgentId(0)].scrollback),
            Some(0),
            "wake marker starts without hooks"
        );

        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt(
                "sess-wake-marker1st",
                "stop",
                Some("task-completed-bg1"),
                false,
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_marker_stop_hook_groups(&agent.scrollback),
            Some(1),
            "the wake stop hook must merge into the wake marker already on screen"
        );
        assert_eq!(
            count_lifecycle_blocks(&agent.scrollback),
            0,
            "no standalone block"
        );
        assert!(
            agent.pending_stop_hooks.is_none(),
            "nothing left stashed once it attached"
        );
    }

    #[test]
    fn late_wake_stop_attaches_to_wake_marker_not_running_local_turn() {
        // A wake turn finished (its marker is on screen); a new local turn is
        // running when the wake's delayed stop hook lands. It attaches to the
        // wake marker, never folding onto the unrelated local turn.
        let mut app = make_app_with_agent("sess-late-wake");

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-late-wake", "task-completed-bg1", None),
            &mut app,
        );
        assert_eq!(
            last_marker_stop_hook_groups(&app.agents[&AgentId(0)].scrollback),
            Some(0)
        );

        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.start_turn(&mut agent.scrollback);
            agent.session.current_prompt_id = Some("pid-L".into());
        }

        let _ = handle_ext_notification(
            &xai_hook_execution_notif_for_prompt(
                "sess-late-wake",
                "stop",
                Some("task-completed-bg1"),
                false,
            ),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_marker_stop_hook_groups(&agent.scrollback),
            Some(1),
            "the late wake stop attaches to the wake marker"
        );
        assert!(
            agent.pending_stop_hooks.is_none(),
            "it must not stash onto the running local turn L"
        );
        assert_eq!(count_lifecycle_blocks(&agent.scrollback), 0);
    }

    #[test]
    fn wake_repeat_stop_after_marker_renders_standalone() {
        // A duplicate same-name wake stop, after its marker already folded the
        // first, renders standalone immediately — not stashed for a marker that
        // will never come (which would defer it to a stale flush).
        let mut app = make_app_with_agent("sess-wake-dup");

        let _ = handle_ext_notification(
            &xai_wake_turn_completed_notif("sess-wake-dup", "task-completed-bg1", None),
            &mut app,
        );
        for _ in 0..2 {
            let _ = handle_ext_notification(
                &xai_hook_execution_notif_for_prompt(
                    "sess-wake-dup",
                    "stop",
                    Some("task-completed-bg1"),
                    false,
                ),
                &mut app,
            );
        }

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_marker_stop_hook_groups(&agent.scrollback),
            Some(1),
            "the first stop folded onto the wake marker"
        );
        assert_eq!(
            count_lifecycle_blocks(&agent.scrollback),
            1,
            "the repeat renders standalone, not deferred"
        );
        assert!(agent.pending_stop_hooks.is_none());
    }

