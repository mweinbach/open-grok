//! HTTP hook handler runner.
//!
//! Executes hooks by POSTing the event envelope JSON to a URL endpoint.
//! Supports the same blocking (deny/allow) response format as command hooks.

use std::net::IpAddr;
use std::time::{Duration, Instant};

use serde::Deserialize;
use url::Url;

use crate::config::HookSpec;
use crate::event::HookEventEnvelope;
use crate::result::{HookDecision, HttpInfo};

use super::{HookRunOutput, HookRunnerResult, RunContext};

/// Maximum characters to keep from the response body for the preview.
const RESPONSE_PREVIEW_MAX: usize = 200;

/// The JSON result structure expected from blocking HTTP hooks.
#[derive(Debug, Deserialize)]
struct HttpHookOutput {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
}

/// CWE-918: Returns `true` if an IP address is in a private, link-local,
/// or cloud metadata range that should be blocked to prevent SSRF attacks.
///
/// Loopback (`127.x` / `::1`) is allowed for local development servers.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if octets[0] == 127 {
                return false; // loopback — allowed for local dev
            }
            if octets[0] == 10 {
                return true; // RFC 1918: 10.0.0.0/8
            }
            if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                return true; // RFC 1918: 172.16.0.0/12
            }
            if octets[0] == 192 && octets[1] == 168 {
                return true; // RFC 1918: 192.168.0.0/16
            }
            if octets[0] == 169 && octets[1] == 254 {
                return true; // RFC 3927: 169.254.0.0/16 (link-local, cloud metadata)
            }
            if octets[0] == 100 && (64..=127).contains(&octets[1]) {
                return true; // RFC 6598: 100.64.0.0/10 (CGNAT)
            }
            if v4.is_unspecified() {
                return true; // 0.0.0.0
            }
            false
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return false; // ::1 — allowed for local dev
            }
            if v6.is_unspecified() {
                return true; // ::
            }
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_ip(&IpAddr::V4(v4));
            }
            let segments = v6.segments();
            if segments[0] & 0xffc0 == 0xfe80 {
                return true; // fe80::/10 — link-local
            }
            if segments[0] & 0xfe00 == 0xfc00 {
                return true; // fc00::/7 — unique local (ULA)
            }
            false
        }
    }
}

/// CWE-918: Validate a hook URL to prevent SSRF.
///
/// Requirements:
/// - Only HTTPS scheme is allowed (reject HTTP / other schemes).
/// - Resolved IP addresses must not be in private/link-local/metadata ranges.
async fn validate_hook_url(url: &str) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;

    // Restrict to HTTPS only.
    if parsed.scheme() != "https" {
        return Err(format!(
            "only https:// URLs are allowed for HTTP hooks, got {}://",
            parsed.scheme()
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // If host is a literal IP, check it directly.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(format!("URL resolves to blocked private/internal IP: {ip}"));
        }
        return Ok(());
    }

    // DNS resolution check.
    let port = parsed.port_or_known_default().unwrap_or(443);
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&addr_str)
        .await
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("DNS resolved no addresses for {host}"));
    }

    for addr in &addrs {
        if is_blocked_ip(&addr.ip()) {
            return Err(format!(
                "URL host {host} resolves to blocked private/internal IP: {}",
                addr.ip()
            ));
        }
    }

    Ok(())
}

/// Run a single HTTP hook.
///
/// POSTs the serialized `HookEventEnvelope` as JSON to `spec.url`.
/// For blocking hooks (`PreToolUse`), parses the response JSON for
/// `{"decision": "allow"}` or `{"decision": "deny", "reason": "..."}`.
/// For non-blocking hooks, any 2xx response is success.
///
/// Respects `spec.timeout_ms` for the entire request.
pub async fn run_http_hook(
    spec: &HookSpec,
    envelope: &HookEventEnvelope,
    _ctx: &RunContext<'_>,
    is_blocking: bool,
) -> HookRunOutput {
    let start = Instant::now();

    let Some(ref raw_url) = spec.url else {
        return (
            HookRunnerResult::Failed("http hook has no 'url' field".into()),
            start.elapsed(),
            None,
        );
    };

    // Expand `${VAR}` / `$VAR` in the URL right before validation. We
    // re-run expansion here (in addition to the load-time pass in
    // `parse_hook_file`) because plugin URLs can reference plugin-injected
    // vars (e.g. `${CLAUDE_PLUGIN_ROOT}/check`) that only land in
    // `spec.extra_env` after the plugin adapter wires them in.
    //
    // For plugin hooks specifically: the load-time pass in
    // `parse_hook_file` runs BEFORE the plugin adapter populates
    // `extra_env` with plugin keys, so `${CLAUDE_PLUGIN_ROOT}` etc.
    // survive that pass and are resolved here at runtime. For
    // non-plugin hooks the load-time pass already resolved everything
    // resolvable, and this pass is effectively a no-op.
    //
    // Unset refs are preserved verbatim, so `validate_hook_url` will
    // reject them with an "invalid URL" error rather than silently
    // smuggling a literal `${VAR}` past validation.
    let expanded_url = crate::env_expand::expand_env_vars_with_extra(raw_url, &spec.extra_env);
    let url: &str = &expanded_url;
    // For tracing/log purposes prefer the pre-expansion source so
    // resolved values from the user `env` map (which may contain
    // secrets like API tokens) don't land in `~/.grok/logs`. Falls
    // back to the expanded form if the spec was constructed by a
    // legacy path that didn't populate `url_raw`. The same `log_url`
    // is also threaded into `format!("HTTP request failed for {}:
    // {}", log_url, e.without_url())` below so reqwest's default
    // `Display` (which appends the request URL) does not bypass the
    // raw-source preference.
    let log_url: &str = spec.url_raw.as_deref().unwrap_or(url);

    // Helper: build an `HttpInfo` populated with both the
    // post-expansion `url` (for SSRF debugging) and the raw source
    // form (for any user-facing display surface). See `HttpInfo`
    // rustdoc on `crate::result::HttpInfo` for the contract.
    let make_info = |status: Option<u16>, preview: Option<String>| -> HttpInfo {
        HttpInfo {
            url: url.to_owned(),
            raw_url: spec.url_raw.clone(),
            status,
            response_preview: preview,
        }
    };

    // CWE-918: Validate URL before sending any data.
    if let Err(reason) = validate_hook_url(url).await {
        tracing::warn!(
            hook_name = %spec.name,
            url = %log_url,
            %reason,
            "SSRF protection: blocked HTTP hook URL"
        );
        return (
            HookRunnerResult::Failed(format!("blocked by SSRF protection: {reason}")),
            start.elapsed(),
            Some(make_info(None, None)),
        );
    }

    let body = match serde_json::to_string(envelope) {
        Ok(j) => j,
        Err(e) => {
            return (
                HookRunnerResult::Failed(format!("failed to serialize envelope: {e}")),
                start.elapsed(),
                Some(make_info(None, None)),
            );
        }
    };

    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(spec.timeout_ms))
        .build()
        .unwrap_or_default();

    let response = match client
        .post(url)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let elapsed = start.elapsed();
            // SECURITY: `reqwest::Error::Display` unconditionally
            // appends the request URL. If the resolved URL embeds a
            // secret resolved from the user `env` map (e.g.
            // `?token=ghp_REAL_SECRET`), the secret would otherwise
            // land in `HookRunResult::Failed.error` and surface in
            // pager scrollback / wire DTOs. `e.without_url()` strips
            // the URL from the formatted output so we substitute our
            // own `log_url` (which prefers the raw source form) in
            // its place.
            let error = if e.is_timeout() {
                format!("timed out after {}ms", spec.timeout_ms)
            } else {
                format!("HTTP request failed for {}: {}", log_url, e.without_url())
            };
            return (
                HookRunnerResult::Failed(error),
                elapsed,
                Some(make_info(None, None)),
            );
        }
    };

    let status = response.status();
    let status_code = status.as_u16();
    let elapsed = start.elapsed();

    tracing::debug!(
        hook_name = %spec.name,
        url = %log_url,
        status = status_code,
        elapsed_ms = elapsed.as_millis() as u64,
        "http hook completed"
    );

    if !is_blocking {
        let http_info = Some(make_info(Some(status_code), None));
        if status.is_success() {
            return (HookRunnerResult::Success, elapsed, http_info);
        }
        return (
            HookRunnerResult::Failed(format!("HTTP status {}", status)),
            elapsed,
            http_info,
        );
    }

    // Blocking hook: parse response JSON for decision.
    let response_text = match response.text().await {
        Ok(t) => t,
        Err(e) => {
            // SECURITY: same `without_url()` reasoning as the send
            // failure above -- reqwest's body-read error also includes
            // the URL by default.
            return (
                HookRunnerResult::Failed(format!(
                    "failed to read response body for {}: {}",
                    log_url,
                    e.without_url()
                )),
                elapsed,
                Some(make_info(Some(status_code), None)),
            );
        }
    };

    let response_preview = if response_text.trim().is_empty() {
        None
    } else {
        Some(truncate_preview(&response_text))
    };

    let http_info = Some(make_info(Some(status_code), response_preview.clone()));

    let result = parse_http_blocking_result(&response_text, status, &spec.name);
    (result, elapsed, http_info)
}

/// Parse an HTTP blocking hook response into a `HookRunnerResult`.
///
/// This is the HTTP analogue of `command::parse_blocking_result`.
/// Extracted as a standalone function so it can be unit-tested without
/// making real HTTP requests.
fn parse_http_blocking_result(
    response_text: &str,
    status: reqwest::StatusCode,
    hook_name: &str,
) -> HookRunnerResult {
    if response_text.trim().is_empty() {
        // No body: use HTTP status as fallback.
        if status.is_success() {
            return HookRunnerResult::Decision(HookDecision::Allow);
        }
        return HookRunnerResult::Failed(format!("HTTP status {} with empty body", status));
    }

    match serde_json::from_str::<HttpHookOutput>(response_text) {
        Ok(output) => {
            if output.decision == "deny" {
                let reason = output
                    .reason
                    .unwrap_or_else(|| format!("denied by hook '{}'", hook_name));
                HookRunnerResult::Decision(HookDecision::Deny {
                    reason,
                    hook_name: hook_name.to_string(),
                })
            } else if output.decision == "allow" {
                HookRunnerResult::Decision(HookDecision::Allow)
            } else {
                HookRunnerResult::Failed(format!(
                    "unknown decision value '{}' from hook '{}'",
                    output.decision, hook_name
                ))
            }
        }
        Err(e) => {
            // Cannot parse response: fail-open if status is success.
            if status.is_success() {
                tracing::warn!(
                    hook_name = %hook_name,
                    error = %e,
                    "could not parse HTTP hook response JSON, treating as allow"
                );
                HookRunnerResult::Decision(HookDecision::Allow)
            } else {
                HookRunnerResult::Failed(format!(
                    "HTTP status {} and failed to parse response: {e}",
                    status
                ))
            }
        }
    }
}

/// Truncate a response body string for preview display.
///
/// Uses `char_indices` to find a safe UTF-8 boundary so we never panic
/// on multi-byte characters.
fn truncate_preview(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= RESPONSE_PREVIEW_MAX {
        trimmed.to_string()
    } else {
        // Find the last char boundary at or before RESPONSE_PREVIEW_MAX bytes.
        let boundary = trimmed
            .char_indices()
            .take_while(|&(i, _)| i <= RESPONSE_PREVIEW_MAX)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        let mut preview = trimmed[..boundary].to_string();
        preview.push_str("...");
        preview
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    // ── parse_http_blocking_result tests ──────────────────────────

    #[test]
    fn http_allow_json() {
        let result =
            parse_http_blocking_result(r#"{"decision":"allow"}"#, StatusCode::OK, "test-hook");
        assert!(matches!(
            result,
            HookRunnerResult::Decision(HookDecision::Allow)
        ));
    }

    #[test]
    fn http_deny_json_with_reason() {
        let result = parse_http_blocking_result(
            r#"{"decision":"deny","reason":"dangerous command"}"#,
            StatusCode::OK,
            "test-hook",
        );
        match result {
            HookRunnerResult::Decision(HookDecision::Deny { reason, hook_name }) => {
                assert_eq!(reason, "dangerous command");
                assert_eq!(hook_name, "test-hook");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn http_deny_json_without_reason() {
        let result =
            parse_http_blocking_result(r#"{"decision":"deny"}"#, StatusCode::OK, "my-hook");
        match result {
            HookRunnerResult::Decision(HookDecision::Deny { reason, .. }) => {
                assert!(
                    reason.contains("my-hook"),
                    "reason should mention hook name"
                );
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn http_unknown_decision_fails() {
        let result =
            parse_http_blocking_result(r#"{"decision":"maybe"}"#, StatusCode::OK, "test-hook");
        match result {
            HookRunnerResult::Failed(msg) => {
                assert!(msg.contains("maybe"));
                assert!(msg.contains("test-hook"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_empty_body_success_allows() {
        let result = parse_http_blocking_result("", StatusCode::OK, "test-hook");
        assert!(matches!(
            result,
            HookRunnerResult::Decision(HookDecision::Allow)
        ));
    }

    #[test]
    fn http_empty_body_whitespace_success_allows() {
        let result = parse_http_blocking_result("   \n  ", StatusCode::OK, "test-hook");
        assert!(matches!(
            result,
            HookRunnerResult::Decision(HookDecision::Allow)
        ));
    }

    #[test]
    fn http_empty_body_error_status_fails() {
        let result = parse_http_blocking_result("", StatusCode::INTERNAL_SERVER_ERROR, "test-hook");
        match result {
            HookRunnerResult::Failed(msg) => {
                assert!(msg.contains("500"));
                assert!(msg.contains("empty body"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_invalid_json_success_status_fail_open() {
        // Unparseable JSON with 200 OK should fail-open to allow.
        let result = parse_http_blocking_result("not json at all", StatusCode::OK, "test-hook");
        assert!(matches!(
            result,
            HookRunnerResult::Decision(HookDecision::Allow)
        ));
    }

    #[test]
    fn http_invalid_json_error_status_fails() {
        // Unparseable JSON with 500 should fail.
        let result =
            parse_http_blocking_result("not json", StatusCode::INTERNAL_SERVER_ERROR, "test-hook");
        match result {
            HookRunnerResult::Failed(msg) => {
                assert!(msg.contains("500"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_deny_with_non_success_status() {
        let result = parse_http_blocking_result(
            r#"{"decision":"deny","reason":"forbidden"}"#,
            StatusCode::FORBIDDEN,
            "test-hook",
        );
        match result {
            HookRunnerResult::Decision(HookDecision::Deny { reason, .. }) => {
                assert_eq!(reason, "forbidden");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn http_allow_with_non_success_status() {
        let result = parse_http_blocking_result(
            r#"{"decision":"allow"}"#,
            StatusCode::BAD_REQUEST,
            "test-hook",
        );
        assert!(matches!(
            result,
            HookRunnerResult::Decision(HookDecision::Allow)
        ));
    }

    #[test]
    fn http_partial_json_success_fail_open() {
        let result =
            parse_http_blocking_result(r#"{"decision":"deny""#, StatusCode::OK, "test-hook");
        assert!(matches!(
            result,
            HookRunnerResult::Decision(HookDecision::Allow)
        ));
    }

    #[test]
    fn http_extra_fields_tolerated() {
        let result = parse_http_blocking_result(
            r#"{"decision":"deny","reason":"nope","extra":"ignored","count":42}"#,
            StatusCode::OK,
            "test-hook",
        );
        match result {
            HookRunnerResult::Decision(HookDecision::Deny { reason, .. }) => {
                assert_eq!(reason, "nope");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    // ── SSRF protection: is_blocked_ip tests ──────────────

    #[test]
    fn ssrf_blocks_rfc1918_10x() {
        assert!(is_blocked_ip(&"10.0.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"10.255.255.255".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_rfc1918_172x() {
        assert!(is_blocked_ip(&"172.16.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"172.31.255.255".parse().unwrap()));
        assert!(!is_blocked_ip(&"172.15.0.1".parse().unwrap()));
        assert!(!is_blocked_ip(&"172.32.0.1".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_rfc1918_192168() {
        assert!(is_blocked_ip(&"192.168.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"192.168.255.255".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_link_local_metadata() {
        assert!(is_blocked_ip(&"169.254.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_cgnat() {
        assert!(is_blocked_ip(&"100.64.0.1".parse().unwrap()));
        assert!(is_blocked_ip(&"100.127.255.255".parse().unwrap()));
        assert!(!is_blocked_ip(&"100.63.0.1".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_unspecified() {
        assert!(is_blocked_ip(&"0.0.0.0".parse().unwrap()));
        assert!(is_blocked_ip(&"::".parse().unwrap()));
    }

    #[test]
    fn ssrf_allows_loopback() {
        assert!(!is_blocked_ip(&"127.0.0.1".parse().unwrap()));
        assert!(!is_blocked_ip(&"::1".parse().unwrap()));
    }

    #[test]
    fn ssrf_allows_public_ips() {
        assert!(!is_blocked_ip(&"1.1.1.1".parse().unwrap()));
        assert!(!is_blocked_ip(&"8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_ipv6_link_local() {
        assert!(is_blocked_ip(&"fe80::1".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_ipv6_unique_local() {
        assert!(is_blocked_ip(&"fc00::1".parse().unwrap()));
        assert!(is_blocked_ip(&"fd00::1".parse().unwrap()));
    }

    #[test]
    fn ssrf_blocks_ipv4_mapped_ipv6_private() {
        assert!(is_blocked_ip(&"::ffff:10.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(is_blocked_ip(
            &"::ffff:192.168.1.1".parse::<IpAddr>().unwrap()
        ));
    }

    // ── SSRF protection: validate_hook_url tests ──────────

    #[tokio::test]
    async fn ssrf_rejects_http_scheme() {
        let result = validate_hook_url("http://example.com/hook").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("https://"));
    }

    #[tokio::test]
    async fn ssrf_rejects_ftp_scheme() {
        let result = validate_hook_url("ftp://example.com/hook").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("https://"));
    }

    #[tokio::test]
    async fn ssrf_rejects_private_ip_literal() {
        let result = validate_hook_url("https://10.0.0.1/hook").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("blocked"));
    }

    #[tokio::test]
    async fn ssrf_rejects_metadata_ip_literal() {
        let result = validate_hook_url("https://169.254.169.254/latest/meta-data/").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("blocked"));
    }

    #[tokio::test]
    async fn ssrf_allows_https_public_ip() {
        let result = validate_hook_url("https://1.1.1.1/hook").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ssrf_rejects_invalid_url() {
        let result = validate_hook_url("not a url").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid URL"));
    }

    // ── URL env-var expansion (extra_env precedence) ───────────

    use crate::config::HookSpec;
    use crate::event::{HookEventEnvelope, HookEventName, HookPayload};
    use crate::test_support::with_env_var;

    /// Regression: an HTTP hook whose `url` references a var present only
    /// in `spec.extra_env` (not the process env) must still be expanded
    /// at runtime by `run_http_hook`. This is the path used by plugin
    /// hooks where the plugin adapter wires `${CLAUDE_PLUGIN_ROOT}` into
    /// `extra_env` after the load-time pass in `parse_hook_file` ran.
    ///
    /// Documentation-of-intent unit test for the helper. The end-to-end
    /// proof through `run_http_hook` lives in
    /// [`run_http_hook_uses_post_expansion_url_for_ssrf`] below.
    #[test]
    fn url_extra_env_takes_precedence_in_runtime_expansion() {
        // Use the same helper the runtime path uses; we don't need to
        // make a real network call to verify the substitution, only that
        // the helper resolves the right value from extra_env.
        let mut extra = std::collections::HashMap::new();
        extra.insert("PLUGIN_HOST".to_string(), "example.com".to_string());
        let out =
            crate::env_expand::expand_env_vars_with_extra("https://${PLUGIN_HOST}/check", &extra);
        assert_eq!(out, "https://example.com/check");
    }

    /// If `extra_env` shadows a process-env var with the same name, the
    /// `extra_env` value wins. This matches the contract documented on
    /// `HookSpec::extra_env` and matches the lookup order in
    /// `runner/command.rs`'s pre-flight check. Documentation-of-intent
    /// unit test (the end-to-end variant via `run_http_hook` lives in
    /// `tests/integration.rs`).
    #[test]
    fn url_extra_env_shadows_process_env() {
        let key = "GROK_HOOKS_HTTP_TEST_SHADOW";
        with_env_var(key, Some("from-process"), || {
            let mut extra = std::collections::HashMap::new();
            extra.insert(key.to_string(), "from-extra".to_string());
            let out = crate::env_expand::expand_env_vars_with_extra(
                &format!("https://${{{key}}}/x"),
                &extra,
            );
            assert_eq!(out, "https://from-extra/x");
        });
    }

    /// Regression: a URL with multiple `${VAR}` references must
    /// expand all of them. Locks down behaviour against shellexpand
    /// regressions that affect consecutive references.
    #[test]
    fn url_with_multiple_consecutive_env_refs_expands_all() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("HOST".to_string(), "api.example.com".to_string());
        extra.insert("PORT".to_string(), "8443".to_string());
        extra.insert("ROUTE".to_string(), "v2/check".to_string());
        let out = crate::env_expand::expand_env_vars_with_extra(
            "https://${HOST}:${PORT}/${ROUTE}",
            &extra,
        );
        assert_eq!(out, "https://api.example.com:8443/v2/check");
    }

    /// Regression: SSRF validation in `run_http_hook` must
    /// operate on the POST-expansion URL. We construct a `HookSpec`
    /// with `url: "https://${INTERNAL}/hook"` and `extra_env` mapping
    /// `INTERNAL=10.0.0.1`, then call `run_http_hook` directly and
    /// assert the failure carries SSRF-blocking language and that the
    /// `HttpInfo.url` returned for scrollback is the post-expansion
    /// form (`10.0.0.1`) rather than the literal placeholder.
    #[tokio::test]
    async fn run_http_hook_uses_post_expansion_url_for_ssrf() {
        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert("INTERNAL_HOST_SSRF".to_string(), "10.0.0.1".to_string());

        let raw = "https://${INTERNAL_HOST_SSRF}/hook";
        let spec = HookSpec {
            name: "test-ssrf-post-expand".into(),
            event: HookEventName::PreToolUse,
            handler_type: "http".into(),
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: None,
            command_raw: None,
            url: Some(raw.to_string()),
            url_raw: Some(raw.to_string()),
            timeout_ms: 1000,
            source_dir: std::env::temp_dir(),
            extra_env,
        };

        let envelope = HookEventEnvelope {
            hook_event_name: HookEventName::PreToolUse,
            session_id: "test".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            payload: HookPayload::PreToolUse {
                tool_name: "test".into(),
                tool_use_id: "id-1".into(),
                tool_input: serde_json::json!({}),
                tool_input_truncated: false,
                permission_mode: None,
                subagent_type: None,
            },
        };
        let ctx = crate::runner::RunContext {
            session_id: "test",
            workspace_root: "/tmp",
        };
        let (result, _, info) = run_http_hook(&spec, &envelope, &ctx, true).await;

        match result {
            crate::runner::HookRunnerResult::Failed(reason) => {
                assert!(
                    reason.contains("blocked") || reason.contains("SSRF"),
                    "expected SSRF block message, got: {reason}"
                );
            }
            other => panic!("expected SSRF Failed, got {other:?}"),
        }

        let info = info.expect("HttpInfo should be present for SSRF block path");
        assert_eq!(
            info.url, "https://10.0.0.1/hook",
            "HttpInfo.url must reflect the post-expansion URL (the actual target SSRF blocked)"
        );
        // HttpInfo.raw_url must mirror the source
        // form so any future scrollback/wire-DTO consumer can prefer
        // it for user-facing display.
        assert_eq!(
            info.raw_url.as_deref(),
            Some("https://${INTERNAL_HOST_SSRF}/hook"),
            "HttpInfo.raw_url must mirror HookSpec::url_raw"
        );
    }

    /// Regression: `reqwest::Error::Display`
    /// unconditionally appends the request URL. If the resolved URL
    /// embeds a secret resolved via `${TOKEN}` substitution from the
    /// user `env` map, the secret would land in
    /// `HookRunResult::Failed.error` and surface in pager scrollback
    /// without the raw-fields work catching it. This test
    /// builds a HookSpec that resolves to a guaranteed-dead host
    /// (TEST-NET-1 192.0.2.0/24 from RFC 5737, used in docs) with a
    /// secret-bearing query string, calls run_http_hook, and asserts
    /// the secret does NOT appear in the returned error message.
    #[tokio::test]
    async fn run_http_hook_scrubs_url_from_reqwest_error() {
        // Use a TEST-NET-1 host (RFC 5737, "MUST NOT be used in
        // public networks"). It is not RFC1918 so SSRF validation
        // will let it through, but no real DNS or connection will
        // succeed -- reqwest will surface a connection error whose
        // default Display includes the URL.
        let secret = "ghp_VERY_REAL_SECRET_TOKEN_42";
        let mut extra_env = std::collections::HashMap::new();
        extra_env.insert("RUNTIME_HOST".to_string(), "192.0.2.1".to_string());
        extra_env.insert("MY_TOKEN".to_string(), secret.to_string());

        let raw = "https://${RUNTIME_HOST}/check?token=${MY_TOKEN}";
        let spec = HookSpec {
            name: "test-scrub-reqwest-error".into(),
            event: HookEventName::PreToolUse,
            handler_type: "http".into(),
            configured_matcher: None,
            matcher: None,
            enabled: true,
            command: None,
            command_raw: None,
            url: Some(raw.to_string()),
            url_raw: Some(raw.to_string()),
            // Short timeout so the test doesn't hang waiting for the
            // dead host. Still long enough to actually attempt the
            // connection so we exercise the Err(e) branch of `send().await`.
            timeout_ms: 500,
            source_dir: std::env::temp_dir(),
            extra_env,
        };
        let envelope = HookEventEnvelope {
            hook_event_name: HookEventName::PreToolUse,
            session_id: "test".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            payload: HookPayload::PreToolUse {
                tool_name: "test".into(),
                tool_use_id: "id-1".into(),
                tool_input: serde_json::json!({}),
                tool_input_truncated: false,
                permission_mode: None,
                subagent_type: None,
            },
        };
        let ctx = crate::runner::RunContext {
            session_id: "test",
            workspace_root: "/tmp",
        };

        let (result, _, info) = run_http_hook(&spec, &envelope, &ctx, true).await;

        // Either `Failed` (timeout / connection error) is fine; both
        // exercise paths that previously embedded the raw URL via
        // `format!("...{e}")`. Pure timeouts use a different
        // formatting branch (no URL involved), so prefer the
        // connection-error case but tolerate either.
        let error_text = match result {
            crate::runner::HookRunnerResult::Failed(reason) => reason,
            other => panic!("expected Failed, got {other:?}"),
        };

        // The secret must NOT be in the error text. This covers BOTH
        // the timeout branch (which doesn't format the URL at all,
        // so trivially passes) and the connection-error branch
        // (which formats `e.without_url()`, scrubbing the URL).
        assert!(
            !error_text.contains(secret),
            "secret leaked into error text: {error_text}"
        );

        // The error must mention the raw URL form (so users can see
        // which hook failed) -- never the resolved form, which would
        // include the secret-bearing query string.
        if !error_text.contains("timed out") {
            // Connection-error branch: error must reference the raw
            // form, not the resolved one.
            assert!(
                error_text.contains("${RUNTIME_HOST}") || error_text.contains("${MY_TOKEN}"),
                "expected error to reference the raw URL form, got: {error_text}"
            );
        }

        // HttpInfo.url is still post-expansion (intentional, for SSRF
        // debugging). The wire-DTO consumer must prefer raw_url for
        // display -- documented in the HttpInfo rustdoc.
        let info = info.expect("HttpInfo should be present for connection failures too");
        assert_eq!(
            info.url,
            "https://192.0.2.1/check?token=ghp_VERY_REAL_SECRET_TOKEN_42"
        );
        assert_eq!(info.raw_url.as_deref(), Some(raw));
    }

    /// Unresolved `${VAR}` refs are preserved verbatim by the helper,
    /// which means `validate_hook_url` will reject the URL with an
    /// "invalid URL" error. This is the desired behaviour: a hook
    /// referencing an unset var must surface a clear failure rather than
    /// silently smuggling the literal placeholder past validation.
    #[tokio::test]
    async fn url_unresolved_var_fails_validation() {
        let key = "GROK_HOOKS_HTTP_TEST_UNRESOLVED";
        // Step 1 (sync): ensure the var is unset and run the
        // expansion. `with_env_var` uses `catch_unwind` so the closure
        // is synchronous; we deliberately do the async `validate_hook_url`
        // call OUTSIDE the helper so we don't try to nest tokio runtimes.
        let expanded = with_env_var(key, None, || {
            let extra = std::collections::HashMap::new();
            crate::env_expand::expand_env_vars_with_extra(
                &format!("https://${{{key}}}/check"),
                &extra,
            )
        });
        // The literal placeholder is preserved.
        assert!(expanded.contains(&format!("${{{key}}}")));
        // Url::parse rejects strings with literal `${` because `{`
        // isn't a valid URL character.
        let result = validate_hook_url(&expanded).await;
        assert!(result.is_err(), "expected invalid URL error, got Ok");
    }
}
