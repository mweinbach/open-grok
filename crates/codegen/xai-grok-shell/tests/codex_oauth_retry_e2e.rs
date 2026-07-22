//! Built-binary regression coverage for provider-scoped Codex 401 recovery.
//!
//! These tests are ignored by default because they launch the pager binary.
//! Run with:
//! `cargo test -p xai-grok-shell --test codex_oauth_retry_e2e -- --ignored`

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::Value;
use tokio::net::TcpListener;
use xai_grok_test_support::{
    HeadlessResult, MockInferenceServer, MockModelEntry, ScriptedResponse, TestSandbox,
    assert_headless_success, assert_no_crashes, git_workdir, grok_binary,
    run_headless_in_sandbox_borrowed, run_headless_in_sandbox_borrowed_with_env,
};

struct OAuthMock {
    base_url: String,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

struct XaiExportMock {
    base_url: String,
    feedback_posts: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for XaiExportMock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn record_xai_export(
    State(feedback_posts): State<Arc<AtomicUsize>>,
    request: axum::extract::Request,
) -> StatusCode {
    if request.method() == axum::http::Method::POST && request.uri().path() == "/feedback" {
        feedback_posts.fetch_add(1, Ordering::SeqCst);
    }
    StatusCode::NOT_FOUND
}

async fn spawn_xai_export_mock() -> XaiExportMock {
    let feedback_posts = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind xAI export mock");
    let address = listener.local_addr().expect("xAI export mock address");
    let app = Router::new()
        .fallback(record_xai_export)
        .with_state(feedback_posts.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve xAI export mock");
    });
    XaiExportMock {
        base_url: format!("http://{address}"),
        feedback_posts,
        task,
    }
}

impl Drop for OAuthMock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn refresh_token(
    State(calls): State<Arc<AtomicUsize>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        request.get("grant_type").and_then(Value::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        request.get("refresh_token").and_then(Value::as_str),
        Some("initial-refresh")
    );
    Json(serde_json::json!({
        "access_token": "refreshed-access",
        "refresh_token": "refreshed-refresh"
    }))
}

async fn spawn_oauth_mock() -> OAuthMock {
    let calls = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind OAuth mock");
    let address = listener.local_addr().expect("OAuth mock address");
    let app = Router::new()
        .route("/oauth/token", post(refresh_token))
        .with_state(calls.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve OAuth mock");
    });
    OAuthMock {
        base_url: format!("http://{address}"),
        calls,
        task,
    }
}

fn jwt(payload: Value) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&payload).expect("serialize JWT payload"));
    format!("{header}.{payload}.signature")
}

fn write_codex_auth(grok_home: &std::path::Path) {
    let id_token = jwt(serde_json::json!({
        "email": "codex-e2e@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_account_id": "account-1",
            "chatgpt_user_id": "user-1",
            "chatgpt_account_is_fedramp": false
        }
    }));
    std::fs::write(
        grok_home.join("codex-auth.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token,
                "access_token": "initial-access",
                "refresh_token": "initial-refresh",
                "account_id": "account-1"
            },
            "last_refresh": chrono::Utc::now()
        }))
        .expect("serialize Codex auth store"),
    )
    .expect("write Codex auth store");
}

fn write_xai_oauth(grok_home: &std::path::Path) {
    std::fs::write(
        grok_home.join("auth.json"),
        r#"{
  "https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828": {
    "key": "xai-sticky-oauth-token",
    "auth_mode": "oidc",
    "create_time": "2026-01-01T00:00:00Z",
    "user_id": "sticky-boundary-user",
    "email": "sticky-boundary-user@test.invalid",
    "expires_at": "2030-01-01T00:00:00Z",
    "refresh_token": "xai-sticky-refresh-token",
    "oidc_issuer": "https://auth.x.ai",
    "oidc_client_id": "b1a00492-073a-47ea-816f-4c329264a828"
  }
}"#,
    )
    .expect("write xAI OAuth store");
}

fn read_session_summary(grok_home: &std::path::Path, session_id: &str) -> Value {
    let sessions_root = grok_home.join("sessions");
    for cwd_dir in std::fs::read_dir(&sessions_root).expect("read sessions root") {
        let summary_path = cwd_dir
            .expect("read encoded cwd entry")
            .path()
            .join(session_id)
            .join("summary.json");
        if summary_path.is_file() {
            return serde_json::from_slice(
                &std::fs::read(&summary_path).expect("read session summary"),
            )
            .expect("parse session summary");
        }
    }
    panic!(
        "session {session_id} has no summary under {}",
        sessions_root.display()
    );
}

struct CaseResult {
    headless: HeadlessResult,
    oauth_calls: usize,
    requests: Vec<xai_grok_test_support::mock_server::LogEntry>,
    request_summary: String,
    unified_log: String,
}

async fn run_case(unauthorized_responses: usize, byok: bool) -> CaseResult {
    let server = MockInferenceServer::start_with_models(vec![
        MockModelEntry::with_agent_type("gpt-5.6-sol", "grok-build").with_api_backend("responses"),
    ])
    .await
    .expect("start inference mock");
    server.set_response("Codex retry succeeded");
    for _ in 0..unauthorized_responses {
        server.enqueue_agent_turn_response(ScriptedResponse::json(
            401,
            serde_json::json!({"error": "expired"}),
        ));
    }

    let oauth = spawn_oauth_mock().await;
    let mut home = TestSandbox::builder().mock_url(server.url()).build();
    home.remove_env("XAI_API_KEY");
    home.remove_env("GROK_LEADER_SOCKET");
    let grok_home = home.grok_home().to_path_buf();
    std::fs::create_dir_all(&grok_home).expect("create OPENGROK_HOME");
    write_codex_auth(&grok_home);
    let byok_line = byok.then_some("api_key = \"codex-byok\"\n").unwrap_or("");
    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"
[model.codex-test]
model = "gpt-5.6-sol"
name = "Codex retry test"
provider = "codex"
base_url = "{base_url}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 353000
{byok_line}

[model.xai-title-helper]
model = "grok-build"
name = "xAI title helper boundary sentinel"
provider = "xai"
base_url = "{base_url}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 256000
api_key = "xai-sidecar-key"

[models]
default = "codex-test"
"#,
            base_url = server.url(),
        ),
    )
    .expect("write config");

    let workdir = git_workdir();
    let mut command = tokio::process::Command::new(grok_binary());
    command
        .args([
            "-p",
            "say hello",
            "--yolo",
            "--model",
            "codex-test",
            "--max-turns",
            "1",
            "--output-format",
            "json",
        ])
        .current_dir(workdir.workspace())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let inference_url = server.url();
    let headless = run_headless_in_sandbox_borrowed_with_env(
        command,
        &home,
        &[
            ("GROK_CODEX_AUTH_BASE_URL", oauth.base_url.as_str()),
            ("GROK_CODEX_INFERENCE_BASE_URL", inference_url.as_str()),
        ],
    )
    .await;
    let oauth_calls = oauth.calls.load(Ordering::SeqCst);
    let requests = server.requests();
    let request_summary = server.request_log_summary();
    let unified_log =
        std::fs::read_to_string(grok_home.join("logs/unified.jsonl")).unwrap_or_default();
    CaseResult {
        headless,
        oauth_calls,
        requests,
        request_summary,
        unified_log,
    }
}

fn main_turn_requests(
    requests: &[xai_grok_test_support::mock_server::LogEntry],
) -> Vec<&xai_grok_test_support::mock_server::LogEntry> {
    requests
        .iter()
        .filter(|request| request.method == "POST" && request.path.ends_with("/responses"))
        .filter(|request| {
            request
                .body
                .as_ref()
                .and_then(|body| body.get("tools"))
                .and_then(Value::as_array)
                .is_some_and(|tools| {
                    tools.len() > 1
                        && !tools.iter().any(|tool| {
                            tool.pointer("/function/name")
                                .or_else(|| tool.get("name"))
                                .and_then(Value::as_str)
                                == Some("session_title")
                        })
                })
        })
        .collect()
}

fn session_title_requests(
    requests: &[xai_grok_test_support::mock_server::LogEntry],
) -> Vec<&xai_grok_test_support::mock_server::LogEntry> {
    requests
        .iter()
        .filter(|request| request.method == "POST" && request.path.ends_with("/responses"))
        .filter(|request| {
            request
                .body
                .as_ref()
                .and_then(|body| body.get("tools"))
                .and_then(Value::as_array)
                .is_some_and(|tools| {
                    tools.iter().any(|tool| {
                        tool.pointer("/function/name")
                            .or_else(|| tool.get("name"))
                            .and_then(Value::as_str)
                            == Some("session_title")
                    })
                })
        })
        .collect()
}

#[tokio::test]
#[ignore] // launches the built pager binary
async fn codex_oauth_401_refreshes_and_resubmits_once() {
    let case = run_case(1, false).await;
    assert_headless_success(&case.headless, "Codex OAuth retry", None);
    assert_no_crashes(&case.headless.stderr);
    assert_eq!(
        case.oauth_calls, 1,
        "401 must force exactly one refresh\n{}",
        case.request_summary
    );
    assert!(
        case.unified_log.contains("provider_boundary"),
        "Codex prompt tracing must stop at the provider boundary"
    );

    let turns = main_turn_requests(&case.requests);
    assert_eq!(turns.len(), 2, "expected original request plus one retry");
    assert_eq!(
        turns[0].authorization.as_deref(),
        Some("Bearer initial-access")
    );
    assert_eq!(
        turns[1].authorization.as_deref(),
        Some("Bearer refreshed-access")
    );
    for request in turns {
        assert_eq!(request.header("chatgpt-account-id"), Some("account-1"));
        let exposed_tools = request
            .body
            .as_ref()
            .and_then(|body| body.get("tools"))
            .and_then(Value::as_array)
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| {
                        tool.pointer("/function/name")
                            .or_else(|| tool.get("name"))
                            .and_then(Value::as_str)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert!(
            !exposed_tools.contains(&"web_search"),
            "Codex without hosted-search support must not see the implicit local xAI search tool"
        );
        for xai_media_tool in [
            "image_gen",
            "image_edit",
            "image_to_video",
            "reference_to_video",
        ] {
            assert!(
                !exposed_tools.contains(&xai_media_tool),
                "Codex must not advertise implicit xAI media tool {xai_media_tool}"
            );
        }
    }

    let titles = session_title_requests(&case.requests);
    assert!(
        !titles.is_empty(),
        "successful first turn must exercise session-title inference"
    );
    for request in titles {
        assert_ne!(
            request.authorization.as_deref(),
            Some("Bearer xai-sidecar-key"),
            "the implicit xAI title default must not receive Codex session content"
        );
        assert_eq!(request.header("chatgpt-account-id"), Some("account-1"));
    }
}

#[tokio::test]
#[ignore] // launches the built pager binary
async fn second_codex_oauth_401_stops_without_another_refresh() {
    let case = run_case(2, false).await;
    assert!(!case.headless.timed_out, "second 401 must terminate");
    assert!(
        !case.headless.status.success(),
        "second 401 must surface as a terminal error\nstdout:\n{}\nstderr:\n{}\nrequests:\n{}",
        case.headless.stdout,
        case.headless.stderr,
        case.request_summary,
    );
    assert_no_crashes(&case.headless.stderr);
    assert_eq!(case.oauth_calls, 1, "second 401 must not refresh again");
    assert_eq!(
        main_turn_requests(&case.requests).len(),
        2,
        "Codex auth recovery may resubmit only once"
    );
}

#[tokio::test]
#[ignore] // launches the built pager binary
async fn codex_byok_401_never_invokes_oauth_refresh() {
    let case = run_case(1, true).await;
    assert!(!case.headless.timed_out, "BYOK 401 must terminate");
    assert!(
        !case.headless.status.success(),
        "BYOK 401 must surface as a terminal error\nstdout:\n{}\nstderr:\n{}\nrequests:\n{}",
        case.headless.stdout,
        case.headless.stderr,
        case.request_summary,
    );
    assert_no_crashes(&case.headless.stderr);
    assert_eq!(case.oauth_calls, 0, "BYOK must never invoke Codex OAuth");
    let turns = main_turn_requests(&case.requests);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].authorization.as_deref(), Some("Bearer codex-byok"));
}

#[tokio::test]
#[ignore] // launches the built pager binary twice
async fn codex_resume_uses_persisted_provider_instead_of_xai_default() {
    let server = MockInferenceServer::start_with_models(vec![
        MockModelEntry::with_agent_type("gpt-5.6-sol", "grok-build").with_api_backend("responses"),
    ])
    .await
    .expect("start inference mock");
    server.set_response("Codex resume succeeded");

    let mut home = TestSandbox::builder().mock_url(server.url()).build();
    home.remove_env("XAI_API_KEY");
    home.remove_env("GROK_LEADER_SOCKET");
    let grok_home = home.grok_home().to_path_buf();
    std::fs::create_dir_all(&grok_home).expect("create OPENGROK_HOME");
    let workdir = git_workdir();
    let session_id = uuid::Uuid::new_v4().to_string();

    let write_config = |default_model: &str, xai_api_key: Option<&str>| {
        let xai_key_line = xai_api_key
            .map(|key| format!("api_key = \"{key}\""))
            .unwrap_or_default();
        std::fs::write(
            grok_home.join("config.toml"),
            format!(
                r#"
[model.codex-test]
model = "gpt-5.6-sol"
name = "Codex resume test"
provider = "codex"
base_url = "{base_url}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 353000
api_key = "codex-resume-byok"

[model.xai-default]
model = "grok-build"
name = "Unauthenticated xAI default"
provider = "xai"
base_url = "{base_url}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 256000
{xai_key_line}

[models]
default = "{default_model}"
"#,
                base_url = server.url(),
            ),
        )
        .expect("write config");
    };

    let run = |args: &[&str]| {
        let mut command = tokio::process::Command::new(grok_binary());
        command
            .args(args)
            .current_dir(workdir.workspace())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
    };

    write_config("codex-test", None);
    let first = run(&[
        "-p",
        "create resumable Codex session",
        "--yolo",
        "--model",
        "codex-test",
        "--session-id",
        &session_id,
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let first = run_headless_in_sandbox_borrowed(first, &home).await;
    assert_headless_success(&first, "create Codex session", None);

    let requests_before_resume = server.requests().len();
    write_config("xai-default", None);
    let resumed = run(&[
        "-p",
        "resume persisted Codex provider",
        "--yolo",
        "--resume",
        &session_id,
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let resumed = run_headless_in_sandbox_borrowed(resumed, &home).await;
    assert_headless_success(&resumed, "resume Codex session", None);
    assert_no_crashes(&resumed.stderr);

    let resume_requests = server.requests();
    let resumed_turns = main_turn_requests(&resume_requests[requests_before_resume..]);
    assert!(
        !resumed_turns.is_empty(),
        "resume must reach Codex inference"
    );
    assert!(
        resumed_turns.iter().all(|request| {
            request.authorization.as_deref() == Some("Bearer codex-resume-byok")
        })
    );

    let profile_path = home.home().join("codex-profile.md");
    std::fs::write(
        &profile_path,
        "---\nname: codex-profile\ndescription: Codex pinned profile\nmodel: codex-test\n---\n",
    )
    .expect("write Codex agent profile");
    let requests_before_profile = server.requests().len();
    let profile_session_id = uuid::Uuid::new_v4().to_string();
    let profiled = run(&[
        "-p",
        "start through a Codex-pinned profile",
        "--yolo",
        "--agent",
        profile_path.to_str().expect("profile path is UTF-8"),
        "--session-id",
        &profile_session_id,
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let profiled = run_headless_in_sandbox_borrowed(profiled, &home).await;
    assert_headless_success(&profiled, "Codex-pinned profile", None);
    let profile_requests = server.requests();
    let profile_turns = main_turn_requests(&profile_requests[requests_before_profile..]);
    assert!(
        !profile_turns.is_empty(),
        "profile must reach Codex inference"
    );
    assert!(
        profile_turns.iter().all(|request| {
            request.authorization.as_deref() == Some("Bearer codex-resume-byok")
        })
    );

    let requests_before_display_name = server.requests().len();
    let display_name_session_id = uuid::Uuid::new_v4().to_string();
    let display_name = run(&[
        "-p",
        "start Codex by display name",
        "--yolo",
        "--model",
        "Codex resume test",
        "--session-id",
        &display_name_session_id,
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let display_name = run_headless_in_sandbox_borrowed(display_name, &home).await;
    assert_headless_success(&display_name, "Codex display-name model", None);
    let display_name_requests = server.requests();
    let display_name_turns =
        main_turn_requests(&display_name_requests[requests_before_display_name..]);
    assert!(
        !display_name_turns.is_empty(),
        "display-name model must reach Codex inference"
    );
    assert!(
        display_name_turns.iter().all(|request| {
            request.authorization.as_deref() == Some("Bearer codex-resume-byok")
        })
    );

    let xai_session_id = uuid::Uuid::new_v4().to_string();
    write_config("xai-default", Some("xai-create-byok"));
    let xai_session = run(&[
        "-p",
        "create resumable xAI session",
        "--yolo",
        "--model",
        "xai-default",
        "--session-id",
        &xai_session_id,
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let xai_session = run_headless_in_sandbox_borrowed(xai_session, &home).await;
    assert_headless_success(&xai_session, "create xAI session", None);

    let requests_before_cross_provider_resume = server.requests().len();
    write_config("xai-default", None);
    let cross_provider = run(&[
        "-p",
        "resume xAI session directly on Codex",
        "--yolo",
        "--resume",
        &xai_session_id,
        "--model",
        "codex-test",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let cross_provider = run_headless_in_sandbox_borrowed(cross_provider, &home).await;
    assert_headless_success(&cross_provider, "cross-provider Codex resume", None);
    assert_no_crashes(&cross_provider.stderr);

    let cross_provider_requests = server.requests();
    let cross_provider_turns =
        main_turn_requests(&cross_provider_requests[requests_before_cross_provider_resume..]);
    assert!(
        !cross_provider_turns.is_empty(),
        "cross-provider resume must reach Codex inference"
    );
    assert!(
        cross_provider_turns.iter().all(|request| {
            request.authorization.as_deref() == Some("Bearer codex-resume-byok")
        })
    );
}

#[tokio::test]
#[ignore] // launches the built pager binary repeatedly
async fn xai_codex_xai_resume_keeps_xai_exports_closed() {
    let server = MockInferenceServer::start_with_models(vec![
        MockModelEntry::with_agent_type("gpt-5.6-sol", "grok-build").with_api_backend("responses"),
    ])
    .await
    .expect("start inference mock");
    server.set_response("sticky provider boundary succeeded");

    let export_mock = spawn_xai_export_mock().await;
    let mut home = TestSandbox::builder().mock_url(server.url()).build();
    home.remove_env("XAI_API_KEY");
    home.remove_env("GROK_LEADER_SOCKET");
    home.set_env("GROK_FEEDBACK_ENABLED", "true");
    home.set_env("GROK_FEEDBACK_BASE_URL", export_mock.base_url.as_str());
    home.set_env("GROK_TELEMETRY_TRACE_UPLOAD", "true");
    home.set_env("GROK_TRACE_UPLOAD", "true");
    home.set_env("GROK_TRACE_UPLOAD_URL", server.url());
    let grok_home = home.grok_home().to_path_buf();
    std::fs::create_dir_all(&grok_home).expect("create OPENGROK_HOME");
    write_xai_oauth(&grok_home);

    std::fs::write(
        grok_home.join("config.toml"),
        format!(
            r#"
[model.xai-sticky]
model = "grok-build"
name = "xAI sticky boundary test"
provider = "xai"
base_url = "{base_url}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 256000

[model.codex-sticky]
model = "gpt-5.6-sol"
name = "Codex sticky boundary test"
provider = "codex"
base_url = "{base_url}"
api_backend = "responses"
agent_type = "grok-build"
tool_mode = "direct"
context_window = 353000
api_key = "codex-sticky-byok"

[models]
default = "xai-sticky"
"#,
            base_url = server.url(),
        ),
    )
    .expect("write config");

    let workdir = git_workdir();
    let session_id = uuid::Uuid::new_v4().to_string();
    let run = |args: &[&str]| {
        let mut command = tokio::process::Command::new(grok_binary());
        command
            .args(args)
            .current_dir(workdir.workspace())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
    };

    let first = run(&[
        "-p",
        "create xAI session before provider boundary",
        "--yolo",
        "--model",
        "xai-sticky",
        "--session-id",
        &session_id,
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let requests_before_first = server.requests().len();
    let first = run_headless_in_sandbox_borrowed(first, &home).await;
    assert_headless_success(&first, "create xAI sticky-boundary session", None);
    assert_no_crashes(&first.stderr);
    let first_requests = server.requests();
    let first_turns = main_turn_requests(&first_requests[requests_before_first..]);
    assert!(!first_turns.is_empty(), "initial xAI turn must run");
    assert!(first_turns.iter().all(|request| {
        request.authorization.as_deref() == Some("Bearer xai-sticky-oauth-token")
    }));
    assert!(
        server.storage_request_count() > 0,
        "xAI OAuth session must exercise trace export before the boundary closes"
    );

    let before_boundary_feedback = run(&[
        "-p",
        "/feedback before Codex provider boundary",
        "--yolo",
        "--resume",
        &session_id,
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let before_boundary_feedback =
        run_headless_in_sandbox_borrowed(before_boundary_feedback, &home).await;
    assert_headless_success(
        &before_boundary_feedback,
        "submit feedback before provider boundary",
        None,
    );
    assert_eq!(
        export_mock.feedback_posts.load(Ordering::SeqCst),
        1,
        "xAI feedback must reach the wire before Codex is used"
    );
    let exports_before_codex = server.storage_request_count();

    let requests_before_codex = server.requests().len();
    let codex = run(&[
        "-p",
        "cross into Codex and close xAI exports",
        "--yolo",
        "--resume",
        &session_id,
        "--model",
        "codex-sticky",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let codex = run_headless_in_sandbox_borrowed(codex, &home).await;
    assert_headless_success(&codex, "cross xAI session into Codex", None);
    assert_no_crashes(&codex.stderr);
    let codex_requests = server.requests();
    let codex_turns = main_turn_requests(&codex_requests[requests_before_codex..]);
    assert!(!codex_turns.is_empty(), "Codex turn must run");
    assert!(
        codex_turns.iter().all(|request| {
            request.authorization.as_deref() == Some("Bearer codex-sticky-byok")
        })
    );
    assert_eq!(
        server.storage_request_count(),
        exports_before_codex,
        "Codex turn must not export trace artifacts to xAI storage"
    );
    assert_eq!(
        read_session_summary(&grok_home, &session_id)
            .get("ever_used_codex")
            .and_then(Value::as_bool),
        Some(true),
        "Codex transition must durably close the provider boundary"
    );

    let requests_before_xai_return = server.requests().len();
    let xai_return = run(&[
        "-p",
        "return to xAI inference without reopening exports",
        "--yolo",
        "--resume",
        &session_id,
        "--model",
        "xai-sticky",
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let xai_return = run_headless_in_sandbox_borrowed(xai_return, &home).await;
    assert_headless_success(&xai_return, "return sticky session to xAI", None);
    assert_no_crashes(&xai_return.stderr);
    let xai_return_requests = server.requests();
    let xai_return_turns = main_turn_requests(&xai_return_requests[requests_before_xai_return..]);
    assert!(!xai_return_turns.is_empty(), "returning xAI turn must run");
    assert!(xai_return_turns.iter().all(|request| {
        request.authorization.as_deref() == Some("Bearer xai-sticky-oauth-token")
    }));
    assert_eq!(
        server.storage_request_count(),
        exports_before_codex,
        "returning to xAI inference must not reopen trace export"
    );
    assert_eq!(
        read_session_summary(&grok_home, &session_id)
            .get("ever_used_codex")
            .and_then(Value::as_bool),
        Some(true),
        "provider boundary must remain monotonic after returning to xAI"
    );

    let after_boundary_feedback = run(&[
        "-p",
        "/feedback after returning to xAI",
        "--yolo",
        "--resume",
        &session_id,
        "--max-turns",
        "1",
        "--output-format",
        "json",
    ]);
    let after_boundary_feedback =
        run_headless_in_sandbox_borrowed(after_boundary_feedback, &home).await;
    assert_headless_success(
        &after_boundary_feedback,
        "keep feedback local after provider boundary",
        None,
    );
    assert_eq!(
        export_mock.feedback_posts.load(Ordering::SeqCst),
        1,
        "returning to xAI must not reopen feedback export"
    );
    assert_eq!(
        server.storage_request_count(),
        exports_before_codex,
        "feedback command must not reopen trace export"
    );
}
