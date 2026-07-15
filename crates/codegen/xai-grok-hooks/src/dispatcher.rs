use crate::discovery::HookRegistry;
use crate::event::{HookEventEnvelope, HookEventName};
use crate::result::{HookDecision, HookRunResult};
use crate::runner::{self, HookRunnerResult, RunContext};

/// Result of a `pre_tool_use` dispatch: the final decision plus per-hook
/// execution details (for scrollback enrichment).
pub struct PreToolUseResult {
    /// Final blocking decision (Allow or Deny).
    pub decision: HookDecision,
    /// Per-hook run results (includes HTTP info when applicable).
    pub results: Vec<HookRunResult>,
}

/// Dispatch a `pre_tool_use` event against all matching hooks.
///
/// Runs hooks sequentially in config order. Only an explicit `deny`
/// decision from a hook stops the chain and blocks the tool call.
///
/// Hook failures (timeouts, crashes, command-not-found, env-var
/// pre-spawn refusals, malformed output) are **fail-open**: the failure
/// is logged and surfaced in the per-hook results for the UI scrollback,
/// but the tool call continues as if the hook had allowed it. Grok
/// runs in protected environments where induced-failure bypass of
/// security hooks is not part of the threat model; the previous
/// fail-closed posture over-blocked innocent tool calls when
/// hooks timed out or had unrelated configuration errors.
///
/// Returns `Allow` if no hooks match, all hooks allow, or all failing
/// hooks are non-blocking by virtue of this fail-open policy.
pub async fn dispatch_pre_tool_use(
    registry: &HookRegistry,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> PreToolUseResult {
    let hooks = registry.hooks_for(HookEventName::PreToolUse);
    if hooks.is_empty() {
        return PreToolUseResult {
            decision: HookDecision::Allow,
            results: Vec::new(),
        };
    }

    let span = tracing::info_span!(
        "hooks.dispatch",
        hook_event = %HookEventName::PreToolUse,
        hook_count = hooks.len() as i64,
        num_success = tracing::field::Empty,
        num_failed = tracing::field::Empty,
        num_blocking = tracing::field::Empty,
        num_skipped = tracing::field::Empty,
        total_duration_ms = tracing::field::Empty,
    );
    let _enter = span.enter();

    let tool_name = extract_tool_name(envelope);
    let mut run_results = Vec::new();

    for spec in hooks {
        if !spec.enabled || crate::trust::is_hook_disabled(&spec.name) {
            tracing::info!(hook_name = %spec.name, "hook skipped (disabled)");
            run_results.push(HookRunResult::Skipped {
                hook_name: spec.name.clone(),
            });
            continue;
        }

        // Check matcher against tool name.
        if let Some(ref matcher) = spec.matcher
            && let Some(ref name) = tool_name
            && !matcher.is_match(name)
        {
            continue;
        }

        let _hook_span = tracing::info_span!(
            "hook.run",
            hook_name = %spec.name,
            hook_event = %HookEventName::PreToolUse,
        )
        .entered();

        let (result, elapsed, http_info) = runner::run_hook(spec, envelope, ctx, true).await;

        match result {
            HookRunnerResult::Decision(HookDecision::Deny { reason, .. }) => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    reason = %reason,
                    "hook denied"
                );
                run_results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: format!("denied: {reason}"),
                    elapsed,
                    http_info,
                });
                record_dispatch_counts(&span, &run_results, 1);
                return PreToolUseResult {
                    decision: HookDecision::Deny {
                        reason,
                        hook_name: spec.name.clone(),
                    },
                    results: run_results,
                };
            }
            HookRunnerResult::Decision(HookDecision::Allow) => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "hook allowed"
                );
                run_results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                });
            }
            // Fail-open: hook failures (timeouts, crashes, refusals to
            // spawn, malformed output) are logged and recorded for the UI
            // but do not deny the tool call. Only an explicit `deny`
            // decision blocks. See module docs on dispatch_pre_tool_use
            // for the rationale (protected-environment threat model).
            HookRunnerResult::Failed(err) => {
                tracing::warn!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    error = %err,
                    "hook failed; ignoring (fail-open)"
                );
                run_results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: err.clone(),
                    elapsed,
                    http_info,
                });
            }
            HookRunnerResult::Success => {
                // Shouldn't happen for blocking hooks, but treat as allow.
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "hook completed"
                );
                run_results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                });
            }
        }
    }

    record_dispatch_counts(&span, &run_results, 0);
    PreToolUseResult {
        decision: HookDecision::Allow,
        results: run_results,
    }
}

/// Dispatch a non-blocking event (`session_start`, `post_tool_use`, `session_end`)
/// against all matching hooks.
///
/// Runs hooks sequentially, collects results. Never denies — callers log
/// results and continue.
pub async fn dispatch_non_blocking(
    registry: &HookRegistry,
    event: HookEventName,
    envelope: &HookEventEnvelope,
    ctx: &RunContext<'_>,
) -> Vec<HookRunResult> {
    let hooks = registry.hooks_for(event);
    if hooks.is_empty() {
        return Vec::new();
    }

    let span = tracing::info_span!(
        "hooks.dispatch",
        hook_event = %event,
        hook_count = hooks.len() as i64,
        num_success = tracing::field::Empty,
        num_failed = tracing::field::Empty,
        num_blocking = tracing::field::Empty,
        num_skipped = tracing::field::Empty,
        total_duration_ms = tracing::field::Empty,
    );
    let _enter = span.enter();

    let tool_name = extract_tool_name(envelope);
    let mut results = Vec::with_capacity(hooks.len());

    for spec in hooks {
        if !spec.enabled || crate::trust::is_hook_disabled(&spec.name) {
            tracing::info!(hook_name = %spec.name, "hook skipped (disabled)");
            results.push(HookRunResult::Skipped {
                hook_name: spec.name.clone(),
            });
            continue;
        }

        // Check matcher against tool name (only for tool events).
        if let Some(ref matcher) = spec.matcher
            && let Some(ref name) = tool_name
            && !matcher.is_match(name)
        {
            continue;
        }

        let _hook_span = tracing::info_span!(
            "hook.run",
            hook_name = %spec.name,
            hook_event = %event,
        )
        .entered();

        let (result, elapsed, http_info) = runner::run_hook(spec, envelope, ctx, false).await;

        match result {
            HookRunnerResult::Success => {
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "hook completed"
                );
                results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                });
            }
            HookRunnerResult::Failed(err) => {
                tracing::warn!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    error = %err,
                    "hook failed"
                );
                results.push(HookRunResult::Failed {
                    hook_name: spec.name.clone(),
                    error: err,
                    elapsed,
                    http_info,
                });
            }
            HookRunnerResult::Decision(_) => {
                // Shouldn't happen for non-blocking hooks.
                tracing::info!(
                    hook_name = %spec.name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "hook completed"
                );
                results.push(HookRunResult::Success {
                    hook_name: spec.name.clone(),
                    elapsed,
                    http_info,
                });
            }
        }
    }

    record_dispatch_counts(&span, &results, 0);

    results
}

/// Record hook outcome counts on the `hooks.dispatch` span. A blocking deny is
/// stored as a `Failed` result, so `num_blocking` is passed in and subtracted
/// from `num_failed` to avoid double-counting.
fn record_dispatch_counts(span: &tracing::Span, results: &[HookRunResult], num_blocking: i64) {
    let mut num_success = 0i64;
    let mut num_failed = 0i64;
    let mut num_skipped = 0i64;
    let mut total_duration_ms = 0i64;
    for r in results {
        match r {
            HookRunResult::Success { elapsed, .. } => {
                num_success += 1;
                total_duration_ms += elapsed.as_millis() as i64;
            }
            HookRunResult::Failed { elapsed, .. } => {
                num_failed += 1;
                total_duration_ms += elapsed.as_millis() as i64;
            }
            HookRunResult::Skipped { .. } => num_skipped += 1,
        }
    }
    span.record("num_success", num_success);
    span.record("num_failed", num_failed - num_blocking);
    span.record("num_blocking", num_blocking);
    span.record("num_skipped", num_skipped);
    span.record("total_duration_ms", total_duration_ms);
}

/// Build the hub custom hook `kind` string for a non-blocking hook event.
///
/// Returns `None` for `PreToolUse` (blocking, local-only). For all other
/// events the kind is `"hook.<snake_case_event_name>"`, derived from the
/// `Display` impl of `HookEventName`.
pub fn hub_hook_kind(event: HookEventName) -> Option<String> {
    if event.is_blocking() {
        return None;
    }
    Some(format!("hook.{event}"))
}

/// The tool name a matcher is tested against, or `None` for events with no tool
/// (lifecycle, prompt, compaction). `Notification` matches on its `notification_type`.
///
/// `tool_name` is the resolved underlying tool for meta-dispatch tools (`use_tool`
/// and the external MCP-call tool), so a matcher keyed on the real tool fires directly.
pub fn extract_tool_name(envelope: &HookEventEnvelope) -> Option<String> {
    use crate::event::HookPayload;
    match &envelope.payload {
        HookPayload::PreToolUse { tool_name, .. } => Some(tool_name.clone()),
        HookPayload::PostToolUse { tool_name, .. } => Some(tool_name.clone()),
        HookPayload::PostToolUseFailure { tool_name, .. } => Some(tool_name.clone()),
        HookPayload::PermissionDenied { tool_name, .. } => Some(tool_name.clone()),
        HookPayload::Notification {
            notification_type, ..
        } => Some(notification_type.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HookSpec;
    use crate::event::{HookEventEnvelope, HookEventName, HookPayload};
    use crate::matcher::HookMatcher;
    use std::collections::HashMap;
    use std::path::PathBuf;

    /// Helper: build a pre_tool_use envelope for the given tool name.
    fn pre_tool_use_envelope(tool_name: &str) -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: HookEventName::PreToolUse,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            payload: HookPayload::PreToolUse {
                tool_name: tool_name.into(),
                tool_use_id: "tu-1".into(),
                tool_input: serde_json::json!({"command": "ls"}),
                tool_input_truncated: false,
                permission_mode: None,
                subagent_type: None,
            },
        }
    }

    /// Helper: build a session_start envelope.
    fn session_start_envelope() -> HookEventEnvelope {
        HookEventEnvelope {
            hook_event_name: HookEventName::SessionStart,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            payload: HookPayload::SessionStart {
                source: "new".into(),
                model_id: None,
                agent_type: None,
            },
        }
    }

    fn run_ctx() -> RunContext<'static> {
        RunContext {
            session_id: "test-session",
            workspace_root: "/tmp",
        }
    }

    /// Helper: create a HookSpec pointing at `sh -c '<script>'` that prints
    /// the given JSON and exits with the given code.
    fn make_command_spec(
        name: &str,
        matcher: Option<&str>,
        enabled: bool,
        script: &str,
    ) -> HookSpec {
        HookSpec {
            name: name.into(),
            event: HookEventName::PreToolUse,
            handler_type: "command".into(),
            configured_matcher: matcher.map(|s| s.to_string()),
            matcher: matcher.map(|s| HookMatcher::new(s).unwrap()),
            enabled,
            command: Some(PathBuf::from(script)),
            command_raw: Some(script.to_string()),
            url: None,
            url_raw: None,
            timeout_ms: 5000,
            source_dir: PathBuf::from("/tmp"),
            extra_env: HashMap::new(),
        }
    }

    /// Build a registry from a list of specs using the public API.
    fn registry_from_specs(specs: Vec<HookSpec>) -> HookRegistry {
        let (mut registry, _) = crate::discovery::load_hooks(None, None);
        registry.append_specs(specs);
        registry
    }

    // ── extract_tool_name tests ──────────────────────────────────

    #[test]
    fn extract_tool_name_from_pre_tool_use() {
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        assert_eq!(
            extract_tool_name(&envelope),
            Some("run_terminal_cmd".into())
        );
    }

    #[test]
    fn extract_tool_name_from_session_start_is_none() {
        let envelope = session_start_envelope();
        assert_eq!(extract_tool_name(&envelope), None);
    }

    #[test]
    fn extract_tool_name_from_notification() {
        let envelope = HookEventEnvelope {
            hook_event_name: HookEventName::Notification,
            session_id: "s".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "t".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            payload: HookPayload::Notification {
                notification_type: "permission_prompt".into(),
                message: None,
                title: None,
                level: None,
            },
        };
        assert_eq!(
            extract_tool_name(&envelope),
            Some("permission_prompt".into())
        );
    }

    // ── dispatch_pre_tool_use tests ──────────────────────────────

    #[tokio::test]
    async fn empty_registry_allows() {
        let registry = registry_from_specs(vec![]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn single_allow_hook() {
        let spec = make_command_spec("allow-hook", None, true, "echo '{\"decision\":\"allow\"}'");
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn single_deny_hook() {
        let spec = make_command_spec(
            "deny-hook",
            None,
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"blocked\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny {
                ref reason,
                ref hook_name,
            } => {
                assert_eq!(reason, "blocked");
                assert_eq!(hook_name, "deny-hook");
            }
            ref other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_hook_is_skipped_allows() {
        // A deny hook that is disabled should be skipped entirely.
        let spec = make_command_spec(
            "disabled-deny",
            None,
            false, // disabled!
            "echo '{\"decision\":\"deny\",\"reason\":\"should not run\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn matcher_skips_non_matching_tool() {
        // Deny hook with matcher for "read_file" should not fire for "run_terminal_cmd".
        let spec = make_command_spec(
            "read-only-deny",
            Some("read_file"),
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"blocked\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn matcher_fires_on_matching_tool() {
        // Deny hook with matcher for "run_terminal_cmd" should fire.
        let spec = make_command_spec(
            "bash-deny",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"bash blocked\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny { ref reason, .. } => assert_eq!(reason, "bash blocked"),
            ref other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn first_deny_wins_short_circuits() {
        // Two hooks: first denies, second allows. First deny should win.
        let deny_spec = make_command_spec(
            "first-deny",
            None,
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"first says no\"}'; exit 2",
        );
        let allow_spec = make_command_spec(
            "second-allow",
            None,
            true,
            "echo '{\"decision\":\"allow\"}'",
        );
        let registry = registry_from_specs(vec![deny_spec, allow_spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny {
                ref reason,
                ref hook_name,
                ..
            } => {
                assert_eq!(reason, "first says no");
                assert_eq!(hook_name, "first-deny");
            }
            ref other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_then_deny_denies() {
        // First hook allows, second hook denies. The deny should win.
        // This is the key "stricter deny filter takes precedence" scenario.
        let allow_spec =
            make_command_spec("broad-allow", None, true, "echo '{\"decision\":\"allow\"}'");
        let deny_spec = make_command_spec(
            "strict-deny",
            None,
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"strict policy\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![allow_spec, deny_spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny {
                ref reason,
                ref hook_name,
                ..
            } => {
                assert_eq!(reason, "strict policy");
                assert_eq!(hook_name, "strict-deny");
            }
            ref other => panic!("expected Deny from strict filter, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_broad_deny_specific_tool_match() {
        // Broad allow hook (no matcher), specific deny hook for "run_terminal_cmd".
        // The deny should fire for matching tool even though allow came first.
        let allow_spec =
            make_command_spec("allow-all", None, true, "echo '{\"decision\":\"allow\"}'");
        let deny_spec = make_command_spec(
            "deny-bash",
            Some("run_terminal_cmd"),
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"bash not allowed\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![allow_spec, deny_spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny { ref reason, .. } => assert_eq!(reason, "bash not allowed"),
            ref other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn allow_broad_deny_specific_non_matching_allows() {
        // Broad allow hook, specific deny for "read_file" only.
        // Calling with "run_terminal_cmd" should allow (deny doesn't match).
        let allow_spec =
            make_command_spec("allow-all", None, true, "echo '{\"decision\":\"allow\"}'");
        let deny_spec = make_command_spec(
            "deny-read",
            Some("read_file"),
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"no read\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![allow_spec, deny_spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn fail_open_on_hook_crash() {
        // Hook exits with code 1 (crash). Under fail-open the tool call
        // should still be allowed; the failure is recorded for the UI.
        let spec = make_command_spec("crasher", None, true, "exit 1");
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(
            result.decision,
            HookDecision::Allow,
            "fail-open: a crashing hook must not block the tool call"
        );
        assert_eq!(result.results.len(), 1);
        assert!(
            matches!(&result.results[0], HookRunResult::Failed { hook_name, .. } if hook_name == "crasher"),
            "the failure must still appear in run_results for UI scrollback, got {:?}",
            result.results
        );
    }

    #[tokio::test]
    async fn fail_open_then_deny_lets_deny_win() {
        // First hook crashes (now fail-open), second denies. Under
        // fail-open the chain continues past the crash and the second
        // hook's explicit deny is what blocks the call.
        let crash_spec = make_command_spec("crasher", None, true, "exit 1");
        let deny_spec = make_command_spec(
            "denier",
            None,
            true,
            "echo '{\"decision\":\"deny\",\"reason\":\"nope\"}'; exit 2",
        );
        let registry = registry_from_specs(vec![crash_spec, deny_spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        match result.decision {
            HookDecision::Deny {
                ref hook_name,
                ref reason,
            } => {
                assert_eq!(hook_name, "denier");
                assert_eq!(reason, "nope");
            }
            ref other => panic!("expected Deny from explicit denier, got {other:?}"),
        }
        // Both hooks ran: the crasher recorded a Failed result, the
        // denier recorded a Failed result with "denied: nope" prefix.
        assert_eq!(result.results.len(), 2);
    }

    #[tokio::test]
    async fn all_hooks_allow_results_in_allow() {
        let specs = vec![
            make_command_spec("a1", None, true, "echo '{\"decision\":\"allow\"}'"),
            make_command_spec("a2", None, true, "echo '{\"decision\":\"allow\"}'"),
            make_command_spec("a3", None, true, "echo '{\"decision\":\"allow\"}'"),
        ];
        let registry = registry_from_specs(specs);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    #[tokio::test]
    async fn mixed_disabled_and_deny() {
        // Disabled deny hook followed by enabled allow. Should allow.
        let disabled_deny = make_command_spec(
            "disabled-deny",
            None,
            false,
            "echo '{\"decision\":\"deny\",\"reason\":\"should not run\"}'; exit 2",
        );
        let enabled_allow = make_command_spec(
            "enabled-allow",
            None,
            true,
            "echo '{\"decision\":\"allow\"}'",
        );
        let registry = registry_from_specs(vec![disabled_deny, enabled_allow]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(result.decision, HookDecision::Allow);
    }

    // ── fail-open regression tests ───────────────────────────────

    #[tokio::test]
    async fn fail_open_records_error_in_run_results() {
        // A hook that returns malformed output and exits non-zero now
        // results in Allow (fail-open) but the failure detail is still
        // captured in run_results for the UI scrollback.
        let spec = make_command_spec("bad-output", None, true, "echo 'not json'; exit 1");
        let registry = registry_from_specs(vec![spec]);
        let envelope = pre_tool_use_envelope("run_terminal_cmd");
        let result = dispatch_pre_tool_use(&registry, &envelope, &run_ctx()).await;
        assert_eq!(
            result.decision,
            HookDecision::Allow,
            "fail-open: bad output must not block the tool call"
        );
        assert_eq!(result.results.len(), 1);
        match &result.results[0] {
            HookRunResult::Failed {
                hook_name, error, ..
            } => {
                assert_eq!(hook_name, "bad-output");
                assert!(
                    error.contains("bad-output") || error.contains("exit code"),
                    "error detail should be preserved for UI: {error}"
                );
            }
            other => panic!("expected Failed run result, got {other:?}"),
        }
    }

    // ── dispatch_non_blocking tests ──────────────────────────────

    #[tokio::test]
    async fn non_blocking_empty_registry() {
        let registry = registry_from_specs(vec![]);
        let envelope = session_start_envelope();
        let results = dispatch_non_blocking(
            &registry,
            HookEventName::SessionStart,
            &envelope,
            &run_ctx(),
        )
        .await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn non_blocking_disabled_hook_skipped() {
        let mut spec = make_command_spec("disabled", None, false, "echo ok");
        spec.event = HookEventName::SessionStart;
        let registry = registry_from_specs(vec![spec]);
        let envelope = session_start_envelope();
        let results = dispatch_non_blocking(
            &registry,
            HookEventName::SessionStart,
            &envelope,
            &run_ctx(),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], HookRunResult::Skipped { .. }));
    }

    #[tokio::test]
    async fn non_blocking_success() {
        let mut spec = make_command_spec("starter", None, true, "echo ok");
        spec.event = HookEventName::SessionStart;
        let registry = registry_from_specs(vec![spec]);
        let envelope = session_start_envelope();
        let results = dispatch_non_blocking(
            &registry,
            HookEventName::SessionStart,
            &envelope,
            &run_ctx(),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], HookRunResult::Success { .. }));
    }

    #[tokio::test]
    async fn non_blocking_failure_does_not_stop_chain() {
        let mut spec1 = make_command_spec("crasher", None, true, "exit 1");
        spec1.event = HookEventName::SessionStart;
        let mut spec2 = make_command_spec("ok", None, true, "echo ok");
        spec2.event = HookEventName::SessionStart;
        let registry = registry_from_specs(vec![spec1, spec2]);
        let envelope = session_start_envelope();
        let results = dispatch_non_blocking(
            &registry,
            HookEventName::SessionStart,
            &envelope,
            &run_ctx(),
        )
        .await;
        assert_eq!(results.len(), 2);
        assert!(matches!(results[0], HookRunResult::Failed { .. }));
        assert!(matches!(results[1], HookRunResult::Success { .. }));
    }

    // ── hub_hook_kind tests ──────────────────────────────────────

    #[test]
    fn hub_hook_kind_returns_none_for_pre_tool_use() {
        assert_eq!(hub_hook_kind(HookEventName::PreToolUse), None);
    }

    #[test]
    fn hub_hook_kind_maps_all_non_blocking_events() {
        let cases: &[(HookEventName, &str)] = &[
            (HookEventName::SessionStart, "hook.session_start"),
            (HookEventName::SessionEnd, "hook.session_end"),
            (HookEventName::Stop, "hook.stop"),
            (HookEventName::StopFailure, "hook.stop_failure"),
            (HookEventName::PostToolUse, "hook.post_tool_use"),
            (
                HookEventName::PostToolUseFailure,
                "hook.post_tool_use_failure",
            ),
            (HookEventName::PermissionDenied, "hook.permission_denied"),
            (HookEventName::UserPromptSubmit, "hook.user_prompt_submit"),
            (HookEventName::Notification, "hook.notification"),
            (HookEventName::SubagentStart, "hook.subagent_start"),
            (HookEventName::SubagentStop, "hook.subagent_stop"),
            (HookEventName::SubagentEnd, "hook.subagent_stop"),
            (HookEventName::PreCompact, "hook.pre_compact"),
            (HookEventName::PostCompact, "hook.post_compact"),
        ];

        // Exhaustive match — adding a new HookEventName variant causes a
        // compiler error here, forcing this test to be updated.
        let total_variants = |e: HookEventName| -> usize {
            match e {
                HookEventName::SessionStart
                | HookEventName::SessionEnd
                | HookEventName::Stop
                | HookEventName::StopFailure
                | HookEventName::PreToolUse
                | HookEventName::PostToolUse
                | HookEventName::PostToolUseFailure
                | HookEventName::PermissionDenied
                | HookEventName::UserPromptSubmit
                | HookEventName::Notification
                | HookEventName::SubagentStart
                | HookEventName::SubagentStop
                | HookEventName::SubagentEnd
                | HookEventName::PreCompact
                | HookEventName::PostCompact => 15,
            }
        };
        assert_eq!(
            cases.len() + 1, // +1 for PreToolUse (blocking, tested separately)
            total_variants(HookEventName::SessionStart),
            "update hub_hook_kind test when new HookEventName variants are added"
        );

        for (event, expected) in cases {
            let kind = hub_hook_kind(*event);
            assert_eq!(
                kind.as_deref(),
                Some(*expected),
                "hub_hook_kind wrong for {event:?}"
            );
        }
    }
}
