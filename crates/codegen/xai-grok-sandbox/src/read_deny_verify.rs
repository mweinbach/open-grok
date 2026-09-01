use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[cfg(target_os = "linux")]
const SENTINEL_DIR_NAME: &str = "sandbox-bwrap-sentinel";

#[cfg(target_os = "linux")]
const MOUNTINFO_MAX_BYTES: u64 = 8 * 1024 * 1024;
#[cfg(target_os = "linux")]
const STATX_MOUNT_ID_MASK: u32 = 0x1000;
#[cfg(target_os = "linux")]
const AT_EMPTY_PATH_FLAG: libc::c_int = 0x1000;

#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
struct MountInfoEntry {
    id: u64,
    mountpoint: PathBuf,
    is_read_only: bool,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxStatxMountId {
    stx_mask: u32,
    _before_mount_id: [u8; 140],
    stx_mnt_id: u64,
    _after_mount_id: [u8; 104],
}

#[cfg(target_os = "linux")]
pub(crate) fn ensure_bwrap_sentinel_dir() -> Result<PathBuf, String> {
    let parent = crate::paths::grok_home();
    std::fs::create_dir_all(&parent)
        .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    ensure_sentinel_dir_under(&parent)
}

#[cfg(target_os = "linux")]
fn ensure_sentinel_dir_under(parent: &Path) -> Result<PathBuf, String> {
    let path = parent.join(SENTINEL_DIR_NAME);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            std::fs::remove_file(&path).map_err(|e| {
                format!(
                    "could not replace non-directory sentinel {}: {e}",
                    path.display()
                )
            })?;
            std::fs::create_dir(&path)
                .map_err(|e| format!("could not create sentinel {}: {e}", path.display()))?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&path)
                .map_err(|e| format!("could not create sentinel {}: {e}", path.display()))?;
        }
        Err(e) => {
            return Err(format!(
                "could not inspect sentinel {}: {e}",
                path.display()
            ));
        }
    }
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(path),
        _ => Err(format!(
            "sentinel {} is not a real directory",
            path.display()
        )),
    }
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
pub(crate) fn verify_bwrap_sentinel() -> Result<(), String> {
    verify_sentinel_under(&crate::paths::grok_home())
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
fn verify_sentinel_under(parent: &Path) -> Result<(), String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let parent_dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(parent)
        .map_err(|e| {
            format!(
                "bwrap sentinel parent {} could not be opened: {e}",
                parent.display()
            )
        })?;
    if fstatvfs_is_read_only(parent_dir.as_raw_fd(), parent)? {
        return Err(format!(
            "bwrap sentinel parent {} is read-only; the sentinel mount shape cannot be verified",
            parent.display()
        ));
    }

    let name = std::ffi::CString::new(SENTINEL_DIR_NAME)
        .map_err(|_| "sentinel name contains interior NUL".to_string())?;
    let child_fd = unsafe {
        libc::openat(
            parent_dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if child_fd < 0 {
        return Err(format!(
            "bwrap sentinel {}/{SENTINEL_DIR_NAME} is not a directory reachable without \
             following symlinks: {}",
            parent.display(),
            std::io::Error::last_os_error()
        ));
    }
    let child_dir = unsafe { std::fs::File::from_raw_fd(child_fd) };

    if !fstatvfs_is_read_only(child_dir.as_raw_fd(), &parent.join(SENTINEL_DIR_NAME))? {
        return Err(format!(
            "bwrap sentinel {}/{SENTINEL_DIR_NAME} is not a read-only mount; this namespace \
             is not bwrap-confined",
            parent.display()
        ));
    }

    let parent_dev = parent_dir
        .metadata()
        .map_err(|e| format!("bwrap sentinel parent metadata failed: {e}"))?
        .dev();
    let child_dev = child_dir
        .metadata()
        .map_err(|e| format!("bwrap sentinel metadata failed: {e}"))?
        .dev();
    if parent_dev != child_dev {
        return Err(format!(
            "bwrap sentinel {}/{SENTINEL_DIR_NAME} is a mount of a foreign filesystem, not \
             the self-bind the re-exec creates",
            parent.display()
        ));
    }
    Ok(())
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
fn fstatvfs_is_read_only(fd: std::os::fd::RawFd, path: &Path) -> Result<bool, String> {
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatvfs(fd, &mut buf) } != 0 {
        return Err(format!(
            "bwrap sentinel statvfs on {} failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(buf.f_flag & libc::ST_RDONLY != 0)
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo_field(field: &str) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;

    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if index + 3 >= bytes.len()
                || !bytes[index + 1..index + 4]
                    .iter()
                    .all(|digit| (b'0'..=b'7').contains(digit))
            {
                return Err(format!("invalid mountinfo escape in {field:?}"));
            }
            let decoded_byte = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            if !matches!(decoded_byte, b' ' | b'\t' | b'\n' | b'\\') {
                return Err(format!("unsupported mountinfo escape in {field:?}"));
            }
            decoded.push(decoded_byte);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(std::ffi::OsString::from_vec(decoded).into())
}

#[cfg(target_os = "linux")]
fn parse_mountinfo_entry(line: &str) -> Result<MountInfoEntry, String> {
    let mut fields = line.split_ascii_whitespace();
    let id = fields
        .next()
        .ok_or_else(|| "mountinfo row has no mount ID".to_string())?
        .parse()
        .map_err(|e| format!("invalid mountinfo ID: {e}"))?;
    fields
        .next()
        .ok_or_else(|| "mountinfo row has no parent ID".to_string())?;
    fields
        .next()
        .ok_or_else(|| "mountinfo row has no device".to_string())?;
    fields
        .next()
        .ok_or_else(|| "mountinfo row has no root".to_string())?;
    let mountpoint = unescape_mountinfo_field(
        fields
            .next()
            .ok_or_else(|| "mountinfo row has no mountpoint".to_string())?,
    )?;
    let options = fields
        .next()
        .ok_or_else(|| "mountinfo row has no mount options".to_string())?;
    let is_read_only = options.split(',').any(|option| option == "ro");
    Ok(MountInfoEntry {
        id,
        mountpoint,
        is_read_only,
    })
}

#[cfg(target_os = "linux")]
fn read_mountinfo() -> Result<Vec<MountInfoEntry>, String> {
    use std::io::Read;

    let file = std::fs::File::open("/proc/self/mountinfo")
        .map_err(|e| format!("read-deny verification could not open mountinfo: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MOUNTINFO_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read-deny verification could not read mountinfo: {e}"))?;
    if bytes.len() as u64 > MOUNTINFO_MAX_BYTES {
        return Err("read-deny verification mountinfo exceeded 8 MiB".to_string());
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|e| format!("read-deny verification mountinfo is not UTF-8: {e}"))?;
    text.lines().map(parse_mountinfo_entry).collect()
}

#[cfg(target_os = "linux")]
fn fd_mount_id(fd: std::os::fd::RawFd, path: &Path) -> Result<u64, String> {
    let empty = c"";
    let mut statx: LinuxStatxMountId = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_statx,
            fd,
            empty.as_ptr(),
            AT_EMPTY_PATH_FLAG | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MOUNT_ID_MASK,
            &raw mut statx,
        )
    };
    if rc != 0 {
        return Err(format!(
            "read-deny path {} mount ID query failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    if statx.stx_mask & STATX_MOUNT_ID_MASK == 0 {
        return Err(format!(
            "read-deny path {} mount ID is unavailable",
            path.display()
        ));
    }
    Ok(statx.stx_mnt_id)
}

#[cfg(target_os = "linux")]
fn verify_exact_read_only_mount(path: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let target = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| format!("read-deny path {} could not be opened: {e}", path.display()))?;
    let metadata = target
        .metadata()
        .map_err(|e| format!("read-deny path {} metadata failed: {e}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "read-deny path {} is a symlink, not a mountpoint",
            path.display()
        ));
    }
    let mount_id = fd_mount_id(target.as_raw_fd(), path)?;
    verify_exact_read_only_mount_entry(path, mount_id, &read_mountinfo()?)
}

#[cfg(target_os = "linux")]
fn verify_exact_read_only_mount_entry(
    path: &Path,
    mount_id: u64,
    mountinfo: &[MountInfoEntry],
) -> Result<(), String> {
    let Some(entry) = mountinfo
        .iter()
        .rev()
        .find(|entry| entry.id == mount_id && entry.mountpoint == path)
    else {
        return Err(format!(
            "read-deny path {} is not the mountpoint for its visible mount",
            path.display()
        ));
    };
    if !entry.is_read_only {
        return Err(format!(
            "read-deny path {} mount is not read-only",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
pub fn verify_read_deny_enforced(
    profile: &crate::ProfileName,
    workspace: &Path,
) -> Result<(), String> {
    crate::hook_write_deny::ensure_namespace_lockdown()?;
    verify_bwrap_sentinel()?;
    verify_resolved_read_deny_masks(profile, workspace)
}

#[cfg(target_os = "linux")]
pub fn verify_data_write_deny_enforced(
    profile: &crate::ProfileName,
    workspace: &Path,
) -> Result<(), String> {
    if !crate::requires_data_write_deny(profile, workspace) {
        return Ok(());
    }
    crate::hook_write_deny::ensure_namespace_lockdown()?;
    verify_exact_read_only_mount(Path::new("/data"))
        .map_err(|error| format!("devbox /data write-deny could not be verified: {error}"))
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
fn verify_resolved_read_deny_masks(
    profile: &crate::ProfileName,
    workspace: &Path,
) -> Result<(), String> {
    let config = crate::profiles::load_sandbox_config(workspace);
    let (resolved, auto_sockets) = profile
        .resolve_profile_with_runtime_sockets(workspace, &config)
        .map_err(|e| format!("read-deny verification could not resolve the profile: {e}"))?;
    let (exact, globs) = crate::deny::partition_deny_entries(&resolved.deny);
    let mut paths = crate::deny::exact_deny_path_strings(workspace, &exact);
    if !globs.is_empty() {
        let expanded =
            crate::deny::expand_deny_globs(workspace, &globs, crate::deny::DENY_GLOB_CAPS)
                .map_err(|reason| {
                    format!("read-deny verification could not expand deny globs: {reason}")
                })?;
        paths.extend(expanded);
    }
    for path in &paths {
        let path = Path::new(path);
        if auto_sockets.iter().any(|socket| socket == path) {
            verify_runtime_socket_deny(path)?;
        } else {
            verify_path_masked(path)?;
        }
    }
    Ok(())
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
fn verify_runtime_socket_deny(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "runtime-socket deny path {} could not be inspected: {e}",
            path.display()
        )),
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "runtime-socket deny path {} is a symlink",
            path.display()
        )),
        Ok(meta) if meta.file_type().is_socket() => Ok(()),
        Ok(meta) if meta.permissions().mode() & 0o7777 == 0 => verify_path_masked(path),
        Ok(_) => Err(format!(
            "runtime-socket deny path {} is exposed and is neither a placeholder nor a socket",
            path.display()
        )),
    }
}

#[cfg(all(feature = "enforce", target_os = "linux"))]
fn verify_path_masked(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    let meta = std::fs::symlink_metadata(path).map_err(|e| {
        format!(
            "read-deny path {} has no placeholder mount: {e}",
            path.display()
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "read-deny path {} is a symlink, not a placeholder mount",
            path.display()
        ));
    }
    if meta.file_type().is_socket() {
        return Err(format!(
            "read-deny path {} is still a live socket",
            path.display()
        ));
    }
    if meta.permissions().mode() & 0o7777 != 0 {
        return Err(format!(
            "read-deny path {} is not a no-access placeholder",
            path.display()
        ));
    }
    verify_exact_read_only_mount(path)
}

#[cfg(not(all(feature = "enforce", target_os = "linux")))]
pub fn verify_read_deny_enforced(
    _profile: &crate::ProfileName,
    _workspace: &Path,
) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn verify_data_write_deny_enforced(
    _profile: &crate::ProfileName,
    _workspace: &Path,
) -> Result<(), String> {
    Ok(())
}

#[cfg(all(test, feature = "enforce", target_os = "linux"))]
#[path = "read_deny_verify_tests.rs"]
mod tests;
