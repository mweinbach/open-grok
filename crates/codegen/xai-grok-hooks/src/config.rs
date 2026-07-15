use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::HookError;
use crate::event::HookEventName;
use crate::matcher::HookMatcher;

/// The parsed `hooks` object from a compatible JSON settings file.
///
/// Parsing is lenient: unrecognized event names are skipped (not errors)
/// so that `~/.claude/settings.json` files with unsupported events don't
/// break loading of the events we do support.
#[derive(Debug)]
pub struct HooksMap {
    pub events: HashMap<HookEventName, Vec<MatcherGroup>>,
    /// Event names present in the JSON but not recognized by Grok.
    pub skipped_events: Vec<String>,
}

impl HooksMap {
    /// Parse a `hooks` JSON value. Unrecognized event names are skipped.
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

            events.insert(event_name, matcher_groups);
        }

        Ok(HooksMap {
            events,
            skipped_events,
        })
    }
}

/// A matcher group: an optional matcher pattern and one or more hook handlers.
#[derive(Debug, Deserialize)]
pub struct MatcherGroup {
    /// Regex pattern to filter tool names (e.g. `"Bash"`, `"Edit|Write"`).
    /// Empty string or absent means match all.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Array of hook handlers to run when matched.
    pub hooks: Vec<RawHandler>,
}

/// A single hook handler entry in the JSON format.
#[derive(Debug, Deserialize)]
pub struct RawHandler {
    /// Handler type: `"command"` or `"http"`.
    #[serde(rename = "type")]
    pub handler_type: String,
    /// Path to the executable script/binary (for `"command"` handlers).
    pub command: Option<String>,
    /// URL endpoint (for `"http"` handlers).
    pub url: Option<String>,
    /// Timeout in seconds (settings-file format). Converted to milliseconds internally.
    pub timeout: Option<u64>,
    /// Optional extra environment variables to inject into the hook process.
    /// Compatible with common agent settings. These are merged into [`HookSpec::extra_env`] and
    /// also feed the load-time env-var expansion of `command` and `url`.
    /// Plugin-injected vars (set by the plugin adapter) override these for
    /// the keys the plugin owns (e.g. `CLAUDE_PLUGIN_ROOT`); see the rustdoc
    /// on [`HookSpec::extra_env`]. User attempts to set runner-reserved
    /// keys (`GROK_HOOK_EVENT`, `GROK_HOOK_NAME`, `GROK_SESSION_ID`,
    /// `GROK_WORKSPACE_ROOT`, `CLAUDE_PROJECT_DIR`) are stripped at load
    /// time and a warning is logged.
    ///
    /// `serde(default)` so that omitting `env` from the JSON gives an
    /// empty map (no extra env). `null` is also tolerated and yields
    /// the same empty map (see `parse_hook_file_env_null_treated_as_empty`).
    /// JSON values that aren't strings (e.g. `"PORT": 8080`) trigger a
    /// serde error -- see `parse_hook_file_env_value_must_be_string`
    /// for the documented failure.
    #[serde(default, deserialize_with = "deserialize_optional_string_map")]
    pub env: HashMap<String, String>,
}

/// Custom deserializer that accepts `null`, an absent field, or a
/// string-keyed map of string values. Used for `RawHandler::env`.
///
/// Without this, `serde` rejects an explicit `"env": null` JSON value
/// for a `HashMap<String, String>` field even with `#[serde(default)]`.
/// Treating `null` as "no env" matches the user's likely intent.
fn deserialize_optional_string_map<'de, D>(de: D) -> Result<HashMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<HashMap<String, String>> = serde::Deserialize::deserialize(de)?;
    Ok(opt.unwrap_or_default())
}

/// Default timeout in seconds when not specified.
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;

/// Default timeout in milliseconds (derived from DEFAULT_TIMEOUT_SECS).
pub const DEFAULT_TIMEOUT_MS: u64 = DEFAULT_TIMEOUT_SECS * 1000;

/// A validated hook specification, ready for use by the dispatcher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookSpec {
    pub name: String,
    pub event: HookEventName,
    /// Handler type: `"command"` or `"http"`.
    pub handler_type: String,
    /// The configured matcher pattern as written in the JSON file (e.g. `"Bash"`).
    /// Used for display in `/hooks-list`. Separate from the compiled matcher
    /// (which applies compat alias expansion and matching).
    pub configured_matcher: Option<String>,
    /// The compiled matcher (exact for simple patterns, alias-expanded,
    /// unanchored regex otherwise).
    #[serde(skip)]
    pub matcher: Option<HookMatcher>,
    pub enabled: bool,
    /// Path to the executable (for `"command"` handlers). `None` for other types.
    ///
    /// **Post-expansion form.** `${VAR}` / `$VAR` references that were
    /// resolvable at parse time have been substituted via
    /// [`crate::env_expand::expand_env_vars_with_extra`] using the
    /// user-supplied `env` map plus the process environment. Unresolved
    /// references and parameter-expansion-modifier forms (`${VAR:-x}`,
    /// `${VAR%pat}`, etc.) are preserved verbatim and resolved at run
    /// time by the runner's `sh -c` branch.
    ///
    /// **Asymmetry vs `url`.** Command paths are NOT re-expanded at run
    /// time: the runtime `sh -c` branch picks up mid-session env
    /// changes for commands containing shell metacharacters, but
    /// direct-exec paths see only the parse-time snapshot. URLs ARE
    /// re-expanded at runtime by the HTTP runner (see [`url`]).
    ///
    /// **Source preservation.** Use [`command_raw`] for display so the
    /// pager UI / ACP DTO never leaks resolved secret values from the
    /// `env` map into log files or the modal.
    ///
    /// [`url`]: HookSpec::url
    /// [`command_raw`]: HookSpec::command_raw
    pub command: Option<PathBuf>,
    /// Pre-expansion source string for `command`, exactly as written in
    /// the JSON file. `None` for non-command handlers and for hooks
    /// loaded by older code paths that pre-date the raw-source field.
    /// Use this in any display surface (pager UI, ACP DTO, tracing
    /// logs) so resolved env-var values from the user `env` map -- some
    /// of which may be secrets -- never leak past the runner.
    pub command_raw: Option<String>,
    /// URL endpoint (for `"http"` handlers). `None` for other types.
    ///
    /// **Post-expansion form** at parse time, with the same semantics
    /// as [`command`]. The HTTP runner additionally re-expands this
    /// field at run time before SSRF validation, so plugin URLs that
    /// reference `extra_env` keys injected after parsing (e.g.
    /// `${CLAUDE_PLUGIN_ROOT}/check`) resolve correctly. This means
    /// mid-session changes to process env DO take effect for URLs but
    /// NOT for commands -- a deliberate asymmetry; document any user
    /// expectation accordingly.
    ///
    /// **Source preservation.** Use [`url_raw`] for display.
    ///
    /// [`command`]: HookSpec::command
    /// [`url_raw`]: HookSpec::url_raw
    pub url: Option<String>,
    /// Pre-expansion source string for `url`, exactly as written in the
    /// JSON file. `None` for non-HTTP handlers. See [`command_raw`].
    ///
    /// [`command_raw`]: HookSpec::command_raw
    pub url_raw: Option<String>,
    pub timeout_ms: u64,
    /// The directory containing the JSON file that defined this hook.
    /// Used for resolving relative command paths.
    pub source_dir: PathBuf,
    /// Extra environment variables injected into the hook process.
    ///
    /// Sources, listed lowest to highest precedence:
    ///
    /// 1. The user-declared `env` map on the JSON `RawHandler` (populated
    ///    by [`parse_hook_file`]). Runner-reserved keys
    ///    (`GROK_HOOK_EVENT`, `GROK_HOOK_NAME`, `GROK_SESSION_ID`,
    ///    `GROK_WORKSPACE_ROOT`, `CLAUDE_PROJECT_DIR`) are stripped at
    ///    load time and a tracing warning is emitted.
    /// 2. Plugin-injected vars merged in by the plugin adapter
    ///    (`xai-grok-agent::plugins::hooks_adapter`). The adapter sets
    ///    `GROK_PLUGIN_ROOT`, `CLAUDE_PLUGIN_ROOT`, `GROK_PLUGIN_DATA`,
    ///    and `CLAUDE_PLUGIN_DATA`; the merge overrides any user
    ///    values for those four keys so the plugin contract is intact.
    /// 3. Runner-injected vars at spawn time
    ///    (`GROK_HOOK_EVENT`, `GROK_HOOK_NAME`, `GROK_SESSION_ID`,
    ///    `GROK_WORKSPACE_ROOT`, `CLAUDE_PROJECT_DIR`). These are
    ///    applied AFTER `extra_env` in the spawn call so they always
    ///    win, even if the layered defenses above leak a reserved key
    ///    through. This is a security property: the spawned child
    ///    must always see authentic identity/event signals, never
    ///    user-controlled spoofed values. See the regression test
    ///    `runner_injected_vars_override_extra_env_at_spawn` in
    ///    `tests/integration.rs`.
    ///
    /// In addition to being passed to the spawned child process, this map
    /// is consulted by the load-time `${VAR}` / `$VAR` expansion of
    /// `command` and `url` (see [`crate::env_expand`]).
    pub extra_env: std::collections::HashMap<String, String>,
}

/// Parse and validate a hook file from its JSON content.
///
/// Accepts any JSON file (settings file, dedicated hook file, etc.).
/// Extracts only the `hooks` key from the top level. All other keys are
/// ignored, so this works with settings files that contain
/// theme, model, permission, and other unrelated configuration.
///
/// Returns the list of validated hook specs and any non-fatal errors
/// (invalid entries are skipped with errors collected).
/// Parse hooks from a JSON value (e.g. from agent definition frontmatter).
///
/// `source_dir` is used to resolve relative command paths in hook specs.
/// Pass the agent definition's directory or the workspace CWD.
pub fn parse_hooks_from_value(
    hooks: &serde_json::Value,
    source_name: &str,
) -> (Vec<HookSpec>, Vec<HookError>) {
    parse_hooks_from_value_with_dir(hooks, source_name, std::path::Path::new("."))
}

/// Like `parse_hooks_from_value` but with an explicit `source_dir` for
/// resolving relative command paths.
pub fn parse_hooks_from_value_with_dir(
    hooks: &serde_json::Value,
    source_name: &str,
    source_dir: &Path,
) -> (Vec<HookSpec>, Vec<HookError>) {
    let wrapper = serde_json::json!({ "hooks": hooks });
    let (mut specs, errors) =
        parse_hook_file(&wrapper.to_string(), std::path::Path::new(source_name));
    // Override the source_dir (which parse_hook_file derived from the fake
    // source_name path) with the real directory.
    for spec in &mut specs {
        spec.source_dir = source_dir.to_path_buf();
    }
    (specs, errors)
}

pub fn parse_hook_file(content: &str, file_path: &Path) -> (Vec<HookSpec>, Vec<HookError>) {
    let mut specs = Vec::new();
    let mut errors = Vec::new();

    // Step 1: parse the full file as a generic JSON value.
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

    // Step 2: extract only the "hooks" key. If absent, the file has no hooks.
    let hooks_value = match top_level.get("hooks") {
        Some(v) => v.clone(),
        None => return (specs, errors), // No hooks key — not an error, just no hooks.
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

    // Iterate events. HashMap order is nondeterministic, but dispatch is
    // per-event so cross-event order within a file does not affect behavior.
    // Within each event, matcher groups and handlers preserve source order.
    for (event, matcher_groups) in hooks_map.events {
        for (group_idx, group) in matcher_groups.into_iter().enumerate() {
            // Normalize empty matcher string to None (match all).
            let matcher_pattern = group
                .matcher
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());

            // Reject lifecycle hooks with matchers.
            if event.is_lifecycle() && matcher_pattern.is_some() {
                let name = format!("{file_stem}:{event}[{group_idx}]");
                errors.push(HookError::LifecycleMatcherNotAllowed {
                    name,
                    path: file_path.to_path_buf(),
                    event: event.to_string(),
                });
                continue;
            }

            // Compile matcher regex if present.
            let compiled_matcher = match &matcher_pattern {
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

                // Fields that are deliberately NOT env-expanded:
                //
                // * `timeout` is a JSON number, so there is nothing to
                //   expand -- it's already a `u64` by this point.
                // * `matcher` is a regex; `$` is the regex anchor for
                //   end-of-line. Substituting `$VAR` here would
                //   silently change the regex's semantics (and likely
                //   produce an invalid pattern). Users who need
                //   dynamic matchers should generate their JSON file
                //   at write time. The `compiled_matcher` above is
                //   already validated; we clone it per-handler below
                //   (`HookMatcher` derives `Clone`).
                //
                // The `command` and `url` fields ARE env-expanded; see
                // the load-time pass in each handler-type branch
                // below.

                // Compatible settings format uses seconds; convert to milliseconds.
                let timeout_ms = handler
                    .timeout
                    .map(|secs| secs * 1000)
                    .unwrap_or(DEFAULT_TIMEOUT_MS);

                // Strip user attempts to override runner-reserved keys
                // and emit a tracing warning per stripped key. The
                // spawn-time precedence ordering in
                // `runner/command.rs` already overrides these keys
                // unconditionally, but stripping them at load time
                // gives users a clear "ignored" signal instead of
                // silent override.
                let mut extra_env: HashMap<String, String> = handler.env;
                strip_reserved_env_keys(&mut extra_env, &name, file_path);

                match handler.handler_type.as_str() {
                    "command" => {
                        let Some(command) = handler.command else {
                            errors.push(HookError::InvalidConfig {
                                name,
                                path: file_path.to_path_buf(),
                                detail: "command handler requires a 'command' field".into(),
                            });
                            continue;
                        };
                        // Env-expand `command` at config-load time using the
                        // hook's own `extra_env` first, then process env. This
                        // makes direct-exec command paths that use `$VAR` /
                        // `${VAR}` references work without depending on the
                        // runtime `sh -c` heuristic in the runner. Unset
                        // refs (e.g. `${SOMETHING_SET_AT_RUN_TIME}`) are
                        // preserved verbatim and handled by the runner's
                        // pre-flight check (see `crate::runner::command`).
                        let expanded_command =
                            crate::env_expand::expand_env_vars_with_extra(&command, &extra_env);
                        specs.push(HookSpec {
                            name,
                            event,
                            handler_type: "command".into(),
                            configured_matcher: matcher_pattern.clone(),
                            matcher: compiled_matcher.clone(),
                            enabled: true,
                            command: Some(PathBuf::from(expanded_command)),
                            command_raw: Some(command),
                            url: None,
                            url_raw: None,
                            timeout_ms,
                            source_dir: source_dir.clone(),
                            extra_env,
                        });
                    }
                    "http" => {
                        let Some(url) = handler.url else {
                            errors.push(HookError::InvalidConfig {
                                name,
                                path: file_path.to_path_buf(),
                                detail: "http handler requires a 'url' field".into(),
                            });
                            continue;
                        };
                        // Env-expand `url` at config-load time. Unset refs
                        // are preserved; the HTTP runner re-runs expansion
                        // immediately before SSRF validation in case
                        // `extra_env` was populated after parsing (e.g. by
                        // the plugin adapter).
                        let expanded_url =
                            crate::env_expand::expand_env_vars_with_extra(&url, &extra_env);
                        specs.push(HookSpec {
                            name,
                            event,
                            handler_type: "http".into(),
                            configured_matcher: matcher_pattern.clone(),
                            matcher: compiled_matcher.clone(),
                            enabled: true,
                            command: None,
                            command_raw: None,
                            url: Some(expanded_url),
                            url_raw: Some(url),
                            timeout_ms,
                            source_dir: source_dir.clone(),
                            extra_env,
                        });
                    }
                    _ => {
                        errors.push(HookError::UnsupportedHandlerType {
                            name,
                            path: file_path.to_path_buf(),
                            handler_type: handler.handler_type,
                        });
                        continue;
                    }
                }
            }
        }
    }

    (specs, errors)
}

/// Strip user-supplied `env` map entries that try to override
/// runner-reserved keys, emitting a tracing warning per stripped key.
///
/// Belt-and-suspenders for the spawn-time precedence ordering: even if
/// somebody slips a reserved key past this strip (e.g. via an alternate
/// load path that bypasses `parse_hook_file`), the spawn-time order in
/// `runner/command.rs` still ensures the runner's value wins. Stripping
/// here gives users a clear "you tried to set a reserved key, ignored"
/// signal.
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
        assert_eq!(s.timeout_ms, 2000); // 2 seconds → 2000 ms
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
        assert!(specs[0].matcher.is_none()); // empty string → None → match all
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
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty());
        assert_eq!(specs[0].timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn reject_lifecycle_hook_with_matcher() {
        let json = r#"{
            "hooks": {
                "SessionStart": [
                    { "matcher": "something", "hooks": [{ "type": "command", "command": "s.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(specs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(
            &errors[0],
            HookError::LifecycleMatcherNotAllowed { .. }
        ));
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
        assert_eq!(specs[0].handler_type, "http");
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
        let (specs, _) = parse_hook_file(json, Path::new("/home/user/.grok/hooks/safety.json"));
        assert_eq!(specs[0].source_dir, PathBuf::from("/home/user/.grok/hooks"));
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
        // A realistic settings.json with many unrelated keys and
        // deeply nested non-hook structures.
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
        // Both events should be present regardless of HashMap order.
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

    #[test]
    fn lenient_parsing_skips_all_unknown_events() {
        let json = r#"{
            "hooks": {
                "PermissionRequest": [
                    { "hooks": [{ "type": "command", "command": "perm.sh" }] }
                ],
                "ConfigChange": [
                    { "hooks": [{ "type": "command", "command": "config.sh" }] }
                ],
                "WorktreeCreate": [
                    { "hooks": [{ "type": "command", "command": "wt.sh" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/settings.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(specs.is_empty(), "expected no specs from unknown events");
    }

    /// Regression: a JSON hook whose `command` references an env var that
    /// IS set in the process environment must be expanded at config-load
    /// time. This removes the dependence on the runtime `sh -c` heuristic
    /// for direct-exec command paths that have no other shell metachars.
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
            // The raw form must preserve the original reference so the
            // pager UI / ACP DTO surface the source string.
            assert_eq!(
                specs[0].command_raw.as_deref(),
                Some(format!("${{{key}}}/check.sh").as_str())
            );
        });
    }

    /// Regression: a JSON HTTP hook whose `url` references an env var that
    /// IS set in the process environment must have the var substituted at
    /// config-load time so SSRF validation sees the resolved host.
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
            // url_raw preserves the source.
            assert_eq!(
                specs[0].url_raw.as_deref(),
                Some(format!("https://${{{key}}}/check").as_str())
            );
        });
    }

    /// Regression: a JSON hook may declare an `env` map that gets injected
    /// into the spawned process via `HookSpec::extra_env`. This is the
    /// compatible-settings feature for non-plugin hooks.
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
        // Lock down exact map size so a regression that
        // accidentally injects extra keys would fail.
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

    /// Regression: a JSON hook whose `env` map provides a value for a var
    /// referenced in `command` must use that value (not the process env)
    /// when expanding the command at load time. This proves that the
    /// per-hook `env` map feeds back into load-time expansion.
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
        // Lock down exact map size.
        assert_eq!(specs[0].extra_env.len(), 1);
        assert_eq!(
            specs[0].extra_env.get("MY_HOOK_ROOT").map(String::as_str),
            Some("/from/env-map")
        );
    }

    /// Regression: a JSON hook whose `command` references a var that is
    /// NOT set anywhere at config-load time must preserve the literal
    /// `${VAR}` text. The runner's pre-flight check is the single source
    /// of truth for "is this resolvable at run time?". Load-time
    /// expansion must therefore be idempotent (a no-op on already
    /// expanded strings) so that the runtime check is never bypassed.
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
            // Lock down both halves with assert_eq! so a
            // regression that strips the trailing `/x.sh` would also
            // be caught.
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

    /// Default for `extra_env` is an empty map when the JSON has no `env`.
    /// Guarantees we don't accidentally populate keys.
    #[test]
    fn parse_hook_file_extra_env_defaults_empty() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    { "hooks": [{ "type": "command", "command": "echo hi" }] }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert_eq!(specs.len(), 1);
        assert!(specs[0].extra_env.is_empty());
    }

    /// Explicit `"env": null` must be tolerated and yield an
    /// empty extra_env map -- documented behaviour rather than serde's
    /// default failure mode.
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

    /// Env values are stored verbatim; references inside them
    /// (e.g. `"${HOME}/x"`) are NOT recursively expanded. This documents
    /// the contract -- the env map is plumbing, not a templating layer.
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

    /// `matcher` is intentionally NOT env-expanded. A
    /// matcher with `$VAR` must store the literal `$VAR` (anchored as
    /// part of the regex by `HookMatcher::new`). A future contributor
    /// adding "completeness" here would break regex semantics.
    #[test]
    fn parse_hook_file_matcher_is_not_env_expanded() {
        let key = "GROK_HOOKS_PARSE_TEST_MATCHER_VAR";
        with_env_var(key, Some("expanded_value_should_not_appear"), || {
            // Use a regex-valid matcher pattern that also embeds `$KEY`.
            // We deliberately use a pattern that's a valid regex even
            // without expansion (`$` in regex is the end-of-line
            // anchor, so `^foo$KEY$` is a valid pattern that matches
            // literally nothing but parses).
            let pattern = format!("foo{key}");
            // Wrap in a JSON-safe regex: the value `foo$VARNAME` is a
            // valid regex (the `$` anchors before `V` -- literal char
            // class). We just want to prove the stored value contains
            // no expansion.
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
            // configured_matcher stores the source string verbatim.
            assert_eq!(
                specs[0].configured_matcher.as_deref(),
                Some(pattern.as_str())
            );
            // The string value must NOT contain the expansion.
            let stored = specs[0].configured_matcher.as_deref().unwrap_or("");
            assert!(
                !stored.contains("expanded_value_should_not_appear"),
                "matcher must NOT be env-expanded, got {stored:?}"
            );
        });
    }

    /// Same property for the `${VAR}` form.
    #[test]
    fn parse_hook_file_matcher_braced_var_is_not_env_expanded() {
        // Build a matcher that is unambiguously
        // VALID regex regardless of whether expansion occurred. Using
        // a character class `[${KEY}]_tool` works because `$` is
        // trivially valid as a literal inside `[...]` (it loses its
        // anchor meaning), and `{`/`}` inside a character class are
        // also literals (not quantifier metachars). So whichever
        // string actually lands in the matcher, regex compilation
        // succeeds. This lets us assert on the single
        // successful-compile path with `assert_eq!(specs.len(), 1)`
        // and a single `assert!(!stored.contains(...))`.
        let key = "GROK_HOOKS_PARSE_TEST_MATCHER_BRACED";
        with_env_var(key, Some("expanded_should_not_appear"), || {
            let pattern = format!("[${{{key}}}]_tool");
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
            let stored = specs[0].configured_matcher.as_deref().unwrap_or("");
            assert!(
                !stored.contains("expanded_should_not_appear"),
                "matcher must NOT be env-expanded, got {stored:?}"
            );
            // Stored value must equal the source pattern verbatim.
            assert_eq!(stored, pattern);
        });
    }

    /// A non-string `env` value (e.g. `"PORT": 8080`) currently
    /// fails deserialization with a serde error. Document the failure
    /// mode and ensure the parse error is reported (not silently
    /// dropped). Users who need numeric values must wrap them in
    /// strings (`"PORT": "8080"`).
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
        // The parse error currently surfaces as a `ParseFile` error
        // from `serde_json::from_value` because `RawHandler` deserialises
        // env values as strings. The whole file fails to parse, which
        // means no specs come back. This is the documented failure
        // mode -- the alternative (stringifying numbers) requires a
        // custom deserializer that we can revisit if the constraint
        // becomes a real pain point in practice.
        assert!(
            specs.is_empty(),
            "expected non-string env value to fail parsing"
        );
        assert!(
            !errors.is_empty(),
            "expected an error for non-string env value, got none"
        );
        // Lock the error variant. The non-string
        // env value should surface as `HookError::ParseFile` (the
        // top-level matcher-group deserialization fails when serde
        // hits the typed `env` field), NOT as some generic
        // `InvalidConfig` or stub error -- which would mask future
        // regressions in error reporting.
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, HookError::ParseFile { .. })),
            "expected at least one HookError::ParseFile, got {errors:?}"
        );
    }

    /// User attempts to set runner-reserved keys (GROK_HOOK_*,
    /// GROK_SESSION_ID, GROK_WORKSPACE_ROOT, CLAUDE_PROJECT_DIR) via
    /// the JSON `env` map are stripped at load time. Spawn-time
    /// precedence ordering also overrides these keys, but stripping
    /// here gives users a clear "ignored" signal.
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
        // All five reserved keys must be stripped.
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
        // User-declared non-reserved key survives.
        assert_eq!(
            specs[0].extra_env.get("USER_KEY").map(String::as_str),
            Some("kept")
        );
        assert_eq!(specs[0].extra_env.len(), 1);
    }

    #[test]
    fn handler_with_extra_claude_fields() {
        let json = r#"{
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {
                                "type": "command",
                                "command": "check.sh",
                                "timeout": 5,
                                "allowedEnvVars": ["API_KEY"],
                                "someOtherField": true
                            }
                        ]
                    }
                ]
            }
        }"#;
        let (specs, errors) = parse_hook_file(json, Path::new("/tmp/test.json"));
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert_eq!(specs.len(), 1);
    }
}
