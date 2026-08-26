use super::*;

fn named_request(id: &str, name: &str) -> SubagentRequest {
    let mut request = request(id, true);
    request.runtime_overrides.native_agent = Some(NativeAgentSpawn {
        task_name: name.into(),
        ..Default::default()
    });
    request
}

fn native_message(
    identity: &AgentMailboxIdentity,
    target: &str,
    body: &str,
    followup: bool,
) -> AgentMailboxMessage {
    let mut message = mailbox_message(&uuid::Uuid::now_v7().to_string(), identity, target, body);
    message.kind = if followup {
        AgentMailboxMessageKind::NativeFollowup
    } else {
        AgentMailboxMessageKind::NativeMessage
    };
    message.native = Some(NativeAgentMessage {
        author: String::new(),
        recipient: String::new(),
        encrypted: true,
        trigger_prompt_id: Some("new-prompt".into()),
    });
    message
}

#[tokio::test]
async fn native_spawn_acknowledges_before_completion_and_names_are_unique() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        harness
            .backend
            .spawn(named_request("native-worker", "worker")),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.success && result.backgrounded);
    harness.requests.recv().await.unwrap();
    harness.started.recv().await.unwrap();
    let duplicate = harness
        .backend
        .spawn(named_request("duplicate-worker", "worker"))
        .await
        .unwrap();
    assert!(!duplicate.success);
    let root = mailbox_identity("parent", "parent");
    let roster = harness
        .backend
        .native_agent(
            root,
            NativeAgentOperation::List {
                path_prefix: Some("/root/worker".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(roster["agents"].as_array().unwrap().len(), 1);
    assert_eq!(roster["agents"][0]["task_name"], "/root/worker");
    assert_eq!(roster["agents"][0]["status"], "running");
    for name in ["root", "", "../escape", "MixedCase"] {
        assert!(
            !harness
                .backend
                .spawn(named_request(&uuid::Uuid::now_v7().to_string(), name))
                .await
                .unwrap()
                .success
        );
    }
    harness.actor.abort();
}

#[tokio::test]
async fn native_followup_reuses_named_context_and_passive_mail_does_not_wake() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let mut original = named_request("native-worker", "worker");
    original.runtime_overrides.model = Some("saved-model".into());
    original.cwd = Some("/saved/working-directory".into());
    harness.backend.spawn(original).await.unwrap();
    harness.requests.recv().await.unwrap();
    harness.started.recv().await.unwrap();
    let _ = harness.finish.send(());
    harness
        .backend
        .query("native-worker", true, Some(1_000))
        .await
        .unwrap();
    let root = mailbox_identity("parent", "parent");
    let message = native_message(&root, "/root/worker", "queued opaque content", false);
    harness
        .backend
        .native_agent(
            root.clone(),
            NativeAgentOperation::Message {
                target: "/root/worker".into(),
                message,
            },
        )
        .await
        .unwrap();
    assert!(harness.requests.try_recv().is_err());
    let message = native_message(&root, "/root/worker", "next opaque task", true);
    harness
        .backend
        .native_agent(
            root.clone(),
            NativeAgentOperation::Message {
                target: "/root/worker".into(),
                message,
            },
        )
        .await
        .unwrap();
    let resumed = harness.requests.recv().await.unwrap();
    assert_eq!(resumed.resume_from.as_deref(), Some("native-worker"));
    assert_eq!(resumed.parent_session_id, "parent");
    assert_eq!(resumed.parent_prompt_id.as_deref(), Some("new-prompt"));
    assert_eq!(
        resumed.runtime_overrides.model.as_deref(),
        Some("saved-model")
    );
    assert_eq!(resumed.cwd.as_deref(), Some("/saved/working-directory"));
    assert_eq!(resumed.subagent_type, "explore");
    assert_ne!(resumed.id, "native-worker");
    harness.started.recv().await.unwrap();
    for expected in ["queued opaque content", "next opaque task"] {
        let delivered = harness.followups.recv().await.unwrap();
        assert_eq!(delivered.body, expected);
        assert_eq!(delivered.kind, AgentMailboxMessageKind::NativeMessage);
        assert_eq!(delivered.native.unwrap().recipient, "/root/worker");
    }
    let roster = harness
        .backend
        .native_agent(
            root,
            NativeAgentOperation::List {
                path_prefix: Some("/root/worker".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(roster["agents"].as_array().unwrap().len(), 1);
    assert_eq!(roster["agents"][0]["agent_id"], resumed.id);
    harness.actor.abort();
}

#[tokio::test]
async fn native_wait_reports_activity_without_consuming_message_content() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    harness
        .backend
        .spawn(named_request("native-worker", "worker"))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    harness.started.recv().await.unwrap();
    let root = mailbox_identity("parent", "parent");
    let child = mailbox_identity("parent", "native-worker");
    let message = native_message(&child, "/root", "private opaque payload", false);
    harness
        .backend
        .native_agent(
            child,
            NativeAgentOperation::Message {
                target: "/root".into(),
                message,
            },
        )
        .await
        .unwrap();
    let result = harness
        .backend
        .native_agent(root.clone(), NativeAgentOperation::Wait { timeout_ms: 0 })
        .await
        .unwrap();
    assert_eq!(result["updates"], serde_json::json!(["/root/worker"]));
    assert!(!result.to_string().contains("private opaque payload"));
    assert_eq!(
        harness.followups.recv().await.unwrap().body,
        "private opaque payload"
    );
    let result = harness
        .backend
        .native_agent(root.clone(), NativeAgentOperation::Wait { timeout_ms: 0 })
        .await
        .unwrap();
    assert_eq!(result["timed_out"], true);
    let _ = harness.finish.send(());
    harness
        .backend
        .query("native-worker", true, Some(1_000))
        .await
        .unwrap();
    let result = harness
        .backend
        .native_agent(root, NativeAgentOperation::Wait { timeout_ms: 0 })
        .await
        .unwrap();
    assert_eq!(result["updates"], serde_json::json!(["/root/worker"]));
    harness.actor.abort();
}

#[tokio::test]
async fn native_interrupt_preserves_agent_and_enforces_team_and_self_boundaries() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    harness
        .backend
        .spawn(named_request("native-worker", "worker"))
        .await
        .unwrap();
    harness.requests.recv().await.unwrap();
    harness.started.recv().await.unwrap();
    let root = mailbox_identity("parent", "parent");
    let result = harness
        .backend
        .native_agent(
            root.clone(),
            NativeAgentOperation::Interrupt {
                target: "/root/worker".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result["previous_status"], "running");
    assert_eq!(
        harness
            .interruptions
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(matches!(
        harness
            .backend
            .query("native-worker", false, None)
            .await
            .unwrap()
            .status,
        SubagentSnapshotStatus::Running { .. }
    ));
    for (identity, target) in [
        (root, "/root"),
        (mailbox_identity("parent", "native-worker"), "/root/worker"),
        (mailbox_identity("foreign", "foreign"), "native-worker"),
    ] {
        assert!(
            harness
                .backend
                .native_agent(
                    identity,
                    NativeAgentOperation::Interrupt {
                        target: target.into()
                    }
                )
                .await
                .is_err()
        );
    }
    let bound = ChannelBackend::for_session(harness.backend.sender(), "different-caller");
    assert!(
        bound
            .native_agent(
                mailbox_identity("parent", "parent"),
                NativeAgentOperation::List { path_prefix: None }
            )
            .await
            .is_err()
    );
    assert_eq!(
        harness
            .interruptions
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    harness.actor.abort();
}

#[tokio::test]
async fn native_registry_reloads_names_and_resumes_through_the_original_owner() {
    let mut harness = harness(false, std::time::Duration::from_secs(60));
    let source_id = uuid::Uuid::now_v7().to_string();
    harness.registry.lock().unwrap().insert(
        "parent".into(),
        vec![NativeAgentRecord {
            task_name: "/root/worker".into(),
            agent_id: source_id.clone(),
            agent_type: "explore".into(),
            model: Some("saved-model".into()),
            reasoning_effort: Some("high".into()),
            service_tier: None,
            cwd: None,
            mailbox: Vec::new(),
        }],
    );
    let root = mailbox_identity("parent", "parent");
    let roster = harness
        .backend
        .native_agent(
            root.clone(),
            NativeAgentOperation::List { path_prefix: None },
        )
        .await
        .unwrap();
    assert!(
        roster["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["task_name"] == "/root/worker" && agent["status"] == "unloaded")
    );
    let message = native_message(&root, "/root/worker", "opaque resumed task", true);
    harness
        .backend
        .native_agent(
            root,
            NativeAgentOperation::Message {
                target: "/root/worker".into(),
                message,
            },
        )
        .await
        .unwrap();
    let resumed = harness.requests.recv().await.unwrap();
    assert_eq!(resumed.parent_session_id, "parent");
    assert_eq!(resumed.resume_from.as_deref(), Some(source_id.as_str()));
    assert_eq!(
        resumed.runtime_overrides.model.as_deref(),
        Some("saved-model")
    );
    assert_eq!(
        resumed.runtime_overrides.reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        resumed.runtime_overrides.native_agent.unwrap().task_name,
        "/root/worker"
    );
    harness.actor.abort();
}
