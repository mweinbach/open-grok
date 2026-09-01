use crate::session::SessionThread;

pub(super) const UNPROMOTED_SESSION_EXIT_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);

pub(super) async fn await_session_thread_exit(
    thread: &SessionThread,
    timeout: std::time::Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if thread.is_finished() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        tokio::time::sleep(remaining.min(std::time::Duration::from_millis(10))).await;
    }
}

#[cfg(test)]
#[path = "child_runtime_tests.rs"]
mod tests;
