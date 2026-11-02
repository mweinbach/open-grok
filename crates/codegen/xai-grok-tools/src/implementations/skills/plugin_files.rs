//! Discover and read textual plugin components under the canonical root captured at discovery.
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

fn outside() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "component path leaves its plugin",
    )
}

pub fn resolve(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let resolved = dunce::canonicalize(path)?;
    if !resolved.starts_with(root) {
        return Err(outside());
    }
    Ok(resolved)
}

/// Enumerate plugin entries in logical path order. Keep logical names for internal
/// symlink aliases; compare canonical ancestors to avoid following a cycle.
pub fn discover(root: &Path, skill_dirs: &[PathBuf], command_dirs: &[PathBuf]) -> Vec<PathBuf> {
    fn visit(root: &Path, dir: &Path, parents: &mut Vec<PathBuf>, files: &mut Vec<PathBuf>) {
        let Ok(resolved) = resolve(root, dir).inspect_err(|err| {
            tracing::warn!(path = %dir.display(), %err, "skipping plugin skill directory");
        }) else {
            return;
        };
        if parents.contains(&resolved) {
            return;
        }
        parents.push(resolved);
        let candidate = dir.join("SKILL.md");
        if candidate.is_file() {
            files.push(candidate);
        }
        // The generic loader includes children at depth MAX + 1.
        if parents.len() <= super::discovery::MAX_SKILL_WALK_DEPTH + 1
            && let Ok(entries) = std::fs::read_dir(dir)
        {
            let mut dirs: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            for subdir in dirs {
                visit(root, &subdir, parents, files);
            }
        }
        parents.pop();
    }

    let mut paths = Vec::new();
    for dir in skill_dirs {
        visit(root, dir, &mut Vec::new(), &mut paths);
    }
    for dir in command_dirs {
        paths.extend(markdown_files(root, dir));
    }
    paths
}

/// Flat command and agent files are also subject to the plugin root.
pub fn markdown_files(root: &Path, dir: &Path) -> Vec<PathBuf> {
    let Ok(resolved) = resolve(root, dir) else {
        return Vec::new();
    };
    super::discovery::scan_md_files(&resolved)
        .into_iter()
        .filter_map(|path| {
            let logical = dir.join(path.file_name()?);
            resolve(root, &logical)
                .inspect_err(|err| tracing::warn!(path = %logical.display(), %err, "skipping plugin component"))
                .ok().map(|_| logical)
        })
        .collect()
}

/// Open a regular plugin component beneath its captured canonical root.
pub fn open(root: &Path, path: &Path) -> io::Result<File> {
    let resolved = resolve(root, path)?;
    let file = open_resolved(&resolved)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular plugin file",
        ));
    }
    Ok(file)
}

// Follow legitimate internal symlinks during canonicalization, then open every
// component of the resolved path without following replacements.
#[cfg(unix)]
fn open_resolved(path: &Path) -> io::Result<File> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    use std::path::Component;

    let mut components = path.components().peekable();
    if components.next() != Some(Component::RootDir) {
        return Err(outside());
    }
    let mut file = File::open("/")?;
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(outside());
        };
        let mut flags = OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK;
        if components.peek().is_some() {
            flags |= OFlag::O_DIRECTORY;
        }
        file = File::from(openat(&file, name, flags, Mode::empty())?);
    }
    Ok(file)
}

#[cfg(windows)]
fn open_resolved(path: &Path) -> io::Result<File> {
    use std::ffi::OsString;
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW};

    let file = File::open(path)?;
    let mut buf = vec![0u16; 32768];
    // SAFETY: file owns the handle and buf is a writable UTF-16 buffer.
    let n = unsafe {
        GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut buf, FILE_NAME_NORMALIZED)
    } as usize;
    if n == 0 || n >= buf.len() {
        return Err(outside());
    }
    let final_path = PathBuf::from(OsString::from_wide(&buf[..n]));
    if dunce::simplified(&final_path) != path {
        return Err(outside());
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_resolved(_: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "confined plugin reads are unavailable",
    ))
}
