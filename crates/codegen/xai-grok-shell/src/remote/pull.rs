//! Pull-on-miss: fetch a session from the backend and hydrate local JSONL storage.

use crate::remote::client::{BackendClient, BackendError};

#[derive(Debug)]
pub enum PullResult {
    /// Written to local storage. The [`Info`] cwd comes from the backend (may differ from caller's).
    Hydrated(crate::session::info::Info),
    /// Not found on the backend.
    NotFound,
}

/// Fetch a session from the backend and hydrate local JSONL storage.
pub async fn pull_session_to_local(
    session_id: &str,
    client: &BackendClient,
) -> Result<PullResult, BackendError> {
    let loaded = match client.load_session_data(session_id).await {
        Ok(resp) => resp,
        Err(BackendError::SessionNotFound { .. }) => return Ok(PullResult::NotFound),
        Err(e) => return Err(e),
    };

    let remote = match loaded.session.as_ref() {
        Some(s) => s,
        None => return Ok(PullResult::NotFound),
    };

    // cwd required for local dir placement; null means pre-writeback session.
    let cwd = match remote.cwd.as_ref() {
        Some(cwd) => cwd,
        None => {
            tracing::warn!(session_id, "Cannot pull session: backend has cwd=null");
            return Ok(PullResult::NotFound);
        }
    };

    let info = crate::session::info::Info {
        id: agent_client_protocol::SessionId::new(std::sync::Arc::from(session_id)),
        cwd: cwd.clone(),
    };
    let dir = crate::session::persistence::session_dir(&info);

    let num_messages = hydrate::write_to_dir(&dir, &loaded)?;

    tracing::info!(session_id, %cwd, num_messages, "Pulled session from backend");

    Ok(PullResult::Hydrated(info))
}

pub(crate) mod hydrate {
    use std::path::Path;
    use std::sync::Arc;

    use crate::remote::client::{BackendError, LoadDataResponse, LoadedMessage, SessionInfo};
    use crate::session::info::Info;
    use crate::session::persistence::{CHAT_FORMAT_VERSION, Summary, default_model_id};

    fn io_err(path: &Path, source: std::io::Error) -> BackendError {
        BackendError::Hydration {
            path: path.to_path_buf(),
            source,
        }
    }

    /// Write all session files to `dir`.
    pub(super) fn write_to_dir(
        dir: &Path,
        loaded: &LoadDataResponse,
    ) -> Result<usize, BackendError> {
        let remote = loaded
            .session
            .as_ref()
            .expect("caller checked session.is_some()");

        let info = Info {
            id: agent_client_protocol::SessionId::new(Arc::from(remote.session_id.as_str())),
            cwd: remote.cwd.clone().expect("caller verified cwd is Some"),
        };

        std::fs::create_dir_all(dir).map_err(|e| io_err(dir, e))?;

        let num_messages = loaded.messages.as_ref().map_or(0, |m| m.len());
        let mut num_chat_messages = 0;

        if let Some(ref messages) = loaded.messages {
            write_updates(dir, messages)?;
            num_chat_messages = rebuild_chat_history(dir)?;
        }

        write_summary(dir, &info, remote, num_messages, num_chat_messages)?;
        write_remote_origin_marker(dir);

        Ok(num_messages)
    }

    fn write_summary(
        dir: &Path,
        info: &Info,
        remote: &SessionInfo,
        num_messages: usize,
        num_chat_messages: usize,
    ) -> Result<(), BackendError> {
        let meta = remote.metadata.as_ref();

        let model_id = meta
            .and_then(|m| m.get("modelId"))
            .and_then(|v| v.as_str())
            .map(agent_client_protocol::ModelId::new)
            .unwrap_or_else(default_model_id);

        let parent_session_id = meta
            .and_then(|m| m.get("parentSessionId"))
            .and_then(|v| v.as_str())
            .map(String::from);

        let summary = Summary {
            info: info.clone(),
            session_summary: remote.title.clone().unwrap_or_default(),
            created_at: parse_rfc3339_or_now(remote.created_at.as_deref()),
            updated_at: parse_rfc3339_or_now(remote.updated_at.as_deref()),
            num_messages,
            num_chat_messages,
            current_model_id: model_id,
            parent_session_id,
            forked_at: None,
            collection_id: None,
            next_trace_turn: 0,
            chat_format_version: CHAT_FORMAT_VERSION,
            prompt_display_cwd: None,
            session_kind: None,
            fork_context_source: None,
            fork_parent_prompt_id: None,
            inherited_prefix_len: None,
            hidden: None,
            source_workspace_dir: None,
            git_root_dir: None,
            git_remotes: Vec::new(),
            head_commit: None,
            head_branch: None,
            request_id: None,
            // Record the *local* grok_home (where this hydrated copy lives),
            // not the original remote session's, since reconstruction runs locally.
            grok_home: crate::session::persistence::grok_home_string(),
            last_active_at: None,
            generated_title: None,
            title_is_manual: false,
            worktree_label: None,
            agent_name: None,
            // Hydrated locally — record the profile this process runs under.
            sandbox_profile: xai_grok_sandbox::configured_profile_name().map(String::from),
            reasoning_effort: None,
        };

        let json = serde_json::to_string_pretty(&summary)?;
        write_file(&dir.join("summary.json"), json.as_bytes())
    }

    /// Convert backend JSON-RPC messages to local updates.jsonl (replayable methods only).
    pub(super) fn write_updates(
        dir: &Path,
        messages: &[LoadedMessage],
    ) -> Result<(), BackendError> {
        use std::io::Write;

        let path = dir.join("updates.jsonl");
        let file = std::fs::File::create(&path).map_err(|e| io_err(&path, e))?;
        let mut w = std::io::BufWriter::new(file);

        for msg in messages {
            let parsed = match serde_json::from_str::<serde_json::Value>(&msg.content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !is_session_update(&parsed) {
                continue;
            }
            if let Some(line) = to_envelope_line(&parsed) {
                let _ = w.write_all(line.as_bytes());
                let _ = w.write_all(b"\n");
            }
        }

        w.flush().map_err(|e| io_err(&path, e))
    }

    /// Rebuild `chat_history.jsonl` from `updates.jsonl` so pulled sessions are continuable.
    fn rebuild_chat_history(dir: &Path) -> Result<usize, BackendError> {
        use crate::session::storage::UpdatesIterator;
        use std::io::{Seek, Write};

        let updates_path = dir.join("updates.jsonl");
        let Some(iter) =
            UpdatesIterator::open(&updates_path).map_err(|e| io_err(&updates_path, e))?
        else {
            return Ok(0);
        };

        let chat_path = dir.join("chat_history.jsonl");
        let file = std::fs::File::create(&chat_path).map_err(|e| io_err(&chat_path, e))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut reducer = ChatReducer::new();

        for result in iter {
            let update = match result {
                Ok(u) => u,
                Err(_) => continue,
            };

            for item in reducer.process(&update) {
                if let Ok(line) = serde_json::to_string(&item) {
                    let _ = writer.write_all(line.as_bytes());
                    let _ = writer.write_all(b"\n");
                }
            }

            // CompactionCheckpoint: truncate file and reset
            if reducer.should_truncate() {
                reducer.clear_truncate_flag();
                let _ = writer.seek(std::io::SeekFrom::Start(0));
                let _ = writer.get_mut().set_len(0);
            }
        }

        // Flush trailing state
        for item in reducer.flush() {
            if let Ok(line) = serde_json::to_string(&item) {
                let _ = writer.write_all(line.as_bytes());
                let _ = writer.write_all(b"\n");
            }
        }

        writer.flush().map_err(|e| io_err(&chat_path, e))?;
        Ok(reducer.count())
    }

    use crate::sampling::{AssistantItem, ContentPart, ConversationItem, ToolCall};
    use agent_client_protocol as acp;
    use std::collections::{HashMap, HashSet};

    /// Reduces ACP session updates into conversation items.
    ///
    /// Turn boundaries: User→Agent flushes user, Agent→User flushes agent,
    /// tool completion flushes agent before emitting result.
    struct ChatReducer {
        user_parts: Vec<ContentPart>,
        agent_text: String,
        agent_tool_calls: Vec<ToolCall>,

        in_user_turn: bool,
        has_agent_content: bool,
        needs_truncate: bool,

        tool_args: HashMap<String, String>,
        emitted_tool_results: HashSet<String>,
        item_count: usize,
    }

    impl ChatReducer {
        fn new() -> Self {
            Self {
                user_parts: Vec::new(),
                agent_text: String::new(),
                agent_tool_calls: Vec::new(),
                in_user_turn: false,
                has_agent_content: false,
                needs_truncate: false,
                tool_args: HashMap::new(),
                emitted_tool_results: HashSet::new(),
                item_count: 0,
            }
        }

        fn process(
            &mut self,
            update: &crate::session::storage::SessionUpdate,
        ) -> Vec<ConversationItem> {
            use crate::session::storage::SessionUpdate;

            match update {
                SessionUpdate::Acp(n) => self.handle_acp(&n.update),
                SessionUpdate::Xai(n) => self.handle_xai(&n.update),
            }
        }

        fn handle_acp(&mut self, update: &acp::SessionUpdate) -> Vec<ConversationItem> {
            match update {
                acp::SessionUpdate::UserMessageChunk(chunk) => self.on_user_chunk(chunk),
                acp::SessionUpdate::AgentMessageChunk(chunk) => self.on_agent_chunk(chunk),
                acp::SessionUpdate::ToolCall(tc) => self.on_tool_call(tc),
                acp::SessionUpdate::ToolCallUpdate(tc) => self.on_tool_call_update(tc),
                _ => Vec::new(), // AgentThoughtChunk, Retry, Plan not needed
            }
        }

        fn handle_xai(
            &mut self,
            update: &crate::extensions::notification::SessionUpdate,
        ) -> Vec<ConversationItem> {
            use crate::extensions::notification::SessionUpdate as XaiUpdate;

            match update {
                XaiUpdate::CompactionCheckpoint(_) => {
                    self.reset();
                    self.needs_truncate = true;
                    Vec::new()
                }
                _ => Vec::new(), // DiffReview, MemoryFlush, etc. not needed
            }
        }

        fn on_user_chunk(&mut self, chunk: &acp::ContentChunk) -> Vec<ConversationItem> {
            let mut out = Vec::new();

            if !self.in_user_turn {
                out.extend(self.flush_agent());
                self.in_user_turn = true;
            }

            match &chunk.content {
                acp::ContentBlock::Text(t) => {
                    self.user_parts.push(ContentPart::Text {
                        text: std::sync::Arc::<str>::from(t.text.clone()),
                    });
                }
                acp::ContentBlock::Image(img) => {
                    if let Some(uri) = &img.uri {
                        self.user_parts.push(ContentPart::Image {
                            url: std::sync::Arc::<str>::from(uri.clone()),
                        });
                    }
                }
                _ => {} // Audio, Resource, etc. not needed for chat replay
            }

            out
        }

        fn on_agent_chunk(&mut self, chunk: &acp::ContentChunk) -> Vec<ConversationItem> {
            let mut out = Vec::new();

            if self.in_user_turn {
                out.extend(self.flush_user());
                self.in_user_turn = false;
            }

            if let acp::ContentBlock::Text(t) = &chunk.content {
                self.agent_text.push_str(&t.text);
                self.has_agent_content = true;
            }

            out
        }

        fn on_tool_call(&mut self, tc: &acp::ToolCall) -> Vec<ConversationItem> {
            let id = tc.tool_call_id.0.to_string();
            let args = tc
                .raw_input
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default();

            self.tool_args.insert(id.clone(), args.clone());
            self.agent_tool_calls.push(ToolCall {
                id: std::sync::Arc::<str>::from(id),
                name: tc.title.clone(),
                arguments: std::sync::Arc::<str>::from(args),
            });

            Vec::new()
        }

        fn on_tool_call_update(&mut self, tc: &acp::ToolCallUpdate) -> Vec<ConversationItem> {
            let id = tc.tool_call_id.0.to_string();
            self.maybe_backfill_args(&id, &tc.fields);

            if Self::is_completed(&tc.fields) && self.emitted_tool_results.insert(id.clone()) {
                return self.emit_tool_result(&id, &tc.fields);
            }
            Vec::new()
        }

        /// Backfill tool arguments from ToolCallUpdate if ToolCall didn't have them.
        fn maybe_backfill_args(&mut self, id: &str, fields: &acp::ToolCallUpdateFields) {
            let Some(raw) = &fields.raw_input else { return };
            let needs_backfill = self.tool_args.get(id).is_none_or(String::is_empty);
            if !needs_backfill {
                return;
            }

            let args = raw.to_string();
            self.tool_args.insert(id.to_string(), args.clone());

            if let Some(call) = self
                .agent_tool_calls
                .iter_mut()
                .find(|c| c.id.as_ref() == id)
            {
                call.arguments = std::sync::Arc::<str>::from(args);
            }
        }

        fn is_completed(fields: &acp::ToolCallUpdateFields) -> bool {
            matches!(
                fields.status,
                Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed)
            )
        }

        fn emit_tool_result(
            &mut self,
            id: &str,
            fields: &acp::ToolCallUpdateFields,
        ) -> Vec<ConversationItem> {
            let mut out = Vec::new();
            out.extend(self.flush_agent());

            let content = extract_tool_result_text(fields);
            let item = ConversationItem::tool_result(id.to_string(), content);
            self.item_count += 1;
            out.push(item);
            out
        }

        fn flush_user(&mut self) -> Option<ConversationItem> {
            if self.user_parts.is_empty() {
                return None;
            }
            let item = ConversationItem::user_with_parts(std::mem::take(&mut self.user_parts));
            self.item_count += 1;
            Some(item)
        }

        fn flush_agent(&mut self) -> Option<ConversationItem> {
            if !self.has_agent_content && self.agent_tool_calls.is_empty() {
                return None;
            }
            let item = ConversationItem::Assistant(AssistantItem {
                content: std::sync::Arc::<str>::from(std::mem::take(&mut self.agent_text)),
                tool_calls: std::mem::take(&mut self.agent_tool_calls),
                model_id: None,
                model_fingerprint: None,
                reasoning_effort: None,
            });
            self.has_agent_content = false;
            self.item_count += 1;
            Some(item)
        }

        fn flush(&mut self) -> Vec<ConversationItem> {
            let mut out = Vec::new();
            out.extend(self.flush_user());
            out.extend(self.flush_agent());
            out
        }

        fn reset(&mut self) {
            self.user_parts.clear();
            self.agent_text.clear();
            self.agent_tool_calls.clear();
            self.tool_args.clear();
            self.emitted_tool_results.clear();
            self.in_user_turn = false;
            self.has_agent_content = false;
            self.item_count = 0;
        }

        fn should_truncate(&self) -> bool {
            self.needs_truncate
        }

        fn clear_truncate_flag(&mut self) {
            self.needs_truncate = false;
        }

        fn count(&self) -> usize {
            self.item_count
        }
    }

    /// Extract displayable text from a completed ToolCallUpdate.
    fn extract_tool_result_text(fields: &agent_client_protocol::ToolCallUpdateFields) -> String {
        if let Some(content) = &fields.content {
            let text: String = content
                .iter()
                .filter_map(|c| match c {
                    agent_client_protocol::ToolCallContent::Content(
                        agent_client_protocol::Content {
                            content: agent_client_protocol::ContentBlock::Text(t),
                            ..
                        },
                    ) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                return text;
            }
        }
        if let Some(raw) = &fields.raw_output {
            return raw.to_string();
        }
        String::new()
    }

    fn write_remote_origin_marker(dir: &Path) {
        let _ = std::fs::write(
            dir.join(".remote_origin"),
            format!("pulled_at={}\n", chrono::Utc::now().to_rfc3339()),
        );
    }

    /// Replayable JSON-RPC methods (excludes metadata like `prompt_complete`).
    const REPLAYABLE_METHODS: &[&str] = &["session/update", "_x.ai/session/update"];

    fn is_session_update(json_rpc: &serde_json::Value) -> bool {
        json_rpc
            .get("method")
            .and_then(|v| v.as_str())
            .is_some_and(|m| REPLAYABLE_METHODS.contains(&m))
    }

    fn to_envelope_line(json_rpc: &serde_json::Value) -> Option<String> {
        let method = json_rpc.get("method").and_then(|v| v.as_str())?;
        let params = json_rpc.get("params").cloned().unwrap_or_default();

        serde_json::to_string(&serde_json::json!({
            "timestamp": 0u64,
            "method": method,
            "params": params,
        }))
        .ok()
    }

    fn parse_rfc3339_or_now(s: Option<&str>) -> chrono::DateTime<chrono::Utc> {
        s.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now)
    }

    fn write_file(path: &Path, data: &[u8]) -> Result<(), BackendError> {
        std::fs::write(path, data).map_err(|e| io_err(path, e))
    }
}

#[cfg(test)]
mod tests {
    use crate::remote::client::LoadedMessage;

    #[test]
    fn hydrate_writes_valid_updates_jsonl() {
        let tmp = tempfile::TempDir::new().unwrap();
        let messages = vec![
            LoadedMessage {
                id: "1".into(),
                content: r#"{"method":"session/update","params":{"update":"hello"}}"#.into(),
                timestamp: None,
            },
            LoadedMessage {
                id: "2".into(),
                content: r#"{"method":"session/update","params":{"update":"world"}}"#.into(),
                timestamp: None,
            },
        ];

        super::hydrate::write_updates(tmp.path(), &messages).unwrap();

        let content = std::fs::read_to_string(tmp.path().join("updates.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);

        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["timestamp"], 0);
            assert_eq!(v["method"], "session/update");
            assert!(v["params"].is_object());
        }
    }

    #[test]
    fn rebuild_chat_history_merges_chunks() {
        use crate::session::export::ExportedMessage;
        use agent_client_protocol::{ContentBlock, ContentChunk, SessionUpdate, TextContent};
        use std::sync::Arc;

        // Build ACP notifications matching the RemoteSync path
        let sid = agent_client_protocol::SessionId::new(Arc::from("test"));
        let notifications = [
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("hello "),
                ))),
            ),
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("world"),
                ))),
            ),
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("hi back"),
                ))),
            ),
        ];

        // Serialize through ExportedMessage (writeback path)
        let messages: Vec<LoadedMessage> = notifications
            .iter()
            .map(|n| {
                let exported = ExportedMessage::from_notification(n);
                LoadedMessage {
                    id: "x".into(),
                    content: exported.content,
                    timestamp: None,
                }
            })
            .collect();

        let data = crate::remote::client::LoadDataResponse {
            messages: Some(messages),
            session: Some(crate::remote::client::SessionInfo {
                session_id: "test".into(),
                title: None,
                cwd: Some("/tmp".into()),
                status: None,
                created_at: None,
                updated_at: None,
                metadata: None,
            }),
        };
        let tmp = tempfile::TempDir::new().unwrap();
        super::hydrate::write_to_dir(tmp.path(), &data).unwrap();

        let chat = std::fs::read_to_string(tmp.path().join("chat_history.jsonl")).unwrap();
        let items: Vec<crate::sampling::ConversationItem> = chat
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        assert_eq!(items.len(), 2, "should have 1 user + 1 agent item");
        assert!(matches!(
            &items[0],
            crate::sampling::ConversationItem::User(_)
        ));
        assert!(matches!(
            &items[1],
            crate::sampling::ConversationItem::Assistant(_)
        ));
        if let crate::sampling::ConversationItem::User(u) = &items[0] {
            let text: String = u
                .content
                .iter()
                .filter_map(|p| match p {
                    crate::sampling::ContentPart::Text { text } => Some(text.as_ref()),
                    _ => None,
                })
                .collect();
            assert_eq!(text, "hello world");
        }
    }

    #[test]
    fn rebuild_chat_history_preserves_user_images() {
        use crate::session::export::ExportedMessage;
        use agent_client_protocol::{
            ContentBlock, ContentChunk, ImageContent, SessionUpdate, TextContent,
        };
        use std::sync::Arc;

        let sid = agent_client_protocol::SessionId::new(Arc::from("test"));
        let notifications = [
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("look at this"),
                ))),
            ),
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::Image(
                    ImageContent::new(String::new(), String::new())
                        .uri(Some("data:image/png;base64,abc".into())),
                ))),
            ),
            agent_client_protocol::SessionNotification::new(
                sid.clone(),
                SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                    TextContent::new("I see an image"),
                ))),
            ),
        ];

        let messages: Vec<LoadedMessage> = notifications
            .iter()
            .map(|n| LoadedMessage {
                id: "x".into(),
                content: ExportedMessage::from_notification(n).content,
                timestamp: None,
            })
            .collect();

        let data = crate::remote::client::LoadDataResponse {
            messages: Some(messages),
            session: Some(crate::remote::client::SessionInfo {
                session_id: "test".into(),
                title: None,
                cwd: Some("/tmp".into()),
                status: None,
                created_at: None,
                updated_at: None,
                metadata: None,
            }),
        };
        let tmp = tempfile::TempDir::new().unwrap();
        super::hydrate::write_to_dir(tmp.path(), &data).unwrap();

        let chat = std::fs::read_to_string(tmp.path().join("chat_history.jsonl")).unwrap();
        let items: Vec<crate::sampling::ConversationItem> = chat
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        assert_eq!(items.len(), 2);
        if let crate::sampling::ConversationItem::User(u) = &items[0] {
            assert_eq!(u.content.len(), 2, "should have text + image parts");
            assert!(matches!(
                &u.content[0],
                crate::sampling::ContentPart::Text { .. }
            ));
            assert!(matches!(
                &u.content[1],
                crate::sampling::ContentPart::Image { .. }
            ));
        } else {
            panic!("expected User item");
        }
    }

    #[test]
    fn hydrate_skips_invalid_messages() {
        let tmp = tempfile::TempDir::new().unwrap();
        let messages = vec![
            LoadedMessage {
                id: "1".into(),
                content: r#"{"method":"session/update","params":{}}"#.into(),
                timestamp: None,
            },
            LoadedMessage {
                id: "bad".into(),
                content: "not valid json".into(),
                timestamp: None,
            },
            LoadedMessage {
                id: "3".into(),
                content: r#"{"method":"session/update","params":{"x":1}}"#.into(),
                timestamp: None,
            },
        ];

        super::hydrate::write_updates(tmp.path(), &messages).unwrap();

        let content = std::fs::read_to_string(tmp.path().join("updates.jsonl")).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "invalid message should be skipped");
    }
}
