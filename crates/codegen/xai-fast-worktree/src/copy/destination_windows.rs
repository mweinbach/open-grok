//! Pin the current directory chain with reparse-point handles that do not
//! share write/delete access. Discard the old chain when changing parents.
use super::DestinationConflict;
use std::fs::{self, File, Metadata};
use std::io::{self, ErrorKind};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
};

pub(super) struct Destination {
    root: PathBuf,
    current: PathBuf,
    pins: Vec<File>,
}

impl Destination {
    pub(super) fn open(root: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(root)?;
        Ok(Self {
            root: root.into(),
            current: PathBuf::new(),
            pins: vec![pin(root)?],
        })
    }

    pub(super) fn create_dir(&mut self, path: &Path) -> anyhow::Result<()> {
        if path == self.current {
            return Ok(());
        }
        self.pins.truncate(1);
        self.current.clear();
        let mut absolute = self.root.clone();
        for component in path.components() {
            let Component::Normal(name) = component else {
                return Err(DestinationConflict.into());
            };
            absolute.push(name);
            match fs::create_dir(&absolute) {
                Ok(()) => {}
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            self.pins.push(pin(&absolute)?);
        }
        self.current = path.into();
        Ok(())
    }

    pub(super) fn copy_file(&mut self, src: &Path, path: &Path) -> anyhow::Result<Metadata> {
        let permissions = fs::metadata(src)?.permissions();
        self.publish(path, false, |target| {
            // ReFS clones do not transfer the read-only attribute. Set that
            // attribute only via the handle to our freshly created inode.
            if !permissions.readonly() {
                match reflink_copy::reflink(src, target) {
                    Ok(()) => return fs::symlink_metadata(target),
                    Err(err)
                        if matches!(
                            err.kind(),
                            ErrorKind::AlreadyExists | ErrorKind::StorageFull
                        ) =>
                    {
                        return Err(err);
                    }
                    Err(_) => {}
                }
            }
            let mut source = File::open(src)?;
            let mut file = File::options()
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ)
                .open(target)?;
            let result = (|| {
                io::copy(&mut source, &mut file)?;
                file.set_permissions(permissions.clone())?;
                file.metadata()
            })();
            if result.is_err() {
                drop(file);
                let _ = fs::remove_file(target);
            }
            result
        })
    }

    pub(super) fn symlink(&mut self, target: &Path, path: &Path) -> anyhow::Result<()> {
        self.publish(path, true, |link| {
            std::os::windows::fs::symlink_file(target, link)
        })
    }

    fn publish<T>(
        &mut self,
        path: &Path,
        replacing_link: bool,
        mut create_new: impl FnMut(&Path) -> io::Result<T>,
    ) -> anyhow::Result<T> {
        let parent = path.parent().ok_or(DestinationConflict)?;
        self.create_dir(parent)?;
        let target = self
            .root
            .join(parent)
            .join(path.file_name().ok_or(DestinationConflict)?);
        match create_new(&target) {
            Ok(value) => return Ok(value),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = fs::symlink_metadata(&target)?;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            if !replacing_link || !metadata.is_symlink() {
                return Err(DestinationConflict.into());
            }
        } else if !metadata.is_file() {
            return Err(DestinationConflict.into());
        }
        let staged = tempfile::Builder::new()
            .prefix(".open-grok-copy-")
            .make_in(self.root.join(parent), create_new)?;
        let (value, mut staged) = staged.into_parts();
        // tempfile's Windows persist clears file attributes, including read-only.
        fs::rename(&staged, target)?;
        staged.disable_cleanup(true);
        Ok(value)
    }
}

fn pin(path: &Path) -> anyhow::Result<File> {
    let file = File::options()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(DestinationConflict.into());
    }
    Ok(file)
}
