//! Rewind concern for `SessionActor`: rewind points, cross-compaction
//! replay detection, and `handle_rewind`.

use super::*;

struct StagedConversationRewind {
    snapshot: xai_chat_state::ChatStateSnapshot,
    prompt_text: Option<String>,
}

impl SessionActor {
    pub(super) async fn close_rewind_window(&self) {
        let mut state = self.state.lock().await;
        state.rewindable = false;
    }

    /// Returns the `prompt_index → num_file_snapshots` map from the on-disk
    /// snapshot index (independent of the chat-state prompt index). The bridge
    /// joins these onto the server's rewind points.
    pub(super) async fn rewind_file_counts(&self) -> std::collections::HashMap<usize, usize> {
        self.file_state_tracker
            .get_rewind_point_metas()
            .await
            .into_iter()
            .map(|m| (m.prompt_index, m.num_file_snapshots))
            .collect()
    }

    /// Get available rewind points for this session.
    ///
    /// Every prompt is a checkpoint — the list always contains `[0, 1, ..., N-1]`
    /// where N is the current prompt_index. File snapshots may or may not exist
    /// for each checkpoint (indicated by `has_file_changes`).
    pub(super) async fn get_rewind_points(&self) -> RewindPointsResponse {
        // Metadata only — don't materialize the (huge) file-content snapshots
        // just to render the picker.
        let file_metas = self.file_state_tracker.get_rewind_point_metas().await;

        // Query prompt state from the chat state actor.
        let snapshot = self.chat_state_handle.snapshot().await;
        let (prompts, current_prompt_index) = match snapshot {
            Some(ref s) => (s.prompt_texts.clone(), s.prompt_index),
            None => (vec![], 0),
        };

        // Build a lookup of which prompt indices have file snapshots.
        let file_meta_map: std::collections::HashMap<
            usize,
            &xai_grok_workspace::session::file_state::RewindPointMeta,
        > = file_metas.iter().map(|m| (m.prompt_index, m)).collect();

        // Generate a rewind point for every prompt 0..current_prompt_index.
        let rewind_points = (0..current_prompt_index)
            .map(|idx| {
                let prompt_preview = prompts.get(idx).and_then(|text| {
                    let clean_text = extract_user_query(text);
                    let first_line = clean_text
                        .lines()
                        .map(|l| l.trim())
                        .find(|l| !l.is_empty())
                        .unwrap_or("");

                    if first_line.is_empty() {
                        None
                    } else if first_line.chars().count() > 60 {
                        Some(format!("{}...", crate::util::truncate(first_line, 57)))
                    } else {
                        Some(first_line.to_string())
                    }
                });

                let file_meta = file_meta_map.get(&idx);
                let num_file_snapshots = file_meta.map_or(0, |m| m.num_file_snapshots);
                let created_at = file_meta
                    .map(|m| m.created_at.to_rfc3339())
                    .unwrap_or_default();

                RewindPointInfo {
                    prompt_index: idx,
                    created_at,
                    num_file_snapshots,
                    has_file_changes: num_file_snapshots > 0,
                    prompt_preview,
                }
            })
            .collect();

        RewindPointsResponse { rewind_points }
    }

    /// Load user prompts from `updates.jsonl` in chronological order.
    ///
    /// Each `UserMessageChunk` sequence is merged into a single prompt string.
    /// `RewindMarker` entries truncate the list back to the marker's target so
    /// only prompts from the current timeline are returned.
    ///
    /// Uses [`PromptExtractIterator`] which peeks at the `update.sessionUpdate`
    /// discriminant field without fully deserialising every notification.  This
    /// avoids large `acp::SessionNotification` allocations for the many update
    /// types (tool calls, assistant chunks, etc.) that are irrelevant to prompt
    /// extraction.
    pub(super) fn load_user_prompts_from_updates(
        updates_path: &std::path::Path,
    ) -> std::io::Result<Vec<String>> {
        use crate::session::storage::{PromptExtractIterator, collect_prompts_from_events};

        let Some(iter) = PromptExtractIterator::open(updates_path)? else {
            return Ok(vec![]);
        };

        tracing::debug!(
            path = %updates_path.display(),
            "load_user_prompts_from_updates: starting selective scan"
        );

        let prompts = collect_prompts_from_events(iter);

        tracing::debug!(
            prompt_count = prompts.len(),
            "load_user_prompts_from_updates: done"
        );

        Ok(prompts)
    }

    /// Build and validate the complete post-rewind chat snapshot without
    /// mutating chat state or the Code Mode runtime. In particular, checkpoint
    /// replay must succeed before the old JavaScript timeline is invalidated.
    async fn stage_conversation_rewind(
        &self,
        target_index: usize,
        mode: RewindMode,
    ) -> anyhow::Result<Result<StagedConversationRewind, RewindResponse>> {
        let mut snapshot = self
            .chat_state_handle
            .snapshot()
            .await
            .ok_or_else(|| anyhow::anyhow!("chat state unavailable while staging rewind"))?;
        let prompt_text = snapshot.prompt_texts.get(target_index).cloned();
        let mut conversation = snapshot.conversation.clone();
        let mut replay_compaction_marker: Option<Option<usize>> = None;

        if let Some(compaction_at) = snapshot.last_compaction_prompt_index {
            tracing::info!(
                compaction_at,
                "Compaction detected — staging replay for rewind"
            );
            let session_dir = crate::session::persistence::session_dir(&self.session_info);
            let updates_path = session_dir.join("updates.jsonl");
            let replay_result = tokio::task::spawn_blocking(move || {
                crate::session::helpers::replay::replay_to_prompt(
                    &updates_path,
                    &session_dir,
                    target_index,
                )
            })
            .await
            .map_err(|e| anyhow::anyhow!("spawn_blocking panicked: {e}"))?;

            let replay_result = match replay_result {
                Ok(result) => result,
                Err(error) => {
                    tracing::error!(
                        ?error,
                        target_index,
                        "Cross-compaction replay validation failed — rewind aborted"
                    );
                    return Ok(Err(RewindResponse {
                        success: false,
                        target_prompt_index: target_index,
                        mode,
                        reverted_files: vec![],
                        clean_files: vec![],
                        conflicts: vec![],
                        prompt_text: None,
                        error: Some(format!(
                            "Cannot rewind to prompt #{} — compaction checkpoint data is \
                             unavailable ({error}). Try rewinding to a prompt after the \
                             compaction point instead.",
                            target_index,
                        )),
                    }));
                }
            };

            tracing::info!(
                target_index,
                prompt_index_reached = replay_result.prompt_index_reached,
                conversation_len = replay_result.conversation.len(),
                "Cross-compaction rewind staged via replay"
            );
            replay_compaction_marker = Some(replay_result.last_compaction_prompt_index);
            if matches!(
                replay_result.conversation.first(),
                Some(ConversationItem::System(_))
            ) {
                conversation = replay_result.conversation;
            } else {
                // Raw pre-compaction replay contains only turns. Preserve the
                // System prefix and restore the historical user-info payload
                // captured in the checkpoint before appending those turns.
                if let Some(original_user_info) = replay_result.original_user_info {
                    conversation.truncate(1);
                    conversation.push(ConversationItem::user(original_user_info));
                } else {
                    conversation.truncate(2);
                }
                conversation.extend(replay_result.conversation);
            }
        } else {
            let keep_count = conversation_truncate_for_prompt(&conversation, target_index);
            conversation.truncate(keep_count);
        }

        snapshot.conversation = conversation;
        snapshot.prompt_index = target_index;
        snapshot.prompt_texts.truncate(target_index);
        snapshot.last_compaction_prompt_index =
            replay_compaction_marker.unwrap_or(snapshot.last_compaction_prompt_index);

        Ok(Ok(StagedConversationRewind {
            snapshot,
            prompt_text,
        }))
    }

    /// Handle a rewind request with mode support.
    ///
    /// Semantics: "restore state before prompt N ran" — prompts 0..N-1 are kept.
    ///
    /// Modes:
    /// - `All`: roll back both conversation and files (full time-travel)
    /// - `ConversationOnly`: roll back conversation, leave files untouched
    /// - `FilesOnly`: roll back files, leave conversation untouched
    pub(super) async fn handle_rewind(
        &self,
        request: RewindRequest,
    ) -> anyhow::Result<RewindResponse> {
        if !request.force {
            return self.handle_rewind_while_gated(request).await;
        }

        if let Err(blocked) = self
            .begin_lifecycle_mutation(LifecycleMutationKind::Rewind)
            .await
        {
            return Ok(RewindResponse {
                success: false,
                target_prompt_index: request.target_prompt_index,
                mode: request.mode,
                reverted_files: vec![],
                clean_files: vec![],
                conflicts: vec![],
                prompt_text: None,
                error: Some(format!("Cannot rewind while {}.", blocked.message())),
            });
        }

        let result = self.handle_rewind_while_gated(request).await;
        self.end_lifecycle_mutation(LifecycleMutationKind::Rewind)
            .await;
        result
    }

    async fn handle_rewind_while_gated(
        &self,
        request: RewindRequest,
    ) -> anyhow::Result<RewindResponse> {
        let target_index = request.target_prompt_index;
        let mode = request.mode;

        // Validate: target must be less than current prompt_index. FilesOnly
        // reverts the on-disk snapshot index (bounded by `get_rewind_points`,
        // not the conversation), so it is exempt — the chat-state prompt index
        // is empty in bridge mode, where the conversation lives server-side.
        let current_prompt_index = self.chat_state_handle.get_prompt_index().await;
        if mode != RewindMode::FilesOnly && target_index >= current_prompt_index {
            return Ok(RewindResponse {
                success: false,
                target_prompt_index: target_index,
                mode,
                reverted_files: vec![],
                clean_files: vec![],
                conflicts: vec![],
                prompt_text: None,
                error: Some(format!(
                    "Cannot rewind to prompt #{} — current prompt index is {}. \
                     Valid targets: 0..{}",
                    target_index,
                    current_prompt_index,
                    current_prompt_index.saturating_sub(1)
                )),
            });
        }

        let wants_file_revert = matches!(mode, RewindMode::All | RewindMode::FilesOnly);
        let wants_conversation_rewind =
            matches!(mode, RewindMode::All | RewindMode::ConversationOnly);
        // Preflight and de-duplicate file paths once for both preview and
        // commit. The workspace transaction re-checks every path immediately
        // before mutation and compensates prior writes on failure.
        let staged_files = if wants_file_revert {
            Some(
                xai_grok_workspace::session::file_state::stage_file_rewind(
                    &self.file_state_tracker,
                    &self.tool_context.fs,
                    target_index,
                )
                .await
                .map_err(anyhow::Error::new)?,
            )
        } else {
            None
        };
        let clean_files = staged_files
            .as_ref()
            .map(|staged| staged.clean_files().to_vec())
            .unwrap_or_default();
        let conflicts: Vec<RewindConflictInfo> = staged_files
            .as_ref()
            .into_iter()
            .flat_map(|staged| staged.conflicts())
            .map(|conflict| RewindConflictInfo {
                path: conflict.path.clone(),
                conflict_type: match &conflict.conflict_type {
                    xai_grok_workspace::session::file_state::ConflictType::DeletedExternally => {
                        "deleted_externally"
                    }
                    xai_grok_workspace::session::file_state::ConflictType::CreatedExternally => {
                        "created_externally"
                    }
                    xai_grok_workspace::session::file_state::ConflictType::ModifiedExternally => {
                        "modified_externally"
                    }
                }
                .to_string(),
            })
            .collect();

        // ── Preview mode (force=false): pure dry run, no mutations ────
        // Return what WOULD happen so the TUI can show a confirmation
        // modal. Nothing is written, deleted, or truncated.
        if !request.force {
            let error = if !conflicts.is_empty() {
                Some("External modifications detected. Confirm to revert anyway.".to_string())
            } else {
                None
            };
            return Ok(RewindResponse {
                success: false,
                target_prompt_index: target_index,
                mode,
                reverted_files: vec![],
                clean_files,
                conflicts,
                prompt_text: None,
                error,
            });
        }

        // ── Commit mode (force=true): execute the rewind ─────────────

        // Stage every fallible conversation/replay decision first. A missing
        // or corrupt compaction checkpoint must leave both the canonical chat
        // snapshot and the persistent JavaScript timeline untouched.
        let staged_conversation = if wants_conversation_rewind {
            match self.stage_conversation_rewind(target_index, mode).await? {
                Ok(staged) => Some(staged),
                Err(rejected) => return Ok(rejected),
            }
        } else {
            None
        };

        let reverted_files = match staged_files.as_ref() {
            Some(staged) => match staged.apply(&self.tool_context.fs).await {
                Ok(reverted) => reverted,
                Err(error) => {
                    return Ok(RewindResponse {
                        success: false,
                        target_prompt_index: target_index,
                        mode,
                        reverted_files: error.unresolved_paths,
                        clean_files: vec![],
                        conflicts,
                        prompt_text: None,
                        error: Some(error.message),
                    });
                }
            },
            None => Vec::new(),
        };

        if wants_conversation_rewind
            && let Err(error) = self.rebuild_spec.code_mode_runtime.reset().await
        {
            let rollback_failures = match staged_files.as_ref() {
                Some(staged) => staged.rollback(&self.tool_context.fs).await,
                None => Vec::new(),
            };
            let rollback_note = if rollback_failures.is_empty() {
                "file changes were rolled back".to_string()
            } else {
                format!(
                    "file rollback also failed for: {}",
                    rollback_failures.join(", ")
                )
            };
            return Ok(RewindResponse {
                success: false,
                target_prompt_index: target_index,
                mode,
                reverted_files: vec![],
                clean_files: vec![],
                conflicts,
                prompt_text: None,
                error: Some(format!(
                    "Failed to reset the Code Mode runtime: {error}; {rollback_note}. Rewind snapshots were preserved."
                )),
            });
        }

        // Commit the already-validated chat snapshot only after the runtime
        // reset and transactional file phase have succeeded.
        let prompt_text = staged_conversation
            .as_ref()
            .and_then(|staged| staged.prompt_text.clone());
        if let Some(staged) = staged_conversation {
            if self
                .chat_state_handle
                .commit_rewind_snapshot(staged.snapshot)
                .await
                .is_none()
            {
                let rollback_failures = match staged_files.as_ref() {
                    Some(staged) => staged.rollback(&self.tool_context.fs).await,
                    None => Vec::new(),
                };
                anyhow::bail!(
                    "chat state unavailable while committing rewind; file rollback failures: {:?}",
                    rollback_failures
                );
            }
            if let Ok(mut pending) = self.rewind_pending_prompt.lock() {
                *pending = staged.prompt_text.clone();
            }

            // Conversation shrank — clear budget-based (size/schema) and stale
            // per-turn suppression so compaction can run against the smaller context.
            // Account-state suppression (credit/auth → SUPPRESS_UNTIL_SUCCESS) isn't
            // budget-related, so it persists until a successful model call.
            if self
                .compaction
                .auto_compact_suppressed
                .load(std::sync::atomic::Ordering::Relaxed)
                != crate::session::compaction_config::SUPPRESS_UNTIL_SUCCESS
            {
                self.compaction.auto_compact_suppressed.store(
                    crate::session::compaction_config::SUPPRESS_NONE,
                    std::sync::atomic::Ordering::Relaxed,
                );
            }

            // Append a RewindMarker to updates.jsonl so the replay pipeline can
            // handle timeline branching (updates.jsonl is append-only).
            self.persist_xai_update_only(XaiSessionUpdate::RewindMarker {
                target_prompt_index: target_index,
                created_at: chrono::Utc::now().to_rfc3339(),
            });
        }

        // Update the file state tracker to reflect the rewind.
        if wants_file_revert {
            // All/FilesOnly: files were reverted, snapshots are stale — truncate.
            self.file_state_tracker.truncate_from(target_index).await;
            let _ = self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::TruncateRewindPoints {
                    from_index: target_index,
                });
        } else if wants_conversation_rewind {
            // ConversationOnly: files are untouched but the conversation is rewound.
            self.merge_rewind_tracker_from(target_index).await;
        }

        // Preserve FIFO ordering with the persistence actor before reporting
        // the already-committed in-memory/file result. `FlushAndAck` is an
        // ordering barrier (the storage layer logs individual I/O failures); a
        // closed persistence channel cannot safely turn this into a retryable
        // rewind error because replaying now would mutate the new timeline a
        // second time.
        let (flush_tx, flush_rx) = tokio::sync::oneshot::channel();
        if self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::FlushAndAck {
                respond_to: flush_tx,
            })
            .is_err()
            || flush_rx.await.is_err()
        {
            tracing::warn!(
                session_id = %self.session_info.id.0,
                target_prompt_index = target_index,
                "rewind committed, but the persistence ordering barrier was unavailable"
            );
        }

        // Feedback should reflect a committed timeline change only. Previews,
        // invalid targets, failed replay validation, and runtime-reset failures
        // all return before this point.
        self.signals_handle().mark_reverted();

        Ok(RewindResponse {
            success: true,
            target_prompt_index: target_index,
            mode,
            reverted_files,
            clean_files: vec![],
            conflicts,
            prompt_text,
            error: None,
        })
    }

    /// `ConversationOnly` rewind-tracker bookkeeping: merge the discarded
    /// prompts' file effects (`>= target_index`) into the previous rewind point
    /// so that (a) `/rewind 0` can still undo all file changes, and (b) a new
    /// prompt at `target_index` gets a fresh rewind point whose before-snapshots
    /// reflect current disk state. Files and the conversation are left untouched.
    ///
    /// Updates the in-memory tracker, then persists via a disk-authoritative
    /// merge so a lazily-unloaded or partial tracker can't truncate history off
    /// disk. No normalize_to_relative needed: per-turn persistence already
    /// normalized the on-disk points (turn.rs, before PersistenceMsg::RewindPoint).
    ///
    /// Shared by local `handle_rewind` (ConversationOnly) and the bridge-mode
    /// ConversationOnly path, whose conversation rewind lands server-side
    /// (SessionCommand::ReconcileRewindTracker).
    pub(super) async fn merge_rewind_tracker_from(&self, target_index: usize) {
        self.file_state_tracker
            .merge_and_remove_from(target_index)
            .await;
        let _ = self
            .notifications
            .persistence_tx
            .send(PersistenceMsg::MergeRewindPointsFrom { target_index });
    }

    /// Out-of-band history repair (`x.ai/session/repair`) for a resident
    /// session: run `xai_chat_state::compaction_utils::repair_history` inside
    /// the chat-state actor, then flush persistence so `chat_history.jsonl`
    /// is rewritten on disk before the caller sees success.
    ///
    /// Refused while a turn is in flight (in-flight tool calls legitimately
    /// await their results). The refusal is enforced inside the chat-state
    /// actor's command handler — see `ChatStateCommand::RepairHistory` for
    /// why a caller-side check alone would race turn start; the check below
    /// is just a fast path.
    pub(super) async fn handle_repair_history(
        &self,
        dry_run: bool,
    ) -> anyhow::Result<xai_chat_state::compaction_utils::HistoryRepairReport> {
        // Per-session flag — NOT `tool_context.is_turn_active`, which is the
        // agent-wide coordinator flag shared by all sessions (using it would
        // refuse repair of an idle session while any other session runs a
        // turn, and another session's turn end could clear it mid-turn).
        let turn_flag = self.session_turn_active.clone();
        if turn_flag.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!(xai_chat_state::commands::RepairHistoryBlocked);
        }

        let report = self
            .chat_state_handle
            .repair_history(dry_run, Some(turn_flag))
            .await
            .ok_or_else(|| anyhow::anyhow!("chat-state actor unavailable"))?
            .map_err(anyhow::Error::new)?;

        if report.changed() && !dry_run {
            // Flush barrier: success must mean the rewrite is on disk.
            let (flush_tx, flush_rx) = tokio::sync::oneshot::channel();
            if self
                .notifications
                .persistence_tx
                .send(PersistenceMsg::FlushAndAck {
                    respond_to: flush_tx,
                })
                .is_err()
                || flush_rx.await.is_err()
            {
                anyhow::bail!("history repaired in memory but the persistence flush failed");
            }
            tracing::warn!(
                session_id = %self.session_info.id.0,
                duplicates_removed = report.duplicates_removed,
                stripped_tool_result_ids = ?report.stripped_tool_result_ids,
                synthetic_results_inserted = report.synthetic_results_inserted,
                "session history repaired"
            );
        }

        Ok(report)
    }
}
