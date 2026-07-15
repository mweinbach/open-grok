//! Hub [`AuthProvider`] from `~/.grok/auth.json` for the standalone
//! `workspace_server` binary: loopback `ws://` uses a plain bearer, otherwise
//! an auto-refreshing OIDC provider that persists rotated tokens to disk.
//!
//! The in-leader `grok workspace` exposure does NOT use this path — it sources
//! an in-memory provider from the leader's `AuthManager` (see
//! `LeaderAuthProvider`) to avoid racing the leader's own auth.json writer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use url::Url;
use xai_computer_hub_sdk::{
    AuthCredential, AuthIdentity, AuthProvider, OidcAuthProviderBuilder, RefreshEvent,
};

/// Plain bearer provider that also carries the owner identity parsed from the
/// same auth.json entry. Used for the loopback / local-dev path (no OIDC
/// refresh) so the workspace can still derive `WorkspaceIdentity` from the auth
/// provider — without a second auth.json read.
struct BearerWithIdentity {
    token: String,
    identity: AuthIdentity,
}

impl std::fmt::Debug for BearerWithIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log the bearer token; surface only the (non-secret) identity.
        f.debug_struct("BearerWithIdentity")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl AuthProvider for BearerWithIdentity {
    fn current(&self) -> AuthCredential {
        AuthCredential::bearer(self.token.clone())
    }

    fn identity(&self) -> Option<AuthIdentity> {
        Some(self.identity.clone())
    }
}

/// Owner identity parsed from an auth.json entry, for the [`AuthProvider`]s
/// built here to surface via [`AuthProvider::identity`].
fn identity_from_entry(entry: &AuthEntry) -> AuthIdentity {
    AuthIdentity {
        user_id: entry.user_id.clone(),
        principal_type: entry.principal_type.clone(),
        principal_id: entry.principal_id.clone(),
    }
}

#[derive(Debug, serde::Deserialize)]
struct AuthEntry {
    key: String,
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    oidc_issuer: Option<String>,
    #[serde(default)]
    oidc_client_id: Option<String>,
    #[serde(default)]
    principal_type: Option<String>,
    #[serde(default)]
    principal_id: Option<String>,
    #[serde(default)]
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn default_auth_path() -> anyhow::Result<PathBuf> {
    let grok = xai_grok_config::user_grok_home()
        .ok_or_else(|| anyhow::anyhow!("no user grok home (set $GROK_HOME or $HOME)"))?;
    Ok(grok.join("auth.json"))
}

/// Read the active OIDC entry and its scope key. The key is threaded to the
/// refresh write so rotation updates exactly the entry that was read.
fn read_auth_entry(path: &Path) -> anyhow::Result<(String, AuthEntry)> {
    if !path.exists() {
        anyhow::bail!(
            "No auth credentials found at {}. Run `grok login` first.",
            path.display()
        );
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", path.display()))?;
    let entries: BTreeMap<String, AuthEntry> = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;

    entries
        .into_iter()
        .find(|(_, e)| e.refresh_token.is_some() && e.oidc_issuer.is_some())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no OIDC auth entry found in {}. Run `grok login` first.",
                path.display()
            )
        })
}

fn build_oidc_provider(
    scope_key: String,
    entry: &AuthEntry,
    auth_path: PathBuf,
) -> anyhow::Result<Arc<dyn AuthProvider>> {
    let refresh_token = entry.refresh_token.as_ref().ok_or_else(|| {
        anyhow::anyhow!("auth entry has no refresh_token — cannot refresh expired tokens")
    })?;
    let issuer = entry.oidc_issuer.as_ref().ok_or_else(|| {
        anyhow::anyhow!("auth entry has no oidc_issuer — cannot refresh expired tokens")
    })?;
    let client_id = entry.oidc_client_id.as_ref().ok_or_else(|| {
        anyhow::anyhow!("auth entry has no oidc_client_id — cannot refresh expired tokens")
    })?;

    let mut builder = OidcAuthProviderBuilder::new(&entry.key, refresh_token, issuer, client_id);

    // Owner identity is surfaced via `AuthProvider::identity()` so the workspace
    // derives `WorkspaceIdentity` from this provider — no separate auth.json read.
    builder = builder.user_id(&entry.user_id);
    if let Some(ref pt) = entry.principal_type {
        builder = builder.principal_type(pt);
    }
    if let Some(ref pid) = entry.principal_id {
        builder = builder.principal_id(pid);
    }
    if let Some(exp) = entry.expires_at {
        builder = builder.expires_at(exp);
    }

    builder = builder.on_refresh(Arc::new(move |event: &RefreshEvent| {
        if let Err(e) = write_refreshed_token(&auth_path, &scope_key, event) {
            tracing::warn!(error = %e, "failed to persist refreshed token to auth.json");
        }
    }));

    Ok(Arc::new(builder.build()))
}

fn write_refreshed_token(path: &Path, scope_key: &str, event: &RefreshEvent) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)?;
    let mut raw: serde_json::Value = serde_json::from_str(&content)?;

    let Some(obj) = raw.get_mut(scope_key).and_then(|e| e.as_object_mut()) else {
        anyhow::bail!("auth entry '{scope_key}' not found while persisting refreshed token");
    };
    obj.insert(
        "key".to_owned(),
        serde_json::Value::String(event.access_token.clone()),
    );
    if let Some(ref rt) = event.new_refresh_token {
        obj.insert(
            "refresh_token".to_owned(),
            serde_json::Value::String(rt.clone()),
        );
    }
    if let Some(exp) = event.expires_at {
        obj.insert(
            "expires_at".to_owned(),
            serde_json::Value::String(exp.to_rfc3339()),
        );
    }

    write_json_atomic(path, &raw)?;
    tracing::info!(path = %path.display(), "persisted refreshed token to auth.json");
    Ok(())
}

/// Atomically replace `path`: temp file (0600 on Unix) + fsync + rename. Avoids
/// the truncate-in-place corruption window when the long-lived binary rewrites
/// auth.json.
fn write_json_atomic(path: &Path, value: &serde_json::Value) -> anyhow::Result<()> {
    use std::io::Write;

    let json = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }

    let mut file = opts
        .open(&tmp)
        .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", tmp.display()))?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    let _ = std::fs::remove_file(path);

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow::anyhow!("failed to replace {}: {e}", path.display()));
    }
    Ok(())
}

/// Build a hub auth provider for `hub_url`. `auth_config` overrides
/// the default credential path (`~/.grok/auth.json`).
pub fn provider(
    hub_url: &Url,
    auth_config: Option<&Path>,
) -> anyhow::Result<Arc<dyn AuthProvider>> {
    let auth_path = match auth_config {
        Some(p) => p.to_path_buf(),
        None => default_auth_path()?,
    };
    let (scope_key, entry) = read_auth_entry(&auth_path)?;

    let is_loopback = hub_url.scheme() == "ws"
        && matches!(hub_url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));

    if is_loopback {
        tracing::info!("Using local-dev auth (loopback hub)");
        Ok(Arc::new(BearerWithIdentity {
            identity: identity_from_entry(&entry),
            token: entry.key.clone(),
        }))
    } else {
        build_oidc_provider(scope_key, &entry, auth_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_auth_json(dir: &std::path::Path, json: &str) -> PathBuf {
        let path = dir.join("auth.json");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(json.as_bytes()).unwrap();
        path
    }

    #[test]
    fn read_auth_entry_picks_oidc_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "legacy": { "key": "xai-plainkey", "user_id": "u1" },
            "oidc": {
                "key": "eyJhbGciOiJFUzI1NiJ9.test",
                "user_id": "u2",
                "refresh_token": "rt",
                "oidc_issuer": "https://auth.example.com",
                "oidc_client_id": "client1"
            }
        }"#,
        );

        let (key, entry) = read_auth_entry(&path).unwrap();
        assert_eq!(key, "oidc");
        assert_eq!(entry.refresh_token.as_deref(), Some("rt"));
        assert_eq!(
            entry.oidc_issuer.as_deref(),
            Some("https://auth.example.com")
        );
    }

    #[test]
    fn read_auth_entry_rejects_non_oidc() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "api_key": { "key": "xai-plainkey", "user_id": "u1" }
        }"#,
        );

        let err = read_auth_entry(&path).unwrap_err();
        assert!(err.to_string().contains("no OIDC auth entry"));
    }

    #[test]
    fn read_auth_entry_missing_file() {
        let path = PathBuf::from("/nonexistent/auth.json");
        let err = read_auth_entry(&path).unwrap_err();
        assert!(err.to_string().contains("No auth credentials"));
    }

    #[test]
    fn read_auth_entry_tolerates_extra_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "scope": {
                "key": "eyJhbGciOiJFUzI1NiJ9.tok",
                "user_id": "u1",
                "auth_mode": "oidc",
                "create_time": "2026-01-01T00:00:00Z",
                "email": "test@x.ai",
                "first_name": "Test",
                "refresh_token": "rt1",
                "oidc_issuer": "https://auth.x.ai",
                "oidc_client_id": "c1",
                "some_future_field": true
            }
        }"#,
        );

        let (_key, entry) = read_auth_entry(&path).unwrap();
        assert_eq!(entry.refresh_token.as_deref(), Some("rt1"));
    }

    #[test]
    fn build_oidc_provider_requires_refresh_token() {
        let entry = AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: None,
            oidc_issuer: Some("https://auth.x.ai".into()),
            oidc_client_id: Some("c1".into()),
            principal_type: None,
            principal_id: None,
            expires_at: None,
        };
        let err = build_oidc_provider("oidc".into(), &entry, PathBuf::from("/tmp/x")).unwrap_err();
        assert!(err.to_string().contains("refresh_token"));
    }

    #[test]
    fn build_oidc_provider_requires_issuer() {
        let entry = AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: Some("rt".into()),
            oidc_issuer: None,
            oidc_client_id: Some("c1".into()),
            principal_type: None,
            principal_id: None,
            expires_at: None,
        };
        let err = build_oidc_provider("oidc".into(), &entry, PathBuf::from("/tmp/x")).unwrap_err();
        assert!(err.to_string().contains("oidc_issuer"));
    }

    #[test]
    fn build_oidc_provider_requires_client_id() {
        let entry = AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: Some("rt".into()),
            oidc_issuer: Some("https://auth.x.ai".into()),
            oidc_client_id: None,
            principal_type: None,
            principal_id: None,
            expires_at: None,
        };
        let err = build_oidc_provider("oidc".into(), &entry, PathBuf::from("/tmp/x")).unwrap_err();
        assert!(err.to_string().contains("oidc_client_id"));
    }

    #[test]
    fn build_oidc_provider_succeeds_with_all_fields() {
        let entry = AuthEntry {
            key: "eyJ.tok".into(),
            user_id: "u1".into(),
            refresh_token: Some("rt".into()),
            oidc_issuer: Some("https://auth.x.ai".into()),
            oidc_client_id: Some("c1".into()),
            principal_type: Some("Team".into()),
            principal_id: Some("t1".into()),
            expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
        };
        let provider = build_oidc_provider("oidc".into(), &entry, PathBuf::from("/tmp/x")).unwrap();
        let cred = provider.current();
        match cred {
            xai_computer_hub_sdk::AuthCredential::Bearer { token } => {
                assert_eq!(token, "eyJ.tok");
            }
            _ => panic!("expected Bearer"),
        }
        // Identity is surfaced from the parsed entry (no second auth.json read).
        let id = provider.identity().expect("identity present");
        assert_eq!(id.user_id, "u1");
        assert_eq!(id.principal_type.as_deref(), Some("Team"));
        assert_eq!(id.principal_id.as_deref(), Some("t1"));
    }

    #[test]
    fn write_refreshed_token_updates_jwt_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "legacy": { "key": "xai-old", "user_id": "u1" },
            "oidc": { "key": "eyJ.old", "user_id": "u2", "refresh_token": "rt-old", "oidc_issuer": "https://auth.x.ai" }
        }"#,
        );

        let event = RefreshEvent {
            access_token: "eyJ.new".into(),
            new_refresh_token: Some("rt-new".into()),
            expires_at: None,
        };
        write_refreshed_token(&path, "oidc", &event).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(updated["oidc"]["key"], "eyJ.new");
        assert_eq!(updated["oidc"]["refresh_token"], "rt-new");
        assert_eq!(updated["legacy"]["key"], "xai-old"); // untouched
    }

    #[test]
    fn write_refreshed_token_targets_exact_scope_key() {
        // Non-sorted order: refresh must update the read-selected key ("aaa"),
        // not the first in file order ("zzz").
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "zzz": { "key": "eyJ.z", "refresh_token": "rt-z", "oidc_issuer": "https://auth.x.ai" },
            "aaa": { "key": "eyJ.a", "refresh_token": "rt-a", "oidc_issuer": "https://auth.x.ai" }
        }"#,
        );

        let (key, _entry) = read_auth_entry(&path).unwrap();
        assert_eq!(key, "aaa");

        let event = RefreshEvent {
            access_token: "eyJ.a-new".into(),
            new_refresh_token: Some("rt-a-new".into()),
            expires_at: None,
        };
        write_refreshed_token(&path, &key, &event).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(updated["aaa"]["key"], "eyJ.a-new");
        assert_eq!(updated["aaa"]["refresh_token"], "rt-a-new");
        assert_eq!(updated["zzz"]["key"], "eyJ.z");
        assert_eq!(updated["zzz"]["refresh_token"], "rt-z");
    }

    #[test]
    fn write_refreshed_token_preserves_existing_rt_when_not_rotated() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{
            "oidc": { "key": "eyJ.old", "user_id": "u1", "refresh_token": "rt-keep", "oidc_issuer": "https://auth.x.ai" }
        }"#,
        );

        let event = RefreshEvent {
            access_token: "eyJ.new".into(),
            new_refresh_token: None,
            expires_at: None,
        };
        write_refreshed_token(&path, "oidc", &event).unwrap();

        let updated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(updated["oidc"]["key"], "eyJ.new");
        assert_eq!(updated["oidc"]["refresh_token"], "rt-keep");
    }

    #[test]
    fn provider_loopback_uses_bearer() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_auth_json(
            dir.path(),
            r#"{ "oidc": { "key": "eyJ.tok", "user_id": "u1", "refresh_token": "rt", "oidc_issuer": "https://auth.x.ai", "oidc_client_id": "c1" } }"#,
        );
        let url = Url::parse("ws://localhost:9988/v1/tools").unwrap();
        let auth = provider(&url, Some(&path)).unwrap();
        match auth.current() {
            AuthCredential::Bearer { token } => assert_eq!(token, "eyJ.tok"),
            _ => panic!("expected Bearer"),
        }
        // Loopback still surfaces identity from the same entry.
        let id = auth.identity().expect("loopback identity present");
        assert_eq!(id.user_id, "u1");
    }
}
