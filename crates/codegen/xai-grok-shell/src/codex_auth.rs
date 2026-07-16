//! Isolated OpenAI Codex (ChatGPT) OAuth account support.
//!
//! This intentionally does not reuse Grok's primary [`crate::auth::AuthManager`].
//! Codex login, refresh, logout, and quota failures must never mutate xAI's
//! `auth.json` or change the ACP primary-auth state.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use axum::extract::{Query, State};
use axum::response::Html;
use axum::routing::get;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpListener;

pub const CODEX_AUTH_FILE_NAME: &str = "codex-auth.json";
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const CODEX_ISSUER: &str = "https://auth.openai.com";
pub const CODEX_BACKEND_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub const CODEX_INFERENCE_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const CODEX_SCOPE: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";
const DEFAULT_CALLBACK_PORT: u16 = 1455;
const FALLBACK_CALLBACK_PORT: u16 = 1457;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REFRESH_WINDOW_SECS: i64 = 5 * 60;
const UNKNOWN_EXPIRY_REFRESH_DAYS: i64 = 8;

#[derive(Clone, Debug)]
struct CodexEndpoints {
    issuer: String,
    backend_base_url: String,
    client_id: String,
}

impl Default for CodexEndpoints {
    fn default() -> Self {
        Self {
            issuer: std::env::var("GROK_CODEX_AUTH_BASE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| CODEX_ISSUER.to_owned()),
            backend_base_url: std::env::var("GROK_CODEX_BACKEND_BASE_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| CODEX_BACKEND_BASE_URL.to_owned()),
            client_id: std::env::var("CODEX_APP_SERVER_LOGIN_CLIENT_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| CODEX_CLIENT_ID.to_owned()),
        }
    }
}

/// Grok-owned copy of Codex's plaintext auth schema, stored separately at
/// `~/.grok/codex-auth.json` with owner-only permissions.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CodexAuthStore {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    pub openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<CodexTokenData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CodexTokenData {
    /// Raw ID-token JWT, matching codex-rs's serialized `TokenData` shape.
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexCredentials {
    pub access_token: String,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan_type: Option<String>,
    pub account_is_fedramp: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexAccountSummary {
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub plan_type: Option<String>,
}

impl From<&CodexCredentials> for CodexAccountSummary {
    fn from(value: &CodexCredentials) -> Self {
        Self {
            email: value.email.clone(),
            account_id: value.account_id.clone(),
            plan_type: value.plan_type.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CodexUsageSnapshot {
    #[serde(skip)]
    pub account: Option<CodexAccountSummary>,
    #[serde(default)]
    pub plan_type: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<CodexRateLimit>,
    #[serde(default)]
    pub credits: Option<CodexCredits>,
    #[serde(default)]
    pub spend_control: Option<CodexSpendControl>,
    #[serde(default)]
    pub additional_rate_limits: Vec<CodexAdditionalRateLimit>,
    #[serde(default)]
    pub rate_limit_reached_type: Option<serde_json::Value>,
    #[serde(skip)]
    pub token_usage: Option<CodexTokenUsageStats>,
    #[serde(skip)]
    pub fetched_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CodexRateLimit {
    #[serde(default)]
    pub allowed: bool,
    #[serde(default)]
    pub limit_reached: bool,
    #[serde(default)]
    pub primary_window: Option<CodexRateLimitWindow>,
    #[serde(default)]
    pub secondary_window: Option<CodexRateLimitWindow>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CodexRateLimitWindow {
    pub used_percent: f64,
    pub limit_window_seconds: i64,
    pub reset_after_seconds: i64,
    pub reset_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CodexCredits {
    #[serde(default)]
    pub has_credits: bool,
    #[serde(default)]
    pub unlimited: bool,
    #[serde(default)]
    pub balance: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CodexSpendControl {
    #[serde(default)]
    pub reached: bool,
    #[serde(default)]
    pub individual_limit: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CodexAdditionalRateLimit {
    #[serde(default)]
    pub limit_name: Option<String>,
    #[serde(default)]
    pub metered_feature: Option<String>,
    #[serde(default)]
    pub rate_limit: Option<CodexRateLimit>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct CodexTokenUsageStats {
    #[serde(default)]
    pub lifetime_tokens: Option<u64>,
    #[serde(default)]
    pub peak_daily_tokens: Option<u64>,
    #[serde(default)]
    pub longest_running_turn_sec: Option<u64>,
    #[serde(default)]
    pub current_streak_days: Option<u64>,
    #[serde(default)]
    pub longest_streak_days: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenUsageProfile {
    stats: CodexTokenUsageStats,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    id_token: String,
    access_token: String,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct RefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct RefreshRequest<'a> {
    client_id: &'a str,
    grant_type: &'static str,
    refresh_token: &'a str,
}

#[derive(Debug, Clone)]
struct Pkce {
    code_verifier: String,
    code_challenge: String,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug)]
struct CallbackState {
    expected_state: String,
    result_tx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<Result<String, String>>>>,
}

#[derive(Debug, Deserialize)]
struct DeviceUserCodeResponse {
    device_auth_id: String,
    #[serde(alias = "user_code", alias = "usercode")]
    user_code: String,
    #[serde(deserialize_with = "deserialize_device_interval")]
    interval: u64,
}

fn deserialize_device_interval<'de, D>(deserializer: D) -> std::result::Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let value = String::deserialize(deserializer)?;
    value.trim().parse().map_err(D::Error::custom)
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    authorization_code: String,
    code_challenge: String,
    code_verifier: String,
}

#[derive(Debug, Serialize)]
struct DeviceUserCodeRequest<'a> {
    client_id: &'a str,
}

#[derive(Debug, Serialize)]
struct DeviceTokenRequest<'a> {
    device_auth_id: &'a str,
    user_code: &'a str,
}

pub fn auth_file_path() -> PathBuf {
    crate::util::grok_home::grok_home().join(CODEX_AUTH_FILE_NAME)
}

pub fn load_credentials() -> io::Result<Option<CodexCredentials>> {
    load_credentials_at(&auth_file_path())
}

pub fn is_logged_in() -> bool {
    load_credentials().ok().flatten().is_some()
}

fn load_store_at(path: &Path) -> io::Result<Option<CodexAuthStore>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if contents.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&contents)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn load_credentials_at(path: &Path) -> io::Result<Option<CodexCredentials>> {
    let Some(store) = load_store_at(path)? else {
        return Ok(None);
    };
    let Some(tokens) = store.tokens else {
        return Ok(None);
    };
    if tokens.access_token.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(credentials_from_tokens(&tokens)))
}

fn credentials_from_tokens(tokens: &CodexTokenData) -> CodexCredentials {
    let claims = decode_jwt_payload(&tokens.id_token).ok();
    let auth = claims
        .as_ref()
        .and_then(|claims| claims.get("https://api.openai.com/auth"));
    let profile = claims
        .as_ref()
        .and_then(|claims| claims.get("https://api.openai.com/profile"));
    let account_id = tokens.account_id.clone().or_else(|| {
        auth.and_then(|value| value.get("chatgpt_account_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    });
    let email = claims
        .as_ref()
        .and_then(|claims| claims.get("email"))
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            profile
                .and_then(|value| value.get("email"))
                .and_then(serde_json::Value::as_str)
        })
        .map(str::to_owned);
    let plan_type = auth
        .and_then(|value| value.get("chatgpt_plan_type"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let account_is_fedramp = auth
        .and_then(|value| value.get("chatgpt_account_is_fedramp"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    CodexCredentials {
        access_token: tokens.access_token.clone(),
        account_id,
        email,
        plan_type,
        account_is_fedramp,
    }
}

fn decode_jwt_payload(token: &str) -> Result<serde_json::Value> {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("invalid JWT format");
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        bail!("invalid JWT format");
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("invalid JWT payload encoding")?;
    serde_json::from_slice(&bytes).context("invalid JWT payload JSON")
}

fn jwt_expiration(token: &str) -> Option<DateTime<Utc>> {
    decode_jwt_payload(token)
        .ok()?
        .get("exp")?
        .as_i64()
        .and_then(|timestamp| DateTime::from_timestamp(timestamp, 0))
}

fn account_id_from_id_token(token: &str) -> Option<String> {
    decode_jwt_payload(token)
        .ok()?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_owned)
}

fn save_store_at(path: &Path, store: &CodexAuthStore) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let file = crate::util::secure_file::open_secure_file(&temp)?;
    let mut writer = io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, store)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|error| error.into_error())?
        .sync_all()?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn acquire_auth_lock(path: &Path) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_file_name(format!("{CODEX_AUTH_FILE_NAME}.lock"));
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options.open(lock_path)?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

fn generate_pkce() -> Pkce {
    let mut random_bytes = [0u8; 64];
    rand::rng().fill_bytes(&mut random_bytes);
    let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes);
    let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(code_verifier.as_bytes()));
    Pkce {
        code_verifier,
        code_challenge,
    }
}

fn generate_state() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn build_authorize_url(
    endpoints: &CodexEndpoints,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
) -> Result<String> {
    let mut url = url::Url::parse(&format!(
        "{}/oauth/authorize",
        endpoints.issuer.trim_end_matches('/')
    ))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &endpoints.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", CODEX_SCOPE)
        .append_pair("code_challenge", &pkce.code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", "codex_cli_rs");
    Ok(url.into())
}

async fn callback_handler(
    State(state): State<Arc<CallbackState>>,
    Query(query): Query<CallbackQuery>,
) -> Html<&'static str> {
    let result = if let Some(error) = query.error {
        Err(match query.error_description {
            Some(description) if !description.is_empty() => format!("{error}: {description}"),
            _ => error,
        })
    } else if query.state.as_deref() != Some(state.expected_state.as_str()) {
        Err("OAuth state mismatch".to_owned())
    } else {
        query
            .code
            .filter(|code| !code.trim().is_empty())
            .ok_or_else(|| "OAuth callback did not include a code".to_owned())
    };
    let success = result.is_ok();
    if let Some(sender) = state.result_tx.lock().await.take() {
        let _ = sender.send(result);
    }
    if success {
        Html(
            "<!doctype html><title>Grok Build connected</title><h1>OpenAI Codex connected</h1><p>You can close this window and return to Grok Build.</p>",
        )
    } else {
        Html(
            "<!doctype html><title>Grok Build login failed</title><h1>OpenAI Codex login failed</h1><p>Return to Grok Build for details.</p>",
        )
    }
}

async fn bind_callback_listener() -> io::Result<TcpListener> {
    match TcpListener::bind(("127.0.0.1", DEFAULT_CALLBACK_PORT)).await {
        Ok(listener) => Ok(listener),
        Err(primary) if primary.kind() == io::ErrorKind::AddrInUse => {
            TcpListener::bind(("127.0.0.1", FALLBACK_CALLBACK_PORT)).await
        }
        Err(error) => Err(error),
    }
}

async fn exchange_code(
    endpoints: &CodexEndpoints,
    code: &str,
    redirect_uri: &str,
    code_verifier: &str,
) -> Result<TokenResponse> {
    let response = reqwest::Client::new()
        .post(format!(
            "{}/oauth/token",
            endpoints.issuer.trim_end_matches('/')
        ))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", endpoints.client_id.as_str()),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await
        .context("Codex OAuth token exchange failed")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Codex OAuth token exchange returned {status}: {}",
            safe_error_excerpt(&body)
        );
    }
    response
        .json()
        .await
        .context("Codex OAuth token response was invalid")
}

fn persist_token_response(path: &Path, response: TokenResponse) -> Result<CodexCredentials> {
    decode_jwt_payload(&response.id_token).context("Codex ID token was invalid")?;
    let account_id = account_id_from_id_token(&response.id_token);
    let tokens = CodexTokenData {
        id_token: response.id_token,
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        account_id,
    };
    let credentials = credentials_from_tokens(&tokens);
    save_store_at(
        path,
        &CodexAuthStore {
            auth_mode: Some("chatgpt".to_owned()),
            openai_api_key: None,
            tokens: Some(tokens),
            last_refresh: Some(Utc::now()),
        },
    )?;
    Ok(credentials)
}

pub async fn run_cli_login(device_code: bool) -> Result<CodexAccountSummary> {
    let endpoints = CodexEndpoints::default();
    let path = auth_file_path();
    let credentials = if device_code {
        run_device_login_at(&path, &endpoints).await?
    } else {
        run_browser_login_at(&path, &endpoints).await?
    };
    Ok(CodexAccountSummary::from(&credentials))
}

async fn run_browser_login_at(path: &Path, endpoints: &CodexEndpoints) -> Result<CodexCredentials> {
    let listener = bind_callback_listener()
        .await
        .context("could not bind Codex OAuth callback on ports 1455 or 1457")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let pkce = generate_pkce();
    let expected_state = generate_state();
    let auth_url = build_authorize_url(endpoints, &redirect_uri, &pkce, &expected_state)?;
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let state = Arc::new(CallbackState {
        expected_state,
        result_tx: tokio::sync::Mutex::new(Some(result_tx)),
    });
    let app = Router::new()
        .route("/auth/callback", get(callback_handler))
        .with_state(state);
    let server = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::debug!(%error, "Codex OAuth callback server stopped");
        }
    });

    eprintln!();
    eprintln!("Signing in to OpenAI Codex with ChatGPT...");
    eprintln!("Open this URL if your browser does not open automatically:");
    eprintln!("  {auth_url}");
    let open_url = auth_url.clone();
    tokio::task::spawn_blocking(move || webbrowser::open(&open_url));

    let callback = tokio::time::timeout(CALLBACK_TIMEOUT, result_rx)
        .await
        .context("timed out waiting for the Codex OAuth callback")?
        .context("Codex OAuth callback server stopped")?;
    server.abort();
    let code = callback.map_err(|error| anyhow!(error))?;
    let response = exchange_code(endpoints, &code, &redirect_uri, &pkce.code_verifier).await?;
    let _lock = acquire_auth_lock(path)?;
    persist_token_response(path, response)
}

async fn run_device_login_at(path: &Path, endpoints: &CodexEndpoints) -> Result<CodexCredentials> {
    let client = reqwest::Client::new();
    let issuer = endpoints.issuer.trim_end_matches('/');
    let response = client
        .post(format!("{issuer}/api/accounts/deviceauth/usercode"))
        .json(&DeviceUserCodeRequest {
            client_id: &endpoints.client_id,
        })
        .send()
        .await
        .context("Codex device-code request failed")?;
    if !response.status().is_success() {
        bail!("Codex device-code request returned {}", response.status());
    }
    let device: DeviceUserCodeResponse = response
        .json()
        .await
        .context("Codex device-code response was invalid")?;
    let verification_url = format!("{issuer}/codex/device");
    eprintln!();
    eprintln!("Open this URL and sign in to ChatGPT:");
    eprintln!("  {verification_url}");
    eprintln!("Then enter this one-time code (expires in 15 minutes):");
    eprintln!("  {}", device.user_code);
    eprintln!("Continue only if you started this login in Grok Build.");
    let open_url = verification_url.clone();
    tokio::task::spawn_blocking(move || webbrowser::open(&open_url));

    let started = tokio::time::Instant::now();
    let code_response = loop {
        let response = client
            .post(format!("{issuer}/api/accounts/deviceauth/token"))
            .json(&DeviceTokenRequest {
                device_auth_id: &device.device_auth_id,
                user_code: &device.user_code,
            })
            .send()
            .await
            .context("Codex device-code poll failed")?;
        if response.status().is_success() {
            break response
                .json::<DeviceCodeResponse>()
                .await
                .context("Codex device-code token response was invalid")?;
        }
        if !matches!(response.status().as_u16(), 403 | 404) {
            bail!("Codex device-code login returned {}", response.status());
        }
        if started.elapsed() >= DEVICE_TIMEOUT {
            bail!("Codex device-code login timed out after 15 minutes");
        }
        tokio::time::sleep(Duration::from_secs(device.interval.max(1))).await;
    };
    let redirect_uri = format!("{issuer}/deviceauth/callback");
    let response = exchange_code(
        endpoints,
        &code_response.authorization_code,
        &redirect_uri,
        &code_response.code_verifier,
    )
    .await?;
    // The server supplies the verifier and challenge as a matched pair. Keep
    // the challenge parsed to pin the upstream response contract.
    if code_response.code_challenge.trim().is_empty() {
        bail!("Codex device-code response omitted its PKCE challenge");
    }
    let _lock = acquire_auth_lock(path)?;
    persist_token_response(path, response)
}

fn access_token_is_fresh(store: &CodexAuthStore) -> bool {
    let Some(tokens) = store.tokens.as_ref() else {
        return false;
    };
    if let Some(expires_at) = jwt_expiration(&tokens.access_token) {
        return expires_at.timestamp() > Utc::now().timestamp() + REFRESH_WINDOW_SECS;
    }
    store.last_refresh.is_some_and(|last_refresh| {
        Utc::now().signed_duration_since(last_refresh).num_days() < UNKNOWN_EXPIRY_REFRESH_DAYS
    })
}

pub async fn fresh_credentials() -> Result<Option<CodexCredentials>> {
    refresh_at(&auth_file_path(), &CodexEndpoints::default(), false).await
}

async fn force_refresh() -> Result<Option<CodexCredentials>> {
    refresh_at(&auth_file_path(), &CodexEndpoints::default(), true).await
}

async fn refresh_at(
    path: &Path,
    endpoints: &CodexEndpoints,
    force: bool,
) -> Result<Option<CodexCredentials>> {
    let Some(initial) = load_store_at(path)? else {
        return Ok(None);
    };
    if !force && access_token_is_fresh(&initial) {
        return load_credentials_at(path).map_err(Into::into);
    }
    let path_owned = path.to_path_buf();
    let _lock = tokio::task::spawn_blocking(move || acquire_auth_lock(&path_owned)).await??;
    let Some(mut store) = load_store_at(path)? else {
        return Ok(None);
    };
    if !force && access_token_is_fresh(&store) {
        return load_credentials_at(path).map_err(Into::into);
    }
    let Some(tokens) = store.tokens.as_mut() else {
        return Ok(None);
    };
    if tokens.refresh_token.trim().is_empty() {
        bail!("Codex OAuth refresh token is missing; run `grok login --codex`");
    }
    let prior_account_id = tokens
        .account_id
        .clone()
        .or_else(|| account_id_from_id_token(&tokens.id_token));
    let response = reqwest::Client::new()
        .post(format!(
            "{}/oauth/token",
            endpoints.issuer.trim_end_matches('/')
        ))
        .json(&RefreshRequest {
            client_id: &endpoints.client_id,
            grant_type: "refresh_token",
            refresh_token: &tokens.refresh_token,
        })
        .send()
        .await
        .context("Codex OAuth refresh request failed")?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let code = refresh_error_code(&body);
        let permanent = status == reqwest::StatusCode::UNAUTHORIZED
            || matches!(
                code.as_deref(),
                Some(
                    "refresh_token_expired" | "refresh_token_reused" | "refresh_token_invalidated"
                )
            );
        let action = if permanent {
            " Run `grok login --codex` to reconnect."
        } else {
            ""
        };
        bail!(
            "Codex OAuth refresh returned {status}{}.{action}",
            code.as_deref()
                .map(|code| format!(" ({code})"))
                .unwrap_or_default()
        );
    }
    let refreshed: RefreshResponse = response
        .json()
        .await
        .context("Codex OAuth refresh response was invalid")?;
    if let Some(ref id_token) = refreshed.id_token {
        decode_jwt_payload(id_token).context("refreshed Codex ID token was invalid")?;
        let new_account_id = account_id_from_id_token(id_token);
        if prior_account_id.is_some()
            && new_account_id.is_some()
            && prior_account_id != new_account_id
        {
            bail!("Codex OAuth refresh returned credentials for a different account");
        }
        tokens.id_token.clone_from(id_token);
        tokens.account_id = new_account_id.or(prior_account_id);
    }
    if let Some(access_token) = refreshed.access_token {
        tokens.access_token = access_token;
    }
    if let Some(refresh_token) = refreshed.refresh_token {
        tokens.refresh_token = refresh_token;
    }
    store.last_refresh = Some(Utc::now());
    save_store_at(path, &store)?;
    load_credentials_at(path).map_err(Into::into)
}

fn refresh_error_code(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(|error| match error {
            serde_json::Value::String(code) => Some(code.as_str()),
            serde_json::Value::Object(object) => object.get("code")?.as_str(),
            _ => None,
        })
        .or_else(|| value.get("code").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}

pub async fn run_cli_logout() -> Result<bool> {
    let path = auth_file_path();
    let Some(store) = load_store_at(&path)? else {
        return Ok(false);
    };
    let endpoints = CodexEndpoints::default();
    if let Some(tokens) = store.tokens.as_ref() {
        let (token, hint, include_client_id) = if !tokens.refresh_token.trim().is_empty() {
            (&tokens.refresh_token, "refresh_token", true)
        } else {
            (&tokens.access_token, "access_token", false)
        };
        let mut body = serde_json::json!({
            "token": token,
            "token_type_hint": hint,
        });
        if include_client_id {
            body["client_id"] = serde_json::Value::String(endpoints.client_id.clone());
        }
        if let Err(error) = reqwest::Client::new()
            .post(format!(
                "{}/oauth/revoke",
                endpoints.issuer.trim_end_matches('/')
            ))
            .json(&body)
            .send()
            .await
        {
            tracing::debug!(%error, "Codex token revocation failed; removing local credentials");
        }
    }
    let _lock = acquire_auth_lock(&path)?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub async fn fetch_usage() -> Result<CodexUsageSnapshot> {
    let endpoints = CodexEndpoints::default();
    let Some(mut credentials) = fresh_credentials().await? else {
        bail!("Not connected; run `grok login --codex`");
    };
    match fetch_usage_with_credentials(&endpoints, &credentials).await {
        Ok(snapshot) => Ok(snapshot),
        Err(UsageRequestError::Unauthorized) => {
            credentials = force_refresh()
                .await?
                .ok_or_else(|| anyhow!("Not connected; run `grok login --codex`"))?;
            fetch_usage_with_credentials(&endpoints, &credentials)
                .await
                .map_err(UsageRequestError::into_anyhow)
        }
        Err(error) => Err(error.into_anyhow()),
    }
}

#[derive(Debug)]
enum UsageRequestError {
    Unauthorized,
    Other(anyhow::Error),
}

impl UsageRequestError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Unauthorized => anyhow!("OpenAI Codex rejected the OAuth token"),
            Self::Other(error) => error,
        }
    }
}

async fn fetch_usage_with_credentials(
    endpoints: &CodexEndpoints,
    credentials: &CodexCredentials,
) -> std::result::Result<CodexUsageSnapshot, UsageRequestError> {
    let client = reqwest::Client::new();
    let base = endpoints.backend_base_url.trim_end_matches('/');
    let usage_request = apply_request_auth(client.get(format!("{base}/wham/usage")), credentials);
    let profile_request =
        apply_request_auth(client.get(format!("{base}/wham/profiles/me")), credentials);
    let (usage_response, profile_response) =
        tokio::join!(usage_request.send(), profile_request.send());
    let usage_response = usage_response.map_err(|error| {
        UsageRequestError::Other(anyhow!(error).context("Codex usage request failed"))
    })?;
    if usage_response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(UsageRequestError::Unauthorized);
    }
    let status = usage_response.status();
    if !status.is_success() {
        let body = usage_response.text().await.unwrap_or_default();
        return Err(UsageRequestError::Other(anyhow!(
            "Codex usage request returned {status}: {}",
            safe_error_excerpt(&body)
        )));
    }
    let mut snapshot: CodexUsageSnapshot = usage_response.json().await.map_err(|error| {
        UsageRequestError::Other(anyhow!(error).context("Codex usage response was invalid"))
    })?;
    snapshot.account = Some(CodexAccountSummary::from(credentials));
    snapshot.fetched_at = Utc::now();
    snapshot.token_usage = match profile_response {
        Ok(response) if response.status().is_success() => response
            .json::<TokenUsageProfile>()
            .await
            .ok()
            .map(|profile| profile.stats),
        _ => None,
    };
    Ok(snapshot)
}

fn apply_request_auth(
    mut request: reqwest::RequestBuilder,
    credentials: &CodexCredentials,
) -> reqwest::RequestBuilder {
    request = request.bearer_auth(&credentials.access_token);
    if let Some(account_id) = credentials.account_id.as_deref() {
        request = request.header("ChatGPT-Account-ID", account_id);
    }
    if credentials.account_is_fedramp {
        request = request.header("X-OpenAI-Fedramp", "true");
    }
    request
}

fn safe_error_excerpt(body: &str) -> String {
    const LIMIT: usize = 512;
    let mut excerpt: String = body.chars().take(LIMIT).collect();
    if body.chars().count() > LIMIT {
        excerpt.push('…');
    }
    excerpt.replace(['\n', '\r'], " ")
}

/// Starts one process-wide refresh loop. The immediate pass makes a cached
/// Codex token fresh before the first model request; later passes keep the
/// live bearer resolver current for long-running sessions.
pub fn start_proactive_refresh(cancel: tokio_util::sync::CancellationToken) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(error) = fresh_credentials().await {
                tracing::debug!(%error, "Codex proactive OAuth refresh skipped");
            }
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(Duration::from_secs(4 * 60)) => {}
            }
        }
    });
}

/// Per-request sync resolver used by the sampler. Refresh happens in the
/// isolated proactive loop; this read observes credential rotation performed
/// by this or another Grok Build process.
#[derive(Debug, Default)]
pub struct CodexBearerResolver;

impl xai_grok_sampler::BearerResolver for CodexBearerResolver {
    fn current_bearer(&self) -> Option<String> {
        load_credentials()
            .ok()
            .flatten()
            .map(|value| value.access_token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{}");
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{payload}.signature")
    }

    fn endpoints(issuer: &str) -> CodexEndpoints {
        CodexEndpoints {
            issuer: issuer.to_owned(),
            backend_base_url: issuer.to_owned(),
            client_id: CODEX_CLIENT_ID.to_owned(),
        }
    }

    #[test]
    fn authorize_url_matches_codex_rust_contract() {
        let pkce = Pkce {
            code_verifier: "verifier".to_owned(),
            code_challenge: "challenge".to_owned(),
        };
        let url = build_authorize_url(
            &endpoints(CODEX_ISSUER),
            "http://localhost:1455/auth/callback",
            &pkce,
            "state",
        )
        .unwrap();
        let url = url::Url::parse(&url).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.path(), "/oauth/authorize");
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some(CODEX_CLIENT_ID)
        );
        assert_eq!(query.get("scope").map(String::as_str), Some(CODEX_SCOPE));
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(
            query.get("id_token_add_organizations").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            query.get("codex_cli_simplified_flow").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            query.get("originator").map(String::as_str),
            Some("codex_cli_rs")
        );
    }

    #[test]
    fn credentials_read_openai_claim_namespace() {
        let id_token = jwt(serde_json::json!({
            "email": "dev@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "pro",
                "chatgpt_account_id": "account-1",
                "chatgpt_account_is_fedramp": true
            }
        }));
        let credentials = credentials_from_tokens(&CodexTokenData {
            id_token,
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            account_id: None,
        });
        assert_eq!(credentials.email.as_deref(), Some("dev@example.com"));
        assert_eq!(credentials.plan_type.as_deref(), Some("pro"));
        assert_eq!(credentials.account_id.as_deref(), Some("account-1"));
        assert!(credentials.account_is_fedramp);
    }

    #[test]
    fn storage_is_separate_and_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CODEX_AUTH_FILE_NAME);
        let store = CodexAuthStore {
            auth_mode: Some("chatgpt".to_owned()),
            openai_api_key: None,
            tokens: Some(CodexTokenData {
                id_token: jwt(serde_json::json!({})),
                access_token: "access".to_owned(),
                refresh_token: "refresh".to_owned(),
                account_id: None,
            }),
            last_refresh: Some(Utc::now()),
        };
        save_store_at(&path, &store).unwrap();
        assert_eq!(load_store_at(&path).unwrap(), Some(store));
        assert!(!dir.path().join("auth.json").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn refresh_error_code_accepts_codex_shapes() {
        assert_eq!(
            refresh_error_code(r#"{"error":{"code":"refresh_token_reused"}}"#).as_deref(),
            Some("refresh_token_reused")
        );
        assert_eq!(
            refresh_error_code(r#"{"error":"refresh_token_expired"}"#).as_deref(),
            Some("refresh_token_expired")
        );
    }

    #[test]
    fn usage_schema_accepts_multiple_windows_and_credits() {
        let snapshot: CodexUsageSnapshot = serde_json::from_value(serde_json::json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 25,
                    "limit_window_seconds": 18000,
                    "reset_after_seconds": 100,
                    "reset_at": 200
                },
                "secondary_window": {
                    "used_percent": 50,
                    "limit_window_seconds": 604800,
                    "reset_after_seconds": 300,
                    "reset_at": 400
                }
            },
            "credits": {"has_credits": true, "unlimited": false, "balance": "12.50"}
        }))
        .unwrap();
        assert_eq!(snapshot.plan_type.as_deref(), Some("pro"));
        assert_eq!(
            snapshot
                .rate_limit
                .unwrap()
                .secondary_window
                .unwrap()
                .used_percent,
            50.0
        );
        assert_eq!(
            snapshot.credits.unwrap().balance,
            Some(serde_json::json!("12.50"))
        );
    }
}
