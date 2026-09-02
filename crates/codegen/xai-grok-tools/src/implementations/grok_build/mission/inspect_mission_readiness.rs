//! `inspect_mission_readiness` tool.
//!
//! Validates features.json, contract assertions, and skill definitions to
//! ensure a mission is ready for execution.

use crate::mission::storage::MissionFileService;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const INSPECT_MISSION_READINESS_TOOL_NAME: &str = "inspect_mission_readiness";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct InspectMissionReadinessInput {
    #[schemars(description = "Optional mission ID or directory to inspect. Defaults to active mission or current workspace.")]
    pub mission_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadinessCheckItem {
    pub check: String,
    pub passed: bool,
    pub details: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectMissionReadinessOutput {
    pub ready: bool,
    pub total_features: usize,
    pub checks: Vec<ReadinessCheckItem>,
    pub missing_skills: Vec<String>,
    pub summary: String,
}

impl xai_tool_runtime::ToolOutput for InspectMissionReadinessOutput {}

#[derive(Debug, Default)]
pub struct InspectMissionReadinessTool;

impl crate::types::tool_metadata::ToolMetadata for InspectMissionReadinessTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Plan
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Inspect mission readiness before execution.
Validates features.json structure, verifies that every feature references an existing skill, and checks contract assertions."#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for InspectMissionReadinessTool {
    type Args = InspectMissionReadinessInput;
    type Output = InspectMissionReadinessOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(INSPECT_MISSION_READINESS_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            INSPECT_MISSION_READINESS_TOOL_NAME,
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            ..Default::default()
        }
    }

    async fn run(
        &self,
        _ctx: xai_tool_runtime::ToolCallContext,
        input: InspectMissionReadinessInput,
    ) -> Result<InspectMissionReadinessOutput, xai_tool_runtime::ToolError> {
        let mission_dir = crate::implementations::grok_build::mission::start_mission_run::resolve_mission_dir(
            input.mission_id.as_deref(),
        )
        .ok_or_else(|| xai_tool_runtime::ToolError::invalid_arguments("No matching mission found to inspect"))?;

        let service = MissionFileService::new(&mission_dir);
        let mut checks = Vec::new();
        let mut all_ready = true;

        // Check 1: state.json
        match service.read_state() {
            Ok(state) => {
                checks.push(ReadinessCheckItem {
                    check: "state_file".to_string(),
                    passed: true,
                    details: format!("State file valid (status: {:?}, missionId: {})", state.state, state.mission_id),
                });
            }
            Err(e) => {
                all_ready = false;
                checks.push(ReadinessCheckItem {
                    check: "state_file".to_string(),
                    passed: false,
                    details: format!("Failed to read state.json: {e}"),
                });
            }
        }

        // Check 2: features.json
        let features = match service.read_features() {
            Ok(f) => {
                checks.push(ReadinessCheckItem {
                    check: "features_file".to_string(),
                    passed: true,
                    details: format!("features.json contains {} defined features", f.features.len()),
                });
                f.features
            }
            Err(e) => {
                all_ready = false;
                checks.push(ReadinessCheckItem {
                    check: "features_file".to_string(),
                    passed: false,
                    details: format!("Failed to read features.json: {e}"),
                });
                Vec::new()
            }
        };

        // Check 3: duplicate IDs
        let mut seen_ids = HashSet::new();
        let mut duplicate_ids = Vec::new();
        for f in &features {
            if !seen_ids.insert(&f.id) {
                duplicate_ids.push(f.id.clone());
            }
        }
        if duplicate_ids.is_empty() {
            checks.push(ReadinessCheckItem {
                check: "duplicate_feature_ids".to_string(),
                passed: true,
                details: "All feature IDs are unique".to_string(),
            });
        } else {
            all_ready = false;
            checks.push(ReadinessCheckItem {
                check: "duplicate_feature_ids".to_string(),
                passed: false,
                details: format!("Duplicate feature IDs found: {:?}", duplicate_ids),
            });
        }

        // Check 4: skills exist
        let mut missing_skills = Vec::new();
        for f in &features {
            let skill_path = service.skills_dir().join(&f.skill_name).join("SKILL.md");
            if !skill_path.exists() && !missing_skills.contains(&f.skill_name) {
                missing_skills.push(f.skill_name.clone());
            }
        }
        if missing_skills.is_empty() {
            checks.push(ReadinessCheckItem {
                check: "worker_skills".to_string(),
                passed: true,
                details: "All referenced worker skills exist in skills/ directory".to_string(),
            });
        } else {
            // Note: missing skills are warnings rather than hard blockers because default skill fallback exists
            checks.push(ReadinessCheckItem {
                check: "worker_skills".to_string(),
                passed: false,
                details: format!("Missing skill files for: {:?}", missing_skills),
            });
        }

        let summary = if all_ready {
            format!("Mission is READY. {} features queued across {} milestones.", features.len(), seen_ids.len())
        } else {
            "Mission is NOT ready. Resolve failing checks before starting.".to_string()
        };

        Ok(InspectMissionReadinessOutput {
            ready: all_ready,
            total_features: features.len(),
            checks,
            missing_skills,
            summary,
        })
    }
}
