//! Skip logic for copy operations (gitignore + additional patterns).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dashmap::{DashMap, DashSet};
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
        .map(|s| Ok(gix::path::try_from_byte_slice(s)?.to_owned()))
        .collect()
}

/// Keep the filesystem walk in charge of visiting physical directory entries.
/// In particular, an indexed directory can be replaced by a symlink outside
/// the checkout, and sparse or deliberately deleted paths can be absent.
pub(crate) fn walk_copy_entries(
    source: &Path,
    parallelism: usize,
    respect_gitignore: bool,
    visit: impl Fn(DirEntry, Option<&Path>) -> WalkState + Sync,
) -> Result<()> {
    let mut walker = WalkBuilder::new(source);
    walker
        .hidden(false)
        .ignore(respect_gitignore)
        .git_ignore(respect_gitignore)
        .git_global(false)
        .git_exclude(false)
        .threads(parallelism);

    let missing = DashMap::new();
    let mut ignored = HashSet::new();
    if respect_gitignore
        && std::path::absolute(source)?
            .ancestors()
            .any(|p| p.join(".git").exists())
    {
        let mut spellings = super::disk_spelling::DiskSpellings::new(source)?;
        for indexed in git_paths(source, &["--cached"])? {
            if let Some(physical) = spellings.resolve(&indexed)? {
                let entry = missing
                    .entry(source.join(physical))
                    .or_insert_with(|| indexed.clone());
                anyhow::ensure!(
                    *entry == indexed,
                    "two Git index paths resolve to one physical worktree entry: {} and {}",
                    entry.display(),
                    indexed.display()
                );
            }
        }
        // Let Git resolve effective repository, info and global excludes.
        // --directory collapses wholly ignored subtrees, while a directory
        // with indexed children is represented by its ignored siblings.
        for excluded in git_paths(
            source,
            &["--others", "--ignored", "--exclude-standard", "--directory"],
        )? {
            if let Some(physical) = spellings.resolve(&excluded)? {
                ignored.insert(source.join(physical));
            }
        }
    }
    walker
        .filter_entry(move |entry| entry.file_name() != ".git" && !ignored.contains(entry.path()));
    walker.build_parallel().run(|| {
        Box::new(|result| {
            let Ok(entry) = result else {
                return WalkState::Continue;
            };
            let tracked = missing.remove(entry.path());
            visit(
                entry,
                tracked.as_ref().map(|(_, indexed)| indexed.as_path()),
            )
        })
    });

    if !missing.is_empty() {
        // A .gitignore or .ignore can prune tracked entries. Walk just their
        // physical parent directories, enqueueing existing indexed leaves.
        let leaves: HashMap<_, _> = missing.into_iter().collect();
        let parents: HashSet<_> = leaves
            .keys()
            .flat_map(|path| path.ancestors().skip(1).map(Path::to_owned))
            .collect();
        let failure = std::sync::Mutex::new(None);
        let leaves = std::sync::Arc::new(leaves);
        let indexed = std::sync::Arc::clone(&leaves);
        WalkBuilder::new(source)
            .standard_filters(false)
            .threads(parallelism)
            .filter_entry(move |entry| {
                entry.file_name() != ".git"
                    && (parents.contains(entry.path())
                        && entry.file_type().is_some_and(|t| t.is_dir())
                        || indexed.contains_key(entry.path()))
            })
            .build_parallel()
            .run(|| {
                Box::new(|result| match result {
                    Ok(entry) if !entry.file_type().is_some_and(|t| t.is_dir()) => {
                        let index_path = leaves.get(entry.path()).map(PathBuf::as_path);
                        visit(entry, index_path)
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

    #[test]
    fn tracked_files_use_physical_spelling_without_copying_hardlink_aliases() {
        use crate::copy::{ParallelCopyConfig, copy_parallel};
        use std::ffi::OsStr;
        use tokio_util::sync::CancellationToken;
        use xai_test_utils::git::run_git;

        xai_test_utils::require_git!();
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        let dest = temp.path().join("dest");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&dest).unwrap();
        init_git_repo(&repo);
        run_git(&repo, &["config", "core.precomposeunicode", "true"]);
        std::fs::create_dir(repo.join("Dir")).unwrap();
        let nfc = "caf\u{e9}.txt";
        let nfd = "cafe\u{301}.txt";
        std::fs::write(repo.join("Dir/lower.txt"), "old").unwrap();
        std::fs::write(repo.join("Dir").join(nfc), "old").unwrap();
        git_commit_all(&repo, "indexed names");

        std::fs::rename(repo.join("Dir"), repo.join("tmp-name")).unwrap();
        std::fs::rename(repo.join("tmp-name"), repo.join("DIR")).unwrap();
        let dir = repo.join("DIR");
        for (old, new) in [("lower.txt", "LOWER.txt"), (nfc, nfd)] {
            std::fs::rename(dir.join(old), dir.join("tmp-name")).unwrap();
            std::fs::rename(dir.join("tmp-name"), dir.join(new)).unwrap();
            std::fs::write(dir.join(new), "working edit").unwrap();
        }
        std::fs::hard_link(dir.join("LOWER.txt"), dir.join("alias.txt")).unwrap();
        std::fs::write(repo.join(".gitignore"), "Dir/\nDIR/\n").unwrap();
        let config = ParallelCopyConfig {
            respect_gitignore: true,
            num_workers: 2,
            channel_buffer: 16,
            ..Default::default()
        };
        copy_parallel(&repo, &dest, config.clone(), CancellationToken::new()).unwrap();

        if !repo.join("Dir/lower.txt").exists() {
            // On a case-sensitive volume these are genuinely deleted index
            // entries with new, ignored untracked names.
            assert!(!dest.join("DIR").exists());
            return;
        }
        let copied: HashSet<_> = std::fs::read_dir(dest.join("DIR"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(copied.contains(OsStr::new("LOWER.txt")));
        assert_eq!(
            copied.contains(OsStr::new(nfd)),
            repo.join("Dir").join(nfc).exists(),
            "the filesystem determines whether NFC and NFD name the same entry"
        );
        assert!(!copied.contains(OsStr::new("alias.txt")));
        assert!(
            std::fs::read_dir(&dest)
                .unwrap()
                .any(|entry| entry.unwrap().file_name() == "DIR")
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("DIR/LOWER.txt")).unwrap(),
            "working edit"
        );
        std::fs::write(dest.join("DIR/LOWER.txt"), "destination edit").unwrap();
        crate::WorktreeBuilder::new(&repo, &dest)
            .parallelism(2)
            .copy_ignored_only()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("DIR/LOWER.txt")).unwrap(),
            "destination edit"
        );

        // The ordinary walk also needs to classify a case-changed file as
        // tracked. A failed write must abort even without any ignore rule.
        std::fs::remove_file(repo.join(".gitignore")).unwrap();
        std::fs::remove_file(dest.join("DIR/LOWER.txt")).unwrap();
        std::fs::create_dir(dest.join("DIR/LOWER.txt")).unwrap();
        assert!(copy_parallel(&repo, &dest, config, CancellationToken::new()).is_err());
    }

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
