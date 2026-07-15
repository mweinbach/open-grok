//! Advisory `auth.json.lock` helpers (free functions, no `AuthManager`
//! dependency).
//!
//! Uses flock + PID-in-file + unlink-to-break for robust stale-lock
//! recovery:
//! - `flock(LOCK_EX | LOCK_NB)` for race-free mutual exclusion
//! - `PID:TIMESTAMP` written into the lock file so waiters can detect
//!   staleness
//! - Waiters that find a dead or stuck holder `unlink` the lock file
//!   and retry on a fresh inode (the old holder's flock lives on the
//!   now-unlinked inode)

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::Path;
use std::time::Duration as StdDuration;

use fs2::FileExt;

use crate::auth::storage::AuthFileLock;
use crate::unified_log;

/// Maximum age (seconds) of a lock holder before it is considered stuck.
const STALE_LOCK_TIMEOUT_SECS: u64 = 60;

// ── Holder-info helpers ──────────────────────────────────────────────

/// Write `PID:UNIX_TIMESTAMP` into the lock file so waiters can detect
/// staleness.
fn write_holder_info(file: &mut File) -> io::Result<()> {
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    file.set_len(0)?;
    file.seek(io::SeekFrom::Start(0))?;
    write!(file, "{pid}:{ts}")?;
    file.sync_all()?;
    Ok(())
}

/// Parse `PID:UNIX_TIMESTAMP` from lock file content.
fn parse_holder_info(content: &str) -> Option<(u32, u64)> {
    let (pid_str, ts_str) = content.trim().split_once(':')?;
    Some((pid_str.parse().ok()?, ts_str.parse().ok()?))
}

// ── Platform-specific helpers ────────────────────────────────────────

/// Check whether the process that wrote the lock file is still running.
#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    // `pid_t` is `i32`; values ≤ 0 have special semantics for `kill(2)`
    // (0 = own process group, -1 = all processes).  Reject them so we
    // don't accidentally probe the wrong target.
    let pid_i = match i32::try_from(pid) {
        Ok(p) if p > 0 => p,
        _ => return false,
    };
    // SAFETY: `kill(pid, 0)` is a POSIX-defined no-op signal used solely
    // for existence testing.
    //   ret ==  0          → process exists and we can signal it
    //   ret == -1, ESRCH   → process does not exist
    //   ret == -1, EPERM   → process exists but we lack permission
    // We must treat EPERM as "alive" to avoid breaking a live holder's
    // lock when running under a different effective UID.
    let ret = unsafe { libc::kill(pid_i as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    // errno == ESRCH means the process is gone; any other errno
    // (e.g. EPERM) means it exists but we can't signal it.
    let err = io::Error::last_os_error();
    err.raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    true // conservative fallback — skip liveness check on non-Unix
}

/// `fstat(fd)` vs `stat(path)` inode comparison.  Detects a concurrent
/// unlink+recreate between our `flock` and the subsequent check.
#[cfg(unix)]
fn inodes_match(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let fd_meta = file.metadata()?;
    let path_meta = std::fs::metadata(path)?;
    Ok(fd_meta.ino() == path_meta.ino() && fd_meta.dev() == path_meta.dev())
}

#[cfg(not(unix))]
fn inodes_match(_file: &File, _path: &Path) -> io::Result<bool> {
    Ok(true) // no inode concept; skip the check
}

// ── Staleness check ──────────────────────────────────────────────────

/// Decide staleness when the lock file carries no usable `PID:TS` holder
/// info — it is empty, was truncated mid-write, or holds non-UTF-8
/// garbage. We have no PID to liveness-probe, so we fall back to the lock
/// file's mtime: every real holder rewrites holder info (bumping mtime)
/// the instant it takes the flock, so a lock file whose mtime is older
/// than [`STALE_LOCK_TIMEOUT_SECS`] has been abandoned and is safe to
/// break. A lock caught in the sub-millisecond `set_len(0)`→write window
/// keeps a fresh mtime and is therefore never broken by this path.
///
/// Returning `false` here used to be unconditional ("assume alive"), which
/// turned a single empty/garbage lock file into an unbreakable lock and
/// wedged every refresh behind it.
fn unidentified_holder_is_stale(file: &File, why: &str) -> bool {
    let Ok(modified) = file.metadata().and_then(|m| m.modified()) else {
        unified_log::debug(
            &format!("auth lock: {why}; mtime unreadable, assuming alive"),
            None,
            None,
        );
        return false;
    };
    let age = modified.elapsed().unwrap_or_default().as_secs();
    if age > STALE_LOCK_TIMEOUT_SECS {
        unified_log::info(
            &format!(
                "auth lock: {why}; mtime age={age}s > {STALE_LOCK_TIMEOUT_SECS}s, breaking stale lock"
            ),
            None,
            Some(serde_json::json!({ "age_secs": age, "threshold_secs": STALE_LOCK_TIMEOUT_SECS })),
        );
        true
    } else {
        unified_log::debug(
            &format!("auth lock: {why}; mtime age={age}s within threshold, assuming alive"),
            None,
            None,
        );
        false
    }
}

/// Read the lock file content and return `true` when the current holder
/// is stale: process dead, holding longer than [`STALE_LOCK_TIMEOUT_SECS`],
/// or unidentifiable (empty/garbage holder info) with an mtime past the
/// stale threshold.
fn is_holder_stale(file: &mut File) -> bool {
    let mut content = String::new();
    if file.seek(io::SeekFrom::Start(0)).is_err() || file.read_to_string(&mut content).is_err() {
        return unidentified_holder_is_stale(file, "holder info unreadable");
    }
    let Some((holder_pid, holder_ts)) = parse_holder_info(&content) else {
        return unidentified_holder_is_stale(
            file,
            &format!("holder info unparseable (raw={content:?})"),
        );
    };

    // Process dead?
    if !is_process_alive(holder_pid) {
        unified_log::info(
            &format!("auth lock: holder pid={holder_pid} is dead, breaking stale lock"),
            None,
            Some(serde_json::json!({ "holder_pid": holder_pid, "holder_ts": holder_ts })),
        );
        return true;
    }

    // Process stuck (holding > STALE_LOCK_TIMEOUT_SECS)?
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let age = now.saturating_sub(holder_ts);
    if age > STALE_LOCK_TIMEOUT_SECS {
        unified_log::info(
            &format!(
                "auth lock: holder pid={holder_pid} appears stuck (age={age}s > {STALE_LOCK_TIMEOUT_SECS}s), breaking stale lock"
            ),
            None,
            Some(
                serde_json::json!({ "holder_pid": holder_pid, "age_secs": age, "threshold_secs": STALE_LOCK_TIMEOUT_SECS }),
            ),
        );
        return true;
    }

    false
}

// ── Single-iteration acquire logic ───────────────────────────────────

/// Outcome of one lock attempt.
enum LockAttempt {
    /// Lock acquired; inner file holds the flock.
    Acquired(File),
    /// Lock is legitimately held by another live process — sleep and retry.
    Busy,
    /// Stale lock was unlinked — retry immediately on a fresh inode.
    StaleUnlinked,
    /// Unrecoverable I/O error — give up.
    Failed,
}

/// Execute one iteration of the acquire loop.
///
/// `lock_path` is the resolved path to `auth.json.lock` — computed once
/// by the caller to avoid re-deriving it on every poll iteration.
fn try_acquire_once(lock_path: &Path) -> LockAttempt {
    // Step 1: open (create if missing) auth.json.lock
    let mut file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            unified_log::warn(
                &format!("auth lock: failed to open {}: {e}", lock_path.display()),
                None,
                None,
            );
            return LockAttempt::Failed;
        }
    };

    // Step 2: flock(LOCK_EX | LOCK_NB)
    match file.try_lock_exclusive() {
        Ok(()) => {
            let pid = std::process::id();
            // Step 3: write holder info, then verify same inode.
            if let Err(e) = write_holder_info(&mut file) {
                unified_log::warn(
                    &format!("auth lock: failed to write holder info: {e}"),
                    None,
                    Some(serde_json::json!({ "pid": pid })),
                );
                // Still hold the flock — proceed
            }

            match inodes_match(&file, lock_path) {
                Ok(true) => {
                    unified_log::debug(
                        &format!("auth lock: acquired (pid={pid})"),
                        None,
                        Some(
                            serde_json::json!({ "pid": pid, "path": lock_path.display().to_string() }),
                        ),
                    );
                    LockAttempt::Acquired(file)
                }
                Ok(false) => {
                    // Someone else unlinked our file and created a new one;
                    // our flock is on the deleted inode.  Retry.
                    unified_log::debug(
                        &format!("auth lock: inode changed after acquire (pid={pid}), retrying"),
                        None,
                        None,
                    );
                    LockAttempt::StaleUnlinked
                }
                Err(e) => {
                    // Path deleted between flock and stat — retry.
                    unified_log::debug(
                        &format!("auth lock: path gone after acquire (pid={pid}): {e}"),
                        None,
                        None,
                    );
                    LockAttempt::StaleUnlinked
                }
            }
        }

        // Step 4: EWOULDBLOCK — lock is held by someone else.
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
            if is_holder_stale(&mut file) {
                match std::fs::remove_file(lock_path) {
                    Ok(()) => LockAttempt::StaleUnlinked,
                    Err(e) => {
                        // Unlink failed (permissions, etc.) — fall back to
                        // Busy so the caller sleeps before retrying instead
                        // of tight-looping on repeated unlink failures.
                        unified_log::warn(
                            &format!("auth lock: failed to unlink stale lock file: {e}"),
                            None,
                            None,
                        );
                        LockAttempt::Busy
                    }
                }
            } else {
                LockAttempt::Busy
            }
        }

        Err(e) => {
            unified_log::warn(&format!("auth lock: flock failed: {e}"), None, None);
            LockAttempt::Failed
        }
    }
}

// ── Blocking acquire (kernel FIFO wait queue) ────────────────────────

/// Attempt a blocking `flock(LOCK_EX)` on the lock file.  Returns the
/// locked file on success, or an error on I/O failure / inode mismatch.
///
/// This blocks the calling thread in the kernel's flock wait queue until
/// the lock is available — FIFO-fair, zero CPU while waiting.
fn blocking_acquire(lock_path: &Path) -> io::Result<File> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;

    // Blocking flock — waits in kernel until the lock is available.
    file.lock_exclusive().map_err(|e| {
        unified_log::warn(
            &format!("auth lock: blocking flock failed: {e}"),
            None,
            None,
        );
        e
    })?;

    let pid = std::process::id();
    if let Err(e) = write_holder_info(&mut file) {
        unified_log::warn(
            &format!("auth lock: failed to write holder info: {e}"),
            None,
            Some(serde_json::json!({ "pid": pid })),
        );
        // Still hold the flock — proceed.
    }

    // Verify the FD's inode still matches the path (detects a concurrent
    // unlink+recreate that happened between our open and our flock).
    match inodes_match(&file, lock_path) {
        Ok(true) => {
            unified_log::debug(
                &format!("auth lock: acquired via blocking flock (pid={pid})"),
                None,
                Some(serde_json::json!({ "pid": pid, "path": lock_path.display().to_string() })),
            );
            Ok(file)
        }
        Ok(false) => Err(io::Error::other(
            "inode changed during blocking flock (concurrent unlink+recreate)",
        )),
        Err(e) => Err(io::Error::other(format!(
            "path gone after blocking flock: {e}"
        ))),
    }
}

// ── Public API ───────────────────────────────────────────────────────

/// Best-effort **non-blocking** acquire for advisory cleanup call sites
/// (`AuthManager::new` WebLogin cleanup, `remove_scope`).
///
/// Unlike [`try_lock_auth_file_async`] this never waits and never breaks a
/// stale lock: it takes the flock iff it is free right now, otherwise
/// returns `None` so the caller simply skips its best-effort write.
/// Crucially it records `PID:TS` holder info after locking, so a waiter
/// that observes the flock can identify the holder (and break it once
/// stale). Taking the flock *without* writing holder info is what used to
/// leave an empty `auth.json.lock` that defeated stale-lock recovery.
pub(crate) fn try_lock_auth_file_nonblocking(auth_json_path: &Path) -> Option<AuthFileLock> {
    let lock_path = auth_json_path.with_file_name("auth.json.lock");
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .ok()?;

    // Non-blocking: bail immediately if another holder has the flock.
    file.try_lock_exclusive().ok()?;

    if let Err(e) = write_holder_info(&mut file) {
        unified_log::warn(
            &format!("auth lock: failed to write holder info (non-blocking): {e}"),
            None,
            Some(serde_json::json!({ "pid": std::process::id() })),
        );
        // Still hold the flock — proceed.
    }
    Some(AuthFileLock { _file: file })
}

/// Acquire the `auth.json.lock` file lock with three phases:
///
/// 1. **Instant try** — non-blocking `flock(LOCK_NB)`.  Succeeds
///    immediately if the lock is free.
/// 2. **Blocking wait** — `flock(LOCK_EX)` on a `spawn_blocking`
///    thread, wrapped in `tokio::time::timeout`.  The kernel's flock
///    wait queue is FIFO: when the holder releases, exactly one waiter
///    wakes.  Zero CPU while waiting.
/// 3. **Stale fallback** — on timeout, try one non-blocking acquire
///    with stale-lock detection (dead PID / age > 60 s).  Breaks the
///    stale lock via unlink and retries.
pub(crate) async fn try_lock_auth_file_async(
    auth_json_path: &Path,
    timeout: StdDuration,
) -> Option<AuthFileLock> {
    let lock_path = auth_json_path.with_file_name("auth.json.lock");

    unified_log::debug(
        &format!(
            "auth lock: attempting acquire (timeout={}ms)",
            timeout.as_millis()
        ),
        None,
        Some(
            serde_json::json!({ "path": lock_path.display().to_string(), "timeout_ms": timeout.as_millis() as u64 }),
        ),
    );

    // Phase 1: instant non-blocking try.
    match try_acquire_once(&lock_path) {
        LockAttempt::Acquired(file) => return Some(AuthFileLock { _file: file }),
        LockAttempt::Failed => return None,
        LockAttempt::StaleUnlinked | LockAttempt::Busy => { /* fall through to Phase 2 */ }
    }

    // Phase 2: blocking flock via spawn_blocking + timeout.
    // Retry loop handles the rare inode-mismatch race (a third process
    // unlinked the lock file between our open and our flock).
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining == StdDuration::ZERO {
            break; // fall through to Phase 3
        }

        let lp = lock_path.clone();
        let result = tokio::time::timeout(
            remaining,
            tokio::task::spawn_blocking(move || blocking_acquire(&lp)),
        )
        .await;

        match result {
            // Blocking flock succeeded, inode matches.
            Ok(Ok(Ok(file))) => return Some(AuthFileLock { _file: file }),
            // Inode mismatch — retry from the top of the loop.
            Ok(Ok(Err(_inode_err))) => continue,
            // spawn_blocking panicked — give up.
            Ok(Err(_join_err)) => return None,
            // Timeout — fall through to Phase 3.
            Err(_timeout) => break,
        }
    }

    // Phase 3: stale-lock recovery (last resort).
    // The blocking flock timed out, meaning the holder is likely stuck.
    // Try non-blocking flock + stale detection (dead PID / age > 60 s).
    unified_log::warn(
        &format!(
            "auth lock: blocking flock timed out after {}ms, trying stale recovery",
            timeout.as_millis()
        ),
        None,
        Some(
            serde_json::json!({ "path": lock_path.display().to_string(), "timeout_ms": timeout.as_millis() as u64 }),
        ),
    );
    for _ in 0..2 {
        match try_acquire_once(&lock_path) {
            LockAttempt::Acquired(file) => return Some(AuthFileLock { _file: file }),
            LockAttempt::StaleUnlinked => continue, // unlinked stale lock, retry once
            LockAttempt::Busy | LockAttempt::Failed => break,
        }
    }

    unified_log::warn(
        &format!(
            "auth lock: all phases exhausted after {}ms",
            timeout.as_millis()
        ),
        None,
        Some(
            serde_json::json!({ "path": lock_path.display().to_string(), "timeout_ms": timeout.as_millis() as u64 }),
        ),
    );
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn auth_json_path(dir: &TempDir) -> std::path::PathBuf {
        dir.path().join("auth.json")
    }

    // ── Pure-function unit tests (no runtime needed) ─────────────────

    #[test]
    fn test_write_and_parse_holder_info() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        write_holder_info(&mut file).unwrap();

        file.seek(io::SeekFrom::Start(0)).unwrap();
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();

        let (pid, ts) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(now - ts < 2, "timestamp should be within 2 seconds");
    }

    #[test]
    fn test_parse_holder_info_edge_cases() {
        assert_eq!(
            parse_holder_info("12345:1700000000"),
            Some((12345, 1700000000))
        );
        assert_eq!(
            parse_holder_info("  12345:1700000000  "),
            Some((12345, 1700000000))
        );
        assert!(parse_holder_info("").is_none());
        assert!(parse_holder_info("no-colon").is_none());
        assert!(parse_holder_info("abc:123").is_none());
        assert!(parse_holder_info("123:abc").is_none());
    }

    #[test]
    fn test_unidentified_holder_is_stale_by_mtime() {
        // An empty / unparseable lock file is broken based on mtime: fresh
        // means a holder may be mid-write (assume alive), old means it was
        // abandoned (break it). Regression for the production wedge where
        // an empty `auth.json.lock` was treated as alive forever.
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        std::fs::write(&lock_path, b"").unwrap(); // empty → unparseable

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();

        assert!(
            !unidentified_holder_is_stale(&file, "test"),
            "fresh empty lock must be assumed alive"
        );

        let old = filetime::FileTime::from_unix_time(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64)
                - (STALE_LOCK_TIMEOUT_SECS as i64 + 30),
            0,
        );
        filetime::set_file_mtime(&lock_path, old).unwrap();

        assert!(
            unidentified_holder_is_stale(&file, "test"),
            "empty lock older than the stale threshold must be broken"
        );
    }

    #[test]
    fn test_nonblocking_acquire_writes_holder_info() {
        // fix: advisory cleanup sites must record `PID:TS`, never hold the
        // flock over an empty lock file.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let lock = try_lock_auth_file_nonblocking(&path).expect("uncontended non-blocking acquire");

        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _ts) =
            parse_holder_info(&content).expect("non-blocking acquire must write parseable info");
        assert_eq!(pid, std::process::id());

        drop(lock);
    }

    #[test]
    fn test_nonblocking_acquire_returns_none_when_held() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let lock1 = try_lock_auth_file_nonblocking(&path).expect("first acquire");
        // Same process, different FD → WouldBlock. Non-blocking acquire
        // must not wait and must not break a live lock.
        let lock2 = try_lock_auth_file_nonblocking(&path);
        assert!(lock2.is_none(), "must return None when the lock is held");
        drop(lock1);
    }

    #[cfg(unix)]
    #[test]
    fn test_is_process_alive() {
        assert!(is_process_alive(std::process::id()));
        assert!(!is_process_alive(0));
        assert!(!is_process_alive(u32::MAX));
        assert!(!is_process_alive(i32::MAX as u32));
    }

    #[cfg(unix)]
    #[test]
    fn test_is_holder_stale_dead_pid() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        let dead_pid: u32 = i32::MAX as u32;
        write!(file, "{dead_pid}:9999999999").unwrap();
        file.sync_all().unwrap();

        assert!(is_holder_stale(&mut file), "dead PID should be stale");
    }

    #[cfg(unix)]
    #[test]
    fn test_is_holder_stale_alive_pid() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        write_holder_info(&mut file).unwrap();

        assert!(
            !is_holder_stale(&mut file),
            "live process with recent timestamp should not be stale"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_is_holder_stale_old_timestamp() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        let our_pid = std::process::id();
        let old_ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 200;
        write!(file, "{our_pid}:{old_ts}").unwrap();
        file.sync_all().unwrap();

        assert!(is_holder_stale(&mut file));
    }

    #[cfg(unix)]
    #[test]
    fn test_inodes_match_same_file() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        assert!(inodes_match(&file, &lock_path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn test_inodes_mismatch_after_unlink_recreate() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("test.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        std::fs::remove_file(&lock_path).unwrap();
        let _new_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&lock_path)
            .unwrap();

        assert!(!inodes_match(&file, &lock_path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn test_still_live_detects_broken_lock() {
        // A held guard reports `still_live() == true`; after a sibling breaks
        // the lock (unlink + recreate on a fresh inode, the stale-recovery
        // path) the SAME guard reports `false`. This is what lets a
        // suspended-then-resumed holder notice its lock was reclaimed and
        // refuse to spend the refresh token.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let lock = try_lock_auth_file_nonblocking(&path).expect("acquire");
        assert!(lock.still_live(&path), "freshly acquired lock must be live");

        // Simulate the stale-recovery break performed by another process.
        let lock_path = path.with_file_name("auth.json.lock");
        std::fs::remove_file(&lock_path).unwrap();
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();

        assert!(
            !lock.still_live(&path),
            "after unlink+recreate the held guard must report not-live"
        );
    }

    // ── Async tests against the production code path ─────────────────

    #[tokio::test]
    async fn test_async_acquire_release_basic() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(1)).await;
        assert!(lock.is_some(), "should acquire lock");

        // Verify lock file has holder info.
        let lock_path = path.with_file_name("auth.json.lock");
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _ts) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());

        // Release.
        drop(lock);

        // Re-acquire should succeed.
        let lock2 = try_lock_auth_file_async(&path, StdDuration::from_secs(1)).await;
        assert!(lock2.is_some(), "should re-acquire after release");
    }

    #[tokio::test]
    async fn test_async_contended_lock_times_out() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);

        let lock1 = try_lock_auth_file_async(&path, StdDuration::from_secs(1)).await;
        assert!(lock1.is_some());

        // Second acquire should time out (same process, different FD —
        // WouldBlock but holder is alive + recent).
        let lock2 = try_lock_auth_file_async(&path, StdDuration::from_millis(500)).await;
        assert!(lock2.is_none(), "should time out when lock is held");

        drop(lock1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_acquire_after_leftover_dead_pid_file() {
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let dead_pid: u32 = i32::MAX as u32;
        std::fs::write(&lock_path, format!("{dead_pid}:9999999999")).unwrap();

        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(1)).await;
        assert!(lock.is_some(), "should acquire over leftover dead-PID file");

        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _ts) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());
    }

    // ── Real cross-process integration tests (async) ─────────────────
    //
    // These spawn a genuine second process so we exercise OS-level flock
    // semantics that threads/extra FDs cannot model: a *dead* holder PID
    // (flock auto-released on process death) and `is_process_alive()`
    // recovery. Following the in-repo subprocess-isolation pattern
    // (`xai-crash-handler/tests/integration.rs`), the holder is this very
    // test binary re-executed via `current_exe()`, gated by the
    // `GROK_TEST_LOCK_HOLDER` env var on an `#[ignore]`d entry-point test —
    // no external `python3` dependency.

    /// Line printed to stdout once the subprocess holds the flock.
    #[cfg(unix)]
    const LOCK_HOLDER_READY: &str = "__GROK_LOCK_HOLDER_READY__";

    /// Subprocess entry point for the cross-process lock tests. Only does
    /// anything when re-executed with `GROK_TEST_LOCK_HOLDER` set; a normal
    /// `cargo test` run sees the env var absent and returns immediately
    /// (it is `#[ignore]`d anyway).
    ///
    /// Spec format: `"<lock_path>|<mode>|<age_secs>"`
    ///   - `pid`   → write `PID:TS` holder info, `TS` backdated by `age_secs`
    ///   - `empty` → leave the file empty; backdate its mtime by `age_secs`
    ///
    /// Holds an exclusive flock, prints [`LOCK_HOLDER_READY`], then blocks
    /// on stdin until the parent writes a line, closes the pipe, or kills us.
    #[cfg(unix)]
    #[test]
    #[ignore = "spawned as a subprocess by the cross-process lock tests"]
    fn subprocess_lock_holder() {
        let Ok(spec) = std::env::var("GROK_TEST_LOCK_HOLDER") else {
            return; // normal test run — not a subprocess invocation
        };
        let mut parts = spec.splitn(3, '|');
        let lock_path = parts.next().expect("spec lock_path");
        let mode = parts.next().expect("spec mode");
        let age_secs: u64 = parts.next().expect("spec age").parse().expect("age parse");

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .expect("open lock file");
        file.lock_exclusive().expect("flock");

        match mode {
            "pid" => {
                file.set_len(0).unwrap();
                file.seek(io::SeekFrom::Start(0)).unwrap();
                write!(file, "{}:{}", std::process::id(), now - age_secs).unwrap();
                file.sync_all().unwrap();
            }
            "empty" => {
                file.set_len(0).unwrap();
                file.sync_all().unwrap();
                if age_secs > 0 {
                    let old = filetime::FileTime::from_unix_time((now - age_secs) as i64, 0);
                    filetime::set_file_mtime(lock_path, old).unwrap();
                }
            }
            other => panic!("unknown lock-holder mode: {other:?}"),
        }

        println!("{LOCK_HOLDER_READY}");
        io::stdout().flush().unwrap();

        // Block until released by the parent (line on stdin / closed pipe)
        // or SIGKILL (the OS releases the flock on process death).
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
    }

    /// Re-execute this test binary as a lock-holder subprocess and return
    /// once it signals that it holds the flock. `mode` is `"pid"` or
    /// `"empty"`; `age_secs` backdates the holder timestamp (`pid`) or the
    /// file mtime (`empty`).
    #[cfg(unix)]
    fn spawn_lock_holder_subprocess(
        lock_path: &std::path::Path,
        mode: &str,
        age_secs: u64,
    ) -> std::process::Child {
        use std::io::BufRead;

        let exe = std::env::current_exe().expect("current_exe");
        let spec = format!("{}|{mode}|{age_secs}", lock_path.to_str().unwrap());
        let mut child = std::process::Command::new(exe)
            .env("GROK_TEST_LOCK_HOLDER", spec)
            .args([
                "--ignored",
                "--exact",
                "--nocapture",
                "auth::manager::lock::tests::subprocess_lock_holder",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn lock-holder subprocess");

        // Read stdout until the ready marker, skipping libtest's
        // `--nocapture` banner lines. Borrow stdout (don't `take`) so the
        // pipe stays open for the child's later libtest output, and scope
        // the borrow so `child` can be moved out on return.
        {
            let stdout = child.stdout.as_mut().expect("child stdout");
            let mut reader = std::io::BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                let n = reader.read_line(&mut line).expect("read child stdout");
                assert!(n > 0, "child exited before signaling ready");
                if line.trim() == LOCK_HOLDER_READY {
                    break;
                }
            }
        }
        child
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_real_stale_holder_broken_by_old_timestamp() {
        // Child holds flock with timestamp 120s in the past. Parent
        // detects stale via timestamp, unlinks, acquires on fresh inode.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "pid", 120);
        let child_pid = child.id();

        assert!(is_process_alive(child_pid));

        let start = tokio::time::Instant::now();
        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(5)).await;
        let elapsed = start.elapsed();

        assert!(lock.is_some(), "should break stale lock held by child");
        assert!(
            elapsed < StdDuration::from_secs(2),
            "stale break should be near-instant, took {elapsed:?}"
        );

        // Verify our PID was written to the NEW lock file.
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());

        // Child is still alive (flock on the old unlinked inode).
        assert!(is_process_alive(child_pid));

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_breaks_old_empty_lock_held_by_live_holder() {
        // Regression: a LIVE process holding the flock on an EMPTY lock
        // file (no `PID:TS`) used to be "alive forever", wedging refresh.
        // With the mtime fallback an old empty lock is broken even though
        // the holder process is still running.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child =
            spawn_lock_holder_subprocess(&lock_path, "empty", STALE_LOCK_TIMEOUT_SECS + 30);
        assert!(is_process_alive(child.id()));

        let start = tokio::time::Instant::now();
        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(5)).await;
        let elapsed = start.elapsed();

        assert!(lock.is_some(), "should break old empty lock held by child");
        assert!(
            elapsed < StdDuration::from_secs(2),
            "stale break should be near-instant, took {elapsed:?}"
        );

        // The fresh lock file must carry our parseable holder info.
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_does_not_break_fresh_empty_lock() {
        // Inverse guard: an EMPTY lock with a RECENT mtime (a holder caught
        // in the sub-ms set_len(0)->write window) must NOT be broken.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "empty", 0); // fresh mtime

        let lock = try_lock_auth_file_async(&path, StdDuration::from_millis(800)).await;
        assert!(
            lock.is_none(),
            "must not break a fresh empty lock (holder may be mid-write)"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_real_killed_process_recovery() {
        // Child holds flock then gets SIGKILL'd. Flock released on
        // process death. Parent acquires immediately.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "pid", 0);
        let child_pid = child.id();

        // Verify child's PID in the lock file.
        let content_before = std::fs::read_to_string(&lock_path).unwrap();
        let (written_pid, _) = parse_holder_info(&content_before).unwrap();
        assert_eq!(written_pid, child_pid);

        // Kill the child.
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!is_process_alive(child_pid));

        // Lock file still has the dead child's PID.
        let content_after = std::fs::read_to_string(&lock_path).unwrap();
        let (dead_pid, _) = parse_holder_info(&content_after).unwrap();
        assert_eq!(dead_pid, child_pid);

        // Acquire should succeed immediately.
        let start = tokio::time::Instant::now();
        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(2)).await;
        let elapsed = start.elapsed();

        assert!(lock.is_some(), "should acquire after child killed");
        assert!(
            elapsed < StdDuration::from_secs(1),
            "should be instant, took {elapsed:?}"
        );

        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_real_contention_resolved_after_release() {
        // Child holds flock for ~2s then exits. Parent's blocking flock
        // (Phase 2) wakes immediately on release — no poll lag.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "pid", 0);

        // Release child after 2s delay (on a background thread since
        // stdin.write is blocking).
        let mut stdin = child.stdin.take().unwrap();
        let release_handle = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_secs(2));
            let _ = stdin.write_all(b"release\n");
        });

        let start = tokio::time::Instant::now();
        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(10)).await;
        let elapsed = start.elapsed();

        assert!(lock.is_some(), "should acquire after child exits");
        assert!(
            elapsed >= StdDuration::from_millis(1500),
            "should have waited for child, took {elapsed:?}"
        );
        assert!(
            elapsed < StdDuration::from_secs(5),
            "should not overshoot, took {elapsed:?}"
        );

        release_handle.join().unwrap();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn test_real_is_process_alive_with_spawned_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let pid = child.id();

        assert!(is_process_alive(pid), "child should be alive");

        child.kill().unwrap();
        child.wait().unwrap();

        assert!(!is_process_alive(pid), "child should be dead after kill");
    }

    // ── Blocking acquire unit tests ──────────────────────────────────

    #[cfg(unix)]
    #[test]
    fn test_blocking_acquire_uncontended() {
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join("auth.json.lock");

        let file =
            blocking_acquire(&lock_path).expect("uncontended blocking acquire should succeed");
        let content = std::fs::read_to_string(&lock_path).unwrap();
        let (pid, _ts) = parse_holder_info(&content).unwrap();
        assert_eq!(pid, std::process::id());
        drop(file);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_async_blocking_path_wakes_promptly_on_release() {
        // Child holds flock for 1s. Verify the blocking flock (Phase 2)
        // acquires within 500ms of the child releasing — much faster
        // than a 200ms poll loop would guarantee.
        let dir = TempDir::new().unwrap();
        let path = auth_json_path(&dir);
        let lock_path = path.with_file_name("auth.json.lock");

        let mut child = spawn_lock_holder_subprocess(&lock_path, "pid", 0);

        // Release child after 1s.
        let mut stdin = child.stdin.take().unwrap();
        let release_handle = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_secs(1));
            let _ = stdin.write_all(b"release\n");
        });

        let start = tokio::time::Instant::now();
        let lock = try_lock_auth_file_async(&path, StdDuration::from_secs(10)).await;
        let elapsed = start.elapsed();

        assert!(lock.is_some(), "should acquire via blocking flock");
        // Should acquire very close to 1s (child hold time), not 1s + poll lag.
        assert!(
            elapsed >= StdDuration::from_millis(800),
            "should have waited for child, took {elapsed:?}"
        );
        assert!(
            elapsed < StdDuration::from_millis(2000),
            "blocking flock should wake promptly, took {elapsed:?}"
        );

        release_handle.join().unwrap();
        let _ = child.wait();
    }
}
