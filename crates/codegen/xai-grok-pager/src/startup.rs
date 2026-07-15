//! Generic startup warnings displayed on the welcome screen.
//!
//! Any subsystem (terminal diagnostics, auth, config migration, etc.) can
//! produce [`StartupWarning`]s.

/// A non-fatal startup warning from any subsystem.
///
/// This is a **display contract only** -- the subsystem formats the message
/// and optional action hint. Detailed diagnostics (fix commands, config paths)
/// live in the subsystem-specific slash commands (e.g. `/terminal-setup`).
#[derive(Debug, Clone)]
pub struct StartupWarning {
    /// Severity controls rendering color (yellow for warnings, dim for info).
    pub severity: WarningSeverity,
    /// Short, user-facing message (fits in ~60 columns).
    pub message: String,
    /// Optional action hint (e.g. "run /terminal-setup").
    pub action: Option<String>,
}

/// Severity level for startup warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarningSeverity {
    /// Rendered in warning color (yellow). Something is misconfigured.
    Warning,
    /// Rendered in dim/gray. Informational, not actionable.
    Info,
}
