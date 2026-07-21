//! End-to-end workflow tool tests: real V8 execution through the code-mode
//! runtime, with a stub [`SubagentBackend`] standing in for the coordinator.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value as JsonValue;

use super::WorkflowTool;
use crate::implementations::grok_build::task::backend::{SubagentBackend, SubagentBackendResource};
use crate::implementations::grok_build::task::types::{
    CurrentPromptIdResource, SessionIdResource, SubagentCancelOutcome, SubagentDepthCounter,
    SubagentDescribeOutcome, SubagentRequest, SubagentResult, SubagentSnapshot,
    SubagentValidateTypeOutcome, TaskModelValidator,
};
use crate::types::output::ToolOutput;
use crate::types::resources::{Cwd, Resources, SessionFolder};
use crate::types::tool_metadata::test_ctx;
use xai_tool_types::WorkflowToolInput;

/// Recorded shape of one spawn for assertions.
#[derive(Debug, Clone)]
struct RecordedSpawn {
    prompt: String,
    subagent_type: String,
    swarm_id: Option<String>,
    swarm_item: Option<String>,
    parent_session_id: String,
    parent_prompt_id: Option<String>,
    run_in_background: bool,
    surface_completion: bool,
    resume_from: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

type Responder = dyn Fn(&SubagentRequest) -> SubagentResult + Send + Sync;

struct StubBackend {
    responder: Box<Responder>,
    delay: Option<Duration>,
    spawned: parking_lot::Mutex<Vec<RecordedSpawn>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl StubBackend {
    fn echo() -> Arc<Self> {
        Self::with_responder(None, |request| echo_result(request, 100))
    }

    fn with_responder(
        delay: Option<Duration>,
        responder: impl Fn(&SubagentRequest) -> SubagentResult + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            responder: Box::new(responder),
            delay,
            spawned: parking_lot::Mutex::new(Vec::new()),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        })
    }

    fn spawn_count(&self) -> usize {
        self.spawned.lock().len()
    }

    fn spawns(&self) -> Vec<RecordedSpawn> {
        self.spawned.lock().clone()
    }

    fn max_observed_concurrency(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

fn echo_result(request: &SubagentRequest, tokens_used: u64) -> SubagentResult {
    SubagentResult {
        success: true,
        output: Arc::from(format!("ECHO:{}", request.prompt)),
        error: None,
        cancelled: false,
        subagent_id: request.id.clone(),
        child_session_id: request.id.clone(),
        tool_calls: 1,
        turns: 1,
        duration_ms: 5,
        tokens_used,
        worktree_path: None,
        backgrounded: false,
    }
}

#[async_trait::async_trait]
impl SubagentBackend for StubBackend {
    async fn spawn(
        &self,
        request: SubagentRequest,
    ) -> Result<SubagentResult, xai_tool_runtime::ToolError> {
        self.spawned.lock().push(RecordedSpawn {
            prompt: request.prompt.clone(),
            subagent_type: request.subagent_type.clone(),
            swarm_id: request.swarm.as_ref().map(|s| s.swarm_id.clone()),
            swarm_item: request.swarm.as_ref().and_then(|s| s.item.clone()),
            parent_session_id: request.parent_session_id.clone(),
            parent_prompt_id: request.parent_prompt_id.clone(),
            run_in_background: request.run_in_background,
            surface_completion: request.surface_completion,
            resume_from: request.resume_from.clone(),
            model: request.runtime_overrides.model.clone(),
            effort: request.runtime_overrides.reasoning_effort.clone(),
        });
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        let result = (self.responder)(&request);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(result)
    }

    async fn query(
        &self,
        _id: &str,
        _block: bool,
        _timeout_ms: Option<u64>,
    ) -> Option<SubagentSnapshot> {
        None
    }

    async fn cancel(&self, _id: &str) -> SubagentCancelOutcome {
        SubagentCancelOutcome::NotFound
    }

    async fn validate_type(
        &self,
        _subagent_type: &str,
        _parent_session_id: &str,
    ) -> SubagentValidateTypeOutcome {
        SubagentValidateTypeOutcome::Ok
    }

    async fn describe_subagent_type(
        &self,
        _subagent_type: &str,
        _harness_agent_type: Option<&str>,
        _parent_session_id: &str,
    ) -> SubagentDescribeOutcome {
        SubagentDescribeOutcome::Unknown {
            available: Vec::new(),
        }
    }
}

struct TestRun {
    backend: Arc<StubBackend>,
    session_folder: Option<std::path::PathBuf>,
    terminal: Option<Arc<dyn crate::computer::types::TerminalBackend>>,
    notifications: Option<crate::notification::handle::ToolNotificationHandle>,
}

impl TestRun {
    fn new(backend: Arc<StubBackend>) -> Self {
        Self {
            backend,
            session_folder: None,
            terminal: None,
            notifications: None,
        }
    }

    fn with_session_folder(mut self, folder: std::path::PathBuf) -> Self {
        self.session_folder = Some(folder);
        self
    }

    fn with_terminal(mut self, terminal: Arc<dyn crate::computer::types::TerminalBackend>) -> Self {
        self.terminal = Some(terminal);
        self
    }

    fn with_notifications(
        mut self,
        notifications: crate::notification::handle::ToolNotificationHandle,
    ) -> Self {
        self.notifications = Some(notifications);
        self
    }

    async fn run(&self, input: WorkflowToolInput) -> Result<String, String> {
        let mut resources = Resources::new();
        resources.insert(SubagentBackendResource(
            self.backend.clone() as Arc<dyn SubagentBackend>
        ));
        resources.insert(SubagentDepthCounter(0));
        resources.insert(SessionIdResource("test-session".to_string()));
        resources.insert(CurrentPromptIdResource("prompt-1".to_string()));
        resources.insert(TaskModelValidator::new(|_| None));
        resources.insert(Cwd(std::env::temp_dir()));
        if let Some(folder) = &self.session_folder {
            resources.insert(SessionFolder(folder.clone()));
        }
        if let Some(terminal) = &self.terminal {
            resources.insert(crate::types::resources::Terminal(terminal.clone()));
        }
        if let Some(notifications) = &self.notifications {
            resources.insert(crate::types::resources::NotificationHandle(
                notifications.clone(),
            ));
        }
        let result =
            xai_tool_runtime::Tool::run(&WorkflowTool, test_ctx(resources.into_shared()), input)
                .await;
        match result {
            Ok(ToolOutput::Text(text)) => Ok(text.text.clone()),
            Ok(other) => Err(format!("unexpected output variant: {other:?}")),
            Err(error) => Err(error.to_string()),
        }
    }
}

fn script_input(script: &str) -> WorkflowToolInput {
    WorkflowToolInput {
        script: Some(script.to_string()),
        script_path: None,
        args: None,
        token_budget: None,
        run_in_background: false,
        resume_from_run_id: None,
        resume_mode: None,
        resume_through: None,
    }
}

fn run_id_of(output: &str) -> String {
    output
        .split("<run_id>")
        .nth(1)
        .and_then(|rest| rest.split("</run_id>").next())
        .expect("run id in output")
        .to_string()
}

const META: &str = "export const meta = { name: 'test-flow', description: 'test workflow' };\n";

fn with_meta(body: &str) -> String {
    format!("{META}{body}")
}

#[tokio::test]
async fn script_returns_value_without_agents() {
    let backend = StubBackend::echo();
    let output = TestRun::new(backend.clone())
        .run(script_input(&with_meta("return { n: 1 + 2 };")))
        .await
        .expect("workflow runs");
    assert!(output.contains("<status>completed</status>"), "{output}");
    assert!(output.contains("\"n\": 3"), "{output}");
    assert_eq!(backend.spawn_count(), 0);
}

#[tokio::test]
async fn agent_round_trip_and_request_shape() {
    let backend = StubBackend::echo();
    let script = with_meta(
        "phase('P1');\n\
         const r = await agent('hello world', { label: 'greet', model: 'test-model', effort: 'low' });\n\
         return r;",
    );
    let mut input = script_input(&script);
    input.args = Some(serde_json::json!({"k": "v"}));
    let output = TestRun::new(backend.clone())
        .run(input)
        .await
        .expect("runs");
    assert!(output.contains("ECHO:hello world"), "{output}");
    assert!(output.contains("<status>completed</status>"), "{output}");

    let spawns = backend.spawns();
    assert_eq!(spawns.len(), 1);
    let spawn = &spawns[0];
    assert_eq!(spawn.prompt, "hello world");
    assert_eq!(spawn.subagent_type, "general-purpose");
    assert_eq!(spawn.parent_session_id, "test-session");
    assert_eq!(spawn.parent_prompt_id.as_deref(), Some("prompt-1"));
    assert!(!spawn.run_in_background);
    assert!(!spawn.surface_completion);
    assert!(spawn.swarm_id.is_some(), "workflow agents carry swarm meta");
    assert_eq!(spawn.swarm_item.as_deref(), Some("greet"));
    assert_eq!(spawn.model.as_deref(), Some("test-model"));
    assert_eq!(spawn.effort.as_deref(), Some("low"));
    assert_eq!(spawn.resume_from, None);
}

#[tokio::test]
async fn parallel_runs_concurrently_and_maps_errors_to_null() {
    let backend = StubBackend::with_responder(Some(Duration::from_millis(120)), |request| {
        echo_result(request, 10)
    });
    let script = with_meta(
        "const rs = await parallel([\n\
           () => agent('a'),\n\
           () => agent('b'),\n\
           () => { throw new Error('thunk exploded'); },\n\
         ]);\n\
         return rs;",
    );
    let output = TestRun::new(backend.clone())
        .run(script_input(&script))
        .await
        .expect("runs");
    assert!(output.contains("ECHO:a"), "{output}");
    assert!(output.contains("ECHO:b"), "{output}");
    assert!(output.contains("null"), "{output}");
    assert_eq!(backend.spawn_count(), 2);
    assert!(
        backend.max_observed_concurrency() >= 2,
        "parallel agents must overlap (max observed: {})",
        backend.max_observed_concurrency()
    );
}

#[tokio::test]
async fn pipeline_drops_failed_items_and_passes_original_and_index() {
    let backend = StubBackend::echo();
    let script = with_meta(
        "const rs = await pipeline(\n\
           ['x', 'y', 'z'],\n\
           (item, orig, i) => agent('stage1 ' + item + ' #' + i),\n\
           (prev, orig, i) => {\n\
             if (orig === 'y') { throw new Error('drop y'); }\n\
             return prev + '|' + orig + '@' + i;\n\
           },\n\
         );\n\
         return rs;",
    );
    let output = TestRun::new(backend.clone())
        .run(script_input(&script))
        .await
        .expect("runs");
    assert!(output.contains("ECHO:stage1 x #0|x@0"), "{output}");
    assert!(output.contains("null"), "{output}");
    assert!(output.contains("ECHO:stage1 z #2|z@2"), "{output}");
    assert_eq!(backend.spawn_count(), 3);
}

#[tokio::test]
async fn budget_exhaustion_throws_in_script() {
    let backend = StubBackend::with_responder(None, |request| echo_result(request, 500));
    let script = with_meta(
        "const first = await agent('one');\n\
         try {\n\
           await agent('two');\n\
           return 'no-throw';\n\
         } catch (e) {\n\
           return 'threw:' + e.message;\n\
         }",
    );
    let mut input = script_input(&script);
    input.token_budget = Some(200);
    let output = TestRun::new(backend.clone())
        .run(input)
        .await
        .expect("runs");
    assert!(
        output.contains("threw:workflow token budget exhausted"),
        "{output}"
    );
    assert_eq!(backend.spawn_count(), 1, "second agent must not spawn");
}

#[tokio::test]
async fn determinism_guards_throw() {
    let backend = StubBackend::echo();
    for (expr, needle) in [
        ("Date.now()", "Date.now() is unavailable"),
        ("Math.random()", "Math.random() is unavailable"),
        ("new Date()", "new Date() without arguments is unavailable"),
    ] {
        let script = with_meta(&format!("return {expr};"));
        let output = TestRun::new(backend.clone())
            .run(script_input(&script))
            .await
            .expect("runs");
        assert!(
            output.contains("<status>failed</status>"),
            "{expr}: {output}"
        );
        assert!(output.contains(needle), "{expr}: {output}");
    }
    // Explicit timestamps stay usable.
    let script = with_meta("return new Date(0).toISOString();");
    let output = TestRun::new(backend)
        .run(script_input(&script))
        .await
        .expect("runs");
    assert!(output.contains("1970-01-01T00:00:00.000Z"), "{output}");
}

#[tokio::test]
async fn invalid_meta_is_rejected_before_execution() {
    let backend = StubBackend::echo();
    let err = TestRun::new(backend)
        .run(script_input("const x = 1;"))
        .await
        .expect_err("must reject");
    assert!(err.contains("must begin with"), "{err}");
}

#[tokio::test]
async fn schema_extracts_json_from_prose_output() {
    let backend = StubBackend::with_responder(None, |request| SubagentResult {
        output: Arc::from("Sure! Here it is:\n```json\n{\"bugs\": [\"b1\", \"b2\"]}\n```\nDone."),
        ..echo_result(request, 10)
    });
    let script = with_meta(
        "const r = await agent('find bugs', { schema: { type: 'object' } });\n\
         return r.bugs;",
    );
    let output = TestRun::new(backend.clone())
        .run(script_input(&script))
        .await
        .expect("runs");
    assert!(output.contains("\"b1\""), "{output}");
    assert!(output.contains("\"b2\""), "{output}");
    // Schema satisfied on first pass: no corrective resume spawn.
    assert_eq!(backend.spawn_count(), 1);
    let spawn = &backend.spawns()[0];
    assert!(
        spawn.prompt.contains("<output_contract>"),
        "schema must inject the output contract: {}",
        spawn.prompt
    );
}

#[tokio::test]
async fn schema_mismatch_gets_one_corrective_resume_retry() {
    let backend = StubBackend::with_responder(None, |request| {
        if request.resume_from.is_some() {
            SubagentResult {
                output: Arc::from("{\"fixed\": true}"),
                ..echo_result(request, 10)
            }
        } else {
            SubagentResult {
                output: Arc::from("I could not produce JSON, sorry."),
                ..echo_result(request, 10)
            }
        }
    });
    let script = with_meta(
        "const r = await agent('strict', { schema: { type: 'object' } });\n\
         return r;",
    );
    let output = TestRun::new(backend.clone())
        .run(script_input(&script))
        .await
        .expect("runs");
    assert!(output.contains("\"fixed\": true"), "{output}");
    let spawns = backend.spawns();
    assert_eq!(spawns.len(), 2, "exactly one corrective retry");
    assert!(
        spawns[1].resume_from.is_some(),
        "retry must resume the child"
    );
}

#[tokio::test]
async fn failed_agents_resolve_to_null_not_throw() {
    let backend = StubBackend::with_responder(None, |request| SubagentResult {
        success: false,
        error: Some("provider exploded".to_string()),
        ..echo_result(request, 10)
    });
    let script = with_meta(
        "const r = await agent('doomed');\n\
         return { got: r === null ? 'null' : 'value' };",
    );
    let output = TestRun::new(backend)
        .run(script_input(&script))
        .await
        .expect("runs");
    assert!(output.contains("\"got\": \"null\""), "{output}");
    assert!(output.contains("<status>completed</status>"), "{output}");
}

#[tokio::test]
async fn logs_phases_and_args_flow_through() {
    let backend = StubBackend::echo();
    let script = with_meta(
        "phase('Verify');\n\
         log('checking ' + args.target);\n\
         return meta.name;",
    );
    let mut input = script_input(&script);
    input.args = Some(serde_json::json!({"target": "auth"}));
    let output = TestRun::new(backend).run(input).await.expect("runs");
    assert!(output.contains("phase: Verify"), "{output}");
    assert!(output.contains("checking auth"), "{output}");
    assert!(output.contains("test-flow"), "{output}");
}

#[tokio::test]
async fn journal_replay_skips_unchanged_agents() {
    let folder = std::env::temp_dir().join(format!("wf-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&folder).unwrap();
    let script = with_meta("return await agent('cache me', { label: 'cached-agent' });");

    let first_backend = StubBackend::echo();
    let first = TestRun::new(first_backend.clone())
        .with_session_folder(folder.clone())
        .run(script_input(&script))
        .await
        .expect("first run");
    assert_eq!(first_backend.spawn_count(), 1);
    let run_id = first
        .split("<run_id>")
        .nth(1)
        .and_then(|rest| rest.split("</run_id>").next())
        .expect("run id in output")
        .to_string();

    let second_backend = StubBackend::echo();
    let mut input = script_input(&script);
    input.resume_from_run_id = Some(run_id);
    let second = TestRun::new(second_backend.clone())
        .with_session_folder(folder.clone())
        .run(input)
        .await
        .expect("second run");
    assert_eq!(second_backend.spawn_count(), 0, "replay must not respawn");
    assert!(second.contains("ECHO:cache me"), "{second}");
    assert!(second.contains("cached"), "{second}");

    // Resume chains: the second run re-journals what it replayed, so a third
    // run resuming from the SECOND run's id must also serve from journal.
    let second_run_id = second
        .split("<run_id>")
        .nth(1)
        .and_then(|rest| rest.split("</run_id>").next())
        .expect("second run id")
        .to_string();
    let third_backend = StubBackend::echo();
    let mut input = script_input(&script);
    input.resume_from_run_id = Some(second_run_id);
    let third = TestRun::new(third_backend.clone())
        .with_session_folder(folder.clone())
        .run(input)
        .await
        .expect("third run");
    assert_eq!(
        third_backend.spawn_count(),
        0,
        "chained resume must replay from the intermediate run's journal"
    );
    assert!(third.contains("ECHO:cache me"), "{third}");

    let _ = std::fs::remove_dir_all(&folder);
}

#[tokio::test]
async fn positional_resume_replays_reworded_script_and_reruns_past_boundary() {
    let folder = std::env::temp_dir().join(format!("wf-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&folder).unwrap();

    let original = with_meta(
        "phase('Plan');\n\
         const plan = await agent('make the plan', { label: 'planner' });\n\
         phase('Build');\n\
         const build = await agent('do the build using ' + plan, { label: 'builder' });\n\
         return { plan, build };",
    );
    let first_backend = StubBackend::echo();
    let first = TestRun::new(first_backend.clone())
        .with_session_folder(folder.clone())
        .run(script_input(&original))
        .await
        .expect("first run");
    assert_eq!(first_backend.spawn_count(), 2);
    let first_run_id = run_id_of(&first);

    // The script is REWRITTEN (different prompts) — exact resume would replay
    // nothing. Positional resume through the Plan phase keeps the planner
    // result and re-runs the builder fresh.
    let edited = with_meta(
        "phase('Plan');\n\
         const plan = await agent('make the plan v2 with different wording', { label: 'planner' });\n\
         phase('Build');\n\
         const build = await agent('rebuild differently using ' + plan, { label: 'builder' });\n\
         return { plan, build };",
    );
    let second_backend = StubBackend::echo();
    let mut input = script_input(&edited);
    input.resume_from_run_id = Some(first_run_id);
    input.resume_mode = Some("positional".to_string());
    input.resume_through = Some(serde_json::json!("Plan"));
    let second = TestRun::new(second_backend.clone())
        .with_session_folder(folder.clone())
        .run(input)
        .await
        .expect("second run");
    assert_eq!(
        second_backend.spawn_count(),
        1,
        "planner replays positionally; only the builder re-runs"
    );
    assert!(
        second.contains("ECHO:make the plan"),
        "planner result kept: {second}"
    );
    assert!(
        second.contains("ECHO:rebuild differently"),
        "builder re-ran with the edited prompt: {second}"
    );
    assert!(second.contains("<status>completed</status>"), "{second}");

    let _ = std::fs::remove_dir_all(&folder);
}

#[tokio::test]
async fn resume_through_unknown_point_is_rejected_with_known_points() {
    let folder = std::env::temp_dir().join(format!("wf-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&folder).unwrap();
    let script = with_meta(
        "phase('Scan');\n\
         return await agent('scan it', { label: 'scanner' });",
    );
    let backend = StubBackend::echo();
    let first = TestRun::new(backend)
        .with_session_folder(folder.clone())
        .run(script_input(&script))
        .await
        .expect("first run");
    let run_id = run_id_of(&first);

    let mut input = script_input(&script);
    input.resume_from_run_id = Some(run_id);
    input.resume_through = Some(serde_json::json!("Verify"));
    let err = TestRun::new(StubBackend::echo())
        .with_session_folder(folder.clone())
        .run(input)
        .await
        .expect_err("unknown point must reject");
    assert!(err.contains("Known points"), "{err}");
    assert!(err.contains("Scan"), "{err}");

    let _ = std::fs::remove_dir_all(&folder);
}

#[tokio::test]
async fn resume_options_require_resume_from_run_id() {
    let mut input = script_input(&with_meta("return 1;"));
    input.resume_mode = Some("positional".to_string());
    let err = TestRun::new(StubBackend::echo())
        .run(input)
        .await
        .expect_err("must reject");
    assert!(err.contains("require resume_from_run_id"), "{err}");
}

#[tokio::test]
async fn depth_limit_rejects_workflow_inside_subagent() {
    let backend = StubBackend::echo();
    let mut resources = Resources::new();
    resources.insert(SubagentBackendResource(
        backend.clone() as Arc<dyn SubagentBackend>
    ));
    resources.insert(SubagentDepthCounter(1));
    resources.insert(SessionIdResource("child".to_string()));
    resources.insert(Cwd(std::env::temp_dir()));
    let err = xai_tool_runtime::Tool::run(
        &WorkflowTool,
        test_ctx(resources.into_shared()),
        script_input(&with_meta("return 1;")),
    )
    .await
    .expect_err("depth 1 must reject");
    assert!(err.to_string().contains("depth limit exceeded"), "{err}");
    assert_eq!(backend.spawn_count(), 0);
}

#[tokio::test]
async fn script_error_reports_failed_status_with_partial_progress() {
    let backend = StubBackend::echo();
    let script = with_meta(
        "await agent('ok one', { label: 'ok-one' });\n\
         throw new Error('script exploded');",
    );
    let output = TestRun::new(backend.clone())
        .run(script_input(&script))
        .await
        .expect("tool call itself succeeds");
    assert!(output.contains("<status>failed</status>"), "{output}");
    assert!(output.contains("script exploded"), "{output}");
    assert!(output.contains("ok-one"), "{output}");
    assert_eq!(backend.spawn_count(), 1);
}

struct NoopTerminal;

#[async_trait::async_trait]
impl crate::computer::types::TerminalBackend for NoopTerminal {
    async fn run(
        &self,
        _request: crate::computer::types::TerminalRunRequest,
    ) -> Result<crate::computer::types::TerminalRunResult, crate::computer::types::ComputerError>
    {
        Err(crate::computer::types::ComputerError::io("noop"))
    }
    async fn run_background(
        &self,
        _request: crate::computer::types::TerminalRunRequest,
    ) -> Result<crate::computer::types::BackgroundHandle, crate::computer::types::ComputerError>
    {
        Err(crate::computer::types::ComputerError::io("noop"))
    }
    async fn get_task(&self, _task_id: &str) -> Option<crate::computer::types::TaskSnapshot> {
        None
    }
    async fn kill_task(&self, _task_id: &str) -> crate::computer::types::KillOutcome {
        crate::computer::types::KillOutcome::NotFound
    }
    async fn wait_for_completion(
        &self,
        _task_id: &str,
        _timeout: Option<Duration>,
    ) -> Option<crate::computer::types::TaskSnapshot> {
        None
    }
    async fn list_tasks(&self) -> Vec<crate::computer::types::TaskSnapshot> {
        Vec::new()
    }
}

fn virtual_terminal() -> Arc<dyn crate::computer::types::TerminalBackend> {
    Arc::new(
        crate::computer::virtual_tasks::VirtualTaskTerminalBackend::new(Arc::new(NoopTerminal)),
    )
}

#[tokio::test]
async fn background_run_registers_completes_and_wakes() {
    let folder = std::env::temp_dir().join(format!("wf-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&folder).unwrap();
    let terminal = virtual_terminal();
    let (notifications, mut notification_rx) =
        crate::notification::handle::ToolNotificationHandle::channel();
    let backend = StubBackend::echo();

    let mut input = script_input(&with_meta(
        "log('background says hi');\nreturn await agent('bg work', { label: 'bg' });",
    ));
    input.run_in_background = true;
    let started = TestRun::new(backend.clone())
        .with_session_folder(folder.clone())
        .with_terminal(terminal.clone())
        .with_notifications(notifications)
        .run(input)
        .await
        .expect("background launch returns immediately");
    assert!(started.contains("<workflow_started>"), "{started}");
    assert!(started.contains("running in background"), "{started}");
    let run_id = run_id_of(&started);

    let final_snapshot = terminal
        .wait_for_completion(&run_id, Some(Duration::from_secs(30)))
        .await
        .expect("virtual task resolves");
    assert!(final_snapshot.completed, "run must complete");
    assert_eq!(final_snapshot.exit_code, Some(0));
    assert!(
        final_snapshot.output.contains("ECHO:bg work"),
        "{}",
        final_snapshot.output
    );
    assert!(
        final_snapshot.output.contains("<status>completed</status>"),
        "{}",
        final_snapshot.output
    );
    assert_eq!(backend.spawn_count(), 1);

    // Progress log exists and carries the script's narration.
    let progress = std::fs::read_to_string(&final_snapshot.output_file).expect("progress log");
    assert!(progress.contains("background says hi"), "{progress}");

    // A TaskCompleted notification fired for the auto-wake path.
    let mut saw_completion = false;
    while let Ok(notification) = notification_rx.try_recv() {
        if let crate::notification::types::ToolNotification::TaskCompleted(snapshot) = notification
            && snapshot.task_id == run_id
        {
            saw_completion = true;
        }
    }
    assert!(
        saw_completion,
        "completion must emit the task-completed wake"
    );

    let _ = std::fs::remove_dir_all(&folder);
}

#[tokio::test]
async fn background_kill_interrupts_then_journal_resume_recovers() {
    let folder = std::env::temp_dir().join(format!("wf-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&folder).unwrap();
    let terminal = virtual_terminal();
    // Every agent takes ~50ms, long enough to land the kill while the second
    // one is in flight.
    let slow_backend = StubBackend::with_responder(Some(Duration::from_millis(50)), |request| {
        echo_result(request, 10)
    });

    let script = with_meta(
        "const first = await agent('quick step', { label: 'quick' });\n\
         const second = await agent('slow step that will be interrupted', { label: 'slow' });\n\
         return { first, second };",
    );
    let mut input = script_input(&script);
    input.run_in_background = true;
    let started = TestRun::new(slow_backend.clone())
        .with_session_folder(folder.clone())
        .with_terminal(terminal.clone())
        .run(input)
        .await
        .expect("launch");
    let run_id = run_id_of(&started);

    // Wait until the second (slow) agent is actually in flight, then kill.
    for _ in 0..200 {
        if slow_backend.spawn_count() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        matches!(
            terminal.kill_task(&run_id).await,
            crate::computer::types::KillOutcome::Killed
        ),
        "kill must land while running"
    );

    let final_snapshot = terminal
        .wait_for_completion(&run_id, Some(Duration::from_secs(30)))
        .await
        .expect("resolves");
    assert!(final_snapshot.completed);
    assert!(final_snapshot.explicitly_killed);
    assert_eq!(final_snapshot.exit_code, Some(1));
    assert!(
        final_snapshot.output.contains("cancelled"),
        "{}",
        final_snapshot.output
    );

    // The interrupted run's journal still holds the completed first step:
    // a foreground resume replays it without respawning.
    let resume_backend = StubBackend::echo();
    let mut resume = script_input(&script);
    resume.resume_from_run_id = Some(run_id);
    let resumed = TestRun::new(resume_backend.clone())
        .with_session_folder(folder.clone())
        .run(resume)
        .await
        .expect("resume");
    assert!(resumed.contains("<status>completed</status>"), "{resumed}");
    assert_eq!(
        resume_backend.spawn_count(),
        1,
        "quick step replays from the interrupted run's journal; only the slow step re-runs"
    );

    let _ = std::fs::remove_dir_all(&folder);
}

#[tokio::test]
async fn background_without_terminal_backend_is_rejected() {
    let folder = std::env::temp_dir().join(format!("wf-test-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&folder).unwrap();
    let mut input = script_input(&with_meta("return 1;"));
    input.run_in_background = true;
    let err = TestRun::new(StubBackend::echo())
        .with_session_folder(folder.clone())
        .run(input)
        .await
        .expect_err("no terminal backend registered");
    assert!(err.contains("run_in_background: false"), "{err}");
    let _ = std::fs::remove_dir_all(&folder);
}

#[tokio::test]
async fn json_value_of_return_marker_is_not_duplicated_in_log() {
    // The __WF_RETURN__ marker is transport, not progress — it must not leak
    // into the rendered log tail.
    let backend = StubBackend::echo();
    let output = TestRun::new(backend)
        .run(script_input(&with_meta("return { secret: 'value-42' };")))
        .await
        .expect("runs");
    let log_tail = output
        .split("<log_tail>")
        .nth(1)
        .and_then(|rest| rest.split("</log_tail>").next())
        .unwrap_or("");
    assert!(!log_tail.contains("__WF_RETURN__"), "{output}");
}
