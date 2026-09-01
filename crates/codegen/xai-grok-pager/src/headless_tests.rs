use pretty_assertions::assert_eq;

#[test]
fn fresh_headless_metadata_preserves_explicit_session_and_model() {
    for session_id in [None, Some("session-1")] {
        for model_id in [None, Some("configured-provider-model")] {
            let meta = super::headless_create_meta(session_id, model_id);
            assert_eq!(meta["sessionKind"], "headless");
            assert_eq!(
                meta.get("sessionId").and_then(serde_json::Value::as_str),
                session_id
            );
            assert_eq!(
                meta.get("modelId").and_then(serde_json::Value::as_str),
                model_id
            );
            assert_eq!(
                meta.len(),
                1 + usize::from(session_id.is_some()) + usize::from(model_id.is_some())
            );
        }
    }
}

#[tokio::test]
async fn memory_flush_waits_for_response_and_drains_lifecycle_notifications() {
    use agent_client_protocol as acp;
    use xai_acp_lib::{AcpAgentMessage, AcpArgs, AcpClientMessage};

    let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel();
    let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();
    let session_id = acp::SessionId::new("flush-session");
    let mut emitter = super::HeadlessEmitter::new(super::OutputFormat::Json, false);
    let server = async {
        let Some(AcpAgentMessage::ExtMethod(request)) = agent_rx.recv().await else {
            panic!("expected memory flush extension request");
        };
        assert_eq!(request.request.method.as_ref(), "x.ai/memory/flush");
        let params: serde_json::Value = serde_json::from_str(request.request.params.get()).unwrap();
        assert_eq!(params, serde_json::json!({"session_id": "flush-session"}));
        for update in [
            serde_json::json!({"sessionUpdate": "memory_flush_started"}),
            serde_json::json!({"sessionUpdate": "memory_flush_completed", "result": "updated", "path": "memory.md"}),
        ] {
            let params = serde_json::value::to_raw_value(&serde_json::json!({
                "sessionId": "flush-session",
                "update": update,
            }))
            .unwrap();
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            client_tx
                .send(AcpClientMessage::ExtNotification(AcpArgs {
                    request: acp::ExtNotification::new("x.ai/session_notification", params.into()),
                    response_tx,
                }))
                .unwrap();
            response_rx.await.unwrap().unwrap();
        }
        let response =
            serde_json::value::to_raw_value(&serde_json::json!({"flushed": true})).unwrap();
        request
            .response_tx
            .send(Ok(acp::ExtResponse::new(response.into())))
            .unwrap();
    };
    let (result, ()) = tokio::join!(
        super::run_headless_memory_flush(
            &agent_tx,
            &mut client_rx,
            &session_id,
            &mut emitter,
            false
        ),
        server,
    );
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn memory_flush_rejects_skipped_or_malformed_responses() {
    use agent_client_protocol as acp;
    use xai_acp_lib::AcpAgentMessage;

    for response in [r#"{"flushed":false}"#, "{}", "null"] {
        let (agent_tx, mut agent_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();
        let session_id = acp::SessionId::new("flush-session");
        let mut emitter = super::HeadlessEmitter::new(super::OutputFormat::Json, false);
        let server = async {
            let Some(AcpAgentMessage::ExtMethod(request)) = agent_rx.recv().await else {
                panic!("expected memory flush extension request");
            };
            let response = serde_json::value::RawValue::from_string(response.to_string()).unwrap();
            request
                .response_tx
                .send(Ok(acp::ExtResponse::new(response.into())))
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            super::run_headless_memory_flush(
                &agent_tx,
                &mut client_rx,
                &session_id,
                &mut emitter,
                false
            ),
            server,
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("memory flush skipped")
        );
    }
}

#[tokio::test]
async fn memory_flush_reports_closed_connection() {
    let (agent_tx, _agent_rx) = tokio::sync::mpsc::unbounded_channel();
    let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();
    drop(client_tx);
    let mut emitter = super::HeadlessEmitter::new(super::OutputFormat::Json, false);
    let session_id = agent_client_protocol::SessionId::new("flush-session");
    let result = super::run_headless_memory_flush(
        &agent_tx,
        &mut client_rx,
        &session_id,
        &mut emitter,
        false,
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("connection closed")
    );
}

#[test]
fn lifecycle_tracking_is_independent_of_wait_flag() {
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    super::track_background_lifecycle(
        super::ExtEvent::TaskBackgrounded {
            task_id: "t1".into(),
            is_monitor: false,
        },
        &mut pending,
        &mut completed,
    );
    super::track_background_lifecycle(
        super::ExtEvent::SubagentSpawned {
            subagent_id: "s1".into(),
        },
        &mut pending,
        &mut completed,
    );
    assert!(pending.contains(&super::BackgroundWork::Task("t1".into())));
    assert!(pending.contains(&super::BackgroundWork::Subagent("s1".into())));

    super::track_background_lifecycle(
        super::ExtEvent::TaskCompleted {
            task_id: "t1".into(),
        },
        &mut pending,
        &mut completed,
    );
    super::track_background_lifecycle(
        super::ExtEvent::SubagentFinished {
            subagent_id: "s1".into(),
        },
        &mut pending,
        &mut completed,
    );
    assert!(pending.is_empty());
}

#[test]
fn completion_before_backgrounded_never_rearms_pending() {
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    super::track_background_lifecycle(
        super::ExtEvent::TaskCompleted {
            task_id: "t1".into(),
        },
        &mut pending,
        &mut completed,
    );
    super::track_background_lifecycle(
        super::ExtEvent::TaskBackgrounded {
            task_id: "t1".into(),
            is_monitor: false,
        },
        &mut pending,
        &mut completed,
    );
    assert!(pending.is_empty());
}

/// A late/duplicate `task_backgrounded` must not resurrect a completed task.
#[test]
fn duplicate_backgrounded_after_completion_stays_dead() {
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    let bg = || super::ExtEvent::TaskBackgrounded {
        task_id: "t1".into(),
        is_monitor: false,
    };
    super::track_background_lifecycle(bg(), &mut pending, &mut completed);
    assert!(pending.contains(&super::BackgroundWork::Task("t1".into())));
    super::track_background_lifecycle(
        super::ExtEvent::TaskCompleted {
            task_id: "t1".into(),
        },
        &mut pending,
        &mut completed,
    );
    assert!(pending.is_empty());
    super::track_background_lifecycle(bg(), &mut pending, &mut completed);
    assert!(
        pending.is_empty(),
        "a backgrounded for an already-completed id must not re-arm pending"
    );
}

/// The same tombstone dedup applies to background subagents.
#[test]
fn duplicate_subagent_spawn_after_finish_stays_dead() {
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    let spawn = || super::ExtEvent::SubagentSpawned {
        subagent_id: "s1".into(),
    };
    super::track_background_lifecycle(spawn(), &mut pending, &mut completed);
    super::track_background_lifecycle(
        super::ExtEvent::SubagentFinished {
            subagent_id: "s1".into(),
        },
        &mut pending,
        &mut completed,
    );
    assert!(pending.is_empty());
    super::track_background_lifecycle(spawn(), &mut pending, &mut completed);
    assert!(
        pending.is_empty(),
        "a spawn for an already-finished subagent id must not re-arm pending"
    );
}

#[test]
fn reap_request_for_task_kills_with_session_scope() {
    let session_id = acp::SessionId::new("sess-1");
    let work = super::BackgroundWork::Task("task-42".into());
    let request = super::reap_request_for_work(&work, &session_id).unwrap();
    assert_eq!(request.method.as_ref(), "x.ai/task/kill");
    let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
    assert_eq!(params["sessionId"], "sess-1");
    assert_eq!(params["taskId"], "task-42");
}

/// A numeric `task_id` is coerced to its string form, tracked, and reaped on exit.
#[test]
fn numeric_task_id_is_decoded_tracked_and_reaped() {
    let payload = serde_json::json!({
        "sessionId": "sess-1",
        "update": { "sessionUpdate": "task_backgrounded", "task_id": 4242 },
    });
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    let notif = xai_acp_lib::AcpArgs {
        request: acp::ExtNotification::new("x.ai/task_backgrounded", raw.into()),
        response_tx: tx,
    }
    .boxed();
    let event = super::handle_ext_notification(&notif);
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    super::track_background_lifecycle(event, &mut pending, &mut completed);
    let work = super::BackgroundWork::Task("4242".into());
    assert!(
        pending.contains(&work),
        "numeric task_id tracked as the coerced string id"
    );
    let session_id = acp::SessionId::new("sess-1");
    let request = super::reap_request_for_work(&work, &session_id).unwrap();
    assert_eq!(request.method.as_ref(), "x.ai/task/kill");
    let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
    assert_eq!(params["taskId"], "4242");
    assert_eq!(params["sessionId"], "sess-1");
}

#[test]
fn reap_request_for_subagent_cancels_with_typed_id() {
    let session_id = acp::SessionId::new("sess-1");
    let work = super::BackgroundWork::Subagent("sub-7".into());
    let request = super::reap_request_for_work(&work, &session_id).unwrap();
    assert_eq!(request.method.as_ref(), "x.ai/subagent/cancel");
    let params: serde_json::Value = serde_json::from_str(request.params.get()).unwrap();
    assert_eq!(params["subagentId"], "sub-7");
}

/// A `task_backgrounded` delivered right at prompt completion is still recorded by the drain.
#[test]
fn drain_records_task_backgrounded_delivered_at_exit() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<xai_acp_lib::AcpClientMessage>();
    let payload = serde_json::json!({
        "sessionId": "sess-1",
        "update": { "sessionUpdate": "task_backgrounded", "task_id": "late-1" },
    });
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    let (resp_tx, _resp_rx) = tokio::sync::oneshot::channel();
    tx.send(xai_acp_lib::AcpClientMessage::ExtNotification(
        xai_acp_lib::AcpArgs {
            request: acp::ExtNotification::new("x.ai/task_backgrounded", raw.into()),
            response_tx: resp_tx,
        },
    ))
    .unwrap();

    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, false);
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    let mut ttf_logged = false;
    super::drain_pending_acp_messages(
        &mut rx,
        &mut emitter,
        std::time::Instant::now(),
        &mut ttf_logged,
        false,
        &mut pending,
        &mut completed,
    );
    assert!(
        pending.contains(&super::BackgroundWork::Task("late-1".into())),
        "drain-to-empty records a task_backgrounded buffered at exit"
    );
}

/// `begin_session` runs before the model and effort are applied, so a post-open error carries the real context.
#[test]
fn post_open_error_carries_real_session_context() {
    let mut pre = reducer_for(OutputFormat::StreamingMessagesJson).unwrap();
    let pre_lines = pre.error("boom", None, 0, None);
    let pre_result = pre_lines
        .iter()
        .find(|l| l["type"] == "result")
        .expect("result line");
    assert_eq!(
        pre_result["session_id"], "",
        "pre-session error keeps the startup-error fallback"
    );

    let mut post = reducer_for(OutputFormat::StreamingMessagesJson).unwrap();
    post.begin(SessionContext {
        session_id: "sess-real".into(),
        model: Some("grok-4".into()),
        cwd: "/work/dir".into(),
        permission_mode: None,
        mcp_servers: Vec::new(),
        include_partial_messages: false,
        api_key_auth: true,
        context_window: None,
    });
    let post_lines = post.error("boom", None, 0, None);
    let post_result = post_lines
        .iter()
        .find(|l| l["type"] == "result")
        .expect("result line");
    assert_eq!(
        post_result["session_id"], "sess-real",
        "post-open error carries the real session id"
    );
    let init = post_lines
        .iter()
        .find(|l| l["type"] == "system" && l["subtype"] == "init")
        .expect("system/init line");
    assert_eq!(init["session_id"], "sess-real");
    assert_eq!(init["cwd"], "/work/dir");
}

use super::*;
use xai_grok_workspace::permission::types::{RuleAction, ToolFilter};

fn s(v: &str) -> String {
    v.to_owned()
}

/// Headless materialization is never chat and carries the pre-sandbox pin flag through.
#[test]
fn headless_materialize_ctx_stays_non_chat() {
    use crate::app::session_startup::TitleResolution;
    for pinned in [false, true] {
        for restore_code in [false, true] {
            let ctx = headless_materialize_ctx(pinned, restore_code);
            assert!(!ctx.chat_mode);
            assert!(
                !ctx.has_worktree,
                "headless must not defer remote miss to a worktree it never creates"
            );
            assert_eq!(ctx.restore_code, restore_code);
            assert_eq!(
                ctx.title_resolution,
                if pinned {
                    TitleResolution::PinnedPreSandbox
                } else {
                    TitleResolution::Allowed
                }
            );
        }
    }
}

#[test]
fn headless_remote_miss_restores_conversation_instead_of_deferring_worktree() {
    use crate::app::session_startup::{RemoteMissPlan, plan_remote_miss};
    for restore_code in [false, true] {
        let ctx = headless_materialize_ctx(false, restore_code);
        assert!(!matches!(
            plan_remote_miss(ctx, true),
            RemoteMissPlan::DeferToWorktree { .. }
        ));
    }
    let mut conv = headless_materialize_ctx(false, false);
    conv.allow_remote_restore = true;
    assert_eq!(
        plan_remote_miss(conv, true),
        RemoteMissPlan::RestoreConversation
    );
    let mut code = headless_materialize_ctx(false, true);
    code.allow_remote_restore = true;
    assert_eq!(
        plan_remote_miss(code, true),
        RemoteMissPlan::RejectInPlaceCodeRestore {
            title_miss_hint: false,
        }
    );
}

#[test]
fn strict_valid_rules_parse_deny_before_allow() {
    let allow = vec![s("Bash(npm*)")];
    let deny = vec![s("Bash(rm*)"), s("Edit(/etc/**)")];
    let rules = parse_permission_rules_strict(&allow, &deny).unwrap();
    assert_eq!(rules.len(), 3);
    assert_eq!(rules[0].action, RuleAction::Deny);
    assert!(matches!(rules[0].tool, ToolFilter::Bash));
    assert_eq!(rules[1].action, RuleAction::Deny);
    assert!(matches!(rules[1].tool, ToolFilter::Edit));
    assert_eq!(rules[2].action, RuleAction::Allow);
    assert!(matches!(rules[2].tool, ToolFilter::Bash));
}

#[test]
fn strict_invalid_rule_errors() {
    let result = parse_permission_rules_strict(&[], &[s("EnterWorktree(foo)")]);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("--deny"));
    assert!(msg.contains("EnterWorktree"));
}

#[test]
fn strict_reports_all_invalid_rules() {
    let result = parse_permission_rules_strict(
        &[s("BadTool(x)")],
        &[s("EnterWorktree(foo)"), s("Bash(rm*)")],
    );
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("EnterWorktree"),
        "should mention first bad deny"
    );
    assert!(msg.contains("BadTool"), "should mention bad allow");
}

#[test]
fn lenient_skips_invalid_keeps_valid() {
    let allow = vec![s("Bash(npm*)")];
    let deny = vec![s("EnterWorktree(foo)"), s("Bash(rm*)")];
    let rules = parse_permission_rules_lenient(&allow, &deny);
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0].action, RuleAction::Deny);
    assert_eq!(rules[0].pattern.as_deref(), Some("rm*"));
    assert_eq!(rules[1].action, RuleAction::Allow);
    assert_eq!(rules[1].pattern.as_deref(), Some("npm*"));
}

#[test]
fn empty_inputs_produce_empty_rules() {
    let rules = parse_permission_rules_strict(&[], &[]).unwrap();
    assert!(rules.is_empty());
    let rules = parse_permission_rules_lenient(&[], &[]);
    assert!(rules.is_empty());
}

#[test]
fn domain_mode_web_fetch() {
    let rules = parse_permission_rules_strict(&[], &[s("WebFetch(domain:evil.com)")]).unwrap();
    assert_eq!(rules.len(), 1);
    assert!(matches!(rules[0].tool, ToolFilter::WebFetch));
    assert_eq!(
        rules[0].pattern_mode,
        xai_grok_workspace::permission::types::PatternMode::Domain
    );
    assert_eq!(rules[0].pattern.as_deref(), Some("evil.com"));
}

#[test]
fn bash_colon_wildcard_deny_translates_to_prefix() {
    let rules = parse_permission_rules_strict(&[], &[s("Bash(sed:*)")]).unwrap();
    assert_eq!(rules.len(), 1);
    assert!(matches!(rules[0].tool, ToolFilter::Bash));
    assert_eq!(rules[0].pattern.as_deref(), Some("sed"));
}

#[test]
fn structured_output_without_meta_errors_never_parses_text() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
    emitter.text_buffer = r#"{"name":"alice","age":30}"#.into();
    emitter.set_structured_output_from_meta(serde_json::json!({}).as_object());
    let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
    assert!(result["structuredOutput"].is_null());
    assert_eq!(
        result["structuredOutputError"],
        "model did not produce structured output"
    );
}

#[test]
fn structured_output_from_meta_wins_over_text_buffer() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
    emitter.text_buffer = "thinking out loud...".into();
    emitter.set_structured_output_from_meta(
        serde_json::json!({"structuredOutput": {"name": "carol"}}).as_object(),
    );
    let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
    assert_eq!(result["structuredOutput"]["name"], "carol");
    assert!(result.get("structuredOutputError").is_none());

    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, true);
    emitter.set_structured_output_from_meta(
        serde_json::json!({
            "structuredOutputError": "output does not match the required schema"
        })
        .as_object(),
    );
    let result = emitter.build_json_result("EndTurn", "sess-1", "req-1");
    assert!(result["structuredOutput"].is_null());
    assert_eq!(
        result["structuredOutputError"],
        "output does not match the required schema"
    );
}

#[test]
fn streaming_json_structured_output_emits_from_meta() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::StreamingJson, true);
    emitter.on_text_chunk(r#"{"name":"#);
    emitter.on_text_chunk(r#""bob"}"#);
    assert!(emitter.text_buffer.is_empty());

    emitter.set_structured_output_from_meta(
        serde_json::json!({"structuredOutput": {"name": "bob"}}).as_object(),
    );
    let mut target = serde_json::json!({});
    emitter.attach_structured_output(&mut target);
    assert_eq!(target["structuredOutput"]["name"], "bob");
    assert!(target.get("structuredOutputError").is_none());
}

#[test]
fn broken_pipe_write_is_a_clean_latched_stop() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::StreamingMessagesJson, false);
    let result = emitter.record_write_result(Err(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "pipe",
    )));
    assert!(result.is_ok(), "broken pipe is a clean stop");
    assert!(emitter.output_closed);
    assert!(emitter.take_output_error().is_none());
}

#[test]
fn hard_write_error_is_latched_and_surfaced_once() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::StreamingMessagesJson, false);
    let result = emitter.record_write_result(Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "denied",
    )));
    assert!(result.is_err(), "hard error is surfaced to the caller");
    assert!(emitter.output_closed);
    let latched = emitter.take_output_error().expect("hard error latched");
    assert_eq!(latched.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(
        emitter.take_output_error().is_none(),
        "taken once, then cleared"
    );
}

#[test]
fn first_hard_write_error_wins_the_latch() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::Json, false);
    let _ = emitter.record_write_result(Err(std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "first",
    )));
    let _ = emitter.record_write_result(Err(std::io::Error::other("second")));
    assert_eq!(
        emitter.take_output_error().map(|e| e.kind()),
        Some(std::io::ErrorKind::PermissionDenied)
    );
}

#[test]
fn successful_write_leaves_no_latched_error() {
    let mut emitter = HeadlessEmitter::new(OutputFormat::Plain, false);
    assert!(emitter.record_write_result(Ok(())).is_ok());
    assert!(!emitter.output_closed);
    assert!(emitter.take_output_error().is_none());
}

#[test]
fn parse_json_schema_rejects_non_objects_and_invalid_json() {
    assert!(super::parse_json_schema(r#"{"type":"object"}"#).is_ok());
    assert!(
        super::parse_json_schema(r#"[1,2,3]"#)
            .unwrap_err()
            .to_string()
            .contains("must be a JSON object")
    );
    assert!(
        super::parse_json_schema(r#"{not json"#)
            .unwrap_err()
            .to_string()
            .contains("invalid JSON")
    );
}

/// A `task_backgrounded` delivered right as the grace drain begins waiting is
/// still recorded: the drain polls the channel once (proving the notification
/// was not pre-queued), then processes it when it arrives mid-grace.
#[tokio::test(flavor = "current_thread")]
async fn grace_drain_captures_notification_sent_after_wait_begins() {
    use std::future::Future as _;
    use std::task::{Context, Poll, Waker};

    let (client_tx, mut client_rx) = tokio::sync::mpsc::unbounded_channel();
    assert!(matches!(
        client_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));

    let mut emitter = HeadlessEmitter::new(OutputFormat::Plain, false);
    let mut ttf_logged = false;
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    let mut drain = Box::pin(super::drain_acp_with_grace(
        &mut client_rx,
        std::time::Duration::from_secs(1),
        &mut emitter,
        std::time::Instant::now(),
        &mut ttf_logged,
        false,
        &mut pending,
        &mut completed,
    ));

    // Poll once while the channel is empty. This deterministically proves
    // the notification was not pre-queued: the drain is already waiting.
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    assert!(matches!(drain.as_mut().poll(&mut context), Poll::Pending));

    let payload = serde_json::json!({
        "sessionId": "sess-1",
        "update": {
            "sessionUpdate": "task_backgrounded",
            "task_id": "late-task",
        },
    });
    let raw = serde_json::value::to_raw_value(&payload).unwrap();
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    client_tx
        .send(xai_acp_lib::AcpClientMessage::ExtNotification(
            xai_acp_lib::AcpArgs {
                request: acp::ExtNotification::new("x.ai/task_backgrounded", raw.into()),
                response_tx,
            },
        ))
        .unwrap();
    // Closing the sender lets the drain return immediately after processing
    // the delayed notification rather than consuming wall-clock grace.
    drop(client_tx);

    drain.await;
    assert!(pending.contains(&super::BackgroundWork::Task("late-task".into())));
    assert!(response_rx.await.unwrap().is_ok());
}

#[test]
fn handler_answers_ext_method_instead_of_dropping() {
    use agent_client_protocol as acp;
    use xai_grok_tools::implementations::grok_build::ask_user_question::AskUserQuestionExtResponse;
    let raw = serde_json::value::to_raw_value(&serde_json::json!({})).unwrap();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    let msg = xai_acp_lib::AcpClientMessage::ExtMethod(xai_acp_lib::AcpArgs {
        request: acp::ExtRequest::new("x.ai/ask_user_question", raw.into()),
        response_tx: tx,
    });
    let mut emitter = super::HeadlessEmitter::new(super::OutputFormat::Json, false);
    let mut pending = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    let mut ttf_logged = false;
    super::handle_headless_acp_message(
        msg.boxed(),
        &mut emitter,
        std::time::Instant::now(),
        &mut ttf_logged,
        false,
        &mut pending,
        &mut completed,
    );
    let resp = rx
        .try_recv()
        .expect("ExtMethod must be answered, never dropped")
        .expect("policy reply, not an error");
    let parsed: AskUserQuestionExtResponse =
        serde_json::from_str(resp.0.get()).expect("typed wire reply");
    assert!(matches!(parsed, AskUserQuestionExtResponse::Cancelled));
}
