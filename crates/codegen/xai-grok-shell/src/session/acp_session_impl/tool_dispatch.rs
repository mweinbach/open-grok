//! Tool dispatch helpers for `SessionActor`: `dispatch_tool` and its lock /
//! display helpers, direct bash-mode execution, and tool argument
//! parse-error formatting.

use super::*;

/// Number of output lines to show in final bash mode output summary
const BASH_MODE_FINAL_OUTPUT_LINES: usize = 10;
const BASH_MODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Phase 2: dispatch a tool call through [`WorkspaceOps::call_tool`].
///
/// Agent sessions always use local workspace ops (in-process toolset).
pub(super) async fn dispatch_tool(
    workspace_ops: &xai_grok_workspace::WorkspaceOps,
    prepared: &PreparedToolCall,
    session_id: &str,
) -> Result<ToolRunResult, xai_tool_runtime::ToolError> {
    tracing::debug!(
        tool = %prepared.tool_name,
        call_id = %prepared.tool_call_id.0,
        session = %session_id,
        mode = "local",
        "dispatch_tool"
    );
    workspace_ops
        .call_tool(
            &prepared.tool_name,
            prepared.parsed_args.clone(),
            if xai_grok_sampling_types::conversation::decode_codex_function_call_id(
                &prepared.call_id,
            )
            .is_some()
            {
                &prepared.call_id
            } else {
                &prepared.tool_call_id.0
            },
            Some(session_id),
        )
        .await
}

/// Nested-Code-Mode variant of [`dispatch_tool`] that also forwards the
/// dispatch stream's progress items into both the embedded cell and the
/// nested tool's existing ACP card without adding model conversation items.
pub(super) async fn dispatch_code_mode_nested_tool_streaming<F, Fut>(
    workspace_ops: &xai_grok_workspace::WorkspaceOps,
    prepared: &PreparedToolCall,
    session_id: &str,
    cancellation_token: &tokio_util::sync::CancellationToken,
    progress: &xai_grok_code_mode_protocol::NestedToolProgressSink,
    on_progress: F,
) -> Result<ToolRunResult, xai_tool_runtime::ToolError>
where
    F: FnMut(acp::ToolCallUpdate) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    if cancellation_token.is_cancelled() {
        return Err(xai_tool_runtime::ToolError::new(
            xai_tool_runtime::ToolErrorKind::Cancelled,
            format!("Code Mode tool `{}` was cancelled", prepared.tool_name),
        ));
    }
    let streaming = match workspace_ops {
        xai_grok_workspace::WorkspaceOps::Local { handle } => {
            handle.session(session_id).map(|session| session.toolset())
        }
        xai_grok_workspace::WorkspaceOps::Proxy { .. } => None,
    };
    let Some(toolset) = streaming else {
        // Proxy workspaces have no client-side progress stream to observe.
        return dispatch_tool(workspace_ops, prepared, session_id).await;
    };
    tracing::debug!(
        tool = %prepared.tool_name,
        call_id = %prepared.tool_call_id.0,
        session = %session_id,
        mode = "local_streaming",
        "dispatch_code_mode_nested_tool_streaming"
    );
    let stream = toolset.call_streaming_with_cancellation_and_viewer_context(
        &prepared.tool_name,
        prepared.parsed_args.clone(),
        &prepared.tool_call_id.0,
        None,
        Some(cancellation_token.clone()),
        Some(xai_tool_runtime::WorkspaceViewerContext {
            stream_tool_progress: true,
        }),
    );
    drain_code_mode_nested_tool_stream(stream, prepared, progress, on_progress).await
}

async fn drain_code_mode_nested_tool_stream<F, Fut>(
    mut stream: xai_tool_runtime::ToolStream<ToolRunResult>,
    prepared: &PreparedToolCall,
    progress: &xai_grok_code_mode_protocol::NestedToolProgressSink,
    mut on_progress: F,
) -> Result<ToolRunResult, xai_tool_runtime::ToolError>
where
    F: FnMut(acp::ToolCallUpdate) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use futures::StreamExt;
    while let Some(item) = stream.next().await {
        match item {
            xai_tool_runtime::ToolStreamItem::Progress(progress_item) => {
                let update = nested_tool_progress_update(prepared, &progress_item);
                progress.push(
                    crate::session::code_mode::nested_tool_progress_from_tool_progress(
                        progress_item,
                    ),
                );
                if let Some(update) = update {
                    on_progress(update).await;
                }
            }
            xai_tool_runtime::ToolStreamItem::Terminal(result) => return result,
        }
    }
    Err(xai_tool_runtime::ToolError::custom(
        "stream_no_terminal",
        "dispatch stream ended without a terminal item",
    ))
}

fn nested_tool_progress_update(
    prepared: &PreparedToolCall,
    progress: &xai_tool_runtime::ToolProgress,
) -> Option<acp::ToolCallUpdate> {
    let mut fields = acp::ToolCallUpdateFields::new().status(Some(acp::ToolCallStatus::InProgress));

    match progress {
        xai_tool_runtime::ToolProgress::Text { text } => {
            fields = fields.content(Some(vec![acp::ToolCallContent::from(
                acp::ContentBlock::Text(acp::TextContent::new(text.clone())),
            )]));
        }
        xai_tool_runtime::ToolProgress::Content { blocks } => {
            let content = blocks
                .iter()
                .filter_map(|block| serde_json::to_value(block).ok())
                .filter_map(|block| serde_json::from_value::<acp::ContentBlock>(block).ok())
                .map(acp::ToolCallContent::from)
                .collect::<Vec<_>>();
            if !content.is_empty() {
                fields = fields.content(Some(content));
            }
        }
        xai_tool_runtime::ToolProgress::Custom { subkind, payload } => {
            if subkind == "bash_output_chunk" {
                return None;
            }
            let text = payload
                .get("delta")
                .or_else(|| payload.get("text"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| payload.to_string());
            fields = fields.content(Some(vec![acp::ToolCallContent::from(
                acp::ContentBlock::Text(acp::TextContent::new(text.clone())),
            )]));
        }
    }

    Some(acp::ToolCallUpdate::new(
        prepared.tool_call_id.clone(),
        fields,
    ))
}

/// First string-valued argument among `keys`, in priority order.
fn str_arg<'a>(args: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k)?.as_str())
}

/// Extract the workspace path that a tool call targets, for the purpose of
/// serializing concurrent same-file edits inside `execute_tool_calls`.
///
/// Different toolsets advertise the path under different JSON keys:
/// - `file_path` — grok_build (`search_replace`), opencode (`EditTool`,
///   `WriteTool`, `ReadTool`), codex (`read_file`), grok_build_hashline
///   (`hashline_edit`)
/// - `path` — alternate edit/read tools
/// - `target_file` — grok_build (`read_file`, via `#[serde(rename)]`)
///
/// Returning the same string for two calls in a batch causes them to share a
/// `tokio::sync::Mutex` and therefore run sequentially in model-emitted order.
/// Returning `None` lets the call run fully concurrently with everything else.
///
/// `target_directory` is deliberately omitted — a directory listing isn't an
/// edit and must not bucket into a file lock.
pub(super) fn lock_path_for_args(args: &serde_json::Value, cwd: &Path) -> Option<String> {
    let input = Path::new(str_arg(args, &["file_path", "path", "target_file"])?);
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        cwd.join(input)
    };
    let mut normalized = std::path::PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    let lock_path = canonicalize_existing_ancestor(&normalized).unwrap_or(normalized);
    Some(lock_path.to_string_lossy().into_owned())
}

fn canonicalize_existing_ancestor(path: &Path) -> Option<std::path::PathBuf> {
    let mut ancestor = path;
    let mut suffix = Vec::new();
    loop {
        if let Ok(mut canonical) = dunce::canonicalize(ancestor) {
            suffix.reverse();
            canonical.extend(suffix);
            return Some(canonical);
        }
        suffix.push(ancestor.file_name()?.to_owned());
        ancestor = ancestor.parent()?;
    }
}

#[cfg(test)]
mod canonical_lock_tests {
    use super::*;

    #[test]
    fn relative_and_absolute_paths_share_a_lock() {
        let directory = tempfile::tempdir().unwrap();
        let canonical = dunce::canonicalize(directory.path()).unwrap();
        let relative = serde_json::json!({"file_path": "nested/../new.txt"});
        let absolute = serde_json::json!({"path": canonical.join("new.txt")});
        assert_eq!(
            lock_path_for_args(&relative, directory.path()),
            lock_path_for_args(&absolute, directory.path())
        );
    }

    #[test]
    fn resolved_model_identity_requires_catalog_opt_in() {
        assert!(!should_show_resolved_model(
            "grok-build",
            "checkpoint",
            false
        ));
        assert!(should_show_resolved_model(
            "custom-model",
            "checkpoint",
            true
        ));
        assert!(!should_show_resolved_model(
            "checkpoint",
            "checkpoint",
            true
        ));
    }
}

/// Pull the path a read/list tool targets and classify it against the store.
/// Keys span harnesses: `read_file`=`target_file`, grep=`path`,
/// `list_dir`=`target_directory`. Grammar lives in `xai_chat_state`.
pub(super) fn compaction_artifact_read(
    args: &serde_json::Value,
) -> Option<xai_chat_state::compaction_transcript::CompactionArtifact> {
    let path = str_arg(
        args,
        &["target_file", "file_path", "path", "target_directory"],
    )?;
    xai_chat_state::compaction_transcript::classify_compaction_path(path)
}

/// Map a backend-hosted tool name to a user-facing title, ACP ToolKind,
/// and `raw_input` JSON for display in the pager's tool call UI.
///
/// The `raw_input` carries metadata that the pager's `tool_call_to_block()`
/// uses to select the correct renderer (e.g., `variant: "WebSearch"` picks
/// the `WebSearchToolCallBlock` instead of the grep `SearchToolCallBlock`).
pub(super) fn backend_tool_display(name: &str) -> (String, acp::ToolKind, serde_json::Value) {
    match name {
        "web_search" => (
            "Web search:".to_string(),
            acp::ToolKind::Search,
            serde_json::json!({"variant": "WebSearch", "backend": true}),
        ),
        "x_search" => (
            "X search:".to_string(),
            acp::ToolKind::Search,
            serde_json::json!({"variant": "XSearch", "backend": true}),
        ),
        n => (
            n.to_string(),
            acp::ToolKind::Other,
            serde_json::json!({"backend": true}),
        ),
    }
}

/// Map a completed backend (server-side) tool call's payload to the ACP terminal
/// status the shell should emit. The backend reports each call's real
/// success/failure in the serialized payload's top-level `status` field (e.g. a
/// `web_search_call`'s `WebSearchToolCallStatus`, which includes `failed`); a
/// `"failed"` status becomes [`acp::ToolCallStatus::Failed`] so downstream
/// consumers — notably the headless `streaming-messages-json`
/// `web_search_tool_result_error` branch — see the real failure instead of a
/// blanket `Completed`. Any other or absent status stays `Completed`
/// (behavior-preserving for the success path).
pub(super) fn backend_tool_call_status(result: Option<&serde_json::Value>) -> acp::ToolCallStatus {
    let failed = result
        .and_then(|r| r.get("status"))
        .and_then(serde_json::Value::as_str)
        == Some("failed");
    if failed {
        acp::ToolCallStatus::Failed
    } else {
        acp::ToolCallStatus::Completed
    }
}

/// Temporary gate: only expose resolved model ID to the user for these models.
pub(super) fn should_show_resolved_model(
    requested: &str,
    resolved: &str,
    show_checkpoint_identity: bool,
) -> bool {
    show_checkpoint_identity && requested != resolved
}

/// Resolve the shell name for the system prompt `Shell:` field.
///
/// Unix: basename of `$SHELL` (e.g. "zsh", "bash").
/// Windows: name from the `detect_windows_shell` cascade
/// (pwsh > powershell.exe > Git Bash > cmd.exe), since `$SHELL` is absent.
pub(super) fn resolve_session_shell() -> String {
    #[cfg(unix)]
    {
        std::env::var("SHELL")
            .ok()
            .and_then(|s| {
                std::path::Path::new(&s)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "bash".to_string())
    }

    #[cfg(not(unix))]
    {
        xai_grok_config::shell::detect_windows_shell()
            .name()
            .to_string()
    }
}

/// Key in `ToolError::details` that carries the HTTP status code.
/// Used by both error producers (image_gen, video_gen, test helpers) and
/// the `is_auth_tool_error` classifier to avoid accidental key mismatch.
pub(crate) const HTTP_STATUS_DETAILS_KEY: &str = "status";

impl SessionActor {
    /// Extract bash command from prompt blocks if present in meta.
    /// Returns Some(command) if the prompt is a direct bash command, None otherwise.
    pub(super) fn extract_bash_command(prompt_blocks: &[acp::ContentBlock]) -> Option<String> {
        use crate::extensions::prompt_meta::PromptBlockMeta;
        for block in prompt_blocks {
            if let acp::ContentBlock::Text(text) = block
                && let Some(meta_val) = &text.meta
                && let Some(meta) = PromptBlockMeta::from_value(meta_val)
            {
                return meta.bash_command;
            }
        }
        None
    }

    /// Handle a direct bash command from bash mode.
    /// Runs the command with streaming output and sends updates to the TUI.
    pub(super) async fn handle_direct_bash_command(
        &self,
        _prompt_id: &str,
        command: String,
        prompt_blocks: &[acp::ContentBlock],
    ) -> PromptTurnResult {
        tracing::info!("Handling direct bash command");

        // Send user message chunks to scrollback (so user sees their command)
        let model_id = self.current_model_id().await;
        let user_chunk_meta = serde_json::json!({ "modelId": model_id })
            .as_object()
            .cloned();
        for block in prompt_blocks.iter() {
            let update = acp::SessionUpdate::UserMessageChunk(
                acp::ContentChunk::new(block.clone()).meta(user_chunk_meta.clone()),
            );
            let notification_meta = self.build_notification_meta();
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::Update(SessionUpdate::Acp(Box::new(
                    acp::SessionNotification::new(self.session_info.id.clone(), update)
                        .meta(notification_meta.as_object().cloned()),
                ))));
        }

        // Persist the user message for session history
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::ContentChunk(PersistenceContentChunk::new(
                prompt_blocks.to_vec(),
            )));
        // Bash turns bypass `handle_prompt`'s commit point; the command is now
        // in the ordered persistence stream, so a send-now may cancel this turn.
        self.mark_front_message_committed().await;

        // Run the bash command with streaming enabled
        let tool_call_id = acp::ToolCallId::from(format!("bash-mode-{}", uuid::Uuid::new_v4()));

        // Send initial ToolCall to register with TUI

        use xai_grok_tools::types::ToolInput;
        // Use the stripped command as description so pager chrome shows the
        // real command (not a generic label) while still satisfying the required field.
        let title_command = xai_grok_tools::util::strip_redundant_session_cd(
            &command,
            self.tool_context.cwd.as_path(),
        );
        let tool_input = ToolInput::Bash(BashToolInput {
            command: command.clone(),
            timeout: None,
            description: title_command.clone().into_owned(),
            is_background: false,
        });
        // Bash mode has no model-issued wire name; resolve the toolset's
        // execute tool by kind so the x.ai/tool identity still stamps.
        let bash_marker = serde_json::json!({"bash_mode": true}).as_object().cloned();
        let exec_wire = {
            let agent = self.agent.borrow();
            agent
                .tool_bridge()
                .toolset()
                .tool_name_for_kind(xai_grok_tools::types::tool::ToolKind::Execute)
        };
        let bash_meta = match exec_wire {
            Some(wire) => self.stamp_tool_meta(bash_marker.clone(), &wire, Some(&tool_input)),
            None => bash_marker,
        };
        self.send_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(tool_call_id.clone(), format!("Execute `{title_command}`"))
                    .kind(acp::ToolKind::Execute)
                    .status(acp::ToolCallStatus::InProgress)
                    .content(Vec::new())
                    .locations(Vec::new())
                    .raw_input(serde_json::to_value(&tool_input).ok())
                    .meta(bash_meta),
            ),
            None,
        )
        .await;

        let request = TerminalRunRequest {
            tool_call_id: tool_call_id.clone(),
            command: command.clone(),
            cwd: self.tool_context.cwd.clone(),
            env: self.tool_context.session_env.as_ref().clone(),
            timeout: BASH_MODE_TIMEOUT,
            output_byte_limit: 1_048_576, // 1 MiB
            stream: true,                 // Enable streaming for bash mode
            output_file: None,            // No file logging for interactive bash mode
        };

        let result = self.tool_context.terminal.run(request).await;

        // Format the output
        let (output, exit_code, timed_out, signal) = match result {
            Ok(res) => (
                res.combined_output,
                res.exit_code.unwrap_or(-1),
                res.timed_out,
                res.signal,
            ),
            Err(e) => (format!("Error running command: {}", e), -1, false, None),
        };

        // Create final summary with last N lines
        // Format: "... (X lines)\nlast\nfew\nlines"
        let lines: Vec<&str> = output.lines().collect();
        let total_lines = lines.len();
        let displayed_output = if total_lines > BASH_MODE_FINAL_OUTPUT_LINES {
            let start = total_lines - BASH_MODE_FINAL_OUTPUT_LINES;
            let last_lines = lines[start..].join("\n");
            format!("... ({} lines)\n{}", total_lines, last_lines)
        } else {
            output.trim_end().to_string()
        };

        let is_backgrounded = signal.as_deref() == Some("backgrounded");

        // Build the final response text with output summary and exit code
        let mut response_text = displayed_output.clone();
        if is_backgrounded {
            response_text.push_str("\n\n[command running in background]");
        } else if timed_out {
            response_text.push_str("\n\n[command timed out]");
        } else if let Some(ref sig) = signal {
            response_text.push_str(&format!("\n\n[killed by signal {}]", sig));
        } else {
            response_text.push_str(&format!("\n\n[exit code: {}]", exit_code));
        }

        // Send final tool call update
        // For backgrounded commands, don't mark as completed/failed - let the background task do that
        if !is_backgrounded {
            let final_status = if exit_code == 0 && signal.is_none() {
                acp::ToolCallStatus::Completed
            } else {
                acp::ToolCallStatus::Failed
            };
            let bash_output = BashOutput {
                output_for_prompt: BashOutput::make_output_for_prompt(&displayed_output),
                output: displayed_output.as_bytes().to_vec(),
                exit_code,
                command: command.clone(),
                truncated: total_lines > BASH_MODE_FINAL_OUTPUT_LINES,
                signal: signal.clone(),
                timed_out,
                description: None,
                current_dir: self.tool_context.cwd.to_string(),
                output_file: String::new(),
                total_bytes: displayed_output.len(),
                output_delta: None,
                was_bare_echo: false,
            };
            self.send_update(
                acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    tool_call_id,
                    acp::ToolCallUpdateFields::new()
                        .status(Some(final_status))
                        .raw_output(serde_json::to_value(ToolsToolOutput::Bash(bash_output)).ok()),
                )),
                None,
            )
            .await;
        }

        // NOTE: The redundant AgentMessageChunk summary that was previously
        // sent here has been removed. The execute block already contains the
        // full command output — sending it again as an agent message created
        // a noisy duplicate scrollback entry. Old sessions that have it will
        // still replay fine; new sessions are cleaner.

        // Build a single user message for chat history that includes command, output, and exit code
        let user_message = format!(
            "I executed a terminal command: `{}`\n\nOutput:\n```\n{}\n```\n\n[exit code: {}]",
            command, displayed_output, exit_code
        );

        // Add to chat history as a user message only
        self.chat_state_handle
            .push_user_message(ConversationItem::user(&user_message));

        self.chat_state_handle.flush();

        let flush_error = self.flush_to_disk().await.err();
        self.disk_full_acp_error(flush_error.as_ref())?;

        let total_tokens = self.chat_state_handle.get_total_tokens().await;
        ok_end_turn(total_tokens, None)
    }
}

// ── Tool argument error formatting ─────────────────────────────────────

// Re-use the UTF-8-safe truncation helper from xai-grok-sampling-types rather
// than duplicating it here (R3).

/// Maximum bytes of `raw_arguments` included in a parse-error tool_result.
///
/// The model already holds the arguments in its recent context window, so
/// echoing the full string (potentially 8 KB+) would grow every subsequent
/// turn by that many tokens for no additional benefit.  The JSON error
/// position (e.g. `line 1 column 81`) is usually sufficient to locate the
/// typo; we include a prefix for orientation.
///
/// Note: when the JSON syntax error falls past this byte limit, the column
/// hint will reference text that was truncated from the message.  The model
/// should still have the full arguments in its context window from the
/// turn it generated them.
pub(crate) const MAX_ARGS_IN_ERROR: usize = 2_000;

/// Build the user-facing error message shown when tool arguments cannot be
/// parsed.  The message is stored as a `tool_result` in the conversation
/// history, so the model sees it on the very next turn.
///
/// The message intentionally includes:
///
/// 1. The normal error description (so the model knows *what* failed).
/// 2. The **original arguments string** the model produced (capped at
///    [`MAX_ARGS_IN_ERROR`] bytes).  Without this, grok-shell would sanitize
///    the arguments to `"{}"` before forwarding them to the provider (to
///    avoid 400 errors), so the model would only see an empty object and have
///    to regenerate all its work from scratch.
/// 3. A JSON-level parse error (position + reason) when the arguments string
///    is itself invalid JSON — e.g. a missing `"` before a key name.  This
///    lets the model fix a one-character typo rather than regenerating a
///    thousand-line file.
pub(super) fn build_tool_parse_error_message(
    function_name: &str,
    err: &xai_tool_runtime::ToolError,
    raw_arguments: &str,
) -> String {
    let mut msg = format!("Failed to parse arguments for tool `{function_name}`: {err}");

    if raw_arguments.is_empty() {
        return msg;
    }

    // Append the original arguments (capped) so the model knows what it sent.
    // Use truncate_bytes to avoid panicking on a multi-byte UTF-8 boundary.
    msg.push_str("\n\nYour original arguments:\n");
    let prefix = truncate_bytes(raw_arguments, MAX_ARGS_IN_ERROR);
    msg.push_str(prefix);
    if prefix.len() < raw_arguments.len() {
        msg.push_str("\n... (truncated)");
    }

    // If the arguments string is not valid JSON, surface the exact position
    // of the syntax error so the model can fix it directly.
    // Use `IgnoredAny` — we only need the error, not a DOM.
    if let Err(json_err) = serde_json::from_str::<serde::de::IgnoredAny>(raw_arguments) {
        msg.push_str(&format!(
            "\n\nNote: the arguments above contain invalid JSON — {json_err}\n\
             Please fix the syntax and retry."
        ));
    }

    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_sampling_types::rs;

    fn nested_prepared_call() -> PreparedToolCall {
        PreparedToolCall {
            call_id: "model-call-1".to_string(),
            tool_call_id: acp::ToolCallId::from("acp-call-1"),
            tool_name: "read_file".to_string(),
            raw_arguments: "{}".to_string(),
            parsed_args: serde_json::json!({}),
            model_id: String::new(),
            concatenated_json_count: 0,
            dispatch_target_name: None,
            is_read_only: true,
            rewriting_hook: None,
            additional_context: Vec::new(),
        }
    }

    /// A local workspace without a bound session has no stream to observe:
    /// the streaming wrapper must fall back to plain `dispatch_tool` and
    /// surface its error instead of inventing progress or panicking.
    #[tokio::test]
    async fn nested_streaming_dispatch_falls_back_without_a_bound_session() {
        let workspace_ops = xai_grok_workspace::WorkspaceOps::for_test();
        let (progress_sink, _progress_rx) =
            xai_grok_code_mode_protocol::nested_tool_progress_channel();
        let error = dispatch_code_mode_nested_tool_streaming(
            &workspace_ops,
            &nested_prepared_call(),
            "no-such-session",
            &tokio_util::sync::CancellationToken::new(),
            &progress_sink,
            |_| async {},
        )
        .await
        .expect_err("unbound session must fail dispatch");
        assert!(
            error.to_string().contains("session not found"),
            "fallback must surface the underlying dispatch error: {error}"
        );
        assert!(!progress_sink.is_closed());
    }

    #[tokio::test]
    async fn nested_streaming_dispatch_rejects_cancelled_calls_before_workspace_dispatch() {
        let workspace_ops = xai_grok_workspace::WorkspaceOps::for_test();
        let cancellation_token = tokio_util::sync::CancellationToken::new();
        cancellation_token.cancel();
        let (progress_sink, progress_rx) =
            xai_grok_code_mode_protocol::nested_tool_progress_channel();

        let error = dispatch_code_mode_nested_tool_streaming(
            &workspace_ops,
            &nested_prepared_call(),
            "no-such-session",
            &cancellation_token,
            &progress_sink,
            |_| async {},
        )
        .await
        .expect_err("cancelled nested calls must not reach workspace dispatch");

        assert!(error.to_string().contains("was cancelled"));
        assert!(progress_rx.try_recv().is_none());
    }

    #[test]
    fn nested_text_progress_updates_the_existing_acp_card() {
        let prepared = nested_prepared_call();
        let update = nested_tool_progress_update(
            &prepared,
            &xai_tool_runtime::ToolProgress::Text {
                text: "partial output".to_string(),
            },
        )
        .expect("text progress must update its ACP card");

        assert_eq!(update.tool_call_id, prepared.tool_call_id);
        assert_eq!(update.fields.status, Some(acp::ToolCallStatus::InProgress));
        let serialized = serde_json::to_value(update).expect("serialize ACP progress update");
        assert_eq!(
            serialized["content"][0]["content"]["text"],
            "partial output"
        );
    }

    #[test]
    fn nested_content_progress_preserves_acp_content_blocks() {
        let update = nested_tool_progress_update(
            &nested_prepared_call(),
            &xai_tool_runtime::ToolProgress::Content {
                blocks: vec![xai_tool_runtime::ContentBlock::Text {
                    text: "content chunk".to_string(),
                }],
            },
        )
        .expect("content progress must update its ACP card");

        let serialized = serde_json::to_value(update).expect("serialize ACP progress update");
        assert_eq!(serialized["content"][0]["content"]["text"], "content chunk");
    }

    #[test]
    fn nested_custom_progress_surfaces_the_projected_text_delta() {
        let update = nested_tool_progress_update(
            &nested_prepared_call(),
            &xai_tool_runtime::ToolProgress::Custom {
                subkind: "grep_match_chunk".to_string(),
                payload: serde_json::json!({ "delta": "src/main.rs:42:match\n" }),
            },
        )
        .expect("grep progress must update its ACP card");

        let serialized = serde_json::to_value(update).expect("serialize ACP progress update");
        assert_eq!(
            serialized["content"][0]["content"]["text"],
            "src/main.rs:42:match\n"
        );
        assert!(serialized.get("rawOutput").is_none());
    }

    #[test]
    fn nested_bash_progress_does_not_duplicate_notification_bridge_updates() {
        let update = nested_tool_progress_update(
            &nested_prepared_call(),
            &xai_tool_runtime::ToolProgress::Custom {
                subkind: "bash_output_chunk".to_string(),
                payload: serde_json::json!({
                    "delta": "hello",
                    "total_bytes": 11,
                    "truncated": true
                }),
            },
        );

        assert!(
            update.is_none(),
            "the notification bridge already sends canonical bash ACP progress"
        );
    }

    #[tokio::test]
    async fn nested_bash_progress_still_reaches_javascript_without_duplicate_acp_updates() {
        let prepared = nested_prepared_call();
        let (progress_sink, progress_rx) =
            xai_grok_code_mode_protocol::nested_tool_progress_channel();
        let acp_updates = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stream = Box::pin(futures::stream::iter(vec![
            xai_tool_runtime::ToolStreamItem::Progress(xai_tool_runtime::ToolProgress::Custom {
                subkind: "bash_output_chunk".to_string(),
                payload: serde_json::json!({ "delta": "hello" }),
            }),
            xai_tool_runtime::ToolStreamItem::Terminal(Err(xai_tool_runtime::ToolError::custom(
                "stream_failed",
                "terminal failure",
            ))),
        ]));

        let _ = drain_code_mode_nested_tool_stream(stream, &prepared, &progress_sink, |_| {
            let acp_updates = std::sync::Arc::clone(&acp_updates);
            async move {
                acp_updates.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .await;

        assert_eq!(
            progress_rx
                .try_recv()
                .expect("bash JavaScript progress")
                .payload,
            Some(serde_json::json!({
                "subkind": "bash_output_chunk",
                "payload": { "delta": "hello" }
            }))
        );
        assert_eq!(acp_updates.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn nested_stream_progress_reaches_javascript_and_acp_in_order_before_terminal_error() {
        let prepared = nested_prepared_call();
        let (progress_sink, progress_rx) =
            xai_grok_code_mode_protocol::nested_tool_progress_channel();
        let updates = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stream = Box::pin(futures::stream::iter(vec![
            xai_tool_runtime::ToolStreamItem::Progress(xai_tool_runtime::ToolProgress::Text {
                text: "first".to_string(),
            }),
            xai_tool_runtime::ToolStreamItem::Progress(xai_tool_runtime::ToolProgress::Text {
                text: "second".to_string(),
            }),
            xai_tool_runtime::ToolStreamItem::Terminal(Err(xai_tool_runtime::ToolError::custom(
                "stream_failed",
                "terminal failure",
            ))),
        ]));

        let error =
            drain_code_mode_nested_tool_stream(stream, &prepared, &progress_sink, |update| {
                let updates = std::sync::Arc::clone(&updates);
                async move {
                    updates.lock().expect("progress update lock").push(update);
                }
            })
            .await
            .expect_err("terminal failures must propagate unchanged");

        assert!(error.to_string().contains("terminal failure"));
        assert_eq!(
            progress_rx.try_recv().expect("first JS chunk").text,
            "first"
        );
        assert_eq!(
            progress_rx.try_recv().expect("second JS chunk").text,
            "second"
        );
        assert!(progress_rx.try_recv().is_none());
        let updates = updates.lock().expect("progress update lock");
        assert_eq!(updates.len(), 2);
        let serialized = updates
            .iter()
            .map(|update| serde_json::to_value(update).expect("serialize ACP progress update"))
            .collect::<Vec<_>>();
        assert_eq!(serialized[0]["content"][0]["content"]["text"], "first");
        assert_eq!(serialized[1]["content"][0]["content"]["text"], "second");
    }

    #[tokio::test]
    async fn nested_stream_without_terminal_fails_after_delivering_progress() {
        let prepared = nested_prepared_call();
        let (progress_sink, progress_rx) =
            xai_grok_code_mode_protocol::nested_tool_progress_channel();
        let stream = Box::pin(futures::stream::iter(vec![
            xai_tool_runtime::ToolStreamItem::Progress(xai_tool_runtime::ToolProgress::Text {
                text: "before disconnect".to_string(),
            }),
        ]));

        let error =
            drain_code_mode_nested_tool_stream(stream, &prepared, &progress_sink, |_| async {})
                .await
                .expect_err("streams without a terminal item must fail closed");

        assert!(error.to_string().contains("without a terminal item"));
        assert_eq!(
            progress_rx
                .try_recv()
                .expect("progress before disconnect")
                .text,
            "before disconnect"
        );
    }

    fn web_search_payload(status: rs::WebSearchToolCallStatus) -> serde_json::Value {
        // The exact serialized `web_search_call` payload the sampler forwards on
        // `BackendToolCallCompleted` (via `serde_json::to_value(ws)`).
        serde_json::to_value(rs::WebSearchToolCall {
            action: rs::WebSearchToolCallAction::Search(rs::WebSearchActionSearch {
                query: "rust async runtime".to_string(),
                sources: None,
            }),
            id: "ws1".to_string(),
            status,
        })
        .expect("serialize web_search_call payload")
    }

    /// A backend-reported web-search failure must map to ACP `Failed` (so the
    /// headless `web_search_tool_result_error` branch becomes reachable in
    /// production), while a completed call — or an absent payload — stays
    /// `Completed`. Exercises the real payload shape, not a hand-built status.
    #[test]
    fn backend_failed_web_search_maps_to_failed_status() {
        let failed = web_search_payload(rs::WebSearchToolCallStatus::Failed);
        assert_eq!(failed["status"], "failed", "wire field name is `status`");
        assert_eq!(
            backend_tool_call_status(Some(&failed)),
            acp::ToolCallStatus::Failed
        );

        let completed = web_search_payload(rs::WebSearchToolCallStatus::Completed);
        assert_eq!(
            backend_tool_call_status(Some(&completed)),
            acp::ToolCallStatus::Completed
        );

        // No payload at all is treated as success (behavior-preserving).
        assert_eq!(
            backend_tool_call_status(None),
            acp::ToolCallStatus::Completed
        );
    }
}
