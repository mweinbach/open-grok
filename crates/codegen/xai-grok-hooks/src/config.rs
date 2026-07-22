use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::HookError;
use crate::event::HookEventName;
use crate::matcher::HookMatcher;

/// The parsed `hooks` object from a compatible JSON settings file.
///
/// Parsing is lenient: unrecognized event names are skipped (not errors) so a
/// `~/.claude/settings.json` with unsupported events still loads the rest.
#[derive(Debug)]
pub struct HooksMap {
    pub events: HashMap<HookEventName, Vec<MatcherGroup>>,
    pub skipped_events: Vec<String>,
}

impl HooksMap {
    pub fn from_value(value: serde_json::Value) -> Result<Self, String> {
        let raw_map: HashMap<String, serde_json::Value> =
            serde_json::from_value(value).map_err(|e| format!("invalid hooks structure: {e}"))?;

        let mut events: HashMap<HookEventName, Vec<MatcherGroup>> = HashMap::new();
        let mut skipped_events = Vec::new();

        for (key, val) in raw_map {
            let event_name: HookEventName =
                match serde_json::from_value(serde_json::Value::String(key.clone())) {
                    Ok(name) => name,
                    Err(_) => {
                        skipped_events.push(key);
                        continue;
                    }
                };

            let matcher_groups: Vec<MatcherGroup> = match serde_json::from_value(val) {
                Ok(groups) => groups,
                Err(e) => {
                    return Err(format!("invalid matcher groups for event '{key}': {e}"));
                }
            };

            // Aliases (e.g. `SubagentEnd`) can parse to one event, so merge
            // groups rather than insert, which would drop all but one.
            events.entry(event_name).or_default().extend(matcher_groups);
        }

        Ok(HooksMap {
            events,
            skipped_events,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct MatcherGroup {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<RawHandler>,
}

#[derive(Debug, Deserialize)]
pub struct RawHandler {
    #[serde(rename = "type")]
    pub handler_type: String,
    pub command: Option<String>,
    pub url: Option<String>,
    /// Seconds (converted to milliseconds internally).
    pub timeout: Option<u64>,
    /// Extra env vars for the hook process; merged into [`HookSpec::extra_env`]
    /// (see its rustdoc for precedence and reserved-key stripping).
    #[serde(default, deserialize_with = "deserialize_optional_string_map")]
    pub env: HashMap<String, String>,
}

/// Accepts `null`, an absent field, or a string map. Serde otherwise rejects an
/// explicit `"env": null` for a `HashMap` field even with `#[serde(default)]`;
/// treating `null` as "no env" matches user intent.
fn deserialize_optional_string_map<'de, D>(de: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<HashMap<String, String>> = serde::Deserialize::deserialize(de)?;
    Ok(opt.unwrap_or_default())
}

pub const DEFAULT_TIMEOUT_SECS: u64 = 5;

pub const DEFAULT_TIMEOUT_MS: u64 = DEFAULT_TIMEOUT_SECS * 1000;

/// Stop gates run real verification (builds, tests) and fail open on timeout, so
/// the short observe default would silently disable a ported stop policy.
pub const DEFAULT_STOP_GATE_TIMEOUT_SECS: u64 = 600;

pub const DEFAULT_STOP_GATE_TIMEOUT_MS: u64 = DEFAULT_STOP_GATE_TIMEOUT_SECS * 1000;

fn default_timeout_ms(event: crate::event::HookEventName) -> u64 {
    if event.traits().gate == crate::event::GateKind::Stop {
        DEFAULT_STOP_GATE_TIMEOUT_MS
    } else {
        DEFAULT_TIMEOUT_MS
    }
}

/// The validated handler kind. `RawHandler::handler_type` keeps the untrusted
/// string; parsing validates it into this so consumers dispatch exhaustively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandlerType {
    Command,
    Http,
}

impl HandlerType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Http => "http",
        }
    }
}

/// A validated hook specification, ready for use by the dispatcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    pub name: String,
    pub event: HookEventName,
    pub handler_type: HandlerType,
    /// Raw pattern as written, kept for `/hooks-list` display (the compiled form
    /// is [`matcher`](HookSpec::matcher)).
    pub configured_matcher: Option<String>,
    #[serde(skip)]
    pub matcher: Option<HookMatcher>,
    pub enabled: bool,
    /// Executable path (command handlers), post-expansion: parse-time-resolvable
    /// `$VAR` refs are substituted, unresolved/modifier forms (`${VAR:-x}`) kept
    /// for the runner's `sh -c` branch. Unlike [`url`](HookSpec::url), commands
    /// are NOT re-expanded at run time, so only `sh -c` sees mid-session env
    /// changes. Display via [`command_raw`](HookSpec::command_raw) so resolved
    /// secrets never leak.
    pub command: Option<PathBuf>,
    /// Pre-expansion source for `command`; use it for display so resolved `env`
    /// values (possibly secrets) never leak past the runner.
    pub command_raw: Option<String>,
    /// URL endpoint (http handlers), post-expanded like [`command`](HookSpec::command).
    /// The HTTP runner re-expands it at run time before SSRF validation, so plugin
    /// URLs referencing later-injected `extra_env` keys resolve: mid-session env
    /// changes take effect for URLs but not commands (deliberate asymmetry).
    /// Display via [`url_raw`](HookSpec::url_raw).
    pub url: Option<String>,
    /// Pre-expansion source for `url`, for display; see [`command_raw`](HookSpec::command_raw).
    pub url_raw: Option<String>,
    pub timeout_ms: u64,
    pub source_dir: PathBuf,
    /// Extra environment variables injected into the hook process.
    ///
    /// Sources, lowest to highest precedence:
    ///
    /// 1. The user-declared `env` map (populated by [`parse_hook_file`]).
    ///    Runner-reserved keys (`GROK_HOOK_EVENT`, `GROK_HOOK_NAME`,
    ///    `GROK_SESSION_ID`, `GROK_WORKSPACE_ROOT`, `CLAUDE_PROJECT_DIR`) are
    ///    stripped at load time with a tracing warning.
    /// 2. Plugin-injected vars merged by the plugin adapter
    ///    (`xai-grok-agent::plugins::hooks_adapter`): `GROK_PLUGIN_ROOT`,
    ///    `CLAUDE_PLUGIN_ROOT`, `GROK_PLUGIN_DATA`, `CLAUDE_PLUGIN_DATA`, which
    ///    override any user values for those keys.
    /// 3. Runner-injected vars applied at spawn time AFTER `extra_env`, so they
    ///    always win even if a reserved key leaks through the layers above. This
    ///    is a security property: the child must see authentic identity/event
    ///    signals, never spoofed values. See the regression test
    ///    `runner_injected_vars_override_extra_env_at_spawn` in
    ///    `tests/integration.rs`.
    ///
    /// Besides being passed to the child, this map is consulted by the load-time
    /// expansion of `command` and `url` (see [`crate::env_expand`]).
    pub extra_env: std::collections::HashMap<String, String>,
}

/// Parse hooks from a JSON value (e.g. from agent definition frontmatter).
///
/// `source_dir` resolves relative command paths: pass the agent definition's
/// directory or the workspace CWD.
pub fn parse_hooks_from_value_with_dir(
    hooks: &serde_json::Value,
    source_name: &str,
    source_dir: &Path,
) -> (Vec<HookSpec>, Vec<HookError>) {
    let wrapper = serde_json::json!({ "hooks": hooks });
    let (mut specs, errors) =
        parse_hook_file(&wrapper.to_string(), std::path::Path::new(source_name));
    for spec in &mut specs {
        spec.source_dir = source_dir.to_path_buf();
    }
    (specs, errors)
}

pub fn parse_hook_file(content: &str, file_path: &Path) -> (Vec<HookSpec>, Vec<HookError>) {
    let mut specs = Vec::new();
    let mut errors = Vec::new();

    let top_level: serde_json::Value = match serde_json::from_str(content) {
        Ok(v) => v,
        Err(e) => {
            errors.push(HookError::ParseFile {
                path: file_path.to_path_buf(),
                detail: e.to_string(),
            });
            return (specs, errors);
        }
    };

    let hooks_value = match top_level.get("hooks") {
        Some(v) => v.clone(),
        None => return (specs, errors),
    };

    let hooks_map: HooksMap = match HooksMap::from_value(hooks_value) {
        Ok(m) => m,
        Err(detail) => {
            errors.push(HookError::ParseFile {
                path: file_path.to_path_buf(),
                detail,
            });
            return (specs, errors);
        }
    };

    if !hooks_map.skipped_events.is_empty() {
        tracing::warn!(
            file = %file_path.display(),
            skipped = ?hooks_map.skipped_events,
            "hooks: skipped unrecognized event names (check for typos)"
        );
    }

    let source_dir = file_path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let file_stem = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // HashMap event order is nondeterministic, but dispatch is per-event so it
    // doesn't matter; within an event, source order is preserved.
    for (event, matcher_groups) in hooks_map.events {
        for (group_idx, group) in matcher_groups.into_iter().enumerate() {
            let matcher_pattern = group
                .matcher
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            // Events with an `Ignored` matcher policy keep the configured pattern
            // for display but never compile it, so the hook always fires.
            let matcher_ignored = matcher_pattern.is_some()
                && event.traits().matcher == crate::event::MatcherPolicy::Ignored;
            if matcher_ignored {
                tracing::warn!(
                    hook = %format!("{file_stem}:{event}[{group_idx}]"),
                    path = %file_path.display(),
                    "hooks: matcher on a {event} group is ignored (this event always fires)"
                );
            }

            let compiled_matcher = match matcher_pattern.as_ref().filter(|_| !matcher_ignored) {
                Some(pattern) => match HookMatcher::new(pattern) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        let name = format!("{file_stem}:{event}[{group_idx}]");
                        errors.push(HookError::InvalidMatcher {
                            name,
                            path: file_path.to_path_buf(),
                            source: e,
                        });
                        continue;
                    }
                },
                None => None,
            };

            for (hook_idx, handler) in group.hooks.into_iter().enumerate() {
                let name = format!("{file_stem}:{event}[{group_idx}].hooks[{hook_idx}]");

                // `matcher` is deliberately NOT env-expanded: `$` is the regex
                // end-of-line anchor, so `$VAR` substitution would corrupt it.

                let timeout_ms = handler
                    .timeout
                    .map(|secs| secs * 1000)
                    .unwrap_or(default_timeout_ms(event));

                let mut extra_env: HashMap<String, String> = handler.env;
                strip_reserved_env_keys(&mut extra_env, &name, file_path);

                let handler_type = match handler.handler_type.as_str() {
                    "command" => HandlerType::Command,
                    "http" => HandlerType::Http,
                    _ => {
                        errors.push(HookError::UnsupportedHandlerType {
                            name,
                            path: file_path.to_path_buf(),
                            handler_type: handler.handler_type,
                        });
                        continue;
                    }
                };

                // Expand `command`/`url` now (`extra_env` first, then process
                // env). Unset refs are preserved: command hooks defer to the
                // runner, and the HTTP runner re-expands before SSRF validation
                // in case `extra_env` was populated after parsing.
                let (command, command_raw, url, url_raw) = match handler_type {
                    HandlerType::Command => {
                        let Some(command) = handler.command else {
                            errors.push(HookError::InvalidConfig {
                                name,
                                path: file_path.to_path_buf(),
                                detail: "command handler requires a 'command' field".into(),
                            });
                            continue;
                        };
                        let expanded =
                            crate::env_expand::expand_env_vars_with_extra(&command, &extra_env);
                        (Some(PathBuf::from(expanded)), Some(command), None, None)
                    }
                    HandlerType::Http => {
                        let Some(url) = handler.url else {
                            errors.push(HookError::InvalidConfig {
                                name,
                                path: file_path.to_path_buf(),
                                detail: "http handler requires a 'url' field".into(),
                            });
                            continue;
                        };
                        let expanded =
                            crate::env_expand::expand_env_vars_with_extra(&url, &extra_env);
                        (None, None, Some(expanded), Some(url))
                    }
                };

                specs.push(HookSpec {
                    name,
                    event,
                    handler_type,
                    configured_matcher: matcher_pattern.clone(),
                    matcher: compiled_matcher.clone(),
                    enabled: true,
                    command,
                    command_raw,
                    url,
                    url_raw,
                    timeout_ms,
                    source_dir: source_dir.clone(),
                    extra_env,
                });
            }
        }
    }

    (specs, errors)
}

/// Strip user-supplied `env` entries that override runner-reserved keys.
///
/// Redundant with the spawn-time ordering in `runner/command.rs`, but stripping
/// here gives a clear "ignored" signal and covers load paths that bypass
/// `parse_hook_file`.
fn strip_reserved_env_keys(
    extra_env: &mut HashMap<String, String>,
    spec_name: &str,
    file_path: &Path,
) {
    for reserved in crate::runner::command::RUNNER_ALWAYS_SET_ENV {
        if extra_env.remove(*reserved).is_some() {
            tracing::warn!(
                hook = %spec_name,
                file = %file_path.display(),
                key = reserved,
                "hook env: ignoring user-supplied value for runner-reserved key (the runner-injected value always wins)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_env_var;

    #[test]
    fn parse_claude_format_single_hook() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "run_terminal_cmd",
                        "hooks": [
                            { "type": "command", "command": "bin/check.sh", "timeout": 2 }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/hooks/test.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 1);
        let s = &specs[0];
        assert_eq!(s.event, HookEventName::PreToolUse);
        assert!(s.matcher.is_some());
        assert!(s.enabled);
        assert_eq!(s.timeout_ms, 2000);
        assert_eq!(s.command, Some(PathBuf::from("bin/check.sh")));
    }

    #[test]
    fn parse_multiple_handlers_in_group() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            { "type": "command", "command": "a.sh" },
                            { "type": "command", "command": "b.sh" }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].command, Some(PathBuf::from("a.sh")));
        assert_eq!(specs[1].command, Some(PathBuf::from("b.sh")));
    }

    #[test]
    fn parse_empty_matcher_matches_all() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "a.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        assert!(specs[0].matcher.is_none());
    }

    #[test]
    fn parse_absent_matcher_matches_all() {
        let json = r#"{
            "hooks": {
                "SessionStart": [
                    { "hooks": [{ "type": "command", "command": "start.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        assert!(specs[0].matcher.is_none());
    }

    #[test]
    fn parse_default_timeout() {
        let json = r#"{
            "hooks": {
                "SessionEnd": [
                    { "hooks": [{ "type": "command", "command": "end.sh" }] }
                ],
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "verify.sh" }] }
                ],
                "SubagentStop": [
                    { "hooks": [{ "type": "command", "command": "sub.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        for spec in &specs {
            let expected = match spec.event {
                HookEventName::Stop | HookEventName::SubagentStop => DEFAULT_STOP_GATE_TIMEOUT_MS,
                _ => DEFAULT_TIMEOUT_MS,
            };
            assert_eq!(spec.timeout_ms, expected, "event {}", spec.event);
        }
    }

    #[test]
    fn session_start_matcher_compiles_and_tests_source() {
        let json = r#"{
            "hooks": {
                "SessionStart": [
                    { "matcher": "startup|resume", "hooks": [{ "type": "command", "command": "s.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 1);
        let matcher = specs[0].matcher.as_ref().expect("matcher compiles");
        assert!(matcher.is_match("startup"));
        assert!(!matcher.is_match("clear"));
    }

    #[test]
    fn alias_event_keys_merge_groups() {
        let json = r#"{
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "a.sh" }] }
                ],
                "stop": [
                    { "hooks": [{ "type": "command", "command": "b.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        assert_eq!(specs.len(), 2, "both groups must survive the key collision");
    }

    #[test]
    fn stop_matcher_ignored_subagent_stop_matcher_kept() {
        let json = r#"{
            "hooks": {
                "Stop": [
                    { "matcher": "*", "hooks": [{ "type": "command", "command": "s.sh" }] }
                ],
                "SubagentStop": [
                    { "matcher": "code-reviewer", "hooks": [{ "type": "command", "command": "r.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "no load errors expected: {errors:?}");
        assert_eq!(specs.len(), 2);

        let stop = specs
            .iter()
            .find(|s| s.command_raw.as_deref() == Some("s.sh"))
            .unwrap();
        assert!(stop.matcher.is_none(), "Stop matcher must not compile");
        assert_eq!(
            stop.configured_matcher.as_deref(),
            Some("*"),
            "the configured pattern stays visible for display"
        );

        let sub = specs
            .iter()
            .find(|s| s.command_raw.as_deref() == Some("r.sh"))
            .unwrap();
        assert!(
            sub.matcher
                .as_ref()
                .is_some_and(|m| m.is_match("code-reviewer")),
            "SubagentStop matcher must be compiled and match its agent type"
        );
    }

    #[test]
    fn reject_invalid_regex() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "matcher": "[invalid", "hooks": [{ "type": "command", "command": "c.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], HookError::InvalidMatcher { .. }));
    }

    #[test]
    fn reject_invalid_json() {
        let json = "this is not valid json {{{";
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], HookError::ParseFile { .. }));
    }

    #[test]
    fn reject_unsupported_handler_type() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "prompt", "command": "test" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            HookError::UnsupportedHandlerType { .. }
        ));
    }

    #[test]
    fn parse_http_handler_type() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "http", "url": "https://hooks.example.com/check" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].handler_type, HandlerType::Http);
        assert!(specs[0].command.is_none());
        assert_eq!(
            specs[0].url.as_deref(),
            Some("https://hooks.example.com/check")
        );
    }

    #[test]
    fn reject_http_handler_without_url() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "http" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], HookError::InvalidConfig { .. }));
    }

    #[test]
    fn source_dir_from_file_path() {
        let json =
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"x.sh"}]}]}}"#;
        let (specs, _) = parse_hook_file(json, Path::new("/home/user/.opengrok/hooks/safety.json"));
        assert_eq!(
            specs[0].source_dir,
            PathBuf::from("/home/user/.opengrok/hooks")
        );
    }

    #[test]
    fn empty_hooks_object() {
        let json = r#"{"hooks": {}}"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        assert!(specs.is_empty());
    }

    #[test]
    fn no_hooks_key() {
        let json = r#"{"theme": "dark"}"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        assert!(specs.is_empty());
    }

    #[test]
    fn realistic_claude_settings_file() {
        let json = r#"{
            "$schema": "https://json.schemastore.org/claude-code-settings.json",
            "permissions": {
                "allow": ["Bash(npm run build)", "Read(**/src/**)", "Edit(**/src/**)"],
                "deny": ["Bash(rm -rf *)"]
            },
            "model": "claude-sonnet-4-20250514",
            "apiKey": "sk-ant-REDACTED",
            "theme": "dark",
            "customInstructions": "Always use TypeScript",
            "mcpServers": {
                "memory": {
                    "command": "npx",
                    "args": ["-y", "@anthropic/mcp-memory"]
                }
            },
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {
                                "type": "command",
                                "command": ".claude/hooks/block-dangerous.sh",
                                "timeout": 10
                            }
                        ]
                    }
                ],
                "PostToolUse": [
                    {
                        "matcher": "Write|Edit",
                        "hooks": [
                            { "type": "command", "command": "bun run format || true" }
                        ]
                    }
                ]
            },
            "autoUpdates": true,
            "telemetry": { "enabled": false, "shareUsageData": false }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/home/user/.claude/settings.json"));
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert_eq!(specs.len(), 2);
        let has_pre = specs.iter().any(|s| s.event == HookEventName::PreToolUse);
        let has_post = specs.iter().any(|s| s.event == HookEventName::PostToolUse);
        assert!(has_pre, "expected PreToolUse hook");
        assert!(has_post, "expected PostToolUse hook");
    }

    #[test]
    fn claude_settings_with_unknown_hook_events_skipped_leniently() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "check.sh" }] }
                ],
                "PermissionRequest": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "perm.sh" }] }
                ],
                "TaskCreated": [
                    { "hooks": [{ "type": "command", "command": "task.sh" }] }
                ],
                "FileChanged": [
                    { "matcher": ".envrc", "hooks": [{ "type": "command", "command": "env.sh" }] }
                ],
                "WorktreeCreate": [
                    { "hooks": [{ "type": "command", "command": "wt.sh" }] }
                ],
                "PostToolUse": [
                    { "hooks": [{ "type": "command", "command": "post.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/settings.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 2);
        let has_pre = specs.iter().any(|s| s.event == HookEventName::PreToolUse);
        let has_post = specs.iter().any(|s| s.event == HookEventName::PostToolUse);
        assert!(has_pre, "expected PreToolUse hook");
        assert!(has_post, "expected PostToolUse hook");
    }

    /// A `command` referencing a process-env var must be expanded at load time,
    /// removing the dependence on the runtime `sh -c` heuristic for direct-exec
    /// paths with no other shell metachars.
    #[test]
    fn parse_hook_file_expands_env_var_in_command_from_process_env() {
        let key = "GROK_HOOKS_PARSE_TEST_CMD_PROC_ENV";
        with_env_var(key, Some("/usr/local"), || {
            let json = format!(
                r#"{{
                    "hooks": {{
                        "PreToolUse": [
                            {{ "hooks": [{{ "type": "command", "command": "${{{key}}}/check.sh" }}] }}
                        ]
                    }}
                }}"#
            );
            let (specs, errors) = parse_hook_file(&json, Path::new("/tmp/test.json"));
            assert!(errors.is_empty(), "unexpected errors: {errors:?}");
            assert_eq!(specs.len(), 1);
            assert_eq!(specs[0].command, Some(PathBuf::from("/usr/local/check.sh")));
            assert_eq!(
                specs[0].command_raw.as_deref(),
                Some(format!("${{{key}}}/check.sh").as_str())
            );
        });
    }

    /// An HTTP `url` referencing a process-env var must be substituted at load
    /// time so SSRF validation sees the resolved host.
    #[test]
    fn parse_hook_file_expands_env_var_in_url_from_process_env() {
        let key = "GROK_HOOKS_PARSE_TEST_URL_PROC_ENV";
        with_env_var(key, Some("hooks.example.com"), || {
            let json = format!(
                r#"{{
                    "hooks": {{
                        "PreToolUse": [
                            {{ "hooks": [{{ "type": "http", "url": "https://${{{key}}}/check" }}] }}
                        ]
                    }}
                }}"#
            );
            let (specs, errors) = parse_hook_file(&json, Path::new("/tmp/test.json"));
            assert!(errors.is_empty(), "unexpected errors: {errors:?}");
            assert_eq!(specs.len(), 1);
            assert_eq!(
                specs[0].url.as_deref(),
                Some("https://hooks.example.com/check")
            );
            assert_eq!(
                specs[0].url_raw.as_deref(),
                Some(format!("https://${{{key}}}/check").as_str())
            );
        });
    }

    /// A declared `env` map is injected into the process via
    /// `HookSpec::extra_env`.
    #[test]
    fn parse_hook_file_env_map_populates_extra_env() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "echo hi",
                                "env": { "FOO": "bar", "BAZ": "qux" }
                            }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].extra_env.len(), 2);
        assert_eq!(
            specs[0].extra_env.get("FOO").map(String::as_str),
            Some("bar")
        );
        assert_eq!(
            specs[0].extra_env.get("BAZ").map(String::as_str),
            Some("qux")
        );
    }

    /// An `env` map value for a var referenced in `command` must win over the
    /// process env when expanding at load time.
    #[test]
    fn parse_hook_file_env_map_feeds_command_expansion() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "${MY_HOOK_ROOT}/check.sh",
                                "env": { "MY_HOOK_ROOT": "/from/env-map" }
                            }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0].command,
            Some(PathBuf::from("/from/env-map/check.sh"))
        );
        assert_eq!(specs[0].extra_env.len(), 1);
        assert_eq!(
            specs[0].extra_env.get("MY_HOOK_ROOT").map(String::as_str),
            Some("/from/env-map")
        );
    }

    /// A `command` referencing a var unset at load time must preserve the
    /// literal `${VAR}`, so the runner's pre-flight check stays the single
    /// source of truth for run-time resolvability.
    #[test]
    fn parse_hook_file_preserves_unresolved_env_refs_in_command() {
        let key = "GROK_HOOKS_PARSE_TEST_NEVER_SET_AT_LOAD_TIME";
        with_env_var(key, None, || {
            let json = format!(
                r#"{{
                    "hooks": {{
                        "PreToolUse": [
                            {{ "hooks": [{{ "type": "command", "command": "${{{key}}}/x.sh" }}] }}
                        ]
                    }}
                }}"#
            );
            let (specs, errors) = parse_hook_file(&json, Path::new("/tmp/test.json"));
            assert!(errors.is_empty(), "unexpected errors: {errors:?}");
            assert_eq!(specs.len(), 1);
            let cmd = specs[0]
                .command
                .as_ref()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            assert_eq!(cmd, format!("${{{key}}}/x.sh"));
        });
    }

    /// Symmetry: load-time expansion of `url` must also preserve unset
    /// refs, otherwise a deferred plugin var would be silently stripped.
    #[test]
    fn parse_hook_file_preserves_unresolved_env_refs_in_url() {
        let key = "GROK_HOOKS_PARSE_TEST_URL_NEVER_SET_AT_LOAD_TIME";
        with_env_var(key, None, || {
            let json = format!(
                r#"{{
                    "hooks": {{
                        "PreToolUse": [
                            {{ "hooks": [{{ "type": "http", "url": "https://${{{key}}}/check" }}] }}
                        ]
                    }}
                }}"#
            );
            let (specs, errors) = parse_hook_file(&json, Path::new("/tmp/test.json"));
            assert!(errors.is_empty(), "unexpected errors: {errors:?}");
            assert_eq!(specs.len(), 1);
            let url = specs[0].url.as_deref().unwrap_or("");
            assert_eq!(url, format!("https://${{{key}}}/check"));
        });
    }

    /// Explicit `"env": null` is tolerated and yields an empty `extra_env` map,
    /// rather than serde's default failure mode.
    #[test]
    fn parse_hook_file_env_null_treated_as_empty() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "hooks": [
                            { "type": "command", "command": "echo hi", "env": null }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 1);
        assert!(specs[0].extra_env.is_empty());
    }

    /// Env values are stored verbatim: references inside them (e.g. `"${HOME}/x"`)
    /// are NOT recursively expanded. The env map is plumbing, not a template layer.
    #[test]
    fn parse_hook_file_env_values_are_stored_verbatim() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "echo hi",
                                "env": { "BAR": "${HOME}/x" }
                            }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 1);
        assert_eq!(
            specs[0].extra_env.get("BAR").map(String::as_str),
            Some("${HOME}/x"),
            "env values must be stored verbatim, not recursively expanded"
        );
    }

    #[test]
    fn parse_hook_file_matcher_is_not_env_expanded() {
        let key = "GROK_HOOKS_PARSE_TEST_MATCHER_VAR";
        with_env_var(key, Some("expanded_value_should_not_appear"), || {
            let pattern = format!("foo{key}");
            let json = serde_json::json!({
                "hooks": {
                    "PreToolUse": [
                        {
                            "matcher": pattern,
                            "hooks": [
                                { "type": "command", "command": "echo hi" }
                            ]
                        }
                    ]
                }
            });
            let (specs, errors) = parse_hook_file(&json.to_string(), Path::new("/tmp/test.json"));
            assert!(errors.is_empty(), "unexpected errors: {errors:?}");
            assert_eq!(specs.len(), 1);
            assert_eq!(
                specs[0].configured_matcher.as_deref(),
                Some(pattern.as_str())
            );
            let stored = specs[0].configured_matcher.as_deref().unwrap_or("");
            assert!(
                !stored.contains("expanded_value_should_not_appear"),
                "matcher must NOT be env-expanded, got {stored:?}"
            );
        });
    }

    /// A non-string `env` value (e.g. `"PORT": 8080`) fails deserialization; the
    /// whole file surfaces a `ParseFile` error rather than silently dropping it.
    /// Users who need numeric values must quote them (`"PORT": "8080"`).
    #[test]
    fn parse_hook_file_env_value_must_be_string() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "echo hi",
                                "env": { "PORT": 8080 }
                            }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(
            specs.is_empty(),
            "expected non-string env value to fail parsing"
        );
        assert!(
            !errors.is_empty(),
            "expected an error for non-string env value, got none"
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, HookError::ParseFile { .. })),
            "expected at least one HookError::ParseFile, got {errors:?}"
        );
    }

    /// User attempts to set runner-reserved keys via the `env` map are stripped
    /// at load time, giving a clear "ignored" signal on top of spawn-time override.
    #[test]
    fn parse_hook_file_strips_runner_reserved_env_keys() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "echo hi",
                                "env": {
                                    "GROK_HOOK_EVENT": "spoofed",
                                    "GROK_HOOK_NAME": "spoofed",
                                    "GROK_SESSION_ID": "spoofed",
                                    "GROK_WORKSPACE_ROOT": "/etc",
                                    "CLAUDE_PROJECT_DIR": "/etc",
                                    "USER_KEY": "kept"
                                }
                            }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 1);
        for reserved in [
            "GROK_HOOK_EVENT",
            "GROK_HOOK_NAME",
            "GROK_SESSION_ID",
            "GROK_WORKSPACE_ROOT",
            "CLAUDE_PROJECT_DIR",
        ] {
            assert!(
                !specs[0].extra_env.contains_key(reserved),
                "reserved key {reserved} must be stripped, got {:?}",
                specs[0].extra_env
            );
        }
        assert_eq!(
            specs[0].extra_env.get("USER_KEY").map(String::as_str),
            Some("kept")
        );
        assert_eq!(specs[0].extra_env.len(), 1);
    }
}
