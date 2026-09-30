//! Grove-parent predicates shared by the worktree facade and the copy engine.
//!
//! Upstream keeps these shapes in `grove_api` + `nfs_off` with stub answers
//! (Grove NFS was removed there). The fork kept its real NFS/FUSE client in
//! `nfs/`, so this module implements the same predicate shapes against live
//! Status + the mount table:
//!
//! - [`source_keeps_grove_create`] mirrors [`crate::nfs::source_is_linked_local_view`]
//!   (Status RPC) with the wider linked-or-forkable predicate.
//! - [`source_is_grove_parent`] is the mount-table-only check the snapshot
//!   arms and the Linked fallback use; it intentionally matches
//!   `dest_is_projected_mount` (preserves the fork's existing Linked semantics).
//! - [`dest_is_grove_projection`] is narrower: FUSE-flavored projections only,
//!   so a Standalone copy of a plain repo on an NFS home is never refused.
//!   Grove-NFS misses fall back to copy, the fork's pre-existing behavior.

use std::path::{Path, PathBuf};

/// Status capability: this daemon forks a second attach from a Grove parent backing.
pub const CAP_FORK_FROM_BACKING: &str = "fork_from_backing";

fn sole_mount(status: &crate::NfsStatusView) -> Option<&serde_json::Value> {
    let mounts = status.raw.as_ref()?.get("mounts")?.as_array()?;
    let [mount] = mounts.as_slice() else {
        return None;
    };
    Some(mount)
}

/// Grove-fork predicates over a Status view (upstream `NfsStatusView` API).
pub trait NfsStatusGroveExt {
    /// Exactly one mount this daemon will send to the fork arm. Older daemons omit the bit.
    #[must_use]
    fn is_forkable(&self) -> bool;
    /// Same predicate the Grove arm uses to send CreateWorktree.
    #[must_use]
    fn can_fork(&self) -> bool;
    /// Linked local view or cap+forkable. Facades issue one Status RPC and call this.
    #[must_use]
    fn keeps_grove_create(&self) -> bool;
    #[must_use]
    fn has_capability(&self, cap: &str) -> bool;
    /// Kernel dest (`MountStatus.mountpoint`). Backing `worktree` / `git_dir` / `store_id` are not dests.
    #[must_use]
    fn slug_root(&self) -> Option<PathBuf>;
}

impl NfsStatusGroveExt for crate::NfsStatusView {
    fn is_forkable(&self) -> bool {
        sole_mount(self)
            .and_then(|m| m.get("forkable"))
            .and_then(|v| v.as_bool())
            == Some(true)
    }

    fn can_fork(&self) -> bool {
        self.has_capability(CAP_FORK_FROM_BACKING) && self.is_forkable()
    }

    fn keeps_grove_create(&self) -> bool {
        self.is_linked_local_view() || self.can_fork()
    }

    fn has_capability(&self, cap: &str) -> bool {
        self.raw
            .as_ref()
            .and_then(|raw| raw.get("capabilities"))
            .and_then(|c| c.as_array())
            .is_some_and(|caps| caps.iter().any(|c| c.as_str() == Some(cap)))
    }

    fn slug_root(&self) -> Option<PathBuf> {
        sole_mount(self)?
            .get("mountpoint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }
}

/// Status-confirmed Grove source: linked local view or forkable parent.
/// Old daemon / miss → false.
#[must_use]
pub fn source_keeps_grove_create(opts: &crate::NfsWorktreeOpts, source: &Path) -> bool {
    if !opts.enabled {
        return false;
    }
    crate::NfsWorktreeClient::from_opts(opts)
        .status_for_dir(source)
        .is_some_and(|status| status.keeps_grove_create())
}

/// Mount table only (exact nfs/fuse or inside Grove FUSE/NFS).
/// No Status-RPC: snapshot arms on a plain checkout must not wait on a down
/// daemon, even if Status would say forkable.
#[must_use]
pub fn source_is_grove_parent(path: &Path) -> bool {
    crate::nfs::dest_is_projected_mount(path)
}

/// FUSE-flavored Grove projection at exactly `path` (callers walk ancestors
/// when a covering mount counts). Plain NFS mounts are excluded: unlike
/// `dest_is_projected_mount`, this must not block a Standalone copy of a
/// plain repo on an NFS home.
#[must_use]
pub fn dest_is_grove_projection(path: &Path) -> bool {
    crate::nfs::dest_is_projected_mount(path) && !crate::nfs::dest_is_nfs_mount(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(raw: serde_json::Value) -> crate::NfsStatusView {
        crate::NfsStatusView {
            hydration_percent: None,
            raw: Some(raw),
            port: None,
            mount_id: None,
            transport: None,
        }
    }

    #[test]
    fn is_forkable_requires_exactly_one_mount_with_the_bit() {
        assert!(!view(serde_json::json!({"mounts":[{"kind":"store"}]})).is_forkable());
        let store = view(serde_json::json!({
            "mounts":[{"kind":"store","forkable":true}]
        }));
        assert!(store.is_forkable());
        let linked = view(serde_json::json!({
            "mounts":[{"kind":"worktree","source_mode":"local","forkable":true}]
        }));
        assert!(linked.is_forkable());
        let bit_false = view(serde_json::json!({
            "mounts":[{"kind":"worktree","forkable":false}]
        }));
        assert!(!bit_false.is_forkable());
        let two = view(serde_json::json!({
            "mounts":[
                {"kind":"store","forkable":true},
                {"kind":"worktree","forkable":true}
            ]
        }));
        assert!(!two.is_forkable());
        assert!(!view(serde_json::json!({"mounts":[]})).is_forkable());
        assert!(
            !crate::NfsStatusView {
                hydration_percent: None,
                raw: None,
                port: None,
                mount_id: None,
                transport: None,
            }
            .is_forkable()
        );
    }

    #[test]
    fn fork_source_mode_is_not_a_linked_local_view() {
        let fork = view(serde_json::json!({
            "mounts":[{"kind":"worktree","source_mode":"fork","forkable":true}]
        }));
        assert!(fork.is_forkable());
        assert!(!fork.is_linked_local_view());
    }

    #[test]
    fn keeps_grove_create_is_linked_or_can_fork() {
        let linked = view(serde_json::json!({
            "mounts":[{"kind":"worktree","source_mode":"local"}]
        }));
        assert!(linked.keeps_grove_create());
        assert!(!linked.can_fork());
        let fork = view(serde_json::json!({
            "capabilities": [CAP_FORK_FROM_BACKING],
            "mounts":[{"kind":"store","forkable":true}]
        }));
        assert!(fork.can_fork());
        assert!(fork.keeps_grove_create());
        assert!(!fork.is_linked_local_view());
        assert!(!view(serde_json::json!({"mounts":[{"kind":"store"}]})).keeps_grove_create());
    }

    #[test]
    fn slug_root_is_kernel_mountpoint_not_backing() {
        assert_eq!(
            view(serde_json::json!({
                "mounts":[{
                    "mountpoint": "/mnt/grove/acme",
                    "worktree": "/var/grove/store/abc/worktree",
                    "git_dir": "/var/grove/store/abc/git",
                    "store_id": "abc"
                }]
            }))
            .slug_root()
            .as_deref(),
            Some(std::path::Path::new("/mnt/grove/acme"))
        );
        assert_eq!(
            view(serde_json::json!({
                "mounts":[{
                    "worktree": "/var/grove/store/abc/worktree",
                    "git_dir": "/var/grove/store/abc/git",
                    "store_id": "abc"
                }]
            }))
            .slug_root(),
            None
        );
        assert_eq!(view(serde_json::json!({"mounts":[{}]})).slug_root(), None);
    }
}
