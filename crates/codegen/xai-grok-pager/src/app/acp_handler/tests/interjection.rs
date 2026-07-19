#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    /// Regression: a shared-queue interjection renders only via the broadcast,
    /// and the shell emits the queue-emptying `x.ai/queue/changed` right after
    /// it — which used to fire the withheld parked marker BELOW the just-
    /// rendered user message ("Worked for …" under the follow-up, flipped
    /// transcript order). The broadcast must consume the marker slot instead.
    #[test]
    fn interjection_broadcast_mid_park_forgoes_parked_marker() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_task_output_wait};

        let mut app = make_app_with_agent("sess-park");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            simulate_task_output_wait(agent, "bg-1");
            assert!(agent.is_parked_on_sendable_wait());
        }

        assert!(handle_ext_notification(
            &interjection_broadcast("sess-park", "queued follow-up"),
            &mut app,
        ));

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            agent.parked_wait_marker_for,
            Some(crate::app::agent_view::ParkedMarkerSlot::Forgone(
                "p1".into()
            )),
            "broadcast render must consume the parked-marker slot as Forgone"
        );
        // The queue-changed following the broadcast must not fire it late.
        agent.maybe_push_parked_marker();
        assert_eq!(
            count_parked(agent),
            0,
            "no late 'Worked for …' marker under the interjection"
        );
    }


    /// Regression: a Forgone slot (interjection continued
    /// the parked turn, no marker on screen) must silence later marker pushes
    /// — a full "Worked for …" line under the interjected message would
    /// recreate the flipped transcript.
    #[test]
    fn forgone_slot_suppresses_later_marker_pushes() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_task_output_wait};

        let mut app = make_app_with_agent("sess-park");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            insert_running_task(agent, "t10", "sleep 10");
            insert_running_task(agent, "t15", "sleep 15");
            simulate_task_output_wait(agent, "t15");
            // The parked drain interjected a queued row before the marker
            // became eligible: slot consumed WITHOUT a marker.
            agent.suppress_parked_marker_on_interject();
            assert!(agent.renders_parked(), "forgone slot keeps parked chrome");
            assert_eq!(count_parked(agent), 0, "no marker on screen");
        }

        // A task completing in the still-parked window must stay silent.
        handle_ext_notification(
            &make_task_completed_notif("sess-park", "t10", "sleep 10", Some(0)),
            &mut app,
        );
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            count_parked(agent),
            0,
            "no 'Worked for …' tick under the interjection"
        );
    }

    /// "sleep 10, 15, 20 in the background": completions within one park
    /// episode push chips only — the marker never re-pushes. (Elapsed
    /// renders as "0.0s": `turn_started_at` is unset in this fixture.)
    #[test]
    fn parked_completions_push_chips_without_marker_repush() {
        use crate::app::agent_view::test_fixtures::simulate_task_output_wait;

        let mut app = make_app_with_agent("sess-park");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            insert_running_task(agent, "t10", "sleep 10");
            insert_running_task(agent, "t15", "sleep 15");
            insert_running_task(agent, "t20", "sleep 20");
            simulate_task_output_wait(agent, "t20");
            agent.maybe_push_parked_marker();
            assert!(agent.renders_parked());
        }

        // Each completion lands as a chip; no marker re-push, no "N commands
        // still running." lines.
        handle_ext_notification(
            &make_task_completed_notif("sess-park", "t10", "sleep 10", Some(0)),
            &mut app,
        );
        // Duplicate completion for the same task: not a Running→Done edge.
        handle_ext_notification(
            &make_task_completed_notif("sess-park", "t10", "sleep 10", Some(0)),
            &mut app,
        );
        handle_ext_notification(
            &make_task_completed_notif("sess-park", "t15", "sleep 15", Some(0)),
            &mut app,
        );
        handle_ext_notification(
            &make_task_completed_notif("sess-park", "t20", "sleep 20", Some(0)),
            &mut app,
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
            "one plain marker per park episode — completions never re-push"
        );
        assert!(
            work_status_lines(&agent.scrollback).is_empty(),
            "no work-only status lines in the transcript"
        );
    }

    /// Parity with the bg-command completion rail: a park withheld at park
    /// time (held queue) gets re-evaluated by a subagent completion once the
    /// blocker cleared, so the boundary marker isn't deferred to whenever the
    /// next unrelated notification happens to arrive.
    #[test]
    fn subagent_finish_reevaluates_withheld_parked_marker() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_wait_all};

        let mut app = make_app_with_agent("sess-park");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            for child_id in ["child-1", "child-2"] {
                agent
                    .subagent_sessions
                    .insert(child_id.into(), make_subagent_info(child_id));
            }
            simulate_wait_all(agent);
            // Held queue at park time: the marker is withheld.
            agent.session.enqueue_prompt("queued follow-up".into());
            agent.maybe_push_parked_marker();
            assert_eq!(count_parked(agent), 0, "held queue withholds the marker");
            // The queue drains; nothing has re-evaluated the marker yet.
            agent.session.pending_prompts.clear();
        }

        handle(
            make_ext_session_notification("sess-park", test_subagent_finished("child-1")),
            &mut app,
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            count_parked(agent),
            1,
            "the completion re-evaluates the withheld park"
        );
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
        );
    }

    #[test]
    fn consecutive_subagent_finishes_leave_single_parked_marker() {
        let mut app = make_app_with_agent("sess-park");
        let marker_id = {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            park_on_subagents(agent, &["child-1", "child-2", "child-3"])
        };

        for child in ["child-1", "child-1", "child-2", "child-3"] {
            handle(
                make_ext_session_notification("sess-park", test_subagent_finished(child)),
                &mut app,
            );
        }
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
            "subagent finishes never re-push or mutate the park marker"
        );
        assert_eq!(parked_marker_ids(agent), vec![marker_id]);
    }

    /// A re-park after new parent output (text / thought / tool) is a new
    /// park episode: the wait-state update that creates the second wait
    /// pushes a fresh marker (epoch mismatch), while completions within one
    /// episode never re-push.
    #[test]
    fn parent_text_thought_and_tool_output_start_new_park_episodes() {
        use crate::acp::meta::NotificationMeta;
        use crate::app::agent_view::test_fixtures::simulate_task_output_wait_call;

        crate::appearance::cache::set_show_thinking_blocks(true);
        for output_kind in ["text", "thought", "tool"] {
            let mut app = make_app_with_agent("sess-park");
            let first_marker_id = {
                let agent = app.agents.get_mut(&AgentId(0)).unwrap();
                agent.session.state = AgentState::TurnRunning;
                agent.session.current_prompt_id = Some("p1".into());
                for child_id in ["child-1", "child-2", "child-3"] {
                    agent
                        .subagent_sessions
                        .insert(child_id.into(), make_subagent_info(child_id));
                }
                if output_kind == "tool" {
                    assert!(agent.session.tracker.handle_update(
                        acp::SessionUpdate::ToolCall(
                            acp::ToolCall::new(
                                acp::ToolCallId::new(std::sync::Arc::from("parent-tool")),
                                "read_file",
                            )
                                .kind(acp::ToolKind::Read)
                                .status(acp::ToolCallStatus::InProgress)
                                .content(vec![])
                                .locations(vec![]),
                        ),
                        &NotificationMeta::default(),
                        &mut agent.scrollback,
                    ));
                }
                simulate_task_output_wait_call(agent, "wait-1", "not-ours", 30_000);
                agent.maybe_push_parked_marker();
                parked_marker_ids(agent)[0]
            };

            handle(
                make_ext_session_notification("sess-park", test_subagent_finished("child-1")),
                &mut app,
            );
            {
                let agent = app.agents.get_mut(&AgentId(0)).unwrap();
                // Same episode: a repeated push attempt (e.g. another wait
                // update restating the same wait) is deduped by epoch.
                agent.maybe_push_parked_marker();
                assert_eq!(parked_marker_ids(agent).len(), 1);

                let output = match output_kind {
                    "text" => acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new("parent text")),
                    )),
                    "thought" => acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new("parent thought")),
                    )),
                    "tool" => acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                        acp::ToolCallId::new(std::sync::Arc::from("parent-tool")),
                        acp::ToolCallUpdateFields::new()
                            .status(Some(acp::ToolCallStatus::Completed)),
                    )),
                    _ => unreachable!(),
                };
                assert!(agent.session.tracker.handle_update(
                    output,
                    &NotificationMeta::default(),
                    &mut agent.scrollback,
                ));
                simulate_task_output_wait_call(agent, "wait-2", "not-ours", 30_000);
                // The wait-state notification path re-evaluates the marker on
                // every wait update (`maybe_push_parked_marker` from the ACP
                // handler); mirror it for the fixture-driven second wait.
                agent.maybe_push_parked_marker();
            }

            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            assert_eq!(
                parked_marker_messages(agent),
                vec!["Worked for 0.0s".to_string(), "Worked for 0.0s".to_string()],
                "{output_kind} output must start a new park episode",
            );
            let marker_ids = parked_marker_ids(agent);
            assert_eq!(marker_ids.len(), 2);
            assert_eq!(marker_ids[0], first_marker_id);
            assert_ne!(marker_ids[0], marker_ids[1]);
        }
    }

    #[test]
    fn interjection_suppresses_later_marker_push() {
        let mut app = make_app_with_agent("sess-park");
        let marker_id = {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            park_on_subagents(agent, &["child-1", "child-2", "child-3"])
        };
        handle(
            make_ext_session_notification("sess-park", test_subagent_finished("child-1")),
            &mut app,
        );
        assert!(handle_ext_notification(
            &interjection_broadcast("sess-park", "continue differently"),
            &mut app,
        ));
        handle(
            make_ext_session_notification("sess-park", test_subagent_finished("child-2")),
            &mut app,
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
        );
        assert_eq!(parked_marker_ids(agent), vec![marker_id]);
    }

    #[test]
    fn replayed_subagent_finish_does_not_touch_marker() {
        let mut app = make_app_with_agent("sess-park");
        let marker_id = {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            let marker_id = park_on_subagents(agent, &["child-1", "child-2"]);
            agent.session.loading_replay = true;
            marker_id
        };

        handle(
            make_ext_session_notification("sess-park", test_subagent_finished("child-1")),
            &mut app,
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
        );
        assert_eq!(parked_marker_ids(agent), vec![marker_id]);
    }

    #[test]
    fn imminent_subagent_wait_keeps_single_marker() {
        use crate::app::agent_view::test_fixtures::simulate_task_output_wait;

        let mut app = make_app_with_agent("sess-park");
        let marker_id = {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            for child_id in ["child-1", "child-2"] {
                agent
                    .subagent_sessions
                    .insert(child_id.into(), make_subagent_info(child_id));
            }
            simulate_task_output_wait(agent, "child-1");
            agent.maybe_push_parked_marker();
            parked_marker_ids(agent)[0]
        };

        handle(
            make_ext_session_notification("sess-park", test_subagent_finished("child-1")),
            &mut app,
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
        );
        assert_eq!(parked_marker_ids(agent), vec![marker_id]);
    }

    /// Synthetic completions from cold-load reconciliation (`session_restart`
    /// signal) finalize quietly — no countdown line, mirroring the suppressed
    /// "Task failed" block: nothing happened in THIS session.
    #[test]
    fn stale_on_load_completion_pushes_no_countdown() {
        use crate::app::agent_view::test_fixtures::simulate_task_output_wait;

        let mut app = make_app_with_agent("sess-park");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            insert_running_task(agent, "t10", "sleep 10");
            insert_running_task(agent, "t15", "sleep 15");
            simulate_task_output_wait(agent, "t15");
            agent.maybe_push_parked_marker();
            assert!(agent.renders_parked());
        }
        handle_ext_notification(
            &make_task_completed_notif_with_signal(
                "sess-park",
                "t10",
                "sleep 10",
                None,
                Some("session_restart"),
            ),
            &mut app,
        );
        // Only the initial parked marker — no countdown re-push.
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(parked_marker_messages(agent).len(), 1);
    }

    /// Task completions with no parked look (running turn chrome is up, or
    /// the turn already ended) must not emit countdown lines — the Tasks
    /// pane and completion blocks already narrate those states.
    #[test]
    fn task_completion_without_parked_look_pushes_no_countdown() {
        let mut app = make_app_with_agent("sess-live");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            insert_running_task(agent, "t10", "sleep 10");
            insert_running_task(agent, "t15", "sleep 15");
            // No wait, no parked marker: chrome is the live turn.
        }
        handle_ext_notification(
            &make_task_completed_notif("sess-live", "t10", "sleep 10", Some(0)),
            &mut app,
        );
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert!(parked_marker_messages(agent).is_empty());
    }

    // -- imminent waits do not park (awaited work already finished) ----------

    /// Waiting on a task that already completed: no marker, slot stays free.
    #[test]
    fn wait_on_already_completed_task_pushes_no_parked_marker() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_task_output_wait};

        let mut app = make_app_with_agent("sess-park");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.current_prompt_id = Some("p1".into());
        insert_running_task(agent, "t10", "sleep 10");
        agent.session.bg_tasks.get_mut("t10").unwrap().status = BgTaskStatus::Done;

        simulate_task_output_wait(agent, "t10");
        agent.maybe_push_parked_marker();

        assert_eq!(count_parked(agent), 0, "imminent wait must not park");
        assert!(
            agent.parked_wait_marker_for.is_none(),
            "slot must stay free for a later genuine park"
        );
        assert!(!agent.renders_parked());
    }

    /// A skipped wait leaves the slot free: a later wait on running work in
    /// the same turn still parks.
    #[test]
    fn later_genuine_wait_still_parks_after_imminent_wait_skip() {
        use crate::app::agent_view::test_fixtures::{
            complete_task_output_wait_call, count_parked, simulate_task_output_wait_call,
        };

        let mut app = make_app_with_agent("sess-park");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.current_prompt_id = Some("p1".into());
        insert_running_task(agent, "done", "sleep 1");
        agent.session.bg_tasks.get_mut("done").unwrap().status = BgTaskStatus::Done;

        simulate_task_output_wait_call(agent, "wait-1", "done", 30_000);
        agent.maybe_push_parked_marker();
        assert_eq!(count_parked(agent), 0);

        complete_task_output_wait_call(agent, "wait-1");
        insert_running_task(agent, "live", "sleep 99");
        simulate_task_output_wait_call(agent, "wait-2", "live", 30_000);
        agent.maybe_push_parked_marker();

        assert_eq!(count_parked(agent), 1, "genuine park still renders");
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
        );
    }

    /// `Failed` is terminal for imminence, not just `Done`.
    #[test]
    fn wait_on_failed_task_pushes_no_parked_marker() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_task_output_wait};

        let mut app = make_app_with_agent("sess-park");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.current_prompt_id = Some("p1".into());
        insert_running_task(agent, "t10", "sleep 10");
        agent.session.bg_tasks.get_mut("t10").unwrap().status = BgTaskStatus::Failed;

        simulate_task_output_wait(agent, "t10");
        agent.maybe_push_parked_marker();

        assert_eq!(count_parked(agent), 0, "failed task wait must not park");
        assert!(agent.parked_wait_marker_for.is_none());
    }

    /// Finished-subagent waits do not park — resolved by subagent id, then by
    /// child session id.
    #[test]
    fn wait_on_finished_subagent_pushes_no_parked_marker() {
        use crate::app::agent_view::test_fixtures::{
            complete_task_output_wait_call, count_parked, simulate_task_output_wait_call,
        };

        let mut app = make_app_with_agent("sess-park");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.current_prompt_id = Some("p1".into());
        let mut info = make_subagent_info("child-1");
        info.finished = true;
        agent.subagent_sessions.insert("child-1".into(), info);

        simulate_task_output_wait_call(agent, "wait-1", "sa-child-1", 30_000);
        agent.maybe_push_parked_marker();
        assert_eq!(count_parked(agent), 0, "finished subagent wait must not park");
        assert!(agent.parked_wait_marker_for.is_none());

        complete_task_output_wait_call(agent, "wait-1");
        simulate_task_output_wait_call(agent, "wait-2", "child-1", 30_000);
        agent.maybe_push_parked_marker();
        assert_eq!(count_parked(agent), 0, "child-session-id wait must not park");
        assert!(agent.parked_wait_marker_for.is_none());
    }

    /// One unresolvable id among terminal ones keeps the park.
    #[test]
    fn wait_including_unknown_id_still_parks() {
        use crate::acp::meta::NotificationMeta;
        use crate::app::agent_view::test_fixtures::count_parked;

        let mut app = make_app_with_agent("sess-park");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.current_prompt_id = Some("p1".into());
        insert_running_task(agent, "done", "sleep 1");
        agent.session.bg_tasks.get_mut("done").unwrap().status = BgTaskStatus::Done;

        let meta = NotificationMeta::default();
        agent.session.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(
                    acp::ToolCallId::new(std::sync::Arc::from("wait-1")),
                    "get_command_or_subagent_output",
                )
                .kind(acp::ToolKind::Other)
                .status(acp::ToolCallStatus::Pending)
                .content(vec![])
                .locations(vec![]),
            ),
            &meta,
            &mut agent.scrollback,
        );
        agent.session.handle_update(
            acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                acp::ToolCallId::new(std::sync::Arc::from("wait-1")),
                acp::ToolCallUpdateFields::new().raw_input(Some(serde_json::json!({
                    "task_ids": ["done", "not-ours"],
                    "timeout_ms": 30_000,
                }))),
            )),
            &meta,
            &mut agent.scrollback,
        );
        agent.maybe_push_parked_marker();

        assert_eq!(count_parked(agent), 1, "unresolvable id keeps the park");
    }

    #[test]
    fn wait_all_with_zero_work_pushes_no_parked_marker() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_wait_all};

        let mut app = make_app_with_agent("sess-park");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.current_prompt_id = Some("p1".into());

        simulate_wait_all(agent);
        agent.maybe_push_parked_marker();

        assert_eq!(count_parked(agent), 0, "zero-work wait-all must not park");
        assert!(agent.parked_wait_marker_for.is_none());
    }

    #[test]
    fn wait_all_with_running_work_still_parks() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_wait_all};

        let mut app = make_app_with_agent("sess-park");
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.session.state = AgentState::TurnRunning;
        agent.session.current_prompt_id = Some("p1".into());
        insert_running_task(agent, "t10", "sleep 10");

        simulate_wait_all(agent);
        agent.maybe_push_parked_marker();

        assert_eq!(count_parked(agent), 1, "wait-all on live work parks");
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
        );
    }

    /// `SubagentSpawned` arriving after the skipped zero-work wait
    /// re-evaluates and restores the park.
    #[test]
    fn subagent_spawn_after_zero_work_wait_all_restores_park() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_wait_all};

        let mut app = make_app_with_agent("sess-park");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            simulate_wait_all(agent);
            agent.maybe_push_parked_marker();
            assert_eq!(count_parked(agent), 0, "zero-work wait-all skipped");
        }

        handle(
            make_ext_session_notification(
                "sess-park",
                test_subagent_spawned("sess-park", "child-1"),
            ),
            &mut app,
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(count_parked(agent), 1, "spawn re-evaluates the skipped park");
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
        );
    }

    /// `x.ai/task_backgrounded` arriving after the skipped zero-work wait
    /// re-evaluates and restores the park.
    #[test]
    fn task_backgrounded_after_zero_work_wait_all_restores_park() {
        use crate::app::agent_view::test_fixtures::{count_parked, simulate_wait_all};

        let mut app = make_app_with_agent("sess-park");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.session.state = AgentState::TurnRunning;
            agent.session.current_prompt_id = Some("p1".into());
            simulate_wait_all(agent);
            agent.maybe_push_parked_marker();
            assert_eq!(count_parked(agent), 0, "zero-work wait-all skipped");
        }

        handle_ext_notification(
            &make_task_backgrounded_notif("sess-park", "tc-late", "t-late", "sleep 99"),
            &mut app,
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(
            count_parked(agent),
            1,
            "task registration re-evaluates the skipped park"
        );
        assert_eq!(
            parked_marker_messages(agent),
            vec!["Worked for 0.0s".to_string()],
        );
    }

    #[test]
    fn interjection_notification_pushes_block_to_matching_session() {
        // Multi-client fix: an interjection typed in one pane is broadcast by
        // the shell as x.ai/session/interjection; EVERY attached pane (incl.
        // the originator, which no longer pushes a local block) renders it.
        let mut app = make_app_with_agent("sess-view");
        let affected =
            handle_ext_notification(&interjection_ext("sess-view", "also add tests"), &mut app);
        assert!(affected, "rendering into the active agent should redraw");

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_interjection_text(&agent.scrollback).as_deref(),
            Some("also add tests"),
            "the interjection block must be pushed from the broadcast"
        );
    }

    #[test]
    fn interjection_notification_for_unknown_session_is_ignored() {
        let mut app = make_app_with_agent("sess-view");
        let affected = handle_ext_notification(&interjection_ext("sess-other", "stray"), &mut app);
        assert!(!affected, "an unmatched session must be a no-op");

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            last_interjection_text(&agent.scrollback).is_none(),
            "no interjection block must be pushed for an unknown session"
        );
    }

    #[test]
    fn interjection_notification_renders_for_a_viewer() {
        // A viewer (attached_as_viewer) watching another client's session must
        // also render interjections broadcast for that session.
        let mut app = make_app_with_agent("sess-view");
        app.agents.get_mut(&AgentId(0)).unwrap().attached_as_viewer = true;
        let affected =
            handle_ext_notification(&interjection_ext("sess-view", "viewer sees this"), &mut app);
        assert!(affected);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_interjection_text(&agent.scrollback).as_deref(),
            Some("viewer sees this"),
            "a viewer must render interjections broadcast for its session"
        );
    }

    #[test]
    fn interjection_notification_dedups_originators_own_echo() {
        // The originator rendered an optimistic block in dispatch_interject and
        // recorded the id; its own broadcast echo must be dropped (no dup) and
        // the id forgotten.
        let mut app = make_app_with_agent("sess-view");
        app.agents
            .get_mut(&AgentId(0))
            .unwrap()
            .self_interjection_ids
            .insert("ij-1".to_string());

        let affected = handle_ext_notification(
            &interjection_ext_with_id("sess-view", "my own", Some("ij-1")),
            &mut app,
        );
        assert!(
            !affected,
            "an originator's own echo must be a no-op (already rendered locally)"
        );
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            last_interjection_text(&agent.scrollback).is_none(),
            "the echo must not push a duplicate block"
        );
        assert!(
            !agent.self_interjection_ids.contains("ij-1"),
            "the id must be forgotten after dedup"
        );
    }

    #[test]
    fn interjection_notification_with_foreign_id_renders() {
        // A broadcast carrying an id this client did NOT mint (another pane's
        // interjection) must render — only the originator dedups by its own id.
        let mut app = make_app_with_agent("sess-view");
        let affected = handle_ext_notification(
            &interjection_ext_with_id("sess-view", "from another pane", Some("other-id")),
            &mut app,
        );
        assert!(affected);
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            last_interjection_text(&agent.scrollback).as_deref(),
            Some("from another pane"),
            "an interjection from another pane must render"
        );
    }
