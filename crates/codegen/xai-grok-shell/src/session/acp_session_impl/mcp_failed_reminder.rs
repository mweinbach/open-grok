//! The half of the "MCP servers that failed to connect" reminder that needs no `SessionActor`: classifying failures and rendering the section.
//! Episode dedupe lives in [`crate::session::announcement_state::McpAnnounced`]; the reminder is injected from `acp_session_impl/mcp.rs`.

use crate::session::announcement_state::{AnnouncedFailure, FailedServer};
use crate::session::managed_mcp::mcp_server_name;
use crate::util::text_sanitize::{flatten_spoofable, flatten_to_spaces};
use agent_client_protocol as acp;
use std::hash::{Hash, Hasher};

/// Hash of the config parts a user edits to fix a broken server (transport, url/command/args, header and env names, never header or env values).
/// An in-place edit therefore starts a new failure episode.
/// Only ever compared for equality; hashing keeps fields that can carry credentials (URLs, arg values) out of the value itself.
/// Never persisted, so restored episodes adopt the current identity instead.
fn config_identity(cfg: &acp::McpServer) -> u64 {
    fn hash_all(parts: impl Hash) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        parts.hash(&mut hasher);
        hasher.finish()
    }
    fn header_names(headers: &[acp::HttpHeader]) -> Vec<&str> {
        headers
            .iter()
            .map(|value_h| value_h.name.as_str())
            .collect()
    }
    match cfg {
        acp::McpServer::Http(value_c) => {
            hash_all(("http", &value_c.url, header_names(&value_c.headers)))
        }
        acp::McpServer::Sse(value_c) => {
            hash_all(("sse", &value_c.url, header_names(&value_c.headers)))
        }
        acp::McpServer::Stdio(value_c) => {
            let env_names: Vec<&str> = value_c
                .env
                .iter()
                .map(|value_e| value_e.name.as_str())
                .collect();
            hash_all(("stdio", &value_c.command, &value_c.args, env_names))
        }

        other => hash_all(("unknown", mcp_server_name(other))),
    }
}

/// Classify every configured but unconnected server for the "failed to connect" reminder.
/// Returns the failures to feed [`crate::session::announcement_state::McpAnnounced::note_failures`], sorted by name.
/// Also returns the full set of unconnected configured names, which keeps existing episodes alive.
///
/// Skipped from the failure list (but kept in the unconnected set):
/// - servers whose retry handshake is still running, and
/// - servers with no recorded failure while init is still settling.
///
/// The skip while settling exists because the episode's one announcement should carry the real cause, not the "connection failed" placeholder.
/// Once init is complete, the placeholder is a legitimate fallback for an unrecorded crash.
pub(super) fn classify_failed_servers(
    mcp_state: &crate::session::mcp_servers::McpState,
    connected_names: &std::collections::HashSet<&str>,
) -> (Vec<FailedServer>, std::collections::HashSet<String>) {
    let mut failed: Vec<FailedServer> = Vec::new();
    let mut unconnected: std::collections::HashSet<String> = std::collections::HashSet::new();
    for cfg in &mcp_state.configs {
        let name = mcp_server_name(cfg);
        if connected_names.contains(name) {
            continue;
        }
        unconnected.insert(name.to_string());
        if mcp_state.is_server_handshaking(name) {
            continue;
        }
        let is_auth = mcp_state.auth_required.contains(name);
        let has_record = is_auth || mcp_state.init_failed.contains_key(name);
        if !has_record && !mcp_state.is_initialized() {
            continue;
        }
        failed.push(FailedServer {
            name: name.to_string(),
            detail: mcp_state
                .init_failed
                .get(name)
                .filter(|value_d| !value_d.is_empty())
                .cloned(),
            class: if is_auth {
                AnnouncedFailure::AuthRequired
            } else {
                AnnouncedFailure::Transport
            },
            retries_on_use: !is_auth
                && matches!(cfg, acp::McpServer::Http(_) | acp::McpServer::Sse(_)),
            config_identity: config_identity(cfg),
        });
    }
    failed.sort_by(|value_a, value_b| value_a.name.cmp(&value_b.name));
    (failed, unconnected)
}

/// Render the "failed to connect" reminder section from the episodes being announced now.
/// This is the single sanitization boundary: remote-influenced detail and names are flattened here.
/// Flattening stops them forging extra reminder lines or smuggling invisible/bidi characters.
/// A detail with nothing legible left falls back to the generic reason.
pub(super) fn render_failed_section(to_announce: &[FailedServer]) -> String {
    let mut value_s = "\nMCP servers that failed to connect:\n".to_string();
    for value_f in to_announce {
        let name = flatten_to_spaces(&value_f.name);
        let base = match value_f.class {
            AnnouncedFailure::AuthRequired => "auth required".to_string(),
            AnnouncedFailure::Transport => {
                match value_f.detail.as_deref().and_then(flatten_spoofable) {
                    Some(detail) => format!("\"{}\"", detail.replace('"', "'")),
                    None => "connection failed".to_string(),
                }
            }
        };

        let suffix = if value_f.retries_on_use {
            " — retries automatically on next tool call"
        } else {
            ""
        };
        value_s.push_str(&format!("- {name} ({base}{suffix})\n"));
    }
    value_s
}
