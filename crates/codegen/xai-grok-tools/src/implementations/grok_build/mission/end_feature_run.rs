//! `end_feature_run` tool.
//!
//! Used by worker subagents to conclude execution on a feature and return
//! a structured handoff with verification evidence.

use crate::mission::runner::MissionRunner;
use crate::mission::types::{WorkerHandoff, WorkerSuccessState};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const END_FEATURE_RUN_TOOL_NAME: &str = "end_feature_run";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EndFeatureRunInput {
    #[schemars(description = "ID of the feature being concluded")]
    pub feature_id: String,

    #[schemars(description = "Outcome of the feature run: 'success', 'failure', or 'partial'")]
    pub success_state: String,

    #[schemars(description = "True if orchestrator review is required (e.g. blockers, ambiguity, or incomplete work)")]
    pub return_to_orchestrator: Option<bool>,

    #[schemars(description = "Git commit SHA or stash identifier where changes were saved, if applicable")]
    pub commit_id: Option<String>,

    #[schemars(description = "Exit code of primary verification command")]
    pub exit_code: Option<i32>,

    #[schemars(description = "Structured handoff documenting changes, verification evidence, tests, and discovered issues")]
    pub handoff: WorkerHandoff,

    #[schemars(description = "Optional mission ID or directory")]
    pub mission_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndFeatureRunOutput {
    pub success: bool,
    pub next_action: String,
    pub message: String,
}

impl xai_tool_runtime::ToolOutput for EndFeatureRunOutput {}

#[derive(Debug, Default)]
pub struct EndFeatureRunTool;

impl crate::types::tool_metadata::ToolMetadata for EndFeatureRunTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Plan
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Conclude work on a mission feature and produce a structured handoff.
Records implementation summary, test coverage, verification commands, and any discovered issues."#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for EndFeatureRunTool {
    type Args = EndFeatureRunInput;
    type Output = EndFeatureRunOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(END_FEATURE_RUN_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            END_FEATURE_RUN_TOOL_NAME,
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
        input: EndFeatureRunInput,
    ) -> Result<EndFeatureRunOutput, xai_tool_runtime::ToolError> {
        let mission_dir = crate::implementations::grok_build::mission::start_mission_run::resolve_mission_dir(
            input.mission_id.as_deref(),
        )
        .ok_or_else(|| xai_tool_runtime::ToolError::invalid_arguments("No active mission directory found"))?;

        let mut runner = MissionRunner::new(&mission_dir);
        let worker_session_id = uuid::Uuid::new_v4().to_string();

        let state_enum = match input.success_state.to_lowercase().as_str() {
            "success" => WorkerSuccessState::Success,
            "partial" => WorkerSuccessState::Partial,
            _ => WorkerSuccessState::Failure,
        };

        let result = runner.handle_worker_completion(
            &worker_session_id,
            &input.feature_id,
            state_enum,
            input.return_to_orchestrator.unwrap_or(false),
            input.commit_id,
            input.exit_code.unwrap_or(0),
            input.handoff,
        ).map_err(|e| xai_tool_runtime::ToolError::execution(self.id(), format!("Failed to record worker completion: {e}")))?;

        let (next_action, msg) = match result {
            crate::mission::MissionRunStepResult::WorkerReady { feature, .. } => (
                "next_worker_ready".to_string(),
                format!("Feature '{}' concluded. Next feature '{}' is queued.", input.feature_id, feature.id),
            ),
            crate::mission::MissionRunStepResult::Completed => (
                "mission_completed".to_string(),
                "Feature concluded and mission is 100% completed!".to_string(),
            ),
            crate::mission::MissionRunStepResult::OrchestratorTurn { reason, .. } => (
                "orchestrator_turn".to_string(),
                format!("Feature concluded. Returned to orchestrator: {}", reason),
            ),
            crate::mission::MissionRunStepResult::ScopeReviewRequired { .. } => (
                "scope_review_required".to_string(),
                "Feature concluded. Scope expansion review required.".to_string(),
            ),
            crate::mission::MissionRunStepResult::Paused { reason } => (
                "paused".to_string(),
                format!("Feature concluded. Mission paused: {}", reason),
            ),
        };

        Ok(EndFeatureRunOutput {
            success: true,
            next_action,
            message: msg,
        })
    }
}
