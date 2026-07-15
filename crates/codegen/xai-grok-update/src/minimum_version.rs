//! Minimum-version enforcement.
//!
//! When `cli.minimum_version` is set in any config layer, Grok refuses to
//! start below that floor. With auto-update on, we install
//! `max(latest, minimum)`; otherwise the user is asked to run `grok update`.
//!
//! Set `GROK_TEST_VERSION` to manually exercise either path without producing
//! a real out-of-date build.

use crate::auto_update::{get_installer, run_install_script};
use crate::version::{
    UpdateConfig, fetch_latest_version, get_installed_grok_version, write_version_cache,
};
use tracing::{info, warn};
use xai_grok_shell::util::config;

/// Result of comparing the running binary against a configured floor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MinimumVersionDecision {
    Allow,
    BelowMinimum { current: String, minimum: String },
}

/// Outcome of a successful enforcement pass.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EnforcementOutcome {
    Allowed,
    /// New binary on disk; caller MUST restart — running process is still old.
    Upgraded,
}

/// User-facing enforcement failures; `Display` is printed to stderr.
/// `AutoUpdateDisabled` and `NoInstaller` share copy but stay separate so
/// telemetry can distinguish them.
#[derive(Debug, thiserror::Error)]
pub(crate) enum MinimumVersionError {
    /// `source` chains via `Error::source()`; omitted from `Display`.
    #[error(
        "The minimum version \"{value}\" in your Grok configuration \
         isn't a valid version number. Update `cli.minimum_version` and try again."
    )]
    InvalidMinimum {
        value: String,
        #[source]
        source: semver::Error,
    },
    #[error(
        "This version of Grok ({current}) is no longer supported. \
         Run `grok update` to install version {minimum} or later."
    )]
    AutoUpdateDisabled { current: String, minimum: String },
    /// `npm` / `gh` / `internal` GCS — none detected.
    #[error(
        "This version of Grok ({current}) is no longer supported. \
         Run `grok update` to install version {minimum} or later."
    )]
    NoInstaller { current: String, minimum: String },
    /// `detail` is telemetry-only; omitted from `Display` to avoid stacking
    /// the installer's own action language.
    #[error(
        "This version of Grok ({current}) is no longer supported, \
         and the update to version {minimum} didn't complete.\n\n\
         Run `grok update` to try again."
    )]
    UpgradeFailed {
        current: String,
        minimum: String,
        detail: String,
    },
    /// Latest release is known but still below the floor (vs `NoReleaseFound`,
    /// which couldn't probe at all).
    #[error(
        "This version of Grok ({current}) is no longer supported. \
         Version {minimum} or later is required, but the most recent release is {latest}. \
         Contact your administrator."
    )]
    NoSatisfyingVersion {
        current: String,
        minimum: String,
        latest: String,
    },
    /// Couldn't probe the registry — likely transient.
    #[error(
        "This version of Grok ({current}) is no longer supported. \
         Version {minimum} or later is required, but no release was found. \
         Check your network connection, or contact your administrator."
    )]
    NoReleaseFound { current: String, minimum: String },
    /// `grok update --version X` requested a version below the floor.
    #[error(
        "Cannot install Grok {target}: the configured minimum is {minimum}. \
         Run `grok update` to install the latest allowed version."
    )]
    TargetBelowFloor { target: String, minimum: String },
}

/// Pure check against the configured floor. Empty / whitespace-only
/// minimums are treated as unset.
fn evaluate_minimum_version(
    current_version: &str,
    minimum_version: Option<&str>,
) -> Result<MinimumVersionDecision, MinimumVersionError> {
    let Some(minimum) = minimum_version.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(MinimumVersionDecision::Allow);
    };

    let parsed_min =
        semver::Version::parse(minimum).map_err(|source| MinimumVersionError::InvalidMinimum {
            value: minimum.to_string(),
            source,
        })?;

    // Unparseable current (e.g. funky dev build): block rather than let an
    // unverifiable binary through.
    let parsed_cur = match semver::Version::parse(current_version) {
        Ok(v) => v,
        Err(_) => {
            return Ok(MinimumVersionDecision::BelowMinimum {
                current: current_version.to_string(),
                minimum: parsed_min.to_string(),
            });
        }
    };

    if parsed_cur >= parsed_min {
        Ok(MinimumVersionDecision::Allow)
    } else {
        Ok(MinimumVersionDecision::BelowMinimum {
            current: parsed_cur.to_string(),
            minimum: parsed_min.to_string(),
        })
    }
}

/// Refuse an explicit install target below the configured floor.
/// Used by `grok update --version X`.
pub(crate) fn check_install_target(target: &str) -> Result<(), MinimumVersionError> {
    let floor = resolve_floor_or_error()?;
    check_install_target_inner(target, floor.as_deref())
}

fn check_install_target_inner(
    target: &str,
    floor: Option<&str>,
) -> Result<(), MinimumVersionError> {
    let Some(min) = floor else { return Ok(()) };
    match evaluate_minimum_version(target, Some(min))? {
        MinimumVersionDecision::Allow => Ok(()),
        MinimumVersionDecision::BelowMinimum {
            current: target,
            minimum,
        } => Err(MinimumVersionError::TargetBelowFloor { target, minimum }),
    }
}

/// `max(target, configured_floor)`; passthrough when no floor is set.
/// Used by `grok update` to keep the install target at or above the pin.
pub(crate) fn apply_floor(target: &str) -> Result<String, MinimumVersionError> {
    let floor = resolve_floor_or_error()?;
    apply_floor_inner(target, floor.as_deref())
}

/// Adapts `config::resolve_minimum_version`'s error shape into ours.
fn resolve_floor_or_error() -> Result<Option<String>, MinimumVersionError> {
    config::resolve_minimum_version()
        .map_err(|(value, source)| MinimumVersionError::InvalidMinimum { value, source })
}

fn apply_floor_inner(target: &str, floor: Option<&str>) -> Result<String, MinimumVersionError> {
    let Some(min) = floor else {
        return Ok(target.to_string());
    };
    match evaluate_minimum_version(target, Some(min))? {
        MinimumVersionDecision::Allow => Ok(target.to_string()),
        MinimumVersionDecision::BelowMinimum { minimum, .. } => Ok(minimum),
    }
}

/// `max(latest, minimum)`; falls back to `minimum` if `latest` is missing or unparseable.
fn pick_target_version(latest: Option<&str>, minimum: &str) -> String {
    match latest.and_then(|v| semver::Version::parse(v).ok()) {
        Some(latest_v) => match semver::Version::parse(minimum) {
            Ok(min_v) if latest_v >= min_v => latest_v.to_string(),
            _ => minimum.to_string(),
        },
        None => minimum.to_string(),
    }
}

/// Call once at startup, before any user-facing UI. On `Ok(Upgraded)` the
/// caller MUST restart. On `Err`, print and exit non-zero.
async fn enforce_minimum_version(
    minimum_version: Option<&str>,
    update_config: &UpdateConfig,
) -> Result<EnforcementOutcome, MinimumVersionError> {
    let current_version = get_installed_grok_version();
    let decision = evaluate_minimum_version(&current_version, minimum_version)?;
    let MinimumVersionDecision::BelowMinimum { current, minimum } = decision else {
        info!(current = %current_version, "minimum_version: floor satisfied");
        return Ok(EnforcementOutcome::Allowed);
    };

    info!(%current, %minimum, "minimum_version: below floor; attempting auto-update");

    // `None` is "default on"; only explicit `false` opts out.
    let cfg = config::load_config().await;
    if cfg.cli.auto_update == Some(false) {
        warn!(%current, %minimum, "minimum_version: auto-update disabled by config");
        return Err(MinimumVersionError::AutoUpdateDisabled { current, minimum });
    }

    let Some(installer) = get_installer().await else {
        warn!(%current, %minimum, "minimum_version: no installer detected");
        return Err(MinimumVersionError::NoInstaller { current, minimum });
    };

    let latest = fetch_latest_version(installer, update_config).await.ok();
    let target = pick_target_version(latest.as_deref(), &minimum);

    info!(%current, %target, installer, "minimum_version: installing upgrade");
    eprintln!(
        "This version of Grok ({current}) is no longer supported. \
         Updating to {target}…"
    );

    if let Err(e) = run_install_script(installer, Some(&target), update_config).await {
        let detail = format!("{e:#}");
        warn!(%current, %target, %detail, "minimum_version: upgrade failed");
        return Err(MinimumVersionError::UpgradeFailed {
            current,
            minimum,
            detail,
        });
    }

    // Post-install: pass None for stable_version (same rationale as run_update).
    write_version_cache(&target, None).await;

    // Stale channel pointer or partial install can leave us below the floor;
    // surface that rather than starting an out-of-policy binary.
    if let MinimumVersionDecision::BelowMinimum { .. } =
        evaluate_minimum_version(&target, Some(&minimum))?
    {
        warn!(%target, %minimum, ?latest, "minimum_version: post-install still below floor");
        return Err(match latest {
            Some(latest) => MinimumVersionError::NoSatisfyingVersion {
                current: target,
                minimum,
                latest,
            },
            None => MinimumVersionError::NoReleaseFound {
                current: target,
                minimum,
            },
        });
    }

    info!(%target, "minimum_version: upgrade installed successfully");
    Ok(EnforcementOutcome::Upgraded)
}

/// Single chokepoint for the pager + tui startup paths. Re-execs after a
/// floor-driven install. Prints + exits non-zero on `Err`.
///
/// `GROK_TEST_VERSION` lets devs override the running version to skip
/// enforcement on a `cargo run` build.
pub async fn enforce_minimum_version_or_exit(update_config: &UpdateConfig) {
    let min = match resolve_floor_or_error() {
        Ok(None) => return,
        Ok(Some(m)) => m,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    match enforce_minimum_version(Some(&min), update_config).await {
        Ok(EnforcementOutcome::Allowed) => {}
        Ok(EnforcementOutcome::Upgraded) => {
            // TODO: restart_grok uses exec() which carries the same
            // SIGABRT risk as the old piped-stderr update path if the
            // child process ever writes to a broken pipe. For now this
            // path is rare (only fires when the server pushes a minimum
            // version bump), so print a relaunch message instead.
            eprintln!("Update installed. Run `grok` to start.");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_minimum_version_decisions() {
        use MinimumVersionDecision::{Allow, BelowMinimum};

        // Allow: floor unset (None / empty / whitespace) or satisfied (equal / above).
        assert_eq!(evaluate_minimum_version("0.1.100", None).unwrap(), Allow);
        assert_eq!(
            evaluate_minimum_version("0.1.100", Some("")).unwrap(),
            Allow
        );
        assert_eq!(
            evaluate_minimum_version("0.1.100", Some("   ")).unwrap(),
            Allow
        );
        assert_eq!(
            evaluate_minimum_version("0.1.100", Some("0.1.100")).unwrap(),
            Allow
        );
        assert_eq!(
            evaluate_minimum_version("0.2.0", Some("0.1.100")).unwrap(),
            Allow
        );

        // BelowMinimum: current < floor.
        assert!(matches!(
            evaluate_minimum_version("0.1.99", Some("0.1.100")).unwrap(),
            BelowMinimum { .. }
        ));

        // InvalidMinimum: unparseable floor (admin typo).
        assert!(matches!(
            evaluate_minimum_version("0.1.100", Some("not-a-version")),
            Err(MinimumVersionError::InvalidMinimum { .. })
        ));
    }

    #[test]
    fn pick_target_returns_max_of_latest_and_minimum() {
        // The `None` branch is only reachable here — apply_floor always
        // passes `Some(target)`. Production hits it on fetch failure.
        assert_eq!(pick_target_version(Some("0.1.200"), "0.1.150"), "0.1.200");
        assert_eq!(pick_target_version(Some("0.1.140"), "0.1.150"), "0.1.150");
        assert_eq!(pick_target_version(None, "0.1.150"), "0.1.150");
    }

    #[test]
    fn install_target_helpers_consult_floor() {
        // check_install_target rejects below-floor targets.
        assert!(check_install_target_inner("0.1.50", None).is_ok());
        assert!(check_install_target_inner("0.1.150", Some("0.1.100")).is_ok());
        assert!(matches!(
            check_install_target_inner("0.1.50", Some("0.1.100")).unwrap_err(),
            MinimumVersionError::TargetBelowFloor { .. }
        ));

        // apply_floor bumps below-floor targets up.
        assert_eq!(apply_floor_inner("0.1.50", None).unwrap(), "0.1.50");
        assert_eq!(
            apply_floor_inner("0.1.200", Some("0.1.100")).unwrap(),
            "0.1.200"
        );
        assert_eq!(
            apply_floor_inner("0.1.50", Some("0.1.100")).unwrap(),
            "0.1.100"
        );
    }

    #[test]
    #[serial_test::serial]
    fn version_env_var_flows_through_to_decision() {
        let saved = std::env::var("GROK_TEST_VERSION").ok();

        // SAFETY: #[serial] excludes other env-touching tests.
        unsafe { std::env::set_var("GROK_TEST_VERSION", "0.1.50") };
        let decision =
            evaluate_minimum_version(&get_installed_grok_version(), Some("0.1.100")).unwrap();
        assert!(matches!(
            decision,
            MinimumVersionDecision::BelowMinimum { .. }
        ));

        match saved {
            Some(v) => unsafe { std::env::set_var("GROK_TEST_VERSION", v) },
            None => unsafe { std::env::remove_var("GROK_TEST_VERSION") },
        }
    }
}
