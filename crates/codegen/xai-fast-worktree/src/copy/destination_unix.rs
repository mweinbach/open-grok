//! Destination writes are relative to pinned directories. No output operation
//! follows a destination symlink or writes into an already-existing inode.
use super::DestinationConflict;
use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io::{self, ErrorKind};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(super) struct Destination {
    root: File,
    // Keep at most one extra descriptor per worker, including on deep trees.
    last_parent: Option<(PathBuf, File)>,
}

impl Destination {
    pub(super) fn open(root: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(root)?;
        Ok(Self {
            root: fs::open(root, DIR_FLAGS, Mode::empty())
                .map_err(io::Error::from)?
                .into(),
            last_parent: None,
        })
    }

    fn directory(&mut self, path: &Path) -> anyhow::Result<&File> {
        if path.as_os_str().is_empty() {
            return Ok(&self.root);
        }
        if self
            .last_parent
            .as_ref()
            .is_none_or(|(cached, _)| cached != path)
        {
            let mut dir = self.root.try_clone()?;
            for component in path.components() {
                let Component::Normal(name) = component else {
                    return Err(DestinationConflict.into());
                };
                let mut opened = fs::openat(&dir, name, DIR_FLAGS, Mode::empty());
                if matches!(opened, Err(Errno::NOENT)) {
                    match fs::mkdirat(&dir, name, Mode::from_raw_mode(0o777)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(err) => return Err(io::Error::from(err).into()),
                    }
                    opened = fs::openat(&dir, name, DIR_FLAGS, Mode::empty());
                }
                dir = match opened {
                    Ok(fd) => fd.into(),
                    Err(Errno::LOOP | Errno::NOTDIR) => return Err(DestinationConflict.into()),
                    Err(err) => return Err(io::Error::from(err).into()),
                };
            }
            self.last_parent = Some((path.to_owned(), dir));
        }
        Ok(&self.last_parent.as_ref().expect("cached directory").1)
    }

    pub(super) fn create_dir(&mut self, path: &Path) -> anyhow::Result<()> {
        self.directory(path).map(|_| ())
    }

    pub(super) fn copy_file(&mut self, src: &Path, path: &Path) -> anyhow::Result<Metadata> {
        let mut source = File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(src)?;
        if !source.metadata()?.is_file() {
            anyhow::bail!("source is not a regular file");
        }
        self.publish(path, false, |dir, name| clone_new(&mut source, dir, name))
    }

    pub(super) fn symlink(&mut self, target: &Path, path: &Path) -> anyhow::Result<()> {
        self.publish(path, true, |dir, name| {
            Ok(fs::symlinkat(target, dir, name)?)
        })
    }

    fn publish<T>(
        &mut self,
        path: &Path,
        replacing_link: bool,
        mut create_new: impl FnMut(&File, &OsStr) -> io::Result<T>,
    ) -> anyhow::Result<T> {
        let name = path.file_name().ok_or(DestinationConflict)?;
        let dir = self.directory(path.parent().ok_or(DestinationConflict)?)?;
        match create_new(dir, name) {
            Ok(value) => return Ok(value),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => {}
                FileType::Symlink if replacing_link => {}
                _ => return Err(DestinationConflict.into()),
            },
            Err(Errno::NOENT) => {}
            Err(error) => return Err(io::Error::from(error).into()),
        }
        // Replace regular files by renaming a new inode. In-place truncation
        // or chmod would also alter a hard link outside this worktree.
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..64 {
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            let tmp = format!(".open-grok-copy-{}-{seq}", std::process::id());
            let value = match create_new(dir, OsStr::new(&tmp)) {
                Ok(value) => value,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            if let Err(err) = fs::renameat(dir, tmp.as_str(), dir, name) {
                let _ = fs::unlinkat(dir, tmp.as_str(), AtFlags::empty());
                return Err(io::Error::from(err).into());
            }
            return Ok(value);
        }
        anyhow::bail!("could not allocate a temporary name for worktree copy")
    }
}

fn clone_new(source: &mut File, dir: &File, name: &OsStr) -> io::Result<Metadata> {
    #[cfg(target_os = "macos")]
    match fs::fclonefileat(&*source, dir, name, fs::CloneFlags::NOOWNERCOPY) {
        Ok(()) => {
            let file = File::from(fs::openat(
                dir,
                name,
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            return file.metadata();
        }
        Err(Errno::EXIST) => return Err(ErrorKind::AlreadyExists.into()),
        Err(Errno::XDEV | Errno::NOTSUP | Errno::OPNOTSUPP | Errno::NOSYS | Errno::INVAL) => {}
        Err(err) => return Err(err.into()),
    }
    let flags = OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let output = File::from(fs::openat(dir, name, flags, Mode::from_raw_mode(0o600))?);
    let result = (|| {
        #[cfg(target_os = "macos")]
        {
            // Match std::fs::copy on a cross-volume copy, including ACLs and
            // extended attributes, using only the already-open descriptors.
            let state = fs::copyfile_state_alloc()?;
            // SAFETY: this state was allocated above and is exclusively ours.
            let copied = unsafe { fs::fcopyfile(&*source, &output, state, fs::CopyfileFlags::ALL) };
            // SAFETY: fcopyfile finished and this is the state's only free.
            let freed = unsafe { fs::copyfile_state_free(state) };
            copied?;
            freed?;
        }
        #[cfg(target_os = "linux")]
        let cloned = match fs::ioctl_ficlone(&output, &*source) {
            Ok(()) => true,
            Err(Errno::NOSPC) => return Err(io::Error::from(Errno::NOSPC)),
            Err(_) => false,
        };
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let cloned = false;
        #[cfg(not(target_os = "macos"))]
        if !cloned {
            io::copy(source, &mut &output)?;
        }
        output.set_permissions(source.metadata()?.permissions())?;
        output.metadata()
    })();
    if result.is_err() {
        let _ = fs::unlinkat(dir, name, AtFlags::empty());
    }
    result
}
