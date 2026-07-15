//! General subprocess plumbing shared by the harnesses in this crate.

use std::sync::Arc;

/// Pipe all three stdio handles, `kill_on_drop`, spawn, and drain the child's
/// stderr into the returned buffer on a background task. The one spawn path
/// shared by every subprocess harness in this crate (`GrokStdioClient`,
/// `RawStdioClient`, `leader::LeaderStdioClient`); env/args stay with the
/// callers, whose hermeticity models differ (sandbox-inherit vs `env_clear`).
/// The drain future is `Send`, so this works on and off a `LocalSet`.
pub(crate) fn spawn_piped_with_stderr_capture(
    mut cmd: tokio::process::Command,
) -> (tokio::process::Child, Arc<std::sync::Mutex<Vec<u8>>>) {
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    // Derived from `cmd` itself so the panic can never name a different binary
    // than the one actually spawned.
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .spawn()
        .unwrap_or_else(|e| panic!("failed to spawn grok at {program}: {e}"));

    let stderr = Arc::new(std::sync::Mutex::new(Vec::new()));
    let stderr_capture = stderr.clone();
    let mut child_stderr = child.stderr.take().expect("child stderr missing");
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;

        let mut buf = [0_u8; 1024];
        loop {
            match child_stderr.read(&mut buf).await {
                Ok(0) => break,
                Ok(read) => stderr_capture
                    .lock()
                    .unwrap()
                    .extend_from_slice(&buf[..read]),
                Err(_) => break,
            }
        }
    });

    (child, stderr)
}
