//! Worker logic for replicating a single filesystem entry.

use std::collections::HashSet;
use std::fs::Metadata;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::{DashMap, DashSet};

use crate::copy::cow;
use crate::copy::types::{CopyEntry, CopyEntryKind};

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
    let mut created_dirs = HashSet::new();
    let mut fatal = None;
    for entry in rx {
        // Drain the bounded queue even on failure, so the producer can finish
        // and the caller can join every worker before removing the destination.
        if fatal.is_some() {
            continue;
        }
        match process_entry(&entry, &ctx, &mut created_dirs) {
            Ok(()) => {
                ctx.handled_paths.insert(entry.rel_path);
            }
            Err(error) => {
                let error = error.context(format!("copy {}", entry.rel_path.display()));
                if entry.tracked || crate::api::is_out_of_disk(&error) {
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
    created_dirs: &mut HashSet<PathBuf>,
) -> anyhow::Result<()> {
    let src = ctx.source.join(&entry.rel_path);
    let dst = ctx.dest.join(&entry.rel_path);
    if let Some(parent) = dst.parent()
        && !parent.as_os_str().is_empty()
        && created_dirs.insert(parent.to_path_buf())
    {
        std::fs::create_dir_all(parent)?;
    }
    match entry.kind {
        CopyEntryKind::Dir => {
            std::fs::create_dir_all(&dst)?;
            ctx.dirs_created.fetch_add(1, Ordering::Relaxed);
        }
        CopyEntryKind::Symlink => {
            cow::replace_symlink(&std::fs::read_link(&src)?, &dst)?;
            ctx.symlinks_copied.fetch_add(1, Ordering::Relaxed);
        }
        CopyEntryKind::File => {
            cow::clone_file(&src, &dst)?;
            ctx.files_copied.fetch_add(1, Ordering::Relaxed);
            if let Ok(metadata) = std::fs::metadata(&dst) {
                ctx.file_metadata.insert(entry.rel_path.clone(), metadata);
            }
        }
    }
    Ok(())
}
