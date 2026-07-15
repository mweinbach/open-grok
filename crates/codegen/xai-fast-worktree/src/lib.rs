//! High-performance git worktree creation using CoW cloning.
//!
//! This crate provides fast worktree creation by:
//! 1. Using `git worktree add --no-checkout` (instant metadata creation)
//! 2. Parallel CoW file cloning with hash-based sharding
//! 3. Optional dirty file replication and ignored file copying
//! 4. BTRFS snapshot support on Linux for O(1) cloning
//! 5. Worktree sync API for pre-created worktree pools
//! 6. SQLite metadata tracking (behind `metadata` feature)

mod api;
#[cfg(target_os = "linux")]
pub mod btrfs;
mod copy;
#[cfg(feature = "metadata")]
pub mod db;
#[cfg(feature = "metadata")]
pub mod discovery;
mod git;
#[cfg(target_os = "linux")]
pub(crate) mod mount_info;
#[cfg(target_os = "linux")]
mod overlay;
pub mod sync;
#[cfg(target_os = "linux")]
pub(crate) mod util;
mod worktree;

#[cfg(target_os = "linux")]
pub use api::cleanup_orphaned_btrfs_snapshots;
#[cfg(target_os = "linux")]
pub use api::cleanup_orphaned_overlay_snapshots;
#[cfg(feature = "metadata")]
pub use api::gc::{GcOptions, GcReport, gc_worktrees, gc_worktrees_with_delegate};
pub use api::{
    BtrfsDelegate, BtrfsMode, CleanupReport, CopyReport, CreationMode, DelegateSnapshotResult,
    DirtyFilesReport, ENOSPC_OS_MESSAGE, IgnoredFilesMode, OUT_OF_DISK_CONTEXT, RemoveReport,
    WorkingTreeMode, WorktreeBuilder, WorktreeReport, cleanup_worktrees_in,
    cleanup_worktrees_in_with_delegate, remove_worktree, remove_worktree_with_delegate,
};
#[cfg(feature = "metadata")]
pub use db::{
    DbStats, ListFilter, WorktreeDb, WorktreeKind, WorktreeRecord, WorktreeStatus, id_from_path,
    now_epoch_secs, repo_name_from_path, resolve_grok_home,
};
#[cfg(feature = "metadata")]
pub use discovery::{RebuildReport, discover_worktrees, rebuild_worktree_db};
pub use git::checkout::{
    rehydrate_worktree_from_ref, snapshot_worktree_to_ref, transfer_snapshot_to_repo,
};
pub use sync::{SourceDirtyState, SyncReport, WorktreeSync, collect_source_dirty_state};
#[cfg(target_os = "linux")]
pub use worktree::execute::cleanup_snapshot_git_state;

/// Count the number of tracked files in a git repository's index.
///
/// Reads the index header via `gix`, which contains the entry count — this
/// is an O(1) read (no directory walk). Useful for deciding whether a repo
/// is large enough to benefit from worktree pooling.
pub fn count_tracked_files(repo_path: &std::path::Path) -> anyhow::Result<usize> {
    let repo = gix::discover(repo_path)
        .map_err(|e| anyhow::anyhow!("failed to discover git repo: {e}"))?;
    let index = repo
        .index_or_load_from_head()
        .map_err(|e| anyhow::anyhow!("failed to load git index: {e}"))?;
    Ok(index.entries().len())
}
