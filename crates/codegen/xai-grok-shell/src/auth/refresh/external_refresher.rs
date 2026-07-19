use std::sync::Arc;

use crate::auth::error::RefreshTokenFailedReason;
use crate::auth::manager::RefreshReason;

use super::{ExternalCommandRunner, RefreshOutcome, TokenRefresher};

/// Refreshes by re-running the operator's external auth binary via
/// `spawn_blocking`. Pure data return -- mutation lives in
/// `refresh_chain` (honors the [`TokenRefresher`] no-mutation contract).
pub(crate) struct ExternalBinaryRefresher {
    runner: Arc<dyn ExternalCommandRunner>,
    command: String,
    timeout: std::time::Duration,
}

impl ExternalBinaryRefresher {
    pub(crate) fn new(runner: Arc<dyn ExternalCommandRunner>, command: String) -> Self {
        Self {
            runner,
            command,
            timeout: EXTERNAL_REFRESH_TIMEOUT,
        }
    }

    /// Override the binary timeout (tests use a short one to exercise the
    /// timeout arm without a real 30s wait).
    #[cfg(test)]
    pub(crate) fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// A failed binary run is a single-strike `Other` permanent failure; the
    /// `PERMANENT_FAILURE_TTL` lets a flaky binary self-heal without `/login`.
    /// No consecutive-blip tolerance like OIDC: a local binary failure is a
    /// stronger signal than a network refresh blip.
    fn record_failure(&self, message: String) -> RefreshOutcome {
        tracing::warn!(%message, "auth: external binary refresh failed -> permanent");
        // No token key in the binary flow; the caller scopes the verdict.
        RefreshOutcome::permanent(RefreshTokenFailedReason::Other, None)
    }
}

/// Timeout for the external auth binary. If the binary hangs, the
/// `spawn_blocking` thread is leaked (it cannot be interrupted), but this is
/// acceptable: the thread holds no locks and mutates no shared state.
const EXTERNAL_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[async_trait::async_trait]
impl TokenRefresher for ExternalBinaryRefresher {
    async fn refresh(&self, reason: RefreshReason) -> RefreshOutcome {
        tracing::debug!(?reason, "auth: external binary refresh starting");
        let runner = self.runner.clone();
        let cmd = self.command.clone();
        let timeout_ms = self.timeout.as_millis() as u64;
        match tokio::time::timeout(
            self.timeout,
            tokio::task::spawn_blocking(move || runner.run_external_command(&cmd)),
        )
        .await
        {
            Err(_elapsed) => {
                // Transient: a hard-expired access token after idle must still
                // allow 401 / pre-flight retry. Mapping timeout to permanent
                // failure poisoned recovery for PERMANENT_FAILURE_TTL.
                tracing::warn!(
                    timeout_ms,
                    "auth: external binary refresh timed out (thread leaked)"
                );
                crate::unified_log::warn(
                    "auth.refresh.external_timeout",
                    None,
                    Some(serde_json::json!({ "timeout_ms": timeout_ms })),
                );
                RefreshOutcome::transient(format!("external binary timed out after {timeout_ms}ms"))
            }
            Ok(Ok(Some(auth))) => {
                crate::unified_log::info("auth: external binary refresh succeeded", None, None);
                RefreshOutcome::success(auth)
            }
            Ok(Ok(None)) => {
                crate::unified_log::warn(
                    "auth: external binary refresh returned no token",
                    None,
                    None,
                );
                self.record_failure("external binary returned no token".into())
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "auth: external binary refresh task failed");
                self.record_failure(format!("external binary task failed: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::GrokAuth;

    /// Minimal runner whose external command returns a fixed result.
    struct FakeRunner {
        external_result: Option<GrokAuth>,
    }
    impl ExternalCommandRunner for FakeRunner {
        fn run_external_command(&self, _command: &str) -> Option<GrokAuth> {
            self.external_result.clone()
        }
    }

    /// A failed binary run is a single-strike `Other` permanent failure that is
    /// NON-sticky: it must age out via the TTL, never lock an external-binary
    /// user out forever. (Flipping this to a sticky reason would be a silent
    /// lockout regression.)
    #[tokio::test]
    async fn external_binary_failure_is_single_strike_non_sticky_permanent() {
        let refresher = ExternalBinaryRefresher::new(
            Arc::new(FakeRunner {
                external_result: None,
            }),
            "auth-binary".into(),
        );
        match refresher.refresh(RefreshReason::ServerRejected).await {
            RefreshOutcome::PermanentFailure { error, .. } => {
                assert_eq!(error.reason, RefreshTokenFailedReason::Other);
                assert!(
                    !error.reason.is_sticky(),
                    "external-binary failure must age out, not strand the user forever",
                );
            }
            other => panic!("a failed binary run must be a permanent Other failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_binary_timeout_is_transient() {
        struct SlowRunner;
        impl ExternalCommandRunner for SlowRunner {
            fn run_external_command(&self, _command: &str) -> Option<GrokAuth> {
                std::thread::sleep(std::time::Duration::from_millis(50));
                Some(GrokAuth::test_default())
            }
        }
        let refresher = ExternalBinaryRefresher::new(Arc::new(SlowRunner), "auth-binary".into())
            .with_timeout(std::time::Duration::from_millis(5));
        match refresher.refresh(RefreshReason::ServerRejected).await {
            RefreshOutcome::TransientFailure { message } => {
                assert!(
                    message.contains("timed out"),
                    "timeout message must be greppable, got {message}"
                );
            }
            other => panic!("a timed-out binary must be TransientFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn external_binary_success_returns_fresh_token() {
        let token = GrokAuth {
            key: "ext-fresh".into(),
            ..GrokAuth::test_default()
        };
        let refresher = ExternalBinaryRefresher::new(
            Arc::new(FakeRunner {
                external_result: Some(token),
            }),
            "auth-binary".into(),
        );
        match refresher.refresh(RefreshReason::ServerRejected).await {
            RefreshOutcome::Success(auth) => assert_eq!(auth.key, "ext-fresh"),
            other => panic!("a successful binary run must return Success, got {other:?}"),
        }
    }
}
