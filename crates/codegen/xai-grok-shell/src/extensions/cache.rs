//! `x.ai/session/cache` — session prompt-cache hit rate and prefix breaks.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use xai_grok_sampling_types::PromptCacheReport;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionCacheRequest {
    session_id: String,
}

/// Wire response for `x.ai/session/cache`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCacheResponse {
    pub report: PromptCacheReport,
    pub text: String,
}

#[tracing::instrument(skip_all, fields(method = %args.method))]
pub async fn handle(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    match args.method.as_ref() {
        "x.ai/session/cache" => handle_session_cache(agent, args).await,
        _ => Err(acp::Error::method_not_found()),
    }
}

async fn handle_session_cache(agent: &MvpAgent, args: &acp::ExtRequest) -> ExtResult {
    let req: SessionCacheRequest = parse_params(args)?;
    let session_id = acp::SessionId::new(req.session_id.as_str());

    let Some(handle) = agent.session_handle_waiting_for_load(&session_id).await else {
        return Err(acp::Error::resource_not_found(Some(format!(
            "session not found: {}",
            req.session_id
        ))));
    };

    let report = handle.prompt_cache.lock().snapshot();
    to_raw_response(&SessionCacheResponse {
        text: report.format_report(),
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::{ConversationItem, ConversationRequest, PromptCacheTracker};

    #[test]
    fn response_serializes_empty_report() {
        let report = PromptCacheTracker::default().snapshot();
        let v = serde_json::to_value(&SessionCacheResponse {
            text: report.format_report(),
            report,
        })
        .unwrap();
        assert_eq!(v["report"]["calls"], 0);
        assert!(v["text"].as_str().unwrap().contains("no model calls yet"));
    }

    #[test]
    fn fingerprint_round_trips_through_the_wire_report() {
        let mut tracker = PromptCacheTracker::default();
        let request = ConversationRequest {
            items: vec![
                ConversationItem::system("sys"),
                ConversationItem::user("hi"),
            ],
            model: Some("grok-4".into()),
            ..ConversationRequest::default()
        };
        tracker.record_request(&request);
        let report = tracker.snapshot();
        assert_eq!(report.last_prefix.as_deref(), Some("cold start"));
    }
}
