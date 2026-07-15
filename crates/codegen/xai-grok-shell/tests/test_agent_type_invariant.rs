//! Agent-type invariant integration tests.
//!
//! These tests exercise the full shell lifecycle via ACP stdio against a mock
//! inference server, verifying that `agent_type = f(model)` holds across:
//!
//! - Session creation with default models
//! - Zero-turn model switching (harness rebuild)
//! - Mid-session model switching (rejection)
//! - Same-type model switching (no rebuild)
//! - Session resume
//!
//! Each test spawns a real `grok agent stdio` process, speaks the full ACP
//! protocol, and asserts on the inference request bodies (system prompt) and
//! stderr tracing output to verify the correct harness was used.
//!
//! Run locally:
//! ```bash
//! cargo test -p xai-grok-shell --test test_agent_type_invariant -- --ignored
//! ```
use agent_client_protocol::Agent as _;
use std::future::Future;
use std::time::Duration;
use xai_grok_test_support::*;
async fn with_local_set<F, Fut>(f: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    tokio::task::LocalSet::new().run_until(f()).await;
}
/// Delete the shell's on-disk models cache so the next process is forced to
/// re-fetch from the mock server's `/v1/models` endpoint. Without this, the
/// second spawn in a resume test reads the stale cache written by phase 1
/// and never sees the updated model list.
fn invalidate_models_cache(home: &std::path::Path) {
    let cache = home.join(".grok").join("models_cache.json");
    if cache.exists() {
        std::fs::remove_file(&cache).expect("failed to delete models_cache.json");
    }
}
/// Start a mock server with two models:
/// - `default-model`: no agent_type (→ defaults to "grok-build")
async fn dual_model_server() -> MockInferenceServer {
    MockInferenceServer::start_with_models(vec![
        MockModelEntry::new("default-model"),
        MockModelEntry::with_agent_type("cursor-model", "cursor"),
    ])
    .await
    .expect("start mock server")
}
/// Start a mock server with two models that share the same agent_type:
/// - `model-a`: no agent_type (→ "grok-build")
/// - `model-b`: no agent_type (→ "grok-build")
async fn same_type_server() -> MockInferenceServer {
    MockInferenceServer::start_with_models(vec![
        MockModelEntry::new("model-a"),
        MockModelEntry::new("model-b"),
    ])
    .await
    .expect("start mock server")
}
/// Session created with a model that has no `agent_type` should use the
/// `grok-build` harness. The system prompt sent to the LLM should contain
/// the grok-build identity string.
#[tokio::test]
#[ignore]
async fn test_default_model_uses_grok_build_harness() {
    with_local_set(|| async {
        let server = MockInferenceServer::start()
            .await
            .expect("start mock server");
        let workdir = git_workdir();
        let client = GrokStdioClient::spawn(&server, workdir.path()).await;
        client.initialize_with_timeout().await;
        let session_id = client.create_session_with_timeout(workdir.path()).await;
        let result = client.prompt_with_timeout(&session_id, "say hello").await;
        assert!(result.is_ok(), "prompt failed: {:?}", result.err());
        let sys_prompt = server
            .last_system_prompt()
            .expect("should have at least one inference request");
        assert!(
            sys_prompt.contains("Grok") || sys_prompt.contains("grok"),
            "default model should use grok-build harness\nsystem prompt preview: {}",
            &sys_prompt[..sys_prompt.len().min(500)]
        );
    })
    .await;
}
/// Switching between two models with the same agent_type should succeed
/// without a harness rebuild.
#[tokio::test]
#[ignore]
async fn test_same_type_model_switch_no_rebuild() {
    with_local_set(|| async {
        let server = same_type_server().await;
        let workdir = git_workdir();
        let client = GrokStdioClient::spawn(&server, workdir.path()).await;
        client.initialize_with_timeout().await;
        let session_id = client
            .create_session_with_model_timeout(workdir.path(), "model-a")
            .await;
        let result = client.prompt_with_timeout(&session_id, "say hello").await;
        assert!(result.is_ok(), "first prompt failed: {:?}", result.err());
        let switch_result = client.set_model_with_timeout(&session_id, "model-b").await;
        assert!(
            switch_result.is_ok(),
            "same-type model switch should succeed\nerror: {:?}\nstderr: {}",
            switch_result.err(),
            stderr_tail(&client.stderr(), 2000)
        );
        let result2 = client.prompt_with_timeout(&session_id, "say goodbye").await;
        assert!(
            result2.is_ok(),
            "second prompt after model switch failed: {:?}",
            result2.err()
        );
    })
    .await;
}
/// A session created with the default model, persisted, and reloaded should
/// still use the same harness — the system prompt in the resumed session's
/// first inference request should match the original.
#[tokio::test]
#[ignore]
async fn test_session_resume_preserves_harness() {
    with_local_set(|| async {
        let server = MockInferenceServer::start()
            .await
            .expect("start mock server");
        let workdir = git_workdir();
        let mut writer = GrokStdioClient::spawn(&server, workdir.path()).await;
        writer.initialize_with_timeout().await;
        let session_id = writer.create_session_with_timeout(workdir.path()).await;
        let result = writer.prompt_with_timeout(&session_id, "say hello").await;
        assert!(result.is_ok(), "prompt failed: {:?}", result.err());
        let original_sys_prompt = server
            .last_system_prompt()
            .expect("should have captured system prompt");
        let shared_home = writer.take_home();
        invalidate_models_cache(shared_home.path());
        drop(writer);
        let reader = GrokStdioClient::spawn_with_home(&server, workdir.path(), shared_home).await;
        reader.initialize_with_timeout().await;
        let _ = reader
            .load_session_with_timeout(&session_id, workdir.path())
            .await;
        let result2 = reader.prompt_with_timeout(&session_id, "say goodbye").await;
        assert!(
            result2.is_ok(),
            "resumed prompt failed: {:?}",
            result2.err()
        );
        let resumed_sys_prompt = server
            .last_system_prompt()
            .expect("should have captured resumed system prompt");
        let original_has_grok =
            original_sys_prompt.contains("Grok") || original_sys_prompt.contains("grok");
        let resumed_has_grok =
            resumed_sys_prompt.contains("Grok") || resumed_sys_prompt.contains("grok");
        assert_eq!(
            original_has_grok,
            resumed_has_grok,
            "resumed session should use the same harness as the original\n\
             original identity markers: grok={original_has_grok}\n\
             resumed identity markers: grok={resumed_has_grok}\n\
             original prompt (first 300): {}\n\
             resumed prompt (first 300): {}",
            &original_sys_prompt[..original_sys_prompt.len().min(300)],
            &resumed_sys_prompt[..resumed_sys_prompt.len().min(300)],
        );
    })
    .await;
}
/// A model that doesn't declare `agent_type` in its metadata should
/// default to `"grok-build"`. This exercises the serde default.
#[tokio::test]
#[ignore]
async fn test_model_without_agent_type_defaults_to_grok_build() {
    with_local_set(|| async {
            let server = MockInferenceServer::start_with_models(
                    vec![MockModelEntry::new("no-agent-type-model"),],
                )
                .await
                .expect("start mock server");
            let workdir = git_workdir();
            let client = GrokStdioClient::spawn(&server, workdir.path()).await;
            client.initialize_with_timeout().await;
            let session_id = client
                .create_session_with_model_timeout(workdir.path(), "no-agent-type-model")
                .await;
            let result = client.prompt_with_timeout(&session_id, "say hello").await;
            assert!(result.is_ok(), "prompt failed: {:?}", result.err());
            let sys_prompt = server
                .last_system_prompt()
                .expect("should have at least one inference request");
            assert!(
                sys_prompt.contains("Grok") || sys_prompt.contains("grok"),
                "model without agent_type should default to grok-build harness\nsystem prompt preview: {}",
                & sys_prompt[..sys_prompt.len().min(500)]
            );
        })
        .await;
}
/// The `GROK_AGENT` escape hatch should override the model's agent_type.
/// Setting `GROK_AGENT=grok-build` with an alternate-agent model should use
/// grok-build harness.
#[tokio::test]
#[ignore]
async fn test_grok_agent_env_overrides_model_agent_type() {
    with_local_set(|| async {
            let server = dual_model_server().await;
            let workdir = git_workdir();
            let binary = grok_binary();
            let home = tempfile::TempDir::new().expect("create temp home");
            let mut cmd = tokio::process::Command::new(&binary);
            cmd.args(["agent", "stdio"])
                .current_dir(workdir.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            xai_grok_test_support::env::test_env_cmd_tokio(
                &mut cmd,
                &server.url(),
                home.path(),
            );
            cmd.env("GROK_AGENT", "grok-build");
            let mut child = cmd.spawn().expect("spawn grok");
            let outgoing = child.stdin.take().unwrap();
            let incoming = child.stdout.take().unwrap();
            use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
            let outgoing = outgoing.compat_write();
            let incoming = incoming.compat();
            let incoming = xai_acp_lib::LineBufferedRead::spawn_local(incoming);
            use agent_client_protocol as acp;
            struct NoopClient;
            #[async_trait::async_trait(?Send)]
            impl acp::Client for NoopClient {
                async fn request_permission(
                    &self,
                    args: acp::RequestPermissionRequest,
                ) -> acp::Result<acp::RequestPermissionResponse> {
                    let outcome = args
                        .options
                        .iter()
                        .find(|o| o.kind == acp::PermissionOptionKind::AllowOnce)
                        .or(args.options.first())
                        .map(|o| acp::RequestPermissionOutcome::Selected(
                            acp::SelectedPermissionOutcome::new(o.option_id.clone()),
                        ))
                        .unwrap_or(acp::RequestPermissionOutcome::Cancelled);
                    Ok(acp::RequestPermissionResponse::new(outcome))
                }
                async fn session_notification(
                    &self,
                    _args: acp::SessionNotification,
                ) -> acp::Result<()> {
                    Ok(())
                }
            }
            let (conn, handle_io) = acp::ClientSideConnection::new(
                NoopClient,
                outgoing,
                incoming,
                |fut| {
                    tokio::task::spawn_local(fut);
                },
            );
            tokio::task::spawn_local(handle_io);
            let _init = tokio::time::timeout(
                    Duration::from_secs(20),
                    conn
                        .initialize(
                            acp::InitializeRequest::new(acp::ProtocolVersion::V1)
                                .client_capabilities(
                                    acp::ClientCapabilities::new()
                                        .fs(acp::FileSystemCapabilities::new())
                                        .terminal(false),
                                )
                                .meta(
                                    serde_json::json!(
                                        { "startupHints" : { "nonInteractive" : true,
                                        "skipGitStatus" : true, "skipProjectLayout" : true },
                                        "clientType" : "test-client", "clientVersion" : "0.0.0-test"
                                        }
                                    )
                                        .as_object()
                                        .cloned(),
                                ),
                        ),
                )
                .await
                .expect("init timed out")
                .expect("init failed");
            conn.authenticate(
                    acp::AuthenticateRequest::new(acp::AuthMethodId::new("xai.api_key"))
                        .meta(
                            serde_json::json!({ "headless" : true }).as_object().cloned(),
                        ),
                )
                .await
                .expect("auth failed");
            let session = tokio::time::timeout(
                    Duration::from_secs(20),
                    conn
                        .new_session(
                            acp::NewSessionRequest::new(workdir.path().to_path_buf())
                                .meta(
                                    serde_json::json!({ "modelId" : "cursor-model" })
                                        .as_object()
                                        .cloned(),
                                ),
                        ),
                )
                .await
                .expect("session/new timed out")
                .expect("session/new failed");
            let _prompt = tokio::time::timeout(
                    Duration::from_secs(30),
                    conn
                        .prompt(
                            acp::PromptRequest::new(
                                session.session_id.clone(),
                                vec![
                                    acp::ContentBlock::Text(acp::TextContent::new("say hello"))
                                ],
                            ),
                        ),
                )
                .await
                .expect("prompt timed out")
                .expect("prompt failed");
            let sys_prompt = server
                .last_system_prompt()
                .expect("should have inference request");
            assert!(
                sys_prompt.contains("Grok") || sys_prompt.contains("grok"),
                "GROK_AGENT=grok-build should override cursor model's agent_type\nsystem prompt preview: {}",
                & sys_prompt[..sys_prompt.len().min(500)]
            );
        })
        .await;
}
