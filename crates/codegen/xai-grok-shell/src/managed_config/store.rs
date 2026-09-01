use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::response::{ApplyOutcome, ManagedConfigResponse, ManagedConfigSource};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManagedPolicyRefusal {
    Compromised,
    Busy,
    LockUnavailable { home: PathBuf },
}

impl std::fmt::Display for ManagedPolicyRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compromised => formatter.write_str(super::MANAGED_POLICY_MISSING_MSG),
            Self::Busy => formatter.write_str(
                "Managed policy is being updated by another Open Grok process or a background policy sync, and could not be verified in time. Retry in a moment.",
            ),
            Self::LockUnavailable { home } => write!(
                formatter,
                "Managed policy could not be verified: the policy lock file under {} is not accessible. Fix permissions and start again.",
                home.display()
            ),
        }
    }
}

impl std::error::Error for ManagedPolicyRefusal {}

pub(super) fn with_gate_lock<ResultValue>(
    home: &Path,
    lock_wait: Duration,
    snapshot: impl FnOnce(&Path) -> ResultValue,
) -> Result<ResultValue, ManagedPolicyRefusal> {
    use fs2::FileExt;
    let unavailable = || ManagedPolicyRefusal::LockUnavailable {
        home: home.to_path_buf(),
    };
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("managed_config.lock"))
        .map_err(|_| unavailable())?;
    match lock_file.try_lock_exclusive() {
        Ok(()) => {}
        Err(error) if lock_is_contended(&error) => {
            let wait = || wait_for_gate_lock(&lock_file, home, lock_wait);
            match tokio::runtime::Handle::try_current() {
                Ok(handle)
                    if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread =>
                {
                    tokio::task::block_in_place(wait)?;
                }
                _ => wait()?,
            }
        }
        Err(_) => return Err(unavailable()),
    }
    Ok(snapshot(home))
}

fn lock_is_contended(error: &std::io::Error) -> bool {
    let contended = fs2::lock_contended_error();
    error.kind() == contended.kind() && error.raw_os_error() == contended.raw_os_error()
}

fn wait_for_gate_lock(
    lock_file: &std::fs::File,
    home: &Path,
    lock_wait: Duration,
) -> Result<(), ManagedPolicyRefusal> {
    use fs2::FileExt;
    let started = Instant::now();
    loop {
        let remaining = lock_wait.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(ManagedPolicyRefusal::Busy);
        }
        std::thread::sleep(Duration::from_millis(50).min(remaining));
        match lock_file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if lock_is_contended(&error) => {}
            Err(_) => {
                return Err(ManagedPolicyRefusal::LockUnavailable {
                    home: home.to_path_buf(),
                });
            }
        }
    }
}

pub(super) fn managed_config_enabled_from_layers(
    layers: &crate::config::ConfigLayers,
) -> Option<bool> {
    layers
        .effective_config_base_without_overlay()
        .get("features")?
        .get("managed_config")?
        .as_bool()
}

pub(super) fn write_failure_is_deny(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::ReadOnlyFilesystem
            | std::io::ErrorKind::ResourceBusy
    )
}

pub(super) fn evict_prior_sidecars(home: &Path) {
    for name in [
        xai_grok_config::signed_policy::SIGNATURE_SIDECAR_FILE,
        xai_grok_config::signed_policy::MANAGED_IDENTITY_SIDECAR_FILE,
    ] {
        super::remove_synced_file(home, name, "evicted prior principal's sidecar");
    }
}

pub(super) fn credential_matches(
    source: ManagedConfigSource,
    principal: Option<&str>,
    key_fingerprint: Option<&str>,
) -> bool {
    match source {
        ManagedConfigSource::DeploymentKey => super::resolve_deployment_key().is_some_and(|key| {
            key_fingerprint == Some(super::deployment_key_fingerprint(&key).as_str())
        }),
        ManagedConfigSource::TeamOauth => {
            matches!(super::team_principal_signed_in(), Ok(true))
                && super::active_team_id_any_expiry()
                    == crate::config::normalize_identity(principal)
                && key_fingerprint.is_none()
        }
    }
}

pub(super) struct ApplyIdentity<'identity> {
    pub(super) source: ManagedConfigSource,
    pub(super) principal: Option<&'identity str>,
    pub(super) key_fingerprint: Option<&'identity str>,
    pub(super) expected_team: Option<&'identity str>,
}

pub(super) fn staged_refresh_path(home: &Path) -> PathBuf {
    home.join("staged").join("managed_config_refresh.json")
}

const MAX_STAGED_REFRESH_BYTES: u64 = 8 * 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct StagedRefresh {
    pub(super) source: ManagedConfigSource,
    pub(super) principal: Option<String>,
    pub(super) key_fingerprint: Option<String>,
    pub(super) response: ManagedConfigResponse,
    #[serde(default)]
    pub(super) parked_at: u64,
}

pub(super) fn stage_refresh(
    home: &Path,
    body: &ManagedConfigResponse,
    source: ManagedConfigSource,
    principal: Option<&str>,
    key_fingerprint: Option<&str>,
) -> std::io::Result<()> {
    let staged = StagedRefresh {
        source,
        principal: principal.map(str::to_owned),
        key_fingerprint: key_fingerprint.map(str::to_owned),
        response: body.clone(),
        parked_at: xai_grok_config::signed_policy::now_unix(),
    };
    let json = serde_json::to_string(&staged).map_err(std::io::Error::other)?;
    if json.len() as u64 > MAX_STAGED_REFRESH_BYTES {
        return Err(std::io::Error::other(
            "managed policy exceeds the staging size limit",
        ));
    }
    let path = staged_refresh_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    xai_grok_config::fs_atomic::write_atomically(&path, &json, Some(0o600))
}

pub(super) fn stage_is_current(parked_at: u64, synced_at: Option<u64>) -> bool {
    parked_at != 0 && parked_at >= synced_at.unwrap_or(0)
}

pub(super) fn read_staged_refresh(path: &Path) -> std::io::Result<StagedRefresh> {
    read_bounded_policy_json(path)
}

fn read_bounded_policy_json<Value: serde::de::DeserializeOwned>(
    path: &Path,
) -> std::io::Result<Value> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("staged managed policy is not a file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_STAGED_REFRESH_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STAGED_REFRESH_BYTES {
        return Err(std::io::Error::other(
            "managed policy exceeds the staging size limit",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| std::io::Error::other("invalid staged managed policy"))
}

fn staged_deployment_binding_is_trusted(
    home: &Path,
    staged: &StagedRefresh,
    verified: &super::response::VerifiedEnvelope,
) -> bool {
    use xai_grok_config::signed_policy::{
        SIGNATURE_SIDECAR_FILE, SignatureEnvelope, verify_fetched,
    };
    if staged.source != ManagedConfigSource::DeploymentKey {
        return true;
    }
    let Some(expected) = staged
        .key_fingerprint
        .as_deref()
        .and_then(crate::config::managed_deployment_id)
    else {
        return false;
    };
    let Ok(sidecar) =
        read_bounded_policy_json::<SignatureEnvelope>(&home.join(SIGNATURE_SIDECAR_FILE))
    else {
        return false;
    };
    let Ok(prior_identity) = verify_fetched(&sidecar, None, 0) else {
        return false;
    };
    prior_identity.deployment_id.as_deref() == Some(expected.as_str())
        && verified.payload.deployment_id.as_deref() == Some(expected.as_str())
        && crate::config::normalize_identity(staged.principal.as_deref()).as_deref()
            == Some(expected.as_str())
}

pub(super) fn apply_staged_managed_config() {
    let home = crate::util::grok_home::grok_home();
    let path = staged_refresh_path(&home);
    let Some(_lock) = super::try_lock_managed_config(&home) else {
        return;
    };
    if !super::is_fetch_enabled() || !xai_grok_config::signed_policy::verification_active() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let staged = match read_staged_refresh(&path) {
        Ok(staged) => staged,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    let fingerprint =
        super::resolve_deployment_key().map(|key| super::deployment_key_fingerprint(&key));
    if staged.key_fingerprint != fingerprint
        || !credential_matches(
            staged.source,
            staged.principal.as_deref(),
            staged.key_fingerprint.as_deref(),
        )
        || !stage_is_current(
            staged.parked_at,
            xai_grok_config::managed_config_synced_at(&home),
        )
    {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let verified = match super::verify_response(&staged.response) {
        Ok(verified) => verified,
        Err(_) => {
            let _ = std::fs::remove_file(&path);
            return;
        }
    };
    if !verified
        .as_ref()
        .is_some_and(|verified| staged_deployment_binding_is_trusted(&home, &staged, verified))
    {
        let _ = std::fs::remove_file(&path);
        return;
    }
    match super::apply_fetched_locked(
        &home,
        &staged.response,
        ApplyIdentity {
            source: staged.source,
            principal: staged.principal.as_deref(),
            key_fingerprint: staged.key_fingerprint.as_deref(),
            expected_team: staged.principal.as_deref(),
        },
        Some(staged.parked_at),
        verified,
    ) {
        Ok(ApplyOutcome::Skipped | ApplyOutcome::Staged) => {}
        Ok(_) => {
            let _ = std::fs::remove_file(&path);
        }
        Err(error) => {
            tracing::warn!("staged managed config refresh failed to apply: {error}");
            let _ = std::fs::remove_file(&path);
        }
    }
}
