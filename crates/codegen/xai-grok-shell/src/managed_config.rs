//! Sync `managed_config.toml` + `requirements.toml` from the deployment-config endpoint per principal.
//! Overwritten per fetch, evicted on a confirmed identity switch, cleared on logout — so config never leaks across principals.

mod response;

use crate::auth::GrokAuth;
pub use response::ManagedConfigError;
use response::{ApplyOutcome, ManagedConfigResponse, ManagedConfigSource, verify_signed_envelope};

/// Delete the server-synced files (incl. the sync-marker cache); never the
/// user's `config.toml`.
fn remove_managed_config_files(home: &std::path::Path) {
    // Marker LAST: every crash prefix keeps the identity-change detector armed, so the next
    // start re-runs the purge and converges offline instead of refusing on a foreign sidecar.
    for name in [
        "managed_config.toml",
        "requirements.toml",
        xai_grok_config::signed_policy::SIGNATURE_SIDECAR_FILE,
        "managed_config_cache.json",
    ] {
        remove_synced_file(home, name, "removed managed config file");
    }
    // A hard kill mid-write can leave a `.tmp` marker/sidecar behind; best-effort sweep so they
    // don't accumulate (a concurrent writer's in-flight temp may also go — its rename fails and
    // self-heals next check).
    if let Ok(entries) = std::fs::read_dir(home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let is_write_tmp = name.ends_with(".tmp")
                && (name.starts_with("managed_config_cache.json.")
                    || name.starts_with("managed_config.sig.json."));
            if is_write_tmp {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

fn remove_synced_file(home: &std::path::Path, name: &str, why: &str) {
    let path = home.join(name);
    match remove_managed_path(&path) {
        Ok(true) => tracing::info!("{why}"),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(error = %e, "failed to remove managed config file")
        }
    }
}

/// Clear a directory squatting where a managed file is about to be WRITTEN — the atomic
/// rename would fail onto it forever, permanently blocking the self-heal. Best-effort:
/// the write's own error surfaces if clearing fails.
fn clear_squatting_dir(path: &std::path::Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
        && let Err(e) = remove_managed_path(path)
    {
        tracing::warn!(error = %e, "failed to clear a directory squatting at a managed config path");
    }
}

/// Remove whatever occupies a managed artifact path — a squatting DIRECTORY too, else a
/// dir-squat would block removal and rewrite forever. Only ever called with the fixed
/// managed artifact/marker/sidecar names. `Ok(true)` = removed; `Ok(false)` = already absent.
fn remove_managed_path(path: &std::path::Path) -> std::io::Result<bool> {
    let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let result = if is_dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// A team principal is eligible to fetch only if non-expired (an expired token
/// would just 401).
fn eligible_team_principal(auth: GrokAuth) -> Option<GrokAuth> {
    (auth.is_team_principal() && !crate::auth::is_expired(&auth)).then_some(auth)
}

/// The eligible team principal in `auth.json`, or `None`. Single-team: managed
/// config is a grok.com feature with one grok.com auth.
fn read_active_team_auth() -> Option<GrokAuth> {
    let home = crate::util::grok_home::grok_home();
    let store = crate::auth::read_auth_json(&home.join("auth.json")).ok()?;
    let team = store.values().find(|a| a.is_team_principal())?.clone();
    eligible_team_principal(team)
}

pub(crate) fn has_active_team_auth() -> bool {
    read_active_team_auth().is_some()
}

/// Whether any team principal is signed in, **ignoring expiry** (a cold-start
/// expired token is not a logout). `Err` = `auth.json` unreadable: callers must
/// NOT treat that as a logout — it would wipe enforced policy on a read blip.
fn team_principal_signed_in() -> std::io::Result<bool> {
    let home = crate::util::grok_home::grok_home();
    match crate::auth::read_auth_json(&home.join("auth.json")) {
        Ok(store) => Ok(store.values().any(|a| a.is_team_principal())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Clear the synced files when no principal could own them: no deployment key
/// configured and no team signed in (logout). A configured deployment key keeps
/// its files (original "never auto-deletes" behavior). Runs at startup and on
/// logout; best-effort.
pub fn clear_orphan() {
    if resolve_deployment_key().is_some() {
        return;
    }
    match team_principal_signed_in() {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(error = %e, "auth.json unreadable; keeping managed config until it recovers");
            return;
        }
    }
    let home = crate::util::grok_home::grok_home();
    let Some(_lock) = try_lock_managed_config(&home) else {
        return; // another process is syncing; retry next call
    };
    remove_managed_config_files(&home);
}

/// Best-effort cross-process lock serializing apply/remove of the managed-config
/// files (TUI tick vs `grok login` vs prefetch). `None` on contention — the
/// caller skips and retries next cycle.
fn try_lock_managed_config(home: &std::path::Path) -> Option<std::fs::File> {
    use fs2::FileExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(home.join("managed_config.lock"))
        .ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

/// Retry budget for a sync, pairing the attempt count with a wall-clock cap.
#[derive(Clone, Copy)]
enum SyncBudget {
    /// Background loop and explicit `grok setup`; runs retries to completion.
    Standard,
    /// Post-login sync; capped because login latency is user-visible.
    Login,
    /// Session-start refresh; capped so startup never stalls.
    SessionStart,
}

impl SyncBudget {
    /// Total fetch attempts (first try included) for transient failures.
    fn max_attempts(self) -> u32 {
        match self {
            Self::Standard => 5,
            Self::Login | Self::SessionStart => 2,
        }
    }

    /// Wall-clock cap, or `None` to let retries run to completion.
    fn deadline(self) -> Option<std::time::Duration> {
        match self {
            Self::Standard => None,
            Self::Login => Some(std::time::Duration::from_secs(15)),
            Self::SessionStart => Some(std::time::Duration::from_secs(8)),
        }
    }
}

/// Budget for the pre-heal `auth()` refresh, so a degraded network can't stall startup;
/// on timeout the heal proceeds with no refreshed override.
const SESSION_START_AUTH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(8);

/// Exponential backoff for retry `attempt` (caller guarantees `attempt >= 1`).
/// Base is 1s; `GROK_DEPLOYMENT_CONFIG_BACKOFF_MS` overrides it for tests.
fn retry_backoff(attempt: u32) -> std::time::Duration {
    let base = std::env::var("GROK_DEPLOYMENT_CONFIG_BACKOFF_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(1000);
    std::time::Duration::from_millis(base << attempt.saturating_sub(1))
}

/// Fetch the managed-config response, retrying transient (network / connection
/// interruption / 5xx) failures with exponential backoff. Auth errors fail
/// immediately, mapped via `source` so the message names the rejected credential.
///
/// Routes the whole once-fetch (send + body read + decode) through `crate::http::send_with_retry_escaping_pool`,
/// so a body-phase interruption is retried (not just the send) and the final attempt escapes a
/// poisoned pool on a fresh connection (see that helper for the escape policy).
async fn fetch_managed_config(
    url: &str,
    token: &str,
    source: ManagedConfigSource,
    max_attempts: u32,
) -> Result<ManagedConfigResponse, ManagedConfigError> {
    crate::http::send_with_retry_escaping_pool(
        move |client: reqwest::Client| async move {
            fetch_managed_config_once(&client, url, token, source).await
        },
        max_attempts,
        |e: &ManagedConfigError| e.is_retryable(),
        |attempt| tokio::time::sleep(retry_backoff(attempt)),
    )
    .await
}

/// Persist a fetched response under `home`, converging disk to the served set: served
/// artifacts are overwritten, unserved ones removed — a leftover must not keep enforcing
/// a withdrawn policy or trip the signed absence check. Returns whether anything changed.
fn apply_managed_config(
    home: &std::path::Path,
    body: &ManagedConfigResponse,
) -> std::io::Result<bool> {
    use crate::util::config::atomic_write_string;

    let artifacts = [
        ("managed_config.toml", body.managed_config.as_deref()),
        ("requirements.toml", body.requirements.as_deref()),
    ];

    let mut changed = false;
    let mut first_err: Option<std::io::Error> = None;
    for (name, content) in artifacts {
        let path = home.join(name);
        match content.filter(|s| !s.is_empty()) {
            Some(content) => {
                clear_squatting_dir(&path);
                match atomic_write_string(&path, content) {
                    Ok(()) => changed = true,
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
            None => match remove_managed_path(&path) {
                Ok(true) => {
                    tracing::info!("removed managed config artifact the server no longer serves");
                    changed = true;
                }
                Ok(false) => {}
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            },
        }
    }

    if changed {
        tracing::info!("managed config refreshed from server");
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(changed),
    }
}

/// Map a classified transport failure to a `ManagedConfigError`. Split out from [`map_send_error`]
/// so the mapping (and its retryability) is unit-testable by constructing `TransportFailure` directly.
fn map_transport_failure(failure: crate::http::TransportFailure) -> ManagedConfigError {
    use crate::http::TransportFailureKind;
    match failure.kind {
        TransportFailureKind::Unreachable => ManagedConfigError::Network(failure.detail),
        TransportFailureKind::Interrupted => {
            ManagedConfigError::ConnectionInterrupted(failure.detail)
        }
        // A builder/redirect failure is a client-side defect, not a bad server response: terminal.
        TransportFailureKind::Permanent => ManagedConfigError::RequestFailed(failure.detail),
    }
}

/// Map a `reqwest` send failure to a `ManagedConfigError` via the shared `xai-grok-http` classifier.
fn map_send_error(e: &reqwest::Error) -> ManagedConfigError {
    map_transport_failure(crate::http::TransportFailure::classify(e))
}

async fn fetch_managed_config_once(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    source: ManagedConfigSource,
) -> Result<ManagedConfigResponse, ManagedConfigError> {
    let resp = match client
        .get(url)
        .header("Authorization", format!("Bearer {}", token))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = r.status().as_u16();
            tracing::debug!(status, "managed config fetch failed");
            return Err(if status == 401 || status == 403 {
                source.auth_rejected_error()
            } else {
                ManagedConfigError::ServerError { status }
            });
        }
        Err(e) => {
            let err = map_send_error(&e);
            tracing::debug!(error = %err, "managed config fetch error");
            return Err(err);
        }
    };

    // Split the body read from the decode so the FAILING OPERATION disambiguates transport from
    // payload: reqwest tags both a mid-body connection drop and malformed JSON as `Kind::Decode`
    // from `json()`, so reading raw `bytes()` first (any error there is an in-flight transport
    // interruption, retryable) then `from_slice` (any error there is a malformed payload, terminal)
    // avoids fragile error-kind/source inspection.
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        // A body-read failure is an in-flight transport interruption, so it is retryable.
        Err(e) => {
            return Err(ManagedConfigError::ConnectionInterrupted(
                crate::http::error_cause_chain(&e),
            ));
        }
    };
    serde_json::from_slice::<ManagedConfigResponse>(&bytes)
        .map_err(|e| ManagedConfigError::InvalidResponse(e.to_string()))
}

/// Override with `GROK_DEPLOYMENT_CONFIG_REFRESH_INTERVAL_SECS`. Clamped to
/// >= 1s: `tokio::time::interval` panics on a zero period.
fn managed_config_sync_interval() -> std::time::Duration {
    if let Ok(s) = std::env::var("GROK_DEPLOYMENT_CONFIG_REFRESH_INTERVAL_SECS")
        && let Ok(secs) = s.parse::<u64>()
    {
        return std::time::Duration::from_secs(secs.max(1));
    }
    std::time::Duration::from_secs(5 * 60)
}

/// Periodically sync managed config in the background. Best-effort.
pub(crate) fn spawn_sync(cancel: tokio_util::sync::CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(managed_config_sync_interval());
        interval.tick().await; // skip immediate first tick

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }

            // Clear a logged-out team's files before deciding to fetch, so
            // stale enforced policy never outlives the tick.
            clear_orphan();

            if !crate::config::is_managed_config_stale_for(&current_serving_identity())
                || !is_fetch_enabled()
            {
                continue;
            }

            match sync().await {
                Ok(true) => tracing::info!("background managed config sync: updated"),
                Ok(false) => {}
                Err(e) => tracing::debug!("background managed config sync failed: {e}"),
            }
        }

        tracing::debug!("managed config sync task stopped");
    });
}

/// Deployment id reported for `deployment_key` on chat requests, credential
/// snapshots, and OTel: the **server** GrokBuildDeployment UUID (the id
/// server-side dashboards filter on) when the managed-config sync marker was
/// written by this same key (fingerprint match), else UUIDv5 of the key.
/// `None` key (team/OAuth) → `None`, never a stale marker value.
pub fn resolve_deployment_id(deployment_key: Option<&str>) -> Option<String> {
    let key = deployment_key.filter(|k| !k.is_empty())?;
    crate::config::managed_deployment_id(&deployment_key_fingerprint(key))
        .or_else(|| Some(crate::agent::config::deployment_id_from_key(key)))
}

/// Resolve deployment key from `GROK_DEPLOYMENT_KEY` env var, then config files.
pub fn resolve_deployment_key() -> Option<String> {
    let config_val = crate::config::load_effective_config()
        .map_err(|e| tracing::warn!("failed to load config files for deployment key: {e}"))
        .ok()
        .and_then(|root| {
            root.get("endpoints")?
                .get("deployment_key")?
                .as_str()
                .map(|s| s.to_owned())
        });
    crate::agent::config::resolve_string_flag(
        None,
        "GROK_DEPLOYMENT_KEY",
        config_val.as_deref(),
        None,
    )
    .map(|r| r.value)
}

/// One-way blake3 fingerprint of a deployment key — the deploy-key identity (see [`crate::config::ServingIdentity`]).
/// Deterministic so the same key matches its marker; the raw key is never written to disk.
fn deployment_key_fingerprint(key: &str) -> String {
    blake3::hash(key.as_bytes()).to_hex().to_string()
}

/// Whether managed config fetching is enabled (env > config.toml > default true).
/// Callers doing auto-fetch should check this; explicit user actions (grok setup) skip it.
pub fn is_fetch_enabled() -> bool {
    if let Some(v) = crate::agent::config::env_bool("GROK_MANAGED_CONFIG") {
        return v;
    }
    crate::config::load_effective_config()
        .ok()
        .and_then(|cfg| cfg.get("features")?.get("managed_config")?.as_bool())
        .unwrap_or(true)
}

/// Fetch managed config + requirements and write to `~/.grok/`, trying the
/// deployment key first, then a signed-in team. `Ok(false)` when neither applies.
pub async fn sync() -> Result<bool, ManagedConfigError> {
    Ok(sync_with_budget(SyncBudget::Standard, None).await?.wrote)
}

struct SyncOutcome {
    wrote: bool,
    /// The server returned non-empty config for the consulted principal — true
    /// even when a concurrent writer held the lock and our write was skipped, so
    /// `grok setup` doesn't misreport a lock skip as "no config".
    served: bool,
    /// Which credential was consulted, so callers word team-vs-deployment
    /// messages by what actually served, not just what's configured.
    source: Option<ManagedConfigSource>,
    /// Team id, or the deploy-key path's server `deployment_id` (deploy-key identity is `key_fingerprint`, not this).
    principal: Option<String>,
    /// Artifacts the server served, recorded so staleness can spot a later deletion.
    had_managed_config: bool,
    had_requirements: bool,
    /// Deploy-key fingerprint that served, else `None` — the deploy-key identity (see [`crate::config::ServingIdentity`]).
    key_fingerprint: Option<String>,
    /// The served `fail_closed` opt-in, recorded in the marker for the gate.
    fail_closed: bool,
    /// Verification was active and the envelope was rejected, so nothing was persisted.
    /// Suppresses the sync marker (it would describe a body never written).
    signature_rejected: bool,
}

impl SyncOutcome {
    /// `principal` / `key_fingerprint` are the two dimensions that differ between the
    /// deploy-key and team paths; everything else derives from the body and the outcome.
    fn from_fetch(
        body: &ManagedConfigResponse,
        source: ManagedConfigSource,
        principal: Option<String>,
        key_fingerprint: Option<String>,
        outcome: &ApplyOutcome,
    ) -> Self {
        Self {
            wrote: outcome.wrote(),
            served: body.config_exists(),
            source: Some(source),
            principal,
            had_managed_config: body.has_managed_config(),
            had_requirements: body.has_requirements(),
            key_fingerprint,
            fail_closed: body.requirements_fail_closed(),
            signature_rejected: outcome.signature_rejected(),
        }
    }
}

/// Runs a sync under `budget`'s deadline, returning `None` when the deadline
/// elapses first.
async fn sync_bounded(
    budget: SyncBudget,
    team_override: Option<GrokAuth>,
) -> Option<Result<SyncOutcome, ManagedConfigError>> {
    let sync = sync_with_budget(budget, team_override);
    match budget.deadline() {
        Some(deadline) => tokio::time::timeout(deadline, sync).await.ok(),
        None => Some(sync.await),
    }
}

/// `team_override` pins a specific team principal (the just-authenticated one,
/// post-login) instead of re-deriving the team from `auth.json`; `None` uses
/// [`read_active_team_auth`] (the current eligible team).
async fn sync_with_budget(
    budget: SyncBudget,
    team_override: Option<GrokAuth>,
) -> Result<SyncOutcome, ManagedConfigError> {
    let outcome = sync_inner(budget, team_override).await?;
    // Mark only when a principal was consulted AND the fetch wasn't signature-rejected —
    // a rejected fetch persisted nothing, so marking would claim an unwritten body. Lock
    // contention still marks (the holder persists the same config).
    if outcome.source.is_some() && !outcome.signature_rejected {
        crate::config::mark_managed_config_synced(crate::config::SyncMarker {
            principal: outcome.principal.as_deref(),
            had_managed_config: outcome.had_managed_config,
            had_requirements: outcome.had_requirements,
            key_fingerprint: outcome.key_fingerprint.as_deref(),
            fail_closed: outcome.fail_closed,
        });
    }
    Ok(outcome)
}

/// A server response paired with the credential that fetched it.
enum FetchedConfig {
    DeploymentKey {
        key: String,
        body: ManagedConfigResponse,
    },
    Team {
        auth: Box<GrokAuth>,
        body: ManagedConfigResponse,
    },
    /// No deployment key configured and no eligible team signed in.
    NoPrincipal,
}

/// Fetches the configuration for the current principal without touching disk:
/// the deployment key first, then a signed-in team. The installing sync and the
/// read-only `grok setup --json` both build on this.
async fn fetch_for_principal(
    budget: SyncBudget,
    team_override: Option<GrokAuth>,
) -> Result<FetchedConfig, ManagedConfigError> {
    let max_attempts = budget.max_attempts();
    // Resolve from the merged config (managed_config_url > cli_chat_proxy_base_url,
    // including the enterprise single-endpoint derivation) so endpoint overrides
    // are honored and the bearer isn't sent to the public default.
    let url =
        crate::agent::config::EndpointsConfig::from_effective_config().resolve_managed_config_url();

    let team_auth = team_override.or_else(read_active_team_auth);

    if let Some(dk) = resolve_deployment_key() {
        let source = ManagedConfigSource::DeploymentKey;
        match fetch_managed_config(&url, &dk, source, max_attempts).await {
            // A rejected dk (stale env/config) must not starve a valid team
            // sign-in: fall through. Network/5xx do NOT — same unreachable
            // server, double the latency for nothing.
            Err(ManagedConfigError::DeploymentKeyRejected) if team_auth.is_some() => {
                tracing::warn!("deployment key rejected; falling back to the team session token");
            }
            Err(e) => return Err(e),
            // Fall through to the team only when the dk has no config row: an apply
            // converges disk to the served set, and the empty dk body must not delete
            // the team's files. Gate on row existence, not content (which can serve empty).
            Ok(body) if !body.config_exists() && team_auth.is_some() => {
                tracing::debug!("deployment key has no config; trying the team principal");
            }
            Ok(body) => return Ok(FetchedConfig::DeploymentKey { key: dk, body }),
        }
    }

    // The proxy resolves the team from the principal and returns its config.
    if let Some(auth) = team_auth {
        let body = fetch_managed_config(
            &url,
            &auth.key,
            ManagedConfigSource::TeamOauth,
            max_attempts,
        )
        .await?;
        return Ok(FetchedConfig::Team {
            auth: Box::new(auth),
            body,
        });
    }

    Ok(FetchedConfig::NoPrincipal)
}

async fn sync_inner(
    budget: SyncBudget,
    team_override: Option<GrokAuth>,
) -> Result<SyncOutcome, ManagedConfigError> {
    match fetch_for_principal(budget, team_override).await? {
        FetchedConfig::DeploymentKey { key, body } => {
            let source = ManagedConfigSource::DeploymentKey;
            let fingerprint = deployment_key_fingerprint(&key);
            let outcome = apply_fetched(
                &body,
                source,
                body.deployment_id.as_deref(),
                Some(&fingerprint),
            )?;
            // Record the served deployment as the marker principal so the load-time
            // gate rejects a cross-tenant signed policy. The VERIFIED payload's id is
            // preferred — the signed-empty response carries it only inside the payload.
            let principal = outcome
                .signed_deployment_id()
                .map(str::to_owned)
                .or_else(|| body.deployment_id.clone());
            Ok(SyncOutcome::from_fetch(
                &body,
                source,
                principal,
                Some(fingerprint),
                &outcome,
            ))
        }
        FetchedConfig::Team { auth, body } => {
            let source = ManagedConfigSource::TeamOauth;
            let outcome = apply_fetched(&body, source, auth.team_id.as_deref(), None)?;
            // Team identity is bound via principal (team id), not a key fingerprint.
            Ok(SyncOutcome::from_fetch(
                &body,
                source,
                auth.team_id.clone(),
                None,
                &outcome,
            ))
        }
        FetchedConfig::NoPrincipal => Ok(SyncOutcome {
            wrote: false,
            served: false,
            source: None,
            principal: None,
            had_managed_config: false,
            had_requirements: false,
            key_fingerprint: None,
            fail_closed: false,
            signature_rejected: false,
        }),
    }
}

/// Apply a fetched response under the cross-process lock (skips if another process holds it — its sync supersedes ours).
/// `new_principal` / `new_key_fingerprint` identify who serves now, so a confirmed switch evicts prior artifacts first.
fn apply_fetched(
    body: &ManagedConfigResponse,
    source: ManagedConfigSource,
    new_principal: Option<&str>,
    new_key_fingerprint: Option<&str>,
) -> std::io::Result<ApplyOutcome> {
    // Verify BEFORE persisting anything: on failure persist NOTHING (no evict, no write,
    // no marker), so the prior trusted policy survives a bad fetch. Verification is pure,
    // so it also runs before the lock — a lock-skip must not report Applied for an
    // envelope that would have failed.
    let verified = if xai_grok_config::signed_policy::verification_active() {
        match verify_signed_envelope(body, active_team_id_any_expiry().as_deref()) {
            Ok(verified) => Some(verified),
            Err(e) => {
                tracing::warn!("managed config signature rejected; not persisting: {e}");
                return Ok(ApplyOutcome::SignatureRejected);
            }
        }
    } else {
        None
    };
    let signed_deployment_id = verified
        .as_ref()
        .and_then(|v| v.payload.deployment_id.clone());
    let home = crate::util::grok_home::grok_home();
    let Some(_lock) = try_lock_managed_config(&home) else {
        tracing::debug!("managed config locked by another process; skipping apply");
        return Ok(ApplyOutcome::Applied {
            wrote: false,
            signed_deployment_id,
        });
    };
    // Re-check under the lock that the credential still exists. A logout during
    // the fetch runs `clear_orphan`; without this, the in-flight write would
    // restore that principal's policy right after it was cleared.
    if !credential_present(source) {
        tracing::info!("credential gone since fetch started; skipping apply");
        return Ok(ApplyOutcome::Applied {
            wrote: false,
            signed_deployment_id,
        });
    }
    // On a confirmed switch, evict prior files before writing the new ones — else an artifact the old
    // principal served but the new one omits keeps enforcing. Never fires on first sync / signed-out / pre-upgrade.
    if crate::config::managed_config_identity_changed(new_principal, new_key_fingerprint) {
        evict_prior_managed_config(&home);
    }
    let wrote = apply_managed_config(&home, body)?;
    // The sidecar is written AFTER the policy files, so a present sidecar always covers
    // the final on-disk set; converge over a squatting directory first (it would fail
    // the rename forever, leaving the online self-heal unable to recover).
    if let Some(verified) = verified {
        clear_squatting_dir(&home.join(xai_grok_config::signed_policy::SIGNATURE_SIDECAR_FILE));
        xai_grok_config::signed_policy::write_sidecar(&home, &verified.sidecar)?;
    }
    Ok(ApplyOutcome::Applied {
        wrote,
        signed_deployment_id,
    })
}

/// Remove the prior principal's policy artifacts on a confirmed identity switch. Leaves the marker
/// (the next `mark_managed_config_synced` overwrites it) and never touches the user's `config.toml`.
fn evict_prior_managed_config(home: &std::path::Path) {
    remove_synced_file(
        home,
        "managed_config.toml",
        "evicted prior principal's managed config",
    );
    remove_synced_file(
        home,
        "requirements.toml",
        "evicted prior principal's requirements",
    );
}

/// Whether the credential a fetch used is still present. Mirrors the
/// expiry-agnostic, fail-safe checks `clear_orphan` uses (an unreadable
/// `auth.json` keeps, not drops).
fn credential_present(source: ManagedConfigSource) -> bool {
    match source {
        ManagedConfigSource::DeploymentKey => resolve_deployment_key().is_some(),
        ManagedConfigSource::TeamOauth => team_principal_signed_in().unwrap_or(true),
    }
}

/// Outcome of [`post_login_sync`], for the CLI to render. The
/// TUI/agent path ignores it (the sync is best-effort and detached there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedConfigSync {
    /// No eligible principal, fetch disabled, or nothing due — no fetch made.
    Skipped,
    /// New config was written. `is_team` lets the caller word the confirmation
    /// (team vs deployment).
    Updated { is_team: bool },
    /// Fetch ran; nothing new to write.
    NoChange,
    /// Fetch failed or timed out (already logged); the background loop retries.
    Failed,
}

/// Post-login hook for `grok login` and the ACP/TUI authenticate flow: clear any
/// orphaned files, then fetch the new principal's config immediately rather than
/// waiting for the background tick. `authenticated` pins the just-logged-in
/// principal (`None` = on-disk team). Latency-bounded by [`SyncBudget::Login`];
/// failures are logged, not propagated (the background loop retries).
pub async fn post_login_sync(authenticated: Option<GrokAuth>) -> ManagedConfigSync {
    clear_orphan();
    if !is_fetch_enabled() {
        return ManagedConfigSync::Skipped;
    }
    // The just-authenticated team, else the on-disk one — reused for the gate
    // and the sync (one auth.json read). With no team, only sync if due anyway.
    let team = authenticated
        .and_then(eligible_team_principal)
        .or_else(read_active_team_auth);
    if team.is_none() && !crate::config::is_managed_config_stale_for(&current_serving_identity()) {
        return ManagedConfigSync::Skipped;
    }
    match sync_bounded(SyncBudget::Login, team).await {
        // Nothing was persisted for a rejected envelope — that's a failure to
        // report, not "no change" (the gate may refuse the next session).
        Some(Ok(SyncOutcome {
            signature_rejected: true,
            ..
        })) => {
            tracing::warn!("post-login managed config sync: server envelope rejected");
            ManagedConfigSync::Failed
        }
        Some(Ok(SyncOutcome {
            wrote: true,
            source,
            ..
        })) => {
            tracing::info!("post-login managed config sync: updated");
            ManagedConfigSync::Updated {
                is_team: source == Some(ManagedConfigSource::TeamOauth),
            }
        }
        Some(Ok(_)) => ManagedConfigSync::NoChange,
        Some(Err(e)) => {
            tracing::debug!("post-login managed config sync failed: {e}");
            ManagedConfigSync::Failed
        }
        None => {
            tracing::debug!("post-login managed config sync timed out");
            ManagedConfigSync::Failed
        }
    }
}

/// Whether a credential exists that `grok setup` could install config for.
pub fn has_principal() -> bool {
    resolve_deployment_key().is_some() || read_active_team_auth().is_some()
}

/// Whether a managed identity owns this machine, IGNORING token expiry (unlike [`has_principal`]) so an
/// expired/backdated `auth.json` can't disarm the gate. Unreadable → present (fail-safe; the gate ANDs this
/// with [`crate::config::managed_policy_compromised_for`], which a personal user never satisfies).
fn managed_principal_present() -> bool {
    resolve_deployment_key().is_some() || team_principal_signed_in().unwrap_or(true)
}

/// The serving identity for an optional team id: a configured deployment key always
/// wins (keyed on its fingerprint), else the team, else none. The two public views
/// differ only in how the team id is resolved (expiry-filtered vs expiry-ignoring).
fn serving_identity_from(team_id: Option<String>) -> crate::config::ServingIdentity {
    use crate::config::ServingIdentity;
    if let Some(key) = resolve_deployment_key() {
        return ServingIdentity::DeploymentKey {
            fingerprint: deployment_key_fingerprint(&key),
        };
    }
    match team_id {
        Some(team_id) => ServingIdentity::Team(team_id),
        None => ServingIdentity::None,
    }
}

/// The identity to check the cache against for whoever serves now: a configured deployment key wins
/// (else the active team, else none).
pub fn current_serving_identity() -> crate::config::ServingIdentity {
    serving_identity_from(read_active_team_auth().and_then(|a| a.team_id))
}

/// The client's team_id, IGNORING token expiry (the binding must survive the cold-start
/// expired window). Must NOT special-case a configured deployment key — that would
/// disable envelope binding for a real team user. Used at fetch time to bind the envelope.
pub fn active_team_id_any_expiry() -> Option<String> {
    let home = crate::util::grok_home::grok_home();
    let store = crate::auth::read_auth_json(&home.join("auth.json")).ok()?;
    store
        .values()
        .find(|a| a.is_team_principal())
        .and_then(|a| a.team_id.clone())
        // A blank team_id (malformed auth.json) is unknown, not a distinct identity: it must not
        // feed the gate's identity checks, the tenant-switch purge, or the envelope binding.
        .filter(|id| !id.trim().is_empty())
}

/// Like [`current_serving_identity`] but IGNORING token expiry, for the enforcement gate:
/// a backdated `auth.json` must not resolve the team to `None` and relax the identity
/// checks. The refetch path stays expiry-filtered (a stray refetch is harmless).
fn current_serving_identity_any_expiry() -> crate::config::ServingIdentity {
    serving_identity_from(active_team_id_any_expiry())
}

/// Best-effort session-start refresh: a bounded token refresh, then a bounded refetch only when the cache is
/// hard-stale. NEVER fails the session — on failure it continues on cached / OS-protected policy.
pub async fn ensure_managed_policy_present(
    auth_manager: &std::sync::Arc<crate::auth::AuthManager>,
) {
    // Gated on fetch-enabled, not `cfg!(test)` — that would diverge test behavior from production.
    if !is_fetch_enabled() {
        return;
    }
    // Cheap disk-only gates before any network token refresh, so the boot path doesn't pay
    // an `auth()` in the common cases. A personal user (no deploy key, and no team in
    // `auth.json` even ignoring expiry) skips entirely; a usable identity whose cache isn't
    // hard-stale also skips. Only an expired-but-refreshable team token (identity reads
    // `None` before the refresh) or a hard-stale cache falls through to `auth()` below.
    // `auth.json` unreadable (`Err`) is NOT treated as "no principal" — that would skip
    // enforcement on a transient read blip.
    if resolve_deployment_key().is_none() && matches!(team_principal_signed_in(), Ok(false)) {
        return;
    }
    let identity = current_serving_identity();
    if !matches!(identity, crate::config::ServingIdentity::None)
        && !crate::config::is_managed_config_hard_stale_for(&identity)
    {
        return;
    }
    // Refresh before the heal so an expired-but-refreshable team token isn't dropped by
    // the expiry filter. Bounded; deploy-key machines have no OAuth (auth() → None).
    let team = tokio::time::timeout(SESSION_START_AUTH_DEADLINE, auth_manager.auth())
        .await
        .ok()
        .and_then(Result::ok)
        .filter(GrokAuth::is_team_principal);
    if !has_principal() {
        return;
    }
    if !crate::config::is_managed_config_hard_stale_for(&current_serving_identity()) {
        return;
    }
    match sync_bounded(SyncBudget::SessionStart, team).await {
        Some(Ok(_)) => {}
        Some(Err(e)) => tracing::warn!("session-start managed policy refresh failed: {e}"),
        None => tracing::warn!("session-start managed policy refresh timed out"),
    }
}

/// Shown when a managed principal's enforced policy is missing/substituted and the refetch couldn't restore it.
const MANAGED_POLICY_MISSING_MSG: &str = "Managed policy is required for this account but is \
missing or could not be verified, and could not be restored from the server.\nThis check needs \
network access: reconnect and start again. If you can't reconnect, contact your administrator.";

/// Best-effort no-network fail-closed gate on every session-start path: a managed principal whose opted-in
/// policy can't be established gets no unmanaged session. With no signing key it reads the user-writable
/// marker (a local user can disarm it by editing one field); non-forgeable enforcement is the trust-rooted
/// layers (root-owned path, MDM, signed cache). No client env disables it; recovery stays open (reconnect /
/// `grok setup`); ceasing to serve `fail_closed` rolls back.
pub fn managed_policy_gate() -> Result<(), String> {
    // Skip under the lib unit-test build only: `bootstrap` reaches this without a staged
    // `GROK_HOME` and would flake on the dev machine's real marker/auth. The pure decision
    // is unit-tested; the integration tests (no `cfg(test)`) exercise this real path.
    if cfg!(test) {
        return Ok(());
    }
    // Purge first: an offline team switch would otherwise read as a substituted cache and refuse.
    purge_prior_tenant_on_identity_change();
    managed_policy_gate_decision(
        managed_principal_present(),
        // Expiry-IGNORING identity: a backdated `auth.json` must not resolve the team to
        // `None` and relax the signed-cache binding (the gate is the enforcement path).
        crate::config::managed_policy_compromised_for(&current_serving_identity_any_expiry()),
    )
}

/// On a confirmed team switch (same [`crate::config::managed_config_identity_changed`] detector
/// as the apply-path eviction), purge the prior team's artifacts so the gate permits the new
/// team instead of refusing over A's now-foreign marker/sidecar; the next online fetch applies
/// B's policy. Signed-out, first-sync, and same-team sessions are no-ops.
/// Deploy-key machines never purge here: the key is local config any process can set, so an
/// offline "switch" is tamper, not identity — genuine rotations evict online via apply.
/// Detector + delete run under the managed-config lock (no rebind to B between them);
/// contention skips — the holder owns the transition, like [`clear_orphan`].
/// Residual: forging A→B→A offline sheds A's policy until its next online fetch — the same
/// self-healing class as deleting the user-writable files outright; /etc/grok and MDM are unaffected.
fn purge_prior_tenant_on_identity_change() {
    let crate::config::ServingIdentity::Team(team_id) = current_serving_identity_any_expiry()
    else {
        return;
    };
    let home = crate::util::grok_home::grok_home();
    let Some(_lock) = try_lock_managed_config(&home) else {
        return; // another process is mid-apply/remove; it owns the transition
    };
    if crate::config::managed_config_identity_changed(Some(&team_id), None) {
        tracing::info!(team_id = %team_id, "identity changed; purging the prior tenant's managed config");
        remove_managed_config_files(&home);
    }
}

/// Pure decision behind [`managed_policy_gate`]: fail closed only when a managed principal is active AND its policy is compromised.
fn managed_policy_gate_decision(
    managed_principal_present: bool,
    policy_compromised: bool,
) -> Result<(), String> {
    if managed_principal_present && policy_compromised {
        return Err(MANAGED_POLICY_MISSING_MSG.to_string());
    }
    Ok(())
}

/// Outcome of the `grok setup` sync. The caller renders it — CLI presentation
/// and exit codes stay out of the library.
#[derive(Debug)]
pub enum SetupOutcome {
    /// Config was written to `~/.grok`.
    Installed,
    /// The principal is valid but the server has no config for it.
    NothingConfigured,
    /// The fetch failed.
    Failed(ManagedConfigError),
}

/// Result of `grok setup --json`: what the server serves for the current
/// principal, verbatim. `managed_config` may embed the enforced deployment key,
/// exactly as `grok setup` would write it to disk.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupReport {
    /// The credential that served: `"deploymentKey"` or `"teamOauth"`, or
    /// `None` when no principal was available.
    pub source: Option<&'static str>,
    /// Whether the server has a configuration for the principal.
    pub configured: bool,
    pub deployment_id: Option<String>,
    pub team_id: Option<String>,
    /// TOML documents exactly as `grok setup` would install them.
    pub managed_config: Option<String>,
    pub requirements: Option<String>,
    pub fail_closed: bool,
}

/// Fetches the report behind `grok setup --json` without writing anything:
/// no artifacts, no signature sidecar, no sync marker.
pub async fn fetch_setup_report() -> Result<SetupReport, ManagedConfigError> {
    let (source, body) = match fetch_for_principal(SyncBudget::Standard, None).await? {
        FetchedConfig::DeploymentKey { body, .. } => (Some("deploymentKey"), body),
        FetchedConfig::Team { body, .. } => (Some("teamOauth"), body),
        FetchedConfig::NoPrincipal => (None, ManagedConfigResponse::default()),
    };
    // Match the installer's trust decision: a payload `grok setup` would refuse
    // is reported as an error, not printed as installable config.
    if source.is_some()
        && xai_grok_config::signed_policy::verification_active()
        && let Err(e) = verify_signed_envelope(&body, active_team_id_any_expiry().as_deref())
    {
        tracing::warn!("managed config signature rejected: {e}");
        return Err(ManagedConfigError::SignatureRejected);
    }
    Ok(SetupReport {
        source,
        configured: body.config_exists(),
        fail_closed: body.requirements_fail_closed(),
        deployment_id: body.deployment_id,
        team_id: body.team_id,
        managed_config: body.managed_config,
        requirements: body.requirements,
    })
}

/// Run the `grok setup` sync for the current principal. The caller must check
/// [`has_principal`] first and render the no-principal guidance.
pub async fn run_setup() -> SetupOutcome {
    match sync_with_budget(SyncBudget::Standard, None).await {
        // A rejected envelope persisted nothing — reporting Installed would mask a
        // fetch the gate is about to refuse.
        Ok(SyncOutcome {
            signature_rejected: true,
            ..
        }) => SetupOutcome::Failed(ManagedConfigError::SignatureRejected),
        // `served` (not `wrote`) so a lock skip by a concurrent writer — which
        // is persisting the same config — isn't reported as "no config".
        Ok(SyncOutcome { served: true, .. }) => SetupOutcome::Installed,
        Ok(_) => SetupOutcome::NothingConfigured,
        Err(e) => SetupOutcome::Failed(e),
    }
}

// Tests in a sibling file (they dwarf the module) but a child module, for private access.
#[cfg(test)]
#[path = "managed_config/tests.rs"]
mod tests;
