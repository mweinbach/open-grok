//! Relocation journal types, validation, lease, and commit-aware persistence.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::fs::{create_dir_durable, io_error, is_lock_contended, sync_dir, write_atomic_durable};
use super::{RelocationError, Result};

pub(super) const RELOCATION_DIR: &str = "relocations";
const JOURNAL_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelocationPhase {
    Prepared,
    Staged,
    TargetPublished,
    Ready,
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RelocationJournal {
    version: u8,
    pub(crate) session_id: String,
    pub(crate) nonce: String,
    pub(crate) source_cwd: String,
    pub(crate) target_cwd: String,
    pub(crate) cwd_generation: u64,
    pub(crate) phase: RelocationPhase,
}

impl RelocationJournal {
    #[cfg(test)]
    pub(super) fn test_new(
        session_id: &str,
        source_cwd: &str,
        target_cwd: &str,
        phase: RelocationPhase,
    ) -> Self {
        Self {
            version: JOURNAL_VERSION,
            session_id: session_id.into(),
            nonce: "nonce-1".into(),
            source_cwd: source_cwd.into(),
            target_cwd: target_cwd.into(),
            cwd_generation: 1,
            phase,
        }
    }

    pub(super) fn new(
        session_id: String,
        nonce: String,
        source_cwd: String,
        target_cwd: String,
        cwd_generation: u64,
    ) -> Self {
        Self {
            version: JOURNAL_VERSION,
            session_id,
            nonce,
            source_cwd,
            target_cwd,
            cwd_generation,
            phase: RelocationPhase::Prepared,
        }
    }

    pub(super) fn validate(&self, grok_home: &Path) -> Result<()> {
        validate_component("session id", &self.session_id)?;
        validate_component("nonce", &self.nonce)?;
        validate_cwd("source cwd", &self.source_cwd)?;
        validate_cwd("target cwd", &self.target_cwd)?;
        if self.cwd_generation == 0 {
            return Err(RelocationError::Inconsistent(
                "cwd generation must be nonzero".into(),
            ));
        }
        if session_dir_at(grok_home, &self.source_cwd, &self.session_id)
            == session_dir_at(grok_home, &self.target_cwd, &self.session_id)
        {
            return Err(RelocationError::Inconsistent(
                "source and target storage paths are identical".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) enum WriteFailure {
    NotCommitted(RelocationError),
    Committed(RelocationError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AtomicWriteFault {
    BeforeRename,
    AfterRename,
}

#[must_use]
pub(crate) struct RelocationLease {
    pub(super) session_id: String,
    file: File,
}

impl Drop for RelocationLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub(super) fn acquire(grok_home: &Path, session_id: &str) -> Result<RelocationLease> {
    validate_component("session id", session_id)?;
    let dir = relocation_dir(grok_home);
    create_dir_durable(&dir)?;
    let path = dir.join(format!("{session_id}.lock"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| io_error("open", &path, e))?;
    match file.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if is_lock_contended(&error) => {
            return Err(RelocationError::LeaseBusy(session_id.to_owned()));
        }
        Err(error) => return Err(io_error("lock", &path, error)),
    }
    Ok(RelocationLease {
        session_id: session_id.to_owned(),
        file,
    })
}

/// Lease files younger than this are never swept. Holding a lease never
/// touches the file mtime, so long-held leases are still rejected by
/// `try_lock`; the age guard only narrows the create-before-flock window in
/// [`acquire`].
const LEASE_SWEEP_MIN_AGE: std::time::Duration = std::time::Duration::from_secs(60);

/// Best-effort removal of lease files left behind by completed relocations.
///
/// [`acquire`] creates `<session_id>.lock` and releases its `flock` on drop
/// but intentionally never deletes the file, so every completed relocation
/// leaves an empty lease file in the relocations directory forever. A lease
/// file is removed only when it is older than [`LEASE_SWEEP_MIN_AGE`] **and**
/// can be exclusively locked, which proves no relocation is active for that
/// session. Journals are never touched; errors are swallowed because sweeping
/// is hygiene, never a correctness step. Returns how many files were removed.
pub(super) fn sweep_stale_leases(grok_home: &Path) -> usize {
    let entries = match fs::read_dir(relocation_dir(grok_home)) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "lock") {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let stale_long_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= LEASE_SWEEP_MIN_AGE);
        if !stale_long_enough {
            continue;
        }
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            else {
                continue;
            };
        match file.try_lock_exclusive() {
            // Unlink while still holding the lease: a racing `acquire` that
            // opened the same inode fails closed with `LeaseBusy` instead of
            // splitting the session's lease across two files.
            Ok(()) => {
                if fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
                let _ = FileExt::unlock(&file);
            }
            Err(_) => {} // actively held by a live relocation
        }
    }
    removed
}

pub(super) fn read(grok_home: &Path, session_id: &str) -> Result<RelocationJournal> {
    validate_component("session id", session_id)?;
    let path = journal_path(grok_home, session_id);
    let bytes = fs::read(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RelocationError::JournalMissing(session_id.to_owned())
        } else {
            io_error("read", &path, e)
        }
    })?;
    let journal: RelocationJournal =
        serde_json::from_slice(&bytes).map_err(|source| RelocationError::Json { path, source })?;
    if journal.version != JOURNAL_VERSION || journal.session_id != session_id {
        return Err(RelocationError::Inconsistent(
            "journal identity or version mismatch".into(),
        ));
    }
    journal.validate(grok_home)?;
    Ok(journal)
}

pub(super) fn write(
    grok_home: &Path,
    journal: &RelocationJournal,
    fault: Option<AtomicWriteFault>,
) -> std::result::Result<(), WriteFailure> {
    journal
        .validate(grok_home)
        .map_err(WriteFailure::NotCommitted)?;
    let path = journal_path(grok_home, &journal.session_id);
    let bytes = serde_json::to_vec_pretty(journal).map_err(|source| {
        WriteFailure::NotCommitted(RelocationError::Json {
            path: path.clone(),
            source,
        })
    })?;
    write_atomic_durable(&path, &bytes, None, fault)
}

pub(super) fn relocation_dir(grok_home: &Path) -> PathBuf {
    grok_home.join(RELOCATION_DIR)
}

pub(super) fn journal_path(grok_home: &Path, session_id: &str) -> PathBuf {
    relocation_dir(grok_home).join(format!("{session_id}.json"))
}

pub(super) fn session_dir_at(grok_home: &Path, cwd: &str, session_id: &str) -> PathBuf {
    grok_home
        .join("sessions")
        .join(xai_grok_config::encode_cwd_dirname(cwd))
        .join(session_id)
}

pub(super) fn sync_namespace(grok_home: &Path) -> Result<()> {
    let dir = relocation_dir(grok_home);
    sync_dir(&dir).map_err(|e| io_error("sync", &dir, e))
}

pub(super) fn validate_component(field: &'static str, value: &str) -> Result<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(RelocationError::InvalidComponent {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_cwd(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() || !Path::new(value).is_absolute() {
        return Err(RelocationError::InvalidComponent {
            field,
            value: value.to_owned(),
        });
    }
    Ok(())
}
