//! Git worktree operations.

use std::path::Path;

use anyhow::{Context, Result};

use crate::git::checkout::git_command;

/// Create a git worktree with `--no-checkout`. Blocking.
pub(crate) fn worktree_add_no_checkout(source: &Path, dest: &str, git_ref: &str) -> Result<()> {
    let output = git_command()
        .current_dir(source)
        .args([
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            dest,
            git_ref,
        ])
        .output()
        .context("failed to run git worktree add")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree add failed: {}", stderr);
    }

    Ok(())
}
