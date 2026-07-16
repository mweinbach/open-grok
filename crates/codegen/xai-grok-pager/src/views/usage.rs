//! Provider-aware formatting for the manual `/usage` summary.

use xai_grok_shell::codex_auth::{CodexRateLimitWindow, CodexUsageSnapshot};

/// Join independently produced provider summaries into one scrollback block.
pub fn format_combined_usage_summary(xai: &str, codex: &str) -> String {
    format!("xAI\n{xai}\n\nOpenAI Codex\n{codex}")
}

/// Format an xAI billing transport/parse error without hiding the Codex half.
pub fn format_xai_usage_error(error: &str) -> String {
    if error.starts_with("View current usage at ") {
        error.to_string()
    } else {
        format!("Usage unavailable: {error}")
    }
}

/// Format a Codex usage error, giving the disconnected state an actionable
/// message distinct from a transient service failure.
pub fn format_codex_usage_error(error: &str) -> String {
    if error.to_ascii_lowercase().contains("not connected") {
        "Not connected. Run `open-grok login --codex`.".to_string()
    } else {
        format!("Usage unavailable: {error}")
    }
}

/// Format the same Codex quota concepts surfaced by codex-rs: remaining
/// percentages for each time window, reset timing, credits, and token totals.
pub fn format_codex_usage_summary(snapshot: &CodexUsageSnapshot) -> String {
    let mut lines = Vec::new();

    if let Some(account) = snapshot.account.as_ref()
        && let Some(email) = account
            .email
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!("Account: {email}"));
    }
    let plan = snapshot.plan_type.as_deref().or_else(|| {
        snapshot
            .account
            .as_ref()
            .and_then(|account| account.plan_type.as_deref())
    });
    if let Some(plan) = plan.filter(|value| !value.trim().is_empty()) {
        lines.push(format!("Plan: {}", display_plan_type(plan)));
    }

    if let Some(rate_limit) = snapshot.rate_limit.as_ref() {
        if let Some(window) = rate_limit.primary_window.as_ref() {
            lines.push(format_codex_window(window, false));
        }
        if let Some(window) = rate_limit.secondary_window.as_ref() {
            lines.push(format_codex_window(window, true));
        }
    }
    for additional in &snapshot.additional_rate_limits {
        let Some(rate_limit) = additional.rate_limit.as_ref() else {
            continue;
        };
        let bucket = additional
            .limit_name
            .as_deref()
            .or(additional.metered_feature.as_deref())
            .filter(|value| !value.trim().is_empty())
            .map(display_plan_type)
            .unwrap_or_else(|| "Additional".to_string());
        if let Some(window) = rate_limit.primary_window.as_ref() {
            lines.push(format!("{bucket} {}", format_codex_window(window, false)));
        }
        if let Some(window) = rate_limit.secondary_window.as_ref() {
            lines.push(format!("{bucket} {}", format_codex_window(window, true)));
        }
    }

    let quota_reached = snapshot
        .rate_limit
        .as_ref()
        .is_some_and(|rate_limit| rate_limit.limit_reached)
        || reached_type_present(snapshot);
    let spend_control_reached = snapshot
        .spend_control
        .as_ref()
        .is_some_and(|control| control.reached);
    if quota_reached || spend_control_reached {
        let credits_can_cover = snapshot
            .credits
            .as_ref()
            .is_some_and(|credits| credits.unlimited || credits.has_credits);
        lines.push(
            if quota_reached && credits_can_cover && !spend_control_reached {
                "Status: Using credits".to_string()
            } else {
                "Status: Rate limited".to_string()
            },
        );
    }

    if let Some(credits) = snapshot.credits.as_ref() {
        if credits.unlimited {
            lines.push("Credits: Unlimited".to_string());
        } else if credits.has_credits {
            let balance = credits.balance.as_ref().and_then(format_credit_balance);
            let balance = balance.map_or_else(
                || "Available".to_string(),
                |balance| format!("{balance} credits"),
            );
            lines.push(format!("Credits: {balance}"));
        }
    }

    if let Some(tokens) = snapshot
        .token_usage
        .as_ref()
        .and_then(|stats| stats.lifetime_tokens)
        .filter(|tokens| *tokens >= 0)
    {
        lines.push(format!(
            "Lifetime tokens: {}",
            format_integer(tokens as u64)
        ));
    }

    if lines.is_empty() {
        "Usage data unavailable.".to_string()
    } else {
        lines.join("\n")
    }
}

fn reached_type_present(snapshot: &CodexUsageSnapshot) -> bool {
    snapshot
        .rate_limit_reached_type
        .as_ref()
        .is_some_and(|value| !value.is_null())
}

fn format_codex_window(window: &CodexRateLimitWindow, secondary: bool) -> String {
    let label = window_label(window.limit_window_seconds, secondary);
    let remaining = (100.0 - window.used_percent).clamp(0.0, 100.0);
    let mut line = format!("{label}: {remaining:.0}% left");
    if window.reset_after_seconds > 0 {
        line.push_str(&format!(
            " (resets in {})",
            format_duration(window.reset_after_seconds)
        ));
    }
    line
}

fn window_label(seconds: i64, secondary: bool) -> &'static str {
    const FIVE_HOURS: i64 = 5 * 60 * 60;
    const DAY: i64 = 24 * 60 * 60;
    const WEEK: i64 = 7 * DAY;
    const MONTH: i64 = 30 * DAY;
    const TOLERANCE_PERCENT: i64 = 5;

    let approximately = |expected: i64| {
        let tolerance = expected * TOLERANCE_PERCENT / 100;
        seconds >= expected - tolerance && seconds <= expected + tolerance
    };
    if approximately(FIVE_HOURS) {
        "5h limit"
    } else if approximately(DAY) {
        "Daily limit"
    } else if approximately(WEEK) {
        "Weekly limit"
    } else if approximately(MONTH) {
        "Monthly limit"
    } else if secondary {
        "Secondary usage limit"
    } else {
        "Usage limit"
    }
}

fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else {
        format!("{}m", minutes.max(1))
    }
}

fn format_credit_balance(value: &serde_json::Value) -> Option<String> {
    let number = match value {
        serde_json::Value::Number(number) => number.as_f64(),
        serde_json::Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    (number.is_finite() && number > 0.0).then(|| format!("{number:.0}"))
}

fn display_plan_type(value: &str) -> String {
    value
        .split(['_', '-'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_integer(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            output.push(',');
        }
        output.push(ch);
    }
    output
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use xai_grok_shell::codex_auth::{
        CodexAccountSummary, CodexAdditionalRateLimit, CodexCredits, CodexRateLimit,
        CodexRateLimitWindow, CodexTokenUsageStats,
    };

    use super::*;

    fn snapshot() -> CodexUsageSnapshot {
        CodexUsageSnapshot {
            account: Some(CodexAccountSummary {
                email: Some("dev@example.com".to_string()),
                account_id: Some("acct_123".to_string()),
                plan_type: Some("pro".to_string()),
            }),
            plan_type: None,
            rate_limit: Some(CodexRateLimit {
                allowed: true,
                limit_reached: false,
                primary_window: Some(CodexRateLimitWindow {
                    used_percent: 28.0,
                    limit_window_seconds: 5 * 60 * 60,
                    reset_after_seconds: 2 * 60 * 60 + 15 * 60,
                    reset_at: 0,
                }),
                secondary_window: Some(CodexRateLimitWindow {
                    used_percent: 59.0,
                    limit_window_seconds: 7 * 24 * 60 * 60,
                    reset_after_seconds: 2 * 24 * 60 * 60,
                    reset_at: 0,
                }),
            }),
            credits: Some(CodexCredits {
                has_credits: true,
                unlimited: false,
                balance: Some(serde_json::json!(12.6)),
            }),
            spend_control: None,
            additional_rate_limits: vec![CodexAdditionalRateLimit {
                limit_name: Some("codex_other".to_string()),
                metered_feature: None,
                rate_limit: Some(CodexRateLimit {
                    allowed: true,
                    limit_reached: false,
                    primary_window: Some(CodexRateLimitWindow {
                        used_percent: 50.0,
                        limit_window_seconds: 5 * 60 * 60,
                        reset_after_seconds: 60 * 60,
                        reset_at: 0,
                    }),
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
    fn combined_summary_labels_both_providers_once() {
        let summary = format_combined_usage_summary("Weekly limit: 20%", "5h limit: 80% left");
        assert_eq!(
            summary,
            "xAI\nWeekly limit: 20%\n\nOpenAI Codex\n5h limit: 80% left"
        );
    }

    #[test]
    fn codex_summary_matches_codex_remaining_quota_concepts() {
        let summary = format_codex_usage_summary(&snapshot());
        assert!(summary.contains("Account: dev@example.com"));
        assert!(summary.contains("Plan: Pro"));
        assert!(summary.contains("5h limit: 72% left (resets in 2h 15m)"));
        assert!(summary.contains("Weekly limit: 41% left (resets in 2d)"));
        assert!(summary.contains("Codex Other 5h limit: 50% left (resets in 1h)"));
        assert!(summary.contains("Credits: 13 credits"));
        assert!(summary.contains("Lifetime tokens: 1,234,567"));
    }

    #[test]
    fn exhausted_quota_with_credits_reports_using_credits() {
        let mut snapshot = snapshot();
        snapshot.rate_limit.as_mut().unwrap().limit_reached = true;
        let summary = format_codex_usage_summary(&snapshot);
        assert!(summary.contains("Status: Using credits"));
        assert!(!summary.contains("Status: Rate limited"));
    }

    #[test]
    fn exhausted_quota_without_credits_reports_rate_limited() {
        let mut snapshot = snapshot();
        snapshot.rate_limit.as_mut().unwrap().limit_reached = true;
        snapshot.credits = Some(CodexCredits {
            has_credits: false,
            unlimited: false,
            balance: None,
        });
        assert!(format_codex_usage_summary(&snapshot).contains("Status: Rate limited"));
    }

    #[test]
    fn disconnected_error_is_explicit_and_actionable() {
        assert_eq!(
            format_codex_usage_error("Not connected; run `open-grok login --codex`"),
            "Not connected. Run `open-grok login --codex`."
        );
    }
}
