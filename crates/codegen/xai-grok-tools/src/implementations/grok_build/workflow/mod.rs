use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

use super::task::types::SubagentDepthCounter;

pub use xai_grok_tools_api::slash_commands::WORKFLOW_TOOL_NAME;

pub fn workflow_tool_short_name(id: &str) -> &str {
    id.rsplit(':').next().unwrap_or(id)
}

pub fn is_workflow_tool_id(id: &str) -> bool {
    workflow_tool_short_name(id) == WORKFLOW_TOOL_NAME
}

pub fn is_workflow_tool(kind: Option<ToolKind>, id: &str) -> bool {
    kind == Some(ToolKind::Workflow) || is_workflow_tool_id(id)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowSource {
    Name { name: String },
    Script { script: String },
    ScriptPath { script_path: String },
    Resume { resume_from_run_id: String },
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct WorkflowToolInputWire {
    #[schemars(required)]
    source: Option<WorkflowSource>,
    #[serde(default)]
    #[schemars(range(min = 1, max = 1024))]
    agent_budget: Option<u64>,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    validate_only: bool,
    #[serde(default)]
    resume_note: Option<String>,
    #[serde(default)]
    #[schemars(skip)]
    name: Option<String>,
    #[serde(default)]
    #[schemars(skip)]
    script: Option<String>,
    #[serde(default)]
    #[schemars(skip)]
    script_path: Option<String>,
    #[serde(default)]
    #[schemars(skip)]
    resume_from_run_id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(try_from = "WorkflowToolInputWire")]
pub struct WorkflowToolInput {
    #[serde(default)]
    #[schemars(
        range(min = 1, max = 1024),
        description = "Absolute cumulative cap on logical child-agent calls for this run. Every agent() and every parallel() item consumes one slot; schema retries do not. Defaults to 128 and may be set from 1 through 1,024. A panel that would exceed the remaining budget is rejected before any of its children launch."
    )]
    pub agent_budget: Option<u64>,

    #[serde(default)]
    #[schemars(
        description = "Name of a registered workflow (built-in, or discovered from the project `.opengrok/workflows/` or user `~/.opengrok/workflows/`). Exactly one of `name`, `script`, or `script_path` must be set."
    )]
    pub name: Option<String>,

    #[serde(default)]
    #[schemars(
        description = "Inline Rhai workflow script. It must start with a pure-literal `let meta = #{ name: ..., description: ... };` map. Before authoring, read the `create-workflow` skill's SKILL.md. Run the path-specific `validate_only` smoke check with representative args."
    )]
    pub script: Option<String>,

    #[serde(default)]
    #[schemars(description = "Path to a .rhai workflow script on disk.")]
    pub script_path: Option<String>,

    #[serde(default)]
    #[schemars(
        description = "JSON value bound to the script's `args` global. Use an object for named arguments."
    )]
    pub args: Option<serde_json::Value>,

    #[serde(default)]
    #[schemars(
        description = "Resume a same-process paused run, continuing its original immutable script and args; do not also pass name, script, script_path, or args. A budget-limited run resumes only when agent_budget is passed with a higher cap. Process-restart interruptions are terminal."
    )]
    pub resume_from_run_id: Option<String>,

    #[serde(default)]
    #[schemars(
        description = "Only with resume_from_run_id, for a run that is blocked at escalate(): a short note describing how the blocking issue was resolved. The script's pending escalate() call returns this text and the run continues. Rejected when the run is not waiting at an escalation."
    )]
    pub resume_note: Option<String>,

    #[serde(default)]
    #[schemars(
        description = "Run a path-specific smoke check without launching: validate metadata, compile the full script, and execute the single path selected by the supplied args and canned host results. It does not exercise every branch or prove live tools and agent outputs work."
    )]
    pub validate_only: bool,
}

impl TryFrom<WorkflowToolInputWire> for WorkflowToolInput {
    type Error = String;

    fn try_from(wire: WorkflowToolInputWire) -> Result<Self, Self::Error> {
        let mut input = Self {
            agent_budget: wire.agent_budget,
            name: wire.name,
            script: wire.script,
            script_path: wire.script_path,
            args: wire.args,
            resume_from_run_id: wire.resume_from_run_id,
            resume_note: wire.resume_note,
            validate_only: wire.validate_only,
        };
        input.normalize();
        if let Some(source) = wire.source {
            if input.name.is_some()
                || input.script.is_some()
                || input.script_path.is_some()
                || input.resume_from_run_id.is_some()
            {
                return Err(
                    "`source` cannot be combined with legacy workflow source fields".into(),
                );
            }
            match source {
                WorkflowSource::Name { name } => input.name = Some(name),
                WorkflowSource::Script { script } => input.script = Some(script),
                WorkflowSource::ScriptPath { script_path } => input.script_path = Some(script_path),
                WorkflowSource::Resume { resume_from_run_id } => {
                    input.resume_from_run_id = Some(resume_from_run_id);
                }
            }
        }
        input.validate()?;
        Ok(input)
    }
}

impl WorkflowToolInput {
    pub const MAX_AGENT_BUDGET: u64 = 1_024;
    pub const MAX_RESUME_NOTE_BYTES: usize = 16 * 1024;

    pub fn normalize(&mut self) {
        self.name = blank_to_none(self.name.take());
        self.script = blank_to_none(self.script.take());
        self.script_path = blank_to_none(self.script_path.take());
        self.resume_from_run_id = blank_to_none(self.resume_from_run_id.take());
        // resume_note is deliberately NOT blanked to None: "" would silently
        // become a noteless resume, which consumes the escalation with an
        // empty answer. validate() rejects blank notes instead.
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(budget) = self.agent_budget {
            if budget == 0 {
                return Err("`agent_budget` must be a positive integer".into());
            }
            if budget > Self::MAX_AGENT_BUDGET {
                return Err(format!(
                    "`agent_budget` must be at most {} agents",
                    Self::MAX_AGENT_BUDGET
                ));
            }
        }
        if let Some(note) = self.resume_note.as_deref() {
            if note.trim().is_empty() {
                return Err(
                    "`resume_note` must not be blank; omit it entirely to resume without \
                     answering the escalation"
                        .into(),
                );
            }
            if self.resume_from_run_id.as_deref().is_none() {
                return Err(
                    "`resume_note` answers a blocked run's escalate(); it requires \
                     `resume_from_run_id`"
                        .into(),
                );
            }
            if self.validate_only {
                return Err(
                    "`resume_note` answers a real blocked run; it cannot be combined with \
                     `validate_only`"
                        .into(),
                );
            }
            if note.len() > Self::MAX_RESUME_NOTE_BYTES {
                return Err(format!(
                    "`resume_note` must be at most {} bytes",
                    Self::MAX_RESUME_NOTE_BYTES
                ));
            }
        }
        let present = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.trim().is_empty());
        let sources = [
            present(&self.name),
            present(&self.script),
            present(&self.script_path),
        ]
        .iter()
        .filter(|v| **v)
        .count();
        if present(&self.resume_from_run_id) {
            if self.args.is_some() {
                return Err(
                    "resume uses immutable source and arguments; do not provide `args`".into(),
                );
            }
            if self.validate_only {
                return Err("`validate_only` cannot be used when resuming a run".into());
            }
            return match sources {
                0 => Ok(()),
                _ => Err(
                    "`resume_from_run_id` continues a same-process paused run's original immutable script and args; do not combine it with `name`, `script`, or `script_path`"
                        .into(),
                ),
            };
        }
        match sources {
            0 => Err("provide one of `name`, `script`, or `script_path`".into()),
            1 => Ok(()),
            _ => Err("`name`, `script`, and `script_path` are mutually exclusive".into()),
        }
    }
}

fn blank_to_none(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

#[derive(Debug)]
pub struct WorkflowLaunchRequest {
    pub input: WorkflowToolInput,
}

#[derive(Debug)]
pub enum WorkflowLaunchAck {
    Started {
        run_id: String,
        task_id: String,
        name: String,
        script_path: Option<String>,
    },
    Validated {
        name: String,
        phases: usize,
        summary: String,
    },
    Rejected {
        code: &'static str,
        detail: String,
    },
}

pub type WorkflowLaunchEnvelope = (
    WorkflowLaunchRequest,
    tokio::sync::oneshot::Sender<WorkflowLaunchAck>,
);

pub struct WorkflowLaunchHandle(pub tokio::sync::mpsc::UnboundedSender<WorkflowLaunchEnvelope>);

impl std::fmt::Debug for WorkflowLaunchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowLaunchHandle").finish()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct WorkflowToolOutput {
    pub run_id: String,
    #[schemars(
        description = "Alias of run_id; workflow runs are not background tasks — do not pass to task_output/wait_tasks. Completion notifies automatically."
    )]
    pub task_id: String,
    #[schemars(
        description = "The session-unique display handle for this run, such as review-changes or review-changes-2. Use it in user-facing status and /workflow management; keep run_id internal."
    )]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
    pub message: String,
}

impl xai_tool_runtime::ToolOutput for WorkflowToolOutput {}

#[derive(Debug, Default)]
pub struct WorkflowTool;

impl crate::types::tool_metadata::ToolMetadata for WorkflowTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Workflow
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r##"Launch a workflow: a Rhai script that orchestrates subagents as one background run. Provide exactly one tagged `source`: `type: "name"` with a registered `name`, `type: "script"` with an inline `script`, `type: "script_path"` with a `script_path`, or `type: "resume"` with `resume_from_run_id`. Registered workflows come from built-ins, project `.opengrok/workflows/`, or user `~/.opengrok/workflows/`. Optionally pass `args` (bound to the script's `args`) and `agent_budget`, an absolute cap on cumulative child-agent calls: every agent() and parallel() item consumes one slot (schema retries do not); default 128. The host also caps live children per run (32 by default, host-configured) — larger parallel() panels are queued and still act as a barrier. The call returns immediately; progress appears in `/workflow runs`${%- if system_reminders_enabled %} and completion is reported automatically — do not poll or sleep-wait${%- endif %}.


Prefer a registered workflow when one fits; author a script for bounded fan-out over a known work list, staged research and verification, or several independent perspectives. Before writing or editing a script, read the `create-workflow` skill's SKILL.md. `validate_only: true` runs a path-specific smoke check (metadata, compile, one canned-host path) — not proof that every branch or live tool works.

A started run gets a session-unique display name (e.g. `review-changes`, `review-changes-2`) — the handle to show the user and use with `/workflow pause|resume|stop <name>`; keep run IDs internal. Each launch persists an editable `script_path`; edit it and launch as a new run to iterate. A run that pauses itself (any kind except `user`), blocks on `escalate()`, or fails does NOT just stop: you are woken with the blocking issue and resume instructions. Fix what you can (environment, missing input, open decision), then resume with `resume_from_run_id` — completed agents replay from the journal, a failed step re-executes, and when the run asked via `escalate()` pass `resume_note` so the script receives your answer and continues (`resume_note` is only accepted while the run is blocked at an escalation). A plain `pause()` replays deterministically — a pause about missing launch input needs a corrected NEW run, not a resume. A budget-limited run resumes only with a higher `agent_budget`; process-restart interruptions are terminal. Save reusable scripts to `.opengrok/workflows/<name>.rhai`."##
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }

    fn is_read_only(&self) -> bool {
        false
    }
}

impl xai_tool_runtime::Tool for WorkflowTool {
    type Args = WorkflowToolInput;
    type Output = WorkflowToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(WORKFLOW_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            WORKFLOW_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            tool_scope: Some(xai_tool_protocol::ToolScope::Write),
            ..Default::default()
        }
    }

    #[tracing::instrument(name = "new_tool.workflow", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        mut input: WorkflowToolInput,
    ) -> Result<WorkflowToolOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        input.normalize();

        if let Err(detail) = input.validate() {
            return Err(xai_tool_runtime::ToolError::custom(
                "workflow_invalid_input",
                detail,
            ));
        }

        let (depth, sender) = {
            let res = resources.lock().await;
            let depth = res.get::<SubagentDepthCounter>().map(|d| d.0).unwrap_or(0);
            let sender = res.get::<WorkflowLaunchHandle>().map(|h| h.0.clone());
            (depth, sender)
        };

        // Workflows stay top-level-only regardless of configurable subagent depth.
        if depth > 0 {
            return Err(xai_tool_runtime::ToolError::custom(
                "workflow_depth_exceeded",
                "Workflows can only be launched from a top-level session (subagents and \
                 workflow-spawned agents cannot start workflows)",
            ));
        }

        let sender = sender.ok_or_else(|| {
            xai_tool_runtime::ToolError::custom(
                "workflow_not_available",
                "Workflow launching is not available in this session (WorkflowLaunchHandle not \
                 registered)",
            )
        })?;

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<WorkflowLaunchAck>();
        sender
            .send((WorkflowLaunchRequest { input }, ack_tx))
            .map_err(|_| {
                xai_tool_runtime::ToolError::custom(
                    "workflow_channel_closed",
                    "Workflow launch channel closed — the session may be shutting down",
                )
            })?;

        match ack_rx.await {
            Ok(WorkflowLaunchAck::Started {
                run_id,
                task_id,
                name,
                script_path,
            }) => Ok(WorkflowToolOutput {
                message: {
                    let iterate = script_path
                        .as_deref()
                        .map(|p| {
                            format!(
                                " The editable script projection is at {p}. Edit it and launch \
                                 that `script_path` as a new run to iterate; same-process pause \
                                 resume continues only this run's original immutable source."
                            )
                        })
                        .unwrap_or_default();
                    format!(
                        "Workflow '{name}' started in the background. Progress appears in \
                         /workflow runs and completion is reported automatically. '{name}' is the \
                         session-unique display handle for user-facing status and /workflow \
                         management; keep the structured run id internal.{iterate}"
                    )
                },
                run_id,
                task_id,
                name,
                script_path,
            }),
            Ok(WorkflowLaunchAck::Validated {
                name,
                phases,
                summary,
            }) => Ok(WorkflowToolOutput {
                message: format!(
                    "Smoke check passed for workflow '{name}' ({phases} declared phases; \
                     canned-host path {summary}). This did not launch the workflow and did not \
                     exercise every branch or live dependency. Offer a real run next."
                ),
                run_id: String::new(),
                task_id: String::new(),
                name,
                script_path: None,
            }),
            Ok(WorkflowLaunchAck::Rejected { code, detail }) => {
                Err(xai_tool_runtime::ToolError::custom(code, detail))
            }
            Err(_) => Err(xai_tool_runtime::ToolError::custom(
                "workflow_launch_no_ack",
                "The session dropped the launch channel before answering; the workflow may not \
                 have started.",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_source_keeps_legacy_launch_and_escalation_compatibility() {
        let named: WorkflowToolInput = serde_json::from_value(serde_json::json!({
            "source": {"type": "name", "name": "deep-research"},
            "agent_budget": 32,
        }))
        .unwrap();
        assert_eq!(named.name.as_deref(), Some("deep-research"));
        let legacy: WorkflowToolInput = serde_json::from_value(serde_json::json!({
            "name": "deep-research",
        }))
        .unwrap();
        assert_eq!(legacy.name, named.name);
        let resume: WorkflowToolInput = serde_json::from_value(serde_json::json!({
            "source": {"type": "resume", "resume_from_run_id": "wf_resume"},
            "resume_note": "The blocked dependency is available.",
        }))
        .unwrap();
        assert_eq!(resume.resume_from_run_id.as_deref(), Some("wf_resume"));
        assert!(resume.resume_note.is_some());
        for invalid in [
            serde_json::json!({"source": {"type": "name", "name": "deep-research"}, "script": "complete(1);"}),
            serde_json::json!({"source": {"type": "name", "name": ""}}),
            serde_json::json!({"source": {"type": "resume", "resume_from_run_id": "wf_resume"}, "args": {}}),
            serde_json::json!({"source": {"type": "resume", "resume_from_run_id": "wf_resume"}, "validate_only": true}),
            serde_json::json!({"name": "deep-research", "script": "complete(1);"}),
        ] {
            assert!(serde_json::from_value::<WorkflowToolInput>(invalid).is_err());
        }
    }

    #[test]
    fn workflow_schema_requires_tagged_source_and_keeps_resume_note() {
        let schema = crate::registry::types::generate_schema::<WorkflowToolInput>();
        let properties = schema["properties"].as_object().unwrap();
        assert!(properties.contains_key("source"));
        assert!(properties.contains_key("resume_note"));
        for legacy_field in ["name", "script", "script_path", "resume_from_run_id"] {
            assert!(!properties.contains_key(legacy_field));
        }
        assert_eq!(schema["required"], serde_json::json!(["source"]));
        let source = &properties["source"];
        let variants = source["oneOf"]
            .as_array()
            .expect("source must be a tagged union");
        assert_eq!(variants.len(), 4);
        for (tag, field) in [
            ("name", "name"),
            ("script", "script"),
            ("script_path", "script_path"),
            ("resume", "resume_from_run_id"),
        ] {
            let variant = variants
                .iter()
                .find(|variant| {
                    let discriminator = &variant["properties"]["type"];
                    discriminator["const"] == tag
                        || discriminator["enum"] == serde_json::json!([tag])
                })
                .unwrap_or_else(|| panic!("missing source variant {tag}: {source}"));
            assert_eq!(variant["type"], "object");
            assert_eq!(variant["additionalProperties"], false);
            let required = variant["required"].as_array().unwrap();
            assert_eq!(required.len(), 2);
            assert!(required.iter().any(|value| value == "type"));
            assert!(required.iter().any(|value| value == field));
            assert_eq!(variant["properties"][field]["type"], "string");
        }
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"source": null}),
            serde_json::json!({"source": {"name": "deep-research"}}),
            serde_json::json!({"source": {"type": "unknown", "name": "deep-research"}}),
            serde_json::json!({"source": {"type": "name"}}),
            serde_json::json!({"source": {"type": "name", "name": "deep-research", "script": "complete(1);"}}),
        ] {
            assert!(
                serde_json::from_value::<WorkflowToolInput>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn workflow_detection_includes_kindless_ids() {
        assert!(is_workflow_tool(None, "GrokBuild:workflow"));
        assert!(is_workflow_tool(Some(ToolKind::Workflow), "custom-name"));
        assert!(!is_workflow_tool(None, "workflow_other"));
    }

    #[test]
    fn validation_requires_exactly_one_source_and_bounded_positive_budget() {
        let base = WorkflowToolInput {
            agent_budget: None,
            name: None,
            script: None,
            script_path: None,
            args: None,
            resume_from_run_id: None,
            resume_note: None,
            validate_only: false,
        };
        assert!(base.validate().is_err());

        let named = WorkflowToolInput {
            name: Some("deep-research".into()),
            ..base.clone()
        };
        assert!(named.validate().is_ok());

        let both = WorkflowToolInput {
            name: Some("goal".into()),
            script: Some("let meta = #{};".into()),
            ..base.clone()
        };
        assert!(both.validate().is_err());

        let resume_only = WorkflowToolInput {
            resume_from_run_id: Some("wf_123".into()),
            ..base.clone()
        };
        assert!(resume_only.validate().is_ok());

        let noted_resume = WorkflowToolInput {
            resume_from_run_id: Some("wf_123".into()),
            resume_note: Some("moved the fixture into place".into()),
            ..base.clone()
        };
        assert!(noted_resume.validate().is_ok());

        let orphan_note = WorkflowToolInput {
            resume_note: Some("no run to deliver this to".into()),
            name: Some("deep-research".into()),
            ..base.clone()
        };
        assert!(orphan_note.validate().is_err());

        let blank_note = WorkflowToolInput {
            resume_from_run_id: Some("wf_123".into()),
            resume_note: Some("   ".into()),
            ..base.clone()
        };
        assert!(
            blank_note.validate().is_err(),
            "a blank note must be rejected, not silently downgraded to a noteless resume"
        );

        let smoke_checked_note = WorkflowToolInput {
            resume_from_run_id: Some("wf_123".into()),
            resume_note: Some("real answer".into()),
            validate_only: true,
            ..base.clone()
        };
        assert!(smoke_checked_note.validate().is_err());

        let oversized_note = WorkflowToolInput {
            resume_from_run_id: Some("wf_123".into()),
            resume_note: Some("x".repeat(WorkflowToolInput::MAX_RESUME_NOTE_BYTES + 1)),
            ..base.clone()
        };
        assert!(oversized_note.validate().is_err());

        let edited_resume = WorkflowToolInput {
            script_path: Some("edited.rhai".into()),
            resume_from_run_id: Some("wf_123".into()),
            ..base.clone()
        };
        assert!(edited_resume.validate().is_err());
        assert!(
            WorkflowToolInput {
                agent_budget: Some(10),
                resume_from_run_id: Some("wf_123".into()),
                name: None,
                ..base.clone()
            }
            .validate()
            .is_ok()
        );

        assert!(
            WorkflowToolInput {
                agent_budget: Some(0),
                name: Some("deep-research".into()),
                ..base.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            WorkflowToolInput {
                agent_budget: Some(WorkflowToolInput::MAX_AGENT_BUDGET + 1),
                name: Some("deep-research".into()),
                ..base.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            WorkflowToolInput {
                agent_budget: Some(1),
                name: Some("deep-research".into()),
                ..base.clone()
            }
            .validate()
            .is_ok()
        );
        assert!(
            WorkflowToolInput {
                agent_budget: Some(1),
                script: Some("let meta = #{};".into()),
                validate_only: true,
                ..base
            }
            .validate()
            .is_ok()
        );
    }
}
