#![cfg_attr(rustfmt, rustfmt::skip)]
    use super::*;

    #[test]
    fn elicitation_completion_requires_exact_root_and_server() {
        use crate::views::elicitation_view::ElicitationViewState;
        use xai_grok_tools::mcp_elicitation::{McpElicitExtRequest, McpElicitExtResponse};
        let mut app = make_app_with_agent("parent-session");
        let request: McpElicitExtRequest = serde_json::from_value(serde_json::json!({
            "sessionId": "parent-session",
            "toolCallId": "elicit-call",
            "serverName": "owner-server",
            "message": "Authenticate",
            "mode": "url",
            "url": "https://example.com/login",
            "elicitationId": "shared-id"
        })).unwrap();
        let (response_tx, _response_rx) = tokio::sync::oneshot::channel();
        let mut state = ElicitationViewState::from_request(request, None, Some(response_tx));
        assert!(state.send_response(McpElicitExtResponse::Accept { content: None }));
        state.begin_url_waiting();
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.elicitation_view = Some(state);
        agent.subagent_views.insert(
            "child-session".into(),
            Box::new(crate::app::agent_view::test_fixtures::make_agent()),
        );
        for (session, server, expected) in [
            ("child-session", "owner-server", false),
            ("other-session", "owner-server", false),
            ("parent-session", "other-server", false),
            ("parent-session", "owner-server", true),
        ] {
            let params = serde_json::value::to_raw_value(&serde_json::json!({
                "sessionId": session,
                "elicitationId": "shared-id",
                "serverName": server,
            })).unwrap();
            let notification = acp::ExtNotification::new("x.ai/mcp/elicit_complete", params.into());
            assert_eq!(handle_mcp_elicit_complete(&notification, &mut app), expected);
        }
        assert!(app.agents[&AgentId(0)].elicitation_view.is_none());
    }

    #[test]
    fn child_elicitation_request_cannot_open_parent_card() {
        let mut app = make_app_with_agent("parent-session");
        app.agents.get_mut(&AgentId(0)).unwrap().subagent_views.insert(
            "child-session".into(),
            Box::new(crate::app::agent_view::test_fixtures::make_agent()),
        );
        let params = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "child-session",
            "toolCallId": "child-elicit",
            "serverName": "server",
            "message": "Confirm",
            "mode": "url",
            "url": "https://example.com/login",
            "elicitationId": "child-id",
        })).unwrap();
        let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
        assert!(!handle_mcp_elicit(xai_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/mcp/elicit", params.into()),
            response_tx,
        }, &mut app));
        assert!(app.agents[&AgentId(0)].elicitation_view.is_none());
        assert!(response_rx.try_recv().is_ok());
    }

    #[test]
    fn interaction_resolved_dismisses_matching_permission() {
        // A peer answered a shared permission → this pane retracts its copy.
        let mut app = make_app_with_agent("sess-1");
        let (msg, _rx) = make_permission_message("sess-1");
        handle(msg, &mut app);
        assert_eq!(app.agents[&AgentId(0)].permission_queue.len(), 1);

        let changed = handle_session_notification(
            &interaction_resolved_ext("sess-1", "call-perm-1"),
            &mut app,
        );
        assert!(changed, "dismissing a visible permission must redraw");
        assert!(
            app.agents[&AgentId(0)].permission_queue.is_empty(),
            "the resolved permission must be removed from the queue"
        );
    }

    #[test]
    fn interaction_resolved_dismisses_matching_question() {
        use crate::views::question_view::QuestionViewState;
        let mut app = make_app_with_agent("sess-1");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            let stashed = agent.prompt.stash();
            agent.question_view = Some(QuestionViewState::new("call-q".into(), vec![], stashed));
        }

        let changed =
            handle_session_notification(&interaction_resolved_ext("sess-1", "call-q"), &mut app);
        assert!(changed, "dismissing a visible question must redraw");
        assert!(
            app.agents[&AgentId(0)].question_view.is_none(),
            "the resolved question must be cleared"
        );
    }

    #[test]
    fn interaction_resolved_dismisses_matching_plan_approval() {
        let mut app = make_app_with_agent("sess-1");
        let (ext, _rx) = make_exit_plan_ext_with_tool_call_id("call-plan", Some("# Plan"));
        assert!(handle_exit_plan_mode(ext, &mut app));
        assert!(app.agents[&AgentId(0)].plan_approval_view.is_some());

        let changed =
            handle_session_notification(&interaction_resolved_ext("sess-1", "call-plan"), &mut app);
        assert!(changed, "dismissing a visible plan approval must redraw");
        assert!(
            app.agents[&AgentId(0)].plan_approval_view.is_none(),
            "the resolved plan approval must be cleared"
        );
    }

    #[test]
    fn interaction_resolved_is_noop_for_unknown_tool_call_id() {
        let mut app = make_app_with_agent("sess-1");
        let (msg, _rx) = make_permission_message("sess-1");
        handle(msg, &mut app);

        let changed = handle_session_notification(
            &interaction_resolved_ext("sess-1", "some-other-call"),
            &mut app,
        );
        assert!(!changed, "an unknown tool_call_id must be a silent no-op");
        assert_eq!(
            app.agents[&AgentId(0)].permission_queue.len(),
            1,
            "an unrelated pending modal must be left intact"
        );
    }

    #[test]
    fn permission_for_inactive_agent_queues_on_owning_agent() {
        // The headline behavior change in handle_permission_request:
        // permissions for an inactive owning agent now QUEUE (not cancel)
        // so the user sees them on switching back.
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        switch_active_to(&mut app, AgentId(1));

        let (msg, mut rx) = make_permission_message("sess-A");
        let affected = handle(msg, &mut app);

        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent_a.permission_queue.len(),
            1,
            "permission for inactive A must queue on A's permission_queue"
        );
        let agent_b = app.agents.get(&AgentId(1)).unwrap();
        assert_eq!(
            agent_b.permission_queue.len(),
            0,
            "active B's permission_queue must remain empty"
        );
        assert!(
            !affected,
            "permission queued on a non-active agent must not request a redraw"
        );
        // Permission is still pending; the response_tx must still be alive
        // (no auto-cancel was sent).
        assert!(
            rx.try_recv().is_err(),
            "permission must NOT have been answered yet (queued, not cancelled)"
        );
    }

    #[test]
    fn ask_user_question_routes_to_background_session_not_active_view() {
        // Repro of the dashboard bug: a session started but not entered asks a
        // question. Active view is agent A (sess-A); the question is for the
        // BACKGROUND agent B (sess-B). It must land on B, not fail or land on A.
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        assert_eq!(app.active_view, ActiveView::Agent(AgentId(0)));

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let raw = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-B",
            "toolCallId": "tc-bg",
            "questions": [],
            "mode": "default",
        }))
        .unwrap();
        let msg = AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/ask_user_question", raw.into()),
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(
            !affected,
            "a background-session question must not redraw the active view"
        );
        assert!(
            app.agents.get(&AgentId(1)).unwrap().question_view.is_some(),
            "question must be parked on the session that asked (background agent B)"
        );
        assert!(
            app.agents.get(&AgentId(0)).unwrap().question_view.is_none(),
            "question must NOT land on the unrelated active agent A"
        );
        assert!(
            rx.try_recv().is_err(),
            "response must NOT be sent yet (parked, waiting for user)"
        );
    }

    #[test]
    fn mcp_elicit_opens_elicitation_view_and_parks_response() {
        let mut app = make_app_with_agent("sess-A");
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let raw = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-A",
            "toolCallId": "mcp-elicit-1",
            "serverName": "demo-mcp",
            "message": "Need your email",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": {
                    "email": { "type": "string", "format": "email" }
                },
                "required": ["email"]
            }
        }))
        .unwrap();
        let msg = AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/mcp/elicit", raw.into()),
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);
        assert!(affected, "active session elicitation should request redraw");
        let agent = app.agents.get(&AgentId(0)).unwrap();
        let ev = agent
            .elicitation_view
            .as_ref()
            .expect("elicitation_view open");
        assert_eq!(ev.server_name, "demo-mcp");
        assert_eq!(ev.tool_call_id, "mcp-elicit-1");
        assert!(
            rx.try_recv().is_err(),
            "response must wait for user Accept/Decline/Cancel"
        );
    }

    #[test]
    fn mcp_elicit_does_not_replace_url_waiting() {
        let mut app = make_app_with_agent("sess-A");
        let (tx1, mut rx1) = tokio::sync::oneshot::channel();
        let raw1 = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-A",
            "toolCallId": "mcp-elicit-url",
            "serverName": "demo-mcp",
            "message": "Open login",
            "mode": "url",
            "url": "https://example.com/login",
            "elicitationId": "eid-1"
        }))
        .unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/mcp/elicit", raw1.into()),
                response_tx: tx1,
            }),
            &mut app,
        );
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            let ev = agent.elicitation_view.as_mut().unwrap();
            assert!(ev.send_response(
                xai_grok_tools::mcp_elicitation::McpElicitExtResponse::Accept { content: None },
            ));
            ev.begin_url_waiting();
        }
        assert!(rx1.try_recv().is_ok(), "URL accept must send ACP immediately");

        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        let raw2 = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-A",
            "toolCallId": "mcp-elicit-form",
            "serverName": "demo-mcp",
            "message": "Need email",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": { "email": { "type": "string" } }
            }
        }))
        .unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/mcp/elicit", raw2.into()),
                response_tx: tx2,
            }),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        let ev = agent.elicitation_view.as_ref().unwrap();
        assert!(ev.is_url_waiting());
        assert_eq!(ev.elicitation_id(), Some("eid-1"));
        assert!(agent.pending_elicitation.is_some());
        assert!(
            rx2.try_recv().is_err(),
            "the next elicit must wait until Waiting chrome is dismissed"
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert!(agent.dismiss_waiting_elicitation("eid-1", None));
        let ev = agent.elicitation_view.as_ref().expect("parked form shown");
        assert!(ev.form().is_some(), "promoted card is the parked form");
        assert_eq!(ev.tool_call_id, "mcp-elicit-form");
        assert!(rx2.try_recv().is_err());
    }

    #[test]
    fn peer_resolved_elicitation_hands_draft_to_open_question() {
        use crate::views::question_view::QuestionViewState;

        let mut app = make_app_with_agent("sess-A");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.prompt.set_text("my precious draft");
        }

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let raw = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-A",
            "toolCallId": "mcp-elicit-1",
            "serverName": "demo-mcp",
            "message": "Need your email",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": { "email": { "type": "string" } }
            }
        }))
        .unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/mcp/elicit", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            assert_eq!(agent.prompt.text(), "", "elicitation displaced the draft");
            let stashed = agent.prompt.stash();
            agent.prompt.set_text("");
            agent.question_view = Some(QuestionViewState::new("call-q".into(), vec![], stashed));
        }

        handle_session_notification(&interaction_resolved_ext("sess-A", "mcp-elicit-1"), &mut app);
        assert!(app.agents[&AgentId(0)].elicitation_view.is_none());
        assert_eq!(
            app.agents[&AgentId(0)].prompt.text(),
            "",
            "the question still owns the composer; the draft must not write through"
        );

        handle_session_notification(&interaction_resolved_ext("sess-A", "call-q"), &mut app);
        assert_eq!(
            app.agents[&AgentId(0)].prompt.text(),
            "my precious draft",
            "the question's close must restore the elicitation's session draft"
        );
    }

    #[test]
    fn elicitation_over_open_question_does_not_wipe_stashed_draft() {
        use crate::views::question_view::QuestionViewState;

        let mut app = make_app_with_agent("sess-A");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.prompt.set_text("my precious draft");
            let stashed = agent.prompt.stash();
            agent.prompt.set_text("");
            agent.question_view =
                Some(QuestionViewState::new("call-q".into(), vec![], stashed));
        }

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let raw = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-A",
            "toolCallId": "mcp-elicit-1",
            "serverName": "demo-mcp",
            "message": "Need your email",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": { "email": { "type": "string" } }
            }
        }))
        .unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/mcp/elicit", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );
        {
            let agent = app.agents.get(&AgentId(0)).unwrap();
            let ev = agent.elicitation_view.as_ref().expect("elicitation open");
            assert!(
                ev.stashed_prompt.is_none(),
                "the question already owns the draft; the elicitation must not stash the blank composer"
            );
        }

        handle_session_notification(&interaction_resolved_ext("sess-A", "call-q"), &mut app);
        assert_eq!(
            app.agents[&AgentId(0)].prompt.text(),
            "my precious draft",
            "question close must put the draft back"
        );

        handle_session_notification(&interaction_resolved_ext("sess-A", "mcp-elicit-1"), &mut app);
        assert!(app.agents[&AgentId(0)].elicitation_view.is_none());
        assert_eq!(
            app.agents[&AgentId(0)].prompt.text(),
            "my precious draft",
            "elicitation close must not restore an empty stash over the draft"
        );
    }

    #[test]
    fn parked_elicit_is_dropped_when_peer_resolves_it() {
        let mut app = make_app_with_agent("sess-A");
        let (tx1, _rx1) = tokio::sync::oneshot::channel();
        let raw1 = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-A",
            "toolCallId": "mcp-elicit-url",
            "serverName": "demo-mcp",
            "message": "Open login",
            "mode": "url",
            "url": "https://example.com/login",
            "elicitationId": "eid-1"
        }))
        .unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/mcp/elicit", raw1.into()),
                response_tx: tx1,
            }),
            &mut app,
        );
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            let ev = agent.elicitation_view.as_mut().unwrap();
            assert!(ev.send_response(
                xai_grok_tools::mcp_elicitation::McpElicitExtResponse::Accept { content: None },
            ));
            ev.begin_url_waiting();
        }

        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        let raw2 = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-A",
            "toolCallId": "mcp-elicit-form",
            "serverName": "demo-mcp",
            "message": "Need email",
            "mode": "form",
            "requestedSchema": {
                "type": "object",
                "properties": { "email": { "type": "string" } }
            }
        }))
        .unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/mcp/elicit", raw2.into()),
                response_tx: tx2,
            }),
            &mut app,
        );
        assert!(app.agents[&AgentId(0)].pending_elicitation.is_some());

        let changed = handle_session_notification(
            &interaction_resolved_ext("sess-A", "mcp-elicit-form"),
            &mut app,
        );
        assert!(changed);
        assert!(
            app.agents[&AgentId(0)].pending_elicitation.is_none(),
            "peer resolve must drop the parked form"
        );
        match rx2.try_recv() {
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
            other => panic!("parked oneshot must be dropped, got {other:?}"),
        }

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert!(agent.dismiss_waiting_elicitation("eid-1", None));
        assert!(
            agent.elicitation_view.is_none(),
            "must not promote a peer-resolved parked form"
        );
    }

    #[test]
    fn elicit_complete_requires_matching_server_name() {
        let mut app = make_app_with_agent("sess-A");
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let raw = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-A",
            "toolCallId": "mcp-elicit-url",
            "serverName": "demo-mcp",
            "message": "Open login",
            "mode": "url",
            "url": "https://example.com/login",
            "elicitationId": "eid-1"
        }))
        .unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/mcp/elicit", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            let ev = agent.elicitation_view.as_mut().unwrap();
            assert!(ev.send_response(
                xai_grok_tools::mcp_elicitation::McpElicitExtResponse::Accept { content: None },
            ));
            ev.begin_url_waiting();
        }

        let complete = |server_name: &str| {
            serde_json::value::to_raw_value(&serde_json::json!({
                "sessionId": "sess-A",
                "elicitationId": "eid-1",
                "serverName": server_name,
            }))
            .unwrap()
        };

        let (tx_bad, _rx_bad) = tokio::sync::oneshot::channel();
        let changed = handle(
            AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
                request: acp::ExtNotification::new(
                    "x.ai/mcp/elicit_complete",
                    complete("evil-mcp").into(),
                ),
                response_tx: tx_bad,
            }),
            &mut app,
        );
        assert!(!changed);
        assert!(
            app.agents[&AgentId(0)].elicitation_view.is_some(),
            "a mismatched serverName must not dismiss the waiting card"
        );

        let (tx_ok, _rx_ok) = tokio::sync::oneshot::channel();
        let changed = handle(
            AcpClientMessage::ExtNotification(xai_acp_lib::AcpArgs {
                request: acp::ExtNotification::new(
                    "x.ai/mcp/elicit_complete",
                    complete("demo-mcp").into(),
                ),
                response_tx: tx_ok,
            }),
            &mut app,
        );
        assert!(changed);
        assert!(
            app.agents[&AgentId(0)].elicitation_view.is_none(),
            "the matching serverName must dismiss the waiting card"
        );
    }

    #[test]
    fn ask_user_question_unknown_session_parks_without_error() {
        // No local view for the session, and the active agent HAS a session_id
        // (so the race-window fallback does not fire). The reverse-request must
        // be left UNANSWERED (dropped) — NOT failed with an error, which would
        // render the tool red. Leader replay-on-attach handles it later.
        let mut app = make_app_with_agent("sess-A");

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let raw = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "sess-unknown",
            "toolCallId": "tc-unknown",
            "questions": [],
            "mode": "default",
        }))
        .unwrap();
        let msg = AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
            request: acp::ExtRequest::new("x.ai/ask_user_question", raw.into()),
            response_tx: tx,
        });

        let affected = handle(msg, &mut app);

        assert!(!affected);
        assert!(
            app.agents.get(&AgentId(0)).unwrap().question_view.is_none(),
            "must not attach the question to an unrelated active agent"
        );
        // A dropped oneshot sender yields `Closed`; `Empty` would mean still
        // held open, `Ok` would mean a (failing) response was sent.
        match rx.try_recv() {
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                panic!("response_tx must be dropped (parked), not held open")
            }
            Ok(_) => panic!("must NOT send any response — that would fail/resolve the tool"),
        }
    }

    #[test]
    fn permission_for_inactive_yolo_agent_auto_approves() {
        // YOLO mode is honored on the OWNING agent, not the active one,
        // so background turns aren't blocked waiting for a switch.
        let mut app = make_app_with_agent("sess-A");
        app.agents.get_mut(&AgentId(0)).unwrap().session.yolo_mode = true;
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        switch_active_to(&mut app, AgentId(1));

        let (msg, rx) = make_permission_message("sess-A");
        let affected = handle(msg, &mut app);

        assert!(!affected, "YOLO auto-approve never needs a redraw");
        let agent_a = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent_a.permission_queue.len(),
            0,
            "YOLO must auto-approve in place of queueing"
        );
        let response = rx
            .blocking_recv()
            .expect("YOLO must have sent a response on response_tx");
        let resp = response.expect("YOLO response must be Ok");
        match resp.outcome {
            acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome {
                option_id,
                ..
            }) => {
                assert_eq!(option_id.0.as_ref(), "allow-once");
            }
            other => panic!("expected Selected, got {other:?}"),
        }
    }

    #[test]
    fn permission_for_unknown_session_id_is_cancelled() {
        // No agent owns the session and the active agent already has a
        // session_id (so the race-window fallback does not fire). The
        // permission must be cancelled rather than queued anywhere.
        let mut app = make_app_with_agent("sess-A");
        insert_agent(&mut app, AgentId(1), Some("sess-B"));
        // make_app_with_agent already activated AgentId(0); no switch needed.

        let (msg, rx) = make_permission_message("sess-unknown");
        let affected = handle(msg, &mut app);

        assert!(!affected);
        for id in [AgentId(0), AgentId(1)] {
            assert_eq!(
                app.agents.get(&id).unwrap().permission_queue.len(),
                0,
                "no agent should have queued the unknown-session permission",
            );
        }
        let response = rx
            .blocking_recv()
            .expect("cancel_permission must have sent a response");
        let resp = response.expect("response must be Ok");
        assert!(
            matches!(resp.outcome, acp::RequestPermissionOutcome::Cancelled),
            "unknown session_id permissions must be cancelled, got {:?}",
            resp.outcome,
        );
    }

    // ── Plan approval persistence tests ─────────────────────────

    #[test]
    fn close_viewer_preserves_plan_approval_state() {
        let mut app = make_app_with_agent("sess-A");

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-A".into(),
            tool_call_id: "tc-persist".into(),
            plan_content: Some("# Plan\nDo stuff".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.plan_approval_view.is_some(), "approval should be set");

        // Close the viewer (simulates Esc / close button).
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.cancel_line_viewer();

        // Approval state must survive the close.
        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.plan_approval_view.is_some(),
            "plan_approval_view must persist after viewer close"
        );
        assert!(agent.line_viewer.is_none(), "viewer should be closed");

        // Response must NOT have been sent (still waiting for user).
        assert!(
            rx.try_recv().is_err(),
            "response must not be sent on viewer close"
        );
    }

    #[test]
    fn reopen_viewer_restores_approval_buttons() {
        let mut app = make_app_with_agent("sess-A");
        // Seed a CreatePlan tool so the source is Inline (plan content
        // is carried in the ext_method params, not read from disk).
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            seed_pending_tool(agent, "tc-reopen", "CreatePlan");
        }

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-A".into(),
            tool_call_id: "tc-reopen".into(),
            plan_content: Some("# Plan\nStep 1".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        // Close viewer.
        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        agent.cancel_line_viewer();
        assert!(agent.line_viewer.is_none());

        // Reopen plan preview — inline content is in plan_approval_view.plan_content.
        agent.show_plan_preview();

        assert!(agent.line_viewer.is_some(), "viewer should reopen");
        assert!(
            agent.line_viewer.as_ref().unwrap().feedback_active(),
            "feedback_active must be true after reopen"
        );
    }

    #[test]
    fn approve_after_reopen_keeps_session_draft_and_sends_freeform() {
        let mut app = make_app_with_agent("sess-A");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.prompt.set_text("session draft mid-thinking");
            seed_pending_tool(agent, "tc-prompt", "CreatePlan");
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-A".into(),
            tool_call_id: "tc-prompt".into(),
            plan_content: Some("# Plan\nDo things".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        let agent = app.agents.get_mut(&AgentId(0)).unwrap();
        assert_eq!(agent.prompt.text(), "session draft mid-thinking");
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .map(|p| p.stashed_prompt.text.as_str()),
            Some("session draft mid-thinking"),
        );

        agent.cancel_line_viewer();
        agent.prompt.set_text("revision freeform notes");

        agent.reopen_plan_approval();
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .map(|p| p.stashed_prompt.text.as_str()),
            Some("session draft mid-thinking"),
        );
        assert_eq!(agent.prompt.text(), "revision freeform notes");

        let outcome = agent.approve_plan();
        assert!(matches!(
            outcome,
            crate::app::app_view::InputOutcome::Action(
                crate::app::actions::Action::Interject { ref text, .. }
            ) if text.contains("revision freeform notes")
        ));

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(agent.prompt.text(), "session draft mid-thinking");

        let response = rx.blocking_recv().expect("should have sent response");
        let raw = response.expect("should be Ok");
        let parsed: serde_json::Value = serde_json::from_str(raw.0.get()).unwrap();
        assert_eq!(parsed["outcome"], "approved");
    }

    #[test]
    fn exit_plan_mode_prefills_mid_thinking_draft_as_freeform() {
        let mut app = make_app_with_agent("sess-B");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.prompt.set_text("typed while thinking");
            seed_pending_tool(agent, "tc-draft", "CreatePlan");
        }

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-B".into(),
            tool_call_id: "tc-draft".into(),
            plan_content: Some("# Plan\n".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(agent.plan_approval_view.is_some());
        assert_eq!(agent.prompt.text(), "typed while thinking");
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .map(|p| p.stashed_prompt.text.as_str()),
            Some("typed while thinking"),
        );
    }

    #[test]
    fn exit_plan_mode_with_permission_followup_keeps_real_session_draft() {
        let mut app = make_app_with_agent("sess-perm");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.permission_stashed_prompt =
                Some(crate::views::prompt_widget::StashedPrompt {
                    text: "real session draft".into(),
                    cursor: 0,
                    images: Vec::new(),
                    chip_elements: Vec::new(),
                    image_counter: 0,
                    image_undo_stash: Vec::new(),
                });
            // Non-empty queue means permission still owns the keyboard.
            agent.permission_queue.push_back(
                crate::app::agent_view::test_fixtures::make_followup_permission_state(),
            );
            agent.prompt.set_text("permission followup text");
            seed_pending_tool(agent, "tc-perm", "CreatePlan");
        }

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-perm".into(),
            tool_call_id: "tc-perm".into(),
            plan_content: Some("# Plan\n".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .map(|p| p.stashed_prompt.text.as_str()),
            Some("real session draft"),
        );
        assert_eq!(
            agent.prompt.text(),
            "",
            "live must stay empty while permission owns keys"
        );
        assert!(agent.permission_stashed_prompt.is_none());
        assert!(!agent.permission_queue.is_empty());
    }

    #[test]
    fn exit_plan_mode_prefills_image_chips_into_freeform() {
        let mut app = make_app_with_agent("sess-img");
        {
            let agent = app.agents.get_mut(&AgentId(0)).unwrap();
            agent.prompt.set_text("see ");
            let img = crate::prompt_images::PastedImage {
                element_id: xai_ratatui_textarea::ElementId::from_raw(0),
                display_number: 0,
                mime_type: "image/png".into(),
                dimensions: Some((100, 80)),
                byte_len: 2048,
                encoded_bytes: Some(vec![0u8; 16].into()),
                source_path: None,
                staged_temp_path: None,
                session_image_path: None,
                preview: crate::prompt_images::PromptImagePreview::default(),
            };
            agent.prompt.insert_image(img).expect("insert image");
            seed_pending_tool(agent, "tc-img", "CreatePlan");
        }

        let (tx, _rx) = tokio::sync::oneshot::channel();
        let ext_req = crate::views::plan_approval_view::ExitPlanModeExtRequest {
            session_id: "sess-img".into(),
            tool_call_id: "tc-img".into(),
            plan_content: Some("# Plan\n".into()),
        };
        let raw = serde_json::value::to_raw_value(&ext_req).unwrap();
        handle(
            AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
                request: acp::ExtRequest::new("x.ai/exit_plan_mode", raw.into()),
                response_tx: tx,
            }),
            &mut app,
        );

        let agent = app.agents.get(&AgentId(0)).unwrap();
        assert!(
            agent.prompt.text().contains("[Image #1]"),
            "freeform prefill must keep image chip text, got {:?}",
            agent.prompt.text()
        );
        assert_eq!(
            agent.prompt.images.len(),
            1,
            "freeform prefill must restore image payload"
        );
        assert_eq!(
            agent
                .plan_approval_view
                .as_ref()
                .map(|p| p.stashed_prompt.images.len()),
            Some(1),
            "session draft must retain its own image payload"
        );
    }
