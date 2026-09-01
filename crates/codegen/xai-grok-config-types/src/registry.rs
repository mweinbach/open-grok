use crate::{
    RemoteSettings,
    flags::{BoolFlag, ConfigSource, Resolved},
};
use xai_grok_config::env_bool;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, strum::EnumIter)]
pub enum Feature {
    SessionSearch,
    LspTools,
    WebFetch,
    SessionRecap,
    AskUserQuestion,
    VoiceMode,
    WriteFile,
    Feedback,
    FeedbackTraceCard,
    TurnSummary,
    CancelRewind,
    CompactionVerbatimInput,
    TwoPassCompaction,
    BackendTools,
    AutoWake,
    SubagentWorktreeSnapshot,
    ActiveAgentMessages,
    RepoStatusInSystemPrompt,
}
#[derive(Debug, Clone, Copy)]
pub struct FeatureSpec {
    pub id: Feature,
    pub key: &'static str,
    pub path: &'static str,
    pub env: &'static str,
    pub default_enabled: bool,
    pub remote: Option<fn(&RemoteSettings) -> Option<bool>>,
}
#[derive(Debug, Clone, Copy, Default)]
pub struct FeatureSources {
    pub pin: Option<bool>,
    pub env: Option<bool>,
    pub config: Option<bool>,
    pub remote: Option<bool>,
}
impl FeatureSources {
    pub fn from_process_env(feature: Feature) -> Self {
        Self {
            env: env_bool(feature.env()),
            ..Self::default()
        }
    }
}
pub const FEATURES: &[FeatureSpec] = &[
    FeatureSpec {
        id: Feature::SessionSearch,
        key: "session_search",
        path: "features.session_search",
        env: "GROK_SESSION_SEARCH",
        default_enabled: true,
        remote: Some(|settings| settings.session_search),
    },
    FeatureSpec {
        id: Feature::LspTools,
        key: "lsp_tools",
        path: "features.lsp_tools",
        env: "GROK_LSP_TOOLS",
        default_enabled: false,
        remote: Some(|settings| settings.lsp_tools_enabled),
    },
    FeatureSpec {
        id: Feature::WebFetch,
        key: "web_fetch",
        path: "features.web_fetch",
        env: "GROK_WEB_FETCH",
        default_enabled: false,
        remote: Some(|settings| settings.web_fetch_enabled),
    },
    FeatureSpec {
        id: Feature::SessionRecap,
        key: "session_recap",
        path: "features.session_recap",
        env: "GROK_SESSION_RECAP",
        default_enabled: true,
        remote: Some(|settings| settings.session_recap),
    },
    FeatureSpec {
        id: Feature::AskUserQuestion,
        key: "ask_user_question",
        path: "features.ask_user_question",
        env: "GROK_ASK_USER_QUESTION",
        default_enabled: true,
        remote: Some(|settings| settings.ask_user_question_enabled),
    },
    FeatureSpec {
        id: Feature::VoiceMode,
        key: "voice_mode",
        path: "features.voice_mode",
        env: "GROK_VOICE_MODE",
        default_enabled: true,
        remote: Some(|settings| settings.voice_mode_enabled),
    },
    FeatureSpec {
        id: Feature::WriteFile,
        key: "write_file",
        path: "features.write_file",
        env: "GROK_WRITE_FILE",
        default_enabled: true,
        remote: Some(|settings| settings.write_file_enabled),
    },
    FeatureSpec {
        id: Feature::Feedback,
        key: "feedback",
        path: "features.feedback",
        env: "GROK_FEEDBACK_ENABLED",
        default_enabled: true,
        remote: Some(|settings| settings.feedback_enabled),
    },
    FeatureSpec {
        id: Feature::FeedbackTraceCard,
        key: "feedback_trace_card",
        path: "features.feedback_trace_card",
        env: "GROK_FEEDBACK_TRACE_CARD",
        default_enabled: false,
        remote: Some(|settings| settings.feedback_trace_card_enabled),
    },
    FeatureSpec {
        id: Feature::TurnSummary,
        key: "turn_summary",
        path: "features.turn_summary",
        env: "GROK_TURN_SUMMARY",
        default_enabled: true,
        remote: Some(|settings| settings.turn_summary),
    },
    FeatureSpec {
        id: Feature::CancelRewind,
        key: "cancel_rewind",
        path: "features.cancel_rewind",
        env: "GROK_CANCEL_REWIND",
        default_enabled: true,
        remote: Some(|settings| settings.cancel_rewind_enabled),
    },
    FeatureSpec {
        id: Feature::CompactionVerbatimInput,
        key: "compaction_verbatim_input",
        path: "features.compaction_verbatim_input",
        env: "GROK_COMPACTION_VERBATIM_INPUT",
        default_enabled: true,
        remote: Some(|settings| settings.compaction_verbatim_input),
    },
    FeatureSpec {
        id: Feature::TwoPassCompaction,
        key: "two_pass_compaction",
        path: "features.two_pass_compaction",
        env: "GROK_TWO_PASS_COMPACTION",
        default_enabled: true,
        remote: Some(|settings| settings.two_pass_compaction_enabled),
    },
    FeatureSpec {
        id: Feature::BackendTools,
        key: "backend_tools",
        path: "features.backend_tools",
        env: "GROK_BACKEND_SEARCH",
        default_enabled: true,
        remote: None,
    },
    FeatureSpec {
        id: Feature::AutoWake,
        key: "auto_wake",
        path: "features.auto_wake",
        env: "GROK_AUTO_WAKE",
        default_enabled: true,
        remote: Some(|settings| settings.auto_wake_enabled),
    },
    FeatureSpec {
        id: Feature::SubagentWorktreeSnapshot,
        key: "subagent_worktree_snapshot",
        path: "features.subagent_worktree_snapshot",
        env: "GROK_SUBAGENT_WORKTREE_SNAPSHOT",
        default_enabled: false,
        remote: Some(|settings| settings.subagent_worktree_snapshot_enabled),
    },
    FeatureSpec {
        id: Feature::ActiveAgentMessages,
        key: "active_agent_messages",
        path: "features.active_agent_messages",
        env: "GROK_ACTIVE_AGENT_MESSAGES",
        default_enabled: false,
        remote: Some(|settings| settings.active_agent_messages_enabled),
    },
    FeatureSpec {
        id: Feature::RepoStatusInSystemPrompt,
        key: "repo_status_in_system_prompt",
        path: "features.repo_status_in_system_prompt",
        env: "GROK_REPO_STATUS_IN_SYSTEM_PROMPT",
        default_enabled: true,
        remote: Some(|settings| settings.repo_status_in_system_prompt),
    },
];
impl Feature {
    fn spec(self) -> &'static FeatureSpec {
        FEATURES
            .iter()
            .find(|spec| spec.id == self)
            .unwrap_or_else(|| unreachable!("missing FeatureSpec for {self:?}"))
    }
    pub fn key(self) -> &'static str {
        self.spec().key
    }
    pub fn env(self) -> &'static str {
        self.spec().env
    }
    pub fn path(self) -> &'static str {
        self.spec().path
    }
    pub fn remote_value(self, settings: Option<&RemoteSettings>) -> Option<bool> {
        let read = self.spec().remote?;
        read(settings?)
    }
    pub fn off_reason(self, sources: FeatureSources) -> Option<String> {
        let resolved = self.resolve(sources);
        if resolved.value {
            return None;
        }
        let spec = self.spec();
        Some(match resolved.source {
            ConfigSource::Requirement => "a requirements.toml pin or an MDM policy".to_owned(),
            ConfigSource::Env => format!("the {} environment variable", spec.env),
            ConfigSource::Config
            | ConfigSource::UserConfig
            | ConfigSource::ManagedConfig
            | ConfigSource::SystemManagedConfig
            | ConfigSource::EnvOverlay => {
                format!("the {} key in config.toml or managed_config.toml", spec.key)
            }
            ConfigSource::Remote => "a remote setting".to_owned(),
            ConfigSource::Default => "the default".to_owned(),
            ConfigSource::Cli => "a command line override".to_owned(),
        })
    }
    pub fn resolve(self, sources: FeatureSources) -> Resolved<bool> {
        let spec = self.spec();
        BoolFlag::env_value(sources.env)
            .requirement(sources.pin)
            .config(sources.config)
            .feature_flag(sources.remote)
            .default(spec.default_enabled)
            .resolve()
    }
}
#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
