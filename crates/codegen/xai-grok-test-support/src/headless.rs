//! Headless mode (`grok -p`) test runner.
//!
//! Runs the grok binary as a subprocess with the mock server, captures output.

use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::AsyncReadExt as _;

use crate::env::{grok_binary, test_env_cmd_tokio};
use crate::mock_server::MockInferenceServer;

pub struct HeadlessResult {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

const HEADLESS_TIMEOUT_SECS: u64 = 60;

/// Run `grok` with the given args against the mock server, with a 60s timeout.
/// Uses an isolated HOME and disables telemetry.
pub async fn run_headless(
    server: &MockInferenceServer,
    args: &[&str],
    cwd: &Path,
) -> HeadlessResult {
    let home = TempDir::new().expect("create temp home");
    let mut cmd = tokio::process::Command::new(grok_binary());
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    test_env_cmd_tokio(&mut cmd, &server.url(), home.path());
    run_headless_with_cmd(cmd).await
}

pub async fn run_headless_with_cmd(mut cmd: tokio::process::Command) -> HeadlessResult {
    let binary = grok_binary();
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn grok binary at {}: {e}", binary.display()));

    let stdout = child.stdout.take().expect("child stdout missing");
    let stderr = child.stderr.take().expect("child stderr missing");

    let stdout_handle = tokio::spawn(async move {
        let mut stdout = stdout;
        let mut stdout_buf = Vec::new();
        stdout.read_to_end(&mut stdout_buf).await?;
        Ok::<Vec<u8>, std::io::Error>(stdout_buf)
    });
    let stderr_handle = tokio::spawn(async move {
        let mut stderr = stderr;
        let mut stderr_buf = Vec::new();
        stderr.read_to_end(&mut stderr_buf).await?;
        Ok::<Vec<u8>, std::io::Error>(stderr_buf)
    });

    let (status, timed_out) = match tokio::time::timeout(
        Duration::from_secs(HEADLESS_TIMEOUT_SECS),
        child.wait(),
    )
    .await
    {
        Ok(result) => (
            result.unwrap_or_else(|e| {
                panic!("failed to wait for grok binary {}: {e}", binary.display())
            }),
            false,
        ),
        Err(_) => {
            let _ = child.kill().await;
            let status = child.wait().await.unwrap_or_else(|e| {
                panic!(
                    "failed to kill timed out grok binary {}: {e}",
                    binary.display()
                )
            });
            (status, true)
        }
    };

    let stdout_bytes = match stdout_handle.await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => panic!("failed to read stdout from {}: {err}", binary.display()),
        Err(err) => panic!("stdout task join failed for {}: {err}", binary.display()),
    };
    let stderr_bytes = match stderr_handle.await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => panic!("failed to read stderr from {}: {err}", binary.display()),
        Err(err) => panic!("stderr task join failed for {}: {err}", binary.display()),
    };

    HeadlessResult {
        status,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        timed_out,
    }
}

const CRASH_PATTERNS: &[&str] = &[
    "panicked at",
    "SIGSEGV",
    "segfault",
    "undefined symbol",
    "SIGABRT",
    "cannot open shared object",
];

/// Diagnostic helper: format the tail of stderr for assertion messages.
pub fn stderr_tail(stderr: &str, max_chars: usize) -> &str {
    &stderr[stderr.len().saturating_sub(max_chars)..]
}

/// Assert that a headless run succeeded (non-timeout, zero exit code).
pub fn assert_headless_success(
    result: &HeadlessResult,
    label: &str,
    server: Option<&MockInferenceServer>,
) {
    assert!(
        !result.timed_out,
        "{label}: timed out after {HEADLESS_TIMEOUT_SECS}s\nstderr tail:\n{}",
        stderr_tail(&result.stderr, 500)
    );
    assert!(
        result.status.success(),
        "{label}: exited with {:?}\nstderr tail:\n{}\n{}",
        result.status.code(),
        stderr_tail(&result.stderr, 1000),
        server
            .map(|s| format!("request log:\n{}", s.request_log_summary()))
            .unwrap_or_default()
    );
}

/// Panic if stderr contains any crash/linking-failure indicators.
pub fn assert_no_crashes(stderr: &str) {
    let lower = stderr.to_lowercase();
    for pattern in CRASH_PATTERNS {
        assert!(
            !lower.contains(&pattern.to_lowercase()),
            "stderr contains crash indicator '{pattern}':\n{}",
            stderr_tail(stderr, 500)
        );
    }
}
