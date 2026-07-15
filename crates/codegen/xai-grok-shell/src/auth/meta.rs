use serde::{Deserialize, Serialize};

/// Access gate from `grok_build_access_gate`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateInfo {
    pub message: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// Typed auth metadata passed from the shell to the pager via ACP.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthMeta {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub auth_mode: Option<String>,
    /// Team principal UUID when the session is a team login (`None` for personal).
    #[serde(default)]
    pub team_id: Option<String>,
    #[serde(default)]
    pub team_name: Option<String>,
    #[serde(default)]
    pub is_zdr: bool,
    #[serde(default)]
    pub team_role: Option<String>,
    #[serde(default)]
    pub coding_data_retention_opt_out: bool,
    #[serde(default)]
    pub show_resolved_model: Option<bool>,
    /// `Some` = user is blocked; `None` = user has access.
    #[serde(default)]
    pub gate: Option<GateInfo>,
    /// User-friendly display name for the current subscription tier
    /// (e.g. "SuperGrok Heavy", "X Premium", "Free"). From CCP `/settings`.
    #[serde(default)]
    pub subscription_tier: Option<String>,
}
