use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::auth::{AuthManager, GrokAuth};

#[must_use]
pub struct ManagedConfigRefresher {
    pub(super) cancel: CancellationToken,
    pub(super) handle: tokio::task::JoinHandle<()>,
}

impl ManagedConfigRefresher {
    pub(super) fn spawn(
        parent: &CancellationToken,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> Self {
        let cancel = parent.child_token();
        let stop = cancel.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = stop.cancelled() => {}
                _ = work => {}
            }
        });
        Self { cancel, handle }
    }
}

impl Drop for ManagedConfigRefresher {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.handle.abort();
    }
}

static REFRESH_SUPERVISOR: Mutex<Option<ManagedConfigRefresher>> = Mutex::new(None);

pub(crate) fn should_start_refresh_supervisor(
    xai_session_route: bool,
    has_deployment_key: bool,
    auth: Option<&GrokAuth>,
) -> bool {
    xai_session_route && (has_deployment_key || auth.is_some_and(GrokAuth::is_managed_mcp_eligible))
}

pub fn start_refresh_supervisor(auth_manager: &Arc<AuthManager>) {
    super::clear_orphan();
    let mut slot = REFRESH_SUPERVISOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ensure_supervisor(&mut slot, || {
        spawn_refresh_supervisor(&CancellationToken::new(), auth_manager.clone())
    });
}

pub(super) fn ensure_supervisor(
    slot: &mut Option<ManagedConfigRefresher>,
    spawn: impl FnOnce() -> ManagedConfigRefresher,
) {
    if slot
        .as_ref()
        .is_some_and(|supervisor| !supervisor.handle.is_finished())
    {
        return;
    }
    *slot = Some(spawn());
}

pub fn take_refresh_supervisor() -> Option<ManagedConfigRefresher> {
    REFRESH_SUPERVISOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

pub fn spawn_refresh_supervisor(
    cancel: &CancellationToken,
    auth_manager: Arc<AuthManager>,
) -> ManagedConfigRefresher {
    ManagedConfigRefresher::spawn(cancel, async move {
        revalidate_stale_start(&auth_manager).await;
        let mut interval = tokio::time::interval(super::managed_config_sync_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            super::clear_orphan();
            super::bump_managed_rollback_floor();
            revalidate_stale_start(&auth_manager).await;
        }
    })
}

async fn revalidate_stale_start(auth_manager: &Arc<AuthManager>) {
    if !super::is_fetch_enabled()
        || (super::resolve_deployment_key().is_none()
            && matches!(super::team_principal_signed_in(), Ok(false)))
        || !crate::config::is_managed_config_stale_for(&super::current_serving_identity_any_expiry())
    {
        return;
    }
    let team = refreshed_team_principal(auth_manager).await;
    match super::sync_bounded(super::SyncBudget::Revalidate, team).await {
        Some(Ok(_)) => {}
        Some(Err(error)) => tracing::debug!("managed config revalidation failed: {error}"),
        None => tracing::debug!("managed config revalidation timed out"),
    }
}

pub(super) async fn refreshed_team_principal(auth_manager: &Arc<AuthManager>) -> Option<GrokAuth> {
    if !auth_manager
        .current_or_expired()
        .is_some_and(|auth| auth.is_team_principal())
    {
        return None;
    }
    refresh_with_deadline(super::SESSION_START_AUTH_DEADLINE, {
        let auth_manager = auth_manager.clone();
        async move { auth_manager.auth().await }
    })
    .await
    .and_then(Result::ok)
    .and_then(super::eligible_team_principal)
}

pub(super) async fn refresh_with_deadline<RefreshResult: Send + 'static>(
    deadline: std::time::Duration,
    refresh: impl std::future::Future<Output = RefreshResult> + Send + 'static,
) -> Option<RefreshResult> {
    tokio::time::timeout(deadline, tokio::spawn(refresh))
        .await
        .ok()
        .and_then(Result::ok)
}

pub(crate) fn policy_repair_pending() -> bool {
    policy_repair_pending_from(
        super::resolve_deployment_key().is_some(),
        &super::team_principal_signed_in(),
    )
}

pub(super) fn policy_repair_pending_from(
    has_deployment_key: bool,
    signed_in_team: &std::io::Result<bool>,
) -> bool {
    if !super::is_fetch_enabled() || (!has_deployment_key && matches!(signed_in_team, Ok(false))) {
        return false;
    }
    let identity = super::current_serving_identity_any_expiry();
    matches!(identity, crate::config::ServingIdentity::None)
        || crate::config::is_managed_config_hard_stale_for(&identity)
}
