//! Data and channel types for subagent coordination.
//!
//! Request data is deliberately separate from command reply envelopes. The
//! shared coordinator actor owns every reply sender and every lifecycle
//! transition; child runners receive only plain request data.
//!
//! ## Resource types
//!
//! The primary resource injected into every session's `Resources`:
//!
//! - `SubagentBackendResource` — wraps an `Arc<dyn SubagentBackend>` that
//!   abstracts spawn/query/cancel (see [`super::backend`])
//! - `SubagentDepthCounter` — current nesting depth
//! - `MaxSubagentDepth` — configured max nesting depth
//! - `SessionIdResource` — carries the current session ID for parent scoping
//! - `TaskModelValidator` — validates explicit model slugs before background spawn
//!
//! All coordinator messages are funnelled through a single
//! `SubagentEventSender` / `SubagentEvent` enum channel.

use std::{fmt, sync::Arc, time::Duration};

use educe::Educe;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use xai_tool_types::SubagentContextMode;
use xai_tool_types::{SubagentCapabilityMode, SubagentIsolationMode, WaitMode};

use crate::register_resource;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SubagentOwner {
    #[default]
    Task,
    Swarm,
    Workflow {
        run_id: String,
    },
}

impl SubagentOwner {
    pub fn workflow(run_id: impl Into<String>) -> Self {
        Self::Workflow {
            run_id: run_id.into(),
        }
    }

    pub fn workflow_run_id(&self) -> Option<&str> {
        match self {
            Self::Task | Self::Swarm => None,
            Self::Workflow { run_id } => Some(run_id),
        }
    }

    pub fn is_swarm(&self) -> bool {
        matches!(self, Self::Swarm)
    }

    pub fn is_workflow(&self) -> bool {
        matches!(self, Self::Workflow { .. })
    }

    pub fn cancel_on_receiver_drop(&self) -> bool {
        matches!(self, Self::Swarm | Self::Workflow { .. })
    }
}

// Request / Response

/// Scheduler decision for a live AgentSwarm turn paused after a provider rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentRateLimitDecision {
    Retry,
    Fail,
}

pub const SWARM_RATE_LIMIT_RETRY_BASE_MS: u64 = 3_000;

pub fn swarm_rate_limit_backoff(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(u64::BITS - 1);
    let multiplier = 1u64 << exponent;
    Duration::from_millis(SWARM_RATE_LIMIT_RETRY_BASE_MS.saturating_mul(multiplier))
}

/// Runtime status emitted by an AgentSwarm child session.
#[derive(Debug, Clone)]
pub enum SubagentStatusEvent {
    ProviderRequestStarted {
        subagent_id: String,
    },
    RateLimitWaiting {
        subagent_id: String,
        attempt: u32,
        decision_tx: mpsc::UnboundedSender<SubagentRateLimitDecision>,
    },
    RateLimitRetrying {
        subagent_id: String,
        attempt: u32,
    },
}

/// Metadata that lets the shell group individual child sessions into one
/// model-facing AgentSwarm invocation.
#[derive(Clone)]
pub struct SwarmMemberMeta {
    pub swarm_id: String,
    pub description: String,
    pub index: u32,
    pub item: Option<String>,
    pub expected_members: u32,
    /// Runtime-only status channel back to the swarm scheduler.
    pub status_tx: Option<mpsc::UnboundedSender<SubagentStatusEvent>>,
}

impl fmt::Debug for SwarmMemberMeta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SwarmMemberMeta")
            .field("swarm_id", &self.swarm_id)
            .field("description", &self.description)
            .field("index", &self.index)
            .field("item", &self.item)
            .field("expected_members", &self.expected_members)
            .field("has_status_tx", &self.status_tx.is_some())
            .finish()
    }
}

impl PartialEq for SwarmMemberMeta {
    fn eq(&self, other: &Self) -> bool {
        self.swarm_id == other.swarm_id
            && self.description == other.description
            && self.index == other.index
            && self.item == other.item
            && self.expected_members == other.expected_members
    }
}

impl Eq for SwarmMemberMeta {}

/// How a spawn wants the child's initial context resolved.
///
/// `Default` defers to the effective child model's `subagent_context_default`
/// catalog flag (fresh when unset); `Explicit` is a caller decision — either
/// the model-facing `context` argument of the `task` tool or a harness caller
/// that requires a specific mode (goal planner forks; swarm members, loop
/// units, and harness-internal helpers stay fresh).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubagentContextRequest {
    /// No explicit choice: resolve from the child model's catalog default.
    #[default]
    Default,
    /// Explicit caller choice; never overridden by model metadata.
    Explicit(SubagentContextMode),
}

impl SubagentContextRequest {
    pub const FORK: Self = Self::Explicit(SubagentContextMode::Fork);
    pub const FRESH: Self = Self::Explicit(SubagentContextMode::Fresh);

    /// Effective mode given the child model's catalog default.
    pub fn resolve(self, model_default: Option<SubagentContextMode>) -> SubagentContextMode {
        match self {
            Self::Explicit(mode) => mode,
            Self::Default => model_default.unwrap_or(SubagentContextMode::Fresh),
        }
    }

    /// True only for a caller-explicit fork (used to preserve the legacy
    /// "fork pins the parent model" behavior for harness callers).
    pub fn is_explicit_fork(self) -> bool {
        self == Self::FORK
    }

    /// True when the spawn may still resolve to a fork (explicit fork, or
    /// deferred to model metadata). Used by spawn-context builders that must
    /// capture fork-only parent state before the effective mode is known.
    pub fn may_fork(self) -> bool {
        self != Self::FRESH
    }
}

/// Plain spawn request emitted by `TaskTool`.
#[derive(Debug, Clone)]
pub struct SubagentRequest {
    /// Subagent ID (UUID v7). Same as `TaskToolInput.task_id`; becomes the child session ID.
    pub id: String,
    pub prompt: String,
    pub description: String,
    pub subagent_type: String,
    pub parent_session_id: String,
    /// Parent turn/prompt ID that launched this subagent.
    ///
    /// Used to cancel only the subagents spawned by the currently-cancelled turn,
    /// without affecting background subagents from earlier turns.
    pub parent_prompt_id: Option<String>,
    /// Present only for children launched by `agent_swarm`.
    pub swarm: Option<SwarmMemberMeta>,
    /// Resume from a previously completed subagent's conversation.
    /// Inherits raw transcript, tool state, and model. System prompt is
    /// freshly rendered.
    pub resume_from: Option<String>,
    /// Explicit working directory for the child session.
    /// Validated at spawn time by the injected child runner.
    pub cwd: Option<String>,
    /// Runtime overrides for the child agent.
    pub runtime_overrides: SubagentRuntimeOverrides,
    /// Whether this subagent was launched with `run_in_background: true`.
    ///
    /// Controls immediate handle delivery and completion surfacing. A
    /// background child still auto-surfaces its completion to the model
    /// (buffered reminder / auto-wake) when `surface_completion` is set —
    /// background does not mean fire-and-forget. Prompt cancellation still
    /// cancels every child owned by that prompt.
    pub run_in_background: bool,
    /// When false, the subagent's completion is NOT buffered for the
    /// between-turn "idle completion" reminder — used by harness-internal
    /// subagents like the goal planner/classifier that the model must never see.
    pub surface_completion: bool,
    pub await_to_completion: bool,
    /// Requested initial-context mode. `Explicit` comes from the model-facing
    /// `context` argument or a harness caller; `Default` resolves from the
    /// effective child model's `subagent_context_default`. Successful
    /// `resume_from` takes precedence over any fork.
    pub context: SubagentContextRequest,
    pub owner: SubagentOwner,
    pub cancel_token: CancellationToken,
}

impl SubagentRequest {
    pub fn from_scheduler_loop(&self) -> bool {
        self.runtime_overrides.loop_task_id.is_some()
    }

    /// The caller blocks on the foreground await budget (neither backgrounded
    /// nor awaiting to completion).
    pub fn awaits_in_foreground(&self) -> bool {
        !self.run_in_background && !self.await_to_completion
    }
}

/// Team identity injected into every model-facing session.
///
/// `agent_id` is the current session ID. `team_scope_id` is the root session
/// that owns the flat subagent cohort. Keeping these separate lets children
/// address siblings without weakening the existing parent-session scoping used
/// by task polling and cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMailboxIdentity {
    pub team_scope_id: String,
    pub agent_id: String,
}

register_resource!("grok_build", "AgentMailboxIdentity", AgentMailboxIdentity);

/// `Message` is the steering channel: it is pushed into the recipient session
/// live (a running turn consumes it at an interjection boundary; an idle
/// recipient starts an agent-message turn). `FollowupTask` is the passive
/// queue: it waits in the recipient's mailbox until drained via `wait_agent`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentMailboxMessageKind {
    Message,
    FollowupTask,
    NativeMessage,
    NativeFollowup,
}

impl AgentMailboxMessageKind {
    pub fn steers_recipient(self) -> bool {
        matches!(
            self,
            Self::Message | Self::NativeMessage | Self::NativeFollowup
        )
    }

    pub fn triggers_turn(self) -> bool {
        matches!(self, Self::Message | Self::NativeFollowup)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message | Self::NativeMessage => "message",
            Self::FollowupTask | Self::NativeFollowup => "followup_task",
        }
    }
}

/// One immutable peer message. Runtime-owned fields are never accepted from
/// the model; the send tools stamp them before dispatch.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct AgentMailboxMessage {
    pub message_id: String,
    pub team_scope_id: String,
    pub from_agent_id: String,
    pub to_agent_id: String,
    pub kind: AgentMailboxMessageKind,
    pub body: String,
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeAgentMessage>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct NativeAgentMessage {
    pub author: String,
    pub recipient: String,
    pub encrypted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_prompt_id: Option<String>,
}

impl AgentMailboxMessage {
    pub fn display_body(&self) -> &str {
        if self.native.as_ref().is_some_and(|native| native.encrypted) {
            "[Encrypted agent message]"
        } else {
            &self.body
        }
    }

    pub fn native_wire_item(&self) -> Option<serde_json::Value> {
        let native = self.native.as_ref()?;
        let content = if native.encrypted {
            serde_json::json!({"type": "encrypted_content", "encrypted_content": self.body})
        } else {
            serde_json::json!({"type": "input_text", "text": self.body})
        };
        Some(serde_json::json!({
            "type": "agent_message",
            "id": self.message_id,
            "author": native.author,
            "recipient": native.recipient,
            "content": [content],
        }))
    }
}

#[derive(Debug, Clone, Default)]
pub struct NativeAgentSpawn {
    pub task_name: String,
    pub fork_turns: Option<usize>,
    pub service_tier: Option<String>,
    pub message: Option<AgentMailboxMessage>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NativeAgentRecord {
    pub task_name: String,
    pub agent_id: String,
    pub agent_type: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub mailbox: Vec<AgentMailboxMessage>,
}

#[derive(Debug)]
pub enum NativeAgentOperation {
    List {
        path_prefix: Option<String>,
    },
    Message {
        target: String,
        message: AgentMailboxMessage,
    },
    Interrupt {
        target: String,
    },
    Wait {
        timeout_ms: u64,
    },
}

#[derive(Debug)]
pub struct NativeAgentRequest {
    pub identity: AgentMailboxIdentity,
    pub operation: NativeAgentOperation,
    pub respond_to: oneshot::Sender<Result<serde_json::Value, String>>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct AgentRosterEntry {
    pub agent_id: String,
    pub is_root: bool,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ListAgentsOutput {
    pub team_scope_id: String,
    pub agents: Vec<AgentRosterEntry>,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageDeliveryStatus {
    Queued,
    Delivered,
    Rejected,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct AgentMessageSendOutput {
    pub message_id: String,
    pub target_agent_id: String,
    pub status: AgentMessageDeliveryStatus,
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct WaitAgentMessagesOutput {
    pub messages: Vec<AgentMailboxMessage>,
    pub timed_out: bool,
}

/// Spawn command envelope owned by the coordinator mailbox.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentSpawnRequest {
    pub request: Box<SubagentRequest>,
    #[educe(Debug(ignore))]
    pub result_tx: oneshot::Sender<SubagentResult>,
}

impl std::ops::Deref for SubagentSpawnRequest {
    type Target = SubagentRequest;

    fn deref(&self) -> &Self::Target {
        &self.request
    }
}

impl SubagentSpawnRequest {
    /// Build and send a reply while the plain request remains borrowable.
    ///
    /// Primarily useful for channel adapters and deterministic test harnesses;
    /// production lifecycle replies are owned by `SubagentCoordinator`.
    pub fn respond_with(
        self,
        build: impl FnOnce(&SubagentRequest) -> SubagentResult,
    ) -> Result<(), SubagentResult> {
        let result = build(&self.request);
        self.result_tx.send(result)
    }
}

/// Per-spawn dynamic runtime overrides for a subagent.
///
/// Optional values inherit from the parent or role default. Explicit values take
/// precedence over role defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ModelOverrideProvenance {
    /// Internal harness, role, persona, or config resolution.
    #[default]
    Harness,
    /// A model-facing `Task.model` argument.
    Tool,
}

#[derive(Debug, Clone, Default)]
pub struct SubagentRuntimeOverrides {
    pub native_agent: Option<NativeAgentSpawn>,
    /// Override the model (e.g. "test-model").
    pub model: Option<String>,
    /// Whether `model` came from a model-facing Task call or internal harness logic.
    pub model_override_provenance: ModelOverrideProvenance,
    /// Override reasoning effort (e.g. "low", "medium", "high").
    pub reasoning_effort: Option<String>,
    /// Named persona/SOUL template to apply.
    pub persona: Option<String>,
    /// Capability mode controlling tool access.
    pub capability_mode: Option<SubagentCapabilityMode>,
    /// Isolation mode for child execution environment.
    /// `None` means "use role/persona default" (which itself defaults to `None`/shared workspace).
    pub isolation: Option<SubagentIsolationMode>,
    /// `/goal`-only harness override: the `agent_type` (e.g. `"cursor"`,
    /// `"grok-build-plan"`) whose `AgentDefinition` decides the child's harness
    /// flavor — system prompt + toolset — applied
    /// REGARDLESS of the parent agent (so a session can pin a
    /// compat-harness verifier and vice versa).
    /// Orthogonal to `subagent_type`, which still selects the toolset-role
    /// (implementer vs explorer). `None` for every non-goal spawn ⇒ the parent
    /// agent decides the flavor (unchanged behavior).
    pub harness_agent_type: Option<String>,
    pub completion_output_cap: Option<usize>,
    pub spawn_depth: Option<u32>,
    pub output_token_budget: Option<u64>,
    pub output_schema: Option<serde_json::Value>,
    pub loop_task_id: Option<String>,
}

/// Re-export of [`xai_tool_types::is_not_sentinel`] for existing call sites.
pub use xai_tool_types::is_not_sentinel;

/// Sanitize a model-emitted `cwd` argument for the `task` tool.
///
/// Strips stray surrounding quote/backtick characters (matched or unmatched),
/// trims whitespace, expands a leading `~` to the user's home directory, and
/// rejects sentinel placeholders (`""`, `"null"`, `"none"`, `"undefined"`).
///
/// Returns `Some(cleaned)` for a usable path, `None` if the value should be
/// treated as absent. Shared by the tool layer (`task::mod`) and the
/// defense-in-depth check in `xai-grok-shell`'s subagent coordinator.
pub fn sanitize_cwd_value(s: &str) -> Option<String> {
    let unquoted = s.trim().trim_matches(['"', '\'', '`']);
    // Re-trim after stripping quotes: this trim flows into the returned
    // value; the trim inside `is_not_sentinel` does not.
    let cleaned = unquoted.trim();
    if !is_not_sentinel(cleaned) {
        return None;
    }
    Some(shellexpand::tilde(cleaned).into_owned())
}

/// Returns `true` if the string looks like a real subagent ID rather than a
/// model-emitted placeholder (`""`, `"null"`, `"none"`, `"undefined"`, whitespace).
pub fn is_valid_resume_id(s: &str) -> bool {
    is_not_sentinel(s)
}

/// Extension methods for [`SubagentCapabilityMode`] that depend on this crate's
/// tool-config internals (`ToolKind` / `ToolServerConfig`).
pub trait SubagentCapabilityModeExt {
    /// Filter a tool config to only include tools allowed by this mode.
    ///
    /// Uses the `kind` field on each `ToolConfig`, populated automatically
    /// by `for_tool::<T>()` / `From<&T: Tool>` at toolset construction time.
    /// Tools without a `kind` (e.g. MCP/custom tools via
    /// `ToolConfig::from_id()`) are preserved unconditionally.
    fn filter_tool_config(self, config: &mut crate::registry::types::ToolServerConfig);

    /// Return the set of `ToolKind`s allowed under this capability mode.
    fn allowed_tool_kinds(self) -> &'static [crate::types::tool::ToolKind];
}

/// Prune background-task lifecycle tools (`get_task_output` / `kill_task`) when
/// no tool that can spawn background work remains in the config.
pub fn prune_orphaned_background_task_tools(config: &mut crate::registry::types::ToolServerConfig) {
    use crate::types::tool::ToolKind;

    let has_task_tool = config
        .tools
        .iter()
        .any(|tc| tc.kind == Some(ToolKind::Task));
    let has_background_capable_bash = config.tools.iter().any(is_background_capable_bash_tool);
    if has_task_tool || has_background_capable_bash {
        return;
    }

    config.tools.retain(|tc| {
        !matches!(
            tc.kind,
            Some(ToolKind::BackgroundTaskAction | ToolKind::KillTaskAction)
        )
    });
}

/// Remove the plan-mode tools (`enter_plan_mode` / `exit_plan_mode`) from a
/// subagent tool config.
///
/// Plan mode is a root-session interaction: `exit_plan_mode` raises the
/// client's plan-approval reverse-request, and the pager renders that approval
/// on the parent agent's view — a child calling it presents a plan dialog
/// indistinguishable from the parent's. Children get a fresh Inactive plan
/// tracker and must never drive the plan-mode lifecycle themselves.
pub fn strip_plan_mode_tools(config: &mut crate::registry::types::ToolServerConfig) {
    use crate::types::tool::ToolKind;

    config
        .tools
        .retain(|tc| !matches!(tc.kind, Some(ToolKind::EnterPlan | ToolKind::ExitPlan)));
}

/// Remove every tool that can spawn nested agents (`task`, `agent_swarm`,
/// `workflow`) from a child toolset at max subagent depth, then prune the
/// background-task lifecycle tools that only exist to manage spawned work.
///
/// This is the single strip point keeping the subagent tree flat: children
/// must never see a spawn surface, no matter which orchestrator launched them.
pub fn strip_nested_spawn_tools(config: &mut crate::registry::types::ToolServerConfig) {
    use crate::types::tool::ToolKind;

    config.tools.retain(|tc| {
        !matches!(
            tc.kind,
            Some(ToolKind::Task | ToolKind::AgentSwarm | ToolKind::Workflow)
        )
    });
    prune_orphaned_background_task_tools(config);
}

fn is_background_capable_bash_tool(tc: &crate::registry::types::ToolConfig) -> bool {
    match tc.id.as_str() {
        "GrokBuild:run_terminal_cmd" | "GrokBuildConcise:run_terminal_cmd" => tc
            .params
            .as_ref()
            .and_then(|params| params.get("enabled_background"))
            .and_then(|value| value.as_bool())
            .unwrap_or(true),
        "OpenCode:bash" => true,
        _ => false,
    }
}

impl SubagentCapabilityModeExt for SubagentCapabilityMode {
    fn filter_tool_config(self, config: &mut crate::registry::types::ToolServerConfig) {
        if self == Self::All {
            return;
        }
        let allowed = self.allowed_tool_kinds();
        config.tools.retain(|tc| match tc.kind {
            Some(k) => allowed.contains(&k),
            None => true,
        });
        prune_orphaned_background_task_tools(config);
    }

    /// Return the set of `ToolKind`s allowed under this capability mode.
    fn allowed_tool_kinds(self) -> &'static [crate::types::tool::ToolKind] {
        use crate::types::tool::ToolKind;
        match self {
            Self::ReadOnly => &[
                ToolKind::Read,
                ToolKind::ListDir,
                ToolKind::List,
                ToolKind::Search,
                ToolKind::Lsp,
                ToolKind::Plan,
                ToolKind::MemorySearch,
                ToolKind::MemoryGet,
                ToolKind::WebSearch,
                ToolKind::WebFetch,
                ToolKind::BackgroundTaskAction,
                ToolKind::KillTaskAction,
                ToolKind::Task,
                ToolKind::AgentCollaboration,
                ToolKind::AskUser,
                ToolKind::Skill,
            ],
            Self::ReadWrite => &[
                ToolKind::Read,
                ToolKind::ListDir,
                ToolKind::List,
                ToolKind::Search,
                ToolKind::Lsp,
                ToolKind::Edit,
                ToolKind::Write,
                ToolKind::Delete,
                ToolKind::Move,
                ToolKind::Plan,
                ToolKind::MemorySearch,
                ToolKind::MemoryGet,
                ToolKind::WebSearch,
                ToolKind::WebFetch,
                ToolKind::ImageGen,
                ToolKind::VideoGen,
                ToolKind::ImageToVideo,
                ToolKind::ReferenceToVideo,
                ToolKind::BackgroundTaskAction,
                ToolKind::KillTaskAction,
                ToolKind::Task,
                ToolKind::AgentCollaboration,
                ToolKind::AskUser,
                ToolKind::Skill,
            ],
            Self::Execute => &[
                ToolKind::Read,
                ToolKind::ListDir,
                ToolKind::List,
                ToolKind::Search,
                ToolKind::Lsp,
                ToolKind::Execute,
                ToolKind::Plan,
                ToolKind::MemorySearch,
                ToolKind::MemoryGet,
                ToolKind::WebSearch,
                ToolKind::WebFetch,
                ToolKind::BackgroundTaskAction,
                ToolKind::KillTaskAction,
                ToolKind::Task,
                ToolKind::AgentCollaboration,
                ToolKind::AskUser,
                ToolKind::Skill,
            ],
            Self::All => &[
                ToolKind::Read,
                ToolKind::ListDir,
                ToolKind::List,
                ToolKind::Search,
                ToolKind::Lsp,
                ToolKind::Edit,
                ToolKind::Write,
                ToolKind::Delete,
                ToolKind::Move,
                ToolKind::Execute,
                ToolKind::Plan,
                ToolKind::MemorySearch,
                ToolKind::MemoryGet,
                ToolKind::WebSearch,
                ToolKind::WebFetch,
                ToolKind::ImageGen,
                ToolKind::VideoGen,
                ToolKind::ImageToVideo,
                ToolKind::ReferenceToVideo,
                ToolKind::BackgroundTaskAction,
                ToolKind::KillTaskAction,
                ToolKind::Task,
                ToolKind::AgentCollaboration,
                ToolKind::AskUser,
                ToolKind::Skill,
            ],
        }
    }
}

/// Result returned by a completed subagent.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub success: bool,
    /// The subagent's final output text.
    ///
    /// Stored as `Arc<str>` so cloning into per-consumer summaries
    /// (`SubagentCompletionSummary`, snapshot status, etc.) is a refcount
    /// bump rather than a full copy. Subagent outputs can be arbitrarily
    /// large (entire transcript), so this matters at scale.
    pub output: Arc<str>,
    /// Error message if the subagent failed.
    pub error: Option<String>,
    /// True if the subagent was cancelled (by user or model).
    /// Distinct from failure — cancellation is intentional.
    pub cancelled: bool,
    pub subagent_id: String,
    /// The child session ID (same as subagent_id for MVP).
    pub child_session_id: String,
    pub tool_calls: u32,
    pub turns: u32,
    pub duration_ms: u64,
    pub tokens_used: u64,
    pub output_tokens_used: u64,
    pub total_tokens_used: u64,
    pub output_usage_incomplete: bool,
    /// Path to the isolated worktree if one was created.
    pub worktree_path: Option<String>,
    /// Set when a blocking subagent exceeded its await budget and was
    /// auto-backgrounded: the child is still running (result via auto-wake /
    /// `get_command_or_subagent_output`), so the tool returns a `task_id` notice
    /// instead of a completion. Never set for natively backgrounded subagents.
    pub backgrounded: bool,
}

impl Default for SubagentResult {
    fn default() -> Self {
        Self {
            success: false,
            output: Arc::from(""),
            error: None,
            cancelled: false,
            subagent_id: String::new(),
            child_session_id: String::new(),
            tool_calls: 0,
            turns: 0,
            duration_ms: 0,
            tokens_used: 0,
            output_tokens_used: 0,
            total_tokens_used: 0,
            output_usage_incomplete: false,
            worktree_path: None,
            backgrounded: false,
        }
    }
}

impl SubagentResult {
    /// Terminal status string: `"cancelled"`, `"completed"`, or `"failed"`.
    pub fn status(&self) -> &'static str {
        if self.cancelled {
            "cancelled"
        } else if self.success {
            "completed"
        } else {
            "failed"
        }
    }
}

// Query protocol

/// Query sent by `TaskOutputTool` to the shared coordinator actor.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentQueryRequest {
    /// The subagent ID to look up.
    pub subagent_id: String,
    /// Restrict the lookup to children owned by this parent session.
    pub parent_session_id: Option<String>,
    /// If true, coordinator waits for completion (up to timeout) before responding.
    pub block: bool,
    /// Max wait time in ms when blocking. Default 30s.
    pub timeout_ms: Option<u64>,
    /// Oneshot for the coordinator to send back the snapshot.
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<Option<SubagentSnapshot>>,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct AgentListRequest {
    pub identity: AgentMailboxIdentity,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<ListAgentsOutput>,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct AgentMessageRequest {
    pub identity: AgentMailboxIdentity,
    pub target: String,
    pub message: AgentMailboxMessage,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<Result<AgentMessageSendOutput, String>>,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct AgentMailboxWaitRequest {
    pub identity: AgentMailboxIdentity,
    pub timeout_ms: u64,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<WaitAgentMessagesOutput>,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentLoopUnitActiveRequest {
    pub task_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<bool>,
}

/// Point-in-time snapshot of a subagent's state.
/// Returned by the coordinator in response to a `SubagentQueryRequest`.
#[derive(Debug, Clone)]
pub struct SubagentSnapshot {
    pub subagent_id: String,
    pub description: String,
    pub subagent_type: String,
    pub status: SubagentSnapshotStatus,
    /// Wall-clock start time (epoch ms).
    pub started_at_epoch_ms: u64,
    /// Elapsed wall-clock time in milliseconds.
    pub duration_ms: u64,
    /// Persona used by this subagent, if any.
    pub persona: Option<String>,
}

/// Lifecycle metadata returned to shell presentation and extension callers.
#[derive(Debug, Clone)]
pub struct SubagentInspection {
    pub snapshot: SubagentSnapshot,
    pub parent_session_id: String,
    pub child_session_id: String,
    pub fork_parent_prompt_id: Option<String>,
    pub resumed_from: Option<String>,
}

impl SubagentSnapshot {
    /// Whether the child is still in flight (initializing or running) — the
    /// shared liveness rule every driver's blocking query loops on.
    pub fn is_running(&self) -> bool {
        matches!(
            self.status,
            SubagentSnapshotStatus::Running { .. } | SubagentSnapshotStatus::Initializing
        )
    }
}

/// Status of a subagent snapshot.
#[derive(Debug, Clone)]
pub enum SubagentSnapshotStatus {
    /// Subagent is being set up (creating worktree, resolving config, spawning
    /// session). Queries during this phase should report the subagent as
    /// initializing rather than "not found".
    Initializing,
    /// Child session is still running. Fields are populated from the child
    /// session's `SessionSignals` snapshot at query time (pull-based).
    Running {
        /// Number of completed turns so far.
        turn_count: u32,
        /// Total tool calls executed so far.
        tool_call_count: u32,
        /// Current tokens used in the context window.
        tokens_used: u64,
        /// Total context window capacity (tokens).
        context_window_tokens: u64,
        /// Context window usage as a percentage (0–100).
        context_usage_pct: u8,
        /// Distinct tool names called so far (e.g. `["bash", "read_file"]`).
        tools_used: Vec<String>,
        /// Number of errors encountered so far.
        error_count: u32,
    },
    /// Child session completed successfully.
    Completed {
        output: String,
        tool_calls: u32,
        turns: u32,
        worktree_path: Option<String>,
    },
    /// Child session failed or crashed.
    Failed { error: String },
    /// Child session was cancelled (by user or model).
    Cancelled { reason: Option<String> },
}

impl SubagentSnapshotStatus {
    /// Returns `true` for terminal states: `Completed`, `Failed`, `Cancelled`.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

// Cancel protocol

#[derive(Debug, Clone)]
pub enum SubagentCancelTarget {
    SubagentId(String),
    /// Turn-scoped cancel (soft cancel / max-turns).
    ParentPromptId(String),
    /// User Stop / Esc with cancel_subagents — prior-turn background too.
    ParentSession,
    WorkflowRunId(String),
}

/// Cancel request sent by `KillTaskTool` or session cancellation paths.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentCancelRequest {
    pub parent_session_id: Option<String>,
    pub target: SubagentCancelTarget,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<SubagentCancelOutcome>,
}

#[derive(Debug, Clone)]
pub enum SubagentCancelOutcome {
    Cancelled,
    AlreadyFinished { status: String },
    NotFound,
}

/// Summary of a completed subagent, used for between-turn delivery.
/// Session ownership lives on the coordinator's `BufferedCompletion` wrapper;
/// drains are scoped there, so delivered summaries carry no owner field.
#[derive(Debug, Clone)]
pub struct SubagentCompletionSummary {
    pub subagent_id: String,
    pub subagent_type: String,
    pub description: String,
    pub success: bool,
    pub duration_ms: u64,
    pub tool_calls: u32,
    pub turns: u32,
    /// The subagent's final output text. Refcount-shared with
    /// `SubagentResult.output` (no allocation on the path from coordinator
    /// to between-turn drain).
    ///
    /// Surfaced inline in completion notifications when the parent agent's
    /// toolset has no `BackgroundTaskAction` tool. Toolsets
    /// that DO have a polling tool keep the existing metadata-only line +
    /// "Use get_task_output(...)" pointer.
    pub output: Arc<str>,
    /// Failure reason for unsuccessful completions (from
    /// `SubagentResult.error`, e.g. a spawn-time credential error). Always
    /// inlined in the completion notification, even when a polling tool
    /// exists: a child that failed before running has no output to poll, and
    /// without the reason the parent tends to silently retry on a different
    /// model instead of telling the user what is missing.
    pub error: Option<String>,
}

/// Multi-wait request: block until one or all of the listed subagents finish.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentMultiWaitRequest {
    pub subagent_ids: Vec<String>,
    pub mode: WaitMode,
    pub timeout_ms: Option<u64>,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<Vec<Option<SubagentSnapshot>>>,
}

/// Request to drain buffered completion summaries.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentCompletionsRequest {
    pub parent_session_id: Option<String>,
    pub suppress_ids: Vec<String>,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<Vec<SubagentCompletionSummary>>,
}

/// Live subagents and whether finished-subagent usage is still missing from the parent bill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentOutstandingReply {
    /// Turn-blocking (foreground) children still pending or active.
    pub live_ids: Vec<String>,
    /// A background child is still running: its spend is missing from the
    /// prompt report but reaches the session ledger when it finishes.
    pub background_live: bool,
    pub subagent_usage_not_applied: bool,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentOutstandingRequest {
    pub parent_session_id: String,
    pub prompt_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<SubagentOutstandingReply>,
}

/// Clear sticky incomplete after freeze/cancel has snapshotted the bill.
#[derive(Debug)]
pub struct SubagentClearUsageNotAppliedRequest {
    pub parent_session_id: String,
    pub prompt_id: String,
}

/// Mark sticky incomplete for a parent prompt (usage apply failed).
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentMarkUsageNotAppliedRequest {
    pub parent_session_id: String,
    pub prompt_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<()>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubagentRegistryCounts {
    pub pending: usize,
    pub active: usize,
    pub completed: usize,
    /// Spawns parked at the session's concurrent limit, not yet started.
    pub queued: usize,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentRegistryCountsRequest {
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<SubagentRegistryCounts>,
}

/// Request for full metadata plus a resolved progress snapshot.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentInspectRequest {
    pub subagent_id: String,
    pub parent_session_id: Option<String>,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<Option<SubagentInspection>>,
}

/// Request for all running children owned by one parent session.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentListRunningRequest {
    pub parent_session_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<Vec<SubagentInspection>>,
}

/// Fork/resume provenance retained by the shared coordinator.
#[derive(Debug, Clone, Default)]
pub struct SubagentProvenance {
    pub fork_parent_prompt_id: Option<String>,
    pub resumed_from: Option<String>,
}

/// Reference to a child spawned during one parent prompt.
#[derive(Debug, Clone)]
pub struct SpawnedSubagentRef {
    pub subagent_id: String,
    pub child_session_id: String,
    pub subagent_type: String,
    pub description: String,
    pub persona: Option<String>,
    pub resumed_from: Option<String>,
}

/// Request for prompt-scoped spawned-child references.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentSpawnedRefsRequest {
    pub parent_session_id: String,
    pub prompt_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<Vec<SpawnedSubagentRef>>,
}

/// In-memory source data used by a runtime adapter to resume a child.
#[derive(Debug, Clone)]
pub struct SubagentResumeSource {
    pub subagent_id: String,
    pub child_session_id: String,
    pub child_cwd: String,
    pub worktree_path: Option<String>,
    pub snapshot_ref: Option<String>,
    pub subagent_type: String,
    pub persona: Option<String>,
    pub model_id: Option<String>,
}

/// Result of a resume-source lookup.
#[derive(Debug, Clone)]
pub enum SubagentResumeLookup {
    Active,
    Completed(SubagentResumeSource),
    Missing,
}

// Validate-type protocol

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SubagentValidateTypeOutcome {
    Ok,
    /// `available` is sorted by `str::cmp` and filtered by `[subagents.toggle]`.
    Unknown {
        available: Vec<String>,
    },
    Disabled,
    NotAllowed {
        allowed: Vec<String>,
    },
    /// Coordinator unreachable; distinct from `Unknown` (the type may be valid).
    ValidationUnavailable,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentValidateTypeRequest {
    pub subagent_type: String,
    pub parent_session_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<SubagentValidateTypeOutcome>,
}

// Describe-type protocol

/// Outcome of a `describe_subagent_type` round-trip.
///
/// Mirrors [`SubagentValidateTypeOutcome`] but, on success, additionally
/// carries the resolved toolset summary (tool names + capability flags)
/// so the parent can gate per-role capability and render per-role prompts
/// WITHOUT spawning the toolset. The non-`Ok` variants are 1:1 with the
/// validate outcomes (config-bug cases) plus `Unavailable` (infra
/// flakiness), so a caller maps every variant to a fail-open reason.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SubagentDescribeOutcome {
    Ok(SubagentTypeSummary),
    /// The type does not resolve to an agent definition. `available` is
    /// sorted by `str::cmp` and filtered by `[subagents.toggle]`.
    Unknown {
        available: Vec<String>,
    },
    /// The type resolves but is not on the parent's allow-list.
    NotAllowed {
        allowed: Vec<String>,
    },
    /// The type resolves but is disabled via `[subagents.toggle]`.
    Disabled,
    /// Coordinator unreachable / responder dropped / timed out — treat as
    /// fail-open (the type may be valid; the description just could not be
    /// obtained).
    Unavailable,
}

/// Resolved toolset summary for a subagent type.
///
/// Built by the coordinator from the type's `AgentDefinition` AFTER the
/// same parent-dependent toolset re-selection a real spawn applies, so a
/// parent's described tool names match what the child would
/// actually get. The capability booleans key on the exact `ToolKind`
/// variants used by the per-role gates (`Search` for grep, `Execute` for
/// terminal/bash — there is no `Grep`/`Bash` variant).
#[derive(Debug, Clone, Default)]
pub struct SubagentTypeSummary {
    /// Client-facing tool name per [`ToolKind`](crate::types::tool::ToolKind),
    /// derived exactly like the finalize-time `kind_to_name` map:
    /// `ToolConfig::resolve_client_name(&entry.id)` (the `name_override`
    /// when set, else the unqualified tool id). First tool per kind wins,
    /// matching `FinalizedToolset`.
    pub tool_names: std::collections::HashMap<crate::types::tool::ToolKind, String>,
    /// The toolset has a [`ToolKind::Read`](crate::types::tool::ToolKind::Read) tool.
    pub can_read: bool,
    /// The toolset has a [`ToolKind::Search`](crate::types::tool::ToolKind::Search)
    /// tool (grep maps to `Search`).
    pub can_search: bool,
    /// The toolset has a [`ToolKind::Execute`](crate::types::tool::ToolKind::Execute)
    /// tool (terminal/bash maps to `Execute`).
    pub can_execute: bool,
}

#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentDescribeRequest {
    pub subagent_type: String,
    /// `/goal`-only harness override mirrored from
    /// [`SubagentRuntimeOverrides::harness_agent_type`]: the coordinator
    /// resolves the toolset for `(subagent_type, harness_agent_type)` so the
    /// per-role capability gate + prompt tool names reflect the harness the
    /// spawn will actually run on. `None` ⇒ the parent agent decides the flavor
    /// (unchanged behavior).
    pub harness_agent_type: Option<String>,
    pub parent_session_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<SubagentDescribeOutcome>,
}

/// Coordinator message enum. Kept exhaustive so every actor command is handled.
pub enum SubagentEvent {
    Spawn(SubagentSpawnRequest),
    NativeAgent(NativeAgentRequest),
    Query(SubagentQueryRequest),
    ListAgents(AgentListRequest),
    SendAgentMessage(AgentMessageRequest),
    WaitAgentMessages(AgentMailboxWaitRequest),
    Cancel(SubagentCancelRequest),
    ListActive(SubagentListActiveRequest),
    ListRunning(SubagentListRunningRequest),
    Completions(SubagentCompletionsRequest),
    /// Cancel children of `parent_session_id` and drop its buffered completions.
    /// `respond_to`, if set, resolves when no children remain (caller should
    /// time-bound the wait).
    TeardownSession {
        parent_session_id: String,
        respond_to: Option<oneshot::Sender<()>>,
    },
    /// Re-open Task spawns for a parent session after a prior ParentSession stop.
    /// Emitted at the start of each user turn so Stop's late-spawn gate does not
    /// permanently block the next prompt.
    OpenSpawnAdmission {
        parent_session_id: String,
    },
    Outstanding(SubagentOutstandingRequest),
    ClearUsageNotApplied(SubagentClearUsageNotAppliedRequest),
    MarkUsageNotApplied(SubagentMarkUsageNotAppliedRequest),
    RegistryCounts(SubagentRegistryCountsRequest),
    Inspect(SubagentInspectRequest),
    SpawnedRefs(SubagentSpawnedRefsRequest),
    ValidateType(SubagentValidateTypeRequest),
    DescribeType(SubagentDescribeRequest),
    LoopUnitActive(SubagentLoopUnitActiveRequest),
}

// Resource types

/// One shared channel to the subagent coordinator, cloned into each session.
#[derive(Clone, Educe)]
#[educe(Debug)]
pub struct SubagentEventSender(#[educe(Debug(ignore))] pub mpsc::UnboundedSender<SubagentEvent>);

register_resource!("grok_build", "SubagentEventSender", SubagentEventSender);

// Active subagent listing (compaction)

/// Lightweight summary of a running subagent.
///
/// The shared coordinator produces this through the channel protocol, and the
/// compaction pipeline consumes it through `RunningSubagentSummary`.
#[derive(Debug, Clone)]
pub struct ActiveSubagentSummary {
    /// The subagent's unique ID (same ID used by `get_task_output` / `kill_task`).
    pub subagent_id: String,
    /// The agent type name (e.g. "Explore", "general-purpose", "Plan").
    pub subagent_type: String,
    /// Human-readable description of what the subagent is doing.
    pub description: String,
    /// Wall-clock elapsed time since the subagent was spawned, in milliseconds.
    pub elapsed_ms: u64,
}

/// Request to list currently-running subagents for a specific parent session.
///
/// Sent by the compaction pipeline in `SessionActor::run_compact_inner()`.
/// Handled by the shared coordinator actor.
#[derive(Educe)]
#[educe(Debug)]
pub struct SubagentListActiveRequest {
    pub parent_session_id: String,
    #[educe(Debug(ignore))]
    pub respond_to: oneshot::Sender<Vec<ActiveSubagentSummary>>,
}

/// Current nesting depth (top-level = 0; child = parent + 1).
#[derive(Debug, Clone)]
pub struct SubagentDepthCounter(pub u32);

register_resource!("grok_build", "SubagentDepthCounter", SubagentDepthCounter);

/// Host-injected max nesting depth; absent → [`super::MAX_SUBAGENT_DEPTH`].
#[derive(Debug, Clone, Copy)]
pub struct MaxSubagentDepth(pub u32);

register_resource!("grok_build", "MaxSubagentDepth", MaxSubagentDepth);

/// Session-scoped validator for model-facing `Task.model` arguments.
///
/// Returns an error message for an invalid slug and `None` for a valid slug.
/// The closure reads the live model catalog so refreshes apply without rebuilding
/// the tool bridge.
type TaskModelValidationFn = dyn Fn(&str) -> Option<String> + Send + Sync;

#[derive(Clone)]
pub struct TaskModelValidator(Arc<TaskModelValidationFn>);

impl TaskModelValidator {
    pub fn new(validate: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(validate))
    }

    pub fn error_for(&self, model: &str) -> Option<String> {
        (self.0)(model)
    }
}

impl std::fmt::Debug for TaskModelValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskModelValidator").finish()
    }
}

register_resource!("grok_build", "TaskModelValidator", TaskModelValidator);

/// Carries the current session ID so TaskTool can set `parent_session_id`
/// on the `SubagentRequest`.
#[derive(Debug, Clone)]
pub struct SessionIdResource(pub String);

register_resource!("grok_build", "SessionIdResource", SessionIdResource);

/// Host-owned RAII token for an interruptible foreground wait.
pub trait ForegroundWaitGuard: Send {}

impl<T: Send> ForegroundWaitGuard for T {}

/// What the running turn is parked on, so the host can decide whether an
/// arriving user prompt may abort the turn.
///
/// A single foreground child (`task`) is cheap to restart, so the host aborts
/// the turn and runs the new prompt immediately. A swarm is a live cohort whose
/// members all die with the turn, so aborting would throw away in-flight work
/// the user did not ask to discard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ForegroundWaitKind {
    /// One foreground child; a user prompt may abort the turn.
    #[default]
    Interruptible,
    /// A multi-member cohort; a user prompt must reach the orchestrator without
    /// aborting the turn.
    Orchestration,
}

type ForegroundWaitFactory =
    dyn Fn(ForegroundWaitKind) -> Box<dyn ForegroundWaitGuard> + Send + Sync;

/// Factory injected by hosts that expose a send-now wait window.
#[derive(Clone)]
pub struct SubagentForegroundWait(Arc<ForegroundWaitFactory>);

impl SubagentForegroundWait {
    pub fn new(
        factory: impl Fn(ForegroundWaitKind) -> Box<dyn ForegroundWaitGuard> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(factory))
    }

    pub fn enter(&self) -> Box<dyn ForegroundWaitGuard> {
        self.enter_kind(ForegroundWaitKind::Interruptible)
    }

    pub fn enter_kind(&self, kind: ForegroundWaitKind) -> Box<dyn ForegroundWaitGuard> {
        (self.0)(kind)
    }
}

impl std::fmt::Debug for SubagentForegroundWait {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentForegroundWait").finish()
    }
}

register_resource!(
    "grok_build",
    "SubagentForegroundWait",
    SubagentForegroundWait
);

/// Host signal that a user message arrived while an orchestration wait is held.
///
/// `agent_swarm` (and `swarm_wait`) select on this to detach the cohort and
/// return control to the model without cancelling live members.
#[derive(Clone, Default)]
pub struct OrchestrationSteerSignal {
    inner: Arc<OrchestrationSteerInner>,
}

#[derive(Default)]
struct OrchestrationSteerInner {
    notify: tokio::sync::Notify,
    generation: std::sync::atomic::AtomicU64,
}

impl OrchestrationSteerSignal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generation observed before waiting; a later [`Self::fire`] advances it.
    pub fn generation(&self) -> u64 {
        self.inner
            .generation
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wake every parked orchestration wait so the cohort can detach.
    pub fn fire(&self) {
        self.inner
            .generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Resolve once [`Self::fire`] advances the generation past `seen`.
    pub async fn wait_after(&self, seen: u64) {
        loop {
            let notified = self.inner.notify.notified();
            if self.generation() > seen {
                return;
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for OrchestrationSteerSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestrationSteerSignal")
            .field("generation", &self.generation())
            .finish()
    }
}

register_resource!(
    "grok_build",
    "OrchestrationSteerSignal",
    OrchestrationSteerSignal
);

/// Carries the current parent prompt/turn ID for TaskTool subagent scoping.
///
/// Set by xai-grok-shell immediately before a prompt turn begins executing so
/// subagents launched during that turn can be cancelled together if the user
/// aborts the turn.
#[derive(Debug, Clone)]
pub struct CurrentPromptIdResource(pub String);

register_resource!(
    "grok_build",
    "CurrentPromptIdResource",
    CurrentPromptIdResource
);

/// True while a `/goal` loop is active. Set by xai-grok-shell at turn start.
/// When true, `TaskCompletionReminder` suppresses bg-task completion
/// reminders (marking them reported) so async "task completed" nudges don't
/// pull a weak model off the goal continuation (e.g. relaunching a killed
/// dev server).
#[derive(Debug, Clone, Copy, Default)]
pub struct GoalLoopActive(pub bool);

register_resource!("grok_build", "GoalLoopActive", GoalLoopActive);

/// Thread-local tracing capture for behavioral log-emission tests.
#[cfg(test)]
pub(crate) mod test_capture {
    use tokio::sync::mpsc;
    use tracing::Subscriber;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::registry::LookupSpan;

    pub(crate) struct CapturedEvent {
        pub level: tracing::Level,
        pub fields: String,
    }

    pub(crate) struct CapturedTracing {
        pub events_rx: mpsc::UnboundedReceiver<CapturedEvent>,
        _guard: tracing::subscriber::DefaultGuard,
    }

    pub(crate) fn capture() -> CapturedTracing {
        let (tx, rx) = mpsc::unbounded_channel();
        let layer = CaptureLayer { tx };
        let subscriber = tracing_subscriber::registry().with(layer);
        let guard = tracing::subscriber::set_default(subscriber);
        CapturedTracing {
            events_rx: rx,
            _guard: guard,
        }
    }

    struct CaptureLayer {
        tx: mpsc::UnboundedSender<CapturedEvent>,
    }

    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            let _ = self.tx.send(CapturedEvent {
                level: *event.metadata().level(),
                fields: visitor.out,
            });
        }
    }

    #[derive(Default)]
    struct FieldVisitor {
        out: String,
    }

    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if !self.out.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str(field.name());
            self.out.push('=');
            self.out.push_str(&format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if !self.out.is_empty() {
                self.out.push(' ');
            }
            self.out.push_str(field.name());
            self.out.push('=');
            self.out.push_str(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::registry::types::{ToolConfig, ToolServerConfig};
    use crate::types::tool::ToolKind;

    use super::SubagentCapabilityMode;
    use super::SubagentCapabilityModeExt;
    use super::is_valid_resume_id;
    use super::strip_plan_mode_tools;

    /// Create a `ToolConfig` with the given id and kind set.
    fn tc(id: &str, kind: ToolKind) -> ToolConfig {
        let mut c = ToolConfig::from_id(id);
        c.kind = Some(kind);
        c
    }

    #[test]
    fn no_capability_mode_allows_plan_mode_tools() {
        for mode in [
            SubagentCapabilityMode::ReadOnly,
            SubagentCapabilityMode::ReadWrite,
            SubagentCapabilityMode::Execute,
            SubagentCapabilityMode::All,
        ] {
            let allowed = mode.allowed_tool_kinds();
            assert!(
                !allowed.contains(&ToolKind::EnterPlan) && !allowed.contains(&ToolKind::ExitPlan),
                "{mode:?} must not allow plan-mode tools: subagent plan approvals render on the parent view"
            );
        }
    }

    #[test]
    fn no_capability_mode_allows_nested_spawn_tools() {
        for mode in [
            SubagentCapabilityMode::ReadOnly,
            SubagentCapabilityMode::ReadWrite,
            SubagentCapabilityMode::Execute,
        ] {
            let allowed = mode.allowed_tool_kinds();
            assert!(
                !allowed.contains(&ToolKind::AgentSwarm) && !allowed.contains(&ToolKind::Workflow),
                "{mode:?} must not re-grant orchestration tools to subagents"
            );
        }
    }

    #[test]
    fn strip_nested_spawn_tools_removes_every_spawn_surface() {
        let mut config = ToolServerConfig {
            tools: vec![
                tc("GrokBuild:read_file", ToolKind::Read),
                tc("GrokBuild:task", ToolKind::Task),
                tc("GrokBuild:agent_swarm", ToolKind::AgentSwarm),
                tc("GrokBuild:workflow", ToolKind::Workflow),
                tc("GrokBuild:send_message", ToolKind::AgentCollaboration),
                tc("GrokBuild:get_task_output", ToolKind::BackgroundTaskAction),
                tc("GrokBuild:kill_task", ToolKind::KillTaskAction),
                ToolConfig::from_id("Mcp:custom_tool"),
            ],
            behavior_preset: None,
        };

        super::strip_nested_spawn_tools(&mut config);

        let ids: Vec<&str> = config.tools.iter().map(|tc| tc.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "GrokBuild:read_file",
                "GrokBuild:send_message",
                "Mcp:custom_tool"
            ],
            "spawn tools must go, mailbox collaboration must remain, and orphaned background-task lifecycle tools go"
        );
    }

    /// `task`, `agent_swarm`, and `workflow` are three separate model surfaces
    /// built on the same subagent machinery. They must stay separately
    /// addressable, and a single subagent must stand alone: `task`'s
    /// requirements hold with neither orchestrator in the toolset.
    #[test]
    fn task_agent_swarm_and_workflow_stay_three_distinct_surfaces() {
        use crate::implementations::grok_build::{AgentSwarmTool, TaskTool, WorkflowTool};
        use crate::types::requirements::{EvalContext, ProposedTool};
        use crate::types::tool_metadata::ToolMetadata;
        use xai_tool_runtime::Tool;

        assert_eq!(
            [
                Tool::id(&TaskTool).as_str().to_string(),
                Tool::id(&AgentSwarmTool).as_str().to_string(),
                Tool::id(&WorkflowTool).as_str().to_string(),
            ],
            ["task", "agent_swarm", "workflow"],
            "each orchestration surface keeps its own stable tool id"
        );
        assert_eq!(
            [
                ToolMetadata::kind(&TaskTool),
                ToolMetadata::kind(&AgentSwarmTool),
                ToolMetadata::kind(&WorkflowTool),
            ],
            [ToolKind::Task, ToolKind::AgentSwarm, ToolKind::Workflow],
            "each surface keeps its own ToolKind so packs and Code Mode can gate them apart"
        );

        // A lone subagent surface: `task` plus the background lifecycle tools it
        // needs to manage what it spawns. No swarm, no workflow.
        let params = serde_json::Value::Object(Default::default());
        let lone_subagent = ["task", "get_task_output", "kill_task"];
        let kinds = [
            ToolKind::Task,
            ToolKind::BackgroundTaskAction,
            ToolKind::KillTaskAction,
        ];
        let proposed: Vec<ProposedTool> = lone_subagent
            .iter()
            .zip(kinds)
            .map(|(id, kind)| ProposedTool {
                namespace: "GrokBuild",
                id,
                kind,
                params: &params,
                input_schema: None,
            })
            .collect();
        let ctx = EvalContext {
            tools: &proposed,
            self_params: &params,
        };
        assert!(
            ToolMetadata::requires_expr(&TaskTool).eval(&|req| req.eval(&ctx)),
            "an individual subagent must be usable without agent_swarm or workflow"
        );
        assert!(
            ToolMetadata::requires_expr(&AgentSwarmTool).eval(&|req| req.eval(&ctx)),
            "agent_swarm layers on the task surface rather than replacing it"
        );
    }

    #[test]
    fn strip_nested_spawn_tools_keeps_background_helpers_for_bg_capable_bash() {
        let mut bash = ToolConfig::from_id("GrokBuild:run_terminal_cmd");
        bash.kind = Some(ToolKind::Execute);
        let mut config = ToolServerConfig {
            tools: vec![
                bash,
                tc("GrokBuild:workflow", ToolKind::Workflow),
                tc("GrokBuild:get_task_output", ToolKind::BackgroundTaskAction),
            ],
            behavior_preset: None,
        };

        super::strip_nested_spawn_tools(&mut config);

        let ids: Vec<&str> = config.tools.iter().map(|tc| tc.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["GrokBuild:run_terminal_cmd", "GrokBuild:get_task_output"],
            "background-capable bash keeps its output helper; workflow still goes"
        );
    }

    #[test]
    fn strip_plan_mode_tools_removes_only_plan_mode_tools() {
        let mut config = ToolServerConfig {
            tools: vec![
                tc("GrokBuild:read_file", ToolKind::Read),
                tc("GrokBuild:enter_plan_mode", ToolKind::EnterPlan),
                tc("GrokBuild:exit_plan_mode", ToolKind::ExitPlan),
                tc("GrokBuild:grep", ToolKind::Search),
                ToolConfig::from_id("Mcp:custom_tool"),
            ],
            behavior_preset: None,
        };

        strip_plan_mode_tools(&mut config);

        let ids: Vec<&str> = config.tools.iter().map(|tc| tc.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["GrokBuild:read_file", "GrokBuild:grep", "Mcp:custom_tool"]
        );
    }

    #[test]
    fn read_only_filter_prunes_orphaned_background_task_tools() {
        let mut config = ToolServerConfig {
            tools: vec![
                tc("GrokBuild:run_terminal_cmd", ToolKind::Execute),
                tc("GrokBuild:read_file", ToolKind::Read),
                tc("GrokBuild:list_dir", ToolKind::List),
                tc("GrokBuild:grep", ToolKind::Search),
                tc("GrokBuild:kill_task", ToolKind::KillTaskAction),
                tc("GrokBuild:get_task_output", ToolKind::BackgroundTaskAction),
            ],
            behavior_preset: None,
        };

        SubagentCapabilityMode::ReadOnly.filter_tool_config(&mut config);

        let ids: Vec<&str> = config.tools.iter().map(|tc| tc.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "GrokBuild:read_file",
                "GrokBuild:list_dir",
                "GrokBuild:grep",
            ]
        );
    }

    #[test]
    fn read_only_filter_keeps_background_task_tools_when_task_tool_remains() {
        let mut config = ToolServerConfig {
            tools: vec![
                tc("GrokBuild:run_terminal_cmd", ToolKind::Execute),
                tc("GrokBuild:read_file", ToolKind::Read),
                tc("GrokBuild:list_dir", ToolKind::List),
                tc("GrokBuild:grep", ToolKind::Search),
                tc("GrokBuild:kill_task", ToolKind::KillTaskAction),
                tc("GrokBuild:get_task_output", ToolKind::BackgroundTaskAction),
                tc("GrokBuild:task", ToolKind::Task),
            ],
            behavior_preset: None,
        };

        SubagentCapabilityMode::ReadOnly.filter_tool_config(&mut config);

        let ids: Vec<&str> = config.tools.iter().map(|tc| tc.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "GrokBuild:read_file",
                "GrokBuild:list_dir",
                "GrokBuild:grep",
                "GrokBuild:kill_task",
                "GrokBuild:get_task_output",
                "GrokBuild:task",
            ]
        );
    }

    #[test]
    fn capability_modes_include_lsp_kind() {
        use crate::types::tool::ToolKind;

        for mode in [
            SubagentCapabilityMode::ReadOnly,
            SubagentCapabilityMode::ReadWrite,
            SubagentCapabilityMode::Execute,
            SubagentCapabilityMode::All,
        ] {
            assert!(
                mode.allowed_tool_kinds().contains(&ToolKind::Lsp),
                "{mode:?} should preserve ToolKind::Lsp"
            );
        }
    }

    #[test]
    fn read_write_filter_keeps_background_capable_bash_when_explicitly_enabled() {
        let mut bash = tc("GrokBuild:run_terminal_cmd", ToolKind::Execute);
        bash.params = Some(
            serde_json::json!({ "enabled_background": true })
                .as_object()
                .unwrap()
                .clone(),
        );
        let mut config = ToolServerConfig {
            tools: vec![bash],
            behavior_preset: None,
        };

        SubagentCapabilityMode::ReadWrite.filter_tool_config(&mut config);

        assert!(
            config.tools.is_empty(),
            "execute tools should still be filtered out"
        );
    }

    #[test]
    fn is_valid_resume_id_rejects_sentinels() {
        for bad in [
            "",
            "  ",
            "null",
            "Null",
            "NULL",
            "none",
            "None",
            "NONE",
            "undefined",
            "  null  ",
        ] {
            assert!(!is_valid_resume_id(bad), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn is_valid_resume_id_accepts_real_ids() {
        for good in ["019e0000-0000-7000-8000-0000000000bb", "abc-123", "prev-id"] {
            assert!(is_valid_resume_id(good), "{good:?} should be valid");
        }
    }

    #[test]
    fn sanitize_cwd_value_strips_unmatched_leading_quote() {
        // Regression: stray leading double-quote from a model-emitted path.
        assert_eq!(
            super::sanitize_cwd_value("\"/Users/dev/work/project"),
            Some("/Users/dev/work/project".to_string()),
        );
    }

    #[test]
    fn sanitize_cwd_value_strips_matched_quotes() {
        assert_eq!(
            super::sanitize_cwd_value("\"/tmp\""),
            Some("/tmp".to_string())
        );
        assert_eq!(
            super::sanitize_cwd_value("'/tmp'"),
            Some("/tmp".to_string())
        );
        assert_eq!(
            super::sanitize_cwd_value("`/tmp`"),
            Some("/tmp".to_string())
        );
    }

    #[test]
    fn sanitize_cwd_value_strips_unmatched_trailing_quote() {
        assert_eq!(
            super::sanitize_cwd_value("/tmp\""),
            Some("/tmp".to_string())
        );
    }

    #[test]
    fn sanitize_cwd_value_rejects_sentinels() {
        for sentinel in ["", "  ", "null", "Null", "NONE", "undefined", "  null  "] {
            assert_eq!(
                super::sanitize_cwd_value(sentinel),
                None,
                "sentinel {sentinel:?} should be None",
            );
        }
    }

    #[test]
    fn sanitize_cwd_value_rejects_quoted_sentinels() {
        for input in ["\"null\"", "'none'", "`undefined`", "\"\"", "''", "``"] {
            assert_eq!(
                super::sanitize_cwd_value(input),
                None,
                "quoted sentinel {input:?} should be None",
            );
        }
    }

    #[test]
    fn sanitize_cwd_value_trims_whitespace_inside_quotes() {
        assert_eq!(
            super::sanitize_cwd_value("\"  /tmp  \""),
            Some("/tmp".to_string()),
        );
    }

    #[test]
    fn sanitize_cwd_value_preserves_clean_paths() {
        assert_eq!(super::sanitize_cwd_value("/tmp"), Some("/tmp".to_string()));
        assert_eq!(
            super::sanitize_cwd_value("/Users/me/project"),
            Some("/Users/me/project".to_string()),
        );
    }

    #[test]
    fn sanitize_cwd_value_expands_tilde() {
        let expected = shellexpand::tilde("~/foo").into_owned();
        let got = super::sanitize_cwd_value("~/foo").expect("should sanitize");
        assert_eq!(got, expected);
        // If we have a real home dir, it should no longer start with `~`.
        if expected != "~/foo" {
            assert!(!got.starts_with('~'), "tilde should be expanded: {got:?}");
        }
    }

    #[test]
    fn sanitize_cwd_value_keeps_inner_quotes() {
        let input = "/path with \"quote\" inside/";
        assert_eq!(super::sanitize_cwd_value(input), Some(input.to_string()));
    }

    #[test]
    fn sanitize_cwd_value_is_idempotent() {
        for input in [
            "\"/tmp",
            "'/tmp'",
            "  /tmp  ",
            "/tmp",
            "~/foo",
            "/path with \"quote\" inside/",
        ] {
            let once = super::sanitize_cwd_value(input);
            let twice = once.as_deref().and_then(super::sanitize_cwd_value);
            assert_eq!(once, twice, "not idempotent for {input:?}");
        }
    }

    #[test]
    fn is_terminal_returns_true_for_completed() {
        let status = super::SubagentSnapshotStatus::Completed {
            output: "done".into(),
            tool_calls: 1,
            turns: 1,
            worktree_path: None,
        };
        assert!(status.is_terminal());
    }

    #[test]
    fn is_terminal_returns_true_for_failed() {
        let status = super::SubagentSnapshotStatus::Failed {
            error: "boom".into(),
        };
        assert!(status.is_terminal());
    }

    #[test]
    fn is_terminal_returns_true_for_cancelled() {
        let status = super::SubagentSnapshotStatus::Cancelled {
            reason: Some("user".into()),
        };
        assert!(status.is_terminal());
    }

    #[test]
    fn is_terminal_returns_false_for_running() {
        let status = super::SubagentSnapshotStatus::Running {
            turn_count: 0,
            tool_call_count: 0,
            tokens_used: 0,
            context_window_tokens: 0,
            context_usage_pct: 0,
            tools_used: vec![],
            error_count: 0,
        };
        assert!(!status.is_terminal());
    }

    #[test]
    fn is_terminal_returns_false_for_initializing() {
        let status = super::SubagentSnapshotStatus::Initializing;
        assert!(!status.is_terminal());
    }

    #[test]
    fn wait_mode_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&super::WaitMode::WaitAny).unwrap(),
            "\"wait_any\""
        );
        assert_eq!(
            serde_json::to_string(&super::WaitMode::WaitAll).unwrap(),
            "\"wait_all\""
        );
    }

    #[test]
    fn wait_mode_deserializes_from_snake_case() {
        let any: super::WaitMode = serde_json::from_str("\"wait_any\"").unwrap();
        assert!(matches!(any, super::WaitMode::WaitAny));
        let all: super::WaitMode = serde_json::from_str("\"wait_all\"").unwrap();
        assert!(matches!(all, super::WaitMode::WaitAll));
    }

    #[test]
    fn wait_mode_rejects_unknown_variant() {
        let result = serde_json::from_str::<super::WaitMode>("\"wait_first\"");
        assert!(result.is_err());
    }

    #[test]
    fn wait_mode_json_schema_has_two_variants() {
        let schema = schemars::schema_for!(super::WaitMode);
        let json = serde_json::to_string(&schema).unwrap();
        assert!(json.contains("wait_any"));
        assert!(json.contains("wait_all"));
    }

    #[test]
    fn completions_request_round_trips_through_channel() {
        use tokio::sync::{mpsc, oneshot};

        let (tx, mut rx) = mpsc::unbounded_channel::<super::SubagentCompletionsRequest>();
        let (respond_to, mut response_rx) = oneshot::channel();

        tx.send(super::SubagentCompletionsRequest {
            parent_session_id: Some("parent".into()),
            suppress_ids: vec!["id-1".into(), "id-2".into()],
            respond_to,
        })
        .unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(req.suppress_ids, vec!["id-1", "id-2"]);

        let summaries = vec![super::SubagentCompletionSummary {
            subagent_id: "sub-1".into(),
            subagent_type: "general-purpose".into(),
            description: "test task".into(),
            success: true,
            duration_ms: 1500,
            tool_calls: 7,
            turns: 3,
            output: std::sync::Arc::from("subagent answer"),
            error: None,
        }];
        req.respond_to.send(summaries).unwrap();

        let result = response_rx.try_recv().unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].subagent_id, "sub-1");
        assert!(result[0].success);
        assert_eq!(result[0].duration_ms, 1500);
        assert_eq!(result[0].tool_calls, 7);
        assert_eq!(result[0].turns, 3);
    }

    #[test]
    fn multi_wait_request_round_trips_through_channel() {
        use tokio::sync::{mpsc, oneshot};

        let (tx, mut rx) = mpsc::unbounded_channel::<super::SubagentMultiWaitRequest>();
        let (respond_to, mut response_rx) = oneshot::channel();

        tx.send(super::SubagentMultiWaitRequest {
            subagent_ids: vec!["a".into(), "b".into()],
            mode: super::WaitMode::WaitAll,
            timeout_ms: Some(5000),
            respond_to,
        })
        .unwrap();

        let req = rx.try_recv().unwrap();
        assert_eq!(req.subagent_ids, vec!["a", "b"]);
        assert!(matches!(req.mode, super::WaitMode::WaitAll));
        assert_eq!(req.timeout_ms, Some(5000));

        // Simulate coordinator response: one found, one not
        let snapshots = vec![
            Some(super::SubagentSnapshot {
                subagent_id: "a".into(),
                description: "task a".into(),
                subagent_type: "general-purpose".into(),
                status: super::SubagentSnapshotStatus::Completed {
                    output: "done".into(),
                    tool_calls: 3,
                    turns: 1,
                    worktree_path: None,
                },
                started_at_epoch_ms: 1000,
                duration_ms: 500,
                persona: None,
            }),
            None,
        ];
        req.respond_to.send(snapshots).unwrap();

        let result = response_rx.try_recv().unwrap();
        assert_eq!(result.len(), 2);
        assert!(result[0].is_some());
        assert!(result[0].as_ref().unwrap().status.is_terminal());
        assert!(result[1].is_none());
    }

    #[test]
    fn event_sender_wraps_channel_and_delivers_completions() {
        use tokio::sync::{mpsc, oneshot};

        let (tx, mut rx) = mpsc::unbounded_channel::<super::SubagentEvent>();
        let sender = super::SubagentEventSender(tx);

        let (respond_to, mut response_rx) = oneshot::channel();
        sender
            .0
            .send(super::SubagentEvent::Completions(
                super::SubagentCompletionsRequest {
                    parent_session_id: None,
                    suppress_ids: vec![],
                    respond_to,
                },
            ))
            .unwrap();

        let event = rx.try_recv().unwrap();
        let req = match event {
            super::SubagentEvent::Completions(r) => r,
            _ => panic!("Expected Completions, got different variant"),
        };
        assert!(req.suppress_ids.is_empty());
        req.respond_to.send(vec![]).unwrap();

        let result = response_rx.try_recv().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn event_sender_is_clone() {
        use tokio::sync::mpsc;

        let (tx, _rx) = mpsc::unbounded_channel::<super::SubagentEvent>();
        let sender = super::SubagentEventSender(tx);
        let cloned = sender.clone();
        // Both clones should be able to send
        let (respond_to, _) = tokio::sync::oneshot::channel();
        cloned
            .0
            .send(super::SubagentEvent::Completions(
                super::SubagentCompletionsRequest {
                    parent_session_id: None,
                    suppress_ids: vec![],
                    respond_to,
                },
            ))
            .unwrap();
    }
}
