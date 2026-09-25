//! MCP integration for the workspace server.
//!
//! Bridges [`McpClient`] to the server's [`McpTransport`] trait and wraps
//! tool handlers with qualified `server__tool` names.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use xai_computer_hub_mcp_adapter::{
    McpBridgeConfig, McpCallResult, McpContent, McpServerInfo, McpToolDefinition, McpToolHandler,
    McpTransport,
};
use xai_computer_hub_sdk::ToolServerHandler;
use xai_grok_mcp::rmcp;
use xai_grok_mcp::servers::{McpClient, parse_mcp_qualified_name};
use xai_tool_protocol::ToolId;
use xai_tool_runtime::{ToolCallContext, ToolStream, TypedToolOutput};
use xai_tool_types::ToolDescription;

/// Adapts [`McpClient`] to the [`McpTransport`] trait for [`McpBridge`].
pub(crate) struct McpClientTransportAdapter {
    /// `None` once closed: the hub's handlers keep the adapter alive after a stop, and must not keep the client.
    client: arc_swap::ArcSwapOption<McpClient>,
}

impl McpClientTransportAdapter {
    pub fn new(client: Arc<McpClient>) -> Self {
        Self {
            client: arc_swap::ArcSwapOption::new(Some(client)),
        }
    }

    fn client(&self) -> Result<Arc<McpClient>, xai_computer_hub_mcp_adapter::McpError> {
        self.client.load_full().ok_or_else(|| {
            xai_computer_hub_mcp_adapter::McpError::Transport("MCP transport closed".into())
        })
    }
}

#[async_trait]
impl McpTransport for McpClientTransportAdapter {
    async fn initialize(&self) -> Result<McpServerInfo, xai_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client()?
            .ensure_initialized()
            .await
            .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;
        let info = service.peer_info().ok_or_else(|| {
            xai_computer_hub_mcp_adapter::McpError::Transport("no peer info after init".into())
        })?;
        // rmcp 3.x makes the server implementation identity optional on peer info.
        let server_info = info.server_info.as_ref();
        Ok(McpServerInfo {
            name: server_info.map(|si| si.name.clone()).unwrap_or_default(),
            version: server_info.map(|si| si.version.clone()).unwrap_or_default(),
            capabilities: serde_json::to_value(&info.capabilities).unwrap_or_default(),
        })
    }

    async fn list_tools(
        &self,
    ) -> Result<Vec<McpToolDefinition>, xai_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client()?
            .ensure_initialized()
            .await
            .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

        let mut all_tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let result = service
                .list_tools(Some(
                    rmcp::model::PaginatedRequestParams::default().with_cursor(cursor.clone()),
                ))
                .await
                .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

            all_tools.extend(result.tools.into_iter().map(|t| McpToolDefinition {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()),
                input_schema: serde_json::to_value(&t.input_schema).ok(),
            }));

            match result.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        Ok(all_tools)
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<McpCallResult, xai_computer_hub_mcp_adapter::McpError> {
        let service = self
            .client()?
            .ensure_initialized()
            .await
            .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;
        // MCP spec requires arguments to be an object; coerce if needed.
        let args_object = match arguments {
            Value::Object(map) => Some(map),
            Value::Null => None,
            other => {
                let mut wrapper = serde_json::Map::new();
                wrapper.insert("value".to_string(), other);
                Some(wrapper)
            }
        };
        let result = service
            .call_tool({
                let mut params = rmcp::model::CallToolRequestParams::new(name.to_string());
                params.arguments = args_object;
                params
            })
            .await
            .map_err(|e| xai_computer_hub_mcp_adapter::McpError::Transport(e.to_string()))?;

        Ok(McpCallResult {
            content: result
                .content
                .into_iter()
                .map(|c| match c {
                    rmcp::model::ContentBlock::Text(t) => McpContent::Text { text: t.text },
                    rmcp::model::ContentBlock::Image(img) => McpContent::Image {
                        mime_type: img.mime_type,
                        data: img.data,
                    },
                    _ => McpContent::Text {
                        text: "[unsupported content type]".to_string(),
                    },
                })
                .collect(),
            is_error: result.is_error.unwrap_or(false),
        })
    }

    async fn close(&self) -> Result<(), xai_computer_hub_mcp_adapter::McpError> {
        // Letting go of the client is what ends a stdio child, once no in-flight call still holds its
        // service; a call routed in afterwards fails here.
        self.client.store(None);
        Ok(())
    }
}

/// Wraps an [`McpToolHandler`] to qualify tool names as `server__tool`.
pub(crate) struct QualifiedMcpToolHandler {
    qualified_id: ToolId,
    qualified_name: String,
    inner: Arc<McpToolHandler>,
}

impl QualifiedMcpToolHandler {
    /// Returns `None` if the qualified name is invalid or ambiguous.
    pub fn try_new(qualified_name: String, inner: Arc<McpToolHandler>) -> Option<Self> {
        let qualified_id = match parse_mcp_qualified_name(&qualified_name) {
            Some((id, _, _)) => id,
            None => {
                tracing::warn!(
                    qualified_name,
                    "skipping MCP tool: qualified name is invalid or ambiguous"
                );
                return None;
            }
        };
        Some(Self {
            qualified_id,
            qualified_name,
            inner,
        })
    }
}

#[async_trait]
impl ToolServerHandler for QualifiedMcpToolHandler {
    fn tool_id(&self) -> ToolId {
        self.qualified_id.clone()
    }

    fn description(&self) -> ToolDescription {
        let inner_desc = self.inner.description();
        ToolDescription::new(self.qualified_name.clone(), inner_desc.description)
    }

    fn input_schema(&self) -> Option<Value> {
        self.inner.input_schema()
    }

    async fn handle_call(&self, ctx: ToolCallContext, args: Value) -> ToolStream<TypedToolOutput> {
        self.inner.handle_call(ctx, args).await
    }
}

/// Result of a `workspace.configure_mcp` RPC call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpStartResult {
    /// Server names that started successfully.
    pub started: Vec<String>,
    /// Servers that failed to start.
    pub failed: Vec<McpStartFailure>,
}

/// A single MCP server startup failure.
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpStartFailure {
    /// Server name.
    pub name: String,
    /// Human-readable error description.
    pub error: String,
}

/// Extract a server name from an [`McpError`](xai_grok_mcp::servers::McpError),
/// falling back to `"unknown"`.
pub(crate) fn server_name_from_mcp_error(e: &xai_grok_mcp::servers::McpError) -> &str {
    e.server_name().unwrap_or("unknown")
}

/// Compose a host-owned built-in `entry` into `servers`: same-named entries are dropped so none
/// can take its first-party posture, and it goes first so any server cap keeps it.
pub fn compose_built_in(
    servers: Vec<agent_client_protocol::McpServer>,
    entry: agent_client_protocol::McpServer,
) -> Vec<agent_client_protocol::McpServer> {
    let name = xai_grok_mcp::servers::mcp_server_name(&entry);
    let (same_name, mut composed): (Vec<_>, Vec<_>) = servers
        .into_iter()
        .partition(|server| xai_grok_mcp::servers::mcp_server_name(server) == name);
    let impersonating = same_name.iter().filter(|server| **server != entry).count();
    if impersonating > 0 {
        tracing::warn!(
            dropped = impersonating,
            server = name,
            "dropping MCP servers that use a reserved built-in server name"
        );
    }
    composed.insert(0, entry);
    composed
}

/// Bridge config factory for MCP bridge connections.
pub(crate) fn make_bridge_config(
    session_id: xai_tool_protocol::SessionId,
    server_name: &str,
) -> McpBridgeConfig {
    McpBridgeConfig {
        session_id,
        namespace: Some(server_name.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_computer_hub_mcp_adapter::{McpBridge, McpError};
    use xai_tool_protocol::SessionId;

    struct TestTransport;

    #[async_trait]
    impl McpTransport for TestTransport {
        async fn initialize(&self) -> Result<McpServerInfo, McpError> {
            Ok(McpServerInfo {
                name: "test".to_owned(),
                version: "1".to_owned(),
                capabilities: Value::Null,
            })
        }

        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpError> {
            Ok(vec![McpToolDefinition {
                name: "tool".to_owned(),
                description: None,
                input_schema: None,
            }])
        }

        async fn call_tool(
            &self,
            _name: &str,
            _arguments: Value,
        ) -> Result<McpCallResult, McpError> {
            unreachable!("constructor test does not call the tool")
        }

        async fn close(&self) -> Result<(), McpError> {
            Ok(())
        }
    }

    #[test]
    fn compose_built_in_reserves_the_name_and_leads_the_list() {
        let composed = compose_built_in(
            vec![
                stdio("other", "/other"),
                stdio("built_in", "/impersonator"),
                stdio("built_in", "/impersonator-2"),
            ],
            stdio("built_in", "/host"),
        );
        assert_eq!(
            composed,
            vec![stdio("built_in", "/host"), stdio("other", "/other")]
        );
    }

    /// A host re-feeding its own composed list (hot reload) must not warn about itself.
    #[test]
    fn compose_built_in_warns_only_for_a_differing_same_named_entry() {
        let host = stdio("built_in", "/host");
        let ((), re_fed) = crate::capturing_warn_logs(|| {
            let composed =
                compose_built_in(vec![host.clone(), stdio("other", "/other")], host.clone());
            assert_eq!(2, composed.len());
        });
        assert!(!re_fed.contains("reserved"), "own entry re-fed: {re_fed}");
        let ((), impersonated) = crate::capturing_warn_logs(|| {
            compose_built_in(vec![stdio("built_in", "/impersonator")], host.clone());
        });
        assert!(
            impersonated.contains("reserved") && impersonated.contains("dropped=1"),
            "impersonator: {impersonated}"
        );
    }

    fn stdio(name: &str, command: &str) -> agent_client_protocol::McpServer {
        agent_client_protocol::McpServer::Stdio(agent_client_protocol::McpServerStdio::new(
            name, command,
        ))
    }

    #[tokio::test]
    async fn qualified_handler_rejects_ambiguous_name() {
        let bridge = McpBridge::connect(
            Arc::new(TestTransport),
            &make_bridge_config(SessionId::new("session").unwrap(), "test"),
        )
        .await
        .unwrap()
        .bridge;
        let inner = bridge.handlers()[0].clone();

        let valid = QualifiedMcpToolHandler::try_new("123__lookup".to_owned(), inner.clone())
            .expect("valid qualified ToolId");
        assert_eq!(valid.tool_id().as_str(), "123__lookup");
        assert!(QualifiedMcpToolHandler::try_new("foo___bar".to_owned(), inner).is_none());
    }
}
