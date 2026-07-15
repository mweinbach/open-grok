//! AGENTS.md / Claude.md / rules directory discovery and loading.
//!
//! Searches from cwd to repo root, plus `~/.grok/`. Also discovers
//! `*.md` files in `.grok/rules/` and `.claude/rules/` directories.

use std::path::{Path, PathBuf};

use crate::prompt::ignore::{build_gitignore, is_ignored};

use xai_grok_tools::types::compat::CompatConfig;

/// Represents an agent config file with its path and content.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentConfigFile {
    /// The filename (e.g., "AGENTS.md", "Claude.md")
    pub file_name: String,
    /// The full absolute path to the config file
    pub file_path: String,
    /// The content of the config file
    pub content: String,
}

/// Find matching agent config files in a directory.
///
/// `filenames` is the (compat-gated) recognized list, precomputed once by the
/// caller so the cwd→root walk doesn't re-allocate it per directory. When all
/// cells are on it equals the legacy `AGENT_FILENAMES` list exactly.
fn find_agent_files(dir: &Path, filenames: &[&str]) -> Vec<PathBuf> {
    filenames
        .iter()
        .filter_map(|name| {
            let path = dir.join(name);
            path.exists().then_some(path)
        })
        .collect()
}

/// Find `*.md` files in `.grok/rules/`, `.claude/rules/`, and `.cursor/rules/`,
/// sorted alphabetically. `rules_subdirs` is the (compat-gated) list, precomputed
/// once by the caller so the walk doesn't re-allocate it per directory.
fn find_rules_files(dir: &Path, rules_subdirs: &[&str]) -> Vec<PathBuf> {
    let mut results = Vec::new();
    for rules_subdir in rules_subdirs {
        let rules_dir = dir.join(rules_subdir);
        if !rules_dir.is_dir() {
            continue;
        }
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(&rules_dir) {
            Ok(iter) => iter
                .filter_map(|entry| entry.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                })
                .collect(),
            Err(_) => continue,
        };
        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        results.extend(entries);
    }
    results
}

/// Read Agents.md from ~/.grok/, git repo root, and session cwd.
/// Returns a list of AgentConfigFile with their file names, full paths, and contents.
///
/// `compat` gates which vendor (`.claude`/`.cursor`) surfaces are scanned for
/// rules / project-instruction files; pass `CompatConfig::default()` to
/// preserve the historical all-vendors behavior.
pub async fn read_agents_config_with_paths(
    working_directory: &str,
    compat: CompatConfig,
) -> Vec<AgentConfigFile> {
    let workspace_user_dir = crate::prompt::workspace_user::optional_workspace_user_dir();
    read_agents_config_with_options(working_directory, workspace_user_dir.as_deref(), compat).await
}

/// Inner implementation that accepts an optional workspace user dir as a
/// parameter, making it testable without environment variable mutation.
async fn read_agents_config_with_options(
    working_directory: &str,
    workspace_user_dir: Option<&Path>,
    compat: CompatConfig,
) -> Vec<AgentConfigFile> {
    let cwd = PathBuf::from(working_directory);
    let global_dir = xai_grok_tools::util::grok_home::grok_home();
    let git_root = git2::Repository::discover(&cwd)
        .ok()
        .and_then(|repo| repo.workdir().map(|p| p.to_path_buf()));

    let gitignore = build_gitignore(git_root.as_deref());

    // Always include grok_home (~/.grok/) first, then ~/.claude/ and ~/.cursor/
    // for compat — each gated by the resolved `agents` compat cell.
    let mut dirs = vec![global_dir];
    if let Some(home) = dirs::home_dir() {
        for compat_dir in compat.agents_home_dirs() {
            let dir = home.join(compat_dir);
            if dir.is_dir() {
                dirs.push(dir);
            }
        }
    }

    // Walk from cwd up to git root to pick up agent files in intermediate directories
    if let Some(ref root) = git_root {
        let mut current = Some(cwd.as_path());
        let mut chain: Vec<PathBuf> = Vec::new();
        while let Some(dir) = current {
            let dir_buf = dir.to_path_buf();
            if !chain.contains(&dir_buf) {
                chain.push(dir_buf);
            }
            if dir == root.as_path() {
                break;
            }
            current = dir.parent();
        }
        // CRITICAL: Reverse to get root → CWD order (deeper files come later)
        chain.reverse();

        // Inject optional workspace user dir if not already in the chain.
        // Insert after repo root (index 0 after reverse) so it's higher priority
        // than repo root AGENTS.md but lower priority than intermediate dirs and cwd.
        if let Some(user_dir) = workspace_user_dir {
            let user_dir_canonical =
                dunce::canonicalize(user_dir).unwrap_or_else(|_| user_dir.to_path_buf());
            let already_in_chain = chain.iter().any(|d| {
                dunce::canonicalize(d).unwrap_or_else(|_| d.clone()) == user_dir_canonical
            });
            if !already_in_chain {
                // chain[0] is repo root after reverse; insert right after it.
                let insert_pos = 1.min(chain.len());
                chain.insert(insert_pos, user_dir.to_path_buf());
            }
        }

        dirs.extend(chain);
    } else if !dirs.contains(&cwd) {
        dirs.push(cwd.clone());
    }

    // Compute the gated lists once (constant across all scanned dirs) so the
    // per-directory scan below doesn't re-allocate them.
    let agent_filenames = compat.agent_filenames();
    let rules_dirs = compat.rules_dirs();
    let files: Vec<PathBuf> = dirs
        .into_iter()
        .flat_map(|dir| {
            let mut combined = find_agent_files(&dir, &agent_filenames);
            combined.extend(find_rules_files(&dir, &rules_dirs));
            combined
        })
        .filter(|path| !is_ignored(path, gitignore.as_ref(), git_root.as_deref()))
        .collect();

    // Deduplicate by canonical path to handle case-insensitive filesystems
    // and symlink-resolved tmpdir paths.
    let mut seen_canonical = std::collections::HashSet::new();

    files
        .into_iter()
        .filter(|path| {
            let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.clone());
            seen_canonical.insert(canonical)
        })
        .filter_map(|file_path| {
            let content = std::fs::read_to_string(&file_path).ok()?;
            let file_name = file_path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("AGENTS.md")
                .to_string();
            let full_path = file_path.display().to_string();
            Some(AgentConfigFile {
                file_name,
                file_path: full_path,
                content,
            })
        })
        .collect()
}

/// Format AGENTS.md configs into a `<system-reminder>` block for user message injection.
pub fn format_agents_md_section(configs: &[AgentConfigFile]) -> Option<String> {
    render_agents_md(configs)
}

/// Verbatim leading bytes [`render_agents_md`] emits for every reminder block.
/// Used by `xai-grok-shell` to structurally detect legacy untagged AGENTS.md
/// copies (pre-`SyntheticReason::ProjectInstructions`) on resumed sessions.
pub const LEGACY_AGENTS_MD_REMINDER_PREFIX: &str =
    "\n\n<system-reminder>\nAs you answer the user's questions, you can use the following context";

fn render_agents_md(configs: &[AgentConfigFile]) -> Option<String> {
    if configs.is_empty() {
        return None;
    }

    let mut section = String::new();
    section.push_str(LEGACY_AGENTS_MD_REMINDER_PREFIX);
    section.push_str(
        " (ordered from repo root to current directory - deeper files take precedence on conflicts):\n",
    );

    for config in configs {
        section.push_str(&format!("\n## From: {}\n", config.file_path));

        // Strip YAML frontmatter from rules files (e.g. .claude/rules/*.md,
        // .grok/rules/*.md) so globs/paths metadata doesn't leak into the
        // system prompt as raw YAML.
        let is_rules_file = config.file_path.contains("/.grok/rules/")
            || config.file_path.contains("/.claude/rules/");
        let content = if is_rules_file {
            xai_grok_tools::implementations::skills::skill::extract_skill_body(&config.content)
        } else {
            config.content.clone()
        };

        section.push_str(&content);
        section.push('\n');
    }

    section.push_str("\nFollow these instructions exactly. When working in subdirectories not listed above, check for additional project instruction files (AGENTS.md, Claude.md, etc.).");
    section.push_str("\n</system-reminder>");

    Some(section)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper: initialize a git repo at `path` so git2::Repository::discover works.
    fn init_git_repo(path: &Path) {
        git2::Repository::init(path).unwrap();
    }

    // ── find_agent_files unit tests ─────────────────────────────────

    #[test]
    fn find_agent_files_finds_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "# Instructions").unwrap();

        let files = find_agent_files(tmp.path(), &CompatConfig::default().agent_filenames());
        // On case-insensitive filesystems (macOS), both "Agents.md" and "AGENTS.md"
        // resolve to the same file, so we may get more than 1 result.
        assert!(!files.is_empty());
        assert!(
            files
                .iter()
                .any(|f| f.to_string_lossy().contains("AGENTS.md")
                    || f.to_string_lossy().contains("Agents.md"))
        );
    }

    #[test]
    fn find_agent_files_finds_all_variants() {
        let tmp = tempfile::tempdir().unwrap();
        let filenames = CompatConfig::default().agent_filenames();
        for name in &filenames {
            let path = tmp.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, format!("# {name}")).unwrap();
        }

        let files = find_agent_files(tmp.path(), &filenames);
        assert_eq!(files.len(), filenames.len());
    }

    #[test]
    fn find_agent_files_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let files = find_agent_files(tmp.path(), &CompatConfig::default().agent_filenames());
        assert!(files.is_empty());
    }

    #[test]
    fn find_agent_files_nonexistent_dir() {
        let files = find_agent_files(
            Path::new("/nonexistent/dir"),
            &CompatConfig::default().agent_filenames(),
        );
        assert!(files.is_empty());
    }

    #[test]
    fn find_agent_files_discovers_claude_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_dir = tmp.path().join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(claude_dir.join("CLAUDE.md"), "# Project instructions").unwrap();

        let files = find_agent_files(tmp.path(), &CompatConfig::default().agent_filenames());
        assert!(
            files
                .iter()
                .any(|f| f.to_string_lossy().contains(".claude/CLAUDE.md")),
            "Should discover .claude/CLAUDE.md, got: {files:?}"
        );
    }

    #[test]
    fn find_rules_files_discovers_claude_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let rules_dir = tmp.path().join(".claude").join("rules");
        fs::create_dir_all(&rules_dir).unwrap();
        fs::write(rules_dir.join("style.md"), "# Style rules").unwrap();
        fs::write(rules_dir.join("safety.md"), "# Safety rules").unwrap();

        let files = find_rules_files(tmp.path(), &CompatConfig::default().rules_dirs());
        assert_eq!(files.len(), 2);
        assert!(files[0].to_string_lossy().contains("safety.md"));
        assert!(files[1].to_string_lossy().contains("style.md"));
    }

    // ── format_agents_md_section tests ──────────────────────────────

    #[test]
    fn format_agents_md_section_empty_returns_none() {
        assert!(format_agents_md_section(&[]).is_none());
    }

    #[test]
    fn format_agents_md_section_includes_all_configs() {
        let configs = vec![
            AgentConfigFile {
                file_name: "AGENTS.md".to_string(),
                file_path: "/repo/AGENTS.md".to_string(),
                content: "Repo-level instructions".to_string(),
            },
            AgentConfigFile {
                file_name: "AGENTS.md".to_string(),
                file_path: "/repo/x/user/AGENTS.md".to_string(),
                content: "User-level instructions".to_string(),
            },
        ];

        let section = format_agents_md_section(&configs).unwrap();
        assert!(section.contains("Repo-level instructions"));
        assert!(section.contains("User-level instructions"));
        assert!(section.contains("/repo/AGENTS.md"));
        assert!(section.contains("/repo/x/user/AGENTS.md"));
        assert!(section.contains("<system-reminder>"));
    }

    #[test]
    fn format_agents_md_section_delivers_full_content() {
        let long_content = "A".repeat(5000);
        let configs = vec![AgentConfigFile {
            file_name: "AGENTS.md".to_string(),
            file_path: "/repo/AGENTS.md".to_string(),
            content: long_content,
        }];
        let section = format_agents_md_section(&configs).unwrap();
        // No cap: the full content is delivered verbatim, with no truncation marker.
        assert!(
            section.contains(&"A".repeat(5000)),
            "full content must be preserved"
        );
        assert!(
            !section.contains("truncated"),
            "content must not be truncated"
        );
    }

    // ── Feature 2: Workspace user AGENTS.md via read_agents_config ───

    #[tokio::test]
    async fn read_agents_config_includes_workspace_user_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_git_repo(&repo_root);

        // Create user AGENTS.md
        let user_dir = repo_root.join("x").join("testuser");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(
            user_dir.join("AGENTS.md"),
            "# User-specific instructions\nAlways use tabs.",
        )
        .unwrap();

        // cwd = repo root (user dir is NOT in the walk path)
        let configs = read_agents_config_with_options(
            repo_root.to_str().unwrap(),
            Some(&user_dir),
            CompatConfig::default(),
        )
        .await;

        let contents: Vec<&str> = configs.iter().map(|c| c.content.as_str()).collect();
        assert!(
            contents.iter().any(|c| c.contains("Always use tabs")),
            "Workspace user AGENTS.md should be included, got: {contents:?}"
        );
    }

    #[tokio::test]
    async fn read_agents_config_workspace_user_dedup_when_cwd_inside_user_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_git_repo(&repo_root);

        // User dir with AGENTS.md
        let user_dir = repo_root.join("x").join("testuser");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("AGENTS.md"), "# Dedup test instructions").unwrap();

        // cwd IS the user dir — the walk already includes it
        let configs = read_agents_config_with_options(
            user_dir.to_str().unwrap(),
            Some(&user_dir),
            CompatConfig::default(),
        )
        .await;

        // "Dedup test instructions" should appear exactly once
        let count = configs
            .iter()
            .filter(|c| c.content.contains("Dedup test instructions"))
            .count();
        assert_eq!(
            count, 1,
            "User AGENTS.md should appear exactly once, got {count}"
        );
    }

    #[tokio::test]
    async fn read_agents_config_no_workspace_user_dir_no_user_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_git_repo(&repo_root);

        // User dir with AGENTS.md (should NOT be found)
        let user_dir = repo_root.join("x").join("ghost");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("AGENTS.md"), "# Ghost instructions").unwrap();

        // Pass None — simulates env vars not set
        let configs = read_agents_config_with_options(
            repo_root.to_str().unwrap(),
            None,
            CompatConfig::default(),
        )
        .await;

        let has_ghost = configs
            .iter()
            .any(|c| c.content.contains("Ghost instructions"));
        assert!(
            !has_ghost,
            "Without optional workspace user dir, ghost AGENTS.md should not be found"
        );
    }

    /// Regression: running outside a git repo must not panic.
    #[tokio::test]
    async fn regression_no_panic_outside_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not_a_repo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("AGENTS.md"), "# outside git").unwrap();

        let configs =
            read_agents_config_with_options(dir.to_str().unwrap(), None, CompatConfig::default())
                .await;
        assert!(configs.iter().any(|c| c.content.contains("outside git")));
    }

    #[tokio::test]
    async fn read_agents_config_workspace_user_and_repo_root_both_found() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_git_repo(&repo_root);

        // Repo root AGENTS.md
        fs::write(repo_root.join("AGENTS.md"), "# XYZZY_REPO_ROOT_MARKER").unwrap();

        // User AGENTS.md
        let user_dir = repo_root.join("x").join("testuser");
        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("AGENTS.md"), "# XYZZY_USER_SPECIFIC_MARKER").unwrap();

        let configs = read_agents_config_with_options(
            repo_root.to_str().unwrap(),
            Some(&user_dir),
            CompatConfig::default(),
        )
        .await;

        // Both should be found
        let has_repo = configs
            .iter()
            .any(|c| c.content.contains("XYZZY_REPO_ROOT_MARKER"));
        let has_user = configs
            .iter()
            .any(|c| c.content.contains("XYZZY_USER_SPECIFIC_MARKER"));

        assert!(
            has_repo,
            "Repo root AGENTS.md not found in: {:?}",
            configs
                .iter()
                .map(|c| (&c.file_path, &c.content))
                .collect::<Vec<_>>()
        );
        assert!(
            has_user,
            "User AGENTS.md not found in: {:?}",
            configs
                .iter()
                .map(|c| (&c.file_path, &c.content))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn render_strips_frontmatter_from_rules_files() {
        let configs = vec![AgentConfigFile {
            file_name: "style.md".to_string(),
            file_path: "/repo/.claude/rules/style.md".to_string(),
            content: "---\nglobs: [\"*.rs\"]\n---\n# Use snake_case".to_string(),
        }];
        let section = format_agents_md_section(&configs).unwrap();
        assert!(section.contains("# Use snake_case"));
        assert!(!section.contains("globs:"));
    }

    // ── .claude/CLAUDE.md integration tests ─────────────────────────

    #[tokio::test]
    async fn read_agents_config_discovers_claude_subdir_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_git_repo(&repo_root);

        // .claude/CLAUDE.md at repo root
        let claude_dir = repo_root.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(claude_dir.join("CLAUDE.md"), "# XYZZY_CLAUDE_SUBDIR_MARKER").unwrap();

        let configs = read_agents_config_with_options(
            repo_root.to_str().unwrap(),
            None,
            CompatConfig::default(),
        )
        .await;

        assert!(
            configs
                .iter()
                .any(|c| c.content.contains("XYZZY_CLAUDE_SUBDIR_MARKER")),
            ".claude/CLAUDE.md should be discovered, got: {:?}",
            configs
                .iter()
                .map(|c| (&c.file_path, &c.content))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn read_agents_config_claude_subdir_and_direct_both_found() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_root = tmp.path().join("repo");
        fs::create_dir_all(&repo_root).unwrap();
        init_git_repo(&repo_root);

        // Direct CLAUDE.md
        fs::write(repo_root.join("CLAUDE.md"), "# XYZZY_DIRECT_MARKER").unwrap();
        // .claude/CLAUDE.md
        let claude_dir = repo_root.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(claude_dir.join("CLAUDE.md"), "# XYZZY_SUBDIR_MARKER").unwrap();

        let configs = read_agents_config_with_options(
            repo_root.to_str().unwrap(),
            None,
            CompatConfig::default(),
        )
        .await;

        let has_direct = configs
            .iter()
            .any(|c| c.content.contains("XYZZY_DIRECT_MARKER"));
        let has_subdir = configs
            .iter()
            .any(|c| c.content.contains("XYZZY_SUBDIR_MARKER"));

        assert!(has_direct, "Direct CLAUDE.md should be found");
        assert!(has_subdir, ".claude/CLAUDE.md should be found");
    }
}
