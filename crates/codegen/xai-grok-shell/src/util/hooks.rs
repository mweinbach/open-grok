//! Shared hook source path discovery.

use std::path::{Path, PathBuf};

use xai_grok_hooks::discovery::HookSource;

/// Owned paths for hook sources. Callers borrow via `as_sources()`.
pub struct HookSourcePaths {
    pub global: Vec<PathBuf>,
    pub project: Vec<PathBuf>,
}

impl HookSourcePaths {
    /// Borrow as `HookSource` refs. Project sources are excluded when untrusted.
    pub fn as_sources(&self, include_project: bool) -> (Vec<HookSource<'_>>, Vec<HookSource<'_>>) {
        let global = self.global.iter().map(|p| path_to_source(p)).collect();
        let project = if include_project {
            self.project.iter().map(|p| path_to_source(p)).collect()
        } else {
            vec![]
        };
        (global, project)
    }
}

fn path_to_source(p: &Path) -> HookSource<'_> {
    if p.is_dir() {
        HookSource::Directory(p)
    } else {
        HookSource::SettingsFile(p)
    }
}

/// Build hook source paths for global (`~/`) and project (`<git_root>/`) scopes.
/// Callers gate project sources on trust via `as_sources(trusted)`.
pub fn discover_hook_source_paths(
    git_root: Option<&Path>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
) -> HookSourcePaths {
    // Compat gate: skip .claude hook sources when disabled.
    let skip_claude_compat = !compat.claude.hooks;
    // Phase 2 cutoff: if the user has imported, skip .claude/settings.json
    // sources. Native .grok/hooks/ directories are still scanned (they hold
    // any hooks that were imported by /import-claude).
    let skip_claude = skip_claude_compat
        || crate::claude_import::is_claude_import_marked_with_log("discover_hook_source_paths");

    // Compat gate: skip Cursor hook sources when disabled.
    let skip_cursor = !compat.cursor.hooks;

    let home = dirs::home_dir();
    // user_grok_home() is None when no home resolves, so inspect lists the same
    // sources a live session loads, instead of a cwd-relative .grok.
    let grok = xai_grok_config::user_grok_home();
    let mut global = Vec::new();

    if !skip_claude && let Some(ref h) = home {
        global.push(h.join(".claude").join("settings.json"));
        global.push(h.join(".claude").join("settings.local.json"));
    }
    if let Some(ref grok) = grok {
        global.push(grok.join("hooks"));
    }

    let custom_paths: Vec<PathBuf> = grok
        .as_ref()
        .and_then(|g| std::fs::read_to_string(g.join("hooks-paths")).ok())
        .map(|content| {
            content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| PathBuf::from(l.trim()))
                .collect()
        })
        .unwrap_or_default();
    global.extend(custom_paths);

    if let Some(ref h) = home
        && !skip_cursor
    {
        global.push(h.join(".cursor").join("hooks.json"));
    }

    let mut project = Vec::new();

    if let Some(root) = git_root {
        if !skip_claude {
            project.push(root.join(".claude").join("settings.json"));
            project.push(root.join(".claude").join("settings.local.json"));
        }
        project.push(root.join(".grok").join("hooks"));
        if !skip_cursor {
            project.push(root.join(".cursor").join("hooks.json"));
        }
    }

    HookSourcePaths { global, project }
}

/// Single load entry point: build compat-aware sources, gate project sources on
/// trust, then load. Every session-startup and mid-session reload site routes
/// through here so the source policy stays in one place. `discover_hook_source_paths`
/// and `HookSourcePaths::as_sources` stay public for the `inspect` path (which
/// enumerates sources with all vendors on) and the unit tests that assert on the
/// raw source lists.
pub fn discover_hooks(
    git_root: Option<&Path>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
    trusted: bool,
) -> (
    xai_grok_hooks::discovery::HookRegistry,
    Vec<xai_grok_hooks::error::HookError>,
) {
    let source_paths = discover_hook_source_paths(git_root, compat);
    let (global_sources, project_sources) = source_paths.as_sources(trusted);
    xai_grok_hooks::discovery::load_hooks_from_sources(&global_sources, &project_sources)
}
