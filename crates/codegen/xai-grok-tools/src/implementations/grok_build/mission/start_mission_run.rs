//! `start_mission_run` tool.
//!
//! Used by the orchestrator to begin or resume autonomous feature execution.

use crate::mission::runner::{MissionRunStepResult, MissionRunner};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const START_MISSION_RUN_TOOL_NAME: &str = "start_mission_run";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StartMissionRunInput {
    #[schemars(description = "Mission ID or directory name to run. Defaults to active mission or latest in current workspace.")]
    pub mission_id: Option<String>,

    #[schemars(description = "Worker session ID to resume if resuming a paused worker mid-session")]
    pub resume_worker_session_id: Option<String>,

    #[schemars(description = "If true, discard paused worker context and restart the in-progress feature from scratch")]
    pub restart_feature: Option<bool>,

    #[schemars(description = "Direct message or instructions to pass into the worker session")]
    pub message_to_worker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartMissionRunOutput {
    pub success: bool,
    pub status: String,
    pub active_feature_id: Option<String>,
    pub skill_name: Option<String>,
    pub worker_prompt: Option<String>,
    pub message: String,
}

impl xai_tool_runtime::ToolOutput for StartMissionRunOutput {}

#[derive(Debug, Default)]
pub struct StartMissionRunTool;

impl crate::types::tool_metadata::ToolMetadata for StartMissionRunTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Plan
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Begin or resume autonomous worker execution for a mission.
Dispatches worker subagents to implement features from features.json sequentially, enforcing verification gates and collecting structured handoffs."#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for StartMissionRunTool {
    type Args = StartMissionRunInput;
    type Output = StartMissionRunOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(START_MISSION_RUN_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            START_MISSION_RUN_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: false,
            ..Default::default()
        }
    }

    async fn run(
        &self,
        _ctx: xai_tool_runtime::ToolCallContext,
        input: StartMissionRunInput,
    ) -> Result<StartMissionRunOutput, xai_tool_runtime::ToolError> {
        let mission_dir = resolve_mission_dir(input.mission_id.as_deref())
            .ok_or_else(|| xai_tool_runtime::ToolError::invalid_arguments("No matching mission found to run"))?;

        let mut runner = MissionRunner::new(&mission_dir);
        if let Err(e) = runner.prepare_run() {
            return Err(xai_tool_runtime::ToolError::execution(
                self.id(),
                format!("Failed to prepare mission run: {e}"),
            ));
        }

        if input.restart_feature.unwrap_or(false) {
            if let Ok(Some(mut in_prog)) = runner.storage().get_in_progress_feature() {
                in_prog.current_worker_session_id = None;
                in_prog.status = crate::mission::types::FeatureStatus::Pending;
                let _ = runner.storage().update_feature(&in_prog);
            }
        }

        match runner.advance() {
            Ok(MissionRunStepResult::WorkerReady {
                feature,
                mut worker_prompt,
                skill_name,
                ..
            }) => {
                if let Some(msg) = input.message_to_worker {
                    worker_prompt = format!("## Direct Orchestrator Instructions:\n{}\n\n{}", msg, worker_prompt);
                }
                Ok(StartMissionRunOutput {
                    success: true,
                    status: "worker_ready".to_string(),
                    active_feature_id: Some(feature.id.clone()),
                    skill_name: Some(skill_name),
                    worker_prompt: Some(worker_prompt),
                    message: format!("Worker ready for feature '{}'. Proceeding with worker execution.", feature.id),
                })
            }
            Ok(MissionRunStepResult::Completed) => Ok(StartMissionRunOutput {
                success: true,
                status: "completed".to_string(),
                active_feature_id: None,
                skill_name: None,
                worker_prompt: None,
                message: "All mission features and validation gates have completed successfully!".to_string(),
            }),
            Ok(MissionRunStepResult::ScopeReviewRequired { initial_count, current_count }) => {
                Ok(StartMissionRunOutput {
                    success: false,
                    status: "scope_review_required".to_string(),
                    active_feature_id: None,
                    skill_name: None,
                    worker_prompt: None,
                    message: format!(
                        "MISSION SCOPE REVIEW REQUIRED: Mission started with {} features and now has {}. Review features.json before continuing.",
                        initial_count, current_count
                    ),
                })
            }
            Ok(MissionRunStepResult::OrchestratorTurn { reason, feature_id }) => {
                Ok(StartMissionRunOutput {
                    success: false,
                    status: "orchestrator_turn".to_string(),
                    active_feature_id: feature_id,
                    skill_name: None,
                    worker_prompt: None,
                    message: format!("Control returned to orchestrator: {}", reason),
                })
            }
            Ok(MissionRunStepResult::Paused { reason }) => Ok(StartMissionRunOutput {
                success: false,
                status: "paused".to_string(),
                active_feature_id: None,
                skill_name: None,
                worker_prompt: None,
                message: format!("Mission paused: {}", reason),
            }),
            Err(e) => Err(xai_tool_runtime::ToolError::execution(
                self.id(),
                format!("Runner error: {e}"),
            )),
        }
    }
}

pub(crate) fn resolve_mission_dir(query: Option<&str>) -> Option<PathBuf> {
    if let Some(q) = query {
        if let Some(found) = crate::mission::find_mission(q) {
            return Some(found.dir);
        }
        let p = PathBuf::from(q);
        if p.is_dir() {
            return Some(p);
        }
    }
    // Fall back to first discovered mission for cwd
    if let Ok(cwd) = std::env::current_dir() {
        let ws_missions = crate::mission::discover_missions_for_workspace(&cwd);
        if let Some(first) = ws_missions.first() {
            return Some(first.dir.clone());
        }
    }
    // Fall back to most recent mission globally
    crate::mission::discover_all_missions().first().map(|m| m.dir.clone())
}
