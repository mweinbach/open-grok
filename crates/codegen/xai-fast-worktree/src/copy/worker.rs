//! Worker logic for replicating a single filesystem entry.

use std::fs::Metadata;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::{DashMap, DashSet};

use crate::copy::types::{CopyEntry, CopyEntryKind};

#[cfg(unix)]
#[path = "destination_unix.rs"]
mod destination;
#[cfg(windows)]
#[path = "destination_windows.rs"]
mod destination;
use destination::Destination;

#[derive(Debug)]
struct DestinationConflict;
impl std::fmt::Display for DestinationConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("refusing to copy through a destination link or incompatible entry")
    }
}
impl std::error::Error for DestinationConflict {}

#[cfg(test)]
#[path = "destination_tests.rs"]
mod confined_copy_tests;

pub(crate) struct WorkerCtx {
    pub source: PathBuf,
    pub dest: PathBuf,
    pub files_copied: Arc<AtomicU64>,
    pub dirs_created: Arc<AtomicU64>,
    pub symlinks_copied: Arc<AtomicU64>,
    pub issues: Arc<std::sync::Mutex<Vec<String>>>,
    pub handled_paths: Arc<DashSet<PathBuf>>,
    pub file_metadata: Arc<DashMap<PathBuf, Metadata>>,
}

pub(crate) fn run_worker(
    rx: crossbeam::channel::Receiver<CopyEntry>,
    ctx: WorkerCtx,
) -> anyhow::Result<()> {
    let mut destination = Destination::open(&ctx.dest)?;
    let mut fatal = None;
    for entry in rx {
        // Drain the bounded queue even on failure, so the producer can finish
        // and the caller can join every worker before removing the destination.
        if fatal.is_some() {
            continue;
        }
        match process_entry(&entry, &ctx, &mut destination) {
            Ok(()) => {
                ctx.handled_paths.insert(entry.rel_path);
            }
            Err(error) => {
                let error = error.context(format!("copy {}", entry.rel_path.display()));
                if entry.tracked
                    || error.is::<DestinationConflict>()
                    || crate::api::is_out_of_disk(&error)
                {
                    fatal = Some(error);
                } else {
                    ctx.issues.lock().unwrap().push(format!("{error:#}"));
                }
            }
        }
    }
    fatal.map_or(Ok(()), Err)
}

fn process_entry(
    entry: &CopyEntry,
    ctx: &WorkerCtx,
    destination: &mut Destination,
) -> anyhow::Result<()> {
    let src = ctx.source.join(&entry.rel_path);
    match entry.kind {
        CopyEntryKind::Dir => {
            destination.create_dir(&entry.rel_path)?;
            ctx.dirs_created.fetch_add(1, Ordering::Relaxed);
        }
        CopyEntryKind::Symlink => {
            destination.symlink(&std::fs::read_link(&src)?, &entry.rel_path)?;
            ctx.symlinks_copied.fetch_add(1, Ordering::Relaxed);
        }
        CopyEntryKind::File => {
            let metadata = destination.copy_file(&src, &entry.rel_path)?;
            ctx.files_copied.fetch_add(1, Ordering::Relaxed);
            ctx.file_metadata.insert(entry.rel_path.clone(), metadata);
        }
    }
    Ok(())
}
