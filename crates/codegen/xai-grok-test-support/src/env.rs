//! Binary resolution, serial env guards, and git sandbox creation.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use crate::sandbox::TestSandbox;

// First setter wins and later sets are ignored, so parallel tests never race the process-wide choice.
static GROK_BINARY_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

pub fn set_grok_binary_override(path: PathBuf) {
    let _ = GROK_BINARY_OVERRIDE.set(path);
}

/// Parse env var `key` into `T`, falling back to `default` when it is unset or
/// present-but-unparseable (warning in the latter case).
pub fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    let Ok(raw) = std::env::var(key) else {
        return default;
    };
    match raw.parse() {
        Ok(value) => value,
        Err(_) => {
            eprintln!("[test-support] ignoring unparseable {key}={raw:?}; using default");
            default
        }
    }
}

/// RAII guard for a single environment variable in `#[serial]` tests: snapshots
/// the prior value on construction, applies the change, then restores the prior
/// value (or unsets it) on drop — even if an assertion panics. Restoring rather
/// than always unsetting avoids clobbering vars a parent process/harness set
/// (e.g. `RUST_LOG`).
///
/// Callers MUST be `#[serial_test::serial]`: the `unsafe` `set_var`/`remove_var`
/// are sound only when no other thread accesses the environment concurrently.
pub struct EnvGuard {
    key: &'static str,
    prior: Option<OsString>,
}

impl EnvGuard {
    /// Set `key` to `value` for the guard's lifetime. Accepts `&str`, `&Path`,
    /// `String`, etc. via `AsRef<OsStr>`.
    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let prior = std::env::var_os(key);
        // SAFETY: callers are `#[serial]`, so no other thread touches the env.
        unsafe { std::env::set_var(key, value) };
        Self { key, prior }
    }

    /// Unset `key` for the guard's lifetime.
    pub fn unset(key: &'static str) -> Self {
        let prior = std::env::var_os(key);
        // SAFETY: see [`EnvGuard::set`].
        unsafe { std::env::remove_var(key) };
        Self { key, prior }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: see [`EnvGuard::set`].
        match self.prior.take() {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

fn workspace_root() -> PathBuf {
    // nth(3): crate is nested three levels below the cargo workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root")
        .to_path_buf()
}

fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("target"))
}

/// Resolve a workspace binary: the prebuilt `target/debug/<bin>`, else `cargo build -p <package>
/// --bin <bin>` with stdin closed, `pager_env()` applied, and the child detached from the TTY.
pub fn ensure_cargo_bin(package: &str, bin: &str) -> PathBuf {
    let binary = target_dir()
        .join("debug")
        .join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
    if binary.exists() {
        return binary;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(workspace_root())
        .args(["build", "-p", package, "--bin", bin])
        .stdin(std::process::Stdio::null())
        .envs(xai_tty_utils::pager_env());
    xai_tty_utils::detach_std_command(&mut cmd);
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {cargo} to build {bin}: {e}"));

    assert!(
        output.status.success(),
        "failed to build {bin} (exit {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        binary.exists(),
        "{bin} build completed but binary missing at {}",
        binary.display()
    );
    binary
}

// Reserve four cores for the pager children other test threads spawn, so a cold build does not starve them.
fn build_jobs() -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    cores.saturating_sub(4).max(1)
}

pub fn ensure_cargo_bin_with_features(
    package: &str,
    bin: &str,
    features: &[&str],
    target_subdir: &str,
) -> PathBuf {
    let out_target = target_dir().join(target_subdir);
    let binary = out_target
        .join("debug")
        .join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
    if binary.exists() {
        return binary;
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let mut cmd = Command::new(&cargo);
    cmd.current_dir(workspace_root())
        .args(["build", "-p", package, "--bin", bin])
        .args(["--features", &features.join(",")])
        .args(["--jobs", &build_jobs().to_string()])
        .env("CARGO_TARGET_DIR", &out_target)
        .stdin(std::process::Stdio::null())
        .envs(xai_tty_utils::pager_env());
    xai_tty_utils::detach_std_command(&mut cmd);
    let output = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {cargo} to build {bin}: {e}"));

    assert!(
        output.status.success(),
        "failed to build {bin} with features {features:?} (exit {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        binary.exists(),
        "{bin} build completed but binary missing at {}",
        binary.display()
    );
    binary
}

/// Resolve the Open Grok binary: `GROK_BINARY` env (CI) or a local `open-grok` build.
pub fn grok_binary() -> PathBuf {
    if let Some(path) = GROK_BINARY_OVERRIDE.get() {
        return path.clone();
    }
    if let Ok(path) = std::env::var("GROK_BINARY") {
        let p = PathBuf::from(path);
        assert!(p.exists(), "GROK_BINARY does not exist: {}", p.display());
        // Bazel's GROK_BINARY is runfiles-relative; the harness spawns the child
        // with a different cwd, so absolutize against the (runfiles-root) cwd now.
        return std::path::absolute(&p).unwrap_or(p);
    }

    if let Ok(path) = std::env::var("CARGO_BIN_EXE_open-grok") {
        let p = PathBuf::from(path);
        if p.exists() {
            return p;
        }
    }

    ensure_cargo_bin("xai-grok-pager-bin", "open-grok")
}

/// Create an owned, git-initialized [`TestSandbox`].
pub fn git_workdir() -> TestSandbox {
    TestSandbox::builder().git().build()
}
