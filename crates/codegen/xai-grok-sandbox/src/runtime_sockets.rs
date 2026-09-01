use std::io;
use std::path::{Path, PathBuf};

pub(crate) const BWRAP_RUNTIME_SOCKET_DENY_ENV_VAR: &str = "__GROK_BWRAP_RUNTIME_SOCKET_DENY";

const SYSTEM_SOCKETS: &[&str] = &[
    "/run/docker.sock",
    "/var/run/docker.sock",
    "/run/podman/podman.sock",
    "/var/run/podman/podman.sock",
    "/run/containerd/containerd.sock",
    "/var/run/containerd/containerd.sock",
];

#[cfg(unix)]
const PER_UID_SOCKET_SUFFIXES: &[&str] = &[
    "docker.sock",
    "podman/podman.sock",
    "containerd/containerd.sock",
];

const PER_HOME_SOCKET_SUFFIXES: &[&str] =
    &[".docker/desktop/docker.sock", ".docker/run/docker.sock"];

pub(crate) fn runtime_socket_deny_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = SYSTEM_SOCKETS.iter().copied().map(PathBuf::from).collect();
    #[cfg(unix)]
    {
        let uid = unsafe { libc::getuid() };
        for suffix in PER_UID_SOCKET_SUFFIXES {
            paths.push(PathBuf::from(format!("/run/user/{uid}/{suffix}")));
        }
    }
    if let Some(home) = dirs::home_dir() {
        for suffix in PER_HOME_SOCKET_SUFFIXES {
            paths.push(home.join(suffix));
        }
    }
    paths
}

pub(crate) fn materialize_runtime_socket_deny_paths() -> io::Result<Vec<PathBuf>> {
    materialize_runtime_socket_deny_paths_from(runtime_socket_deny_paths())
}

fn runtime_socket_deny_paths_for_resolution() -> io::Result<Vec<PathBuf>> {
    if !(cfg!(target_os = "linux") && crate::is_inside_bwrap()) {
        return materialize_runtime_socket_deny_paths();
    }
    runtime_socket_deny_paths_for_context_with_policy(
        std::env::var(BWRAP_RUNTIME_SOCKET_DENY_ENV_VAR),
        runtime_socket_deny_paths(),
    )
}

fn runtime_socket_deny_paths_for_context_with_policy(
    handed: Result<String, std::env::VarError>,
    allowed: Vec<PathBuf>,
) -> io::Result<Vec<PathBuf>> {
    match handed {
        Ok(encoded) => decode_bwrap_runtime_socket_denies_with_policy(&encoded, allowed),
        Err(std::env::VarError::NotPresent) => Ok(Vec::new()),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid bwrap runtime-socket deny handoff: {error}"),
        )),
    }
}

fn materialize_runtime_socket_deny_paths_from(
    candidates: impl IntoIterator<Item = PathBuf>,
) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for candidate in candidates {
        let with_context = |error: io::Error| {
            io::Error::new(
                error.kind(),
                format!(
                    "could not resolve runtime-socket deny path {}: {error}",
                    candidate.display()
                ),
            )
        };
        let parent = candidate.parent().ok_or_else(|| {
            with_context(io::Error::new(
                io::ErrorKind::InvalidInput,
                "endpoint has no parent directory",
            ))
        })?;
        let file_name = candidate.file_name().ok_or_else(|| {
            with_context(io::Error::new(
                io::ErrorKind::InvalidInput,
                "endpoint has no file name",
            ))
        })?;
        let canonical_parent = match dunce::canonicalize(parent) {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&candidate) {
                    Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {
                        continue;
                    }
                    Ok(_) => return Err(with_context(error)),
                    Err(metadata_error) => return Err(with_context(metadata_error)),
                }
            }
            Err(error) => return Err(with_context(error)),
        };
        let path = canonical_parent.join(file_name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(with_context(error)),
        };
        if metadata.file_type().is_symlink() {
            return Err(with_context(io::Error::new(
                io::ErrorKind::InvalidInput,
                "endpoint is a symlink",
            )));
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

pub(crate) fn encode_bwrap_runtime_socket_denies(paths: &[PathBuf]) -> io::Result<String> {
    serde_json::to_string(paths).map_err(io::Error::other)
}

fn decode_bwrap_runtime_socket_denies_with_policy(
    encoded: &str,
    policy: Vec<PathBuf>,
) -> io::Result<Vec<PathBuf>> {
    let handed: Vec<PathBuf> = serde_json::from_str(encoded).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid bwrap runtime-socket deny handoff: {error}"),
        )
    })?;
    let mut paths = Vec::new();
    for path in handed {
        if !path.is_absolute() || !runtime_socket_policy_contains(&policy, &path)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "bwrap runtime-socket deny handoff path {} is not in the automatic policy",
                    path.display()
                ),
            ));
        }
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn runtime_socket_policy_contains(policy: &[PathBuf], handed: &Path) -> io::Result<bool> {
    if policy.iter().any(|candidate| candidate == handed) {
        return Ok(true);
    }
    let Some(handed_file_name) = handed.file_name() else {
        return Ok(false);
    };
    for candidate in policy {
        if candidate.file_name() != Some(handed_file_name) {
            continue;
        }
        let Some(parent) = candidate.parent() else {
            continue;
        };
        if normalize_existing_parent_alias(parent)?.join(handed_file_name) == handed {
            return Ok(true);
        }
    }
    Ok(false)
}

fn normalize_existing_parent_alias(parent: &Path) -> io::Result<PathBuf> {
    let mut missing_suffix = Vec::new();
    let mut existing = parent;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let Some(file_name) = existing.file_name() else {
            return Ok(parent.to_path_buf());
        };
        missing_suffix.push(file_name.to_os_string());
        let Some(next) = existing.parent() else {
            return Ok(parent.to_path_buf());
        };
        existing = next;
    }
    let mut normalized = dunce::canonicalize(existing)?;
    for component in missing_suffix.iter().rev() {
        normalized.push(component);
    }
    Ok(normalized)
}

pub(crate) fn append_runtime_socket_denies(
    deny: &mut Vec<PathBuf>,
    auto_sockets: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let policy = runtime_socket_deny_paths();
    let materialized = runtime_socket_deny_paths_for_resolution()?;
    merge_runtime_socket_denies(deny, &materialized, &policy)?;
    for path in materialized {
        if !auto_sockets.contains(&path) {
            auto_sockets.push(path);
        }
    }
    Ok(())
}

fn merge_runtime_socket_denies(
    deny: &mut Vec<PathBuf>,
    auto_sockets: &[PathBuf],
    static_policy: &[PathBuf],
) -> io::Result<()> {
    let mut retained = Vec::with_capacity(deny.len() + auto_sockets.len());
    for path in &*deny {
        let is_covered_auto_socket = if !static_policy.contains(path) {
            false
        } else if auto_sockets.contains(path) {
            true
        } else if let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) {
            auto_sockets
                .iter()
                .any(|socket| socket.file_name() == Some(file_name))
                && auto_sockets.contains(&normalize_existing_parent_alias(parent)?.join(file_name))
        } else {
            false
        };
        if !is_covered_auto_socket {
            retained.push(path.clone());
        }
    }
    for path in auto_sockets {
        if !retained.contains(path) {
            retained.push(path.clone());
        }
    }
    *deny = retained;
    Ok(())
}

#[cfg(test)]
#[path = "runtime_sockets_tests.rs"]
mod tests;
