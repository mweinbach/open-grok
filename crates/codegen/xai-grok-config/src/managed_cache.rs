//! The managed-config cloud-cache subsystem: the sync marker, serving identity,
//! staleness (timer + hard), and the fail-closed enforcement gate that combines
//! the signed-cache verdict with the best-effort marker.
//!
//! The marker is **unsigned** and user-writable — a refresh hint, not a tamper
//! control; real tamper resistance is [`crate::signed_policy`] plus the
//! OS-protected layers (root-owned `/etc/grok`, MDM).

use std::path::Path;

use crate::paths::user_grok_home;

/// Sync marker; staleness keys on this, not mtimes.
const MANAGED_CONFIG_CACHE_FILE: &str = "managed_config_cache.json";

/// The on-disk marker: unsigned, detects only deletion / identity change, not
/// in-place edits (see the module doc).
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct ManagedConfigCache {
    /// Unix seconds of the last successful fetch.
    synced_at: Option<u64>,
    /// Team id, or the deploy-key path's server `deployment_id` (reported via
    /// [`managed_deployment_id`]; identity is `key_fingerprint`).
    principal: Option<String>,
    /// Artifacts this sync served, so staleness spots a later deletion; `default` false so pre-upgrade markers don't over-claim.
    #[serde(default)]
    had_managed_config: bool,
    #[serde(default)]
    had_requirements: bool,
    /// Deploy-key fingerprint (never the raw key) — the deploy-key identity (see [`ServingIdentity`]); `None` on the team path.
    #[serde(default)]
    key_fingerprint: Option<String>,
    /// Served opt-in (`fail_closed = true`); `default` false so a pre-upgrade or un-opted marker never fails closed.
    #[serde(default)]
    fail_closed: bool,
}

/// What the cache is bound to (one value, so a (team, key) combo can't form). The
/// deploy-key fingerprint is the only identity verifiable offline (no key→deployment_id map without the network).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServingIdentity {
    Team(String),
    DeploymentKey { fingerprint: String },
    None,
}

/// Whether to refetch for `identity`: no marker, past the timer, different identity, or a served artifact now missing.
/// Best-effort — callers continue without managed config on failure.
pub fn is_managed_config_stale_for(identity: &ServingIdentity) -> bool {
    managed_config_stale_at(user_grok_home().as_deref(), identity)
}

/// Fields a successful sync records. A struct (destructured without `..`) so a new field is
/// a compile error at every writer — three adjacent positional bools would silently transpose.
pub struct SyncMarker<'a> {
    pub principal: Option<&'a str>,
    pub had_managed_config: bool,
    pub had_requirements: bool,
    pub key_fingerprint: Option<&'a str>,
    pub fail_closed: bool,
}

/// Record a successful sync (best-effort; called even for a config-less principal so it doesn't refetch every tick).
pub fn mark_managed_config_synced(marker: SyncMarker<'_>) {
    if let Some(home) = user_grok_home() {
        mark_managed_config_synced_at(&home, marker);
    }
}

/// Server-side GrokBuildDeployment UUID from the last deploy-key managed-config
/// sync, bound to the key that synced it: returns the marker's `principal` only
/// when the marker's `key_fingerprint` equals `key_fingerprint`, so a rotated or
/// removed key never reports the previous deployment's id. Team-path syncs store
/// a team id and no fingerprint, so they never match.
pub fn managed_deployment_id(key_fingerprint: &str) -> Option<String> {
    managed_deployment_id_at(user_grok_home()?.as_path(), key_fingerprint)
}

fn managed_deployment_id_at(home: &Path, key_fingerprint: &str) -> Option<String> {
    if key_fingerprint.trim().is_empty() {
        return None;
    }
    let cache = read_managed_config_cache(home)?;
    if cache.key_fingerprint.as_deref() != Some(key_fingerprint) {
        return None;
    }
    cache.principal.filter(|p| !p.trim().is_empty())
}

fn mark_managed_config_synced_at(home: &Path, marker: SyncMarker<'_>) {
    let SyncMarker {
        principal,
        had_managed_config,
        had_requirements,
        key_fingerprint,
        fail_closed,
    } = marker;
    let synced_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .ok();
    let cache = ManagedConfigCache {
        synced_at,
        principal: principal.map(str::to_owned),
        // What THIS sync served, not on-disk presence — a confirmed switch already evicted any prior files.
        had_managed_config,
        had_requirements,
        key_fingerprint: key_fingerprint.map(str::to_owned),
        fail_closed,
    };
    match serde_json::to_string(&cache) {
        Ok(json) => write_marker_atomically(home, &json),
        Err(e) => tracing::warn!("failed to serialize managed config cache: {e}"),
    }
}

/// Atomic write of the marker; best-effort (failure is logged, never surfaced).
fn write_marker_atomically(home: &Path, json: &str) {
    if let Err(e) =
        crate::fs_atomic::write_atomically(&home.join(MANAGED_CONFIG_CACHE_FILE), json, None)
    {
        tracing::warn!("failed to write managed config cache: {e}");
    }
}

/// The sync marker, or `None` if absent / unreadable / corrupt. Allow-on-unreadable:
/// a read blip or torn write mustn't lock out a managed user. Unreadable/corrupt are
/// logged (a corruption-to-disarm isn't silent) and self-heal on the next sync.
fn read_managed_config_cache(home: &Path) -> Option<ManagedConfigCache> {
    let json = match std::fs::read_to_string(home.join(MANAGED_CONFIG_CACHE_FILE)) {
        Ok(json) => json,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!("managed config cache unreadable; treating as no marker: {e}");
            return None;
        }
    };
    match serde_json::from_str(&json) {
        Ok(cache) => Some(cache),
        Err(e) => {
            tracing::warn!(
                "managed config cache is corrupt; treating as no marker, next sync rewrites it: {e}"
            );
            None
        }
    }
}

/// A confirmed identity switch vs the marker — both sides of a dimension present and differing (team id or fingerprint).
/// Callers evict prior artifacts on true; a missing marker / `None` / pre-upgrade never counts (first sync / signed-out / legacy never evict).
/// A blank/whitespace value on either side of either dimension (principal or key fingerprint)
/// is "unknown", not a distinct tenant — a malformed `auth.json` parse blip must not confirm a
/// switch and shed a real tenant's policy.
pub fn managed_config_identity_changed(
    new_principal: Option<&str>,
    new_key_fingerprint: Option<&str>,
) -> bool {
    user_grok_home().is_some_and(|home| {
        managed_config_identity_changed_at(&home, new_principal, new_key_fingerprint)
    })
}

fn managed_config_identity_changed_at(
    home: &Path,
    new_principal: Option<&str>,
    new_key_fingerprint: Option<&str>,
) -> bool {
    let Some(cache) = read_managed_config_cache(home) else {
        return false;
    };
    let principal_changed = matches!(
        (cache.principal.as_deref(), new_principal),
        (Some(old), Some(new))
            if !old.trim().is_empty() && !new.trim().is_empty() && old != new
    );
    let key_changed = matches!(
        (cache.key_fingerprint.as_deref(), new_key_fingerprint),
        (Some(old), Some(new))
            if !old.trim().is_empty() && !new.trim().is_empty() && old != new
    );
    principal_changed || key_changed
}

/// True when an artifact the marker recorded serving is now absent. Only served artifacts count, so a config-less
/// principal (or legacy marker) isn't misread as stale. Detects deletion, not edits.
fn cache_missing_required_artifact(cache: &ManagedConfigCache, home: &Path) -> bool {
    (cache.had_requirements && !home.join("requirements.toml").exists())
        || (cache.had_managed_config && !home.join("managed_config.toml").exists())
}

/// Whether the cached principal differs from the team serving now — the team dimension only.
/// Deploy-key identity is verified by fingerprint ([`cache_key_fingerprint_mismatch`]); `None` never fires.
fn cache_identity_mismatch(cache: &ManagedConfigCache, identity: &ServingIdentity) -> bool {
    match identity {
        ServingIdentity::Team(team_id) => cache.principal.as_deref() != Some(team_id.as_str()),
        ServingIdentity::DeploymentKey { .. } | ServingIdentity::None => false,
    }
}

/// Whether the configured deployment key differs from the cache's, by one-way fingerprint (never the raw key) —
/// the only identity verifiable offline. A pre-upgrade marker (no fingerprint) never fires; only a *changed* key.
fn cache_key_fingerprint_mismatch(cache: &ManagedConfigCache, identity: &ServingIdentity) -> bool {
    match identity {
        ServingIdentity::DeploymentKey { fingerprint } => {
            matches!(cache.key_fingerprint.as_deref(), Some(recorded) if recorded != fingerprint)
        }
        ServingIdentity::Team(_) | ServingIdentity::None => false,
    }
}

/// The team id for the signed-cache check; `None` for a deployment key (bound by the
/// marker's deployment id, not a team) or no identity.
fn serving_team_id(identity: &ServingIdentity) -> Option<&str> {
    match identity {
        ServingIdentity::Team(team_id) => Some(team_id.as_str()),
        ServingIdentity::DeploymentKey { .. } | ServingIdentity::None => None,
    }
}

/// Tamper signals for the current identity, split two ways: [`Self::needs_refetch`] (staleness) on ANY
/// signal; [`Self::compromised_for_gate`] (gate) only on artifact-missing or key-change — never a pure
/// identity mismatch (a foreign marker the online refetch rebinds).
#[derive(Clone, Copy)]
struct TamperSignals {
    artifact_missing: bool,
    identity_mismatch: bool,
    key_fingerprint_mismatch: bool,
}

impl TamperSignals {
    fn evaluate(cache: &ManagedConfigCache, home: &Path, identity: &ServingIdentity) -> Self {
        Self {
            artifact_missing: cache_missing_required_artifact(cache, home),
            identity_mismatch: cache_identity_mismatch(cache, identity),
            key_fingerprint_mismatch: cache_key_fingerprint_mismatch(cache, identity),
        }
    }

    fn needs_refetch(self) -> bool {
        self.artifact_missing || self.identity_mismatch || self.key_fingerprint_mismatch
    }

    fn compromised_for_gate(self) -> bool {
        self.artifact_missing || self.key_fingerprint_mismatch
    }
}

/// Cache unusable now: different identity, a served artifact missing, or no marker. The session-start refresh blocks
/// (bounded) on this but not timer-staleness, so a present same-identity cache never delays startup offline.
pub fn is_managed_config_hard_stale_for(identity: &ServingIdentity) -> bool {
    match user_grok_home() {
        Some(home) => is_managed_config_hard_stale_for_at(&home, identity),
        None => false,
    }
}

/// Whether the cache can't be used for `identity` — a served artifact missing or a different
/// identity. Shared by the staleness and session-start paths so the siblings can't drift.
fn cache_unusable_for(cache: &ManagedConfigCache, home: &Path, identity: &ServingIdentity) -> bool {
    TamperSignals::evaluate(cache, home, identity).needs_refetch()
}

/// The principal the SIGNED cache must be bound to: the live team id, else the marker
/// principal (the recorded deployment id on a deployment-key machine). One derivation
/// shared by the gate and both staleness checks, so a foreign-but-authentic cache
/// reads foreign on every sibling path.
fn expected_signed_principal<'a>(
    cache: Option<&'a ManagedConfigCache>,
    identity: &'a ServingIdentity,
) -> Option<&'a str> {
    serving_team_id(identity).or_else(|| cache.and_then(|c| c.principal.as_deref()))
}

/// A signing-enabled build over a legacy unsigned / edited / forged or foreign-bound
/// cache refetches a signed copy. Dark build or no policy on disk → false, so this is
/// inert until a key is provisioned.
fn signed_cache_needs_refetch(
    home: &Path,
    cache: Option<&ManagedConfigCache>,
    identity: &ServingIdentity,
) -> bool {
    crate::signed_policy::cloud_cache_signature_invalid(
        home,
        expected_signed_principal(cache, identity),
        crate::signed_policy::now_unix(),
    )
}

fn is_managed_config_hard_stale_for_at(home: &Path, identity: &ServingIdentity) -> bool {
    let cache = read_managed_config_cache(home);
    cache
        .as_ref()
        .is_none_or(|cache| cache_unusable_for(cache, home, identity))
        || signed_cache_needs_refetch(home, cache.as_ref(), identity)
}

/// No-network fail-closed predicate: true only on a `fail_closed` policy with tamper for
/// the current identity. With a key compiled in the SIGNED verdict leads (non-forgeable
/// opt-in, catches edits the marker can't, and a fail-closed marker then REQUIRES an
/// authentic sidecar); the dark build uses only the best-effort marker decision.
pub fn managed_policy_compromised_for(identity: &ServingIdentity) -> bool {
    user_grok_home().is_some_and(|home| managed_policy_compromised_for_at(&home, identity))
}

/// Apply writes the policy files before the sidecar with no lock shared with gate
/// readers, so a session start racing a background sync can pair new files with the
/// old sidecar and transiently read Compromised. One pause covers the tiny write gap.
const APPLY_RACE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

fn managed_policy_compromised_for_at(home: &Path, identity: &ServingIdentity) -> bool {
    compromised_with_apply_race_retry(
        || managed_policy_compromised_once(home, identity),
        || std::thread::sleep(APPLY_RACE_RETRY_DELAY),
    )
}

/// GATE-only retry for the apply race (see [`APPLY_RACE_RETRY_DELAY`]): on a refusing
/// Compromised verdict, re-evaluate once and use the second verdict — real tamper is
/// still Compromised on the second pass. The staleness/refetch siblings never retry:
/// flagging a transient mismatch there is exactly the self-heal.
fn compromised_with_apply_race_retry(
    mut evaluate: impl FnMut() -> (bool, crate::signed_policy::SignedVerdict),
    pause: impl FnOnce(),
) -> bool {
    match evaluate() {
        (false, _) => false,
        (true, crate::signed_policy::SignedVerdict::Compromised) => {
            pause();
            evaluate().0
        }
        (true, _) => true,
    }
}

/// One full evaluation of the gate decision, returning the signed verdict alongside so
/// the retry wrapper can distinguish a (possibly racing) Compromised refusal.
fn managed_policy_compromised_once(
    home: &Path,
    identity: &ServingIdentity,
) -> (bool, crate::signed_policy::SignedVerdict) {
    let cache = read_managed_config_cache(home);
    let signed_verdict = crate::signed_policy::signed_cache_compromised(
        home,
        expected_signed_principal(cache.as_ref(), identity),
        crate::signed_policy::now_unix(),
    );
    // The signature binds a deployment_id, not the local deploy key, so a Trusted verdict
    // can't attest the configured key — pass the fingerprint mismatch through so it gates
    // on every path.
    let key_fingerprint_mismatch = cache
        .as_ref()
        .is_some_and(|c| cache_key_fingerprint_mismatch(c, identity));
    let compromised = managed_policy_compromised_decision(
        signed_verdict,
        key_fingerprint_mismatch,
        cache.as_ref(),
        home,
        identity,
    );
    (compromised, signed_verdict)
}

/// Combine the signed verdict with the best-effort marker fallback — one row per
/// verdict; each row's reasoning lives on its [`SignedVerdict`] variant doc. Split
/// out so the signed↔marker integration is unit-testable without a compiled-in key.
fn managed_policy_compromised_decision(
    signed_verdict: crate::signed_policy::SignedVerdict,
    key_fingerprint_mismatch: bool,
    cache: Option<&ManagedConfigCache>,
    home: &Path,
    identity: &ServingIdentity,
) -> bool {
    use crate::signed_policy::SignedVerdict;
    // A fail-closed marker that recorded served policy requires an authentic sidecar.
    let sidecar_required_but_missing = || {
        let required =
            cache.is_some_and(|c| c.fail_closed && (c.had_managed_config || c.had_requirements));
        if required {
            tracing::warn!(
                "managed policy fail-closed gate: refusing session — signed sidecar missing or unverifiable"
            );
        }
        required
    };
    // The best-effort marker decision: refuse only an opted-in marker with gate-grade tamper.
    let marker_compromised = || {
        cache.is_some_and(|cache| {
            if !cache.fail_closed {
                return false;
            }
            let signals = TamperSignals::evaluate(cache, home, identity);
            let compromised = signals.compromised_for_gate();
            // Booleans only — never the raw key (the fingerprint is already a one-way hash).
            if compromised {
                tracing::warn!(
                    artifact_missing = signals.artifact_missing,
                    identity_mismatch = signals.identity_mismatch,
                    key_fingerprint_mismatch = signals.key_fingerprint_mismatch,
                    "managed policy fail-closed gate: refusing session on tamper evidence"
                );
            } else if signals.identity_mismatch {
                tracing::debug!(
                    identity_mismatch = true,
                    "managed policy fail-closed gate: foreign marker, not refusing (online refetch rebinds)"
                );
            }
            compromised
        })
    };
    match signed_verdict {
        SignedVerdict::Compromised => true,
        // Trusted clears the gate — except the deploy-key fingerprint, which the signature can't attest.
        SignedVerdict::Trusted => key_fingerprint_mismatch && marker_compromised(),
        SignedVerdict::NoAuthenticSidecar => sidecar_required_but_missing() || marker_compromised(),
        SignedVerdict::SidecarUnreadable => marker_compromised(),
        SignedVerdict::Inactive => marker_compromised(),
    }
}

/// Stale when never synced, past the threshold, identity differs, a served artifact is now missing,
/// or (keyed builds) the signed cache no longer verifies. No home → nothing to refresh into → not
/// stale. Reads the marker once.
fn managed_config_stale_at(home: Option<&Path>, identity: &ServingIdentity) -> bool {
    let Some(home) = home else {
        return false;
    };
    let Some(cache) = read_managed_config_cache(home) else {
        return true; // no marker → never synced → stale
    };
    if cache_unusable_for(&cache, home, identity) {
        return true;
    }
    // Same signed check as the session-start hard-stale sibling: the background tick
    // must also refetch a tampered/foreign-signed cache, not leave it until startup.
    if signed_cache_needs_refetch(home, Some(&cache), identity) {
        return true;
    }
    match cache.synced_at {
        // `duration_since` errs when `synced_at` is in the future (clock skew);
        // treat that as freshly synced rather than stale.
        Some(secs) => {
            let synced_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
            std::time::SystemTime::now()
                .duration_since(synced_at)
                .is_ok_and(|age| age > managed_config_stale_threshold())
        }
        None => true,
    }
}

/// Override with `GROK_DEPLOYMENT_CONFIG_CACHE_TTL_SECS` for testing.
fn managed_config_stale_threshold() -> std::time::Duration {
    if let Ok(s) = std::env::var("GROK_DEPLOYMENT_CONFIG_CACHE_TTL_SECS")
        && let Ok(secs) = s.parse::<u64>()
    {
        return std::time::Duration::from_secs(secs);
    }
    std::time::Duration::from_secs(30 * 60)
}

// Tests in a sibling file (they dwarf the module) but a child module, for private access.
#[cfg(test)]
#[path = "managed_cache/tests.rs"]
mod tests;
