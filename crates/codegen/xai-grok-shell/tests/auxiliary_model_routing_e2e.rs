//! Built-binary coverage for provider-aware auxiliary model routing.
//!
//! These tests deliberately give the active chat model and its auxiliary
//! model different endpoints and credentials. A passing response alone is not
//! enough: the request body, reasoning effort, and bearer token must all land
//! on the intended provider boundary.
//!
//! Run locally after building `open-grok`:
//! ```bash
//! GROK_BINARY="$PWD/target/debug/open-grok" \
//!   cargo test -p xai-grok-shell --test auxiliary_model_routing_e2e -- --ignored
//! ```

use std::future::Future;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use xai_grok_test_support::mock_server::LogEntry;
use xai_grok_test_support::{
    GrokStdioClient, MockInferenceServer, MockModelEntry, TestSandbox, git_workdir, stderr_tail,
};

const SOL_RECAP_MAIN_SENTINEL: &str = "SOL_RECAP_MAIN_SENTINEL";
const SOL_MEMORY_MAIN_SENTINEL: &str = "SOL_XAI_MEMORY_MAIN_SENTINEL";
const XAI_MEMORY_RAW_SENTINEL: &str = "XAI_MEMORY_RAW_SENTINEL";
const RECAP_REQUEST_MARKER: &str = "Write ONE sentence recap body";

async fn with_local_set<F, Fut>(f: F)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    tokio::task::LocalSet::new().run_until(f()).await;
}

fn write_config(home: &TestSandbox, config: String) {
    let open_grok_home = home.grok_home();
    std::fs::create_dir_all(open_grok_home).expect("create isolated OPENGROK_HOME");
    std::fs::write(open_grok_home.join("config.toml"), config).expect("write isolated config.toml");
}

fn is_response_request(entry: &LogEntry) -> bool {
    entry.method == "POST" && entry.path.ends_with("/responses")
}

fn body_contains(entry: &LogEntry, marker: &str) -> bool {
    entry
        .body
        .as_ref()
        .is_some_and(|body| body.to_string().contains(marker))
}

async fn wait_for_response_request(server: &MockInferenceServer, marker: &str) -> LogEntry {
    timeout(Duration::from_secs(15), async {
        loop {
            if let Some(entry) = server
                .requests()
                .into_iter()
                .find(|entry| is_response_request(entry) && body_contains(entry, marker))
            {
                return entry;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for response request containing {marker:?}; requests:\n{}",
            server.request_log_summary()
        )
    })
}

fn assert_wire_route(
    request: &LogEntry,
    expected_model: &str,
    expected_effort: &str,
    expected_token: &str,
) {
    let body = request.body.as_ref().expect("response request body");
    let expected_authorization = format!("Bearer {expected_token}");
    assert_eq!(
        body.get("model").and_then(Value::as_str),
        Some(expected_model),
        "wrong wire model: {body}"
    );
    assert_eq!(
        body.pointer("/reasoning/effort").and_then(Value::as_str),
        Some(expected_effort),
        "wrong auxiliary reasoning effort: {body}"
    );
    assert_eq!(
        request.authorization.as_deref(),
        Some(expected_authorization.as_str()),
        "wrong endpoint credential for {expected_model}"
    );
}

/// An unconfigured Codex recap must use the provider-local Terra default,
/// including Terra's endpoint credential and medium reasoning effort.
#[tokio::test]
#[ignore] // launches the pre-built open-grok binary
async fn automatic_codex_recap_routes_to_terra_medium_with_isolated_auth() {
    with_local_set(|| async {
        let sol = MockInferenceServer::start_with_required_auth(
            vec![
                MockModelEntry::with_agent_type("gpt-5.6-sol", "grok-build")
                    .with_api_backend("responses")
                    .with_supports_reasoning_effort(true),
            ],
            "codex-sol-token",
        )
        .await
        .expect("start Codex Sol mock");
        sol.set_response("Sol main turn complete.");

        let terra = MockInferenceServer::start_with_required_auth(
            vec![
                MockModelEntry::with_agent_type("gpt-5.6-terra", "grok-build")
                    .with_api_backend("responses")
                    .with_supports_reasoning_effort(true),
            ],
            "codex-terra-token",
        )
        .await
        .expect("start Codex Terra mock");
        terra.set_response("Implemented the requested routing and is verifying it now.");

        let home = TestSandbox::new();
        write_config(
            &home,
            format!(
                r#"
[model.codex-sol]
model = "gpt-5.6-sol"
name = "Codex Sol chat"
provider = "codex"
base_url = "{}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 353000
api_key = "codex-sol-token"
supports_reasoning_effort = true
reasoning_efforts = ["low", "medium", "high"]

[model."gpt-5.6-terra"]
model = "gpt-5.6-terra"
name = "Codex Terra recap"
provider = "codex"
base_url = "{}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 353000
api_key = "codex-terra-token"
supports_reasoning_effort = true
reasoning_efforts = ["low", "medium", "high"]

[models]
default = "codex-sol"

[features]
session_recap = true
"#,
                sol.url(),
                terra.url(),
            ),
        );

        let workdir = git_workdir();
        let client = GrokStdioClient::spawn_with_sandbox(&sol, workdir.workspace(), home).await;
        client.initialize_with_timeout().await;
        let session_id = client
            .create_session_with_model_timeout(workdir.workspace(), "codex-sol")
            .await;
        let prompt = client
            .prompt_with_timeout(&session_id, SOL_RECAP_MAIN_SENTINEL)
            .await;
        assert!(
            prompt.is_ok(),
            "Sol prompt failed: {:?}\nstderr:\n{}",
            prompt.err(),
            stderr_tail(&client.stderr(), 1600)
        );

        let recap_ack = client
            .ext_method(
                "x.ai/recap",
                json!({
                    "sessionId": session_id.0.as_ref(),
                    "auto": false,
                }),
            )
            .await;
        assert!(
            recap_ack.is_ok(),
            "recap request failed: {:?}\nstderr:\n{}",
            recap_ack.err(),
            stderr_tail(&client.stderr(), 1600)
        );

        let main_request = wait_for_response_request(&sol, SOL_RECAP_MAIN_SENTINEL).await;
        assert_eq!(
            main_request.authorization.as_deref(),
            Some("Bearer codex-sol-token"),
            "the chat turn must retain the Sol credential"
        );
        assert_eq!(
            main_request
                .body
                .as_ref()
                .and_then(|body| body.get("model"))
                .and_then(Value::as_str),
            Some("gpt-5.6-sol"),
            "the active chat turn must stay on Sol"
        );

        let recap_request = wait_for_response_request(&terra, RECAP_REQUEST_MARKER).await;
        assert_wire_route(
            &recap_request,
            "gpt-5.6-terra",
            "medium",
            "codex-terra-token",
        );
        assert!(
            !sol.requests()
                .iter()
                .any(|entry| body_contains(entry, RECAP_REQUEST_MARKER)),
            "recap content leaked to the active Sol endpoint; requests:\n{}",
            sol.request_log_summary()
        );
    })
    .await;
}

/// An explicit xAI memory helper selected from a Codex chat must cross the
/// provider boundary intentionally, with Grok's credential and low effort.
#[tokio::test]
#[ignore] // launches the pre-built open-grok binary
async fn explicit_codex_chat_to_xai_memory_routes_to_grok_low_with_isolated_auth() {
    with_local_set(|| async {
        let sol = MockInferenceServer::start_with_required_auth(
            vec![
                MockModelEntry::with_agent_type("gpt-5.6-sol", "grok-build")
                    .with_api_backend("responses")
                    .with_supports_reasoning_effort(true),
            ],
            "codex-chat-token",
        )
        .await
        .expect("start Codex chat mock");
        sol.set_response("Codex chat turn complete.");

        let xai = MockInferenceServer::start_with_required_auth(
            vec![
                MockModelEntry::with_agent_type("grok-4.5", "grok-build")
                    .with_api_backend("responses")
                    .with_supports_reasoning_effort(true),
            ],
            "xai-memory-token",
        )
        .await
        .expect("start xAI memory mock");
        xai.set_response("## Preference\n\nUse the isolated helper route.");

        let home = TestSandbox::new();
        write_config(
            &home,
            format!(
                r#"
[model.codex-sol]
model = "gpt-5.6-sol"
name = "Codex Sol chat"
provider = "codex"
base_url = "{}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 353000
api_key = "codex-chat-token"
supports_reasoning_effort = true
reasoning_efforts = ["low", "medium", "high"]

[model.xai-memory]
model = "grok-4.5"
name = "xAI memory helper"
provider = "xai"
base_url = "{}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 500000
api_key = "xai-memory-token"
supports_reasoning_effort = true
reasoning_efforts = ["low", "medium", "high"]

[models]
default = "codex-sol"
memory = "xai-memory"
"#,
                sol.url(),
                xai.url(),
            ),
        );

        let workdir = git_workdir();
        let client = GrokStdioClient::spawn_with_sandbox(&sol, workdir.workspace(), home).await;
        client.initialize_with_timeout().await;
        let session_id = client
            .create_session_with_model_timeout(workdir.workspace(), "codex-sol")
            .await;
        let prompt = client
            .prompt_with_timeout(&session_id, SOL_MEMORY_MAIN_SENTINEL)
            .await;
        assert!(
            prompt.is_ok(),
            "Codex prompt failed: {:?}\nstderr:\n{}",
            prompt.err(),
            stderr_tail(&client.stderr(), 1600)
        );

        let rewrite = client
            .ext_method(
                "x.ai/memory/rewrite",
                json!({
                    "sessionId": session_id.0.as_ref(),
                    "rawText": format!("Remember {XAI_MEMORY_RAW_SENTINEL}"),
                    "contextSummary": "A Codex chat explicitly configured an xAI memory helper.",
                }),
            )
            .await;
        assert!(
            rewrite.is_ok(),
            "memory rewrite failed: {:?}\nstderr:\n{}",
            rewrite.err(),
            stderr_tail(&client.stderr(), 1600)
        );

        let main_request = wait_for_response_request(&sol, SOL_MEMORY_MAIN_SENTINEL).await;
        assert_eq!(
            main_request.authorization.as_deref(),
            Some("Bearer codex-chat-token"),
            "the active Codex chat must retain its own credential"
        );
        assert_eq!(
            main_request
                .body
                .as_ref()
                .and_then(|body| body.get("model"))
                .and_then(Value::as_str),
            Some("gpt-5.6-sol"),
            "the active chat turn must stay on Codex Sol"
        );

        let memory_request = wait_for_response_request(&xai, XAI_MEMORY_RAW_SENTINEL).await;
        assert_wire_route(&memory_request, "grok-4.5", "low", "xai-memory-token");
        assert!(
            !sol.requests()
                .iter()
                .any(|entry| body_contains(entry, XAI_MEMORY_RAW_SENTINEL)),
            "memory content leaked to the Codex endpoint; requests:\n{}",
            sol.request_log_summary()
        );
    })
    .await;
}
