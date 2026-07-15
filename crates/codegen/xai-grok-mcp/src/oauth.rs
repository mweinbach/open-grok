//! OAuth flow orchestration for local MCP servers.
//!
//! Uses rmcp's `AuthorizationManager` for RFC-compliant discovery (RFC 8414 +
//! 9728), DCR, PKCE, and token exchange. Proactive discovery determines auth
//! requirements before connecting (not reactively after a 401), and `AuthClient`
//! wraps the transport for transparent token injection and refresh.
//!
//! This module handles the interactive browser-based consent flow and
//! cross-process dedup. Credential persistence is delegated to
//! [`crate::credentials::McpCredentialStoreAdapter`] (implements rmcp's
//! `CredentialStore` trait).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot, watch};

use crate::oauth_config::McpOAuthConfig;
use crate::rmcp::transport::auth::{AuthorizationManager, OAuthClientConfig};

/// Client name advertised to MCP servers during Dynamic Client Registration
/// (RFC 7591). Surfaces as the application name on third-party OAuth consent
/// screens (e.g. Linear, GitHub), so keep this human-recognizable.
const MCP_OAUTH_CLIENT_NAME: &str = "Grok";

/// How often the interactive OAuth flow polls the credential store to detect
/// a login completed in another window or process.
const CREDENTIAL_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Two-layer dedup: prevents duplicate browser tabs both within one process
// (multiple async tasks / sessions) and across separate processes (leader
// mode disabled, multiple `grok` invocations).
//
// Layer 1 (cross-process): filesystem lock at $GROK_HOME/mcp_auth_{safe_name}.lock
// Layer 2 (in-process):    watch channel so only one task runs the flow
// ---------------------------------------------------------------------------

/// In-process in-flight auth tracker. Keyed by server name.
/// Each entry has a generation counter so that when a forced override evicts
/// a stale leader, the old leader's cleanup doesn't clobber the new entry.
struct InFlightEntry {
    rx: watch::Receiver<Option<Result<(), String>>>,
    generation: u64,
}

#[allow(clippy::type_complexity)]
static IN_FLIGHT_AUTH: std::sync::LazyLock<Mutex<HashMap<String, InFlightEntry>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Run the browser-based OAuth flow with dedup.
///
/// If another task or process is already running the flow for the same server,
/// waits for it instead of opening another browser tab. The provided
/// `AuthorizationManager` must already have metadata set (from
/// `discover_metadata`). On success, the manager's credentials are updated
/// and persisted via its `CredentialStore`.
///
/// When `force` is true (user-initiated auth), any existing in-flight entry
/// is evicted so a fresh browser flow starts immediately. The old leader's
/// browser tab becomes orphaned but its cleanup is generation-safe.
pub async fn authenticate_mcp_server_dedup(
    server_name: &str,
    server_url: &str,
    auth_manager: &Arc<Mutex<AuthorizationManager>>,
    byo_config: Option<&McpOAuthConfig>,
    force: bool,
) -> Result<(), String> {
    // --- Layer 2: in-process dedup via watch channel ---
    let mut in_flight = IN_FLIGHT_AUTH.lock().await;

    // Remove stale entries left by panicked leaders (sender dropped).
    if in_flight
        .get(server_name)
        .is_some_and(|e| e.rx.has_changed().is_err())
    {
        in_flight.remove(server_name);
    }

    if let Some(entry) = in_flight.get(server_name) {
        if force {
            tracing::info!(
                server = server_name,
                "User-initiated auth override; evicting stale in-flight entry"
            );
            in_flight.remove(server_name);
        } else {
            let mut rx = entry.rx.clone();
            drop(in_flight);
            tracing::info!(
                server = server_name,
                "Another task in this process is already authenticating; waiting..."
            );
            loop {
                let snapshot = rx.borrow_and_update().clone();
                if let Some(result) = snapshot {
                    if result.is_ok() {
                        let mut mgr = auth_manager.lock().await;
                        let _ = mgr.initialize_from_store().await;
                    }
                    return result;
                }
                if rx.changed().await.is_err() {
                    return Err("Auth leader dropped".to_string());
                }
            }
        }
    }

    // We are the in-process leader.
    let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (tx, rx) = watch::channel::<Option<Result<(), String>>>(None);
    in_flight.insert(server_name.to_string(), InFlightEntry { rx, generation });
    drop(in_flight);

    // --- Layer 1: cross-process dedup via filesystem lock (Unix only) ---
    // When force is set, skip the fs lock — the old leader may still hold it
    // and we don't want to block behind a stale browser flow.
    #[cfg(unix)]
    let result = if force {
        run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await
    } else {
        authenticate_with_fs_lock(server_name, server_url, auth_manager, byo_config).await
    };
    #[cfg(not(unix))]
    let result = run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await;

    // Broadcast to in-process followers and clean up.
    // Only remove if our generation is still current (a force override may
    // have replaced us).
    let _ = tx.send(Some(result.clone()));
    let mut in_flight = IN_FLIGHT_AUTH.lock().await;
    if in_flight
        .get(server_name)
        .is_some_and(|e| e.generation == generation)
    {
        in_flight.remove(server_name);
    }

    result
}

/// Acquire a filesystem lock, then either run the auth flow (if we're the
/// first process) or reload credentials from disk (if another process
/// already completed auth while we waited for the lock).
#[cfg(unix)]
async fn authenticate_with_fs_lock(
    server_name: &str,
    server_url: &str,
    auth_manager: &Arc<Mutex<AuthorizationManager>>,
    byo_config: Option<&McpOAuthConfig>,
) -> Result<(), String> {
    use oauth2::TokenResponse as _;

    let lock_path = auth_lock_path(server_name);

    if let Some(parent) = lock_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Snapshot the current access token before waiting for the lock.
    // After acquiring, we compare to detect if another process authed.
    let token_before = {
        let mgr = auth_manager.lock().await;
        mgr.get_credentials()
            .await
            .ok()
            .and_then(|(_, tok)| tok)
            .map(|t| t.access_token().secret().to_string())
    };

    let lock_file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(%e, "Failed to create auth lock file; proceeding without cross-process dedup");
            return run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await;
        }
    };

    let lock_file = tokio::task::spawn_blocking(move || {
        use std::os::unix::io::AsRawFd;
        let fd = lock_file.as_raw_fd();
        loop {
            if unsafe { libc::flock(fd, libc::LOCK_EX) } == 0 {
                return Some(lock_file);
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
    })
    .await
    .ok()
    .flatten();

    let Some(_lock_guard) = lock_file else {
        tracing::warn!("Failed to acquire auth lock; proceeding without cross-process dedup");
        return run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await;
    };

    // We hold the lock. Reload from disk and check if another process
    // wrote a DIFFERENT token while we waited (not just any token).
    {
        let mut mgr = auth_manager.lock().await;
        if let Ok(true) = mgr.initialize_from_store().await {
            let token_after = mgr
                .get_credentials()
                .await
                .ok()
                .and_then(|(_, tok)| tok)
                .map(|t| t.access_token().secret().to_string());
            if token_after != token_before {
                tracing::info!(
                    server = server_name,
                    "Another process already authenticated; reusing fresh token"
                );
                return Ok(());
            }
        }
    }

    run_browser_auth_flow(server_name, server_url, auth_manager, byo_config).await
}

#[cfg(unix)]
fn auth_lock_path(server_name: &str) -> std::path::PathBuf {
    let safe: String = server_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    xai_grok_config::grok_home().join(format!("mcp_auth_{safe}.lock"))
}

/// Run the interactive browser-based OAuth flow.
///
/// Drives the `AuthorizationManager` (which must already have metadata set)
/// through client setup, authorization URL generation, browser consent, and
/// code exchange. Credentials are auto-persisted by the manager's
/// `CredentialStore` on successful token exchange.
async fn run_browser_auth_flow(
    server_name: &str,
    server_url: &str,
    auth_manager: &Arc<Mutex<AuthorizationManager>>,
    byo_config: Option<&McpOAuthConfig>,
) -> Result<(), String> {
    // 1. Try token refresh first (no browser needed).
    {
        let mgr = auth_manager.lock().await;
        match mgr.refresh_token().await {
            Ok(_) => {
                tracing::info!(
                    server = server_name,
                    "Token refreshed successfully (no browser)"
                );
                return Ok(());
            }
            Err(e) => {
                tracing::info!(
                    server = server_name,
                    %e,
                    "Token refresh failed, falling through to browser auth"
                );
            }
        }
    }

    // 2. Bind the loopback callback port (fixed BYO port if set, else ephemeral).
    let requested_port = byo_config.and_then(|b| b.callback_port).unwrap_or(0);
    let listener =
        tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], requested_port)))
            .await
            .map_err(|e| format!("Failed to bind loopback port {requested_port}: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("Failed to get loopback port: {e}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    // 3. Configure client and get authorization URL (lock held briefly).
    let byo_scopes: Vec<String> = byo_config
        .and_then(|b| b.scopes.as_ref())
        .cloned()
        .unwrap_or_default();

    let auth_url = {
        let mut mgr = auth_manager.lock().await;

        let scopes: Vec<String>;
        if let Some(byo) = byo_config
            && let Some(client_id) = byo.client_id.clone()
        {
            tracing::info!(
                server = server_name,
                "Using BYO client credentials (oauth_client_id from config)"
            );
            scopes = byo_scopes;
            let mut config =
                OAuthClientConfig::new(client_id, redirect_uri.clone()).with_scopes(scopes.clone());
            config.client_secret = byo.client_secret.clone();
            mgr.configure_client(config)
                .map_err(|e| format!("Failed to configure BYO client: {e}"))?;
        } else {
            scopes = if byo_scopes.is_empty() {
                mgr.select_scopes(None, &[])
            } else {
                byo_scopes
            };
            let scope_refs: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
            mgr.register_client(MCP_OAUTH_CLIENT_NAME, &redirect_uri, &scope_refs)
                .await
                .map_err(|e| format!("Dynamic client registration failed: {e}"))?;
        }
        let scopes: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();

        mgr.get_authorization_url(&scopes)
            .await
            .map_err(|e| format!("Failed to get authorization URL: {e}"))?
    };
    // Lock released — browser flow can take minutes.

    // Snapshot token before browser opens so we can detect if another flow
    // (e.g. the old evicted leader, or another process) writes fresh tokens.
    let token_before_browser = {
        let mgr = auth_manager.lock().await;
        mgr.get_credentials()
            .await
            .ok()
            .and_then(|(_, tok)| tok)
            .map(|t| {
                use oauth2::TokenResponse as _;
                t.access_token().secret().to_string()
            })
    };

    // 4. Open browser for user consent.
    tracing::info!(server = server_name, "Opening browser for OAuth consent");
    if let Err(e) = webbrowser::open(&auth_url) {
        // eprintln! corrupts the TUI alternate screen (in-process, fd 2).
        // TODO: surface auth URL via ACP notification instead.
        tracing::warn!(%e, url = %auth_url, "Failed to open browser for MCP OAuth; user must visit URL manually");
    }

    // 5. Wait for the OAuth callback OR for tokens to appear on disk.
    //    The credential store poll catches the case where a force-evicted
    //    old leader (or another process) completed auth via a different
    //    browser tab while we're waiting for our own callback.
    //
    //    IMPORTANT: peek the on-disk file directly via `McpCredentialStore::load_default`
    //    rather than calling `mgr.initialize_from_store()`. The latter has the
    //    side effect of running `configure_client_id(stored.client_id)`, which
    //    replaces `oauth_client`'s freshly-DCR'd `client_id` and ephemeral
    //    `redirect_uri` with the *old* stored values (`redirect_uri = base_url`).
    //    If the user then completes the browser flow before another process
    //    writes new tokens, `exchange_code_for_token` would use the clobbered
    //    config and the server would reject with `invalid_grant: Invalid redirect_uri`.
    let parsed_server_url = match url::Url::parse(server_url) {
        Ok(u) => Some(u),
        Err(e) => {
            tracing::warn!(
                server = server_name,
                url = server_url,
                error = %e,
                "could not parse server URL for credential-store poll; falling back to callback-only auth-completion detection"
            );
            None
        }
    };
    let server_name_for_poll = server_name.to_string();
    let token_snapshot = token_before_browser.clone();
    let poll_store = async move {
        let Some(url) = parsed_server_url else {
            // No credential-store key — disable the poll. Callback path still works.
            std::future::pending::<()>().await;
            return;
        };
        loop {
            tokio::time::sleep(CREDENTIAL_POLL_INTERVAL).await;
            let Ok(store) = crate::credentials::McpCredentialStore::load_default() else {
                continue;
            };
            let token_now = store
                .get(&server_name_for_poll, &url)
                .and_then(|entry| entry.token_response.as_ref())
                .map(|t| {
                    use oauth2::TokenResponse as _;
                    t.access_token().secret().to_string()
                });
            if token_now.is_some() && token_now != token_snapshot {
                return;
            }
        }
    };

    let (callback_server, callback_rx) = start_oauth_callback_server(listener);

    tokio::select! {
        result = callback_rx => {
            callback_server.abort();
            let (code, csrf_state) = result
                .map_err(|_| "Callback channel dropped".to_string())?
                .map_err(|e| format!("OAuth callback failed: {e}"))?;

            // 6. Exchange code for tokens (auto-persists via CredentialStore).
            let mgr = auth_manager.lock().await;
            mgr.exchange_code_for_token(&code, &csrf_state)
                .await
                .map_err(|e| format!("Token exchange failed: {e}"))?;

            tracing::info!(server = server_name, "MCP OAuth authentication successful");
        }
        _ = poll_store => {
            callback_server.abort();
            tracing::info!(
                server = server_name,
                "Fresh tokens detected on disk from another auth flow; skipping callback wait"
            );
        }
    }

    Ok(())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Start a loopback HTTP server for the OAuth callback.
///
/// Returns the server task handle (for cleanup) and a oneshot receiver
/// that resolves with `(code, state)` when the callback arrives.
///
/// The caller is responsible for aborting the server handle.
#[allow(clippy::type_complexity)]
fn start_oauth_callback_server(
    listener: tokio::net::TcpListener,
) -> (
    tokio::task::JoinHandle<()>,
    oneshot::Receiver<Result<(String, String), String>>,
) {
    use axum::{Router, extract::Query, response::Html, routing::get};

    let (tx, rx) = oneshot::channel::<Result<(String, String), String>>();
    let tx = Arc::new(tokio::sync::Mutex::new(Some(tx)));

    let handler = {
        let tx = tx.clone();
        move |Query(params): Query<HashMap<String, String>>| {
            let tx = tx.clone();
            async move {
                let result = if let Some(error) = params.get("error") {
                    let desc = params
                        .get("error_description")
                        .cloned()
                        .unwrap_or_else(|| "Unknown error".to_string());
                    Err(format!("OAuth error: {error} - {desc}"))
                } else {
                    match (params.get("code"), params.get("state")) {
                        (Some(code), Some(state)) => Ok((code.clone(), state.clone())),
                        (None, _) => Err("Missing authorization code".to_string()),
                        (_, None) => Err("Missing state parameter".to_string()),
                    }
                };

                let html = match &result {
                    Ok(_) => {
                        r#"<!DOCTYPE html><html><head><title>Authorization Complete</title></head>
                    <body style="font-family: sans-serif; text-align: center; padding: 50px;">
                    <h1>Authorization Complete</h1>
                    <p>You can close this window and return to the terminal.</p>
                    <script>window.close();</script></body></html>"#
                            .to_string()
                    }
                    Err(e) => {
                        let msg = html_escape(e);
                        format!(
                            r#"<!DOCTYPE html><html><head><title>Authorization Failed</title></head>
                            <body style="font-family: sans-serif; text-align: center; padding: 50px;">
                            <h1>Authorization Failed</h1>
                            <p>{msg}</p>
                            <p>You can close this window and return to the terminal.</p>
                            </body></html>"#
                        )
                    }
                };

                if let Some(tx) = tx.lock().await.take() {
                    let _ = tx.send(result);
                }

                Html(html)
            }
        }
    };

    let app = Router::new().route("/callback", get(handler.clone()).post(handler));

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (server, rx)
}
