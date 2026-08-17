//! `x.ai/session/add_working_directory` / `x.ai/session/remove_working_directory`
//! extension handlers (`/add-dir`).
//!
//! Resolves the target session, forwards the mutation to the session actor
//! (which validates, canonicalizes, widens the permission scope, persists,
//! and discloses to the model), and replies with the resulting directory
//! list. Load-race-tolerant like `x.ai/interject`: a request racing a
//! reconnect-replayed `session/load` waits for the load instead of failing.

use agent_client_protocol as acp;

use super::{ExtResult, parse_params};
use crate::agent::MvpAgent;
use crate::session::SessionCommand;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkingDirectoryRequest {
    session_id: String,
    /// Directory to add or remove. Absolute, cwd-relative, or `~`-relative.
    path: String,
}

async fn handle_mutation(agent: &MvpAgent, args: &acp::ExtRequest, remove: bool) -> ExtResult {
    let req: WorkingDirectoryRequest = parse_params(args)?;
    let sid: acp::SessionId = req.session_id.clone().into();
    let session_handle = agent.session_handle_waiting_for_load(&sid).await;
    let Some(session) = session_handle else {
        return Err(
            acp::Error::invalid_params().data(format!("session not found: {}", req.session_id))
        );
    };
    let path = std::path::PathBuf::from(&req.path);
    let (respond_to, respond_rx) = tokio::sync::oneshot::channel();
    let command = if remove {
        SessionCommand::RemoveWorkingDirectory { path, respond_to }
    } else {
        SessionCommand::AddWorkingDirectory { path, respond_to }
    };
    if session.cmd_tx.send(command).is_err() {
        return Err(acp::Error::internal_error().data("session actor is not running".to_string()));
    }
    let outcome = match respond_rx.await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(message)) => {
            return Err(acp::Error::invalid_params().data(message));
        }
        Err(_) => {
            return Err(
                acp::Error::internal_error().data("session actor dropped the request".to_string())
            );
        }
    };
    super::to_ext_response(Ok(serde_json::json!({
        "changed": outcome.changed,
        "directories": outcome
            .directories
            .iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>(),
    })))
}

/// Handle `x.ai/session/add_working_directory`.
pub async fn handle_add(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    handle_mutation(agent, args, false).await
}

/// Handle `x.ai/session/remove_working_directory`.
pub async fn handle_remove(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    handle_mutation(agent, args, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Wire {
        session_id: &'static str,
        path: &'static str,
    }

    #[test]
    fn params_parse_camel_case() {
        let raw = serde_json::to_string(&Wire {
            session_id: "s1",
            path: "~/projects/other",
        })
        .unwrap();
        let req: WorkingDirectoryRequest = serde_json::from_str(&raw).unwrap();
        assert_eq!(req.session_id, "s1");
        assert_eq!(req.path, "~/projects/other");
    }
}
