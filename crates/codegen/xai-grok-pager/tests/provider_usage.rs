use chrono::Utc;
use xai_grok_pager::views::usage::{
    format_codex_usage_error, format_codex_usage_summary, format_combined_usage_summary,
};
use xai_grok_shell::codex_auth::{
    CodexAdditionalRateLimit, CodexCredits, CodexRateLimit, CodexRateLimitWindow,
    CodexTokenUsageStats, CodexUsageSnapshot,
};

fn window(used_percent: f64, seconds: i64) -> CodexRateLimitWindow {
    CodexRateLimitWindow {
        used_percent,
        limit_window_seconds: seconds,
        reset_after_seconds: 60 * 60,
        reset_at: 0,
    }
}

fn snapshot(limit_reached: bool, has_credits: bool) -> CodexUsageSnapshot {
    CodexUsageSnapshot {
        account: None,
        plan_type: Some("pro".to_string()),
        rate_limit: Some(CodexRateLimit {
            allowed: true,
            limit_reached,
            primary_window: Some(window(28.0, 5 * 60 * 60)),
            secondary_window: Some(window(59.0, 7 * 24 * 60 * 60)),
        }),
        credits: Some(CodexCredits {
            has_credits,
            unlimited: false,
            balance: has_credits.then(|| serde_json::json!(12.6)),
        }),
        spend_control: None,
        additional_rate_limits: vec![CodexAdditionalRateLimit {
            limit_name: Some("codex_other".to_string()),
            metered_feature: None,
            rate_limit: Some(CodexRateLimit {
                allowed: true,
                limit_reached: false,
                primary_window: Some(window(50.0, 5 * 60 * 60)),
                secondary_window: None,
            }),
        }],
        rate_limit_reached_type: None,
        token_usage: Some(CodexTokenUsageStats {
            lifetime_tokens: Some(1_234_567),
            peak_daily_tokens: None,
            longest_running_turn_sec: None,
            current_streak_days: None,
            longest_streak_days: None,
            daily_usage_buckets: None,
        }),
        fetched_at: Utc::now(),
    }
}

#[test]
fn combined_usage_labels_both_providers() {
    assert_eq!(
        format_combined_usage_summary("Weekly limit: 20%", "5h limit: 80% left"),
        "xAI\nWeekly limit: 20%\n\nOpenAI Codex\n5h limit: 80% left"
    );
}

#[test]
fn codex_usage_shows_remaining_windows_credits_and_additional_limits() {
    let summary = format_codex_usage_summary(&snapshot(false, true));
    assert!(summary.contains("5h limit: 72% left"), "{summary}");
    assert!(summary.contains("Weekly limit: 41% left"), "{summary}");
    assert!(
        summary.contains("Codex Other 5h limit: 50% left"),
        "{summary}"
    );
    assert!(summary.contains("Credits: 13 credits"), "{summary}");
    assert!(summary.contains("Lifetime tokens: 1,234,567"), "{summary}");
}

#[test]
fn exhausted_quota_distinguishes_credits_from_rate_limit() {
    let using_credits = format_codex_usage_summary(&snapshot(true, true));
    assert!(using_credits.contains("Status: Using credits"));
    assert!(!using_credits.contains("Status: Rate limited"));

    let rate_limited = format_codex_usage_summary(&snapshot(true, false));
    assert!(rate_limited.contains("Status: Rate limited"));
}

#[test]
fn disconnected_codex_usage_is_explicit() {
    assert_eq!(
        format_codex_usage_error("Not connected; run `open-grok login --codex`"),
        "Not connected. Run `open-grok login --codex`."
    );
}
