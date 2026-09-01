//!

use crate::extensions::notification::SessionUpdate;
use xai_grok_sampler::SamplingErrorKind;

/// `stop_reason` is always a JSON string; `agent_result` is a string or null.
pub(crate) fn build_turn_completed(
    prompt_id: String,
    stop_reason: serde_json::Value,
    agent_result: serde_json::Value,
    error_kind: Option<SamplingErrorKind>,
    usage: Option<crate::extensions::notification::PromptUsage>,
    elapsed_ms: Option<u64>,
) -> SessionUpdate {
    SessionUpdate::TurnCompleted {
        prompt_id,
        stop_reason: json_to_string(stop_reason),
        agent_result: match agent_result {
            serde_json::Value::Null => None,
            other => Some(json_to_string(other)),
        },
        error_kind: error_kind.map(|kind| kind.as_str().to_string()),
        usage,
        elapsed_ms,
    }
}

pub(crate) fn prompt_complete_payload(
    session_id: &agent_client_protocol::SessionId,
    prompt_id: &str,
    result: &std::result::Result<agent_client_protocol::StopReason, agent_client_protocol::Error>,
) -> serde_json::Value {
    let (stop_reason, agent_result, error_kind) =
        crate::sampling::error::prompt_complete_fields(result);
    let mut payload = serde_json::json!({
        "sessionId": session_id.to_string(),
        "promptId": prompt_id,
        "stopReason": stop_reason,
        "agentResult": agent_result,
    });
    if let Some(kind) = error_kind {
        payload[crate::extensions::notification::PROMPT_COMPLETE_ERROR_KIND_KEY] =
            serde_json::json!(kind.as_str());
    }
    payload
}

fn json_to_string(value: serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_ok_end_turn_pair() {
        // The exact pair `prompt_complete_fields(&Ok(EndTurn))` produces.
        let update = build_turn_completed(
            "p-1".into(),
            serde_json::json!("end_turn"),
            serde_json::Value::Null,
            None,
            None,
            Some(1500),
        );
        assert_eq!(
            update,
            SessionUpdate::TurnCompleted {
                prompt_id: "p-1".into(),
                stop_reason: "end_turn".into(),
                agent_result: None,
                error_kind: None,
                usage: None,
                elapsed_ms: Some(1500),
            }
        );
    }

    #[test]
    fn maps_error_pair_with_detail() {
        // The pair `prompt_complete_fields(&Err(..))` produces for a generic error.
        let update = build_turn_completed(
            "p-2".into(),
            serde_json::json!("error"),
            serde_json::json!("connection reset"),
            None,
            None,
            Some(0),
        );
        assert_eq!(
            update,
            SessionUpdate::TurnCompleted {
                prompt_id: "p-2".into(),
                stop_reason: "error".into(),
                agent_result: Some("connection reset".into()),
                error_kind: None,
                usage: None,
                elapsed_ms: Some(0),
            }
        );
    }

    #[test]
    fn null_agent_result_maps_to_none() {
        let update = build_turn_completed(
            "p-3".into(),
            serde_json::json!("cancelled"),
            serde_json::Value::Null,
            None,
            None,
            Some(42),
        );
        assert!(matches!(
            update,
            SessionUpdate::TurnCompleted {
                agent_result: None,
                ..
            }
        ));
    }

    #[test]
    fn non_string_values_fall_back_to_json_text() {
        let update = build_turn_completed(
            "p-4".into(),
            serde_json::json!(42),
            serde_json::json!({ "k": "v" }),
            None,
            None,
            Some(7),
        );
        assert_eq!(
            update,
            SessionUpdate::TurnCompleted {
                prompt_id: "p-4".into(),
                stop_reason: "42".into(),
                agent_result: Some("{\"k\":\"v\"}".into()),
                error_kind: None,
                usage: None,
                elapsed_ms: Some(7),
            }
        );
    }

    #[test]
    fn missing_elapsed_stays_none() {
        let update = build_turn_completed(
            "p-5".into(),
            serde_json::json!("cancelled"),
            serde_json::Value::Null,
            None,
            None,
            None,
        );
        assert!(matches!(
            update,
            SessionUpdate::TurnCompleted {
                elapsed_ms: None,
                ..
            }
        ));
    }

    #[test]
    fn typed_error_kind_lands_in_the_typed_field() {
        let update = build_turn_completed(
            "p-6".into(),
            serde_json::json!("error"),
            serde_json::json!("truncated"),
            Some(SamplingErrorKind::MaxTokensTruncation),
            None,
            Some(3),
        );
        assert!(matches!(
            update,
            SessionUpdate::TurnCompleted { error_kind: Some(kind), .. } if kind == "max_tokens_truncation"
        ));
    }

    #[test]
    fn prompt_complete_payload_stamps_error_kind_for_truncation_only() {
        use agent_client_protocol as acp;

        let sid = acp::SessionId::new("s1".to_string());
        let result = Err(crate::sampling::error::map_sampling_err_to_acp(
            crate::sampling::error::SamplingError::MaxTokensTruncation,
        ));
        let payload = prompt_complete_payload(&sid, "p1", &result);
        assert_eq!(payload["sessionId"], "s1");
        assert_eq!(payload["promptId"], "p1");
        assert_eq!(payload["stopReason"], "error");
        assert_eq!(payload["errorKind"], "max_tokens_truncation");

        let ok: std::result::Result<acp::StopReason, acp::Error> = Ok(acp::StopReason::EndTurn);
        let payload = prompt_complete_payload(&sid, "p2", &ok);
        assert_eq!(payload["stopReason"], "end_turn");
        assert!(payload.get("errorKind").is_none());

        let generic = Err(acp::Error::internal_error().data("boom"));
        let payload = prompt_complete_payload(&sid, "p3", &generic);
        assert_eq!(payload["stopReason"], "error");
        assert!(payload.get("errorKind").is_none());
    }
}
