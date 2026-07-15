//! Git-based plugin installation.
//!
//! Handles cloning repos, copying local directories into the managed
//! `installed-plugins` snapshot (not live symlinks), discovering plugins within
//! installed sources, and URL/path parsing. Trusted / user-home local installs
//! are re-copied at session spawn / reload by [`super::local_refresh`].

use std::path::{Path, PathBuf};
use std::process::Command;

use super::install_registry::{
    InstallError, InstallKind, InstallRegistry, InstalledRepo, RepoPlugin,
};
use super::manifest::{ManifestLoadResult, load_manifest, name_from_dirname};

/// Source of a plugin installation.
#[derive(Debug, Clone)]
pub enum InstallSource {
    /// Remote git repo — will be cloned.
    Git {
        url: String,
        git_ref: Option<String>,
        git_sha: Option<String>,
        subdir: Option<String>,
    },
    /// Local directory — will be copied into the managed install snapshot.
    Local {
        path: PathBuf,
        subdir: Option<String>,
    },
}

/// Result of installing a source.
pub struct InstallResult {
    pub repo_key: String,
    pub repo_path: PathBuf,
    pub plugins: Vec<DiscoveredPlugin>,
    pub commit: Option<String>,
}

/// A plugin discovered within an installed source.
pub struct DiscoveredPlugin {
    pub name: String,
    pub subdir: Option<String>,
    pub version: Option<String>,
}

/// Parse an install source string into an `InstallSource`.
///
/// Supports:
/// - `https://github.com/user/repo` — Git HTTPS
/// - `https://github.com/user/repo@v1.0.0` — Git with ref
/// - `https://github.com/user/repo#subdir` — Git with subdirectory
/// - `git@github.com:user/repo.git` — Git SSH
/// - `user/repo` — GitHub shorthand (expands to `https://github.com/user/repo`)
/// - `user/repo@v1.0.0` — GitHub shorthand with ref
/// - `user/repo#subdir` — GitHub shorthand with subdirectory
/// - `/path/to/dir` or `./relative` or `~/dir` — Local
/// - `/path/to/dir#subdir` — Local with subdirectory
pub fn parse_install_source(input: &str, cwd: &Path) -> InstallSource {
    // Split on # for subdir
    let (main, subdir) = match input.rsplit_once('#') {
        Some((m, s)) if !s.is_empty() => (m, Some(s.to_string())),
        _ => (input, None),
    };

    // Detect if this is a URL or local path
    if main.contains("://") || main.contains("git@") {
        // Git URL — split on @ for ref (but not the git@ prefix)
        let (url, git_ref) = if main.starts_with("git@") {
            // SSH URL: git@host:user/repo.git@ref
            // The @ in git@ is part of the URL, look for @ after the first :
            if let Some(colon_pos) = main.find(':') {
                let after_colon = &main[colon_pos + 1..];
                if let Some(at_pos) = after_colon.rfind('@') {
                    let url = format!("{}:{}", &main[..colon_pos], &after_colon[..at_pos]);
                    let git_ref = after_colon[at_pos + 1..].to_string();
                    (url, Some(git_ref))
                } else {
                    (main.to_string(), None)
                }
            } else {
                (main.to_string(), None)
            }
        } else {
            // HTTPS URL: https://host/user/repo@ref
            match main.rsplit_once('@') {
                Some((url, r)) if !r.is_empty() => (url.to_string(), Some(r.to_string())),
                _ => (main.to_string(), None),
            }
        };

        InstallSource::Git {
            url,
            git_ref,
            git_sha: None,
            subdir,
        }
    } else if is_github_shorthand(main) {
        // GitHub shorthand: owner/repo or owner/repo@ref
        let (owner_repo, git_ref) = match main.rsplit_once('@') {
            Some((or, r)) if !r.is_empty() => (or, Some(r.to_string())),
            _ => (main, None),
        };
        InstallSource::Git {
            url: format!("https://github.com/{owner_repo}"),
            git_ref,
            git_sha: None,
            subdir,
        }
    } else {
        // Local path
        let path = if let Some(stripped) = main.strip_prefix("~/") {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(stripped)
        } else {
            let p = PathBuf::from(main);
            if p.is_relative() { cwd.join(p) } else { p }
        };

        InstallSource::Local { path, subdir }
    }
}

/// Check if a string looks like a GitHub `owner/repo` shorthand.
///
/// Returns `true` for strings like `user/repo` or `user/repo@v1.0`
/// that should be expanded to `https://github.com/user/repo`.
/// Avoids false positives for local paths (`/abs`, `./rel`, `~/home`).
fn is_github_shorthand(s: &str) -> bool {
    // Local path indicators.
    if s.starts_with('/') || s.starts_with('.') || s.starts_with('~') {
        return false;
    }
    // Strip optional @ref suffix before checking the owner/repo pattern.
    let base = match s.rsplit_once('@') {
        Some((b, r)) if !r.is_empty() => b,
        _ => s,
    };
    // Must be exactly owner/repo (two non-empty segments separated by one /).
    let parts: Vec<&str> = base.splitn(3, '/').collect();
    parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty()
}

fn repo_source_id(source: &InstallSource) -> String {
    match source {
        InstallSource::Git { url, subdir, .. } => match subdir {
            Some(sub) => format!("{url}#{sub}"),
            None => url.clone(),
        },
        InstallSource::Local { path, subdir } => {
            let path_str = path.to_str().unwrap_or("local");
            match subdir {
                Some(sub) => format!("{path_str}#{sub}"),
                None => path_str.to_string(),
            }
        }
    }
}

/// Install a plugin source (clone or symlink) and discover plugins.
pub fn install_from_source(
    source: &InstallSource,
    registry: &InstallRegistry,
) -> Result<InstallResult, InstallError> {
    let source_id = repo_source_id(source);
    let repo_key = InstallRegistry::repo_key(&source_id);

    // Check if already installed
    if registry.get_repo(&repo_key).is_some() {
        return Err(InstallError::AlreadyInstalled { key: repo_key });
    }

    let install_dir = registry.install_dir().to_path_buf();
    std::fs::create_dir_all(&install_dir).map_err(|e| InstallError::Io {
        path: install_dir.clone(),
        source: e,
    })?;

    let repo_path = install_dir.join(&repo_key);

    let (_kind, commit) = match source {
        InstallSource::Git {
            url,
            git_ref,
            git_sha,
            subdir,
        } => {
            clone_repo(url, git_ref.as_deref(), git_sha.as_deref(), &repo_path)?;
            let commit = read_head_commit(&repo_path);
            let kind = InstallKind::Git {
                url: url.clone(),
                git_ref: git_ref.clone(),
                commit: commit.clone().unwrap_or_default(),
                subdir: subdir.clone(),
            };
            (kind, commit)
        }
        InstallSource::Local { path, subdir } => {
            if !path.is_dir() {
                return Err(InstallError::InstallFailed {
                    detail: format!("local path is not a directory: {}", path.display()),
                });
            }
            // Deliberate full copy (not a symlink): isolates the install from
            // later source edits/deletion and keeps uninstall a simple dir remove;
            // local_refresh re-copies trusted sources to pick up new components.
            copy_dir_recursive(path, &repo_path).map_err(|e| InstallError::Io {
                path: repo_path.clone(),
                source: e,
            })?;
            let kind = InstallKind::Local {
                source_path: path.clone(),
                subdir: subdir.clone(),
            };
            (kind, None)
        }
    };

    // Discover plugins
    let subdir = match source {
        InstallSource::Git { subdir, .. } | InstallSource::Local { subdir, .. } => subdir.clone(),
    };
    let plugins = discover_plugins_in_dir(&repo_path, subdir.as_deref())?;

    if plugins.is_empty() {
        // Clean up — no valid plugins found
        let _ = remove_repo_path(&repo_path);
        return Err(InstallError::InstallFailed {
            detail: "no plugins found in the source (no plugin.json or convention components)"
                .to_string(),
        });
    }

    Ok(InstallResult {
        repo_key,
        repo_path,
        plugins,
        commit,
    })
}

/// Clone a git repo using the `git` CLI (supports shallow clone, SSH, etc.;
/// optionally SHA-pinned via `git_sha`).
fn clone_repo(
    url: &str,
    git_ref: Option<&str>,
    git_sha: Option<&str>,
    target: &Path,
) -> Result<(), InstallError> {
    if let Some(sha) = git_sha {
        if git_ref.is_some() {
            tracing::debug!(?git_ref, sha, "git_sha takes precedence over git_ref");
        }
        return clone_repo_at_sha(url, sha, target);
    }

    let mut cmd = Command::new("git");
    xai_grok_tools::util::detach_std_command(&mut cmd);
    cmd.arg("clone").arg("--depth").arg("1");
    cmd.stdin(std::process::Stdio::null());
    cmd.envs(xai_grok_tools::util::pager_env());

    if let Some(r) = git_ref {
        cmd.arg("--branch").arg(r);
    }

    cmd.arg(url).arg(target);

    tracing::info!(url = url, target = %target.display(), "cloning plugin repo");

    let output = cmd.output().map_err(|e| InstallError::InstallFailed {
        detail: format!("failed to run git clone: {e}"),
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Clean up partial clone
        let _ = std::fs::remove_dir_all(target);
        return Err(InstallError::InstallFailed {
            detail: format!(
                "git clone failed (exit {}):\n{stderr}",
                output.status.code().unwrap_or(-1)
            ),
        });
    }

    Ok(())
}

fn clone_repo_at_sha(url: &str, sha: &str, target: &Path) -> Result<(), InstallError> {
    if sha.is_empty() {
        return Err(InstallError::InstallFailed {
            detail: "empty SHA provided for pinned clone".into(),
        });
    }

    tracing::info!(url = url, sha = sha, target = %target.display(), "cloning plugin repo at SHA");

    std::fs::create_dir_all(target).map_err(|e| InstallError::Io {
        path: target.to_path_buf(),
        source: e,
    })?;

    let wrap_fail = |detail: String| {
        let _ = std::fs::remove_dir_all(target);
        InstallError::InstallFailed { detail }
    };

    run_git_in(target, &["init", "--quiet"]).map_err(wrap_fail)?;
    run_git_in(target, &["remote", "add", "origin", url]).map_err(wrap_fail)?;
    run_git_in(target, &["fetch", "--depth", "1", "origin", sha])
        .map_err(|d| wrap_fail(format!("fetch-by-sha failed: {d}")))?;
    run_git_in(target, &["checkout", "--quiet", "FETCH_HEAD"]).map_err(wrap_fail)?;

    let head = read_head_commit(target).ok_or_else(|| {
        let _ = std::fs::remove_dir_all(target);
        InstallError::InstallFailed {
            detail: "could not read HEAD commit after SHA-pinned clone".into(),
        }
    })?;
    if !head.eq_ignore_ascii_case(sha) {
        let _ = std::fs::remove_dir_all(target);
        return Err(InstallError::ShaMismatch {
            expected: sha.to_string(),
            actual: head,
        });
    }

    Ok(())
}

fn run_git_in(cwd: &Path, args: &[&str]) -> Result<(), String> {
    run_git_in_capture(cwd, args).map(|_| ())
}

fn run_git_in_capture(cwd: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    let mut cmd = Command::new("git");
    xai_grok_tools::util::detach_std_command(&mut cmd);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .envs(xai_grok_tools::util::pager_env());
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args.first().unwrap_or(&"")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git {} failed (exit {}):\n{stderr}",
            args.first().unwrap_or(&""),
            output.status.code().unwrap_or(-1)
        ));
    }
    Ok(output)
}

/// Read the HEAD commit SHA from a git repo.
fn read_head_commit(repo_path: &Path) -> Option<String> {
    run_git_in_capture(repo_path, &["rev-parse", "HEAD"])
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Remove a repo path (handles both symlinks and directories).
pub fn remove_repo_path(path: &Path) -> Result<(), InstallError> {
    if path.is_symlink() {
        std::fs::remove_file(path).map_err(|e| InstallError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    } else if path.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| InstallError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    }
    Ok(())
}

/// Clean up plugin data directories for all plugins in a repo.
///
/// Each plugin has a data dir at `~/.grok/plugin-data/<plugin_id>/`.
/// This iterates all plugins in the repo and removes their data dirs.
pub fn cleanup_plugin_data(repo: &InstalledRepo, scope: super::discovery::PluginScope) {
    let plugin_data_base = xai_grok_config::grok_home().join("plugin-data");

    for (plugin_name, repo_plugin) in &repo.plugins {
        let plugin_root = match &repo_plugin.subdir {
            Some(sub) => repo.path.join(sub),
            None => repo.path.clone(),
        };
        let id = super::discovery::PluginId::new(scope, &plugin_root, plugin_name);
        let data_dir = plugin_data_base.join(&id.0);
        if data_dir.is_dir() {
            tracing::info!(
                plugin = plugin_name,
                data_dir = %data_dir.display(),
                "cleaning up plugin data directory"
            );
            let _ = std::fs::remove_dir_all(&data_dir);
        }
    }
}

/// Discover plugins within a directory.
///
/// Discovery logic:
/// 1. If `subdir` is specified, only look in that subdirectory
/// 2. If root has plugin.json or convention components, it's a single plugin
/// 3. Otherwise, scan immediate subdirectories for plugins
pub(super) fn discover_plugins_in_dir(
    root: &Path,
    subdir: Option<&str>,
) -> Result<Vec<DiscoveredPlugin>, InstallError> {
    let scan_root = match subdir {
        Some(s) => {
            let candidate = Path::new(s);
            if candidate.is_absolute()
                || candidate.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(InstallError::InstallFailed {
                    detail: format!("subdirectory '{s}' escapes the source root"),
                });
            }
            let sub_path = root.join(s);
            if !sub_path.is_dir() {
                return Err(InstallError::InstallFailed {
                    detail: format!("subdirectory '{s}' not found in source"),
                });
            }
            sub_path
        }
        None => root.to_path_buf(),
    };

    // Check if scan_root itself is a plugin
    if let Some(plugin) = try_load_plugin(&scan_root, subdir) {
        return Ok(vec![plugin]);
    }

    // Scan immediate subdirectories
    let mut plugins = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&scan_root) {
        for entry in entries.flatten() {
            if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                let entry_path = entry.path();
                let entry_name = entry.file_name().to_str().unwrap_or_default().to_string();
                let sub = match subdir {
                    Some(s) => Some(format!("{s}/{entry_name}")),
                    None => Some(entry_name),
                };
                if let Some(plugin) = try_load_plugin(&entry_path, sub.as_deref()) {
                    plugins.push(plugin);
                }
            }
        }
    }

    Ok(plugins)
}

/// Try to load a plugin from a directory.
///
/// Returns `Some` if the directory contains a plugin.json or convention components.
fn try_load_plugin(dir: &Path, subdir: Option<&str>) -> Option<DiscoveredPlugin> {
    // Try loading manifest
    match load_manifest(dir) {
        Ok(ManifestLoadResult::Found(manifest)) => {
            return Some(DiscoveredPlugin {
                name: manifest.name.clone(),
                subdir: subdir.map(|s| s.to_string()),
                version: manifest.version.clone(),
            });
        }
        Ok(ManifestLoadResult::NotFound) => {}
        Err(_) => {}
    }

    // Check convention components
    let has_skills = dir.join("skills").is_dir();
    let has_agents = dir.join("agents").is_dir();
    let has_mcp = dir.join(".mcp.json").is_file();
    let has_hooks = dir.join("hooks").join("hooks.json").is_file();

    if has_skills || has_agents || has_mcp || has_hooks {
        let name = name_from_dirname(dir)?;
        Some(DiscoveredPlugin {
            name,
            subdir: subdir.map(|s| s.to_string()),
            version: None,
        })
    } else {
        None
    }
}

/// Build an `InstalledRepo` from an install result and the original source.
pub fn build_installed_repo(result: &InstallResult, source: &InstallSource) -> InstalledRepo {
    let kind = match source {
        InstallSource::Git {
            url,
            git_ref,
            git_sha,
            subdir,
        } => InstallKind::Git {
            url: url.clone(),
            git_ref: git_sha.clone().or_else(|| git_ref.clone()),
            commit: result.commit.clone().unwrap_or_default(),
            subdir: subdir.clone(),
        },
        InstallSource::Local { path, subdir } => InstallKind::Local {
            source_path: path.clone(),
            subdir: subdir.clone(),
        },
    };
    let now = chrono::Utc::now().to_rfc3339();
    InstalledRepo {
        kind,
        installed_at: now.clone(),
        updated_at: now,
        path: result.repo_path.clone(),
        plugins: repo_plugin_map(&result.plugins),
        marketplace: None,
    }
}

/// Build the `InstalledRepo.plugins` map from discovered plugins.
pub fn repo_plugin_map(
    plugins: &[DiscoveredPlugin],
) -> std::collections::HashMap<String, RepoPlugin> {
    plugins
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                RepoPlugin {
                    subdir: p.subdir.clone(),
                    version: p.version.clone(),
                },
            )
        })
        .collect()
}

/// Result of updating a repo.
pub struct UpdateResult {
    pub repo_key: String,
    pub old_commit: Option<String>,
    pub new_commit: Option<String>,
    pub changed: bool,
    pub plugins: Vec<DiscoveredPlugin>,
}

/// Status of an update attempt.
pub enum UpdateStatus {
    /// Repo was updated successfully.
    Updated(UpdateResult),
    /// Repo is pinned to a tag or commit — no automatic update.
    Pinned { ref_name: String },
    /// Local install — explicit update is a no-op; session spawn / reload uses
    /// [`super::local_refresh`] to re-copy trusted sources (install is a full
    /// directory copy, not a live symlink).
    LiveLocal,
}

/// Update an installed repo by fetching latest changes.
///
/// Update semantics (from design decision #6):
/// - Branch installs: `git fetch` + fast-forward to remote branch head
/// - Tag installs: pinned — no-op
/// - Commit installs: pinned — no-op
/// - Local installs: no-op (explicit update); [`super::local_refresh`] re-copies on session spawn / reload
pub fn update_repo(repo_key: &str, repo: &InstalledRepo) -> Result<UpdateStatus, InstallError> {
    match &repo.kind {
        InstallKind::Local { .. } => Ok(UpdateStatus::LiveLocal),
        InstallKind::Git {
            git_ref,
            commit,
            subdir,
            ..
        } => {
            // Check if pinned
            if let Some(r) = git_ref {
                // Heuristic: if the ref looks like a commit hash (40 hex chars)
                // or a version tag (starts with v and contains dots), it's pinned.
                let is_tag_or_commit = r.len() == 40 && r.chars().all(|c| c.is_ascii_hexdigit())
                    || (r.starts_with('v') && r.contains('.'));
                if is_tag_or_commit {
                    return Ok(UpdateStatus::Pinned {
                        ref_name: r.clone(),
                    });
                }
            }

            let old_commit = Some(commit.clone());

            // Git fetch + pull
            let repo_path = &repo.path;
            if !repo_path.is_dir() {
                return Err(InstallError::InstallFailed {
                    detail: format!("repo directory not found: {}", repo_path.display()),
                });
            }

            let mut cmd = Command::new("git");
            xai_grok_tools::util::detach_std_command(&mut cmd);
            cmd.args(["pull", "--ff-only"])
                .current_dir(repo_path)
                .stdin(std::process::Stdio::null())
                .envs(xai_grok_tools::util::pager_env());
            let output = cmd.output().map_err(|e| InstallError::InstallFailed {
                detail: format!("failed to run git pull: {e}"),
            })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(InstallError::InstallFailed {
                    detail: format!(
                        "git pull failed (exit {}):\n{stderr}",
                        output.status.code().unwrap_or(-1)
                    ),
                });
            }

            let new_commit = read_head_commit(repo_path);
            let changed = old_commit.as_deref() != new_commit.as_deref();

            // Re-discover plugins (new ones may have been added)
            let plugins = discover_plugins_in_dir(repo_path, subdir.as_deref())?;

            Ok(UpdateStatus::Updated(UpdateResult {
                repo_key: repo_key.to_string(),
                old_commit,
                new_commit,
                changed,
                plugins,
            }))
        }
    }
}

/// Recursively copy a directory tree.
///
/// Symlinks are **not** followed: directory symlinks are skipped (not recursed),
/// file symlinks are skipped (not materialized as target contents). This keeps
/// refresh/install snapshots from pulling secrets outside the source tree.
/// Shared by install and refresh, so the skip applies to install-time copies too.
pub(super) fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&src_path)?;
        if meta.file_type().is_symlink() {
            tracing::debug!(path = %src_path.display(), "copy_dir_recursive: skipping symlink (not followed)");
            continue;
        }
        if meta.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if meta.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_key_distinct_per_git_subdir_and_bare_unchanged() {
        let url = "https://github.com/acme/agent-skills.git";
        let bare = InstallSource::Git {
            url: url.to_string(),
            git_ref: None,
            git_sha: None,
            subdir: None,
        };
        let acme = InstallSource::Git {
            url: url.to_string(),
            git_ref: None,
            git_sha: None,
            subdir: Some("plugins/acme".to_string()),
        };
        let cloud = InstallSource::Git {
            url: url.to_string(),
            git_ref: None,
            git_sha: None,
            subdir: Some("plugins/acme-cloud".to_string()),
        };

        let key_bare = InstallRegistry::repo_key(&repo_source_id(&bare));
        let key_acme = InstallRegistry::repo_key(&repo_source_id(&acme));
        let key_cloud = InstallRegistry::repo_key(&repo_source_id(&cloud));

        assert_ne!(key_acme, key_cloud);
        assert_ne!(key_bare, key_acme);
        assert_eq!(key_bare, InstallRegistry::repo_key(url));
    }

    #[test]
    fn repo_key_distinct_per_local_subdir_and_bare_unchanged() {
        let path_str = "/Users/me/workspace";
        let bare = InstallSource::Local {
            path: PathBuf::from(path_str),
            subdir: None,
        };
        let acme = InstallSource::Local {
            path: PathBuf::from(path_str),
            subdir: Some("plugins/acme".to_string()),
        };
        let cloud = InstallSource::Local {
            path: PathBuf::from(path_str),
            subdir: Some("plugins/acme-cloud".to_string()),
        };

        let key_bare = InstallRegistry::repo_key(&repo_source_id(&bare));
        let key_acme = InstallRegistry::repo_key(&repo_source_id(&acme));
        let key_cloud = InstallRegistry::repo_key(&repo_source_id(&cloud));

        assert_ne!(key_acme, key_cloud);
        assert_ne!(key_bare, key_acme);
        assert_eq!(key_bare, InstallRegistry::repo_key(path_str));
    }

    #[test]
    fn parse_https_url() {
        let source = parse_install_source("https://github.com/user/repo", Path::new("/tmp"));
        match source {
            InstallSource::Git {
                url,
                git_ref,
                git_sha,
                subdir,
            } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert!(git_ref.is_none());
                assert!(git_sha.is_none());
                assert!(subdir.is_none());
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_https_url_with_ref() {
        let source = parse_install_source("https://github.com/user/repo@v1.0.0", Path::new("/tmp"));
        match source {
            InstallSource::Git { url, git_ref, .. } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert_eq!(git_ref.as_deref(), Some("v1.0.0"));
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_https_url_with_subdir() {
        let source =
            parse_install_source("https://github.com/user/repo#my-plugin", Path::new("/tmp"));
        match source {
            InstallSource::Git { url, subdir, .. } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert_eq!(subdir.as_deref(), Some("my-plugin"));
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_https_url_with_ref_and_subdir() {
        let source = parse_install_source(
            "https://github.com/user/repo@main#deploy",
            Path::new("/tmp"),
        );
        match source {
            InstallSource::Git {
                url,
                git_ref,
                subdir,
                ..
            } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert_eq!(git_ref.as_deref(), Some("main"));
                assert_eq!(subdir.as_deref(), Some("deploy"));
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_ssh_url() {
        let source = parse_install_source("git@github.com:user/my-plugin.git", Path::new("/tmp"));
        match source {
            InstallSource::Git {
                url,
                git_ref,
                subdir,
                ..
            } => {
                assert_eq!(url, "git@github.com:user/my-plugin.git");
                assert!(git_ref.is_none());
                assert!(subdir.is_none());
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_github_shorthand() {
        let source = parse_install_source("user/repo", Path::new("/tmp"));
        match source {
            InstallSource::Git {
                url,
                git_ref,
                subdir,
                ..
            } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert!(git_ref.is_none());
                assert!(subdir.is_none());
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_github_shorthand_with_ref() {
        let source = parse_install_source("user/repo@v1.0.0", Path::new("/tmp"));
        match source {
            InstallSource::Git { url, git_ref, .. } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert_eq!(git_ref.as_deref(), Some("v1.0.0"));
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_github_shorthand_with_subdir() {
        let source = parse_install_source("user/repo#my-plugin", Path::new("/tmp"));
        match source {
            InstallSource::Git { url, subdir, .. } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert_eq!(subdir.as_deref(), Some("my-plugin"));
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_github_shorthand_with_ref_and_subdir() {
        let source = parse_install_source("user/repo@main#deploy", Path::new("/tmp"));
        match source {
            InstallSource::Git {
                url,
                git_ref,
                subdir,
                ..
            } => {
                assert_eq!(url, "https://github.com/user/repo");
                assert_eq!(git_ref.as_deref(), Some("main"));
                assert_eq!(subdir.as_deref(), Some("deploy"));
            }
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn parse_github_shorthand_not_deep_path() {
        // Three segments like "a/b/c" should be treated as a local path, not shorthand.
        let source = parse_install_source("a/b/c", Path::new("/tmp"));
        match source {
            InstallSource::Local { .. } => {}
            _ => panic!("expected Local for deep relative path"),
        }
    }

    #[test]
    fn parse_local_absolute_path() {
        let source = parse_install_source("/home/user/my-plugin", Path::new("/tmp"));
        match source {
            InstallSource::Local { path, subdir } => {
                assert_eq!(path, PathBuf::from("/home/user/my-plugin"));
                assert!(subdir.is_none());
            }
            _ => panic!("expected Local"),
        }
    }

    #[test]
    fn parse_local_relative_path() {
        let source = parse_install_source("./my-plugin", Path::new("/projects"));
        match source {
            InstallSource::Local { path, subdir } => {
                assert_eq!(path, PathBuf::from("/projects/my-plugin"));
                assert!(subdir.is_none());
            }
            _ => panic!("expected Local"),
        }
    }

    #[test]
    fn parse_local_with_subdir() {
        let source = parse_install_source("/home/user/workspace#my-plugin", Path::new("/tmp"));
        match source {
            InstallSource::Local { path, subdir } => {
                assert_eq!(path, PathBuf::from("/home/user/workspace"));
                assert_eq!(subdir.as_deref(), Some("my-plugin"));
            }
            _ => panic!("expected Local"),
        }
    }

    #[test]
    fn discover_single_plugin_at_root() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path();
        std::fs::create_dir_all(plugin_dir.join("skills")).unwrap();

        let plugins = discover_plugins_in_dir(plugin_dir, None).unwrap();
        assert_eq!(plugins.len(), 1);
    }

    #[test]
    fn discover_multiple_plugins_in_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Create two plugin subdirectories
        std::fs::create_dir_all(root.join("linter/skills")).unwrap();
        std::fs::create_dir_all(root.join("formatter/agents")).unwrap();
        // Non-plugin dir (should be ignored)
        std::fs::create_dir_all(root.join("docs")).unwrap();

        let plugins = discover_plugins_in_dir(root, None).unwrap();
        assert_eq!(plugins.len(), 2);

        let names: Vec<_> = plugins.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"linter"));
        assert!(names.contains(&"formatter"));
    }

    #[test]
    fn discover_with_subdir_selector() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        std::fs::create_dir_all(root.join("packages/plugin-a/skills")).unwrap();
        std::fs::create_dir_all(root.join("packages/plugin-b/skills")).unwrap();

        // Only discover plugin-a
        let plugins = discover_plugins_in_dir(root, Some("packages/plugin-a")).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "plugin-a");
    }

    #[test]
    fn discover_plugins_in_dir_rejects_escaping_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in ["../escape", "/etc"] {
            match discover_plugins_in_dir(tmp.path(), Some(bad)) {
                Err(InstallError::InstallFailed { detail }) => {
                    assert!(detail.contains("escapes"), "got detail: {detail}");
                }
                Err(e) => panic!("expected escapes InstallFailed, got: {e:?}"),
                Ok(_) => panic!("expected escapes error for subdir {bad:?}, got Ok"),
            }
        }
    }

    #[test]
    fn discover_with_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name": "my-tool", "version": "1.2.0"}"#,
        )
        .unwrap();

        let plugins = discover_plugins_in_dir(plugin_dir, None).unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "my-tool");
        assert_eq!(plugins[0].version.as_deref(), Some("1.2.0"));
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn run_git_test(cwd: &Path, args: &[&str]) {
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .stdin(std::process::Stdio::null());
        let output = cmd.output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn make_local_repo() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        run_git_test(repo, &["init", "--initial-branch=main", "--quiet"]);
        std::fs::write(repo.join("plugin.json"), r#"{"name":"pinned-plugin"}"#).unwrap();
        run_git_test(repo, &["add", "plugin.json"]);
        run_git_test(repo, &["commit", "-m", "initial", "--quiet"]);
        run_git_test(repo, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        let sha = read_head_commit(repo).expect("read HEAD of local repo");
        (tmp, sha)
    }

    #[test]
    fn clone_at_correct_sha_succeeds() {
        if !git_available() {
            eprintln!("skipping: `git` binary not available in test sandbox");
            return;
        }
        let (repo, sha) = make_local_repo();
        let dest = tempfile::tempdir().unwrap();
        let url = format!("file://{}", repo.path().display());

        clone_repo_at_sha(&url, &sha, dest.path()).expect("clone at correct sha should succeed");

        assert_eq!(read_head_commit(dest.path()).as_deref(), Some(sha.as_str()));
        assert!(dest.path().join("plugin.json").exists());
    }

    #[test]
    fn clone_at_wrong_sha_returns_mismatch() {
        if !git_available() {
            eprintln!("skipping: `git` binary not available in test sandbox");
            return;
        }
        let (repo, _real_sha) = make_local_repo();
        let bogus_sha = "0000000000000000000000000000000000000000";
        let dest = tempfile::tempdir().unwrap();
        let url = format!("file://{}", repo.path().display());

        let err = clone_repo_at_sha(&url, bogus_sha, dest.path())
            .expect_err("clone at bogus sha should fail");

        match err {
            InstallError::InstallFailed { detail } => {
                assert!(
                    detail.contains("fetch-by-sha"),
                    "expected fetch-by-sha error, got: {detail}"
                );
            }
            other => panic!("expected InstallFailed, got: {other:?}"),
        }
        assert!(!dest.path().join(".git").exists());
    }

    #[test]
    fn clone_at_sha_handles_short_sha_via_mismatch() {
        if !git_available() {
            eprintln!("skipping: `git` binary not available in test sandbox");
            return;
        }
        let (repo, sha) = make_local_repo();
        let short = &sha[..7];
        let dest = tempfile::tempdir().unwrap();
        let url = format!("file://{}", repo.path().display());

        let err = clone_repo_at_sha(&url, short, dest.path())
            .expect_err("short sha should fail verification");

        match err {
            InstallError::ShaMismatch { expected, actual } => {
                assert_eq!(expected, short);
                assert_eq!(actual, sha);
            }
            InstallError::InstallFailed { .. } => {}
            other => panic!("expected ShaMismatch or InstallFailed, got: {other:?}"),
        }
    }

    #[test]
    fn update_repo_rediscovers_plugin_at_stored_subdir() {
        if !git_available() {
            eprintln!("skipping: `git` binary not available in test sandbox");
            return;
        }
        use std::collections::HashMap;

        let origin = tempfile::tempdir().unwrap();
        run_git_test(origin.path(), &["init", "--initial-branch=main", "--quiet"]);
        let plugin_dir = origin.path().join("plugins").join("acme");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"acme","version":"1.0.0"}"#,
        )
        .unwrap();
        run_git_test(origin.path(), &["add", "-A"]);
        run_git_test(origin.path(), &["commit", "-m", "init", "--quiet"]);

        let install = tempfile::tempdir().unwrap();
        let repo_path = install.path().join("acme-deadbeef");
        let origin_url = format!("file://{}", origin.path().display());
        run_git_test(
            install.path(),
            &[
                "clone",
                "--quiet",
                origin_url.as_str(),
                repo_path.to_str().unwrap(),
            ],
        );

        let commit = read_head_commit(&repo_path).expect("read HEAD of clone");
        let repo = InstalledRepo {
            kind: InstallKind::Git {
                url: origin_url,
                git_ref: Some("main".to_string()),
                commit,
                subdir: Some("plugins/acme".to_string()),
            },
            installed_at: String::new(),
            updated_at: String::new(),
            path: repo_path,
            plugins: HashMap::from([(
                "acme".to_string(),
                RepoPlugin {
                    subdir: Some("plugins/acme".to_string()),
                    version: Some("1.0.0".to_string()),
                },
            )]),
            marketplace: None,
        };

        match update_repo("acme-deadbeef", &repo).expect("update should succeed") {
            UpdateStatus::Updated(result) => {
                assert_eq!(result.plugins.len(), 1);
                assert_eq!(result.plugins[0].name, "acme");
                assert_eq!(result.plugins[0].subdir.as_deref(), Some("plugins/acme"));
            }
            _ => panic!("expected UpdateStatus::Updated"),
        }
    }

    #[test]
    fn update_repo_errors_when_stored_subdir_missing() {
        if !git_available() {
            eprintln!("skipping: `git` binary not available in test sandbox");
            return;
        }
        use std::collections::HashMap;

        let origin = tempfile::tempdir().unwrap();
        run_git_test(origin.path(), &["init", "--initial-branch=main", "--quiet"]);
        let plugin_dir = origin.path().join("plugins").join("acme");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("plugin.json"),
            r#"{"name":"acme","version":"1.0.0"}"#,
        )
        .unwrap();
        run_git_test(origin.path(), &["add", "-A"]);
        run_git_test(origin.path(), &["commit", "-m", "init", "--quiet"]);

        let install = tempfile::tempdir().unwrap();
        let repo_path = install.path().join("acme-deadbeef");
        let origin_url = format!("file://{}", origin.path().display());
        run_git_test(
            install.path(),
            &[
                "clone",
                "--quiet",
                origin_url.as_str(),
                repo_path.to_str().unwrap(),
            ],
        );

        let commit = read_head_commit(&repo_path).expect("read HEAD of clone");
        let repo = InstalledRepo {
            kind: InstallKind::Git {
                url: origin_url,
                git_ref: Some("main".to_string()),
                commit,
                subdir: Some("plugins/does-not-exist".to_string()),
            },
            installed_at: String::new(),
            updated_at: String::new(),
            path: repo_path,
            plugins: HashMap::from([(
                "acme".to_string(),
                RepoPlugin {
                    subdir: Some("plugins/does-not-exist".to_string()),
                    version: Some("1.0.0".to_string()),
                },
            )]),
            marketplace: None,
        };

        match update_repo("acme-deadbeef", &repo) {
            Err(InstallError::InstallFailed { .. }) => {}
            Err(e) => panic!("expected InstallFailed, got {e:?}"),
            Ok(_) => panic!("expected InstallFailed when stored subdir is missing, got Ok"),
        }
    }
}
