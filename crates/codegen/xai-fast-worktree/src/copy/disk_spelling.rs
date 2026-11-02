//! Resolve Git's path spelling to the directory entry used by the copy walker.
//!
//! On macOS, APFS can look up NFC/NFD or case variants as the same entry.
//! Ask the filesystem for its name instead of inferring aliases from strings
//! or inode numbers (distinct directory entries can be hard links).
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

struct Directory {
    relative: PathBuf,
    names: HashSet<OsString>,
}

pub(super) struct DiskSpellings {
    root: PathBuf,
    // Keyed by index spelling. Sparse directories, missing directories and
    // symlink parents have no children in a physical worktree walk.
    parents: HashMap<PathBuf, Option<Directory>>,
}

impl DiskSpellings {
    pub(super) fn new(source: &Path) -> Result<Self> {
        Ok(Self {
            // Absolute paths may start with a system alias such as /var or /tmp.
            root: std::fs::canonicalize(source)?,
            parents: HashMap::new(),
        })
    }

    pub(super) fn resolve(&mut self, relative: &Path) -> Result<Option<PathBuf>> {
        let parent = relative
            .parent()
            .context("expected a Git path relative to the worktree")?;
        let leaf = relative
            .file_name()
            .context("expected a Git path component")?;
        if !self.parents.contains_key(parent) {
            let physical = if parent.as_os_str().is_empty() {
                Some(PathBuf::new())
            } else {
                self.resolve(parent)?
            };
            let listing = match physical {
                Some(path) => self.list_directory(path)?,
                None => None,
            };
            self.parents.insert(parent.to_owned(), listing);
        }
        let Some(dir) = &self.parents[parent] else {
            return Ok(None);
        };
        // This also distinguishes hardlinked but differently named entries on
        // a case-sensitive volume. No per-file syscall on the exact-name path.
        if dir.names.contains(leaf) {
            return Ok(Some(dir.relative.join(leaf)));
        }
        let Some(name) = entry_name(&self.root.join(&dir.relative).join(leaf))? else {
            return Ok(None);
        };
        anyhow::ensure!(
            dir.names.contains(&name),
            "Git path changed while resolving its on-disk spelling: {}",
            relative.display()
        );
        Ok(Some(dir.relative.join(name)))
    }

    fn list_directory(&self, relative: PathBuf) -> Result<Option<Directory>> {
        let path = self.root.join(&relative);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if missing_path(&error) => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        };
        if !meta.is_dir() || meta.is_symlink() {
            return Ok(None);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
            if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Ok(None);
            }
        }
        let names = std::fs::read_dir(&path)?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<std::io::Result<_>>()?;
        Ok(Some(Directory { relative, names }))
    }
}

fn missing_path(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
}

#[cfg(windows)]
fn entry_name(path: &Path) -> Result<Option<OsString>> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{FindClose, FindFirstFileW, WIN32_FIND_DATAW};

    // FindFirstFileW accepts wildcards in the final component. This lookup
    // must identify one literal filename, even for a malformed index.
    let leaf = path.file_name().context("missing filename")?;
    anyhow::ensure!(
        !leaf.encode_wide().any(|c| matches!(c, 0 | 0x2a | 0x3f)),
        "invalid Git filename: {}",
        path.display()
    );
    let wide: Vec<_> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut data = WIN32_FIND_DATAW::default();
    // SAFETY: wide is a NUL-terminated pathname and data is a writable result.
    let handle = unsafe { FindFirstFileW(wide.as_ptr(), &raw mut data) };
    if handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        if missing_path(&error) {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("read directory entry spelling for {}", path.display()));
    }
    // SAFETY: the successful find handle is exclusively owned here.
    unsafe { FindClose(handle) };
    let end = data
        .cFileName
        .iter()
        .position(|&c| c == 0)
        .context("unterminated Windows filename")?;
    Ok(Some(OsString::from_wide(&data.cFileName[..end])))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn entry_name(path: &Path) -> Result<Option<OsString>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if missing_path(&error) => Ok(None),
        Err(error) => Err(error.into()),
        // An unusual filesystem resolved an alias that was not enumerated.
        // Inode matching alone could select a different hardlink; stop safely.
        Ok(_) => anyhow::bail!(
            "cannot determine the physical directory entry for {}",
            path.display()
        ),
    }
}

#[cfg(target_os = "macos")]
fn entry_name(path: &Path) -> anyhow::Result<Option<std::ffi::OsString>> {
    use std::ffi::{CStr, CString, OsStr};
    use std::os::unix::ffi::OsStrExt;

    #[repr(C)]
    struct Reply {
        length: u32,
        reference: libc::attrreference_t,
        data: [u8; libc::PATH_MAX as usize],
    }

    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: libc::ATTR_CMN_NAME,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    let mut reply = Reply {
        length: 0,
        reference: libc::attrreference_t {
            attr_dataoffset: 0,
            attr_length: 0,
        },
        data: [0; libc::PATH_MAX as usize],
    };
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: the pathname is NUL terminated, and both structs are writable
    // repr(C) buffers of the declared sizes. NOFOLLOW_ANY permits a symlink
    // leaf but refuses to traverse any symlink in an indexed parent path.
    let status = unsafe {
        libc::getattrlist(
            c_path.as_ptr(),
            (&raw mut attributes).cast(),
            (&raw mut reply).cast(),
            size_of::<Reply>(),
            libc::FSOPT_NOFOLLOW_ANY.into(),
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if missing_path(&error) || error.raw_os_error() == Some(libc::ELOOP) {
            return Ok(None);
        }
        return Err(error)
            .with_context(|| format!("read directory entry spelling for {}", path.display()));
    }
    // attr_dataoffset is relative to the reference, not the start of the reply.
    let start = usize::try_from(reply.reference.attr_dataoffset)?
        .checked_sub(size_of::<libc::attrreference_t>())
        .context("invalid macOS filename offset")?;
    let data_len = (reply.length as usize)
        .checked_sub(std::mem::offset_of!(Reply, data))
        .context("invalid macOS filename response length")?;
    let bytes = reply
        .data
        .get(..data_len)
        .and_then(|data| data.get(start..))
        .and_then(|data| data.get(..reply.reference.attr_length as usize))
        .context("invalid macOS filename response")?;
    Ok(Some(
        OsStr::from_bytes(CStr::from_bytes_with_nul(bytes)?.to_bytes()).to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_spelling_distinguishes_hardlinked_directory_entries() {
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        std::fs::write(root.join("lower"), "fixture").unwrap();
        std::fs::hard_link(root.join("lower"), root.join("different")).unwrap();
        let case_sensitive = !root.join("LOWER").exists();
        if case_sensitive {
            std::fs::hard_link(root.join("lower"), root.join("LOWER")).unwrap();
        }
        let mut paths = DiskSpellings::new(root).unwrap();
        for name in ["lower", "different"] {
            assert_eq!(
                paths.resolve(Path::new(name)).unwrap(),
                Some(PathBuf::from(name))
            );
        }
        assert_eq!(
            paths.resolve(Path::new("LOWER")).unwrap(),
            Some(PathBuf::from(if case_sensitive {
                "LOWER"
            } else {
                "lower"
            }))
        );
    }

    #[cfg(unix)]
    #[test]
    fn physical_spelling_can_classify_dangling_links_and_fifos() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path();
        std::os::unix::fs::symlink("missing-target", root.join("dangling")).unwrap();
        let fifo = CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo is a NUL-terminated path in this test's private directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let mut paths = DiskSpellings::new(root).unwrap();
        for name in ["dangling", "fifo"] {
            assert_eq!(
                paths.resolve(Path::new(name)).unwrap(),
                Some(PathBuf::from(name))
            );
            let upper = name.to_uppercase();
            if std::fs::symlink_metadata(root.join(&upper)).is_ok() {
                assert_eq!(
                    paths.resolve(Path::new(&upper)).unwrap(),
                    Some(PathBuf::from(name))
                );
            }
        }
    }
}
