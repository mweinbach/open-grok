//! Rejoin a detached `agent_swarm` cohort.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{registry::SwarmRegistry, render_detached_xml, render_xml};
use crate::{
    implementations::grok_build::task::types::{
        ForegroundWaitKind, OrchestrationSteerSignal, SessionIdResource, SubagentForegroundWait,
    },
    types::{
        output::ToolOutput,
        requirements::{Expr, ToolRequirement},
        tool::{ToolKind, ToolNamespace},
        tool_metadata::shared_resources,
    },
};

const DEFAULT_WAIT_MS: u64 = 600_000;
const MAX_WAIT_MS: u64 = 2 * 60 * 60 * 1000;

#[derive(Debug, Default)]
pub struct SwarmWaitTool;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SwarmWaitToolInput {
    /// Swarm id returned in a prior detached `<agent_swarm_result>`. Omit when
    /// only one detached swarm is active in this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Swarm id from a detached agent_swarm_result. Omit when only one detached swarm is active."
    )]
    pub swarm_id: Option<String>,

    /// Maximum wait in milliseconds. Omit for 10 minutes; pass 0 to poll
    /// without blocking.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(
        description = "Maximum wait in milliseconds. Omit for 600000 (10 min); pass 0 for a non-blocking poll."
    )]
    pub timeout_ms: Option<u64>,
}

impl crate::types::tool_metadata::ToolMetadata for SwarmWaitTool {
    fn kind(&self) -> ToolKind {
        ToolKind::AgentSwarm
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        concat!(
            "Wait for a previously detached agent_swarm cohort to finish and return its full ",
            "agent_swarm_result XML. Pass swarm_id from the detached result when more than one ",
            "swarm is outstanding; omit it when only one detached swarm is active. timeout_ms ",
            "defaults to 600000 (10 minutes); pass 0 to poll the current partial state without ",
            "blocking. A user message arriving while this wait is held detaches again so you can ",
            "keep working; call swarm_wait later to rejoin."
        )
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::Value(ToolRequirement::tool_kind(ToolKind::AgentSwarm))
    }

    fn is_read_only(&self) -> bool {
        true
    }
}

impl xai_tool_runtime::Tool for SwarmWaitTool {
    type Args = SwarmWaitToolInput;
    type Output = ToolOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("swarm_wait").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "swarm_wait",
            crate::types::tool_metadata::ToolMetadata::description_template(self),
        )
    }

    fn capabilities(&self) -> xai_tool_protocol::ToolCapabilities {
        xai_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(xai_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: SwarmWaitToolInput,
    ) -> Result<ToolOutput, xai_tool_runtime::ToolError> {
        let tool_cancellation = ctx
            .get::<xai_tool_runtime::Cancellation>()
            .map(|cancellation| cancellation.0.clone());
        let timeout_ms = input.timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(MAX_WAIT_MS);
        let resources = shared_resources(&ctx)?;
        let (registry, parent_session_id, foreground_wait, steer) = {
            let res = resources.lock().await;
            (
                res.get::<SwarmRegistry>().cloned().ok_or_else(|| {
                    xai_tool_runtime::ToolError::custom(
                        "missing_resource",
                        "SwarmRegistry is not initialized for this session",
                    )
                })?,
                res.get::<SessionIdResource>()
                    .map(|s| s.0.clone())
                    .unwrap_or_default(),
                res.get::<SubagentForegroundWait>().cloned(),
                res.get::<OrchestrationSteerSignal>().cloned(),
            )
        };
        let swarm = registry
            .resolve(input.swarm_id.as_deref(), &parent_session_id)
            .map_err(xai_tool_runtime::ToolError::invalid_arguments)?;

        if timeout_ms == 0 {
            return Ok(ToolOutput::Text(snapshot_xml(&swarm).into()));
        }

        let _foreground_wait =
            foreground_wait.map(|wait| wait.enter_kind(ForegroundWaitKind::Orchestration));
        let steer_seen = steer
            .as_ref()
            .map(|signal| signal.generation())
            .unwrap_or(0);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);

        loop {
            if swarm.is_finished() {
                let xml = take_finished_xml(&registry, &swarm)?;
                return Ok(ToolOutput::Text(xml.into()));
            }

            let steer_wait = async {
                if let Some(signal) = steer.as_ref() {
                    signal.wait_after(steer_seen).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let cancel_wait = async {
                if let Some(token) = tool_cancellation.as_ref() {
                    token.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };

            tokio::select! {
                biased;
                _ = cancel_wait => {
                    swarm.cancellation.cancel();
                    return Err(xai_tool_runtime::ToolError::custom(
                        "cancelled",
                        "swarm_wait was cancelled",
                    ));
                }
                _ = steer_wait => {
                    return Ok(ToolOutput::Text(snapshot_xml(&swarm).into()));
                }
                _ = swarm.wait_finished() => {
                    let xml = take_finished_xml(&registry, &swarm)?;
                    return Ok(ToolOutput::Text(xml.into()));
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Ok(ToolOutput::Text(snapshot_xml(&swarm).into()));
                }
            }
        }
    }
}

fn snapshot_xml(swarm: &super::registry::DetachedSwarm) -> String {
    let slots = swarm.slots.snapshot();
    render_detached_xml(
        &swarm.swarm_id,
        &swarm.description,
        swarm.expected_members,
        &slots,
    )
}

fn take_finished_xml(
    registry: &SwarmRegistry,
    swarm: &super::registry::DetachedSwarm,
) -> Result<String, xai_tool_runtime::ToolError> {
    let results = swarm.slots.take_complete().ok_or_else(|| {
        xai_tool_runtime::ToolError::custom(
            "incomplete_swarm",
            "Detached swarm finished without a complete result set",
        )
    })?;
    registry.remove(&swarm.swarm_id);
    Ok(render_xml(&results))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_id_and_description_pin_rejoin_contract() {
        assert_eq!(
            xai_tool_runtime::Tool::id(&SwarmWaitTool).as_str(),
            "swarm_wait"
        );
        let description =
            crate::types::tool_metadata::ToolMetadata::description_template(&SwarmWaitTool);
        assert!(description.contains("detached"));
        assert!(description.contains("timeout_ms"));
    }
}
