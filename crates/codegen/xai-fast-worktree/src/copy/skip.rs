//! Skip logic for copy operations (gitignore + additional patterns).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dashmap::DashSet;
use ignore::{DirEntry, WalkBuilder, WalkState};

/// Build a globset matcher for skip patterns.
pub(crate) fn build_skip_matcher(patterns: &[String]) -> Result<globset::GlobSet> {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(globset::Glob::new(pattern)?);
    }
    Ok(builder.build()?)
}

/// Git emits paths relative to this source directory, including when it is a
/// subdirectory of a linked checkout. NUL framing preserves literal filenames.
fn git_paths(source: &Path, flags: &[&str]) -> Result<Vec<PathBuf>> {
    let output = crate::git::checkout::git_command()
        .current_dir(source)
        .args(["ls-files", "-z"])
        .args(flags)
        .output()
        .context("failed to classify worktree paths with git ls-files")?;
    anyhow::ensure!(
        output.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    output
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| Ok(source.join(gix::path::try_from_byte_slice(s)?)))
        .collect()
}

/// Keep the filesystem walk in charge of visiting physical directory entries.
/// In particular, an indexed directory can be replaced by a symlink outside
/// the checkout, and sparse or deliberately deleted paths can be absent.
pub(crate) fn walk_copy_entries(
    source: &Path,
    parallelism: usize,
    respect_gitignore: bool,
    visit: impl Fn(DirEntry, bool) -> WalkState + Sync,
) -> Result<()> {
    let mut walker = WalkBuilder::new(source);
    walker
        .hidden(false)
        .ignore(respect_gitignore)
        .git_ignore(respect_gitignore)
        .git_global(false)
        .git_exclude(false)
        .threads(parallelism);

    let mut missing = DashSet::new();
    let mut ignored = HashSet::new();
    if respect_gitignore
        && std::path::absolute(source)?
            .ancestors()
            .any(|p| p.join(".git").exists())
    {
        missing.extend(git_paths(source, &["--cached"])?);
        // Let Git resolve effective repository, info and global excludes.
        // --directory collapses wholly ignored subtrees, while a directory
        // with indexed children is represented by its ignored siblings.
        ignored.extend(git_paths(
            source,
            &["--others", "--ignored", "--exclude-standard", "--directory"],
        )?);
    }
    walker
        .filter_entry(move |entry| entry.file_name() != ".git" && !ignored.contains(entry.path()));
    walker.build_parallel().run(|| {
        Box::new(|result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            let tracked = respect_gitignore && missing.remove(entry.path()).is_some();
            visit(entry, tracked)
        })
    });

    if !missing.is_empty() {
        // A .gitignore or .ignore can prune tracked entries. Walk just their
        // physical parent directories, enqueueing existing indexed leaves.
        let leaves: HashSet<_> = missing.into_iter().collect();
        let parents: HashSet<_> = leaves
            .iter()
            .flat_map(|path| path.ancestors().skip(1).map(Path::to_owned))
            .collect();
        let failure = std::sync::Mutex::new(None);
        WalkBuilder::new(source)
            .standard_filters(false)
            .threads(parallelism)
            .filter_entry(move |entry| {
                entry.file_name() != ".git"
                    && (parents.contains(entry.path())
                        && entry.file_type().is_some_and(|t| t.is_dir())
                        || leaves.contains(entry.path()))
            })
            .build_parallel()
            .run(|| {
                Box::new(|result| match result {
                    Ok(entry) if !entry.file_type().is_some_and(|t| t.is_dir()) => {
                        visit(entry, true)
                    }
                    Ok(_) => WalkState::Continue,
                    Err(error) => {
                        failure.lock().unwrap().get_or_insert(error);
                        WalkState::Quit
                    }
                })
            });
        if let Some(error) = failure.into_inner().unwrap() {
            return Err(error).context("failed to walk parents of tracked worktree files");
        }
    }
    Ok(())
}

/// Collect unignored paths so a later ignored-only copy preserves local edits.
pub(crate) fn collect_unignored_paths(
    source: &Path,
    parallelism: usize,
) -> Result<DashSet<PathBuf>> {
    let paths = DashSet::new();
    walk_copy_entries(source, parallelism, true, |entry, _| {
        if let Ok(relative) = entry.path().strip_prefix(source) {
            paths.insert(relative.to_owned());
        }
        WalkState::Continue
    })?;
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use xai_test_utils::git::{git_commit_all, init_git_repo};

    #[cfg(unix)]
    #[test]
    fn ignored_tracked_walk_does_not_follow_replaced_parent() {
        xai_test_utils::require_git!();
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("repo");
        std::fs::create_dir(&source).unwrap();
        init_git_repo(&source);
        let parent = source.join("tracked-dir");
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("tracked"), "fixture").unwrap();
        git_commit_all(&source, "track path");
        std::fs::write(source.join(".gitignore"), "tracked-dir\n").unwrap();
        let elsewhere = temp.path().join("elsewhere");
        std::fs::rename(&parent, &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &parent).unwrap();
        let paths = collect_unignored_paths(&source, 2).unwrap();
        assert!(!paths.contains(Path::new("tracked-dir/tracked")));
    }

    #[test]
    fn collect_unignored_includes_tracked_file_matching_git_exclude() {
        xai_test_utils::require_git!();
        // A tracked file matching `.git/info/exclude` must stay "unignored" so
        // the ignored-copy doesn't re-copy and clobber it.
        let temp = TempDir::new().unwrap();
        let repo = temp.path();
        init_git_repo(repo);

        std::fs::write(repo.join("data.zip"), "tracked-archive").unwrap();
        std::fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(repo.join(".gitignore"), "build/\n").unwrap();
        git_commit_all(repo, "initial");

        // External tooling appends broad patterns here (e.g., *.min.js, *.zip).
        // `git init` does not always create `.git/info/` (the hermetic git on
        // arm64 CI ships no init template), so create it before writing.
        let info_dir = repo.join(".git").join("info");
        std::fs::create_dir_all(&info_dir).unwrap();
        std::fs::write(info_dir.join("exclude"), "*.zip\n").unwrap();

        // A truly-ignored (gitignored, untracked) artifact.
        std::fs::create_dir(repo.join("build")).unwrap();
        std::fs::write(repo.join("build/out.o"), "obj").unwrap();

        let unignored = collect_unignored_paths(repo, 1).unwrap();

        assert!(
            unignored.contains(&PathBuf::from("data.zip")),
            "tracked file matching .git/info/exclude must be classed unignored"
        );
        assert!(unignored.contains(&PathBuf::from("main.rs")));
        assert!(
            !unignored.contains(&PathBuf::from("build/out.o")),
            "a real .gitignore'd file must remain ignored (not unignored)"
        );
    }
}
