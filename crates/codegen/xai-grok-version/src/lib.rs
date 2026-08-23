//! Installed Open Grok build identity.
//!
//! Release identity is registered by the composition-root binary at process
//! startup. Keeping the release stamp out of this widely shared crate prevents
//! a version-only release build from invalidating every reverse dependency.

use semver::Version;
use std::sync::OnceLock;

pub const TEST_VERSION_ENV: &str = "GROK_TEST_VERSION";

const FALLBACK_VERSION: &str = env!("CARGO_PKG_VERSION");
const FALLBACK_VERSION_WITH_COMMIT: &str = concat!(env!("CARGO_PKG_VERSION"), " (unknown)");

/// Whether the composition-root binary carries an explicit release stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    Local,
    Release,
}

/// Immutable identity registered by the final binary before any subsystem runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    version: &'static str,
    version_with_commit: &'static str,
    kind: BuildKind,
}

impl BuildInfo {
    pub const fn local(version_with_commit: &'static str) -> Self {
        Self {
            version: FALLBACK_VERSION,
            version_with_commit,
            kind: BuildKind::Local,
        }
    }

    pub const fn release(version: &'static str, version_with_commit: &'static str) -> Self {
        Self {
            version,
            version_with_commit,
            kind: BuildKind::Release,
        }
    }

    pub const fn from_compile_stamp(
        release_version: Option<&'static str>,
        version_with_commit: &'static str,
    ) -> Self {
        match release_version {
            Some(version) => Self::release(version, version_with_commit),
            None => Self::local(version_with_commit),
        }
    }

    /// Build identity for a standalone binary that has a release version stamp
    /// but no commit-stamping build script.
    pub const fn from_version_stamp(release_version: Option<&'static str>) -> Self {
        match release_version {
            Some(version) => Self::release(version, version),
            None => Self::local(FALLBACK_VERSION_WITH_COMMIT),
        }
    }

    pub const fn version(self) -> &'static str {
        self.version
    }

    pub const fn version_with_commit(self) -> &'static str {
        self.version_with_commit
    }

    pub const fn kind(self) -> BuildKind {
        self.kind
    }
}

static BUILD_INFO: OnceLock<BuildInfo> = OnceLock::new();

/// Register the final binary's build identity.
///
/// Re-registering an identical identity is harmless. A conflicting identity is
/// rejected so embedded/library callers cannot silently change security or
/// updater behavior after startup.
pub fn initialize(info: BuildInfo) -> Result<(), BuildInfo> {
    initialize_cell(&BUILD_INFO, info)
}

fn initialize_cell(cell: &OnceLock<BuildInfo>, info: BuildInfo) -> Result<(), BuildInfo> {
    match cell.set(info) {
        Ok(()) => Ok(()),
        Err(info) if cell.get() == Some(&info) => Ok(()),
        Err(info) => Err(info),
    }
}

/// The identity explicitly registered by the composition root, if any.
pub fn registered_build_info() -> Option<BuildInfo> {
    BUILD_INFO.get().copied()
}

/// The running Open Grok version.
///
/// Standalone library/tests that have no composition root retain the historical
/// package-version fallback. The shipped binary always initializes this before
/// dispatching any work.
pub fn version() -> &'static str {
    BUILD_INFO
        .get()
        .copied()
        .map(BuildInfo::version)
        .unwrap_or(FALLBACK_VERSION)
}

/// The running version plus source commit. An uninitialized standalone library
/// process retains a display-safe `"<package version> (unknown)"` fallback.
pub fn version_with_commit() -> &'static str {
    BUILD_INFO
        .get()
        .copied()
        .map(BuildInfo::version_with_commit)
        .unwrap_or(FALLBACK_VERSION_WITH_COMMIT)
}

/// Whether this is a release-stamped build.
///
/// An uninitialized optimized process is treated as a release. This is a
/// deliberate fail-closed default for folder trust: deleting the binary's
/// initialization cannot make a production-shaped build auto-trust project
/// code. Debug/test library processes keep the historical local-build default.
pub fn is_release_build() -> bool {
    classify_build_kind(
        BUILD_INFO.get().copied().map(BuildInfo::kind),
        cfg!(debug_assertions),
    ) == BuildKind::Release
}

fn classify_build_kind(registered: Option<BuildKind>, debug_assertions: bool) -> BuildKind {
    registered.unwrap_or(if debug_assertions {
        BuildKind::Local
    } else {
        BuildKind::Release
    })
}

/// [`TEST_VERSION_ENV`] override first, then [`version`]. Trimmed so
/// non-semver-aware callers can pass the result straight into parsing.
pub fn installed() -> String {
    std::env::var(TEST_VERSION_ENV)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| version().to_string())
}

pub fn installed_semver() -> Result<Version, semver::Error> {
    Version::parse(&installed())
}

/// Format the compiled version with a channel label for user-facing display.
///
/// `channel_label` is a pre-formatted suffix such as `" [alpha]"`, `" [stable]"`,
/// or `""` (empty when no cached pointer is available). Obtain it from
/// `xai_grok_update::channel_label()`.
///
/// Example: `"0.2.5 [stable]"` or `"0.2.5 [alpha]"`.
pub fn display_version(channel_label: &str) -> String {
    format!("{}{}", version(), channel_label)
}

/// Format a version-with-commit string with a channel label.
///
/// Same semantics as [`display_version`] but for the full
/// `"0.2.5 (abc1234)"` string.
pub fn display_version_with_commit(version_with_commit: &str, channel_label: &str) -> String {
    format!("{}{}", version_with_commit, channel_label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Display formatting invariant matrix — verifies label appending
    /// works correctly across all label states (alpha, stable, empty).
    #[test]
    fn test_display_version_formatting_matrix() {
        let cases: &[(&str, &str, &str)] = &[
            // (version_with_commit,    label,        expected_suffix)
            ("0.2.5 (abc1234)", " [alpha]", "0.2.5 (abc1234) [alpha]"),
            ("0.2.5 (abc1234)", " [stable]", "0.2.5 (abc1234) [stable]"),
            ("0.2.5 (abc1234)", "", "0.2.5 (abc1234)"),
            (
                "0.1.220-alpha.2 (def0)",
                " [alpha]",
                "0.1.220-alpha.2 (def0) [alpha]",
            ),
        ];
        for (vwc, label, expected) in cases {
            assert_eq!(
                display_version_with_commit(vwc, label),
                *expected,
                "display_version_with_commit({:?}, {:?})",
                vwc,
                label,
            );
        }
        // display_version uses the registered/fallback version.
        assert_eq!(display_version(""), version());
        assert!(display_version(" [stable]").ends_with("[stable]"));
    }

    #[test]
    fn compile_stamp_classifies_local_and_release_builds() {
        let local = BuildInfo::from_compile_stamp(None, "1.0.0 (abc1234)");
        assert_eq!(local.version(), FALLBACK_VERSION);
        assert_eq!(local.version_with_commit(), "1.0.0 (abc1234)");
        assert_eq!(local.kind(), BuildKind::Local);

        let release = BuildInfo::from_compile_stamp(
            Some("1.0.0-open-grok.83"),
            "1.0.0-open-grok.83 (def5678)",
        );
        assert_eq!(release.version(), "1.0.0-open-grok.83");
        assert_eq!(
            release.version_with_commit(),
            "1.0.0-open-grok.83 (def5678)"
        );
        assert_eq!(release.kind(), BuildKind::Release);

        let standalone_release = BuildInfo::from_version_stamp(Some("1.0.0-open-grok.83"));
        assert_eq!(standalone_release.version(), "1.0.0-open-grok.83");
        assert_eq!(
            standalone_release.version_with_commit(),
            "1.0.0-open-grok.83"
        );
        assert_eq!(standalone_release.kind(), BuildKind::Release);

        let standalone_local = BuildInfo::from_version_stamp(None);
        assert_eq!(standalone_local.version(), FALLBACK_VERSION);
        assert_eq!(
            standalone_local.version_with_commit(),
            FALLBACK_VERSION_WITH_COMMIT
        );
        assert_eq!(standalone_local.kind(), BuildKind::Local);
    }

    #[test]
    fn uninitialized_optimized_build_fails_closed_as_release() {
        assert_eq!(classify_build_kind(None, false), BuildKind::Release);
        assert_eq!(classify_build_kind(None, true), BuildKind::Local);
        assert_eq!(
            classify_build_kind(Some(BuildKind::Local), false),
            BuildKind::Local
        );
        assert_eq!(
            classify_build_kind(Some(BuildKind::Release), true),
            BuildKind::Release
        );
    }

    #[test]
    fn identical_concurrent_initialization_is_idempotent() {
        use std::sync::Arc;

        let cell = Arc::new(OnceLock::new());
        let info = BuildInfo::release("1.0.0-open-grok.83", "1.0.0-open-grok.83 (abc1234)");
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cell = Arc::clone(&cell);
                std::thread::spawn(move || initialize_cell(&cell, info))
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), Ok(()));
        }
        assert_eq!(cell.get(), Some(&info));
        assert_eq!(
            initialize_cell(&cell, BuildInfo::release("2.0.0", "2.0.0 (def5678)")),
            Err(BuildInfo::release("2.0.0", "2.0.0 (def5678)"))
        );
    }
}
