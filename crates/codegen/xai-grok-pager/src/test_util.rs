//! Shared test utilities for the pager crate.
//!
//! Compiled only in `#[cfg(test)]` builds. Import via `crate::test_util`.
/// Minimal `AgentView` for unit tests outside the dispatch/handler modules
/// (which keep their own richer factories).
pub fn make_agent_view(session_id: Option<&str>, cwd: &str) -> crate::app::agent_view::AgentView {
    use crate::app::agent::{AgentId, AgentSession, AgentState};
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let session = AgentSession {
        id: AgentId(0),
        acp_tx: tx,
        session_id: session_id.map(agent_client_protocol::SessionId::new),
        models: crate::acp::model_state::ModelState::default(),
        state: AgentState::Idle,
        tracker: crate::acp::tracker::AcpUpdateTracker::new(),
        cwd: std::path::PathBuf::from(cwd),
        is_worktree: false,
        forked_from: None,
        pending_prompts: std::collections::VecDeque::new(),
        next_queue_id: 0,
        yolo_mode: false,
        auto_mode: false,
        prompt_history: Vec::new(),
        prompt_history_loading: false,
        loading_replay: false,
        restore_degree: None,
        rate_limited: false,
        model_incompatible: false,
        credit_limit_blocked: false,
        free_usage_blocked: false,
        available_commands: Vec::new(),
        available_commands_generation: 0,
        available_tools: None,
        model_switch_pending: false,
        user_model_preference: None,
        deferred_model_switch: None,
        bg_tasks: std::collections::BTreeMap::new(),
        bg_tool_call_to_task: std::collections::HashMap::new(),
        scheduled_tasks: std::collections::HashMap::new(),
        in_flight_prompt: None,
        current_prompt_id: None,
        created_via_new: false,
    };
    crate::app::agent_view::AgentView::new(
        session,
        crate::scrollback::state::ScrollbackState::new(),
    )
}
/// RAII guard for temporarily overriding an environment variable.
///
/// Captures the original value on construction and restores it on drop.
/// Used by theme and persist tests to redirect `HOME`/`USERPROFILE` to
/// temp directories without affecting the real user config.
pub struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}
impl EnvVarGuard {
    /// Override `key` to `value` (paths, URLs, flags — anything OsStr-able),
    /// returning a guard that restores the original on drop.
    pub fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}
impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(value) = &self.original {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }
}
