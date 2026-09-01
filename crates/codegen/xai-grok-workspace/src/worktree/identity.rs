//! Path-derived identity of a grok-managed worktree.
//!
//! Every creation path resolves its destination to `<grok home>/worktrees/<repo slug>/<label>`, with the label as the last path component.
//! A session cwd anywhere inside a worktree is therefore enough to recover the label.
//! The worktree DB only enriches the result with the recorded source repo.
//! It is a cache, never a dependency, so identity can be stamped on summaries even when the DB is missing or empty.

use std::path::{Path, PathBuf};

/// Identity of the grok-managed worktree containing a cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeIdentity {
    pub label: String,
    pub source_workspace_dir: Option<String>,
}

/// [`worktree_identity_in`] against the default `<grok home>/worktrees`.
pub fn worktree_identity_for_cwd(cwd: &str) -> Option<WorktreeIdentity> {
    worktree_identity_in(&super::grok_home().join("worktrees"), cwd)
}

/// Derive the identity of the worktree containing `cwd`, where worktrees live at `<worktrees_dir>/<slug>/<label>`.
/// `cwd` may be any directory inside one.
/// Returns `None` when `cwd` is not inside a worktree.
pub fn worktree_identity_in(worktrees_dir: &Path, cwd: &str) -> Option<WorktreeIdentity> {
    let cwd_path = Path::new(cwd);
    let cwd_canon = canonical(cwd_path);
    let worktrees_canon = canonical(worktrees_dir);
    let suffix = cwd_canon
        .strip_prefix(&worktrees_canon)
        .ok()
        .or_else(|| cwd_path.strip_prefix(worktrees_dir).ok())?;
    let mut components = suffix.components();
    let slug = components.next()?;
    let label = components.next()?;
    let worktree_root = worktrees_dir.join(slug).join(label);
    let source_workspace_dir = super::source_repo_for_cwd(cwd)
        .or_else(|| standalone_source_marker(&worktree_root))
        .or_else(|| {
            let repo = git2::Repository::open_ext(
                &worktree_root,
                git2::RepositoryOpenFlags::NO_SEARCH,
                &[] as &[&std::ffi::OsStr],
            )
            .ok()?;
            let root = repo.commondir().parent()?.to_path_buf();
            (!canonical(&root).starts_with(canonical(worktrees_dir))).then_some(root)
        })
        .map(|root| root.to_string_lossy().into_owned());
    Some(WorktreeIdentity {
        label: label.as_os_str().to_string_lossy().into_owned(),
        source_workspace_dir,
    })
}

fn standalone_source_marker(worktree_root: &Path) -> Option<PathBuf> {
    let contents =
        std::fs::read_to_string(worktree_root.join(".git").join("grok-worktree-source")).ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn canonical(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod tests;
