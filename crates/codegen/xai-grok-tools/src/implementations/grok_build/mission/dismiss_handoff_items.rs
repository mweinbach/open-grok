//! `dismiss_handoff_items` tool.
//!
//! Used by the orchestrator to record explicit, justified dismissals of
//! discovered issues or incomplete work reported in worker handoffs.

use crate::mission::storage::MissionFileService;
use crate::mission::types::{HandoffDismissal, ProgressLogEntry};
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const DISMISS_HANDOFF_ITEMS_TOOL_NAME: &str = "dismiss_handoff_items";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DismissItemInput {
    #[schemars(description = "Type of item: 'discovered_issue' or 'incomplete_work'")]
    pub r#type: String,

    #[schemars(description = "Feature ID that originated the item")]
    pub source_feature_id: String,

    #[schemars(description = "Short summary of the item being dismissed")]
    pub summary: String,

    #[schemars(description = "Technical justification explaining why this item is safe to dismiss or out of scope")]
    pub justification: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DismissHandoffItemsInput {
    #[schemars(description = "Optional mission ID or directory")]
    pub mission_id: Option<String>,

    #[schemars(description = "List of items being explicitly dismissed with justification")]
    pub dismissals: Vec<DismissItemInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DismissHandoffItemsOutput {
    pub success: bool,
    pub dismissed_count: usize,
    pub message: String,
}

impl xai_tool_runtime::ToolOutput for DismissHandoffItemsOutput {}

#[derive(Debug, Default)]
pub struct DismissHandoffItemsTool;

impl crate::types::tool_metadata::ToolMetadata for DismissHandoffItemsTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Plan
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        r#"Dismiss worker handoff items (discovered issues or incomplete work) with explicit justification.
Every item dismissed is durably appended to progress_log.jsonl to maintain verification auditability."#
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for DismissHandoffItemsTool {
    type Args = DismissHandoffItemsInput;
    type Output = DismissHandoffItemsOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new(DISMISS_HANDOFF_ITEMS_TOOL_NAME).expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            DISMISS_HANDOFF_ITEMS_TOOL_NAME,
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
        input: DismissHandoffItemsInput,
    ) -> Result<DismissHandoffItemsOutput, xai_tool_runtime::ToolError> {
        let mission_dir = crate::implementations::grok_build::mission::start_mission_run::resolve_mission_dir(
            input.mission_id.as_deref(),
        )
        .ok_or_else(|| xai_tool_runtime::ToolError::invalid_arguments("No matching mission found"))?;

        let service = MissionFileService::new(&mission_dir);
        let dismissals: Vec<HandoffDismissal> = input
            .dismissals
            .into_iter()
            .map(|d| HandoffDismissal {
                r#type: d.r#type,
                source_feature_id: d.source_feature_id,
                summary: d.summary,
                justification: d.justification,
            })
            .collect();

        let count = dismissals.len();
        let entry = ProgressLogEntry::HandoffItemsDismissed {
            timestamp: chrono::Utc::now().to_rfc3339(),
            dismissals,
        };

        service
            .append_progress_log(&entry)
            .map_err(|e| xai_tool_runtime::ToolError::execution(self.id(), format!("Failed to record dismissals: {e}")))?;

        Ok(DismissHandoffItemsOutput {
            success: true,
            dismissed_count: count,
            message: format!("Successfully recorded {} handoff item dismissal(s)", count),
        })
    }
}
