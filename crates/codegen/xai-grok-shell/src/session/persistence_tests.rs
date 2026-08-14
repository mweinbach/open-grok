use super::*;
use crate::session::storage::jsonl::AppendDurability;

struct ActorGuard {
    handle: PersistenceHandle,
    task: tokio::task::JoinHandle<()>,
}

impl ActorGuard {
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

fn test_actor(info: Info, storage: Arc<dyn StorageAdapter>) -> ActorGuard {
    test_actor_with_remote_sync(info, storage, None)
}

fn test_actor_inner(
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    remote_sync: Option<RemoteSync>,
    mark_summary_done: bool,
) -> ActorGuard {
    let (tx, rx) = mpsc::unbounded_channel();
    let summary_tx = tx.downgrade();
    let provider_boundary = ProviderBoundary::default();
    let (disk_full_tx, disk_full_rx) = tokio::sync::watch::channel(false);
    let sampling_client = OaiCompatClient::new(xai_grok_sampler::SamplerConfig::default()).unwrap();
    let mut summary =
        crate::session::summary::SummaryGenerator::new(crate::session::summary::SummaryConfig {
            sampling_client,
            model: String::new(),
            persistence_tx: summary_tx,
        });
    if mark_summary_done {
        summary.mark_done();
    }
    let task = tokio::spawn(
        SessionPersistence {
            info,
            storage,
            pending_notification: None,
            rx,
            provider_boundary: provider_boundary.clone(),
            current_provider: ModelProvider::Xai,
            remote_sync,
            // Resumed-style actor for these tests; upgrade backfill is fresh-only.
            created_fresh: false,
            relay_sync: None,
            summary,
            registry_title_sync: None,
            gateway: None,
            disk_full_tx,
            disk_full_notified: false,
        }
        .run(),
    );
    ActorGuard {
        handle: PersistenceHandle {
            tx,
            provider_boundary,
            noop: false,
            disk_full_rx,
        },
        task,
    }
}

fn test_actor_with_remote_sync(
    info: Info,
    storage: Arc<dyn StorageAdapter>,
    remote_sync: Option<RemoteSync>,
) -> ActorGuard {
    test_actor_inner(info, storage, remote_sync, false)
}

fn notification(info: &Info, text: &str) -> acp::SessionNotification {
    acp::SessionNotification::new(
        info.id.clone(),
        acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(acp::ContentBlock::Text(
            acp::TextContent::new(text),
        ))),
    )
}

fn neutral_update(info: &Info, text: &str) -> SessionUpdate {
    SessionUpdate::Acp(Box::new(notification(info, text)))
}

#[tokio::test]
async fn writeback_backfill_is_fresh_only_and_acp_only() {
    let info = Info {
        id: acp::SessionId::new("wb-backfill"),
        cwd: "/test".into(),
    };

    // Fresh session: every ACP update is queued to the writeback sync.
    let (sync, mut observed) = RemoteSync::test_observer();
    let updates = vec![neutral_update(&info, "a"), neutral_update(&info, "b")];
    let n = backfill_updates_to_sync(true, updates, &sync);
    assert_eq!(n, 2, "a fresh session backfills its full local ACP history");
    for _ in 0..2 {
        tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
            .await
            .expect("backfilled notification not observed within 1s")
            .expect("observer channel closed unexpectedly");
    }

    // Resumed session: nothing is backfilled (prior history may already be synced).
    let (sync2, mut observed2) = RemoteSync::test_observer();
    let n2 = backfill_updates_to_sync(false, vec![neutral_update(&info, "a")], &sync2);
    assert_eq!(n2, 0, "a resumed session is forward-only, no backfill");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), observed2.recv())
            .await
            .is_err(),
        "resumed session must not re-send any prior history",
    );
}

fn break_summary_writes(dir: &std::path::Path) {
    let summary = dir.join("summary.json");
    std::fs::remove_file(&summary).unwrap();
    std::fs::create_dir(summary).unwrap();
}

async fn recv_observed(
    observed: &mut tokio::sync::mpsc::UnboundedReceiver<acp::SessionNotification>,
) -> acp::SessionNotification {
    tokio::time::timeout(std::time::Duration::from_secs(1), observed.recv())
        .await
        .expect("remote sync timed out")
        .expect("remote sync observer closed")
}

#[test]
fn committed_error_returns_sync_disposition() {
    let info = Info {
        id: acp::SessionId::new("committed-update"),
        cwd: "/test".into(),
    };
    let notification = notification(&info, "committed");
    let PendingAppendOutcome::CommittedErr(sync_notification, error) =
        SessionPersistence::finish_pending_append(
            notification,
            Err(crate::session::storage::AppendUpdateError::Committed(
                io::Error::other("summary patch failed"),
            )),
        )
    else {
        panic!("expected committed failure");
    };
    assert_eq!(sync_notification.session_id, info.id);
    assert_eq!(error.to_string(), "summary patch failed");
}

#[test]
fn uncommitted_error_returns_restore_disposition() {
    let info = Info {
        id: acp::SessionId::new("uncommitted-update"),
        cwd: "/test".into(),
    };
    let notification = notification(&info, "pending");
    let PendingAppendOutcome::NotCommittedErr(pending_notification, error) =
        SessionPersistence::finish_pending_append(
            notification,
            Err(crate::session::storage::AppendUpdateError::NotCommitted(
                io::Error::other("append failed"),
            )),
        )
    else {
        panic!("expected uncommitted failure");
    };
    assert_eq!(pending_notification.session_id, info.id);
    assert_eq!(error.to_string(), "append failed");
}

#[tokio::test]
async fn noop_handle_rejects_durable_append() {
    let info = Info {
        id: acp::SessionId::new("noop-durable-update"),
        cwd: "/test".into(),
    };
    assert!(matches!(
        PersistenceHandle::noop()
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::NotCommitted(error))
            if error.kind() == io::ErrorKind::Unsupported
    ));
}

#[tokio::test]
async fn pending_drain_disposition_controls_remote_sync() {
    let info = Info {
        id: acp::SessionId::new("pending-remote-sync"),
        cwd: "/test".into(),
    };
    let storage = JsonlStorageAdapter::with_update_append_probe("/unused".into(), |_| {
        Err(io::Error::other("append failed"))
    });
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), Arc::new(storage), Some(remote_sync));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::NotCommitted(_))
    ));
    assert!(observed.try_recv().is_err());
    actor.stop().await;

    let dir = tempfile::tempdir().unwrap();
    let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_attempts = attempts.clone();
    let storage = Arc::new(JsonlStorageAdapter::with_update_append_probe(
        dir.path().to_path_buf(),
        move |durability| {
            observed_attempts.lock().unwrap().push(durability);
            Ok(())
        },
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    break_summary_writes(dir.path());
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::Committed(_))
    ));
    let synced = recv_observed(&mut observed).await;
    assert_eq!(synced.session_id, info.id);
    assert!(matches!(
        attempts.lock().unwrap().as_slice(),
        [AppendDurability::Buffered]
    ));
    actor.stop().await;
}

#[tokio::test]
async fn durable_append_committed_failure_is_synced() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("durable-remote-sync"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    break_summary_writes(dir.path());
    let (remote_sync, mut observed) = RemoteSync::test_observer();
    let actor = test_actor_with_remote_sync(info.clone(), storage, Some(remote_sync));
    assert!(matches!(
        actor
            .handle
            .append_update_durably(neutral_update(&info, "durable"))
            .await,
        Err(DurableAppendError::Committed(_))
    ));
    let synced = recv_observed(&mut observed).await;
    assert_eq!(synced.session_id, info.id);
    actor.stop().await;
}

#[tokio::test]
async fn failed_pending_drain_retains_record_and_skips_durable_update() {
    let info = Info {
        id: acp::SessionId::new("durable-drain-failure"),
        cwd: "/test".into(),
    };
    let attempts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = attempts.clone();
    let storage =
        JsonlStorageAdapter::with_update_append_probe("/unused".into(), move |durability| {
            observed.lock().unwrap().push(durability);
            Err(io::Error::other("pending append failed"))
        });
    let actor = test_actor(info.clone(), Arc::new(storage));
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "pending")))
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            actor
                .handle
                .append_update_durably(neutral_update(&info, "durable"))
                .await
                .unwrap_err()
                .to_string(),
            "pending append failed"
        );
    }
    assert!(matches!(
        attempts.lock().unwrap().as_slice(),
        [AppendDurability::Buffered, AppendDurability::Buffered]
    ));
    actor.stop().await;
}

#[tokio::test]
async fn durable_append_drains_pending_update_in_fifo_order() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("durable-update"),
        cwd: dir.path().to_string_lossy().into_owned(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage.clone());
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "before")))
        .unwrap();
    actor
        .handle
        .append_update_durably(neutral_update(&info, "durable"))
        .await
        .unwrap();
    let summary = storage.load_summary(&info).await.unwrap();
    assert_eq!(summary.num_messages, 2);

    let updates = storage.load_session(&info).await.unwrap().updates;
    let texts = updates
        .iter()
        .filter_map(|update| {
            let SessionUpdate::Acp(notification) = update else {
                return None;
            };
            let acp::SessionUpdate::AgentMessageChunk(chunk) = &notification.update else {
                return None;
            };
            let acp::ContentBlock::Text(text) = &chunk.content else {
                return None;
            };
            Some(text.text.clone())
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["before", "durable"]);
    actor.stop().await;
}

async fn flush_ack(handle: &PersistenceHandle) -> io::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(PersistenceMsg::FlushAndAck { respond_to: tx })
        .unwrap();
    rx.await.unwrap()
}

async fn probe_writable(handle: &PersistenceHandle) -> io::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .tx
        .send(PersistenceMsg::ProbeWritable { respond_to: tx })
        .unwrap();
    rx.await.unwrap()
}

#[derive(Debug)]
struct CapturedTitle {
    title: String,
    title_present: bool,
    title_is_manual: Option<bool>,
}

fn save_session_titles(
    server: &xai_grok_test_support::MockInferenceServer,
    session_id: &str,
) -> Vec<CapturedTitle> {
    let suffix = format!("/sessions/{session_id}/data");
    server
        .received_requests()
        .into_iter()
        .filter(|req| req.path.ends_with(&suffix))
        .filter_map(|req| {
            let body = req.body.as_ref()?;
            let meta = body.get("metadata")?;
            let title = meta.get("title").and_then(|t| t.as_str());
            let title_is_manual = meta.get("title_is_manual").and_then(|m| m.as_bool());
            Some(CapturedTitle {
                title: title.unwrap_or_default().to_string(),
                title_present: title.is_some(),
                title_is_manual,
            })
        })
        .collect()
}

/// Auto → manual → unpin: generator starts Done (as production `load` would),
/// a ContentChunk before reset must not spawn, ResetTitleToAuto must
/// `reset()` + clear the remote pin, and a later ContentChunk must adopt
/// via the fallback (empty model, no live LLM).
#[tokio::test]
async fn reset_title_to_auto_then_generated_title_is_adopted() {
    use std::sync::Arc;

    use crate::auth::{AuthManager, GrokAuth};
    use crate::remote::BackendClient;
    use crate::session::export::ExportedMetadata;
    use crate::session::helpers::session_summary::title_fallback_from_user_text;
    use crate::session::persistence::PersistenceContentChunk;
    use xai_grok_test_support::MockInferenceServer;

    const AUTO: &str = "Auto first title";
    const MANUAL: &str = "Pinned manual";
    const CHUNK: &str = "fresh auto title from next chunk please";
    const SESSION_ID: &str = "rename-reset-to-auto";
    let expected_fresh = title_fallback_from_user_text(CHUNK);

    let server = MockInferenceServer::start()
        .await
        .expect("start MockInferenceServer");
    let home = tempfile::tempdir().unwrap();
    let auth = Arc::new(AuthManager::new(
        home.path(),
        crate::auth::GrokComConfig::default(),
    ));
    auth.hot_swap(GrokAuth {
        key: "writeback-test-token".into(),
        ..GrokAuth::test_default()
    });

    let info = Info {
        id: acp::SessionId::new(SESSION_ID),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    assert!(
        storage
            .set_generated_title_if_absent(&info, AUTO.to_owned())
            .await
            .unwrap()
    );
    storage
        .update_session_title(&info, MANUAL.to_owned())
        .await
        .unwrap();
    assert!(
        !storage
            .set_generated_title_if_absent(&info, "Rejected auto".into())
            .await
            .unwrap(),
        "manual pin must reject auto title before reset"
    );

    let metadata = ExportedMetadata {
        title: Some(MANUAL.into()),
        cwd: info.cwd.clone(),
        model_id: Some("test-model".into()),
        created_at: None,
        updated_at: None,
        total_messages: None,
        parent_session_id: None,
        session_kind: None,
        subagent_type: None,
        subagent_persona: None,
        subagent_role: None,
        fork_context_source: None,
        subagent_depth: None,
        title_is_manual: Some(true),
    };
    let client = BackendClient::with_base_url(server.origin()).with_auth_manager(auth);
    let remote_sync = RemoteSync::new(SESSION_ID.to_owned(), metadata, client);
    let actor = test_actor_inner(
        info.clone(),
        storage.clone(),
        Some(remote_sync),
        true, /* mark_summary_done: production load after a titled session */
    );

    let pre_reset_chunk = PersistenceContentChunk::new(vec![acp::ContentBlock::Text(
        acp::TextContent::new("should not generate while still manual and Done"),
    )]);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ContentChunk(pre_reset_chunk))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();
    let summary_path = dir.path().join("summary.json");
    let still_manual: crate::session::persistence::Summary =
        serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
    assert_eq!(still_manual.display_title(), MANUAL);
    assert!(still_manual.title_is_manual);
    assert!(
        save_session_titles(&server, SESSION_ID)
            .iter()
            .filter(|t| t.title_present)
            .all(|t| t.title == MANUAL),
        "Done generator must not adopt an in-flight title before reset"
    );

    assert!(storage.reset_title_to_auto(&info).await.unwrap());
    actor
        .handle
        .tx
        .send(PersistenceMsg::ResetTitleToAuto)
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "after reset")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let titles = save_session_titles(&server, SESSION_ID);
        if titles
            .iter()
            .any(|t| t.title_present && t.title.is_empty() && t.title_is_manual == Some(false))
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "unpin must POST title:\"\" and title_is_manual:false (merge backends keep a prior true if omitted): {titles:?}"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let post_reset: crate::session::persistence::Summary =
        serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
    assert!(
        post_reset.display_title().trim().is_empty(),
        "display_title must be blank so if-absent can adopt"
    );

    let post_reset_chunk =
        PersistenceContentChunk::new(vec![acp::ContentBlock::Text(acp::TextContent::new(CHUNK))]);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ContentChunk(post_reset_chunk))
        .unwrap();
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "after auto")))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let on_disk = loop {
        let on_disk: crate::session::persistence::Summary =
            serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
        if on_disk.display_title() == expected_fresh && !on_disk.title_is_manual {
            break on_disk;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "ContentChunk after reset never adopted fallback {expected_fresh:?}; display={:?}",
                on_disk.display_title()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(on_disk.display_title(), expected_fresh);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let titles = save_session_titles(&server, SESSION_ID);
        if titles
            .iter()
            .any(|t| t.title == expected_fresh && t.title_is_manual.is_none())
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("adopted auto title after reset never reached save_session_data: {titles:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    actor.stop().await;
}

/// In-flight first-chunk generation overlapping unpin: disk is already blank
/// when the stale `GeneratedTitle` arrives, so if-absent adopts it as auto
/// (never re-pins).
#[tokio::test]
async fn reset_title_to_auto_adopts_in_flight_generation_as_auto() {
    use std::sync::Arc;

    use crate::session::persistence::PersistenceContentChunk;

    const MANUAL: &str = "Pinned manual";
    const CHUNK: &str = "in flight chunk text for title fallback";

    let info = Info {
        id: acp::SessionId::new("rename-reset-inflight"),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    storage
        .update_session_title(&info, MANUAL.to_owned())
        .await
        .unwrap();

    let actor = test_actor_inner(info.clone(), storage.clone(), None, false);

    actor
        .handle
        .tx
        .send(PersistenceMsg::ContentChunk(PersistenceContentChunk::new(
            vec![acp::ContentBlock::Text(acp::TextContent::new(CHUNK))],
        )))
        .unwrap();
    assert!(storage.reset_title_to_auto(&info).await.unwrap());
    actor
        .handle
        .tx
        .send(PersistenceMsg::ResetTitleToAuto)
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let summary_path = dir.path().join("summary.json");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let on_disk = loop {
        let on_disk: crate::session::persistence::Summary =
            serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
        if !on_disk.display_title().trim().is_empty() {
            break on_disk;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("in-flight GeneratedTitle after unpin never adopted");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_ne!(on_disk.display_title(), MANUAL);
    assert!(
        !on_disk.title_is_manual,
        "in-flight adopt after unpin must stay auto"
    );
    actor.stop().await;
}

/// Non-resident unpin only patches disk. The next load sees a blank
/// `display_title()` so the generator stays Idle and a ContentChunk adopts.
#[tokio::test]
async fn non_resident_reset_then_load_regenerates() {
    use std::sync::Arc;

    use crate::session::helpers::session_summary::title_fallback_from_user_text;
    use crate::session::persistence::PersistenceContentChunk;

    const CHUNK: &str = "dormant session next turn title text";
    let expected = title_fallback_from_user_text(CHUNK);

    let info = Info {
        id: acp::SessionId::new("rename-reset-dormant-load"),
        cwd: "/test".into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    assert!(
        storage
            .set_generated_title_if_absent(&info, "Auto Title".into())
            .await
            .unwrap()
    );
    storage
        .update_session_title(&info, "Manual Title".into())
        .await
        .unwrap();
    assert!(storage.reset_title_to_auto(&info).await.unwrap());

    let summary_path = dir.path().join("summary.json");
    let after_reset: crate::session::persistence::Summary =
        serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
    let has_title = !after_reset.display_title().is_empty();
    assert!(
        !has_title,
        "production load would mark_done() if display_title stayed set"
    );

    let actor = test_actor_inner(info.clone(), storage, None, has_title);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ContentChunk(PersistenceContentChunk::new(
            vec![acp::ContentBlock::Text(acp::TextContent::new(CHUNK))],
        )))
        .unwrap();
    flush_ack(&actor.handle).await.unwrap();

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    let on_disk = loop {
        let on_disk: crate::session::persistence::Summary =
            serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
        if on_disk.display_title() == expected && !on_disk.title_is_manual {
            break on_disk;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "load after non-resident unpin never adopted {expected:?}; display={:?}",
                on_disk.display_title()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(on_disk.display_title(), expected);
    actor.stop().await;
}

#[tokio::test]
async fn reset_title_to_auto_is_noop_without_remote_sync() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("reset-local-only"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_explicit_session_dir(
        dir.path().to_path_buf(),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    storage
        .update_session_title(&info, "Manual".into())
        .await
        .unwrap();
    storage.reset_title_to_auto(&info).await.unwrap();
    let actor = test_actor(info.clone(), storage);
    actor
        .handle
        .tx
        .send(PersistenceMsg::ResetTitleToAuto)
        .unwrap();
    flush_ack(&actor.handle)
        .await
        .expect("ResetTitleToAuto with remote_sync=None must not fail");
    actor.stop().await;
}

#[tokio::test]
async fn successful_append_clears_disk_full_latch() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("disk-full-clear"),
        cwd: "/test".into(),
    };
    let fail = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let fail_flag = fail.clone();
    let storage = Arc::new(JsonlStorageAdapter::with_update_append_probe(
        dir.path().to_path_buf(),
        move |_| {
            if fail_flag.load(std::sync::atomic::Ordering::SeqCst) {
                Err(io::Error::from(io::ErrorKind::StorageFull))
            } else {
                Ok(())
            }
        },
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "chunk")))
        .unwrap();
    assert!(flush_ack(&actor.handle).await.is_err());
    assert!(actor.handle.is_disk_full());

    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "recovered")))
        .unwrap();
    assert!(flush_ack(&actor.handle).await.is_ok());
    assert!(!actor.handle.is_disk_full());
    actor.stop().await;
}

#[tokio::test]
async fn successful_probe_writable_clears_disk_full_latch() {
    let dir = tempfile::tempdir().unwrap();
    let info = Info {
        id: acp::SessionId::new("disk-full-probe"),
        cwd: "/test".into(),
    };
    let storage = Arc::new(JsonlStorageAdapter::with_update_append_probe(
        dir.path().to_path_buf(),
        |_| Err(io::Error::from(io::ErrorKind::StorageFull)),
    ));
    storage
        .init_session(&info, default_model_id())
        .await
        .unwrap();
    let actor = test_actor(info.clone(), storage);
    actor
        .handle
        .tx
        .send(PersistenceMsg::Update(neutral_update(&info, "chunk")))
        .unwrap();
    assert!(flush_ack(&actor.handle).await.is_err());
    assert!(actor.handle.is_disk_full());

    assert!(probe_writable(&actor.handle).await.is_ok());
    assert!(!actor.handle.is_disk_full());
    actor.stop().await;
}

#[cfg(unix)]
mod prompt_file_tests {
    use super::*;
    use crate::test_support::unix_mode;

    #[test]
    fn prompt_file_dir_chain_is_owner_only() {
        let home = tempfile::TempDir::new().unwrap();
        let info = Info {
            id: agent_client_protocol::SessionId::new("prompt-perm-test"),
            cwd: "/some/project".to_string(),
        };

        let path = get_prompt_file_path_in(home.path(), &info, 0);

        // The chain below prompts/ is ensure_owner_only_session_dir_in's job,
        // pinned by ensure_owner_only_session_dir_tightens_chain — only the
        // prompts/ level is this path's own creation.
        let prompts_dir = path.parent().unwrap();
        assert_eq!(unix_mode(prompts_dir), 0o700, "prompts dir must be 0700");
    }

    /// The chat-kind (noop-persistence) writers' dir creator.
    #[test]
    fn ensure_owner_only_session_dir_tightens_chain() {
        let home = tempfile::TempDir::new().unwrap();
        let info = Info {
            id: agent_client_protocol::SessionId::new("chat-kind-perm-test"),
            cwd: "/some/project".to_string(),
        };

        let dir = ensure_owner_only_session_dir_in(home.path(), &info).unwrap();

        assert_eq!(unix_mode(&dir), 0o700, "session dir must be 0700");
        assert_eq!(
            unix_mode(dir.parent().unwrap()),
            0o700,
            "<encoded-cwd> dir must be 0700"
        );
        assert_eq!(
            unix_mode(&home.path().join("sessions")),
            0o700,
            "sessions root must be 0700"
        );
    }
}
