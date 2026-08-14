//! `x.ai/session/cache` — prompt cache telemetry, hit rate, and break diagnostics.

use agent_client_protocol as acp;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::{ExtResult, parse_params, to_raw_response};
use crate::agent::MvpAgent;
use crate::session::commands::SessionCommand;
use crate::session::{CacheSummary, CacheTurnRecord};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionCacheRequest {
    session_id: String,
}

/// Wire response for `x.ai/session/cache`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCacheResponse {
    pub summary: CacheSummary,
    pub recent_turns: Vec<CacheTurnRecord>,
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

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(SessionCommand::GetCacheInfo { responds_to: tx })
        .map_err(|_| acp::Error::internal_error().data("session actor channel closed"))?;

    let (summary, recent_turns) = rx
        .await
        .map_err(|_| acp::Error::internal_error().data("session actor dropped cache query reply"))?;

    to_raw_response(&SessionCacheResponse {
        summary,
        recent_turns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{CacheStatus, PrefixDivergence};

    #[test]
    fn test_session_cache_response_serialization() {
        let record = CacheTurnRecord {
            turn_idx: "1".into(),
            loop_index: 0,
            prompt_tokens: 1500,
            cached_prompt_tokens: 1200,
            completion_tokens: 100,
            cache_hit_rate_pct: 80.0,
            status: CacheStatus::Hit,
            divergence: PrefixDivergence::PrefixIntact {
                preserved_items: 2,
                new_items: 1,
            },
            diagnostic: "Cache hit: 80.0%".into(),
            timestamp_rfc3339: "2026-08-14T00:00:00Z".into(),
        };

        let summary = CacheSummary {
            total_input_tokens: 1500,
            total_cached_tokens: 1200,
            overall_hit_rate_pct: 80.0,
            total_turns: 1,
            hits: 1,
            partial_hits: 0,
            breaks: 0,
            last_break_diagnostic: None,
        };

        let resp = SessionCacheResponse {
            summary,
            recent_turns: vec![record],
        };

        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["summary"]["overallHitRatePct"], 80.0);
        assert_eq!(json["summary"]["totalCachedTokens"], 1200);
        assert_eq!(json["recentTurns"][0]["cacheHitRatePct"], 80.0);
        assert_eq!(json["recentTurns"][0]["status"], "hit");
    }
}
