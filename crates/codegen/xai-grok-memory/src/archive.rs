//! Build a `memory.tar.gz` archive containing session logs and MEMORY.md files.
//!
//! The archive is uploaded to GCS at session finalize time. The reconstruct
//! pipeline injects these into the Docker image for full replay fidelity.

use anyhow::{Context, Result};

use super::MemoryStorage;

const MAX_MEMORY_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn open_regular_nofollow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    Ok(file)
}

fn append_file_snapshot<Writer: std::io::Write>(
    archive: &mut tar::Builder<Writer>,
    path: &std::path::Path,
    name: &str,
) -> Result<()> {
    use std::io::Read;

    let file = match open_regular_nofollow(path) {
        Ok(file) => file,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipping unreadable memory file");
            return Ok(());
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipping unstatable memory file");
            return Ok(());
        }
    };
    if metadata.len() > MAX_MEMORY_FILE_BYTES {
        tracing::warn!(path = %path.display(), "skipping oversized memory file");
        return Ok(());
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_MEMORY_FILE_BYTES + 1).read_to_end(&mut bytes) {
        tracing::warn!(path = %path.display(), %error, "skipping unreadable memory file");
        return Ok(());
    }
    if bytes.len() as u64 > MAX_MEMORY_FILE_BYTES {
        tracing::warn!(path = %path.display(), "skipping oversized memory file");
        return Ok(());
    }
    let mut header = tar::Header::new_gnu();
    header.set_metadata(&metadata);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    archive
        .append_data(&mut header, name, bytes.as_slice())
        .with_context(|| format!("archive {name}"))
}

/// Build a `memory.tar.gz` archive with session logs and MEMORY.md files.
pub fn build_memory_archive(storage: &MemoryStorage) -> Result<Vec<u8>> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let buf = Vec::new();
    let enc = GzEncoder::new(buf, Compression::default());
    let mut ar = tar::Builder::new(enc);

    // Session logs
    let sessions_dir = storage.workspace_dir().join("sessions");
    if sessions_dir.is_dir() {
        for entry in std::fs::read_dir(&sessions_dir)
            .context("read sessions dir")?
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = format!("workspace/sessions/{}", entry.file_name().to_string_lossy());
                append_file_snapshot(&mut ar, &path, &name)?;
            }
        }
    }

    // MEMORY.md files
    let global_mem = storage.global_memory_file();
    if global_mem.is_file() {
        append_file_snapshot(&mut ar, &global_mem, "global/MEMORY.md")?;
    }

    let workspace_mem = storage.workspace_memory_file();
    if workspace_mem.is_file() {
        let ws_dir_name = storage
            .workspace_dir()
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let archive_path = format!("{ws_dir_name}/MEMORY.md");
        append_file_snapshot(&mut ar, &workspace_mem, &archive_path)?;
    }

    let enc = ar.into_inner().context("finalize tar")?;
    enc.finish().context("compress tar.gz")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_storage(tmp: &TempDir) -> MemoryStorage {
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        MemoryStorage::with_paths(global, workspace)
    }

    #[test]
    fn test_build_empty_archive() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        let archive = build_memory_archive(&storage).unwrap();
        assert!(!archive.is_empty());
    }

    #[test]
    fn test_build_archive_with_files() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();
        storage
            .write_daily_log("2026-03-09", "test", "sess12345678", "# Test", false)
            .unwrap();

        let archive = build_memory_archive(&storage).unwrap();
        assert!(archive.len() > 100);
    }

    #[test]
    fn test_build_archive_includes_memory_md() {
        let tmp = TempDir::new().unwrap();
        let storage = test_storage(&tmp);
        storage.ensure_initialized().unwrap();

        std::fs::write(storage.global_memory_file(), "# Global Memory").unwrap();
        std::fs::write(storage.workspace_memory_file(), "# Workspace Memory").unwrap();

        let archive = build_memory_archive(&storage).unwrap();
        let entries = tar_entry_names(&archive);
        assert!(entries.contains(&"global/MEMORY.md".to_string()));
        assert!(entries.contains(&"test_ws/MEMORY.md".to_string()));
    }

    fn tar_entry_names(gz_bytes: &[u8]) -> Vec<String> {
        use flate2::read::GzDecoder;
        let decoder = GzDecoder::new(gz_bytes);
        let mut archive = tar::Archive::new(decoder);
        archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().display().to_string())
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn archive_skips_symlinked_memory_and_session_files() {
        let directory = TempDir::new().unwrap();
        let storage = test_storage(&directory);
        storage.ensure_initialized().unwrap();
        let secret = directory.path().join("secret");
        std::fs::write(&secret, "must not be archived").unwrap();
        let global = storage.global_memory_file();
        let _ = std::fs::remove_file(&global);
        std::os::unix::fs::symlink(&secret, &global).unwrap();
        let sessions = storage.workspace_dir().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(&secret, sessions.join("linked.md")).unwrap();

        let entries = tar_entry_names(&build_memory_archive(&storage).unwrap());
        assert!(!entries.iter().any(|name| name == "global/MEMORY.md"));
        assert!(!entries.iter().any(|name| name.ends_with("linked.md")));
    }

    #[cfg(unix)]
    #[test]
    fn archive_skips_fifo_without_blocking() {
        let directory = TempDir::new().unwrap();
        let storage = test_storage(&directory);
        storage.ensure_initialized().unwrap();
        let sessions = storage.workspace_dir().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("pipe.md");
        let path_bytes = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);

        let entries = tar_entry_names(&build_memory_archive(&storage).unwrap());
        assert!(!entries.iter().any(|name| name.ends_with("pipe.md")));
    }

    #[test]
    fn archive_skips_oversized_files() {
        let directory = TempDir::new().unwrap();
        let storage = test_storage(&directory);
        storage.ensure_initialized().unwrap();
        std::fs::File::create(storage.global_memory_file())
            .unwrap()
            .set_len(MAX_MEMORY_FILE_BYTES + 1)
            .unwrap();

        let entries = tar_entry_names(&build_memory_archive(&storage).unwrap());
        assert!(!entries.iter().any(|name| name == "global/MEMORY.md"));
    }

    #[test]
    fn snapshot_header_and_content_agree() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("note.md");
        let content = b"# Memory\ncurrent snapshot\n";
        std::fs::write(&source, content).unwrap();
        let mut archive = tar::Builder::new(Vec::new());
        append_file_snapshot(&mut archive, &source, "note.md").unwrap();
        let bytes = archive.into_inner().unwrap();
        let mut reader = tar::Archive::new(bytes.as_slice());
        let mut entries = reader.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.header().size().unwrap(), content.len() as u64);
        let mut restored = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut restored).unwrap();
        assert_eq!(restored, content);
        assert!(entries.next().is_none());
    }
}
