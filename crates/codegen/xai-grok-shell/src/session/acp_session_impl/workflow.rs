use std::sync::Arc;
use xai_grok_sampling_types::{ReasoningEffort, ReasoningEffortOption};

use super::super::acp_session::SessionActor;

impl SessionActor {
    pub(crate) fn named_workflow_snapshot(
        &self,
    ) -> (
        crate::session::workflow::registry::WorkflowRegistry,
        Vec<crate::session::workflow::registry::WorkflowListing>,
    ) {
        #[cfg(test)]
        crate::session::slash_authority::record_workflow_discovery_call();
        crate::session::workflow::registry::workflow_snapshot(Some(std::path::Path::new(
            self.session_info.cwd.as_str(),
        )))
    }

    pub(crate) fn workflow_listing_for_prompt(&self) -> Option<String> {
        self.workflow_listing_snapshot().map(|(text, _)| text)
    }

    pub(crate) fn workflow_listing_snapshot(&self) -> Option<(String, usize)> {
        if !self.background_workflows_enabled || self.startup_hints.is_subagent {
            return None;
        }
        let (_, workflows) = self.named_workflow_snapshot();
        let count = workflows.len();
        crate::session::workflow::listing::format_workflow_listing(&workflows)
            .map(|text| (text, count))
    }

    pub(crate) async fn launch_named_workflow(
        self: &Arc<Self>,
        registry: &crate::session::workflow::registry::WorkflowRegistry,
        name: &str,
        input: &str,
    ) -> String {
        let resolved = match registry.resolve_by_name(name) {
            Ok(r) => r,
            Err(e) => return format!("Workflow '{name}' unavailable: {e}"),
        };
        let sampling_config = self.chat_state_handle.get_sampling_config().await;
        let model_id = sampling_config
            .as_ref()
            .map(|config| config.model.as_str())
            .unwrap_or("unknown");
        let effort_options = self.models_manager.model_reasoning_efforts(model_id);
        let parsed =
            match parse_named_workflow_args(input, &resolved.meta.description, &effort_options) {
                Ok(parsed) => parsed,
                Err(error) => return format!("Could not start workflow '{name}': {error}"),
            };
        let spec = crate::session::workflow::manager::LaunchSpec {
            objective: parsed.objective,
            args: parsed.args,
            agent_budget: parsed.agent_budget,
            effort: parsed
                .effort
                .or_else(|| sampling_config.and_then(|config| config.reasoning_effort)),
            resume_run_id: None,
            resume_note: None,
        };
        let launched = self.workflow_manager.lock().await.launch(resolved, spec);
        match launched {
            Ok((run_id, outcome_rx)) => {
                let (display, objective) = self
                    .workflow_tracker()
                    .await
                    .lock()
                    .get(&run_id)
                    .map(|r| (r.name.clone(), r.objective.clone()))
                    .unwrap_or_else(|| (name.to_string(), String::new()));
                let command_line = if input.trim().is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {}", input.trim())
                };
                self.push_workflow_launch_reminder(
                    &display,
                    &run_id,
                    &objective,
                    &command_line,
                    false,
                );
                tokio::spawn(async move {
                    if let Ok(outcome) = outcome_rx.await {
                        tracing::info!(run_id, ?outcome, "named workflow finished");
                    }
                });
                format!(
                    "Workflow '{display}' started in the background. Watch it in /workflow runs; \
                     the result lands here when it finishes."
                )
            }
            Err(e) => format!("Could not start workflow '{name}': {e}"),
        }
    }

    pub(crate) async fn manage_workflow_run(self: &Arc<Self>, run_id: &str, op: &str) -> String {
        use crate::session::workflow::tracker::WorkflowRunStatus;

        const USAGE: &str = "Usage: /workflow <name> [args] to launch a saved workflow, \
                             /workflow runs (or bare /workflow) for a runs overview, or \
                             /workflow <op> [name] (also `/workflow <name> <op>`) to manage \
                             a run — ops: pause, resume, stop, save.";
        if run_id.is_empty() && (op.is_empty() || op == "runs") {
            let runs = {
                let tracker = self.workflow_tracker().await;
                let tracker = tracker.lock();
                let mut runs = tracker.list();
                for run in &mut runs {
                    run.elapsed_ms_floor = tracker.elapsed_ms(&run.run_id);
                }
                runs
            };
            return format_workflow_runs_overview(runs);
        }
        if op.is_empty() || op == "runs" {
            return USAGE.to_string();
        }
        let Some(op) = ManageOp::parse(op) else {
            return format!("Unknown op '{op}'. {USAGE}");
        };

        if run_id.is_empty() {
            let runs = {
                let tracker = self.workflow_tracker().await;
                tracker.lock().list()
            };
            let savable =
                savable_definition_names(std::path::Path::new(self.session_info.cwd.as_str()));
            return format_manage_needs_name(op, &runs, &savable);
        }

        let matches: Vec<(String, WorkflowRunStatus, String)> = {
            let tracker = self.workflow_tracker().await;
            let tracker = tracker.lock();
            let all: Vec<_> = tracker
                .list()
                .iter()
                .filter(|r| r.run_id.starts_with(run_id) || r.name.starts_with(run_id))
                .map(|r| (r.run_id.clone(), r.status, r.name.clone()))
                .collect();
            narrow_run_matches(all, run_id, op)
        };
        let (full_id, status, name) = match matches.as_slice() {
            [] => return format!("No workflow run matches '{run_id}'."),
            [one] => one.clone(),
            many => {
                let rows: Vec<String> = many
                    .iter()
                    .map(|(_, status, name)| format!("  {name} ({})", status.as_str()))
                    .collect();
                return format!(
                    "Several runs could be '{}' — pick one by name:\n{}\n(/workflow {} <name>)",
                    op.as_str(),
                    rows.join("\n"),
                    op.as_str(),
                );
            }
        };
        let id_suffix = format!(" {name}");

        match op {
            ManageOp::Pause => {
                if status != WorkflowRunStatus::Active {
                    return format!("Run '{name}' is not active (status: {}).", status.as_str());
                }
                self.workflow_manager.lock().await.pause(&full_id);
                format!("Paused {name}. /workflow resume{id_suffix} to continue.")
            }
            ManageOp::Stop => {
                if status.is_terminal() {
                    return format!(
                        "Run '{name}' is already finished (status: {}).",
                        status.as_str()
                    );
                }
                self.workflow_manager.lock().await.cancel(&full_id);
                format!("Stopped {name}.")
            }
            ManageOp::Resume => {
                if status == WorkflowRunStatus::Active {
                    return format!("Run '{name}' is already running.");
                }
                if !status.is_resumable() {
                    return format!(
                        "Run '{name}' cannot be resumed (status: {}). Start a new run instead.",
                        status.as_str()
                    );
                }
                if status == WorkflowRunStatus::BudgetLimited {
                    let (used, limit) = {
                        let tracker = self.workflow_tracker().await;
                        let tracker = tracker.lock();
                        let run = tracker.get(&full_id);
                        (
                            run.as_ref().map_or(0, |r| r.agents_used),
                            run.as_ref().and_then(|r| r.agent_budget),
                        )
                    };
                    let limit = limit.map_or_else(String::new, |l| format!("/{l}"));
                    if used >= xai_workflow::MAX_AGENT_BUDGET {
                        return format!(
                            "Run '{name}' exhausted the maximum agent budget ({used}{limit} agents) \
                             and cannot be resumed. Start a new run instead."
                        );
                    }
                    let suggested = used.saturating_add(64).min(xai_workflow::MAX_AGENT_BUDGET);
                    return format!(
                        "Run '{name}' exhausted its agent budget ({used}{limit} agents). \
                         Resuming keeps all finished work but needs a higher absolute cap — \
                         ask the agent to resume it with an agent budget above {used}, e.g. \
                         \"resume {name} with an agent budget of {suggested}\"."
                    );
                }
                let (script, args) = {
                    let manager = self.workflow_manager.lock().await;
                    (
                        manager.script_copy_for(&full_id),
                        manager.args_copy_for(&full_id),
                    )
                };
                let Some(script) = script else {
                    return format!("No persisted script for '{name}'; cannot resume.");
                };
                let resolved = match crate::session::workflow::registry::resolve_inline(script) {
                    Ok(r) => r,
                    Err(e) => return format!("Persisted script invalid: {e}"),
                };
                let objective = {
                    let tracker = self.workflow_tracker().await;
                    tracker
                        .lock()
                        .get(&full_id)
                        .map(|r| r.objective.clone())
                        .unwrap_or_default()
                };
                let agent_budget = {
                    let tracker = self.workflow_tracker().await;
                    tracker
                        .lock()
                        .get(&full_id)
                        .and_then(|run| run.agent_budget)
                };
                let objective_echo = objective.clone();
                let spec = crate::session::workflow::manager::LaunchSpec {
                    objective,
                    args,
                    agent_budget,
                    effort: None,
                    resume_run_id: Some(full_id.clone()),
                    resume_note: None,
                };
                match self.workflow_manager.lock().await.launch(resolved, spec) {
                    Ok((rid, outcome_rx)) => {
                        tokio::spawn(async move {
                            if let Ok(outcome) = outcome_rx.await {
                                tracing::info!(run_id = rid, ?outcome, "resumed workflow finished");
                            }
                        });
                        self.push_workflow_launch_reminder(
                            &name,
                            &full_id,
                            &objective_echo,
                            &format!("/workflow resume {name}"),
                            true,
                        );
                        format!("Resumed {name} from its journal.")
                    }
                    Err(e) => format!("Could not resume '{name}': {e}"),
                }
            }
            ManageOp::Save => {
                let Some(script) = self.workflow_manager.lock().await.script_copy_for(&full_id)
                else {
                    return format!("No persisted script for '{name}'; nothing to save.");
                };
                let definition_name =
                    match crate::session::workflow::registry::resolve_inline(script.clone()) {
                        Ok(resolved) => resolved.meta.name,
                        Err(error) => return format!("Could not save workflow '{name}': {error}"),
                    };
                if definition_name != name {
                    return format!(
                        "Save is disabled for run '{name}': it is a duplicate-run display handle, \
                         while the script is still named '{definition_name}'. Choose a new unique \
                         meta.name and save the script under that name instead."
                    );
                }
                if crate::session::workflow::registry::BUILTIN_WORKFLOWS
                    .iter()
                    .any(|builtin| builtin.name == definition_name)
                {
                    return format!(
                        "Save is disabled for built-in workflow '{definition_name}', which is \
                         already runnable. To customize it, create a copy with a new unique \
                         meta.name."
                    );
                }
                match crate::session::workflow::registry::save_project_workflow(
                    std::path::Path::new(self.session_info.cwd.as_str()),
                    &definition_name,
                    &script,
                ) {
                    Ok(path) => format!(
                        "Saved workflow '{definition_name}' to {} — runnable by name from now on.",
                        path.display()
                    ),
                    Err(e) => format!("Could not save workflow '{definition_name}': {e}"),
                }
            }
        }
    }
}

fn format_workflow_runs_overview(
    mut runs: Vec<crate::session::workflow::tracker::WorkflowRunState>,
) -> String {
    use crate::session::workflow::tracker::WorkflowRunStatus;
    use std::fmt::Write as _;

    if runs.is_empty() {
        return "No workflow runs in this session yet. Launch one with /workflow <name> [args]; \
                browse with /workflows."
            .to_string();
    }
    runs.reverse();
    runs.sort_by_key(|run| {
        (
            run.status.is_terminal(),
            run.status != WorkflowRunStatus::Active,
        )
    });

    let mut out = String::new();
    for run in &runs {
        let _ = write!(
            out,
            "- '{}' — {}",
            run.name,
            run.status.as_str().replace('_', " ")
        );
        if let Some(line) = super::reminders::workflow_phase_line(run) {
            let _ = write!(out, "\n  {line}");
        }
        if let Some(line) = super::reminders::workflow_agents_line(&run.agents) {
            let _ = write!(out, "\n  {line}");
        }
        let _ = write!(
            out,
            "\n  Elapsed: {}",
            super::reminders::format_workflow_elapsed(run.elapsed_ms_floor)
        );
        let objective = run
            .objective
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !objective.is_empty() {
            let _ = write!(
                out,
                "\n  Objective: {}",
                xai_grok_tools::util::truncate_str(
                    &objective,
                    super::reminders::WORKFLOW_OBJECTIVE_REMINDER_CAP
                )
            );
        }
        out.push('\n');
    }
    out.push_str("Manage with /workflow pause|resume|stop|save <name>.");
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManageOp {
    Pause,
    Resume,
    Stop,
    Save,
}

impl ManageOp {
    fn parse(input: &str) -> Option<Self> {
        match input.to_ascii_lowercase().as_str() {
            "pause" => Some(Self::Pause),
            "resume" => Some(Self::Resume),
            "stop" => Some(Self::Stop),
            "save" => Some(Self::Save),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Stop => "stop",
            Self::Save => "save",
        }
    }
}

fn savable_definition_names(session_cwd: &std::path::Path) -> std::collections::HashSet<String> {
    crate::session::workflow::registry::list_workflows(Some(session_cwd))
        .into_iter()
        .filter(|listing| listing.source != "builtin")
        .map(|listing| listing.name)
        .collect()
}

fn format_manage_needs_name(
    op: ManageOp,
    runs: &[crate::session::workflow::tracker::WorkflowRunState],
    savable_names: &std::collections::HashSet<String>,
) -> String {
    use crate::session::workflow::tracker::WorkflowRunStatus;
    if runs.is_empty() {
        return "No workflow runs in this session yet.".to_string();
    }
    let applicable: Vec<_> = runs
        .iter()
        .filter(|run| match op {
            ManageOp::Pause => run.status == WorkflowRunStatus::Active,
            ManageOp::Resume => run.status.is_resumable(),
            ManageOp::Stop => !run.status.is_terminal(),
            ManageOp::Save => savable_names.contains(&run.name),
        })
        .collect();
    if applicable.is_empty() {
        return format!("No runs to {}.", op.as_str());
    }
    let rows: Vec<String> = applicable
        .iter()
        .map(|run| format!("  {} ({})", run.name, run.status.as_str().replace('_', " ")))
        .collect();
    format!(
        "Say which run to {}:\n{}\n(/workflow {} <name>)",
        op.as_str(),
        rows.join("\n"),
        op.as_str(),
    )
}

pub(crate) struct NamedWorkflowArgs {
    pub args: serde_json::Value,
    pub objective: String,
    pub agent_budget: Option<u64>,
    pub effort: Option<ReasoningEffort>,
}

#[derive(serde::Deserialize)]
struct KnownLaunchArgs {
    #[serde(default)]
    objective: ObjectiveArg,
    #[serde(default)]
    query: ObjectiveArg,
    #[serde(default, deserialize_with = "deserialize_agent_budget")]
    agent_budget: Option<AgentBudget>,
    #[serde(default)]
    effort: Option<serde_json::Value>,
}

#[derive(Default, serde::Deserialize)]
#[serde(untagged)]
enum ObjectiveArg {
    Text(String),
    Other(serde_json::Value),
    #[default]
    Missing,
}

impl ObjectiveArg {
    fn resolve(self, query: Self) -> Option<String> {
        match self {
            Self::Text(text) => Some(text),
            Self::Other(value) => {
                drop(value);
                None
            }
            Self::Missing => match query {
                Self::Text(text) => Some(text),
                Self::Other(value) => {
                    drop(value);
                    None
                }
                Self::Missing => None,
            },
        }
    }
}

struct AgentBudget(u64);

impl AgentBudget {
    fn try_new(value: u64) -> Result<Self, String> {
        if value == 0 {
            return Err("`agent_budget` must be a positive integer".to_string());
        }
        if value > xai_workflow::MAX_AGENT_BUDGET {
            return Err(format!(
                "`agent_budget` must be at most {} agents",
                xai_workflow::MAX_AGENT_BUDGET
            ));
        }
        Ok(Self(value))
    }

    fn into_inner(self) -> u64 {
        self.0
    }
}

fn deserialize_agent_budget<'de, D>(deserializer: D) -> Result<Option<AgentBudget>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    let budget = value
        .as_u64()
        .ok_or_else(|| serde::de::Error::custom("`agent_budget` must be a positive integer"))?;
    AgentBudget::try_new(budget)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

struct WorkflowEffort(ReasoningEffort);

impl WorkflowEffort {
    fn try_new(value: &str, effort_options: &[ReasoningEffortOption]) -> Result<Self, String> {
        if let Ok(effort) = value.parse::<ReasoningEffort>() {
            return Ok(Self(effort));
        }
        effort_options
            .iter()
            .find(|option| {
                option.id.eq_ignore_ascii_case(value) || option.label.eq_ignore_ascii_case(value)
            })
            .map(|option| Self(option.value))
            .ok_or_else(|| format!("invalid workflow `effort`: unknown reasoning effort '{value}'"))
    }

    fn into_inner(self) -> ReasoningEffort {
        self.0
    }
}

pub(crate) fn parse_named_workflow_args(
    input: &str,
    description: &str,
    effort_options: &[ReasoningEffortOption],
) -> Result<NamedWorkflowArgs, String> {
    let input = input.trim();
    let (flag_budget, flag_effort, input) = parse_named_workflow_flags(input, effort_options)?;
    if input.is_empty() {
        return Ok(NamedWorkflowArgs {
            args: serde_json::Value::Null,
            objective: description.to_string(),
            agent_budget: flag_budget,
            effort: flag_effort,
        });
    }
    if let Ok(args @ serde_json::Value::Object(_)) =
        serde_json::from_str::<serde_json::Value>(input)
    {
        let known: KnownLaunchArgs =
            serde_json::from_value(args.clone()).map_err(|error| error.to_string())?;
        let objective = known
            .objective
            .resolve(known.query)
            .unwrap_or_else(|| input.to_string());
        let json_budget = known.agent_budget.map(AgentBudget::into_inner);
        let json_effort = known
            .effort
            .map(|value| {
                let effort = value
                    .as_str()
                    .ok_or_else(|| "`effort` must be a string".to_string())?;
                WorkflowEffort::try_new(effort, effort_options).map(WorkflowEffort::into_inner)
            })
            .transpose()?;
        if flag_budget.is_some() && json_budget.is_some() {
            return Err("set `agent_budget` once, using either the slash flag or JSON".to_string());
        }
        if flag_effort.is_some() && json_effort.is_some() {
            return Err("set `effort` once, using either the slash flag or JSON".to_string());
        }
        return Ok(NamedWorkflowArgs {
            args,
            objective,
            agent_budget: flag_budget.or(json_budget),
            effort: flag_effort.or(json_effort),
        });
    }
    Ok(NamedWorkflowArgs {
        args: serde_json::json!({ "query": input, "objective": input }),
        objective: input.to_string(),
        agent_budget: flag_budget,
        effort: flag_effort,
    })
}

fn parse_named_workflow_flags<'a>(
    mut input: &'a str,
    effort_options: &[ReasoningEffortOption],
) -> Result<(Option<u64>, Option<ReasoningEffort>, &'a str), String> {
    let mut agent_budget = None;
    let mut effort = None;
    loop {
        if let Some((value, remaining)) = parse_leading_arg(input, "agent-budget")? {
            if agent_budget.is_some() {
                return Err("set `--agent-budget` once".to_string());
            }
            let budget = value
                .parse::<u64>()
                .map_err(|_| "`--agent-budget` must be a positive integer".to_string())?;
            agent_budget = Some(AgentBudget::try_new(budget)?.into_inner());
            input = remaining;
        } else if let Some((value, remaining)) = parse_leading_arg(input, "effort")? {
            if effort.is_some() {
                return Err("set `--effort` once".to_string());
            }
            effort = Some(WorkflowEffort::try_new(value, effort_options)?.into_inner());
            input = remaining;
        } else {
            return Ok((agent_budget, effort, input));
        }
    }
}

fn parse_leading_arg<'a>(input: &'a str, name: &str) -> Result<Option<(&'a str, &'a str)>, String> {
    let flag = format!("--{name}");
    let Some(rest) = input.strip_prefix(&flag) else {
        return Ok(None);
    };
    let value_input = if let Some(rest) = rest.strip_prefix('=') {
        rest
    } else if rest.is_empty() {
        return Err(format!("`{flag}` requires a value"));
    } else if rest.chars().next().is_some_and(char::is_whitespace) {
        rest.trim_start()
    } else {
        return Ok(None);
    };
    if value_input.is_empty() {
        return Err(format!("`{flag}` requires a value"));
    }
    let (value, remaining) = value_input
        .split_once(char::is_whitespace)
        .map_or((value_input, ""), |(value, input)| {
            (value, input.trim_start())
        });
    Ok(Some((value, remaining)))
}

#[cfg(test)]
mod named_workflow_args_tests {
    use super::{
        NamedWorkflowArgs, ReasoningEffort, ReasoningEffortOption, parse_leading_arg,
        parse_named_workflow_args as parse_with_effort_options,
    };

    fn parse_named_workflow_args(
        input: &str,
        description: &str,
    ) -> Result<NamedWorkflowArgs, String> {
        parse_with_effort_options(input, description, &[])
    }

    fn remapped_effort_options() -> Vec<ReasoningEffortOption> {
        vec![ReasoningEffortOption {
            id: "deep".to_string(),
            value: ReasoningEffort::Xhigh,
            label: "Deep".to_string(),
            description: None,
            default: false,
        }]
    }

    #[test]
    fn typed_json_fields_preserve_objective_precedence() {
        let parsed = parse_named_workflow_args(
            r#"{"objective":"primary","query":"alias","extra":{"nested":true}}"#,
            "fallback",
        )
        .expect("valid args");
        assert_eq!(parsed.objective, "primary");
        assert_eq!(
            parsed.args,
            serde_json::json!({
                "objective": "primary",
                "query": "alias",
                "extra": {"nested": true},
            })
        );

        let alias =
            parse_named_workflow_args(r#"{"query":"alias"}"#, "fallback").expect("valid alias");
        assert_eq!(alias.objective, "alias");

        let non_text_objective =
            parse_named_workflow_args(r#"{"objective":null,"query":"alias"}"#, "fallback")
                .expect("valid non-text objective");
        assert_eq!(
            non_text_objective.objective,
            r#"{"objective":null,"query":"alias"}"#
        );
    }

    #[test]
    fn json_promotes_agent_budget_and_preserves_args() {
        let parsed = parse_named_workflow_args(
            r#"{"query":"review this","agent_budget":256,"target":"main"}"#,
            "fallback",
        )
        .expect("valid args");
        assert_eq!(parsed.objective, "review this");
        assert_eq!(parsed.agent_budget, Some(256));
        assert_eq!(parsed.effort, None);
        assert_eq!(
            parsed.args,
            serde_json::json!({
                "query": "review this",
                "agent_budget": 256,
                "target": "main",
            })
        );
    }

    #[test]
    fn slash_flag_promotes_budget_for_json_or_plain_args() {
        let json = parse_named_workflow_args(
            r#"--agent-budget 64 {"objective":"audit","target":"main"}"#,
            "fallback",
        )
        .expect("valid JSON args");
        assert_eq!(json.agent_budget, Some(64));
        assert_eq!(json.objective, "audit");
        assert_eq!(
            json.args,
            serde_json::json!({"objective": "audit", "target": "main"})
        );

        let plain = parse_named_workflow_args("--agent-budget=32 audit the release", "fallback")
            .expect("valid plain args");
        assert_eq!(plain.agent_budget, Some(32));
        assert_eq!(plain.objective, "audit the release");
        assert_eq!(
            plain.args,
            serde_json::json!({
                "query": "audit the release",
                "objective": "audit the release",
            })
        );
    }

    #[test]
    fn json_or_slash_flags_promote_effort() {
        let json =
            parse_named_workflow_args(r#"{"objective":"audit","effort":"HIGH"}"#, "fallback")
                .expect("valid JSON effort");
        assert_eq!(json.effort, Some(ReasoningEffort::High));

        for input in [
            "--effort medium --agent-budget 64 audit the release",
            "--agent-budget 64 --effort=medium audit the release",
        ] {
            let flags = parse_named_workflow_args(input, "fallback").expect("valid slash flags");
            assert_eq!(flags.effort, Some(ReasoningEffort::Medium));
            assert_eq!(flags.agent_budget, Some(64));
            assert_eq!(flags.objective, "audit the release");
        }
    }

    #[test]
    fn current_model_effort_aliases_canonicalize_for_all_flag_orders() {
        let options = remapped_effort_options();
        for input in [
            "--effort deep --agent-budget 64 audit",
            "--agent-budget 64 --effort Deep audit",
            "--effort=xhigh --agent-budget=64 audit",
            "--agent-budget=64 --effort=xhigh audit",
        ] {
            let parsed = parse_with_effort_options(input, "fallback", &options)
                .unwrap_or_else(|error| panic!("input={input:?}, error={error}"));
            assert_eq!(
                parsed.effort,
                Some(ReasoningEffort::Xhigh),
                "input={input:?}"
            );
            assert_eq!(parsed.agent_budget, Some(64), "input={input:?}");
            assert_eq!(parsed.objective, "audit", "input={input:?}");
        }

        let json = parse_with_effort_options(
            r#"{"objective":"audit","effort":"Deep"}"#,
            "fallback",
            &options,
        )
        .expect("current-model label must canonicalize");
        assert_eq!(json.effort, Some(ReasoningEffort::Xhigh));

        for input in ["--effort turbo audit", r#"{"effort":"turbo"}"#] {
            let error = parse_with_effort_options(input, "fallback", &options)
                .err()
                .unwrap_or_else(|| panic!("input={input:?} should fail"));
            assert!(error.contains("invalid workflow `effort`"), "{error}");
        }
    }

    #[test]
    fn absent_budget_keeps_default_launch_behavior() {
        let empty = parse_named_workflow_args("", "fallback").expect("empty args");
        assert_eq!(empty.agent_budget, None);
        assert_eq!(empty.effort, None);
        assert_eq!(empty.objective, "fallback");
        assert_eq!(empty.args, serde_json::Value::Null);

        let plain = parse_named_workflow_args("audit", "fallback").expect("plain args");
        assert_eq!(plain.agent_budget, None);
        assert_eq!(plain.objective, "audit");
    }

    #[test]
    fn invalid_budgets_are_rejected() {
        for (input, expected) in [
            (r#"{"agent_budget":0}"#, "positive integer"),
            (r#"{"agent_budget":1025}"#, "at most 1024"),
            (r#"{"agent_budget":"64"}"#, "positive integer"),
            ("--agent-budget nope audit", "positive integer"),
            ("--agent-budget", "requires a value"),
            (r#"{"effort":"turbo"}"#, "invalid workflow `effort`"),
            (r#"{"effort":3}"#, "must be a string"),
            ("--effort turbo audit", "invalid workflow `effort`"),
            ("--effort", "requires a value"),
        ] {
            let error = parse_named_workflow_args(input, "fallback")
                .err()
                .unwrap_or_else(|| panic!("{input:?} should fail"));
            assert!(error.contains(expected), "input={input:?}, error={error}");
        }
    }

    #[test]
    fn duplicate_flag_and_json_launch_fields_are_rejected() {
        let budget =
            parse_named_workflow_args(r#"--agent-budget 64 {"agent_budget":128}"#, "fallback")
                .err()
                .expect("duplicate budget must fail");
        assert!(budget.contains("set `agent_budget` once"), "{budget}");

        let effort = parse_named_workflow_args(r#"--effort low {"effort":"high"}"#, "fallback")
            .err()
            .expect("duplicate effort must fail");
        assert!(effort.contains("set `effort` once"), "{effort}");
    }

    #[test]
    fn duplicate_slash_effort_flags_are_rejected() {
        for input in [
            "--effort low --effort high audit",
            "--effort=low --effort=high audit",
        ] {
            let error = parse_named_workflow_args(input, "fallback")
                .err()
                .unwrap_or_else(|| panic!("{input:?} should fail"));
            assert_eq!(error, "set `--effort` once", "input={input:?}");
        }
    }

    #[test]
    fn duplicate_slash_budget_flags_are_rejected() {
        for input in [
            "--agent-budget 32 --agent-budget 64 audit",
            "--agent-budget 32 --agent-budget=64 audit",
            "--agent-budget=32 --agent-budget 64 audit",
            "--agent-budget=32 --agent-budget=64 audit",
        ] {
            let error = parse_named_workflow_args(input, "fallback")
                .err()
                .unwrap_or_else(|| panic!("{input:?} should fail"));
            assert_eq!(error, "set `--agent-budget` once", "input={input:?}");
        }
    }

    #[test]
    fn whitespace_delimits_slash_budget_value() {
        for whitespace in ["\t", "\n", "\r\n", "\u{2003}"] {
            let input = format!("--agent-budget{whitespace}64{whitespace}audit");
            let parsed = parse_named_workflow_args(&input, "fallback")
                .unwrap_or_else(|error| panic!("input={input:?}, error={error}"));
            assert_eq!(parsed.agent_budget, Some(64), "input={input:?}");
            assert_eq!(parsed.objective, "audit", "input={input:?}");
        }
    }

    #[test]
    fn generic_leading_arg_supports_equals_whitespace_and_missing_values() {
        assert_eq!(
            parse_leading_arg("--effort=high audit", "effort").expect("valid equals arg"),
            Some(("high", "audit"))
        );
        for whitespace in [" ", "\t", "\n", "\r\n", "\u{2003}"] {
            let input = format!("--effort{whitespace}high{whitespace}audit");
            assert_eq!(
                parse_leading_arg(&input, "effort").expect("valid whitespace arg"),
                Some(("high", "audit")),
                "input={input:?}"
            );
        }
        assert_eq!(
            parse_leading_arg("--unknown value", "effort").expect("different flag"),
            None
        );
        assert_eq!(
            parse_leading_arg("--effort", "effort").expect_err("missing value"),
            "`--effort` requires a value"
        );
        assert_eq!(
            parse_leading_arg("--effort=", "effort").expect_err("missing equals value"),
            "`--effort` requires a value"
        );
    }
}

type RunMatch = (
    String,
    crate::session::workflow::tracker::WorkflowRunStatus,
    String,
);

fn narrow_run_matches(mut all: Vec<RunMatch>, selector: &str, op: ManageOp) -> Vec<RunMatch> {
    use crate::session::workflow::tracker::WorkflowRunStatus;
    if selector.is_empty() {
        return all;
    }
    let exact: Vec<_> = all
        .iter()
        .filter(|(id, _, name)| id.as_str() == selector || name.as_str() == selector)
        .cloned()
        .collect();
    if !exact.is_empty() {
        all = exact;
    }
    if all.len() > 1 {
        let applicable: Vec<_> = all
            .iter()
            .filter(|(_, status, ..)| match op {
                ManageOp::Pause => *status == WorkflowRunStatus::Active,
                ManageOp::Resume => status.is_resumable(),
                ManageOp::Stop => !status.is_terminal(),
                ManageOp::Save => true,
            })
            .cloned()
            .collect();
        if applicable.len() == 1 {
            return applicable;
        }
    }
    all
}

#[cfg(test)]
mod overview_tests {
    use super::{ManageOp, format_manage_needs_name, format_workflow_runs_overview};
    use crate::session::workflow::tracker::{
        WorkflowAgentRow, WorkflowRunState, WorkflowRunStatus, WorkflowTracker,
    };

    fn tracked_runs(names: &[&str]) -> Vec<WorkflowRunState> {
        let mut tracker = WorkflowTracker::default();
        for (index, name) in names.iter().enumerate() {
            tracker.start_run(
                format!("wf_{index}"),
                (*name).to_string(),
                String::new(),
                vec![],
                None,
                None,
            );
        }
        tracker.list()
    }

    fn agent(id: &str, state: &str) -> WorkflowAgentRow {
        WorkflowAgentRow {
            agent_id: id.into(),
            label: id.into(),
            phase: None,
            model: None,
            state: state.into(),
            tokens_used: 0,
            duration_ms: 0,
        }
    }

    #[test]
    fn bare_stop_lists_stoppable_runs_instead_of_picking_one() {
        let mut runs = tracked_runs(&["review-pr", "review-pr-2"]);
        runs[0].status = WorkflowRunStatus::Complete;
        let text = format_manage_needs_name(ManageOp::Stop, &runs, &Default::default());
        assert!(text.starts_with("Say which run to stop:"), "{text}");
        assert!(text.contains("review-pr-2"), "{text}");
        assert!(!text.contains("review-pr ("), "{text}");
        assert!(text.contains("/workflow stop <name>"), "{text}");
        assert!(!text.contains("wf_"), "run ids must not surface: {text}");
    }

    #[test]
    fn bare_pause_with_only_finished_runs_does_not_list_them() {
        let mut runs = tracked_runs(&["done"]);
        runs[0].status = WorkflowRunStatus::Complete;
        assert_eq!(
            format_manage_needs_name(ManageOp::Pause, &runs, &Default::default()),
            "No runs to pause."
        );
    }

    #[test]
    fn bare_save_lists_only_catalog_definition_names() {
        let runs = tracked_runs(&["review-pr", "review-pr-2", "sprint-2"]);
        let savable = ["review-pr", "sprint-2"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let text = format_manage_needs_name(ManageOp::Save, &runs, &savable);
        assert!(text.contains("review-pr ("), "{text}");
        assert!(text.contains("sprint-2"), "{text}");
        assert!(!text.contains("review-pr-2"), "{text}");
    }

    #[test]
    fn bare_stop_with_no_runs_says_so() {
        assert_eq!(
            format_manage_needs_name(ManageOp::Stop, &[], &Default::default()),
            "No workflow runs in this session yet."
        );
    }

    #[test]
    fn nameless_operations_require_a_name_even_for_one_run() {
        for (op, status) in [
            (ManageOp::Pause, WorkflowRunStatus::Active),
            (ManageOp::Stop, WorkflowRunStatus::Active),
            (ManageOp::Resume, WorkflowRunStatus::UserPaused),
            (ManageOp::Save, WorkflowRunStatus::Complete),
        ] {
            let mut runs = tracked_runs(&["only-run"]);
            runs[0].status = status;
            let savable = ["only-run".to_owned()].into_iter().collect();
            let text = format_manage_needs_name(op, &runs, &savable);
            assert!(text.starts_with(&format!("Say which run to {}:", op.as_str())));
            assert!(text.contains(&format!("/workflow {} <name>", op.as_str())));
            assert!(text.contains("only-run"));
            assert_eq!(runs[0].status, status);
        }
    }

    #[test]
    fn empty_overview_hints_launch_and_catalog() {
        assert_eq!(
            format_workflow_runs_overview(vec![]),
            "No workflow runs in this session yet. Launch one with /workflow <name> [args]; \
             browse with /workflows."
        );
    }

    #[test]
    fn overview_orders_active_first_then_recency_without_run_ids() {
        let mut runs = tracked_runs(&["old-active", "waiting", "done-run", "new-active"]);
        runs[1].status = WorkflowRunStatus::UserPaused;
        runs[2].status = WorkflowRunStatus::Complete;
        let text = format_workflow_runs_overview(runs);
        let pos = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing from {text}"))
        };
        assert!(pos("'new-active'") < pos("'old-active'"));
        assert!(pos("'old-active'") < pos("'waiting'"));
        assert!(pos("'waiting'") < pos("'done-run'"));
        assert!(!text.contains("wf_"), "run ids must not surface: {text}");
        assert!(text.ends_with("Manage with /workflow pause|resume|stop|save <name>."));
    }

    #[test]
    fn overview_run_details_render_phase_agents_elapsed_objective() {
        let mut runs = tracked_runs(&["builder"]);
        runs[0].objective = "ship  the\tthing".into();
        runs[0].phases = vec![
            xai_workflow::PhaseMeta {
                title: "plan".into(),
                detail: None,
            },
            xai_workflow::PhaseMeta {
                title: "build".into(),
                detail: None,
            },
        ];
        runs[0].current_phase = Some("build".into());
        runs[0].elapsed_ms_floor = 61_000;
        runs[0].agents = vec![
            agent("a1", "done"),
            agent("a2", "running"),
            agent("a3", "failed"),
        ];
        let text = format_workflow_runs_overview(runs);
        assert!(text.contains("- 'builder' — active"), "{text}");
        assert!(text.contains("Phase: build (2/2)"), "{text}");
        assert!(
            text.contains("Agents: 1 done, 1 running, 1 failed"),
            "{text}"
        );
        assert!(text.contains("Elapsed: 1m 1s"), "{text}");
        assert!(
            text.contains("Objective: ship the thing"),
            "objective must be whitespace-collapsed to one line: {text}"
        );
    }

    #[test]
    fn overview_humanizes_paused_status_and_falls_back_on_stale_phase() {
        let mut runs = tracked_runs(&["stuck"]);
        runs[0].status = WorkflowRunStatus::NoProgressPaused;
        runs[0].current_phase = Some("ghost".into());
        let text = format_workflow_runs_overview(runs);
        assert!(text.contains("- 'stuck' — no progress paused"), "{text}");
        assert!(!text.contains("no_progress_paused"), "{text}");
        assert!(text.contains("Phase: ghost"), "{text}");
        assert!(!text.contains("(1/0)"), "{text}");
    }

    #[test]
    fn overview_caps_objective_at_reminder_cap() {
        let mut runs = tracked_runs(&["chatty"]);
        runs[0].objective = "x".repeat(300);
        let text = format_workflow_runs_overview(runs);
        assert!(
            text.contains(&format!("Objective: {}", "x".repeat(256))),
            "objective must keep the first 256 chars: {text}"
        );
        assert!(!text.contains(&"x".repeat(257)), "{text}");
    }
}

#[cfg(test)]
mod run_match_tests {
    use super::{ManageOp, narrow_run_matches};
    use crate::session::workflow::tracker::WorkflowRunStatus;

    fn run(id: &str, name: &str, status: WorkflowRunStatus) -> super::RunMatch {
        (id.to_string(), status, name.to_string())
    }

    #[test]
    fn exact_name_beats_prefix_of_uniquified_sibling() {
        let all = vec![
            run("wf_1", "deep-research", WorkflowRunStatus::Active),
            run("wf_2", "deep-research-2", WorkflowRunStatus::Active),
        ];
        let picked = narrow_run_matches(all, "deep-research", ManageOp::Stop);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].2, "deep-research");
    }

    #[test]
    fn prefix_still_narrows_by_op_applicability() {
        let all = vec![
            run("wf_1", "deep-research", WorkflowRunStatus::Complete),
            run("wf_2", "deep-research-2", WorkflowRunStatus::Active),
        ];
        let picked = narrow_run_matches(all, "deep", ManageOp::Stop);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].2, "deep-research-2");
    }

    #[test]
    fn empty_selector_does_not_auto_pick_the_only_applicable_run() {
        let all = vec![
            run("wf_1", "a", WorkflowRunStatus::Complete),
            run("wf_2", "b", WorkflowRunStatus::UserPaused),
        ];
        let picked = narrow_run_matches(all, "", ManageOp::Resume);
        assert_eq!(picked.len(), 2);
    }

    #[test]
    fn failed_run_is_applicable_for_resume_narrowing() {
        let all = vec![
            run("wf_1", "a", WorkflowRunStatus::Complete),
            run("wf_2", "b", WorkflowRunStatus::Failed),
        ];
        let picked = narrow_run_matches(all, "b", ManageOp::Resume);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].2, "b");
    }

    #[test]
    fn ambiguous_stays_ambiguous() {
        let all = vec![
            run("wf_1", "a", WorkflowRunStatus::Active),
            run("wf_2", "b", WorkflowRunStatus::Active),
        ];
        assert_eq!(narrow_run_matches(all, "", ManageOp::Stop).len(), 2);
    }
}
