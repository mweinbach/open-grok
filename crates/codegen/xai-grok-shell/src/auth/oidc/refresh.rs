//! Pure-data OIDC refresh. Talks to the IdP and returns
//! [`OidcRefreshResult`] without touching [`AuthManager`].

use super::super::GrokAuth;
use super::protocol::{OidcError, OidcUserInfo, build_grok_auth, discover, refresh_tokens};
use crate::auth::error::RefreshTokenFailedReason;

/// Outcome of a pure OIDC token refresh (no AuthManager mutations).
pub(crate) enum OidcRefreshResult {
    /// Fresh token obtained. Caller must persist.
    Success(Box<GrokAuth>),
    /// Terminal error from the IdP, already classified into a reason.
    TerminalError { reason: RefreshTokenFailedReason },
    /// Non-terminal failure (discovery failed, network error, etc.)
    Failed,
}

/// Classify an OAuth2 `error` code as a terminal refresh failure. `None` means
/// non-terminal (retryable). Single source of truth for which codes are fatal;
/// the retry gate (`protocol::is_transient_refresh_error`) defers to this too.
pub(super) fn classify_terminal(error_code: &str) -> Option<RefreshTokenFailedReason> {
    match error_code {
        "invalid_grant" => Some(RefreshTokenFailedReason::RefreshTokenRejected),
        "invalid_client" => Some(RefreshTokenFailedReason::ClientRejected),
        _ => None,
    }
}

/// `oauth2-provider` refresh-token rotation-grace window (ms). Only a clock
/// divergence past this bound is flagged as a suspected suspend-straddle, since
/// a longer suspend can turn a lost refresh response into a revoked RT.
const ROTATION_GRACE_MS: u64 = 60_000;

/// Exchange a refresh_token for fresh tokens at the IdP. Pure data return, no
/// `AuthManager` mutations; the caller (`OidcRefresher`) routes the result
/// through `refresh_chain`.
pub(crate) async fn oidc_token_exchange(auth: &GrokAuth) -> OidcRefreshResult {
    let has_rt = auth.refresh_token.is_some();
    let has_issuer = auth.oidc_issuer.is_some();
    let has_client_id = auth.oidc_client_id.is_some();
    tracing::debug!(
        has_rt,
        has_issuer,
        has_client_id,
        "oidc try_refresh_pure enter"
    );
    if !has_rt || !has_issuer || !has_client_id {
        xai_grok_telemetry::unified_log::warn(
            "oidc try_refresh skipped: missing fields",
            None,
            Some(serde_json::json!({
                "has_refresh_token": has_rt,
                "has_issuer": has_issuer,
                "has_client_id": has_client_id,
                "auth_mode": format!("{:?}", auth.auth_mode),
            })),
        );
    }
    let Some(refresh_tok) = auth.refresh_token.as_ref() else {
        return OidcRefreshResult::Failed;
    };
    let Some(issuer) = auth.oidc_issuer.as_ref() else {
        return OidcRefreshResult::Failed;
    };
    let Some(client_id) = auth.oidc_client_id.as_ref() else {
        return OidcRefreshResult::Failed;
    };

    crate::unified_log::info(
        "oidc try_refresh_pure enter",
        None,
        Some(serde_json::json!({ "issuer": issuer, "client_id": client_id })),
    );

    // Suspend probe: the monotonic clock pauses while the machine is asleep
    // but the wall clock does not, so a large divergence around the IdP call
    // means the process was suspended mid-refresh — the exact condition that
    // can revoke the refresh token (response lost across sleep).
    let started_mono = std::time::Instant::now();
    let started_wall = chrono::Utc::now();
    let timing = || {
        let mono_ms = started_mono.elapsed().as_millis() as u64;
        let wall_ms = (chrono::Utc::now() - started_wall)
            .num_milliseconds()
            .max(0) as u64;
        let suspended_ms = wall_ms.saturating_sub(mono_ms);
        (
            mono_ms,
            wall_ms,
            suspended_ms,
            suspended_ms > ROTATION_GRACE_MS,
        )
    };

    let discovery = match discover(issuer).await {
        Ok(d) => d,
        Err(e) => {
            let (mono_ms, wall_ms, suspended_ms, suspected_suspend) = timing();
            crate::unified_log::error(
                "oidc try_refresh_pure discovery failed",
                None,
                Some(serde_json::json!({
                    "error": format!("{e:#}"),
                    "mono_ms": mono_ms,
                    "wall_ms": wall_ms,
                    "suspended_ms": suspended_ms,
                    "suspected_suspend": suspected_suspend,
                })),
            );
            if suspected_suspend {
                emit_suspend_spanned("discovery_failed", suspended_ms);
            }
            return OidcRefreshResult::Failed;
        }
    };
    let tokens = match refresh_tokens(
        &discovery.token_endpoint,
        refresh_tok,
        client_id,
        auth.principal_type.as_deref(),
        auth.principal_id.as_deref(),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            if let Some(OidcError::TokenRefreshHttp { body, .. }) = e.downcast_ref::<OidcError>()
                && let Some(error_code) = serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|v| v.get("error")?.as_str().map(str::to_owned))
                && let Some(reason) = classify_terminal(&error_code)
            {
                let (mono_ms, wall_ms, suspended_ms, suspected_suspend) = timing();
                let cred_age_secs = auth.mint_age_seconds();
                crate::unified_log::error(
                    "oidc try_refresh_pure terminal error",
                    None,
                    Some(serde_json::json!({
                        "error_code": error_code,
                        "client_id": client_id,
                        "tried_rt_prefix": auth.refresh_token.as_deref().map(crate::auth::token_suffix),
                        "error_description": serde_json::from_str::<serde_json::Value>(body)
                            .ok()
                            .and_then(|v| v.get("error_description").cloned()),
                        "mono_ms": mono_ms,
                        "wall_ms": wall_ms,
                        "suspended_ms": suspended_ms,
                        "suspected_suspend": suspected_suspend,
                        "cred_age_secs": cred_age_secs,
                    })),
                );
                if suspected_suspend {
                    emit_suspend_spanned(&error_code, suspended_ms);
                }
                return OidcRefreshResult::TerminalError { reason };
            }
            let http_status = e.downcast_ref::<OidcError>().and_then(|oe| match oe {
                OidcError::TokenRefreshHttp { status, .. } => Some(*status),
                _ => None,
            });
            let (mono_ms, wall_ms, suspended_ms, suspected_suspend) = timing();
            crate::unified_log::error(
                "oidc try_refresh_pure token exchange failed",
                None,
                Some(serde_json::json!({
                    "error": e.to_string(),
                    "client_id": client_id,
                    "http_status": http_status,
                    "mono_ms": mono_ms,
                    "wall_ms": wall_ms,
                    "suspended_ms": suspended_ms,
                    "suspected_suspend": suspected_suspend,
                })),
            );
            tracing::warn!(
                error = %e,
                http_status = ?http_status,
                client_id = %client_id,
                issuer = %issuer,
                "OIDC: token refresh failed"
            );
            if suspected_suspend {
                emit_suspend_spanned("transient_failed", suspended_ms);
            }
            return OidcRefreshResult::Failed;
        }
    };

    // Reuse identity from original login; new id_token from refresh is intentionally skipped.
    let user_info = OidcUserInfo {
        user_id: auth.user_id.clone(),
        email: auth.email.clone(),
        first_name: auth.first_name.clone(),
        last_name: auth.last_name.clone(),
        profile_image_asset_id: auth.profile_image_asset_id.clone(),
        principal_type: auth.principal_type.clone(),
        principal_id: auth.principal_id.clone(),
        team_id: auth.team_id.clone(),
        team_name: auth.team_name.clone(),
        team_role: auth.team_role.clone(),
        organization_id: auth.organization_id.clone(),
        organization_name: auth.organization_name.clone(),
        organization_role: auth.organization_role.clone(),
        user_blocked_reason: auth.user_blocked_reason.clone(),
        team_blocked_reasons: auth.team_blocked_reasons.clone(),
        coding_data_retention_opt_out: auth.coding_data_retention_opt_out,
    };
    let mut new_auth = build_grok_auth(tokens, user_info, issuer, client_id);
    let idp_rotated = new_auth.refresh_token.is_some();
    // Keep old refresh token if IdP didn't rotate it
    if new_auth.refresh_token.is_none() {
        new_auth.refresh_token = auth.refresh_token.clone();
    }
    tracing::debug!(
        idp_rotated,
        key_prefix = crate::auth::token_suffix(&new_auth.key),
        "oidc try_refresh_pure token obtained"
    );
    let (mono_ms, wall_ms, suspended_ms, suspected_suspend) = timing();
    crate::unified_log::info(
        "oidc try_refresh_pure succeeded",
        None,
        Some(serde_json::json!({
            "expires_at": new_auth.expires_at.map(|e| e.to_rfc3339()),
            "mono_ms": mono_ms,
            "wall_ms": wall_ms,
            "suspended_ms": suspended_ms,
            "suspected_suspend": suspected_suspend,
        })),
    );
    if suspected_suspend {
        emit_suspend_spanned("ok", suspended_ms);
    }
    OidcRefreshResult::Success(Box::new(new_auth))
}

/// Alertable event: an OIDC refresh's network call spanned a suspend (wall
/// clock ran far ahead of the monotonic clock) — the precondition for a
/// lost-response refresh-token revocation.
fn emit_suspend_spanned(outcome: &str, suspended_ms: u64) {
    crate::unified_log::warn(
        "auth.refresh.suspend_spanned",
        None,
        Some(serde_json::json!({
            "outcome": outcome,
            "suspended_ms": suspended_ms,
        })),
    );
}
