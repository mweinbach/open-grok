//! Session context — legacy name "ToolContext".
//!
//! This struct holds session-level state (cwd, gateway, filesystem, hunk tracker, etc.)
//! that the session actor needs for non-tool operations (ACP communication, git, rewind, etc.).
//!
//! Tool execution goes through the ToolBridge, which has its own SessionContext from
//! xai-grok-tools. This struct is NOT used for tool execution — it's session infrastructure.
//!
//! Note: Could be renamed to `SessionConfig` or flattened onto `SessionActor` in a future PR.
use crate::terminal::AsyncTerminalRunner;
use agent_client_protocol as acp;
use std::collections::HashMap;
use std::sync::Arc;
use xai_acp_lib::AcpAgentGatewaySender as GatewaySender;
use xai_grok_paths::AbsPathBuf;
use xai_grok_workspace::file_system::{AsyncFileSystem, AsyncFsWrapper};
use xai_grok_workspace::session::file_state::FileStateHandle;
use xai_hunk_tracker::HunkTrackerHandle;
/// RAII marker: the turn is blocked inside an interruptible wait. Increments
/// [`ToolContext::blocking_wait_depth`] for its lifetime; `Drop` decrements
/// (a cancelled turn can't leak the count).
pub(crate) struct BlockingWaitGuard(Arc<std::sync::atomic::AtomicUsize>);
impl BlockingWaitGuard {
    pub(crate) fn enter(depth: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        depth.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(depth)
    }
}
impl Drop for BlockingWaitGuard {
    fn drop(&mut self) {
        let _ = self.0.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |depth| Some(depth.saturating_sub(1)),
        );
    }
}
/// Session-level context. NOT used for tool execution (bridge handles that).
/// Holds ACP gateway, cwd, hunk tracker, etc. for session infrastructure.
#[derive(Clone)]
pub struct ToolContext {
    pub gateway: Option<GatewaySender>,
    pub session_id: Option<acp::SessionId>,
    pub fs: AsyncFsWrapper,
    pub terminal: Arc<dyn AsyncTerminalRunner>,
    pub cwd: AbsPathBuf,
    pub file_state_handle: Option<FileStateHandle>,
    pub session_env: Arc<HashMap<String, String>>,
    pub hunk_tracker_handle: HunkTrackerHandle,
    /// Whether hunk tracking is active for this session. `false` when the
    /// resolved mode is `off`/`disabled` — `hunk_tracker_handle` is then a
    /// `noop()` and the fs-notify loop skips forwarding to avoid per-event cost.
    pub hunk_tracking_enabled: bool,
    pub prompt_index: Arc<tokio::sync::Mutex<usize>>,
    /// Current subagent nesting depth for this session.
    /// Top-level sessions start at 0; child sessions are parent_depth + 1.
    pub subagent_depth: u32,
    /// Unified subagent event sender — carries spawn, query, cancel,
    /// list-active, completions, and outstanding messages to the coordinator.
    /// `None` if subagent support is not enabled.
    pub subagent_event_tx: Option<
        tokio::sync::mpsc::UnboundedSender<
            xai_grok_tools::implementations::grok_build::task::types::SubagentEvent,
        >,
    >,
    /// Shared LSP runtime — cloned cheaply (Arc) from parent to child.
    /// Same pattern as `fs` and `terminal`.
    pub lsp: Option<Arc<dyn xai_grok_tools::implementations::lsp::LspBackend>>,
    /// LSP server names snapshot from session creation (not updated mid-session).
    pub lsp_server_names: Vec<String>,
    /// Shared turn-active flag — set `true` at turn start, `false` at turn end.
    /// Used by the between-turn completion drain in `handle_prompt`.
    pub is_turn_active: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Shared buffer for mid-turn monitor event notifications.
    /// Events pushed here are drained by the session turn loop
    /// (`inject_pending_monitor_events`) and surfaced as ONE hidden
    /// synthetic user message before the next sampling step.
    pub monitor_event_buffer:
        Option<xai_grok_tools::implementations::grok_build::task::types::MonitorEventBuffer>,
    /// Shared set of IDs delivered via auto-wake synthetic prompts.
    /// Used by `TaskCompletionReminder` to suppress duplicate reminders.
    pub auto_wake_delivered:
        Option<xai_grok_tools::reminders::task_completion::AutoWakeDeliveredIds>,
    /// Channel for requesting trace uploads for synthetic auto-wake turns.
    pub(crate) synthetic_trace_tx:
        Option<tokio::sync::mpsc::UnboundedSender<crate::upload::turn::SyntheticTurnTraceRequest>>,
    /// Shared slot for the synthetic trace channel. Populated by
    /// `start_subagent_coordinator` after the notification bridge is spawned.
    /// The notification bridge reads from this slot on each completion event.
    pub(crate) synthetic_trace_tx_shared: Option<
        std::sync::Arc<
            std::sync::Mutex<
                Option<
                    tokio::sync::mpsc::UnboundedSender<
                        crate::upload::turn::SyntheticTurnTraceRequest,
                    >,
                >,
            >,
        >,
    >,
    /// Resolved name of the `BackgroundTaskAction` tool in the current toolset.
    /// Used by auto-wake to format completion messages with the correct tool name.
    pub task_output_tool_name: String,
    /// Whether auto-wake is enabled. When `false`, background task and subagent
    /// completions fall back to the idle-gated notification drain.
    pub auto_wake_enabled: bool,
    /// When set, bash + subagent auto-wake synthetic prompts are suppressed.
    /// Shared `Arc` written at one chokepoint — see
    /// `SessionActor::set_goal_loop_active_resource` for the rationale.
    pub goal_loop_active_gate: Arc<std::sync::atomic::AtomicBool>,
    /// Count of interruptible blocking waits the running turn is parked in (via
    /// [`BlockingWaitGuard`]). `queue_input` reads it: a prompt arriving while
    /// non-zero takes the send-now path.
    pub blocking_wait_depth: Arc<std::sync::atomic::AtomicUsize>,
}
impl ToolContext {
    pub fn new(
        cwd: AbsPathBuf,
        gateway: Option<GatewaySender>,
        session_id: Option<acp::SessionId>,
        fs: Arc<dyn AsyncFileSystem>,
        terminal: Arc<dyn AsyncTerminalRunner>,
        hunk_tracker_handle: HunkTrackerHandle,
    ) -> Self {
        let session_env = xai_grok_workspace::envrc::load_envrc_or_empty_when_trusted(
            cwd.as_path(),
            crate::agent::folder_trust::project_scope_allowed(cwd.as_path()),
        );
        Self {
            gateway,
            session_id,
            fs: AsyncFsWrapper::new(fs),
            terminal,
            cwd,
            file_state_handle: None,
            session_env: Arc::new(session_env),
            hunk_tracker_handle,
            hunk_tracking_enabled: true,
            prompt_index: Arc::new(tokio::sync::Mutex::new(0)),
            subagent_depth: 0,
            subagent_event_tx: None,
            lsp: None,
            lsp_server_names: Vec::new(),
            is_turn_active: None,
            monitor_event_buffer: None,
            auto_wake_delivered: None,
            synthetic_trace_tx: None,
            synthetic_trace_tx_shared: None,
            task_output_tool_name:
                xai_grok_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL.to_string(),
            auto_wake_enabled: true,
            goal_loop_active_gate: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            blocking_wait_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
    pub fn with_preloaded_env(
        cwd: AbsPathBuf,
        gateway: Option<GatewaySender>,
        session_id: Option<acp::SessionId>,
        fs: Arc<dyn AsyncFileSystem>,
        terminal: Arc<dyn AsyncTerminalRunner>,
        hunk_tracker_handle: HunkTrackerHandle,
        session_env: HashMap<String, String>,
    ) -> Self {
        Self {
            gateway,
            session_id,
            fs: AsyncFsWrapper::new(fs),
            terminal,
            cwd,
            file_state_handle: None,
            session_env: Arc::new(session_env),
            hunk_tracker_handle,
            hunk_tracking_enabled: true,
            prompt_index: Arc::new(tokio::sync::Mutex::new(0)),
            subagent_depth: 0,
            subagent_event_tx: None,
            lsp: None,
            lsp_server_names: Vec::new(),
            is_turn_active: None,
            monitor_event_buffer: None,
            auto_wake_delivered: None,
            synthetic_trace_tx: None,
            synthetic_trace_tx_shared: None,
            task_output_tool_name:
                xai_grok_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL.to_string(),
            auto_wake_enabled: true,
            goal_loop_active_gate: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            blocking_wait_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }
    pub fn with_file_state_handle(mut self, handle: FileStateHandle) -> Self {
        self.file_state_handle = Some(handle);
        self
    }
    pub fn with_prompt_index(mut self, prompt_index: Arc<tokio::sync::Mutex<usize>>) -> Self {
        self.prompt_index = prompt_index;
        self
    }
    /// Set whether hunk tracking is active. `false` pairs with a `noop()`
    /// `hunk_tracker_handle` so the fs-notify loop skips the per-event forward.
    pub fn with_hunk_tracking_enabled(mut self, enabled: bool) -> Self {
        self.hunk_tracking_enabled = enabled;
        self
    }
}
#[cfg(test)]
mod tests {
    use crate::{terminal::AsyncTerminalRunner, tools::ToolContext};
    use std::collections::HashMap;
    use std::sync::Arc;
    use xai_grok_paths::AbsPathBuf;
    use xai_grok_workspace::file_system::{AsyncFileSystem, AsyncFsWrapper};
    use xai_hunk_tracker::HunkTrackerHandle;
    impl ToolContext {
        pub fn new_local_context(
            cwd: AbsPathBuf,
            fs: Arc<dyn AsyncFileSystem>,
            terminal: Arc<dyn AsyncTerminalRunner>,
        ) -> Self {
            Self {
                gateway: None,
                session_id: None,
                fs: AsyncFsWrapper::new(fs),
                terminal,
                cwd,
                file_state_handle: None,
                session_env: Arc::new(HashMap::new()),
                hunk_tracker_handle: HunkTrackerHandle::noop(),
                hunk_tracking_enabled: true,
                prompt_index: Arc::new(tokio::sync::Mutex::new(0)),
                subagent_depth: 0,
                subagent_event_tx: None,
                lsp: None,
                lsp_server_names: Vec::new(),
                is_turn_active: None,
                monitor_event_buffer: None,
                auto_wake_delivered: None,
                synthetic_trace_tx: None,
                synthetic_trace_tx_shared: None,
                task_output_tool_name:
                    xai_grok_tools::reminders::task_completion::DEFAULT_TASK_OUTPUT_TOOL.to_string(),
                auto_wake_enabled: true,
                goal_loop_active_gate: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                blocking_wait_depth: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }
}
