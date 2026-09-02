//! `propose_mission` tool.
//!
//! Used by the orchestrator to propose and initialize a new mission or update
//! the high-level plan and scope.

use crate::mission::storage::MissionFileService;
use crate::mission::types::{FeaturesFile, MissionState, MissionStateFile, ProgressLogEntry};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const PROPOSE_MISSION_TOOL_NAME: &str = "propose_mission";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProposeMissionInput {
    #[schemars(description = "Concise title of the mission")]
    pub title: String,

    #[schemars(description = "Detailed markdown overview, goals, architectural principles, and milestones")]
    pub overview: String,

    #[schemars(description = "Target project working directory (defaults to current workspace)")]
    pub working_directory: Option<String>,

    #[schemars(description = "Optional existing mission directory ID if updating an existing proposal")]
    pub mission_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposeMissionOutput {
    pub success: bool,
    pub mission_id: String,
    pub mission_dir: String,
    pub message: String,
}

impl xai_tool_runtime::ToolOutput for ProposeMissionOutput {}

#[derive(Debug, Default)]
pub struct ProposeMissionTool;

impl crate::types::tool_metadata::ToolMetadata for ProposeMissionTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Plan
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Propose and initialize a new long-running autonomous mission.
Creates the mission workspace, initial state.json, mission.md, empty features.json, and default worker skills.
Call this tool after completing initial planning with the user."#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for ProposeMissionTool {
    type Args = ProposeMissionInput;
    type Output = ProposeMissionOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(PROPOSE_MISSION_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            PROPOSE_MISSION_TOOL_NAME,
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
        input: ProposeMissionInput,
    ) -> Result<ProposeMissionOutput, xai_tool_runtime::ToolError> {
        let wd = input.working_directory
            .unwrap_or_else(|| std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_else(|_| ".".to_string()));

        let id = input.mission_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let mission_dir = crate::mission::opengrok_missions_dir().join(&id);

        let service = MissionFileService::new(&mission_dir);
        if let Err(e) = service.initialize_mission_dir() {
            return Err(xai_tool_runtime::ToolError::execution(
                self.id(),
                format!("Failed to init mission dir: {e}"),
            ));
        }
        let _ = service.ensure_default_skills();

        let state = MissionStateFile {
            mission_id: format!("mis_{}", &id[..8.min(id.len())]),
            state: MissionState::AwaitingInput,
            working_directory: wd.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            initial_feature_count: None,
            last_reviewed_handoff_count: None,
            feature_retry_budget_bonus: Default::default(),
        };

        let _ = service.write_state(&state);
        let _ = service.write_working_directory(&wd);
        let _ = service.write_mission_md(&input.title, &input.overview);

        if !service.features_path().exists() {
            let _ = service.write_features(&FeaturesFile { features: Vec::new() });
        }

        let _ = service.append_progress_log(&ProgressLogEntry::MissionAccepted {
            timestamp: chrono::Utc::now().to_rfc3339(),
            title: input.title.clone(),
        });

        Ok(ProposeMissionOutput {
            success: true,
            mission_id: id,
            mission_dir: mission_dir.display().to_string(),
            message: format!("Mission \"{}\" initialized successfully at {}", input.title, mission_dir.display()),
        })
    }
}
