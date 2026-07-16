use chrono::Utc;
use xai_grok_shell::codex_auth::{
    CodexAuthStore, CodexTokenData, CodexUsageSnapshot,
};

#[test]
fn codex_auth_store_matches_codex_rust_json_shape() {
    let store = CodexAuthStore {
        auth_mode: Some("chatgpt".to_owned()),
        openai_api_key: None,
        tokens: Some(CodexTokenData {
            id_token: "header.payload.signature".to_owned(),
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            account_id: Some("account-1".to_owned()),
        }),
        last_refresh: Some(Utc::now()),
    };
    let json = serde_json::to_value(&store).unwrap();
    assert_eq!(json["auth_mode"], "chatgpt");
    assert!(json["OPENAI_API_KEY"].is_null());
    assert_eq!(json["tokens"]["access_token"], "access");
    assert_eq!(json["tokens"]["account_id"], "account-1");
    assert_eq!(serde_json::from_value::<CodexAuthStore>(json).unwrap(), store);
}

#[test]
fn codex_usage_accepts_quota_windows_credits_and_extra_limits() {
    let usage: CodexUsageSnapshot = serde_json::from_value(serde_json::json!({
        "plan_type": "pro",
        "rate_limit": {
            "allowed": true,
            "limit_reached": false,
            "primary_window": {
                "used_percent": 25,
                "limit_window_seconds": 18000,
                "reset_after_seconds": 100,
                "reset_at": 200
            },
            "secondary_window": {
                "used_percent": 50,
                "limit_window_seconds": 604800,
                "reset_after_seconds": 300,
                "reset_at": 400
            }
        },
        "credits": {
            "has_credits": true,
            "unlimited": false,
            "balance": "12.50"
        },
        "additional_rate_limits": [{
            "limit_name": "review",
            "metered_feature": "review",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false
            }
        }]
    }))
    .unwrap();
    assert_eq!(usage.plan_type.as_deref(), Some("pro"));
    assert_eq!(
        usage
            .rate_limit
            .as_ref()
            .and_then(|limit| limit.secondary_window.as_ref())
            .map(|window| window.used_percent),
        Some(50.0)
    );
    assert_eq!(usage.additional_rate_limits.len(), 1);
}
