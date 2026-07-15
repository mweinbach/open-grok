//! Contains the various kind of notifications which can be sent by the tools
//! This is used to talk to the wider systems which integrate with this crate
//! notifications can be of many types:
//! - updates being sent by the tools as they are executing (for example bash tools)

use std::path::PathBuf;
use std::sync::Arc;

use crate::types::TaskSnapshot;

/// Common fields for all bash execution notifications.
/// Extracting these ensures consistent naming and makes refactoring easier.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BashNotificationBase {
    /// The tool call ID (used to correlate with the tool call in TUI)
    pub tool_call_id: String,

    /// The command being executed
    pub command: String,

    /// Output bytes (may be truncated if exceeds limit).
    /// Use `output_lossy()` for string conversion.
    ///
    /// Serialized as base64; see `crate::util::serde_base64` for the wire format
    /// and deploy ordering.
    #[cfg_attr(feature = "serde", serde(with = "crate::util::serde_base64"))]
    // Wire form is a base64 string, not a byte array, so advertise `String`.
    #[schemars(with = "String")]
    pub output: Vec<u8>,

    /// Total bytes of output received (before any truncation)
    pub total_bytes: usize,

    /// Whether the output was truncated due to size limits
    pub truncated: bool,

    /// Working directory where command is running
    pub cwd: PathBuf,
}

impl BashNotificationBase {
    /// Lossy UTF-8 conversion of the raw `output` bytes.
    ///
    /// Bytes that are not valid UTF-8 (e.g. a delta that begins or ends
    /// mid–multi-byte sequence) are replaced with the Unicode replacement
    /// character. Suitable for human-readable log display.
    pub fn output_lossy(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }
}

/// A chunk of output streamed from a bash command execution.
/// Sent periodically during execution when streaming is enabled.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BashOutputChunk {
    /// Common notification fields
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub base: BashNotificationBase,
}

/// Notification that a bash command execution completed.
/// Sent when the process exits (with or without error).
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BashExecutionComplete {
    /// Common notification fields
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub base: BashNotificationBase,

    /// Exit code (None if killed by signal before exit)
    pub exit_code: Option<i32>,

    /// Signal that terminated the process (e.g., "SIGKILL", "SIGTERM")
    /// None if process exited normally
    pub signal: Option<String>,
}

impl BashExecutionComplete {
    /// Returns true if the process was killed by a signal
    pub fn was_signaled(&self) -> bool {
        self.signal.is_some()
    }
}

/// Notification that a bash command execution timed out.
/// Sent when the command exceeds the configured timeout and is killed.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BashExecutionTimeout {
    /// Common notification fields (output contains partial data captured before timeout)
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub base: BashNotificationBase,

    /// How long the command ran before being killed
    pub elapsed: std::time::Duration,

    /// The configured timeout that was exceeded
    pub timeout: std::time::Duration,
}

/// Notification that a bash command was moved to background.
/// Sent when user backgrounds a running command or when is_background=true.
///
/// NOTE: This is the final notification from the tool layer. The background
/// task monitor will send BashExecutionComplete when the process exits.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BashExecutionBackgrounded {
    /// Common notification fields (output contains data captured before backgrounding)
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub base: BashNotificationBase,

    /// Path to the output file where full output is being written.
    /// Background tasks always write to file for later retrieval.
    pub output_file: PathBuf,

    /// Task ID for background task registry.
    ///
    /// This is different from `tool_call_id`:
    /// - `tool_call_id` (in base): Correlates with the original tool call in TUI
    /// - `task_id`: Used with `get_task_output` tool to query status later
    ///
    /// They are always different because task_id is generated when backgrounding,
    /// while tool_call_id was assigned when the tool was invoked.
    pub task_id: String,

    /// When `Some`, this backgrounded task is a **monitor** (not an ordinary
    /// bash command), and the string is the monitor's human-readable
    /// description (e.g. "errors in deploy.log"). Consumers (the pager) use
    /// it both as the display label and as the signal to tag the row as a
    /// monitor rather than syntax-highlighting the command. `None` for
    /// ordinary backgrounded commands.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub monitor_description: Option<String>,

    /// Human-readable description from the tool call (e.g. model-supplied
    /// `description` on `run_terminal_command`). Used by the pager for
    /// "Task started: …" / tasks-pane labels instead of the raw command.
    /// `None` only on legacy paths that never had a model description
    /// (e.g. reparented monitors).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub description: Option<String>,
}

/// Notification that a bash command failed to execute.
/// Sent when the command cannot be spawned or encounters an I/O error.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BashExecutionFailed {
    /// The tool call ID
    pub tool_call_id: String,

    /// The command that failed
    pub command: String,

    /// Working directory where command was attempted
    pub cwd: PathBuf,

    /// Error message describing what went wrong
    pub error: String,
}

/// Notification that a tool read a file.
/// Emitted by ReadFile after successfully reading a file.
/// Consumers can use this for rewind tracking (capturing file state for accessed files).
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileRead {
    /// The tool call ID (correlates with the tool invocation)
    pub tool_call_id: String,

    /// Absolute path to the file that was read
    pub absolute_path: PathBuf,
}

/// Notification that a tool wrote a file.
/// Emitted by SearchReplace after write_file() so consumers can
/// track agent writes (e.g., for hunk tracking, audit logging).
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileWritten {
    /// The tool call ID (correlates with the tool invocation)
    pub tool_call_id: String,

    /// Absolute path to the file that was written
    pub absolute_path: PathBuf,

    /// Full file content after the write.
    /// For new file creation: the entire new content.
    /// For replacements: the full file content after applying the edit.
    pub content: String,

    /// Full file content BEFORE the write.
    /// `None` if this is a new file creation (file didn't exist before).
    /// `Some(text)` if this is an edit to an existing file.
    /// Consumers use this for rewind — restoring the file to its pre-edit state.
    pub previous_content: Option<String>,

    /// Whether this was a new file creation (old_string was empty)
    pub is_new_file: bool,
}

/// Notification that the agent has entered plan mode.
///
/// Sent by the `EnterPlanMode` tool so the gateway / client can transition
/// into plan-mode state (enforce read-only constraints, inject plan-mode
/// system prompts, display plan-mode UI indicators, etc.).
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlanModeEntered {
    /// The tool call ID (correlates with the EnterPlanMode tool invocation)
    pub tool_call_id: String,
}

/// Notification that the agent has exited plan mode.
///
/// Sent by the `ExitPlanMode` tool so the gateway / client can transition
/// out of plan-mode state. The notification carries the plan file content
/// (if any) so the client can present it for user approval without needing
/// a separate file-read round-trip.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PlanModeExited {
    /// The tool call ID (correlates with the ExitPlanMode tool invocation)
    pub tool_call_id: String,

    /// The plan file content at the time ExitPlanMode was called.
    /// `None` if the plan file did not exist or was empty.
    pub plan_content: Option<String>,

    /// The path where the plan file lives (e.g., `.grok/plan.md`).
    pub plan_file_path: String,
}

/// Notification that the agent is asking the user a question.
///
/// Sent by the `AskUserQuestion` tool so the gateway / client can present
/// a structured question UI with options. The client collects the user's
/// answers and returns them as the tool result.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UserQuestionAsked {
    /// The tool call ID (correlates with the AskUserQuestion tool invocation)
    pub tool_call_id: String,

    /// The questions being asked, serialized as JSON for the client to render.
    pub questions_json: serde_json::Value,
}

/// LSP server is being spawned, waiting for initialize handshake.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LspServerStarting {
    pub server_name: String,
    pub command: String,
}

/// LSP server initialized successfully and is ready for use.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LspServerReady {
    pub server_name: String,
}

/// LSP server process died unexpectedly.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LspServerCrashed {
    pub server_name: String,
}

/// LSP server is being retried after a crash.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LspServerRetrying {
    pub server_name: String,
    pub attempt: u32,
    pub max_restarts: u32,
    pub backoff_ms: u64,
}

/// LSP server is dead and will not recover (init failure or max restarts exceeded).
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LspServerFailed {
    pub server_name: String,
    pub error: String,
    /// 0 = init failure (never started), >0 = gave up after N crash restarts.
    pub attempts: u32,
}

/// Notification that a scheduled task has fired and its prompt should be executed.
/// Sent by the `SchedulerActor` when a recurring or one-shot task's interval elapses.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ScheduledTaskFired {
    /// The scheduled task's unique ID.
    pub task_id: String,
    /// The prompt to execute.
    pub prompt: String,
    /// Human-readable schedule description, e.g. "every 5 minutes".
    pub human_schedule: String,
    /// RFC3339 timestamp of next fire (for live countdown viz).
    pub next_fire_at: Option<String>,
}

/// Notification that a scheduled task was removed (deleted, expired, or one-shot completed).
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ScheduledTaskRemoved {
    pub task_id: String,
}

/// Notification that a scheduled task was created and should appear in the tasks pane.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ScheduledTaskCreated {
    /// The scheduled task's unique ID.
    pub task_id: String,
    /// The prompt to execute.
    pub prompt: String,
    /// Human-readable schedule description, e.g. "every 5 minutes".
    pub human_schedule: String,
    /// RFC3339 timestamp of next fire (for live countdown viz).
    pub next_fire_at: Option<String>,
}

/// A streaming event from a Monitor tool background process.
/// Each event is an XML-wrapped stdout line (or batch of lines) that should
/// be injected into the conversation as a user-role message.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MonitorEvent {
    /// Background task ID of the monitor.
    pub task_id: String,
    /// Human-readable description (e.g. "errors in deploy.log").
    pub description: String,
    /// The event text, already XML-wrapped with `<monitor-event>` tags.
    /// Injected into the session conversation for the LLM.
    pub event_text: String,
    /// Raw event text without XML wrapping. Used by the pager for stdout display.
    pub raw_text: String,
    /// Session that owns the monitor task (from the task snapshot). `None` for
    /// legacy backends. The bridge drops events whose owner isn't its session.
    #[cfg_attr(feature = "serde", serde(default))]
    pub owner_session_id: Option<String>,
}

/// A notification emitted by a tool during or after execution.
/// These are sent to external consumers (TUI, logging, etc.) to provide
/// real-time visibility into tool execution.
#[derive(Debug, Clone, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type"))]
pub enum ToolNotification {
    /// Incremental output chunk (sent periodically during execution)
    BashOutputChunk(BashOutputChunk),

    /// Command completed (success or failure)
    BashExecutionComplete(BashExecutionComplete),

    /// Command timed out and was killed
    BashExecutionTimeout(BashExecutionTimeout),

    /// Command was backgrounded (process continues, tool returns)
    BashExecutionBackgrounded(BashExecutionBackgrounded),

    /// Command failed to execute (spawn error, I/O error, etc.)
    BashExecutionFailed(BashExecutionFailed),

    /// A file was written by a tool (search_replace).
    /// Consumers can forward to hunk tracking, audit logging, etc.
    FileWritten(FileWritten),

    /// Task completed notification which sends the exit code as well and notifies any client
    /// about the task being finished status
    TaskCompleted(TaskSnapshot),

    /// The agent requested to enter plan mode.
    /// Consumers (gateway, TUI) use this to transition the client into
    /// plan-mode UI state (e.g., enforce read-only, inject plan-mode
    /// system prompts, show plan-mode indicators).
    PlanModeEntered(PlanModeEntered),

    /// The agent signaled it is done planning and wants to exit plan mode.
    /// Consumers (gateway, TUI) use this to present the plan for user
    /// approval and transition out of plan-mode state.
    PlanModeExited(PlanModeExited),

    /// The agent is asking the user a structured question.
    /// Consumers (gateway, TUI) use this to present the question UI
    /// and collect the user's answers.
    UserQuestionAsked(UserQuestionAsked),

    LspServerStarting(LspServerStarting),
    LspServerReady(LspServerReady),
    LspServerCrashed(LspServerCrashed),
    LspServerRetrying(LspServerRetrying),
    LspServerFailed(LspServerFailed),

    /// A scheduled task fired and its prompt should be injected into the session.
    ScheduledTaskFired(ScheduledTaskFired),

    /// A scheduled task was removed (deleted, expired, or one-shot completed).
    ScheduledTaskRemoved(ScheduledTaskRemoved),

    /// A scheduled task was created and should appear in the tasks pane.
    ScheduledTaskCreated(ScheduledTaskCreated),

    /// A streaming event from a monitor background process.
    MonitorEvent(MonitorEvent),
}

/// Single source of truth for the `(variant tag => payload type)` mapping of
/// [`ToolNotification`], feeding [`ALL_NOTIFICATION_TAGS`] and
/// [`notification_schema_catalog`]. A compile-time exhaustive `match`
/// (`_assert_all_variants_listed`) forces this list to stay in sync with the enum.
macro_rules! notification_variants {
    ($($tag:ident => $payload:ty),+ $(,)?) => {
        /// Every [`ToolNotification`] variant tag (its serde `type`
        /// discriminator), in enum-declaration order.
        pub const ALL_NOTIFICATION_TAGS: &[&str] = &[$(stringify!($tag)),+];

        /// Build the shared notification-schema catalog: every
        /// [`ToolNotification`] variant tag → the JSON Schema of its payload,
        /// using the same draft07 settings as tool input schemas.
        ///
        /// Requires the `serde` feature for wire-faithful schemas: the
        /// `serde(tag/flatten)` attributes that shape payloads are only read by
        /// schemars when `serde` is on (the default for the generator and tests).
        pub fn notification_schema_catalog()
            -> std::collections::BTreeMap<String, serde_json::Value>
        {
            use crate::registry::types::generate_schema;
            let mut catalog = std::collections::BTreeMap::new();
            $(
                catalog.insert(stringify!($tag).to_string(), generate_schema::<$payload>());
            )+
            catalog
        }

        /// Compile-time guard only — the exhaustive `match` forces this macro
        /// invocation to list exactly the enum's variants.
        #[allow(dead_code)]
        fn _assert_all_variants_listed(n: &ToolNotification) {
            match n {
                $( ToolNotification::$tag(_) => {} ),+
            }
        }
    };
}

notification_variants! {
    BashOutputChunk => BashOutputChunk,
    BashExecutionComplete => BashExecutionComplete,
    BashExecutionTimeout => BashExecutionTimeout,
    BashExecutionBackgrounded => BashExecutionBackgrounded,
    BashExecutionFailed => BashExecutionFailed,
    FileWritten => FileWritten,
    TaskCompleted => TaskSnapshot,
    PlanModeEntered => PlanModeEntered,
    PlanModeExited => PlanModeExited,
    UserQuestionAsked => UserQuestionAsked,
    LspServerStarting => LspServerStarting,
    LspServerReady => LspServerReady,
    LspServerCrashed => LspServerCrashed,
    LspServerRetrying => LspServerRetrying,
    LspServerFailed => LspServerFailed,
    ScheduledTaskFired => ScheduledTaskFired,
    ScheduledTaskRemoved => ScheduledTaskRemoved,
    ScheduledTaskCreated => ScheduledTaskCreated,
    MonitorEvent => MonitorEvent,
}

/// Handle for sending notifications to consumers.
/// Clone-able so it can be passed to multiple tool implementations.
///
/// Internally holds one-or-many sender targets. Every existing constructor
/// (`new`, `from_sender`, `channel`, `noop`) builds a single-target handle and
/// behaves exactly as before; [`ToolNotificationHandle::tee`] builds a
/// fan-out handle whose [`send`](Self::send) delivers each notification to all
/// targets, in order, preserving per-target ordering.
#[derive(Clone)]
pub struct ToolNotificationHandle {
    targets: Arc<[tokio::sync::mpsc::UnboundedSender<ToolNotification>]>,
}

impl Default for ToolNotificationHandle {
    fn default() -> Self {
        Self::noop()
    }
}

impl ToolNotificationHandle {
    /// Create a new handle with the given sender
    pub fn new(sender: tokio::sync::mpsc::UnboundedSender<ToolNotification>) -> Self {
        Self {
            targets: Arc::from([sender]),
        }
    }

    /// Create a handle from an existing unbounded sender.
    /// Alias for `new()` — used by tests and consumers that want to receive notifications.
    pub fn from_sender(sender: tokio::sync::mpsc::UnboundedSender<ToolNotification>) -> Self {
        Self::new(sender)
    }

    /// Create a channel pair (handle + receiver)
    pub fn channel() -> (Self, tokio::sync::mpsc::UnboundedReceiver<ToolNotification>) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        (Self::new(sender), receiver)
    }

    /// Create a no-op handle (sends are silently dropped)
    pub fn noop() -> Self {
        let (sender, _receiver) = tokio::sync::mpsc::unbounded_channel();
        Self::new(sender)
    }

    /// Fan-out: build a handle that delivers every notification to all the
    /// underlying targets of the given `handles`, in order.
    ///
    /// Each send is delivered to every target in `handles` (flattened), so a
    /// single tool call's notifications can be surfaced on several sinks at
    /// once (e.g. the session-wide handle plus a per-call sink). Per-target
    /// ordering is preserved: targets observe sends in the same order on the
    /// caller's thread. [`ToolNotification`] derives `Clone`, so each extra
    /// target receives a clone.
    ///
    /// An empty input (`tee(vec![])`) yields a handle with no targets whose
    /// `send` silently drops every notification — i.e. equivalent to
    /// [`noop`](Self::noop).
    pub fn tee(handles: Vec<ToolNotificationHandle>) -> ToolNotificationHandle {
        let targets: Vec<_> = handles
            .iter()
            .flat_map(|h| h.targets.iter().cloned())
            .collect();
        Self {
            targets: Arc::from(targets),
        }
    }

    /// Send a notification to all targets, in order.
    pub fn send(&self, notification: ToolNotification) {
        // Single-target hot path is one send with no clone; for fan-out we
        // clone for every target except the last, which takes ownership.
        let last = self.targets.len().saturating_sub(1);
        for (i, target) in self.targets.iter().enumerate() {
            if i == last {
                let _ = target.send(notification);
                break;
            }
            let _ = target.send(notification.clone());
        }
    }

    // === Convenience methods ===

    pub fn send_output_chunk(&self, chunk: BashOutputChunk) {
        self.send(ToolNotification::BashOutputChunk(chunk));
    }

    pub fn send_complete(&self, complete: BashExecutionComplete) {
        self.send(ToolNotification::BashExecutionComplete(complete));
    }

    pub fn send_timeout(&self, timeout: BashExecutionTimeout) {
        self.send(ToolNotification::BashExecutionTimeout(timeout));
    }

    pub fn send_backgrounded(&self, backgrounded: BashExecutionBackgrounded) {
        self.send(ToolNotification::BashExecutionBackgrounded(backgrounded));
    }

    pub fn send_failed(&self, failed: BashExecutionFailed) {
        self.send(ToolNotification::BashExecutionFailed(failed));
    }

    pub fn send_file_written(&self, written: FileWritten) {
        self.send(ToolNotification::FileWritten(written));
    }

    pub fn send_task_complete(&self, task_completed: TaskSnapshot) {
        self.send(ToolNotification::TaskCompleted(task_completed))
    }

    pub fn send_plan_mode_entered(&self, entered: PlanModeEntered) {
        self.send(ToolNotification::PlanModeEntered(entered));
    }

    pub fn send_plan_mode_exited(&self, exited: PlanModeExited) {
        self.send(ToolNotification::PlanModeExited(exited));
    }

    pub fn send_user_question_asked(&self, asked: UserQuestionAsked) {
        self.send(ToolNotification::UserQuestionAsked(asked));
    }

    pub fn send_lsp_starting(&self, starting: LspServerStarting) {
        self.send(ToolNotification::LspServerStarting(starting));
    }

    pub fn send_lsp_ready(&self, ready: LspServerReady) {
        self.send(ToolNotification::LspServerReady(ready));
    }

    pub fn send_lsp_crashed(&self, crashed: LspServerCrashed) {
        self.send(ToolNotification::LspServerCrashed(crashed));
    }

    pub fn send_lsp_retrying(&self, retrying: LspServerRetrying) {
        self.send(ToolNotification::LspServerRetrying(retrying));
    }

    pub fn send_lsp_failed(&self, failed: LspServerFailed) {
        self.send(ToolNotification::LspServerFailed(failed));
    }

    pub fn send_scheduled_task_fired(&self, fired: ScheduledTaskFired) {
        self.send(ToolNotification::ScheduledTaskFired(fired));
    }

    pub fn send_scheduled_task_removed(&self, removed: ScheduledTaskRemoved) {
        self.send(ToolNotification::ScheduledTaskRemoved(removed));
    }

    pub fn send_scheduled_task_created(&self, created: ScheduledTaskCreated) {
        self.send(ToolNotification::ScheduledTaskCreated(created));
    }

    pub fn send_monitor_event(&self, event: MonitorEvent) {
        self.send(ToolNotification::MonitorEvent(event));
    }
}

/// Per-call notification override.
///
/// When present in `ToolCallContext::extensions`, tools tee their execution
/// notifications here IN ADDITION to the session-wide handle, so a single
/// call's notifications (e.g. bash output chunks) can be surfaced as in-band
/// progress for that one tool call without disturbing the session-wide
/// side-channel.
///
/// This follows the same per-call ctx-extension pattern as `InnerDispatch` /
/// `Cwd`: a simple clone-able newtype wrapper inserted into and pulled out of
/// `ToolCallContext::extensions`.
#[derive(Clone)]
pub struct PerCallNotificationSink(pub ToolNotificationHandle);

#[cfg(test)]
mod handle_tests {
    use super::*;

    fn chunk(tool_call_id: &str) -> ToolNotification {
        ToolNotification::BashOutputChunk(BashOutputChunk {
            base: BashNotificationBase {
                tool_call_id: tool_call_id.into(),
                command: "echo hi".into(),
                output: b"hi".to_vec(),
                total_bytes: 2,
                truncated: false,
                cwd: PathBuf::from("/"),
            },
        })
    }

    fn tool_call_id(n: &ToolNotification) -> &str {
        match n {
            ToolNotification::BashOutputChunk(c) => &c.base.tool_call_id,
            other => panic!("expected BashOutputChunk, got {other:?}"),
        }
    }

    #[test]
    fn single_target_hot_path_receives_exactly_what_was_sent() {
        let (handle, mut rx) = ToolNotificationHandle::channel();
        handle.send(chunk("a"));
        handle.send(chunk("b"));
        drop(handle);

        assert_eq!(tool_call_id(&rx.try_recv().unwrap()), "a");
        assert_eq!(tool_call_id(&rx.try_recv().unwrap()), "b");
        assert!(rx.try_recv().is_err(), "no extra notifications expected");
    }

    #[test]
    fn tee_delivers_to_all_targets_in_order() {
        let (h1, mut rx1) = ToolNotificationHandle::channel();
        let (h2, mut rx2) = ToolNotificationHandle::channel();
        let teed = ToolNotificationHandle::tee(vec![h1, h2]);

        teed.send(chunk("a"));
        teed.send(chunk("b"));
        teed.send(chunk("c"));
        drop(teed);

        for rx in [&mut rx1, &mut rx2] {
            assert_eq!(tool_call_id(&rx.try_recv().unwrap()), "a");
            assert_eq!(tool_call_id(&rx.try_recv().unwrap()), "b");
            assert_eq!(tool_call_id(&rx.try_recv().unwrap()), "c");
            assert!(rx.try_recv().is_err(), "no extra notifications expected");
        }
    }

    #[test]
    fn catalog_has_one_schema_per_variant() {
        let catalog = notification_schema_catalog();
        let mut keys: Vec<&str> = catalog.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut tags: Vec<&str> = ALL_NOTIFICATION_TAGS.to_vec();
        tags.sort_unstable();
        assert_eq!(keys, tags, "catalog keys must equal ALL_NOTIFICATION_TAGS");
        for (tag, schema) in &catalog {
            assert!(schema.is_object(), "schema for {tag} is not an object");
        }
    }

    #[test]
    fn output_lossy_replaces_invalid_utf8() {
        let base = BashNotificationBase {
            tool_call_id: "tc".into(),
            command: "c".into(),
            output: vec![b'h', b'i', 0xff],
            total_bytes: 3,
            truncated: false,
            cwd: PathBuf::from("/"),
        };
        assert_eq!(base.output_lossy(), "hi\u{fffd}");
    }
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    fn base_with_output(output: Vec<u8>) -> BashNotificationBase {
        BashNotificationBase {
            tool_call_id: "tc-1".into(),
            command: "echo hi".into(),
            output,
            total_bytes: 0,
            truncated: false,
            cwd: PathBuf::from("/"),
        }
    }

    #[test]
    fn base_output_serializes_as_base64_string() {
        let original = base_with_output(vec![0x00, 0xff, 0xfe, b'h', b'i']);
        let value = serde_json::to_value(&original).unwrap();
        assert!(
            value["output"].is_string(),
            "output must be a base64 string, got {value:?}"
        );
        let back: BashNotificationBase = serde_json::from_value(value).unwrap();
        assert_eq!(back, original);
    }

    // Guards against an accidental alphabet/padding switch (STANDARD vs URL-safe).
    #[test]
    fn base_output_exact_base64_string() {
        let original = base_with_output(b"hello".to_vec());
        let value = serde_json::to_value(&original).unwrap();
        assert_eq!(value["output"], serde_json::json!("aGVsbG8="));
    }

    #[test]
    fn base_output_reads_legacy_integer_array() {
        let legacy = serde_json::json!({
            "tool_call_id": "tc-1",
            "command": "echo hi",
            "output": [104, 101, 108, 108, 111],
            "total_bytes": 0,
            "truncated": false,
            "cwd": "/"
        });
        let base: BashNotificationBase = serde_json::from_value(legacy).unwrap();
        assert_eq!(base.output, b"hello".to_vec());
    }

    // Production path: legacy int-array through the tagged + flattened enum.
    #[test]
    fn tool_notification_reads_legacy_integer_array() {
        let legacy = serde_json::json!({
            "type": "BashOutputChunk",
            "tool_call_id": "tc-1",
            "command": "echo hi",
            "output": [104, 101, 108, 108, 111],
            "total_bytes": 5,
            "truncated": false,
            "cwd": "/"
        });
        let note: ToolNotification = serde_json::from_value(legacy).unwrap();
        match note {
            ToolNotification::BashOutputChunk(chunk) => {
                assert_eq!(chunk.base.output, b"hello".to_vec());
            }
            other => panic!("expected BashOutputChunk, got {other:?}"),
        }
    }

    // Production path: base64 round-trip through the tagged + flattened enum.
    #[test]
    fn tool_notification_base64_round_trips() {
        let original = ToolNotification::BashOutputChunk(BashOutputChunk {
            base: base_with_output(vec![0x00, 0xff, 0xfe, b'h', b'i']),
        });
        let value = serde_json::to_value(&original).unwrap();
        assert_eq!(value["type"], serde_json::json!("BashOutputChunk"));
        assert!(
            value["output"].is_string(),
            "output must be a base64 string through the enum, got {value:?}"
        );

        let back: ToolNotification = serde_json::from_value(value).unwrap();
        assert_eq!(back, original);
        match back {
            ToolNotification::BashOutputChunk(chunk) => {
                assert_eq!(chunk.base.output, vec![0x00, 0xff, 0xfe, b'h', b'i']);
            }
            other => panic!("expected BashOutputChunk, got {other:?}"),
        }
    }
}
