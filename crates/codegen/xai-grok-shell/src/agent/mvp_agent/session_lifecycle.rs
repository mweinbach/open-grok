//! Session lifecycle, roster deltas, and the idle-session supervisor for [`MvpAgent`].
//! Co-located `#[path]`-style child of `mvp_agent` (`use super::*`) so the `impl`
//! block keeps access to `MvpAgent`'s private fields.
use super::*;
impl MvpAgent {
    /// Ask a live session actor to shut down.
    pub(crate) fn request_session_shutdown(&self, id: &acp::SessionId) {
        if let Some(handle) = self.sessions.borrow().get(id) {
            let _ = handle.cmd_tx.send(SessionCommand::Shutdown);
        }
    }
    /// Finalize the cloud session replica (fire-and-forget, "Hook 4").
    ///
    /// Marks the session **done** upstream, so this MUST only run on a genuine
    /// session end — a terminal/explicit close (`x.ai/session/close`). It must
    /// NOT run on a mere client disconnect or a dead-actor reap: those leave the
    /// conversation resumable on disk, and finalizing would wrongly mark a still
    /// running/resumable session "done".
    pub(super) fn finalize_session_replica(&self, id: &acp::SessionId) {
        #[cfg(test)]
        self.finalize_spy.borrow_mut().push(id.0.to_string());
        if let Some(client) = self.session_registry_client() {
            let sid = id.0.to_string();
            tokio::spawn(async move {
                if let Err(e) = client.finalize(&sid).await {
                    tracing::warn!(
                        error = % e, "session registry finalize failed (non-fatal)"
                    );
                }
            });
        }
    }
    /// Remove a session and its thread handle **without** finalizing the cloud
    /// replica.
    ///
    /// Used for dead-actor reaping and idle-unload: the conversation stays
    /// resumable on disk, so it must NOT be marked "done" upstream. Genuine
    /// terminal closes go through [`MvpAgent::close_session_explicit`]. Also
    /// drops the `session_live_state` entry so that map stays bounded.
    pub(crate) fn remove_session(&self, id: &acp::SessionId) {
        self.sessions.borrow_mut().remove(id);
        self.prompt_intake_locks.borrow_mut().remove(id);
        self.session_threads.borrow_mut().remove(id);
        self.session_index_claims.borrow_mut().remove(id);
        self.require_gateway_sessions.borrow_mut().remove(id);
        self.session_live_state.borrow_mut().remove(id);
    }
    /// Get-or-create the per-session prompt-intake lock (see
    /// [`Self::prompt_intake_locks`]). Cheap clone of the shared `Rc`.
    pub(super) fn prompt_intake_lock(
        &self,
        id: &acp::SessionId,
    ) -> std::rc::Rc<tokio::sync::Mutex<()>> {
        self.prompt_intake_locks
            .borrow_mut()
            .entry(id.clone())
            .or_default()
            .clone()
    }
    /// Close a session in response to an **explicit** terminal close
    /// (`x.ai/session/close`). Finalizes the cloud replica (genuine session
    /// end), then removes the session terminally as `Completed`.
    pub(crate) fn close_session_explicit(&self, id: &acp::SessionId) {
        self.finalize_session_replica(id);
        self.remove_session_terminal(id, SessionLiveState::Completed);
    }
    /// Record the coarse lifecycle state for a session.
    pub(super) fn set_session_live_state(&self, id: &acp::SessionId, state: SessionLiveState) {
        self.session_live_state
            .borrow_mut()
            .insert(id.clone(), state);
    }
    /// Read the recorded lifecycle state for a session (test observability).
    #[cfg(test)]
    pub(super) fn session_live_state_for(&self, id: &acp::SessionId) -> Option<SessionLiveState> {
        self.session_live_state.borrow().get(id).copied()
    }
    /// Roster-delta hook for a terminally removed session. Broadcasts an
    /// `x.ai/sessions/changed` notification with the session in `removed` so
    /// every attached dashboard drops the row promptly. Also
    /// records the call site (and the terminal state) for test observability,
    /// since the `session_live_state` entry is dropped on removal.
    pub(super) fn record_roster_delta(&self, id: &acp::SessionId, final_state: SessionLiveState) {
        #[cfg(test)]
        self.roster_delta_spy
            .borrow_mut()
            .push((id.0.to_string(), final_state));
        tracing::debug!(
            session_id = % id.0, ? final_state, "roster delta: session removed"
        );
        self.emit_roster_changed(Vec::new(), vec![id.0.to_string()]);
    }
    /// Roster-delta hook for a newly-resident / changed session. Broadcasts an
    /// `x.ai/sessions/changed` notification with the current entry in
    /// `upserted` so dashboards add/refresh the row.
    pub(crate) fn push_roster_delta_upserted(&self, id: &acp::SessionId) {
        if let Some(entry) = self.resident_roster_entry(id) {
            self.emit_roster_changed(vec![entry], Vec::new());
        }
    }
    /// Emit an `x.ai/sessions/changed` upsert for a resident session with an
    /// explicit `activity`, so every attached dashboard reflects a
    /// turn-boundary transition (Working / Idle / NeedsInput) *immediately*
    /// rather than waiting for the ≤1s roster poll (deltas are emitted
    /// at turn-start/turn-end). Without this, a viewer client that holds no
    /// local `AgentView` for the session only learns its activity from the
    /// poll, so a turn driven by another client shows as `Idle` for up to a
    /// poll interval — and not at all while that viewer's poll is dormant.
    ///
    /// The `activity` is supplied by the caller rather than read from
    /// `resident_activity` because at turn-start the actor may not have
    /// published `current_prompt_id` yet (it is set asynchronously once the
    /// actor dequeues the `SessionCommand::Prompt`), so a natural read would
    /// still observe `Idle`. The authoritative entry (cwd / worktree / model /
    /// yolo) is built by `resident_roster_entry`, so it never diverges from
    /// the polled entry; only the `activity` field is overridden.
    pub(super) fn push_roster_activity_delta(
        &self,
        id: &acp::SessionId,
        activity: crate::agent::roster::RosterActivity,
    ) {
        if let Some(mut entry) = self.resident_roster_entry(id) {
            entry.activity = activity;
            self.emit_roster_changed(vec![entry], Vec::new());
        }
    }
    /// Fan an `x.ai/sessions/changed` delta out to every attached client.
    ///
    /// This is a roster-wide notification (no `sessionId`), so the leader IPC
    /// server broadcasts it to all clients rather than routing by session (see
    /// the `x.ai/sessions/changed` special-case in `leader/server.rs`).
    pub(super) fn emit_roster_changed(
        &self,
        upserted: Vec<crate::agent::roster::RosterEntry>,
        removed: Vec<String>,
    ) {
        if upserted.is_empty() && removed.is_empty() {
            return;
        }
        let payload = crate::agent::roster::RosterChanged { upserted, removed };
        if let Ok(params) = serde_json::value::to_raw_value(&payload) {
            self.gateway
                .forward_fire_and_forget(acp::ExtNotification::new(
                    crate::agent::roster::SESSIONS_CHANGED_METHOD,
                    params.into(),
                ));
        }
    }
    /// Coarse activity of a resident session for the dashboard status column.
    ///
    /// Precedence: a non-empty pending-interaction map →
    /// `NeedsInput` (wins even over a running turn — a session awaiting a
    /// permission *mid-turn* is "needs input"); else a running turn →
    /// `Working`; else map the coarse `SessionLiveState`.
    pub(super) fn resident_activity(
        &self,
        id: &acp::SessionId,
    ) -> crate::agent::roster::RosterActivity {
        use crate::agent::roster::RosterActivity;
        let (needs_input, turn_running) = self
            .sessions
            .borrow()
            .get(id)
            .map(|h| {
                let needs_input = h
                    .pending_interactions
                    .lock()
                    .map(|g| !g.is_empty())
                    .unwrap_or(false);
                let turn_running = h
                    .current_prompt_id
                    .lock()
                    .map(|g| g.is_some())
                    .unwrap_or(false);
                (needs_input, turn_running)
            })
            .unwrap_or((false, false));
        if needs_input {
            return RosterActivity::NeedsInput;
        }
        if turn_running {
            return RosterActivity::Working;
        }
        match self.session_live_state.borrow().get(id).copied() {
            Some(SessionLiveState::Completed) => RosterActivity::Completed,
            Some(SessionLiveState::DeadFailed) => RosterActivity::Dead,
            Some(SessionLiveState::Dormant) => RosterActivity::Dormant,
            _ => RosterActivity::Idle,
        }
    }
    /// Build a single roster entry for a resident session, or `None` if it is
    /// not currently resident.
    pub(super) fn resident_roster_entry(
        &self,
        id: &acp::SessionId,
    ) -> Option<crate::agent::roster::RosterEntry> {
        let session_id = id.0.to_string();
        let (cwd, is_worktree, model_id, reasoning_effort, yolo) = {
            let sessions = self.sessions.borrow();
            let h = sessions.get(id)?;
            (
                h.display_cwd.clone().unwrap_or_else(|| h.info.cwd.clone()),
                h.display_cwd.is_some(),
                Some(h.model_id.0.to_string()),
                h.reasoning_effort,
                h.yolo_mode,
            )
        };
        Some(crate::agent::roster::RosterEntry {
            title: self
                .resident_roster_titles
                .borrow()
                .get(&session_id)
                .cloned(),
            session_id,
            cwd,
            is_worktree,
            model_id,
            reasoning_effort,
            yolo,
            activity: self.resident_activity(id),
            resident: true,
            last_change_unix_ms: chrono::Utc::now().timestamp_millis(),
            origin: crate::agent::roster::RosterOrigin::Local,
        })
    }
    /// Snapshot all resident sessions as roster entries (synchronous; no disk).
    pub(super) fn resident_roster_entries(&self) -> Vec<crate::agent::roster::RosterEntry> {
        let ids: Vec<acp::SessionId> = self.sessions.borrow().keys().cloned().collect();
        ids.iter()
            .filter_map(|id| self.resident_roster_entry(id))
            .collect()
    }
    /// Build the full roster: resident actors plus recently-touched on-disk
    /// (`Dormant`) sessions. Resident wins on an id collision; hidden sessions
    /// are excluded.
    pub(crate) async fn build_roster(&self) -> Vec<crate::agent::roster::RosterEntry> {
        let resident = self.resident_roster_entries();
        let summaries = crate::session::persistence::list_recent_summaries(200)
            .await
            .unwrap_or_default();
        let entries = crate::agent::roster::merge_roster(resident, summaries);
        self.cache_resident_titles(&entries);
        entries
    }
    /// Refresh `resident_roster_titles` from the freshly-built roster.
    pub(super) fn cache_resident_titles(&self, entries: &[crate::agent::roster::RosterEntry]) {
        *self.resident_roster_titles.borrow_mut() = entries
            .iter()
            .filter(|e| e.resident)
            .filter_map(|e| Some((e.session_id.clone(), e.title.clone()?)))
            .collect();
    }
    /// Terminally remove a session: emit the roster delta with its final state,
    /// then drop it from all maps (no finalize — callers that need finalize do
    /// it first, see `close_session_explicit`).
    pub(super) fn remove_session_terminal(
        &self,
        id: &acp::SessionId,
        final_state: SessionLiveState,
    ) {
        self.record_roster_delta(id, final_state);
        self.remove_session(id);
    }
    /// Reap a session whose **resident** actor thread exited unexpectedly
    /// (panic / load failure). Demotes it to `DeadFailed`, emits the roster
    /// delta, and removes it WITHOUT finalize — the conversation persists on
    /// disk and stays resumable (reaping a dead actor is harmless;
    /// it demotes to Dormant).
    pub(super) fn reap_dead_session(&self, id: &acp::SessionId) {
        self.remove_session_terminal(id, SessionLiveState::DeadFailed);
    }
    /// Sweep `session_threads` for finished threads and clean them up.
    ///
    /// A finished thread has two distinct meanings, and conflating them
    /// corrupts the `SessionLiveState` roster source:
    ///
    /// - **Still resident in `sessions`** → the actor exited unexpectedly while
    ///   the session was hosted (panic / load failure). Reap as `DeadFailed`.
    /// - **Not resident** (already idle-unloaded → `Dormant`, or explicitly
    ///   closed) → this is the *expected* clean exit. The `SessionThread` was
    ///   kept only so `drain_old_session_thread` could wait on it; now that it
    ///   has finished there is nothing left to drain, so just drop the leftover
    ///   `SessionThread`/state entries. Do **not** demote to `DeadFailed` and do
    ///   **not** emit a second roster delta.
    ///
    /// `JoinHandle::is_finished()` is non-blocking and cannot distinguish a
    /// clean exit from a panic on its own, which is exactly why the residency
    /// check is required. Runs both opportunistically and from the join-handle
    /// supervisor (`ensure_session_supervisor`).
    pub(super) fn sweep_dead_sessions(&self) {
        let dead: Vec<acp::SessionId> = self
            .session_threads
            .borrow()
            .iter()
            .filter(|(_, t)| t.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            if self.sessions.borrow().contains_key(&id) {
                tracing::warn!(
                    session_id = % id.0,
                    "Resident session actor exited unexpectedly; reaping as DeadFailed"
                );
                self.reap_dead_session(&id);
            } else {
                self.session_threads.borrow_mut().remove(&id);
                self.session_live_state.borrow_mut().remove(&id);
                tracing::debug!(
                    session_id = % id.0,
                    "Reaped finished thread for non-resident session (clean exit)"
                );
            }
        }
    }
    /// Start the join-handle supervisor. **Idempotent.**
    ///
    /// A single `spawn_local` task periodically reaps actor threads that have
    /// exited (panicked or finished) so a dead actor never lingers as a roster
    /// zombie. `std::thread::JoinHandle` is not awaitable, so we poll
    /// `is_finished()` on a tick — the same mechanism `drain_old_session_thread`
    /// and `sweep_dead_sessions` already use. A panicked actor is therefore
    /// reaped within one [`SESSION_SUPERVISOR_TICK`].
    ///
    /// The sweep body is wrapped in `catch_unwind` so a single panicking sweep
    /// can never terminate the loop (which would silently disable reaping for
    /// the rest of the process). The task holds a `LocalRef` (raw pointer) to
    /// `self` for the lifetime of the `LocalSet`; this is sound because the
    /// agent owns the `LocalSet` and outlives it (same contract as
    /// `start_subagent_coordinator`), and `LocalRef` is `!Send`.
    pub(super) fn ensure_session_supervisor(&self) {
        if self.supervisor_started.replace(true) {
            return;
        }
        #[cfg(test)]
        self.supervisor_spawn_count
            .set(self.supervisor_spawn_count.get() + 1);
        let agent_ref = LocalRef::new(self);
        tokio::task::spawn_local(async move {
            loop {
                tokio::time::sleep(SESSION_SUPERVISOR_TICK).await;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    agent_ref.get().sweep_dead_sessions();
                }));
                if result.is_err() {
                    tracing::error!("session supervisor sweep panicked; continuing supervision");
                }
            }
        });
    }
    /// Coarse "any work pending" check for the idle-unload stub.
    /// Returns `true` while the session has work in flight.
    ///
    /// Three layers:
    /// 1. **Fast path (sync):** the shared `current_prompt_id` slot, which the
    ///    actor sets while a turn is running (`maybe_start_running_task`) and
    ///    clears via its RAII guard. A poisoned lock is treated as busy → never
    ///    unload.
    /// 1b. **Parked plan-approval (sync):** the shared `pending_interactions`
    ///    slot. The parked plan-approval resume re-park is the one outstanding work with no
    ///    running turn, so it needs its own sync check (the same shared-`Arc`
    ///    idiom as `current_prompt_id`) rather than the async round-trip below.
    /// 2. **Queue check (async):** when no turn is running, the actor is between
    ///    turns and responsive, so we ask it whether `pending_inputs` is
    ///    non-empty (a prompt queued at the turn boundary). This closes the
    ///    sub-tick window where `current_prompt_id` is momentarily `None` but a
    ///    queued input is about to be drained. On timeout we keep the session
    ///    resident (conservative).
    ///
    /// TODO(PR-4): once the aggregate `SessionActivity` signal exists, also
    /// consult the autonomous background sources so a detached session is never
    /// idle-unloaded (→ `Shutdown` → `KillOnDrop`) while they are live:
    /// `monitor_event_buffer`, pending scheduler fires,
    /// `ToolContext.background_tasks`, and background subagent sessions. Until
    /// then those background-only sessions rely on the keep-resident default and
    /// the `current_prompt_id` auto-wake turn being active.
    ///
    /// TODO(PR-4): this is also inherently a *check-then-act* across the
    /// actor-thread boundary — work can arrive (a new `Prompt`/auto-wake) in the
    /// gap between this `IsBusy` answer and the caller's subsequent `Shutdown`,
    /// so an idle-unload can still race a just-arrived turn. The actor processes
    /// its mailbox in order, so the lost work is bounded and recoverable on
    /// reload; PR-4 closes the gap properly by gating the unload inside the
    /// actor (a single `Unload`-if-idle command) rather than check-then-send.
    pub(super) async fn session_has_live_work(&self, id: &acp::SessionId) -> bool {
        let Some(handle) = self.sessions.borrow().get(id).cloned() else {
            return false;
        };
        let turn_running = handle
            .current_prompt_id
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(true);
        if turn_running {
            return true;
        }
        if crate::session::pending_interaction::has_parked_plan_approval(
            &handle.pending_interactions,
        ) {
            return true;
        }
        tokio::time::timeout(IDLE_QUERY_TIMEOUT, handle.is_busy())
            .await
            .unwrap_or(true)
    }
}
