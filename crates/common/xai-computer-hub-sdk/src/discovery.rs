use crate::pool::HubConnectionPool;
use crate::{AuthProvider, ClientError, ToolHarnessBuilder};
use std::sync::Arc;
use std::time::Duration;
use xai_tool_protocol::{ServerInfo, SessionId};
#[derive(Debug, thiserror::Error)]
pub enum HubUrlError {
    #[error("computer hub is not configured")]
    NotConfigured,
    #[error("invalid COMPUTER_HUB_URL: {0}")]
    Invalid(#[from] url::ParseError),
    #[error("COMPUTER_HUB_URL must be ws:// or wss://")]
    UnsupportedScheme,
}
pub fn resolve_hub_url() -> Result<url::Url, HubUrlError> {
    let raw = std::env::var("COMPUTER_HUB_URL")
        .ok()
        .filter(|url_text| !url_text.is_empty())
        .ok_or(HubUrlError::NotConfigured)?;
    let url = url::Url::parse(&raw)?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(HubUrlError::UnsupportedScheme);
    }
    Ok(url)
}
#[derive(Debug, thiserror::Error)]
pub enum ListServersError {
    #[error("invalid session id: {0}")]
    SessionId(#[from] xai_tool_protocol::IdError),
    #[error("failed to connect to computer hub: {0}")]
    Connect(ClientError),
    #[error("servers.list failed: {0}")]
    List(ClientError),
    #[error("servers.list timed out")]
    Timeout,
}
pub async fn list_servers(
    url: url::Url,
    auth: Arc<dyn AuthProvider>,
    session_id_prefix: &str,
    timeout: Option<Duration>,
) -> Result<Vec<ServerInfo>, ListServersError> {
    let session_id = SessionId::new(format!("{session_id_prefix}-{}", uuid::Uuid::new_v4()))?;
    let allow_insecure_ws = url.scheme() == "ws";
    let fut = async {
        let harness = ToolHarnessBuilder::default()
            .pool(HubConnectionPool::shared().await)
            .url(url)
            .auth_provider(auth)
            .session(session_id)
            .allow_insecure_ws(allow_insecure_ws)
            .build()
            .await
            .map_err(ListServersError::Connect)?;
        harness.list_servers().await.map_err(ListServersError::List)
    };
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| ListServersError::Timeout)?,
        None => fut.await,
    }
}
