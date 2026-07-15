use serde::Serialize;

/// Maximum serialized size for `toolInput` or `toolResult` in bytes (128 KB).
pub const MAX_PAYLOAD_SIZE: usize = 128 * 1024;

/// Hook event types.
///
/// Accepts both PascalCase (`"PreToolUse"`) and snake_case (`"pre_tool_use"`)
/// during deserialization for migration compatibility.
/// Serializes to snake_case for the hook envelope wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventName {
    // ── Session lifecycle ───────────────────────────────────────
    SessionStart,
    SessionEnd,
    /// Fires when an agent turn ends (completed, cancelled, or error).
    Stop,
    /// Fires when the turn ends due to an API error. Output and exit code are ignored.
    StopFailure,

    // ── Tool events ─────────────────────────────────────────────
    PreToolUse,
    PostToolUse,
    /// Fires after a tool call fails (throws an error).
    PostToolUseFailure,
    /// Fires when a tool call is denied by the permission system.
    PermissionDenied,

    // ── User / notification events ──────────────────────────────
    /// Fires when the user submits a prompt.
    UserPromptSubmit,
    /// Fires when a notification is sent (e.g., permission prompt, idle).
    Notification,

    // ── Subagent events ─────────────────────────────────────────
    /// Fires when a subagent is spawned.
    SubagentStart,
    /// Fires when a subagent completes.
    SubagentStop,
    /// Alias for SubagentStop (kept for backward compatibility).
    SubagentEnd,

    // ── Compaction events ───────────────────────────────────────
    /// Fires before context compaction.
    PreCompact,
    /// Fires after context compaction completes.
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
            // map to our generic PreToolUse/PostToolUse — the hook script receives the
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
                 PreCompact, PostCompact"
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

    /// Returns true if this event type uses blocking (deny/allow) semantics.
    pub fn is_blocking(&self) -> bool {
        matches!(self, Self::PreToolUse)
    }

    /// Events that don't support matcher patterns (fire on every occurrence).
    pub fn is_lifecycle(&self) -> bool {
        matches!(
            self,
            Self::SessionStart | Self::SessionEnd | Self::Stop | Self::UserPromptSubmit
        )
    }
}

/// The normalized event envelope sent to hook commands on stdin as JSON.
///
/// Contains common metadata plus an event-specific payload.
/// All field names use camelCase for the JSON wire format.
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
    #[serde(flatten)]
    pub payload: HookPayload,
}

/// Event-specific payload variants, flattened into the envelope JSON via
/// `#[serde(untagged)]`. Grouped to match `HookEventName`.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum HookPayload {
    // ── Session lifecycle ───────────────────────────────────────
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
    },
    StopFailure {
        error: String,
    },

    // ── Tool events ─────────────────────────────────────────────
    PreToolUse {
        /// The tool the model invoked. For the meta-dispatch tools (`use_tool`
        /// and the external MCP-call tool) this is the resolved underlying tool
        /// (`server__tool`), not the dispatcher — matchers key on it directly.
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
        #[serde(rename = "permissionMode", skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
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

    // ── User / notification events ──────────────────────────────
    /// Fires when the user submits a prompt.
    UserPromptSubmit {
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// Fires on agent notifications (permission prompts, idle, etc.).
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

    // ── Subagent events ─────────────────────────────────────────
    /// Fires when a subagent is spawned.
    SubagentStart {
        #[serde(rename = "subagentId")]
        subagent_id: String,
        #[serde(rename = "subagentType")]
        subagent_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// Fires when a subagent completes.
    SubagentStop {
        #[serde(rename = "subagentId")]
        subagent_id: String,
        #[serde(rename = "subagentType")]
        subagent_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(rename = "exitCode", skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },

    // ── Compaction events ───────────────────────────────────────
    PreCompact {
        /// "manual" or "auto".
        source: String,
    },
    PostCompact {
        /// "manual" or "auto".
        source: String,
    },
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
    fn event_name_serde_roundtrip() {
        let name = HookEventName::PreToolUse;
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"pre_tool_use\"");
        let parsed: HookEventName = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, name);
    }

    #[test]
    fn event_name_unknown_rejected() {
        let result = serde_json::from_str::<HookEventName>("\"UnknownEvent\"");
        assert!(result.is_err());
    }

    #[test]
    fn event_name_is_blocking() {
        assert!(HookEventName::PreToolUse.is_blocking());
        for event in [
            HookEventName::SessionStart,
            HookEventName::PostToolUse,
            HookEventName::PostToolUseFailure,
            HookEventName::SessionEnd,
            HookEventName::Stop,
            HookEventName::StopFailure,
            HookEventName::Notification,
            HookEventName::UserPromptSubmit,
            HookEventName::PermissionDenied,
            HookEventName::SubagentStart,
            HookEventName::SubagentStop,
            HookEventName::SubagentEnd,
            HookEventName::PreCompact,
            HookEventName::PostCompact,
        ] {
            assert!(!event.is_blocking(), "{event:?} should not be blocking");
        }
    }

    #[test]
    fn event_name_is_lifecycle() {
        let lifecycle = [
            HookEventName::SessionStart,
            HookEventName::SessionEnd,
            HookEventName::Stop,
            HookEventName::UserPromptSubmit,
        ];
        for event in lifecycle {
            assert!(event.is_lifecycle(), "{event:?} should be lifecycle");
        }

        let matchable = [
            HookEventName::PreToolUse,
            HookEventName::PostToolUse,
            HookEventName::PostToolUseFailure,
            HookEventName::PermissionDenied,
            HookEventName::StopFailure,
            HookEventName::Notification,
            HookEventName::SubagentStart,
            HookEventName::SubagentStop,
            HookEventName::SubagentEnd,
            HookEventName::PreCompact,
            HookEventName::PostCompact,
        ];
        for event in matchable {
            assert!(
                !event.is_lifecycle(),
                "{event:?} should support matchers, not be lifecycle"
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
        let big_string = "x".repeat(MAX_PAYLOAD_SIZE + 1000);
        let value = serde_json::Value::String(big_string);
        let (result, truncated) = truncate_payload(value);
        assert!(truncated);
        let s = result.as_str().unwrap();
        assert!(s.ends_with("[truncated]"));
        // Serialized size of the result string value should be <= MAX_PAYLOAD_SIZE + overhead
        assert!(s.len() < MAX_PAYLOAD_SIZE + 100);
    }

    #[test]
    fn truncate_large_payload_cuts_on_char_boundary() {
        // '€' is 3 bytes, so the MAX_PAYLOAD_SIZE-th byte lands mid-codepoint.
        let value = serde_json::Value::String("€".repeat(MAX_PAYLOAD_SIZE));
        let (result, truncated) = truncate_payload(value);
        assert!(truncated);
        assert!(result.as_str().unwrap().ends_with("[truncated]"));
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
            payload: HookPayload::SessionStart {
                source: "new".into(),
                model_id: Some("grok-3".into()),
                agent_type: None,
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("hookEventName"));
        assert!(json.contains("sessionId"));
        assert!(json.contains("workspaceRoot"));
        assert!(json.contains("modelId"));
        // Should NOT contain snake_case versions
        assert!(!json.contains("hook_event_name"));
        assert!(!json.contains("session_id"));
    }
}
