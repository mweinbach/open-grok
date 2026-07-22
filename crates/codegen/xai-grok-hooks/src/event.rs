use serde::Serialize;

/// Maximum serialized size for `toolInput` or `toolResult` in bytes (128 KB).
pub const MAX_PAYLOAD_SIZE: usize = 128 * 1024;

/// Hook event types.
///
/// Deserialization accepts PascalCase, snake_case, camelCase, and per-operation
/// aliases (e.g. `beforeShellExecution` maps to `PreToolUse`); see the `Deserialize` impl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventName {
    SessionStart,
    SessionEnd,
    /// Fires on a genuine turn-end with stop decision control (a hook can block);
    /// not on user interrupts (API-error turns fire `StopFailure`); observe-only at session end.
    Stop,
    /// Fires when the turn ends due to an API error. Output and exit code are ignored.
    StopFailure,

    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionDenied,

    UserPromptSubmit,
    Notification,

    SubagentStart,
    SubagentStop,
    SubagentEnd,

    PreCompact,
    PostCompact,
}

impl<'de> serde::Deserialize<'de> for HookEventName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            // PascalCase (native) + snake_case + camelCase (third-party compat).
            // Per-operation hook names (beforeShellExecution, afterFileEdit, etc.)
            // map to our generic PreToolUse/PostToolUse; the hook script receives the
            // tool name in JSON input and can filter, or use the `matcher` field.
            "SessionStart" | "session_start" | "sessionStart" => Ok(Self::SessionStart),
            "PreToolUse"
            | "pre_tool_use"
            | "preToolUse"
            | "beforeShellExecution"
            | "beforeMCPExecution"
            | "beforeReadFile" => Ok(Self::PreToolUse),
            "PostToolUse"
            | "post_tool_use"
            | "postToolUse"
            | "afterShellExecution"
            | "afterMCPExecution"
            | "afterFileEdit"
            | "afterAgentResponse"
            | "afterAgentThought" => Ok(Self::PostToolUse),
            "PostToolUseFailure" | "post_tool_use_failure" | "postToolUseFailure" => {
                Ok(Self::PostToolUseFailure)
            }
            "SessionEnd" | "session_end" | "sessionEnd" => Ok(Self::SessionEnd),
            "Stop" | "stop" => Ok(Self::Stop),
            "StopFailure" | "stop_failure" | "stopFailure" => Ok(Self::StopFailure),
            "Notification" | "notification" => Ok(Self::Notification),
            "UserPromptSubmit" | "user_prompt_submit" | "beforeSubmitPrompt" => {
                Ok(Self::UserPromptSubmit)
            }
            "PermissionDenied" | "permission_denied" | "permissionDenied" => {
                Ok(Self::PermissionDenied)
            }
            "SubagentStart" | "subagent_start" | "subagentStart" => Ok(Self::SubagentStart),
            "SubagentStop" | "subagent_stop" | "subagentStop" => Ok(Self::SubagentStop),
            "SubagentEnd" | "subagent_end" | "subagentEnd" => Ok(Self::SubagentEnd),
            "PreCompact" | "pre_compact" | "preCompact" => Ok(Self::PreCompact),
            "PostCompact" | "post_compact" | "postCompact" => Ok(Self::PostCompact),
            other => Err(serde::de::Error::custom(format!(
                "unknown hook event: '{other}'. Expected one of: \
                 SessionStart, PreToolUse, PostToolUse, PostToolUseFailure, \
                 SessionEnd, Stop, StopFailure, Notification, UserPromptSubmit, \
                 PermissionDenied, SubagentStart, SubagentStop, \
                 PreCompact, PostCompact (camelCase and per-operation aliases \
                 such as beforeShellExecution are also accepted)"
            ))),
        }
    }
}

impl std::fmt::Display for HookEventName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionStart => write!(f, "session_start"),
            Self::PreToolUse => write!(f, "pre_tool_use"),
            Self::PostToolUse => write!(f, "post_tool_use"),
            Self::PostToolUseFailure => write!(f, "post_tool_use_failure"),
            Self::SessionEnd => write!(f, "session_end"),
            Self::Stop => write!(f, "stop"),
            Self::StopFailure => write!(f, "stop_failure"),
            Self::Notification => write!(f, "notification"),
            Self::UserPromptSubmit => write!(f, "user_prompt_submit"),
            Self::PermissionDenied => write!(f, "permission_denied"),
            Self::SubagentStart => write!(f, "subagent_start"),
            Self::SubagentStop | Self::SubagentEnd => write!(f, "subagent_stop"),
            Self::PreCompact => write!(f, "pre_compact"),
            Self::PostCompact => write!(f, "post_compact"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    /// Hook output recorded, decisions ignored.
    Observe,
    Tool,
    /// Stop decision control (`block`, `continue: false`, `additionalContext`).
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatcherPolicy {
    /// Never evaluated: kept for display with a load-time warning, the hook fires on every occurrence.
    Ignored,
    /// Tested against the value [`HookPayload::match_value`] extracts from the payload.
    Tested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventTraits {
    pub gate: GateKind,
    pub matcher: MatcherPolicy,
    /// Whether hub custom hooks receive this event (see `dispatcher::hub_hook_kind`).
    pub hub_forward: bool,
}

impl HookEventName {
    /// Collapse alias variants to their canonical form so a registration and the fired
    /// event meet on one key regardless of which spelling each used (`SubagentEnd` is an
    /// alias of `SubagentStop`).
    pub fn canonical(self) -> Self {
        match self {
            Self::SubagentEnd => Self::SubagentStop,
            other => other,
        }
    }

    /// The event's dispatch traits. Exhaustive on purpose: a new variant fails to
    /// compile until its gate, matcher, and hub forwarding are chosen here.
    pub fn traits(self) -> EventTraits {
        use GateKind::*;
        use MatcherPolicy::*;
        let t = |gate, matcher, hub_forward| EventTraits {
            gate,
            matcher,
            hub_forward,
        };
        match self.canonical() {
            Self::SessionStart => t(Observe, Tested, true),
            Self::SessionEnd => t(Observe, Tested, true),
            Self::Stop => t(Stop, Ignored, true),
            Self::StopFailure => t(Observe, Tested, true),
            Self::PreToolUse => t(Tool, Tested, false),
            Self::PostToolUse => t(Observe, Tested, true),
            Self::PostToolUseFailure => t(Observe, Tested, true),
            Self::PermissionDenied => t(Observe, Tested, true),
            Self::UserPromptSubmit => t(Observe, Ignored, true),
            Self::Notification => t(Observe, Tested, true),
            Self::SubagentStart => t(Observe, Tested, true),
            Self::SubagentStop => t(Stop, Tested, true),
            Self::SubagentEnd => unreachable!("canonicalized above"),
            Self::PreCompact => t(Observe, Tested, true),
            Self::PostCompact => t(Observe, Tested, true),
        }
    }
}

/// Max characters for free-text fields in `StopBackgroundTask`/`StopSessionCron` entries.
pub const MAX_STOP_ENTRY_TEXT_CHARS: usize = 1000;

/// Clip `text` to `max` chars (on a char boundary) with a `… [+N chars]` marker.
pub fn clip_text(text: &str, max: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max).collect();
    format!("{clipped}… [+{} chars]", char_count - max)
}

pub fn clip_stop_entry_text(text: &str) -> String {
    clip_text(text, MAX_STOP_ENTRY_TEXT_CHARS)
}

/// `SubagentStop` fire phase: always `Gate` today, `Observe` reserved and not emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStopPhase {
    Gate,
    Observe,
}

/// One in-flight background task in a `Stop` hook input (camelCase on the wire).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopBackgroundTask {
    pub id: String,
    pub r#type: BackgroundTaskType,
    /// Always `running` for in-flight entries.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

/// One session-scoped scheduled wakeup (scheduler task or `/loop`) in a `Stop` hook input.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopSessionCron {
    pub id: String,
    /// Human-readable interval (e.g. `every 5 minutes`): grok schedules are intervals, not cron.
    pub schedule: String,
    pub recurring: bool,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskType {
    Shell,
    Monitor,
    Subagent,
}

/// `StopFailure` error type. Grok emits a subset: capacity errors fold into
/// `RateLimit`, and there is no `billing_error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopFailureKind {
    RateLimit,
    AuthenticationFailed,
    InvalidRequest,
    ServerError,
    MaxOutputTokens,
    Unknown,
}

impl StopFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::AuthenticationFailed => "authentication_failed",
            Self::InvalidRequest => "invalid_request",
            Self::ServerError => "server_error",
            Self::MaxOutputTokens => "max_output_tokens",
            Self::Unknown => "unknown",
        }
    }
}

/// The normalized event envelope sent to hook commands on stdin as JSON:
/// common metadata plus an event-specific payload.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookEventEnvelope {
    pub hook_event_name: HookEventName,
    pub session_id: String,
    pub cwd: String,
    pub workspace_root: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    /// Session permission mode (`default`, `auto`, `plan`, `bypassPermissions`) at fire time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(flatten)]
    pub payload: HookPayload,
}

/// Event-specific payload, flattened into the envelope JSON.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum HookPayload {
    SessionStart {
        source: String,
        #[serde(rename = "modelId", skip_serializing_if = "Option::is_none")]
        model_id: Option<String>,
        #[serde(rename = "agentType", skip_serializing_if = "Option::is_none")]
        agent_type: Option<String>,
    },
    SessionEnd {
        reason: String,
        #[serde(rename = "turnCount", skip_serializing_if = "Option::is_none")]
        turn_count: Option<u64>,
        #[serde(rename = "toolCallCount", skip_serializing_if = "Option::is_none")]
        tool_call_count: Option<u64>,
    },
    Stop {
        reason: String,
        /// True when this Stop fires while the agent is already continuing from a
        /// previous Stop-hook block this turn; hooks check it to avoid blocking on a
        /// condition that will never resolve.
        #[serde(rename = "stopHookActive")]
        stop_hook_active: bool,
        #[serde(
            rename = "lastAssistantMessage",
            skip_serializing_if = "Option::is_none"
        )]
        last_assistant_message: Option<String>,
        /// In-flight background work that could wake the session; empty when none in
        /// flight, omitted (not empty) at fire sites that don't enumerate (session end).
        #[serde(rename = "backgroundTasks", skip_serializing_if = "Option::is_none")]
        background_tasks: Option<Vec<StopBackgroundTask>>,
        #[serde(rename = "sessionCrons", skip_serializing_if = "Option::is_none")]
        session_crons: Option<Vec<StopSessionCron>>,
    },
    StopFailure {
        error: StopFailureKind,
        #[serde(rename = "errorDetails", skip_serializing_if = "Option::is_none")]
        error_details: Option<String>,
        /// Rendered error text shown in the conversation: unlike `Stop`, the error
        /// string, not assistant output.
        #[serde(
            rename = "lastAssistantMessage",
            skip_serializing_if = "Option::is_none"
        )]
        last_assistant_message: Option<String>,
    },

    PreToolUse {
        /// The tool the model invoked. For the meta-dispatch tools (`use_tool`
        /// and the external MCP-call tool) this is the resolved underlying tool
        /// (`server__tool`) rather than the dispatcher, so matchers key on it.
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
        /// The subagent's type when this tool runs inside one (the envelope's `sessionId`
        /// gives its identity); `None` for the top-level session.
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    PostToolUse {
        /// Resolved underlying tool for meta-dispatch tools (see `PreToolUse`).
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolResult")]
        tool_result: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
        #[serde(rename = "toolResultTruncated")]
        tool_result_truncated: bool,
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(rename = "isBackgrounded")]
        is_backgrounded: bool,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    PostToolUseFailure {
        /// Resolved underlying tool for meta-dispatch tools (see `PreToolUse`).
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
        error: String,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    PermissionDenied {
        /// Resolved underlying tool for meta-dispatch tools (see `PreToolUse`).
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
    },

    UserPromptSubmit {
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    Notification {
        #[serde(rename = "notificationType")]
        notification_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        /// Compat: some callers use `level` instead of `notificationType`.
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<String>,
    },

    SubagentStart {
        #[serde(rename = "subagentId")]
        subagent_id: String,
        #[serde(rename = "subagentType")]
        subagent_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    SubagentStop {
        phase: SubagentStopPhase,
        #[serde(rename = "subagentId")]
        subagent_id: String,
        #[serde(rename = "subagentType")]
        subagent_type: String,
        /// Subagent analogue of `Stop::stop_hook_active`.
        #[serde(rename = "stopHookActive", skip_serializing_if = "Option::is_none")]
        stop_hook_active: Option<bool>,
        #[serde(
            rename = "lastAssistantMessage",
            skip_serializing_if = "Option::is_none"
        )]
        last_assistant_message: Option<String>,
    },

    PreCompact {
        /// "manual" or "auto".
        source: String,
    },
    PostCompact {
        /// "manual" or "auto".
        source: String,
    },
}

impl HookPayload {
    /// The value a [`MatcherPolicy::Tested`] matcher is tested against, or `None` when
    /// the payload carries nothing selectable (matchers then fire-all, the fail-open default).
    pub fn match_value(&self) -> Option<&str> {
        let value = match self {
            Self::PreToolUse { tool_name, .. }
            | Self::PostToolUse { tool_name, .. }
            | Self::PostToolUseFailure { tool_name, .. }
            | Self::PermissionDenied { tool_name, .. } => tool_name,
            Self::Notification {
                notification_type, ..
            } => notification_type,
            Self::SubagentStart { subagent_type, .. }
            | Self::SubagentStop { subagent_type, .. } => subagent_type,
            Self::SessionStart { source, .. }
            | Self::PreCompact { source }
            | Self::PostCompact { source } => source,
            Self::SessionEnd { reason, .. } => reason,
            // Always a non-empty name, unlike the free-text arms above.
            Self::StopFailure { error, .. } => return Some(error.as_str()),
            // Ignored events listed explicitly so a new Tested event can't silently return None.
            Self::Stop { .. } | Self::UserPromptSubmit { .. } => return None,
        };
        Some(value.as_str()).filter(|v| !v.is_empty())
    }
}

/// Truncate a JSON value if its serialized size exceeds `MAX_PAYLOAD_SIZE`.
///
/// Returns `(possibly_truncated_value, was_truncated)`.
pub fn truncate_payload(value: serde_json::Value) -> (serde_json::Value, bool) {
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    if serialized.len() <= MAX_PAYLOAD_SIZE {
        return (value, false);
    }

    // Cut at the largest char boundary <= MAX_PAYLOAD_SIZE so the slice never
    // splits a multibyte codepoint.
    let mut end = MAX_PAYLOAD_SIZE;
    while !serialized.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = serialized[..end].to_string();
    result.push_str(" [truncated]");
    (serde_json::Value::String(result), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_name_deser_all_variants() {
        let cases: &[(&str, &str, HookEventName)] = &[
            ("SessionStart", "session_start", HookEventName::SessionStart),
            ("PreToolUse", "pre_tool_use", HookEventName::PreToolUse),
            ("PostToolUse", "post_tool_use", HookEventName::PostToolUse),
            (
                "PostToolUseFailure",
                "post_tool_use_failure",
                HookEventName::PostToolUseFailure,
            ),
            ("SessionEnd", "session_end", HookEventName::SessionEnd),
            ("Stop", "stop", HookEventName::Stop),
            ("StopFailure", "stop_failure", HookEventName::StopFailure),
            ("Notification", "notification", HookEventName::Notification),
            (
                "UserPromptSubmit",
                "user_prompt_submit",
                HookEventName::UserPromptSubmit,
            ),
            (
                "PermissionDenied",
                "permission_denied",
                HookEventName::PermissionDenied,
            ),
            (
                "SubagentStart",
                "subagent_start",
                HookEventName::SubagentStart,
            ),
            ("SubagentStop", "subagent_stop", HookEventName::SubagentStop),
            ("SubagentEnd", "subagent_end", HookEventName::SubagentEnd),
            ("PreCompact", "pre_compact", HookEventName::PreCompact),
            ("PostCompact", "post_compact", HookEventName::PostCompact),
        ];

        for (pascal, snake, expected) in cases {
            let from_pascal: HookEventName =
                serde_json::from_str(&format!("\"{pascal}\"")).unwrap();
            assert_eq!(
                from_pascal, *expected,
                "PascalCase deser failed for {pascal}"
            );

            let from_snake: HookEventName = serde_json::from_str(&format!("\"{snake}\"")).unwrap();
            assert_eq!(from_snake, *expected, "snake_case deser failed for {snake}");
        }
    }

    #[test]
    fn event_name_display_all_variants() {
        let cases: &[(HookEventName, &str)] = &[
            (HookEventName::SessionStart, "session_start"),
            (HookEventName::PreToolUse, "pre_tool_use"),
            (HookEventName::PostToolUse, "post_tool_use"),
            (HookEventName::PostToolUseFailure, "post_tool_use_failure"),
            (HookEventName::SessionEnd, "session_end"),
            (HookEventName::Stop, "stop"),
            (HookEventName::StopFailure, "stop_failure"),
            (HookEventName::Notification, "notification"),
            (HookEventName::UserPromptSubmit, "user_prompt_submit"),
            (HookEventName::PermissionDenied, "permission_denied"),
            (HookEventName::SubagentStart, "subagent_start"),
            (HookEventName::SubagentStop, "subagent_stop"),
            (HookEventName::SubagentEnd, "subagent_stop"), // alias collapses
            (HookEventName::PreCompact, "pre_compact"),
            (HookEventName::PostCompact, "post_compact"),
        ];
        for (event, expected) in cases {
            assert_eq!(&event.to_string(), expected, "Display wrong for {event:?}");
        }
    }

    #[test]
    fn event_name_deser_camel_and_operation_aliases() {
        let cases: &[(&str, HookEventName)] = &[
            ("sessionStart", HookEventName::SessionStart),
            ("preToolUse", HookEventName::PreToolUse),
            ("beforeShellExecution", HookEventName::PreToolUse),
            ("beforeMCPExecution", HookEventName::PreToolUse),
            ("beforeReadFile", HookEventName::PreToolUse),
            ("postToolUse", HookEventName::PostToolUse),
            ("afterShellExecution", HookEventName::PostToolUse),
            ("afterMCPExecution", HookEventName::PostToolUse),
            ("afterFileEdit", HookEventName::PostToolUse),
            ("afterAgentResponse", HookEventName::PostToolUse),
            ("afterAgentThought", HookEventName::PostToolUse),
            ("beforeSubmitPrompt", HookEventName::UserPromptSubmit),
            ("subagentStop", HookEventName::SubagentStop),
            ("subagentEnd", HookEventName::SubagentEnd),
            ("preCompact", HookEventName::PreCompact),
            ("stopFailure", HookEventName::StopFailure),
        ];
        for (spelling, expected) in cases {
            let parsed: HookEventName = serde_json::from_str(&format!("\"{spelling}\"")).unwrap();
            assert_eq!(parsed, *expected, "alias deser failed for {spelling}");
        }
    }

    #[test]
    fn event_name_unknown_rejected() {
        let result = serde_json::from_str::<HookEventName>("\"UnknownEvent\"");
        assert!(result.is_err());
    }

    #[test]
    fn event_traits_report_gate_matcher_and_hub_forward() {
        use super::{GateKind, MatcherPolicy};

        assert_eq!(HookEventName::PreToolUse.traits().gate, GateKind::Tool);
        assert_eq!(HookEventName::Stop.traits().gate, GateKind::Stop);
        assert_eq!(HookEventName::SubagentStop.traits().gate, GateKind::Stop);
        assert_eq!(
            HookEventName::SubagentEnd.traits().gate,
            GateKind::Stop,
            "alias resolves through canonical()"
        );
        assert_eq!(HookEventName::PostToolUse.traits().gate, GateKind::Observe);

        assert_eq!(HookEventName::Stop.traits().matcher, MatcherPolicy::Ignored);
        assert_eq!(
            HookEventName::UserPromptSubmit.traits().matcher,
            MatcherPolicy::Ignored
        );
        assert_eq!(
            HookEventName::SessionStart.traits().matcher,
            MatcherPolicy::Tested
        );

        assert!(!HookEventName::PreToolUse.traits().hub_forward);
        assert!(HookEventName::Stop.traits().hub_forward);
    }

    #[test]
    fn clip_stop_entry_text_clips_on_char_boundary() {
        assert_eq!(clip_stop_entry_text("short"), "short");
        let exact = "x".repeat(MAX_STOP_ENTRY_TEXT_CHARS);
        assert_eq!(clip_stop_entry_text(&exact), exact);

        let long = "x".repeat(MAX_STOP_ENTRY_TEXT_CHARS + 42);
        let clipped = clip_stop_entry_text(&long);
        assert!(clipped.ends_with("… [+42 chars]"));

        let unicode = "€".repeat(MAX_STOP_ENTRY_TEXT_CHARS + 7);
        let clipped = clip_stop_entry_text(&unicode);
        assert!(clipped.ends_with("… [+7 chars]"));
    }

    #[test]
    fn stop_payload_serializes_task_and_cron_entries() {
        let envelope = HookEventEnvelope {
            hook_event_name: HookEventName::Stop,
            session_id: "s".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "t".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::Stop {
                reason: "end_turn".into(),
                stop_hook_active: true,
                last_assistant_message: Some("done".into()),
                background_tasks: Some(vec![
                    StopBackgroundTask {
                        id: "task-001".into(),
                        r#type: BackgroundTaskType::Shell,
                        status: "running".into(),
                        description: None,
                        command: Some("tail -f /var/log/syslog".into()),
                        agent_type: None,
                    },
                    StopBackgroundTask {
                        id: "task-002".into(),
                        r#type: BackgroundTaskType::Subagent,
                        status: "running".into(),
                        description: Some("explore the repo".into()),
                        command: None,
                        agent_type: Some("explore".into()),
                    },
                ]),
                session_crons: Some(vec![StopSessionCron {
                    id: "cron-001".into(),
                    schedule: "every 2h".into(),
                    recurring: true,
                    prompt: "check the build".into(),
                }]),
            },
        };
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["stopHookActive"], true);
        assert_eq!(value["backgroundTasks"][0]["id"], "task-001");
        assert_eq!(value["backgroundTasks"][0]["type"], "shell");
        assert_eq!(
            value["backgroundTasks"][0]["command"],
            "tail -f /var/log/syslog"
        );
        assert_eq!(value["backgroundTasks"][1]["agentType"], "explore");
        assert_eq!(value["sessionCrons"][0]["schedule"], "every 2h");
        assert_eq!(value["sessionCrons"][0]["recurring"], true);
    }

    #[test]
    fn subagent_stop_phase_serializes_lowercase() {
        let payload = HookPayload::SubagentStop {
            phase: SubagentStopPhase::Observe,
            subagent_id: "sub-1".into(),
            subagent_type: "explore".into(),
            stop_hook_active: None,
            last_assistant_message: None,
        };
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value["phase"], "observe");
        assert_eq!(
            serde_json::to_value(SubagentStopPhase::Gate).unwrap(),
            "gate"
        );
    }

    #[test]
    fn stop_failure_kind_as_str_matches_serialization() {
        for kind in [
            StopFailureKind::RateLimit,
            StopFailureKind::AuthenticationFailed,
            StopFailureKind::InvalidRequest,
            StopFailureKind::ServerError,
            StopFailureKind::MaxOutputTokens,
            StopFailureKind::Unknown,
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::from(kind.as_str()),
                "{kind:?} serialization drifted from as_str"
            );
        }
    }

    #[test]
    fn truncate_small_payload() {
        let value = serde_json::json!({"key": "small"});
        let (result, truncated) = truncate_payload(value.clone());
        assert!(!truncated);
        assert_eq!(result, value);
    }

    #[test]
    fn truncate_large_payload() {
        let value = serde_json::Value::String("x".repeat(MAX_PAYLOAD_SIZE + 1000));
        let (result, truncated) = truncate_payload(value);
        assert!(truncated);
        let s = result.as_str().unwrap();
        assert!(s.ends_with("[truncated]"));
        assert!(s.len() < MAX_PAYLOAD_SIZE + 100);

        // '€' is 3 bytes, so the cut lands mid-codepoint and must fall back to a char boundary.
        let (unicode, truncated) =
            truncate_payload(serde_json::Value::String("€".repeat(MAX_PAYLOAD_SIZE)));
        assert!(truncated);
        assert!(unicode.as_str().unwrap().ends_with("[truncated]"));
    }

    #[test]
    fn envelope_serializes_camel_case() {
        let envelope = HookEventEnvelope {
            hook_event_name: HookEventName::SessionStart,
            session_id: "test-session".into(),
            cwd: "/tmp".into(),
            workspace_root: "/tmp".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::SessionStart {
                source: "new".into(),
                model_id: Some("grok-3".into()),
                agent_type: None,
            },
        };
        let value = serde_json::to_value(&envelope).unwrap();
        for key in ["hookEventName", "sessionId", "workspaceRoot", "modelId"] {
            assert!(value.get(key).is_some(), "missing camelCase key {key}");
        }
        for key in ["hook_event_name", "session_id", "model_id"] {
            assert!(value.get(key).is_none(), "leaked snake_case key {key}");
        }
    }
}
