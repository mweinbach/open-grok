//! Git marketplace source support.
//!
//! Provides persistent caching of git marketplace repos.
//! Cache root: `~/.grok/marketplace-cache/<url-hash>/`

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs2::FileExt;

/// Default TTL for marketplace cache freshness (5 minutes).
const CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    UseTtl,
    Force,
}

pub struct SourceCacheLease {
    pub path: PathBuf,
    lock_file: File,
}

impl Drop for SourceCacheLease {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

/// Sync a git marketplace source to the persistent cache.
///
/// Returns the path to the cached repo on success.
pub fn sync_source_cache(
    url: &str,
    branch: Option<&str>,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    let lease = sync_source_cache_with_mode(url, branch, cache_root, SyncMode::UseTtl)?;
    Ok(lease.path.clone())
}

pub fn force_sync_source_cache(
    url: &str,
    branch: Option<&str>,
    cache_root: &Path,
) -> Result<PathBuf, String> {
    let lease = sync_source_cache_with_mode(url, branch, cache_root, SyncMode::Force)?;
    Ok(lease.path.clone())
}

pub fn sync_source_cache_with_mode(
    url: &str,
    branch: Option<&str>,
    cache_root: &Path,
    mode: SyncMode,
) -> Result<SourceCacheLease, String> {
    let hash = cache_hash(url);
    let cache_dir = cache_root.join(&hash);
    let start = Instant::now();

    std::fs::create_dir_all(cache_root).map_err(|e| format!("failed to create cache root: {e}"))?;
    let lock_file = acquire_cache_lock(&cache_root.join(format!("{hash}.lock")), LOCK_TIMEOUT)?;

    let result = sync_cache_locked(url, branch, &cache_dir, mode);
    match &result {
        Ok(()) => {
            tracing::debug!(mode = ?mode, elapsed_ms = start.elapsed().as_millis(), "marketplace cache sync complete")
        }
        Err(error) => {
            tracing::warn!(mode = ?mode, elapsed_ms = start.elapsed().as_millis(), error = %error, "marketplace cache sync failed")
        }
    }
    result?;

    Ok(SourceCacheLease {
        path: cache_dir,
        lock_file,
    })
}

fn sync_cache_locked(
    url: &str,
    branch: Option<&str>,
    cache_dir: &Path,
    mode: SyncMode,
) -> Result<(), String> {
    if cache_dir.join(".git").exists() {
        if mode == SyncMode::UseTtl && is_cache_fresh(cache_dir) {
            return Ok(());
        }
        fetch_reset_cached_repo(cache_dir, branch).or_else(|e| {
            tracing::warn!(error = %e, "git fetch/reset failed, re-cloning marketplace cache");
            reclone_repo(url, branch, cache_dir)
        })
    } else {
        clone_repo(url, branch, cache_dir)
    }
}

fn acquire_cache_lock(lock_path: &Path, timeout: Duration) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|e| format!("failed to open cache lock {}: {e}", lock_path.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(format!(
                        "cache lock timeout after {}s for {}",
                        timeout.as_secs(),
                        lock_path.display()
                    ));
                }
                std::thread::sleep(LOCK_POLL_INTERVAL);
            }
            Err(e) => return Err(format!("failed to lock cache {}: {e}", lock_path.display())),
        }
    }
}

/// Check if the cache was fetched recently enough to skip fetching.
fn is_cache_fresh(cache_dir: &Path) -> bool {
    let fetch_head = cache_dir.join(".git").join("FETCH_HEAD");
    match std::fs::metadata(&fetch_head) {
        Ok(meta) => meta
            .modified()
            .ok()
            .and_then(|mtime| mtime.elapsed().ok())
            .is_some_and(|age| age < CACHE_TTL),
        Err(_) => false,
    }
}

/// Get the default cache root directory.
pub fn default_cache_root() -> PathBuf {
    xai_grok_config::grok_home().join("marketplace-cache")
}

/// Deterministic hash for a URL (used as cache directory name).
fn cache_hash(url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    url.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Clone a git repo with depth 1.
fn clone_repo(url: &str, branch: Option<&str>, dest: &Path) -> Result<(), String> {
    // Try git2 first.
    match clone_with_git2(url, branch, dest) {
        Ok(()) => return Ok(()),
        Err(e) => {
            tracing::debug!("git2 clone failed, trying CLI: {e}");
            // Clean up partial clone.
            let _ = std::fs::remove_dir_all(dest);
        }
    }

    // Fallback to git CLI.
    clone_with_cli(url, branch, dest)
}

fn reclone_repo(url: &str, branch: Option<&str>, dest: &Path) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("cache path has no parent: {}", dest.display()))?;
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("cache path has no file name: {}", dest.display()))?;
    let suffix = format!("{}-{}", std::process::id(), unique_reclone_suffix());
    let temp_dest = parent.join(format!(".{name}.reclone-{suffix}"));
    let backup_dest = parent.join(format!(".{name}.backup-{suffix}"));

    let _ = std::fs::remove_dir_all(&temp_dest);
    let _ = std::fs::remove_dir_all(&backup_dest);

    clone_repo(url, branch, &temp_dest).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&temp_dest);
    })?;

    let had_existing = dest.exists();
    if had_existing {
        std::fs::rename(dest, &backup_dest)
            .map_err(|e| format!("failed to move existing cache aside: {e}"))?;
    }

    match std::fs::rename(&temp_dest, dest) {
        Ok(()) => {
            if had_existing {
                let _ = std::fs::remove_dir_all(&backup_dest);
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&temp_dest);
            if had_existing && let Err(restore_err) = std::fs::rename(&backup_dest, dest) {
                return Err(format!(
                    "failed to install recloned cache: {e}; failed to restore original cache: {restore_err}; original cache preserved at {}",
                    backup_dest.display()
                ));
            }
            Err(format!("failed to install recloned cache: {e}"))
        }
    }
}

fn unique_reclone_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

fn clone_with_git2(url: &str, branch: Option<&str>, dest: &Path) -> Result<(), String> {
    let mut fetch_opts = git2::FetchOptions::new();
    fetch_opts.depth(1);

    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch_opts);
    if let Some(b) = branch {
        builder.branch(b);
    }

    builder
        .clone(url, dest)
        .map_err(|e| format!("git2 clone failed: {e}"))?;
    Ok(())
}

/// Environment variables set on every git command to suppress interactive prompts.
pub const GIT_AUTH_SUPPRESSION_ENVS: [(&str, &str); 4] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_ASKPASS", ""),
    ("GIT_LFS_SKIP_SMUDGE", "1"),
    ("GIT_SSH_COMMAND", "ssh -o BatchMode=yes"),
];

/// Git command with auth/LFS/SSH prompt suppression and `--no-optional-locks`.
pub fn git_command() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    xai_tty_utils::detach_std_command(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.envs(xai_tty_utils::pager_env());
    for &(key, val) in &GIT_AUTH_SUPPRESSION_ENVS {
        cmd.env(key, val);
    }
    cmd.arg("--no-optional-locks");
    cmd
}

fn clone_with_cli(url: &str, branch: Option<&str>, dest: &Path) -> Result<(), String> {
    let mut cmd = git_command();
    cmd.args(["clone", "--depth", "1"]);
    if let Some(b) = branch {
        cmd.args(["--branch", b]);
    }
    cmd.arg(url).arg(dest.as_os_str());

    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git clone: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git clone failed: {stderr}"));
    }
    Ok(())
}

fn fetch_reset_cached_repo(repo_dir: &Path, branch: Option<&str>) -> Result<(), String> {
    let branch_arg = branch.unwrap_or("HEAD");
    let fetch_output = git_command()
        .current_dir(repo_dir)
        .args(["fetch", "--depth", "1", "origin", branch_arg])
        .output()
        .map_err(|e| format!("failed to run git fetch: {e}"))?;

    if !fetch_output.status.success() {
        let stderr = String::from_utf8_lossy(&fetch_output.stderr);
        return Err(format!("git fetch failed: {stderr}"));
    }

    let checkout_output = git_command()
        .current_dir(repo_dir)
        .args(["checkout", "--detach", "FETCH_HEAD"])
        .output()
        .map_err(|e| format!("failed to run git checkout: {e}"))?;

    if !checkout_output.status.success() {
        let stderr = String::from_utf8_lossy(&checkout_output.stderr);
        return Err(format!("git checkout failed: {stderr}"));
    }

    let reset_output = git_command()
        .current_dir(repo_dir)
        .args(["reset", "--hard", "FETCH_HEAD"])
        .output()
        .map_err(|e| format!("failed to run git reset: {e}"))?;

    if !reset_output.status.success() {
        let stderr = String::from_utf8_lossy(&reset_output.stderr);
        return Err(format!("git reset failed: {stderr}"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_hash_is_deterministic() {
        let url = "https://github.com/xai-org/xai-plugin-marketplace.git";
        let h1 = cache_hash(url);
        let h2 = cache_hash(url);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn cache_hash_differs_for_different_urls() {
        let h1 = cache_hash("https://github.com/a/b.git");
        let h2 = cache_hash("https://github.com/c/d.git");
        assert_ne!(h1, h2);
    }

    #[test]
    fn default_cache_root_under_grok() {
        let root = default_cache_root();
        assert!(root.to_string_lossy().contains("marketplace-cache"));
    }

    #[test]
    fn sync_source_cache_uses_ttl_by_default() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy();

        let cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        let fetch_head = cache_dir.join(".git").join("FETCH_HEAD");
        std::fs::write(&fetch_head, "ttl-sentinel").unwrap();
        let second_cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        assert_eq!(second_cache_dir, cache_dir);
        assert_eq!(
            std::fs::read_to_string(&fetch_head).unwrap(),
            "ttl-sentinel"
        );
    }

    #[test]
    fn force_sync_source_cache_ignores_fresh_fetch_head() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy();

        let cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        let first_head = current_head(&cache_dir);
        add_commit(remote.path(), "second.txt", "second");

        let forced_cache_dir =
            force_sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        assert_eq!(forced_cache_dir, cache_dir);
        assert_ne!(current_head(&cache_dir), first_head);
    }

    #[test]
    fn cache_lease_blocks_concurrent_reclone_during_scan() {
        let cache_root = tempfile::tempdir().unwrap();
        let url = "https://example.com/repo.git";
        let hash = cache_hash(url);
        std::fs::create_dir_all(cache_root.path()).unwrap();
        let lock_path = cache_root.path().join(format!("{hash}.lock"));
        let lease = SourceCacheLease {
            path: cache_root.path().join(&hash),
            lock_file: acquire_cache_lock(&lock_path, Duration::from_millis(1)).unwrap(),
        };

        let start = Instant::now();
        let err = acquire_cache_lock(&lock_path, Duration::from_millis(50)).unwrap_err();
        assert!(err.contains("cache lock timeout"));
        assert!(start.elapsed() >= Duration::from_millis(50));
        drop(lease);
        let _lock = acquire_cache_lock(&lock_path, Duration::from_millis(1)).unwrap();
    }

    #[test]
    fn force_sync_source_cache_preserves_cache_when_reclone_fails() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy();

        let cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        std::fs::remove_dir_all(cache_dir.join(".git").join("objects")).unwrap();
        std::fs::remove_dir_all(remote.path()).unwrap();

        let result = force_sync_source_cache(&url, Some("main"), cache_root.path());
        assert!(result.is_err());
        assert!(cache_dir.exists());
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("file.txt")).unwrap(),
            "initial"
        );
    }

    #[test]
    fn force_sync_source_cache_reclones_corrupt_cache() {
        if !git_available() {
            eprintln!("skipping git-dependent test: git binary not available");
            return;
        }
        let remote = tempfile::tempdir().unwrap();
        init_remote_repo(remote.path());
        let cache_root = tempfile::tempdir().unwrap();
        let url = remote.path().to_string_lossy();

        let cache_dir = sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        std::fs::remove_dir_all(cache_dir.join(".git").join("objects")).unwrap();

        let forced_cache_dir =
            force_sync_source_cache(&url, Some("main"), cache_root.path()).unwrap();
        assert_eq!(forced_cache_dir, cache_dir);
        assert!(cache_dir.join(".git").join("objects").exists());
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("file.txt")).unwrap(),
            "initial"
        );
    }

    fn init_remote_repo(path: &Path) {
        run_git(path, &["init", "--initial-branch", "main"]);
        run_git(path, &["config", "user.email", "test@example.com"]);
        run_git(path, &["config", "user.name", "Test User"]);
        add_commit(path, "file.txt", "initial");
    }

    fn add_commit(repo: &Path, file: &str, contents: &str) {
        std::fs::write(repo.join(file), contents).unwrap();
        run_git(repo, &["add", file]);
        run_git(repo, &["commit", "-m", file]);
    }

    fn current_head(repo: &Path) -> String {
        let output = git_command()
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn git_available() -> bool {
        let git_bin = std::env::var("GIT_BIN_PATH").unwrap_or_else(|_| "git".to_string());
        std::process::Command::new(git_bin)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let git_bin = std::env::var("GIT_BIN_PATH").unwrap_or_else(|_| "git".to_string());
        let output = std::process::Command::new(git_bin)
            .current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
