//! MCP server configuration value types, extracted from xai-grok-shell
//! (config dependency inversion).

use agent_client_protocol as acp;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use xai_grok_mcp::oauth_config::McpOAuthConfig;

/// serde default helper. Kept module-local rather than shared — the `pool`
/// module keeps its own copy for `PoolConfig`.
fn default_true() -> bool {
    true
}

/// Read an MCP OAuth client secret from the named env var. Moved here with
/// `McpServerConfig` (its only caller).
fn resolve_oauth_client_secret(env_var: Option<&String>) -> Option<String> {
    let env_var = env_var?;
    match std::env::var(env_var) {
        Ok(secret) => Some(secret),
        Err(_) => {
            tracing::warn!(
                env_var = env_var.as_str(),
                "MCP OAuth client_secret env var is configured but not set in the environment; \
                 proceeding without a client secret"
            );
            None
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum McpServerTransportConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<HashMap<String, String>>,
        /// Standard MCP JSON supports `cwd`, but ACP stdio server config does not yet expose it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    StreamableHttp {
        url: String,
        #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
        transport_type: Option<String>,
        /// Name of the environment variable to read and set for `Authorization: Bearer <token>`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bearer_token_env_var: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        /// OAuth client ID for providers that don't support Dynamic Client Registration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth_client_id: Option<String>,
        /// Name of the env var holding the OAuth client secret (for BYO credentials).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth_client_secret_env_var: Option<String>,
        /// OAuth scopes to request during authorization.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth_scopes: Option<Vec<String>>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpJsonOAuthBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_env_var: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(flatten)]
    pub transport: McpServerTransportConfig,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpJsonOAuthBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeout_sec: Option<u64>,
    /// Per-tool timeout overrides in seconds: `{ "create_issue" = 120, "search" = 30 }`.
    /// Falls back to `tool_timeout_sec` for tools not listed here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_timeouts: Option<HashMap<String, u64>>,
    /// Also keep the raw base64 in tool-result text so agents can forward
    /// bytes via path-based tools (`base64 -d > /tmp/x.png && send_file ...`).
    /// ~2× tokens per image. Overridden by `_meta.mcpConfig.<server>.exposeImageBase64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose_image_base64: Option<bool>,
}
impl McpServerConfig {
    pub fn expand_strings(&mut self, sub: &dyn Fn(&str) -> String) {
        match &mut self.transport {
            McpServerTransportConfig::Stdio {
                command,
                args,
                env,
                cwd,
            } => {
                *command = sub(command);
                for arg in args.iter_mut() {
                    *arg = sub(arg);
                }
                if let Some(env) = env.as_mut() {
                    for value in env.values_mut() {
                        *value = sub(value);
                    }
                }
                if let Some(cwd) = cwd.as_mut() {
                    *cwd = sub(cwd);
                }
            }
            McpServerTransportConfig::StreamableHttp { url, headers, .. } => {
                *url = sub(url);
                if let Some(headers) = headers.as_mut() {
                    for value in headers.values_mut() {
                        *value = sub(value);
                    }
                }
            }
        }
    }

    pub fn to_acp_mcp_server(&self, name: impl Into<String>) -> Option<acp::McpServer> {
        if !self.enabled {
            return None;
        }
        let name = name.into();
        match &self.transport {
            McpServerTransportConfig::Stdio {
                command,
                args,
                env,
                cwd: _,
            } => {
                let env_variables: Vec<acp::EnvVariable> = env
                    .as_ref()
                    .map(|e| {
                        e.iter()
                            .map(|(k, v)| acp::EnvVariable::new(k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default();

                Some(acp::McpServer::Stdio(
                    acp::McpServerStdio::new(name, PathBuf::from(command))
                        .args(args.clone())
                        .env(env_variables),
                ))
            }
            McpServerTransportConfig::StreamableHttp {
                url,
                transport_type,
                bearer_token_env_var,
                headers,
                ..
            } => {
                let mut http_headers: Vec<acp::HttpHeader> = headers
                    .as_ref()
                    .map(|h| {
                        h.iter()
                            .map(|(k, v)| acp::HttpHeader::new(k.clone(), v.clone()))
                            .collect()
                    })
                    .unwrap_or_default();

                // Add bearer token from environment variable if specified
                if let Some(env_var) = bearer_token_env_var {
                    match std::env::var(env_var) {
                        Ok(token) => {
                            http_headers.push(acp::HttpHeader::new(
                                "Authorization",
                                format!("Bearer {}", token),
                            ));
                        }
                        Err(_) => {
                            tracing::warn!(
                                "MCP server '{}': bearer_token_env_var '{}' not set in environment",
                                name,
                                env_var
                            );
                        }
                    }
                }

                let is_sse = transport_type
                    .as_deref()
                    .is_some_and(|transport| transport.eq_ignore_ascii_case("sse"))
                    || url.ends_with("/sse");

                Some(if is_sse {
                    acp::McpServer::Sse(
                        acp::McpServerSse::new(name, url.clone()).headers(http_headers),
                    )
                } else {
                    acp::McpServer::Http(
                        acp::McpServerHttp::new(name, url.clone()).headers(http_headers),
                    )
                })
            }
        }
    }

    /// Extract OAuth configuration for this server, if any OAuth fields are set.
    pub fn oauth_config(&self) -> Option<McpOAuthConfig> {
        if let McpServerTransportConfig::StreamableHttp {
            oauth_client_id,
            oauth_client_secret_env_var,
            oauth_scopes,
            ..
        } = &self.transport
            && oauth_client_id.is_some()
        {
            return Some(McpOAuthConfig {
                client_id: oauth_client_id.clone(),
                client_secret: resolve_oauth_client_secret(oauth_client_secret_env_var.as_ref()),
                scopes: oauth_scopes.clone(),
                callback_port: None,
            });
        }

        if let Some(block) = &self.oauth
            && block.client_id.is_some()
        {
            return Some(McpOAuthConfig {
                client_id: block.client_id.clone(),
                client_secret: resolve_oauth_client_secret(block.client_secret_env_var.as_ref()),
                scopes: block.scopes.clone(),
                callback_port: block.callback_port,
            });
        }

        None
    }
}

/// Configuration for relay session sharing.
/// Set in config.toml under [relay] section.
///
/// Example:
/// ```toml
/// [relay]
/// enabled = true
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RelaySyncConfig {
    pub enabled: Option<bool>,
}

impl RelaySyncConfig {
    /// Check if relay sync is enabled. Env var takes precedence over config.
    pub fn is_enabled(&self) -> bool {
        if let Ok(env_val) = std::env::var("GROK_RELAY_SYNC_ENABLED") {
            return env_val.eq_ignore_ascii_case("true") || env_val == "1";
        }
        self.enabled.unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: IndexMap<String, McpServerConfig>,
}
