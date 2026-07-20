//! `x_search` tool — client-executed X (Twitter) search.
//!
//! Backed by the xAI Responses API's `x_search` capability. Unlike the
//! provider-hosted `HostedTool::XSearch` (server-side, xAI sessions only),
//! this tool runs on the client with xAI credentials, so it can be handed to
//! any model — Codex and Kimi sessions get X search through it. Reads a
//! pre-constructed [`XSearchClient`] from Resources (inserted when the
//! session's x_search config is enabled).

use crate::implementations::web_search::client::WebSearchClient;
use crate::types::output::WebSearchOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

/// The xAI-backed client used by [`XSearchTool`]. A distinct resource from
/// the session's `WebSearchClient`: web search may be routed to another
/// backend (e.g. Perplexity) while X search stays on xAI.
#[derive(Clone)]
pub struct XSearchClient(pub WebSearchClient);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct XSearchInput {
    #[schemars(description = "The search query for posts on X (Twitter).")]
    pub query: String,
}

#[derive(Debug, Default)]
pub struct XSearchTool;

impl crate::types::tool_metadata::ToolMetadata for XSearchTool {
    // Same kind as web_search (read-only search); the kind→name template
    // map is first-registration-wins, and web_search registers first, so
    // `${{ tools.by_kind.web_search }}` keeps resolving to `web_search`.
    fn kind(&self) -> ToolKind {
        ToolKind::WebSearch
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::GrokBuild
    }

    fn description_template(&self) -> &str {
        "Search posts on X (Twitter) for real-time reactions, announcements, and discussion."
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl xai_tool_runtime::Tool for XSearchTool {
    type Args = XSearchInput;
    type Output = WebSearchOutput;

    fn id(&self) -> xai_tool_protocol::ToolId {
        xai_tool_protocol::ToolId::new("x_search").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::xai_tool_runtime::ListToolsContext,
    ) -> xai_tool_types::ToolDescription {
        xai_tool_types::ToolDescription::new(
            "x_search",
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

    #[tracing::instrument(name = "tool.x_search", skip_all)]
    async fn run(
        &self,
        ctx: xai_tool_runtime::ToolCallContext,
        input: XSearchInput,
    ) -> Result<WebSearchOutput, xai_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        let client;
        {
            let res = resources.lock().await;
            client = res.require::<XSearchClient>()?.clone();
        }

        let (content, citations) = client.0.x_search(&input.query).await.map_err(|e| {
            xai_tool_runtime::ToolError::execution(
                xai_tool_protocol::ToolId::new("x_search").expect("valid"),
                e.to_string(),
            )
        })?;

        Ok(WebSearchOutput {
            query: input.query.clone(),
            content,
            citations,
            allowed_domains: None,
            pre_formatted: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::resources::Resources;
    use crate::types::tool_metadata::test_ctx_with_call_id;

    #[test]
    fn tool_name_and_description() {
        let tool = XSearchTool;
        assert_eq!(xai_tool_runtime::Tool::id(&tool).as_str(), "x_search");
        assert!(
            crate::types::tool_metadata::ToolMetadata::description_template(&tool)
                .contains("X (Twitter)")
        );
    }

    #[test]
    fn tool_is_read_only_web_search_kind() {
        assert!(xai_tool_runtime::Tool::capabilities(&XSearchTool).is_read_only);
        assert_eq!(
            crate::types::tool_metadata::ToolMetadata::kind(&XSearchTool),
            ToolKind::WebSearch
        );
    }

    #[tokio::test]
    async fn errors_when_client_not_in_resources() {
        let resources = Resources::new();
        let result = xai_tool_runtime::Tool::run(
            &XSearchTool,
            test_ctx_with_call_id(resources.into_shared(), "test-call"),
            XSearchInput {
                query: "test".into(),
            },
        )
        .await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("missing required resource"),
            "Expected 'missing required resource' error, got: {err_msg}"
        );
    }
}
