use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{self, HookSpec};
use crate::error::HookError;
use crate::event::HookEventName;
use crate::matcher::HookMatcher;

/// The loaded set of hooks, indexed by event type for fast lookup.
///
/// This is a point-in-time snapshot. Edits to hook files on disk are only
/// picked up by new sessions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookRegistry {
    hooks: HashMap<HookEventName, Vec<HookSpec>>,
}

impl HookRegistry {
    /// Returns the hooks registered for the given event type.
    pub fn hooks_for(&self, event: HookEventName) -> &[HookSpec] {
        self.hooks.get(&event).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Returns true if the registry contains no hooks at all.
    pub fn is_empty(&self) -> bool {
        self.hooks.values().all(|v| v.is_empty())
    }

    /// Returns the total number of hooks across all event types.
    pub fn len(&self) -> usize {
        self.hooks.values().map(|v| v.len()).sum()
    }

    /// Append additional hook specs into this registry.
    pub fn append_specs(&mut self, specs: Vec<HookSpec>) {
        for spec in specs {
            self.hooks.entry(spec.event).or_default().push(spec);
        }
    }

    /// Remove all hook specs whose name starts with the given prefix.
    pub fn remove_by_prefix(&mut self, prefix: &str) {
        for specs in self.hooks.values_mut() {
            specs.retain(|s| !s.name.starts_with(prefix));
        }
    }

    /// All event types in canonical display order.
    const ALL_EVENTS: &[HookEventName] = &[
        HookEventName::SessionStart,
        HookEventName::UserPromptSubmit,
        HookEventName::PreToolUse,
        HookEventName::PostToolUse,
        HookEventName::PostToolUseFailure,
        HookEventName::PermissionDenied,
        HookEventName::Stop,
        HookEventName::StopFailure,
        HookEventName::Notification,
        HookEventName::SubagentStart,
        HookEventName::SubagentStop,
        HookEventName::SubagentEnd,
        HookEventName::PreCompact,
        HookEventName::PostCompact,
        HookEventName::SessionEnd,
    ];

    /// Returns all hooks as a flat list, ordered by event type then position.
    pub fn all_hooks(&self) -> Vec<&HookSpec> {
        let mut all = Vec::new();
        for event in Self::ALL_EVENTS {
            all.extend(self.hooks_for(*event));
        }
        all
    }

    /// Recompile the `matcher` field on every [`HookSpec`] from its
    /// `configured_matcher` pattern string.
    ///
    /// After deserialization, the compiled [`HookMatcher`] is `None`
    /// (it is `#[serde(skip)]`). This method rebuilds it using the
    /// same logic as the original parse path, via [`HookMatcher::new`].
    ///
    /// Specs whose `configured_matcher` is `None` (match-all) are
    /// left untouched. Invalid patterns are logged and the spec's
    /// matcher remains `None` (match-all fallback), which is the
    /// safest fail-open behavior.
    pub fn recompile_matchers(&mut self) {
        for specs in self.hooks.values_mut() {
            for spec in specs.iter_mut() {
                if let Some(ref pattern) = spec.configured_matcher {
                    match HookMatcher::new(pattern) {
                        Ok(m) => spec.matcher = Some(m),
                        Err(e) => {
                            tracing::warn!(
                                hook = %spec.name,
                                pattern = %pattern,
                                error = %e,
                                "hooks: failed to recompile matcher after deserialization"
                            );
                            // Leave matcher as None → match-all (fail-open).
                        }
                    }
                }
            }
        }
    }
}

/// A hook source: either a single settings file or a directory of hook files.
#[derive(Debug, Clone)]
pub enum HookSource<'a> {
    /// A single JSON settings file (e.g. `~/.claude/settings.json`).
    /// The `hooks` key is extracted; other keys are ignored.
    SettingsFile(&'a Path),
    /// A directory of `*.json` hook files (e.g. `~/.grok/hooks/`).
    Directory(&'a Path),
}

/// Load hooks from global and project sources.
///
/// Sources are additive: hooks from all sources are merged into a single
/// registry. Global hooks run before project hooks. Within each scope,
/// earlier sources execute before later sources.
///
/// Returns the registry plus any non-fatal load errors.
/// A fully empty registry is valid (no-op when no hooks are configured).
pub fn load_hooks_from_sources(
    global_sources: &[HookSource<'_>],
    project_sources: &[HookSource<'_>],
) -> (HookRegistry, Vec<HookError>) {
    tracing::debug!(
        global_sources = global_sources.len(),
        project_sources = project_sources.len(),
        "hooks: starting discovery"
    );

    let mut all_specs = Vec::new();
    let mut all_errors = Vec::new();

    // Load global hooks first (precedence order: global, then project).
    for source in global_sources {
        let (mut specs, errors) = load_from_source(source);
        for spec in &mut specs {
            spec.name = format!("global/{}", spec.name);
        }
        tracing::debug!(
            source = ?source,
            count = specs.len(),
            "hooks: loaded from global source"
        );
        all_specs.extend(specs);
        all_errors.extend(errors);
    }

    // Load project hooks second.
    for source in project_sources {
        let (mut specs, errors) = load_from_source(source);
        for spec in &mut specs {
            spec.name = format!("project/{}", spec.name);
        }
        tracing::debug!(
            source = ?source,
            count = specs.len(),
            "hooks: loaded from project source"
        );
        all_specs.extend(specs);
        all_errors.extend(errors);
    }

    // Index by event type, deduplicating by hook content (command/url) +
    // matcher across all sources. This prevents the same hook from executing
    // multiple times when it's defined in multiple sources (e.g., ~/.grok/hooks/ +
    // ~/.claude/settings.json + ~/.cursor/hooks.json), while still allowing
    // hooks that share a command/URL but have different matchers (e.g. tool-scoped
    // hooks) to all run.
    //
    // Deduplication key: (event, command_raw, url_raw, configured_matcher).
    // Hooks with identical content + matcher are deduplicated regardless of
    // source. Global hooks take precedence because they're loaded first.
    let mut hooks: HashMap<HookEventName, Vec<HookSpec>> = HashMap::new();
    let mut seen_content: std::collections::HashSet<(HookEventName, String, String, String)> =
        std::collections::HashSet::new();
    for spec in all_specs {
        let key = (
            spec.event,
            spec.command_raw.clone().unwrap_or_default(),
            spec.url_raw.clone().unwrap_or_default(),
            spec.configured_matcher.clone().unwrap_or_default(),
        );
        if seen_content.insert(key) {
            hooks.entry(spec.event).or_default().push(spec);
        } else {
            tracing::debug!(
                hook_name = %spec.name,
                event = %spec.event,
                matcher = ?spec.configured_matcher,
                "hooks: skipping duplicate hook (same content + matcher already loaded from earlier source)"
            );
        }
    }

    let registry = HookRegistry { hooks };
    tracing::info!(
        total_hooks = registry.len(),
        session_start = registry.hooks_for(HookEventName::SessionStart).len(),
        pre_tool = registry.hooks_for(HookEventName::PreToolUse).len(),
        post_tool = registry.hooks_for(HookEventName::PostToolUse).len(),
        session_end = registry.hooks_for(HookEventName::SessionEnd).len(),
        stop = registry.hooks_for(HookEventName::Stop).len(),
        notification = registry.hooks_for(HookEventName::Notification).len(),
        user_prompt_submit = registry.hooks_for(HookEventName::UserPromptSubmit).len(),
        subagent_start = registry.hooks_for(HookEventName::SubagentStart).len(),
        subagent_stop = registry.hooks_for(HookEventName::SubagentStop).len()
            + registry.hooks_for(HookEventName::SubagentEnd).len(),
        "hooks: discovery complete"
    );

    (registry, all_errors)
}

/// Convenience wrapper: load hooks from a single global directory and optional
/// project directory. Used by the existing shell integration.
pub fn load_hooks(
    global_dir: Option<&Path>,
    project_dir: Option<&Path>,
) -> (HookRegistry, Vec<HookError>) {
    let global: Vec<HookSource<'_>> = global_dir.into_iter().map(HookSource::Directory).collect();
    let project: Vec<HookSource<'_>> = project_dir.into_iter().map(HookSource::Directory).collect();
    load_hooks_from_sources(&global, &project)
}

/// Load hooks from a single source (settings file or directory).
fn load_from_source(source: &HookSource<'_>) -> (Vec<HookSpec>, Vec<HookError>) {
    match source {
        HookSource::SettingsFile(path) => load_hooks_from_settings_file(path),
        HookSource::Directory(dir) => load_hooks_from_directory(dir),
    }
}

/// Load hooks from a single JSON settings file.
///
/// Reads the file, extracts the `hooks` key, and parses it. If the file
/// does not exist or has no `hooks` key, returns empty results (not an error).
fn load_hooks_from_settings_file(path: &Path) -> (Vec<HookSpec>, Vec<HookError>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                return (Vec::new(), Vec::new()); // Missing file is fine.
            }
            return (
                Vec::new(),
                vec![HookError::ReadFile {
                    path: path.to_path_buf(),
                    source: e,
                }],
            );
        }
    };

    let (specs, errors) = config::parse_hook_file(&content, path);
    for err in &errors {
        tracing::warn!("hook loading from settings file: {err}");
    }
    (specs, errors)
}

/// Load hooks from a single directory.
///
/// - Only loads `*.json` files.
/// - Ignores hidden/temp/editor files (dotfiles, `~`-suffixed, `.swp`).
/// - Sorts files lexicographically for deterministic ordering.
fn load_hooks_from_directory(dir: &Path) -> (Vec<HookSpec>, Vec<HookError>) {
    let mut specs = Vec::new();
    let mut errors = Vec::new();

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            // Missing directory is not an error — it just means no hooks.
            if e.kind() == std::io::ErrorKind::NotFound {
                return (specs, errors);
            }
            errors.push(HookError::ReadFile {
                path: dir.to_path_buf(),
                source: e,
            });
            return (specs, errors);
        }
    };

    // Collect and sort file paths lexicographically.
    let mut json_files: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                errors.push(HookError::ReadFile {
                    path: dir.to_path_buf(),
                    source: e,
                });
                continue;
            }
        };

        let path = entry.path();
        if !is_valid_hook_file(&path) {
            continue;
        }
        json_files.push(path);
    }
    json_files.sort();

    // Parse each file.
    for path in json_files {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                errors.push(HookError::ReadFile {
                    path: path.clone(),
                    source: e,
                });
                continue;
            }
        };

        let (file_specs, file_errors) = config::parse_hook_file(&content, &path);
        for err in &file_errors {
            tracing::warn!("hook loading: {err}");
        }
        specs.extend(file_specs);
        errors.extend(file_errors);
    }

    (specs, errors)
}

/// Check whether a path is a valid hook file (*.json, not hidden/temp).
fn is_valid_hook_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    // Must have .json extension.
    if path.extension().and_then(|e| e.to_str()) != Some("json") {
        return false;
    }

    // Skip hidden files (dotfiles).
    if name.starts_with('.') {
        return false;
    }

    // Skip editor temp files.
    if name.ends_with('~') || name.ends_with(".swp") || name.ends_with(".swo") {
        return false;
    }

    // Must be a file, not a directory.
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_json(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    /// Create a simple compatible-format JSON hook file for the given event.
    /// The `unique_id` parameter ensures each hook has a unique command,
    /// preventing deduplication when testing multiple files.
    fn simple_hook(event: &str) -> String {
        simple_hook_with_id(event, "test")
    }

    /// Create a simple compatible-format JSON hook file with a unique command.
    fn simple_hook_with_id(event: &str, id: &str) -> String {
        serde_json::json!({
            "hooks": {
                event: [{"hooks": [{"type": "command", "command": format!("{}.sh", id)}]}]
            }
        })
        .to_string()
    }

    #[test]
    fn load_empty_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert!(errors.is_empty());
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn load_missing_dirs() {
        let (registry, errors) = load_hooks(None, None);
        assert!(errors.is_empty());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_nonexistent_dir() {
        let (registry, errors) = load_hooks(Some(Path::new("/nonexistent/path/hooks")), None);
        assert!(errors.is_empty()); // NotFound is silent
        assert!(registry.is_empty());
    }

    #[test]
    fn load_single_hook() {
        let dir = tempfile::tempdir().unwrap();
        write_json(dir.path(), "safety.json", &simple_hook("PreToolUse"));

        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert_eq!(registry.len(), 1);
        let hooks = registry.hooks_for(HookEventName::PreToolUse);
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn lexicographic_ordering_across_files() {
        let dir = tempfile::tempdir().unwrap();
        // Use unique IDs so hooks aren't deduplicated.
        write_json(
            dir.path(),
            "02-second.json",
            &simple_hook_with_id("PreToolUse", "second"),
        );
        write_json(
            dir.path(),
            "01-first.json",
            &simple_hook_with_id("PreToolUse", "first"),
        );
        write_json(
            dir.path(),
            "03-third.json",
            &simple_hook_with_id("PreToolUse", "third"),
        );

        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert!(errors.is_empty());
        let hooks = registry.hooks_for(HookEventName::PreToolUse);
        assert_eq!(hooks.len(), 3);
        // All hooks are PreToolUse, loaded in file order (01, 02, 03).
    }

    #[test]
    fn global_before_project() {
        let global = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        // Use unique IDs so hooks aren't deduplicated.
        write_json(
            global.path(),
            "global.json",
            &simple_hook_with_id("PreToolUse", "global"),
        );
        write_json(
            project.path(),
            "project.json",
            &simple_hook_with_id("PreToolUse", "project"),
        );

        let (registry, errors) = load_hooks(Some(global.path()), Some(project.path()));
        assert!(errors.is_empty());
        let hooks = registry.hooks_for(HookEventName::PreToolUse);
        assert_eq!(hooks.len(), 2);
    }

    #[test]
    fn skip_hidden_and_non_json_files() {
        let dir = tempfile::tempdir().unwrap();
        write_json(dir.path(), "valid.json", &simple_hook("SessionStart"));
        write_json(dir.path(), ".hidden.json", &simple_hook("SessionStart"));
        write_json(dir.path(), "backup.json~", "{}");
        write_json(dir.path(), "not-json.txt", "{}");
        write_json(dir.path(), "not-json.toml", "version = 1");

        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert!(errors.is_empty());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn multiple_handlers_in_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let content = r#"{
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
        write_json(dir.path(), "multi.json", content);

        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert!(errors.is_empty());
        let hooks = registry.hooks_for(HookEventName::PreToolUse);
        assert_eq!(hooks.len(), 2);
    }

    #[test]
    fn invalid_file_skipped_others_loaded() {
        let dir = tempfile::tempdir().unwrap();
        write_json(dir.path(), "01-good.json", &simple_hook("SessionStart"));
        write_json(dir.path(), "02-bad.json", "not valid json {{{");
        write_json(dir.path(), "03-also-good.json", &simple_hook("SessionEnd"));

        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert_eq!(errors.len(), 1);
        assert!(matches!(&errors[0], HookError::ParseFile { .. }));
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn hooks_indexed_by_event_type() {
        let dir = tempfile::tempdir().unwrap();
        // One file with all four event types.
        let content = r#"{
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "a.sh"}]}],
                "PreToolUse": [{"hooks": [{"type": "command", "command": "b.sh"}]}],
                "PostToolUse": [{"hooks": [{"type": "command", "command": "c.sh"}]}],
                "SessionEnd": [{"hooks": [{"type": "command", "command": "d.sh"}]}]
            }
        }"#;
        write_json(dir.path(), "all.json", content);

        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert!(errors.is_empty());
        assert_eq!(registry.hooks_for(HookEventName::SessionStart).len(), 1);
        assert_eq!(registry.hooks_for(HookEventName::PreToolUse).len(), 1);
        assert_eq!(registry.hooks_for(HookEventName::PostToolUse).len(), 1);
        assert_eq!(registry.hooks_for(HookEventName::SessionEnd).len(), 1);
    }

    #[test]
    fn all_hooks_covers_every_event_type() {
        let dir = tempfile::tempdir().unwrap();
        // Create hooks for all 10 event types in one file.
        let content = r#"{
            "hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": "a.sh"}]}],
                "PreToolUse": [{"hooks": [{"type": "command", "command": "b.sh"}]}],
                "PostToolUse": [{"hooks": [{"type": "command", "command": "c.sh"}]}],
                "SessionEnd": [{"hooks": [{"type": "command", "command": "d.sh"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "e.sh"}]}],
                "Notification": [{"hooks": [{"type": "command", "command": "f.sh"}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "g.sh"}]}],
                "SubagentStart": [{"hooks": [{"type": "command", "command": "h.sh"}]}],
                "SubagentStop": [{"hooks": [{"type": "command", "command": "i.sh"}]}],
                "SubagentEnd": [{"hooks": [{"type": "command", "command": "j.sh"}]}]
            }
        }"#;
        write_json(dir.path(), "all-events.json", content);

        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert_eq!(registry.len(), 10);

        // all_hooks() must return all 10 — not just the original 4.
        let all = registry.all_hooks();
        assert_eq!(
            all.len(),
            10,
            "all_hooks() returned {} hooks, expected 10 (all event types)",
            all.len()
        );

        // Verify each event type is represented.
        let events: Vec<HookEventName> = all.iter().map(|h| h.event).collect();
        assert!(events.contains(&HookEventName::SessionStart));
        assert!(events.contains(&HookEventName::PreToolUse));
        assert!(events.contains(&HookEventName::PostToolUse));
        assert!(events.contains(&HookEventName::SessionEnd));
        assert!(events.contains(&HookEventName::Stop));
        assert!(events.contains(&HookEventName::Notification));
        assert!(events.contains(&HookEventName::UserPromptSubmit));
        assert!(events.contains(&HookEventName::SubagentStart));
        assert!(events.contains(&HookEventName::SubagentStop));
        assert!(events.contains(&HookEventName::SubagentEnd));
    }

    #[test]
    fn is_valid_hook_file_cases() {
        let dir = tempfile::tempdir().unwrap();

        let valid = dir.path().join("hooks.json");
        std::fs::write(&valid, "").unwrap();
        assert!(is_valid_hook_file(&valid));

        let hidden = dir.path().join(".hidden.json");
        std::fs::write(&hidden, "").unwrap();
        assert!(!is_valid_hook_file(&hidden));

        let backup = dir.path().join("backup.json~");
        std::fs::write(&backup, "").unwrap();
        assert!(!is_valid_hook_file(&backup));

        let txt = dir.path().join("readme.txt");
        std::fs::write(&txt, "").unwrap();
        assert!(!is_valid_hook_file(&txt));

        let toml = dir.path().join("hooks.toml");
        std::fs::write(&toml, "").unwrap();
        assert!(!is_valid_hook_file(&toml)); // TOML no longer accepted
    }

    // ── Settings file discovery tests ────────────────────────────

    #[test]
    fn load_from_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"check.sh"}]}]}}"#,
        )
        .unwrap();

        let (registry, errors) =
            load_hooks_from_sources(&[HookSource::SettingsFile(&settings)], &[]);
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn load_from_missing_settings_file() {
        let (registry, errors) = load_hooks_from_sources(
            &[HookSource::SettingsFile(Path::new(
                "/nonexistent/settings.json",
            ))],
            &[],
        );
        assert!(errors.is_empty()); // Missing file is fine, not an error.
        assert!(registry.is_empty());
    }

    #[test]
    fn load_from_settings_file_no_hooks_key() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(&settings, r#"{"theme": "dark", "model": "grok-3"}"#).unwrap();

        let (registry, errors) =
            load_hooks_from_sources(&[HookSource::SettingsFile(&settings)], &[]);
        assert!(errors.is_empty());
        assert!(registry.is_empty());
    }

    #[test]
    fn mixed_sources_settings_and_directory() {
        let dir = tempfile::tempdir().unwrap();

        // Settings file with one hook.
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"from-settings.sh"}]}]}}"#,
        )
        .unwrap();

        // Directory with another hook.
        let hooks_dir = dir.path().join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        write_json(&hooks_dir, "extra.json", &simple_hook("SessionStart"));

        let (registry, errors) = load_hooks_from_sources(
            &[
                HookSource::SettingsFile(&settings),
                HookSource::Directory(&hooks_dir),
            ],
            &[],
        );
        assert!(errors.is_empty(), "errors: {errors:?}");
        // Both hooks should be loaded (additive merge).
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.hooks_for(HookEventName::PreToolUse).len(), 1);
        assert_eq!(registry.hooks_for(HookEventName::SessionStart).len(), 1);
    }

    #[test]
    fn global_and_project_settings_merged() {
        let dir = tempfile::tempdir().unwrap();

        let global_settings = dir.path().join("global.json");
        std::fs::write(
            &global_settings,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"global.sh"}]}]}}"#,
        )
        .unwrap();

        let project_settings = dir.path().join("project.json");
        std::fs::write(
            &project_settings,
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"project.sh"}]}]}}"#,
        )
        .unwrap();

        let (registry, errors) = load_hooks_from_sources(
            &[HookSource::SettingsFile(&global_settings)],
            &[HookSource::SettingsFile(&project_settings)],
        );
        assert!(errors.is_empty());
        let hooks = registry.hooks_for(HookEventName::PreToolUse);
        assert_eq!(hooks.len(), 2);
        // Global hook first, project hook second.
        assert!(hooks[0].name.starts_with("global/"));
        assert!(hooks[1].name.starts_with("project/"));
    }

    #[test]
    fn deduplicates_hooks_with_same_content_across_sources() {
        let dir = tempfile::tempdir().unwrap();

        // Create three sources with the SAME hook command.
        // Only the first one (global) should be kept.
        let global_settings = dir.path().join("global.json");
        std::fs::write(
            &global_settings,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"safety.sh"}]}]}}"#,
        )
        .unwrap();

        let claude_settings = dir.path().join("claude.json");
        std::fs::write(
            &claude_settings,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"safety.sh"}]}]}}"#,
        )
        .unwrap();

        let cursor_settings = dir.path().join("cursor.json");
        std::fs::write(
            &cursor_settings,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"safety.sh"}]}]}}"#,
        )
        .unwrap();

        let (registry, errors) = load_hooks_from_sources(
            &[
                HookSource::SettingsFile(&global_settings),
                HookSource::SettingsFile(&claude_settings),
                HookSource::SettingsFile(&cursor_settings),
            ],
            &[],
        );
        assert!(errors.is_empty());
        // Only one hook should be loaded (the first one, from global).
        let hooks = registry.hooks_for(HookEventName::SessionStart);
        assert_eq!(
            hooks.len(),
            1,
            "expected exactly 1 SessionStart hook after dedup, got {}",
            hooks.len()
        );
        assert!(
            hooks[0].name.starts_with("global/"),
            "first source (global) should win, got: {}",
            hooks[0].name
        );
    }

    #[test]
    fn different_commands_not_deduplicated() {
        let dir = tempfile::tempdir().unwrap();

        // Different hook commands - should NOT be deduplicated.
        let global_settings = dir.path().join("global.json");
        std::fs::write(
            &global_settings,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"first.sh"}]}]}}"#,
        )
        .unwrap();

        let claude_settings = dir.path().join("claude.json");
        std::fs::write(
            &claude_settings,
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"second.sh"}]}]}}"#,
        )
        .unwrap();

        let (registry, errors) = load_hooks_from_sources(
            &[
                HookSource::SettingsFile(&global_settings),
                HookSource::SettingsFile(&claude_settings),
            ],
            &[],
        );
        assert!(errors.is_empty());
        // Both hooks should be loaded since they have different commands.
        let hooks = registry.hooks_for(HookEventName::SessionStart);
        assert_eq!(
            hooks.len(),
            2,
            "expected 2 SessionStart hooks with different commands, got {}",
            hooks.len()
        );
    }

    #[test]
    fn different_event_types_not_deduplicated() {
        let dir = tempfile::tempdir().unwrap();

        // Same command but different event types - should NOT be deduplicated.
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            r#"{
                "hooks": {
                    "SessionStart": [{"hooks": [{"type": "command", "command": "hook.sh"}]}],
                    "SessionEnd": [{"hooks": [{"type": "command", "command": "hook.sh"}]}]
                }
            }"#,
        )
        .unwrap();

        let (registry, errors) =
            load_hooks_from_sources(&[HookSource::SettingsFile(&settings)], &[]);
        assert!(errors.is_empty());
        // Both hooks should be loaded since they're different event types.
        assert_eq!(registry.hooks_for(HookEventName::SessionStart).len(), 1);
        assert_eq!(registry.hooks_for(HookEventName::SessionEnd).len(), 1);
    }

    #[test]
    fn same_command_in_same_directory_deduplicated() {
        // When the same hook command is defined in multiple files within
        // the same directory, they should be deduplicated (only the first
        // one runs). This prevents accidental duplicate execution.
        let dir = tempfile::tempdir().unwrap();

        // Two files with the same hook command.
        write_json(
            dir.path(),
            "01-first.json",
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"same.sh"}]}]}}"#,
        );
        write_json(
            dir.path(),
            "02-second.json",
            r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"same.sh"}]}]}}"#,
        );

        let (registry, errors) = load_hooks(Some(dir.path()), None);
        assert!(errors.is_empty());
        // Only one hook should be loaded (deduplicated by content).
        let hooks = registry.hooks_for(HookEventName::SessionStart);
        assert_eq!(
            hooks.len(),
            1,
            "expected exactly 1 SessionStart hook after dedup, got {}",
            hooks.len()
        );
    }

    #[test]
    fn realistic_claude_settings_discovery() {
        let dir = tempfile::tempdir().unwrap();

        // Simulate ~/.claude/settings.json with many extra keys.
        let claude_settings = dir.path().join("settings.json");
        std::fs::write(
            &claude_settings,
            r#"{
                "model": "claude-sonnet-4-20250514",
                "permissions": {"allow": ["Bash(npm test)"]},
                "hooks": {
                    "PreToolUse": [
                        {"matcher": "Bash", "hooks": [{"type": "command", "command": "check.sh"}]}
                    ]
                },
                "mcpServers": {"memory": {"command": "npx"}}
            }"#,
        )
        .unwrap();

        let (registry, errors) =
            load_hooks_from_sources(&[HookSource::SettingsFile(&claude_settings)], &[]);
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert_eq!(registry.len(), 1);
    }
}
