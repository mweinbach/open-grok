//! Cross-platform crash handler with startup crash detection.
//!
//! - **Unix**: SIGBUS/SIGSEGV via `sigaction(2)`.
//! - **Windows**: access violations via `SetUnhandledExceptionFilter`.
//!
//! # Usage
//!
//! Call [`check_previous_crash`] first to detect crashes from the previous
//! session, then [`install`] early in `main()`, before any async runtime or
//! thread spawning. `check_previous_crash` must run before `install` because
//! `install` opens `last-crash.bin` with `O_TRUNC`.
//!
//! ```rust,no_run
//! use std::path::PathBuf;
//!
//! let crash_dir = PathBuf::from("/home/user/.myapp/crash");
//!
//! if let Some(report) = xai_crash_handler::check_previous_crash(&crash_dir) {
//!     eprintln!("Application crashed during your last session.");
//!     eprintln!("  Signal: {}", report.signal_name);
//!     eprintln!("  Report: {}", report.report_path.display());
//! }
//!
//! xai_crash_handler::install(xai_crash_handler::CrashHandlerConfig {
//!     app_version: "0.1.0".to_string(),
//!     crash_dir: crash_dir.clone(),
//! });
//! ```

pub mod format;
mod handler;
pub mod symbolicate;
pub mod terminal;

use std::path::{Path, PathBuf};

pub use symbolicate::ResolvedFrame;

const MAX_HISTORY: usize = 5;

/// Configuration for the crash handler.
pub struct CrashHandlerConfig {
    /// Application version string (e.g. "0.1.169-alpha.2").
    pub app_version: String,
    /// Directory where crash dumps are written.
    /// Created if it does not exist.
    pub crash_dir: PathBuf,
}

/// Information about a crash from the previous session.
#[derive(Debug)]
pub struct CrashReport {
    /// Human-readable signal name (e.g. "SIGBUS (Bus error)").
    pub signal_name: &'static str,
    /// The `si_code` from `siginfo_t`.
    pub si_code: i32,
    /// The faulting memory address.
    pub faulting_address: u64,
    /// Unix timestamp of the crash.
    pub timestamp: u64,
    /// Application version at crash time.
    pub app_version: String,
    /// Symbolicated backtrace frames.
    pub backtrace: Vec<ResolvedFrame>,
    /// Path to the saved human-readable crash report.
    pub report_path: PathBuf,
}

/// Install the crash handler for SIGBUS and SIGSEGV.
///
/// Must be called early in `main()`, before any async runtime or thread
/// spawning. Creates `crash_dir` if it does not exist.
///
/// Returns `true` if the handler was installed successfully.
/// On unsupported platforms, this is a no-op that returns `false`.
pub fn install(config: CrashHandlerConfig) -> bool {
    handler::install(&config.crash_dir, &config.app_version)
}

/// Install a minimal SIGSEGV/SIGBUS handler that only restores the terminal.
///
/// On Unix, saves the current termios state, allocates an alternate signal
/// stack, and registers a handler that writes terminal restore escape
/// sequences to stderr, restores termios, then re-raises with default
/// disposition (preserving core dumps).
///
/// On Windows, registers an unhandled-exception filter that writes restore
/// sequences; no termios equivalent.
///
/// No-op on unsupported platforms.
///
/// No crash reporting (no file I/O, no stack walking). If [`install`] is
/// called later, it replaces these handlers with full crash-reporting
/// variants.
pub fn install_terminal_restore_only() {
    handler::install_terminal_restore_only()
}

/// Upgrade SIGSEGV/SIGBUS handlers to include terminal escape code
/// restoration. Call when TUI modes are enabled.
pub fn enable_terminal_escape_restore() {
    handler::enable_terminal_escape_restore()
}

/// Downgrade SIGSEGV/SIGBUS handlers to termios-only restoration.
/// Call when TUI modes are disabled.
pub fn disable_terminal_escape_restore() {
    handler::disable_terminal_escape_restore()
}

/// Check for a crash from the previous session.
///
/// Reads `crash_dir/last-crash.bin`, symbolicates the backtrace,
/// writes a human-readable report, and archives it. Returns `Some` if
/// a valid crash file was found, `None` otherwise.
pub fn check_previous_crash(crash_dir: &Path) -> Option<CrashReport> {
    let crash_file = crash_dir.join("last-crash.bin");
    let data = std::fs::read(&crash_file).ok()?;
    let blob = format::CrashBlob::parse(&data)?;

    let frames = symbolicate::resolve_frames(&blob);
    let report_text = symbolicate::format_report(&blob, &frames);

    // Write the human-readable report.
    let report_path = crash_dir.join("last-crash-report.txt");
    let _ = std::fs::write(&report_path, &report_text);

    // Archive to history/ (keep last MAX_HISTORY).
    archive_report(crash_dir, &report_text, blob.timestamp);

    // Remove the binary blob so it's not re-processed.
    let _ = std::fs::remove_file(&crash_file);

    Some(CrashReport {
        signal_name: symbolicate::signal_name(blob.signal),
        si_code: blob.si_code,
        faulting_address: blob.si_addr,
        timestamp: blob.timestamp,
        app_version: blob.app_version,
        backtrace: frames,
        report_path,
    })
}

fn archive_report(crash_dir: &Path, report_text: &str, timestamp: u64) {
    let history_dir = crash_dir.join("history");
    let _ = std::fs::create_dir_all(&history_dir);

    let filename = format!("crash-{}.txt", timestamp);
    let _ = std::fs::write(history_dir.join(&filename), report_text);

    // Prune old reports beyond MAX_HISTORY.
    if let Ok(mut entries) = std::fs::read_dir(&history_dir) {
        let mut files: Vec<PathBuf> = entries
            .by_ref()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "txt"))
            .collect();
        files.sort();
        if files.len() > MAX_HISTORY {
            for old in &files[..files.len() - MAX_HISTORY] {
                let _ = std::fs::remove_file(old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_previous_crash_returns_none_when_no_file() {
        let dir = PathBuf::from("/tmp/xai-crash-handler-test-nonexistent");
        assert!(check_previous_crash(&dir).is_none());
    }
}
