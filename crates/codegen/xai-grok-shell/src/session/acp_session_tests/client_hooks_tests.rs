use super::support::*;
use super::*;

/// Client hooks must fire even with no on-disk hook registry: `notify_client_hooks`
/// reads `client_hooks` (never `hook_registry`) and its call sites sit outside the
/// file-registry guard.
#[tokio::test(flavor = "current_thread")]
async fn client_hooks_fire_without_file_registry() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            assert!(
                actor.hook_registry.borrow().is_none(),
                "fixture must have no file registry for this invariant"
            );
            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            client_hooks.insert(
                xai_grok_hooks::event::HookEventName::Stop,
                vec![crate::extensions::hooks::ClientHookGroup {
                    matcher: None,
                    callback_ids: vec!["cb_0".to_string()],
                    timeout: None,
                }],
            );
            *actor.client_hooks.borrow_mut() = client_hooks;

            actor.fire_hook(
                xai_grok_hooks::event::HookEventName::Stop,
                None,
                xai_grok_hooks::event::HookPayload::Stop {
                    reason: "end_turn".to_string(),
                },
            );

            let msg = gateway_rx
                .try_recv()
                .expect("client hook must fire with no file registry");
            let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg else {
                panic!("expected an x.ai/hooks/event ext notification");
            };
            assert_eq!(args.request.method.as_ref(), "x.ai/hooks/event");
            let params: serde_json::Value =
                serde_json::from_str(args.request.params.get()).unwrap();
            assert_eq!(params["hookCallbackId"], "cb_0");
            assert_eq!(params["hookEventName"], "stop");
        })
        .await;
}

/// The PreToolUse gate blocks a tool when a client hook returns `deny`: the reverse
/// `x.ai/hooks/run` request is answered with a deny and `run_pre_tool_use_client_hook`
/// returns `ToolLoop::HookDenied`. Complements the pure `classify` test by covering the
/// gate wiring (the one new path that can block tool execution).
#[tokio::test(flavor = "current_thread")]
async fn pre_tool_use_client_deny_blocks_the_tool() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            client_hooks.insert(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                vec![crate::extensions::hooks::ClientHookGroup {
                    matcher: None,
                    callback_ids: vec!["cb_0".to_string()],
                    timeout: None,
                }],
            );
            *actor.client_hooks.borrow_mut() = client_hooks;

            // Answer the x.ai/hooks/run reverse request with a deny; ack the UI
            // notifications `deny_tool` emits so it can't block the gate.
            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let deny: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&serde_json::json!({
                                    "decision": "deny",
                                    "systemMessage": "nope",
                                }))
                                .unwrap()
                                .into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                        }
                        xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "run_terminal_command",
                    "{}",
                ),
            };
            let tool_call_id = acp::ToolCallId::new("call_1");
            let envelope = actor.make_hook_envelope(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                None,
                xai_grok_hooks::event::HookPayload::PreToolUse {
                    tool_name: call.function.name.clone(),
                    tool_use_id: call.id.clone(),
                    tool_input: serde_json::json!({}),
                    tool_input_truncated: false,
                    permission_mode: None,
                    subagent_type: None,
                },
            );

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_pre_tool_use_client_hook(&call, &tool_call_id, &envelope),
            )
            .await
            .expect("the gate must not hang")
            .expect("the gate must not error");
            assert!(
                matches!(result, Some(ToolLoop::HookDenied { .. })),
                "a client deny must block the tool"
            );
        })
        .await;
}

/// A `use_tool` call whose wire `function.name` is the dispatcher surfaces to PreToolUse
/// hooks as its resolved target, so a matcher keyed on the qualified MCP name
/// (`linear__save_issue`) gates the dispatch. Drives the real `prepare_tool_call`
/// construction path (not a hand-built envelope); the deny only fires if the resolved
/// name reached the envelope.
#[tokio::test(flavor = "current_thread")]
async fn pre_tool_use_resolves_meta_dispatch_tool_name_end_to_end() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            // The toolset must know `use_tool` so it parses to `ToolInput::UseTool`.
            *actor.agent.borrow_mut() = test_agent_with_tools(vec![
                xai_grok_tools::registry::types::ToolConfig::for_tool::<
                    xai_grok_tools::implementations::use_tool::UseTool,
                >(),
            ])
            .await;

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            client_hooks.insert(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                vec![crate::extensions::hooks::ClientHookGroup {
                    matcher: Some(
                        xai_grok_hooks::matcher::HookMatcher::new("linear__save_issue").unwrap(),
                    ),
                    callback_ids: vec!["cb_0".to_string()],
                    timeout: None,
                }],
            );
            *actor.client_hooks.borrow_mut() = client_hooks;

            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let deny: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&serde_json::json!({
                                    "decision": "deny",
                                    "systemMessage": "nope",
                                }))
                                .unwrap()
                                .into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                        }
                        xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            // Wire `function.name` is the dispatcher; the arguments carry the real target.
            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "use_tool",
                    r#"{"tool_name":"linear__save_issue","tool_input":{}}"#,
                ),
            };

            let mut deferred = Vec::new();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.prepare_tool_call(call, &mut deferred),
            )
            .await
            .expect("prepare_tool_call must not hang")
            .expect("prepare_tool_call must not error");
            assert!(
                matches!(result, Err(ToolLoop::HookDenied { .. })),
                "a hook matched on the resolved tool must gate the use_tool dispatch; \
                 got {result:?}"
            );
        })
        .await;
}

/// Subagent inheritance (the design headline): a tool call inside a SUBAGENT is gated by
/// the PARENT's registered client hook. In prod the subagent inherits the parent's hooks via
/// `ctx.client_hooks.clone()` (`agent/subagent/`), itself fed by the `SnapshotClientHooks`
/// clone (`session.client_hooks.clone()`). This is the seam-level test: it reproduces that
/// exact clone into a child `SessionActor` (a full subagent spawn needs the sampler / child
/// thread / gateway bridge, disproportionate here), then proves a subagent tool call hits the
/// parent's PreToolUse gate (deny blocks it) and that the dispatch carries the `subagentType`.
#[tokio::test(flavor = "current_thread")]
async fn subagent_inherits_parent_pre_tool_use_client_hook() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (parent_gateway_tx, _parent_gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (parent_persistence_tx, _parent_persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let parent =
                create_test_actor(0, 256_000, 85, parent_gateway_tx, parent_persistence_tx).await;

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            client_hooks.insert(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                vec![crate::extensions::hooks::ClientHookGroup {
                    matcher: None,
                    callback_ids: vec!["cb_0".to_string()],
                    timeout: None,
                }],
            );
            *parent.client_hooks.borrow_mut() = client_hooks;

            let (child_gateway_tx, mut child_gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (child_persistence_tx, _child_persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let subagent =
                create_test_actor(0, 256_000, 85, child_gateway_tx, child_persistence_tx).await;

            // The inheritance seam under test (subagent.rs `ctx.client_hooks.clone()`): a child
            // with no hooks of its own takes a clone of the parent's.
            assert!(
                subagent.client_hooks.borrow().is_empty(),
                "the subagent starts with no hooks of its own"
            );
            *subagent.client_hooks.borrow_mut() = parent.client_hooks.borrow().clone();

            // Record the subagentType the parent's hook is dispatched with; answer the run deny.
            let seen_subagent_type = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
            let seen = seen_subagent_type.clone();
            tokio::task::spawn_local(async move {
                while let Some(msg) = child_gateway_rx.recv().await {
                    match msg {
                        xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let params: serde_json::Value =
                                serde_json::from_str(args.request.params.get()).unwrap();
                            *seen.lock().unwrap() =
                                params["subagentType"].as_str().map(str::to_string);
                            let deny: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&serde_json::json!({
                                    "decision": "deny",
                                }))
                                .unwrap()
                                .into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                        }
                        xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "run_terminal_command",
                    "{}",
                ),
            };
            let tool_call_id = acp::ToolCallId::new("call_1");
            // The subagent builds the envelope, tagging the call with its subagent type.
            let envelope = subagent.make_hook_envelope(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                None,
                xai_grok_hooks::event::HookPayload::PreToolUse {
                    tool_name: call.function.name.clone(),
                    tool_use_id: call.id.clone(),
                    tool_input: serde_json::json!({}),
                    tool_input_truncated: false,
                    permission_mode: None,
                    subagent_type: Some("code-reviewer".to_string()),
                },
            );

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                subagent.run_pre_tool_use_client_hook(&call, &tool_call_id, &envelope),
            )
            .await
            .expect("the gate must not hang")
            .expect("the gate must not error");

            assert!(
                matches!(result, Some(ToolLoop::HookDenied { .. })),
                "a subagent tool call must be blocked by the parent's inherited PreToolUse hook"
            );
            assert_eq!(
                seen_subagent_type.lock().unwrap().as_deref(),
                Some("code-reviewer"),
                "the parent's hook must observe the subagent's type on the dispatch"
            );
        })
        .await;
}

/// A slow/hung callback must not starve a later deny: with the first-registered callback
/// never replying and the second denying, the gate returns `HookDenied` quickly (a
/// sequential gate would block on the hung one's full timeout). Pins the concurrency claim.
#[tokio::test(flavor = "current_thread")]
async fn pre_tool_use_slow_callback_does_not_starve_a_deny() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            client_hooks.insert(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                vec![crate::extensions::hooks::ClientHookGroup {
                    matcher: None,
                    // "slow_cb" is registered first and never replies; "deny_cb" denies.
                    callback_ids: vec!["slow_cb".to_string(), "deny_cb".to_string()],
                    timeout: None,
                }],
            );
            *actor.client_hooks.borrow_mut() = client_hooks;

            tokio::task::spawn_local(async move {
                let mut held = Vec::new();
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let params: serde_json::Value =
                                serde_json::from_str(args.request.params.get()).unwrap();
                            if params["hookCallbackId"] == "deny_cb" {
                                let deny: Arc<serde_json::value::RawValue> =
                                    serde_json::value::to_raw_value(&serde_json::json!({
                                        "decision": "deny",
                                    }))
                                    .unwrap()
                                    .into();
                                let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                            } else {
                                held.push(args.response_tx);
                            }
                        }
                        xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "run_terminal_command",
                    "{}",
                ),
            };
            let tool_call_id = acp::ToolCallId::new("call_1");
            let envelope = actor.make_hook_envelope(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                None,
                xai_grok_hooks::event::HookPayload::PreToolUse {
                    tool_name: call.function.name.clone(),
                    tool_use_id: call.id.clone(),
                    tool_input: serde_json::json!({}),
                    tool_input_truncated: false,
                    permission_mode: None,
                    subagent_type: None,
                },
            );

            // 5s ceiling is well under the hung callback's 30s per-callback timeout, so a
            // pass proves the deny was not serialized behind the slow callback.
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.run_pre_tool_use_client_hook(&call, &tool_call_id, &envelope),
            )
            .await
            .expect("a deny must resolve without waiting on the hung callback")
            .expect("the gate must not error");
            assert!(matches!(result, Some(ToolLoop::HookDenied { .. })));
        })
        .await;
}

/// PostToolUse and PostToolUseFailure must never both fire for one tool call: a hard
/// dispatch error fires only PostToolUseFailure; a successful dispatch fires only
/// PostToolUse. Guards the explicitly-hardened no-double-fire path (the PostToolUse
/// success block routes through `dispatch_hook`, the same as the failure arm). Each
/// post-tool event is observed as a fire-and-forget `x.ai/hooks/event` notification.
#[tokio::test(flavor = "current_thread")]
async fn post_tool_use_and_failure_never_double_fire() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            // The agent's tool bridge must know `todo_write` for it to parse + dispatch.
            *actor.agent.borrow_mut() = test_grok_build_agent_with_todo().await;

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            for event in [
                xai_grok_hooks::event::HookEventName::PostToolUse,
                xai_grok_hooks::event::HookEventName::PostToolUseFailure,
            ] {
                client_hooks.insert(
                    event,
                    vec![crate::extensions::hooks::ClientHookGroup {
                        matcher: None,
                        callback_ids: vec!["cb".to_string()],
                        timeout: None,
                    }],
                );
            }
            *actor.client_hooks.borrow_mut() = client_hooks;

            // Collect the `hookEventName` of every `x.ai/hooks/event` notification queued.
            let drain =
                |rx: &mut tokio::sync::mpsc::UnboundedReceiver<xai_acp_lib::AcpClientMessage>| {
                    let mut events = Vec::new();
                    while let Ok(msg) = rx.try_recv() {
                        if let xai_acp_lib::AcpClientMessage::ExtNotification(args) = msg
                            && args.request.method.as_ref() == "x.ai/hooks/event"
                        {
                            let params: serde_json::Value =
                                serde_json::from_str(args.request.params.get()).unwrap();
                            if let Some(name) = params["hookEventName"].as_str() {
                                events.push(name.to_string());
                            }
                        }
                    }
                    events
                };

            let todo_call = |id: &str| crate::sampling::types::ToolCallResponse {
                id: id.to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "todo_write",
                    r#"{"todos":[{"id":"t1","content":"do","status":"completed"}]}"#,
                ),
            };

            // Failure: no workspace session is bound, so the dispatch hard-errors.
            actor
                .execute_tool_calls(vec![todo_call("call_err")])
                .await
                .expect("execute_tool_calls must not error");
            assert_eq!(
                drain(&mut gateway_rx),
                ["post_tool_use_failure"],
                "an errored tool must fire only PostToolUseFailure, never PostToolUse"
            );

            // Success: bind the session so the tool dispatches cleanly.
            actor
                .workspace_ops
                .bind_local_session(
                    &actor.session_id_string(),
                    actor.tool_context.cwd.as_path().to_path_buf(),
                    actor.tool_context.hunk_tracker_handle.clone(),
                    actor.agent.borrow().tool_bridge().toolset(),
                    None,
                )
                .expect("bind_local_session must succeed");
            actor
                .execute_tool_calls(vec![todo_call("call_ok")])
                .await
                .expect("execute_tool_calls must not error");
            assert_eq!(
                drain(&mut gateway_rx),
                ["post_tool_use"],
                "a successful tool must fire PostToolUse exactly once, never PostToolUseFailure"
            );
        })
        .await;
}

/// A `pre_tool_use` deny must NOT cancel the turn. `execute_tool_calls` feeds the
/// deny reason back as the blocked tool's `tool_result` and returns
/// `ToolLoop::Continue`, so the turn loop keeps going and the model re-samples with
/// the reason in context and can adapt/retry (common agent-hook semantics).
///
/// Regression guard for the bug where a hook deny surfaced as `ToolLoop::HookDenied`,
/// which `execute_tool_calls` treated as a terminal `final_result` and the turn loop
/// turned into `TurnOutcome::Cancelled` — ending the whole turn instead of letting
/// the model retry based on the reason.
#[tokio::test(flavor = "current_thread")]
async fn pre_tool_use_deny_feeds_reason_back_and_continues_turn() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gateway_tx, mut gateway_rx) =
                tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
            let (persistence_tx, _persistence_rx) =
                tokio::sync::mpsc::unbounded_channel::<PersistenceMsg>();
            let actor = create_test_actor(0, 256_000, 85, gateway_tx, persistence_tx).await;
            // The agent's tool bridge must know `todo_write` so it parses + reaches
            // the PreToolUse gate (rather than short-circuiting as an unknown tool).
            *actor.agent.borrow_mut() = test_grok_build_agent_with_todo().await;

            let mut client_hooks = crate::extensions::hooks::ClientHooks::new();
            client_hooks.insert(
                xai_grok_hooks::event::HookEventName::PreToolUse,
                vec![crate::extensions::hooks::ClientHookGroup {
                    matcher: None,
                    callback_ids: vec!["cb_0".to_string()],
                    timeout: None,
                }],
            );
            *actor.client_hooks.borrow_mut() = client_hooks;

            // Answer the reverse x.ai/hooks/run request with a deny carrying a reason;
            // ack the UI notifications `deny_tool` emits so it can't block the gate.
            tokio::task::spawn_local(async move {
                while let Some(msg) = gateway_rx.recv().await {
                    match msg {
                        xai_acp_lib::AcpClientMessage::ExtMethod(args) => {
                            let deny: Arc<serde_json::value::RawValue> =
                                serde_json::value::to_raw_value(&serde_json::json!({
                                    "decision": "deny",
                                    "systemMessage": "use read_file instead",
                                }))
                                .unwrap()
                                .into();
                            let _ = args.response_tx.send(Ok(acp::ExtResponse::new(deny)));
                        }
                        xai_acp_lib::AcpClientMessage::SessionNotification(args) => {
                            let _ = args.response_tx.send(Ok(()));
                        }
                        _ => {}
                    }
                }
            });

            let call = ToolCallResponse {
                id: "call_1".to_string(),
                kind: "function".to_string(),
                function: crate::sampling::types::ToolCallFunction::new(
                    "todo_write",
                    r#"{"todos":[{"id":"t1","content":"do","status":"completed"}]}"#,
                ),
            };

            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                actor.execute_tool_calls(vec![call]),
            )
            .await
            .expect("execute_tool_calls must not hang")
            .expect("execute_tool_calls must not error");

            // The turn must continue (deny fed back), NOT terminate.
            assert!(
                matches!(result, ToolLoop::Continue),
                "a pre_tool_use deny must continue the turn, got {result:?}"
            );

            // The deny reason must be pushed as the blocked tool's result so the
            // model sees it on the next sampling and can retry.
            let conv = actor.chat_state_handle.get_conversation().await;
            assert!(
                conv.iter()
                    .any(|c| c.text_content().contains("use read_file instead")),
                "the deny reason must be fed back as the tool_result"
            );
        })
        .await;
}
