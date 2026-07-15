//! Stable agent identifier.
//!
//! Extracted from `xai-grok-shell::agent::unique_identifier` so the
//! telemetry engine can stamp events without depending on shell internals.
//! `$GROK_HOME` is resolved through `xai-grok-config::grok_home`.

use std::sync::OnceLock;

/// Cached agent ID - stored in memory after first load.
static AGENT_ID: OnceLock<String> = OnceLock::new();
/// Cached agent instance ID - per-process lifetime.
static AGENT_INSTANCE_ID: OnceLock<String> = OnceLock::new();

/// Returns the agent ID, using a file-based cache to avoid expensive system calls.
///
/// On macOS, `mid::get()` calls `system_profiler` which takes ~1-3 seconds.
/// This function caches the result in `$GROK_HOME/agent_id` so subsequent calls
/// (even across process restarts) are instant file reads.
///
/// The in-memory `OnceLock` ensures we only read the file once per process.
pub fn agent_id() -> String {
    AGENT_ID.get_or_init(load_or_compute_agent_id).clone()
}

/// Returns a per-process agent instance ID.
/// This is stable across WebSocket reconnects within the same process,
/// but changes on process restart.
pub fn agent_instance_id() -> String {
    AGENT_INSTANCE_ID
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

fn load_or_compute_agent_id() -> String {
    let cache_path = xai_grok_config::grok_home().join("agent_id");

    // Try to read from cache file first (fast path)
    if let Ok(cached) = std::fs::read_to_string(&cache_path) {
        let cached = cached.trim();
        if !cached.is_empty() {
            return cached.to_string();
        }
    }

    // Compute a unique machine hash:
    // - macOS: mid uses unique hardware IDs (serial, UUID, SEID).
    // - Linux: /etc/machine-id is shared across containers from the same base
    //   image, so include $HOSTNAME (container/host name) for uniqueness.
    // - Fallback: random UUIDv4 if mid or hostname are unavailable.
    let machine_hash = if cfg!(target_os = "linux") {
        match std::env::var("HOSTNAME") {
            Ok(hostname) if !hostname.is_empty() => {
                let key = format!("agent_id:{hostname}");
                mid::get(&key).unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
            }
            _ => uuid::Uuid::new_v4().to_string(),
        }
    } else {
        mid::get("agent_id").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
    };
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, machine_hash.as_bytes()).to_string();

    // Save to cache file (best effort, ignore errors)
    let _ = std::fs::write(&cache_path, &id);

    id
}

/// Returns true when workspace marker env vars (`XAI_ROOT` and `XAI_USER`) are set.
///
/// Used as a coarse local gate for features that require a full workspace
/// checkout. External installs typically leave both unset.
pub fn has_workspace_env_markers() -> bool {
    std::env::var("XAI_ROOT").is_ok() && std::env::var("XAI_USER").is_ok()
}

/// Opt-in special-user gate for telemetry.
///
/// Enabled only when `GROK_TELEMETRY_SPECIAL_USER=1` (or `true`). There is no
/// hardcoded username allowlist.
pub fn is_special_user() -> bool {
    matches!(
        std::env::var("GROK_TELEMETRY_SPECIAL_USER").as_deref(),
        Ok("1") | Ok("true") | Ok("TRUE")
    )
}
