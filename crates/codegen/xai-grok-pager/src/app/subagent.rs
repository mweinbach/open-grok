//! Tracking state for spawned child sessions.
//! [`SubagentInfo`] is the single source of truth, used by both the subagent pane (display) and the permission view (provenance labels).
//!
//! # Child-transcript replay and eviction
//!
//! - **replay**: read a child's persisted `updates.jsonl` and apply it to that child's view.
//!   [`ensure_subagent_child_replayed`] runs on fullscreen open and dashboard attach.
//!   [`replay_resumed_child_before_live_block`] runs only through its single funnel,
//!   the [`child_view_for_live_update_mut`](crate::app::agent_view::AgentView::child_view_for_live_update_mut) accessor.
//!   The funnel reads a resumed child's inherited history before its first live block overwrites the prompt-only window.
//! - **evict**: drop a finished child's retained view once disk is proven able to rebuild it ([`evict_finished_child_view`]).
//!
//! The ordering rule both depend on: a replay may only append to a view that *shows nothing but the task prompt*.
//! Disk history can therefore never land after a live block.
//! A finished foreground child is reset to that state first; a child that is still running, or a background child, waits instead.
//! The spawn path itself never reads the child transcript (the MB-scale `updates.jsonl`), so a burst of spawns cannot block the UI thread.
//! The small `meta.json` enrichment ([`enrich_from_meta`]) is a separate, bounded read.

use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;
use xai_grok_shell::session::storage::{
    ReplayEmission, ReplayLookupFallback, ReplayPathHint, ReplayedUpdate, replay_would_emit,
    stream_replay_updates_at_hinted,
};

/// Enriched subagent tracking info, keyed by `child_session_id` in `AgentView::subagent_sessions`.
#[derive(Debug, Clone)]
pub struct SubagentInfo {
    pub subagent_id: Arc<str>,
    pub child_session_id: Arc<str>,
    pub description: Arc<str>,
    pub subagent_type: Arc<str>,
    pub persona: Option<Arc<str>>,
    pub role: Option<Arc<str>>,
    pub model: Option<Arc<str>>,
    /// "new" or "resumed".
    pub context_source: Option<Arc<str>>,
    pub resumed_from: Option<Arc<str>>,
    /// "read-only", "read-write", "execute", or "all".
    pub capability_mode: Option<Arc<str>>,
    pub workflow_run_id: Option<Arc<str>>,
    /// Whether the context was normalized into `<background_context>`.
    pub context_normalized: bool,
    pub parent_prompt_id: Option<Arc<str>>,
    pub swarm_id: Option<Arc<str>>,
    pub started_at: Instant,
    /// Latest progress/finish update, else `started_at`; the dashboard's "last activity" sort key.
    pub last_progress_at: Instant,
    /// One terminal transition per child: a duplicate finish must not re-finalize and a duplicate spawn must not replace this state.
    pub finished: bool,

    /// Terminal status from `SubagentFinished`: "completed", "failed", or "cancelled".
    pub status: Option<Arc<str>>,
    pub error: Option<Arc<str>>,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: Option<u64>,
    pub tool_calls: Option<u32>,
    pub turns: Option<u32>,

    /// Live progress from `SubagentProgress`.
    pub turn_count: Option<u32>,
    pub tool_call_count: Option<u32>,
    pub tokens_used: Option<u64>,
    pub context_window_tokens: Option<u64>,
    /// 0-100.
    pub context_usage_pct: Option<u8>,
    pub tools_used: Vec<Arc<str>>,
    pub error_count: Option<u32>,
    /// Live activity label ("Thinking", "Running: cargo build") for the tasks pane and dashboard; cleared on `SubagentFinished`.
    pub activity_label: Option<String>,

    /// Affects scrollback rendering (background shows "started:"/"completed:").
    pub is_background: bool,

    /// Set on kill request, cleared on `SubagentFinished`.
    pub pending_kill: bool,
    /// Auto-clears `pending_kill` after a timeout so the user can retry if the kill notification is lost.
    pub kill_requested_at: Option<Instant>,

    /// Set on spawn, updated on finish.
    pub scrollback_entry_id: Option<crate::scrollback::entry::EntryId>,

    /// Enriched from the on-disk `meta.json`.
    pub prompt: Option<Arc<str>>,
    pub child_cwd: Option<Arc<str>>,
    pub worktree_path: Option<Arc<str>>,

    pub(crate) transcript: ChildTranscript,
}

/// Where a child's authoritative transcript lives.
/// One state feeds both the replay-on-open and the eviction decision, so the two cannot drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ChildTranscript {
    /// No disk copy proven yet: the next fullscreen open replays `updates.jsonl`.
    /// A failed read stays here, so a lagging persistence flush is retried.
    /// So does an empty read of a finished child, or of a still-running resumed child whose inherited history is expected on disk.
    #[default]
    NeedsReplay,
    /// An emitting replay proved disk reproduces the transcript: the retained view may be dropped and rebuilt.
    DiskBacked,
    /// A replay of a still-running child that inherits nothing found an empty disk.
    /// The result is cached so later opens skip the relocation scan.
    /// [`Self::retry_disk_after_finish`] grants one more try once the child is terminal and disk is final.
    /// A resumed child never caches here: its inherited history is expected on disk, so an empty read stays `NeedsReplay` to retry.
    DiskEmptyWhileRunning,
    /// The in-memory view is the only copy (disk resolved to nothing while the view held content), so evicting it would lose the transcript.
    MemoryOnly,
}

/// Disk is only final once the child is terminal.
/// An empty read means "not written yet" for a running child and "nothing was ever written" for a finished one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildLifecycle {
    Running,
    Finished,
}

/// Whether the child's transcript is expected to already exist on disk.
/// A resumed child inherits its source's persisted history, copied into its session dir at spawn.
/// An empty read while it runs is therefore transient ("not visible yet").
/// A fresh or forked child starts with an empty replay transcript, so an empty read is a settled negative worth caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildOrigin {
    Resumed,
    Fresh,
}

impl ChildTranscript {
    pub(crate) fn needs_replay(self) -> bool {
        matches!(self, Self::NeedsReplay)
    }

    pub(crate) fn evictable(self) -> bool {
        matches!(self, Self::DiskBacked)
    }

    /// Only an emitting replay proves the disk copy.
    /// A failed read stays `NeedsReplay` so the next open retries.
    /// An empty read caches the negative result only for a still-running child that inherits nothing.
    /// A resumed child's inherited history is expected on disk, so its empty-while-running read is transient and must stay `NeedsReplay`.
    fn record_replay(
        &mut self,
        outcome: &std::io::Result<ReplayEmission>,
        lifecycle: ChildLifecycle,
        origin: ChildOrigin,
    ) {
        debug_assert!(self.needs_replay());
        match (outcome, lifecycle, origin) {
            (Ok(ReplayEmission::Emitted), _, _) => *self = Self::DiskBacked,
            (Ok(ReplayEmission::Empty), ChildLifecycle::Running, ChildOrigin::Fresh) => {
                *self = Self::DiskEmptyWhileRunning
            }
            (Ok(ReplayEmission::Empty), ChildLifecycle::Running, ChildOrigin::Resumed)
            | (Ok(ReplayEmission::Empty), ChildLifecycle::Finished, _)
            | (Err(_), _, _) => {}
        }
    }

    /// The child is terminal, so disk is final and the cached empty read is worth one more try.
    /// A proven `DiskBacked` or `MemoryOnly` state is untouched.
    pub(crate) fn retry_disk_after_finish(&mut self) {
        if matches!(self, Self::DiskEmptyWhileRunning) {
            *self = Self::NeedsReplay;
        }
    }

    /// The view was reset to the task-prompt baseline: rebuild on next open.
    pub(crate) fn evicted(&mut self) {
        debug_assert!(
            !matches!(self, Self::MemoryOnly),
            "evicting a MemoryOnly transcript would lose its only copy"
        );
        *self = Self::NeedsReplay;
    }

    pub(crate) fn discovered_memory_only(&mut self) {
        debug_assert!(
            !self.evictable(),
            "must not downgrade a proven DiskBacked copy to MemoryOnly"
        );
        *self = Self::MemoryOnly;
    }
}

impl SubagentInfo {
    pub fn is_running(&self) -> bool {
        !self.finished
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// Uses the authoritative `duration_ms` from `SubagentFinished` when available, else the live wall-clock elapsed.
    pub fn display_elapsed(&self) -> std::time::Duration {
        if self.finished {
            self.duration_ms
                .map(std::time::Duration::from_millis)
                .unwrap_or_else(|| self.elapsed())
        } else {
            self.elapsed()
        }
    }
}

/// Pager-side slice of the shell's on-disk `SubagentMeta`.
#[derive(Debug, Deserialize)]
struct SubagentMetaSlice {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    child_cwd: Option<String>,
    #[serde(default)]
    worktree_path: Option<String>,
}

/// Grok home for the replay path (overridable in tests).
#[cfg(not(test))]
fn effective_grok_home() -> std::path::PathBuf {
    xai_grok_shell::util::grok_home::grok_home()
}

#[cfg(test)]
thread_local! {
    static REPLAY_OPENGROK_HOME: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Override grok home for disk-replay unit tests (thread-local).
#[cfg(test)]
pub(crate) fn set_replay_grok_home_for_tests(home: Option<std::path::PathBuf>) {
    REPLAY_OPENGROK_HOME.with(|h| *h.borrow_mut() = home);
}

#[cfg(test)]
fn effective_grok_home() -> std::path::PathBuf {
    if let Some(home) = REPLAY_OPENGROK_HOME.with(|h| h.borrow().clone()) {
        return home;
    }
    xai_grok_shell::util::grok_home::grok_home()
}

/// Best-effort enrichment from the shell's on-disk `meta.json`.
pub(crate) fn enrich_from_meta(
    info: &mut SubagentInfo,
    parent_cwd: &std::path::Path,
    parent_session_id: &str,
) {
    enrich_from_meta_with_home(info, &effective_grok_home(), parent_cwd, parent_session_id);
}

fn enrich_from_meta_with_home(
    info: &mut SubagentInfo,
    grok_home: &std::path::Path,
    parent_cwd: &std::path::Path,
    parent_session_id: &str,
) {
    let meta_path = grok_home
        .join("sessions")
        .join(urlencoding::encode(&parent_cwd.to_string_lossy()).as_ref())
        .join(parent_session_id)
        .join("subagents")
        .join(info.subagent_id.as_ref())
        .join("meta.json");

    let content = match std::fs::read_to_string(&meta_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "meta.json not found");
            return;
        }
    };

    let meta: SubagentMetaSlice = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "meta.json parse failed");
            return;
        }
    };

    info.prompt = meta.prompt.map(Arc::from);
    info.child_cwd = meta.child_cwd.map(Arc::from);
    info.worktree_path = meta.worktree_path.map(Arc::from);
}

/// Best-effort streamed replay of a child's inherited conversation.
///
/// `Err`: the read failed, so callers must not mark the child replayed.
/// `Ok(Empty)`: nothing on disk, so callers holding detached content restore it.
/// The `child_cwd` hint skips the full relocation scan when it matches.
fn replay_inherited_updates(
    child_view: &mut crate::app::agent_view::AgentView,
    child_session_id: &str,
    parent_cwd: &std::path::Path,
    child_cwd: Option<&std::path::Path>,
    fallback: ReplayLookupFallback,
) -> std::io::Result<ReplayEmission> {
    let home = effective_grok_home();
    let hint = ReplayPathHint {
        parent_cwd: Some(parent_cwd),
        child_cwd,
        fallback,
    };
    #[cfg(test)]
    test_support::record_transcript_read();

    child_view.scrollback.begin_batch();
    let outcome =
        stream_replay_updates_at_hinted(child_session_id, &home, hint, |update| match update {
            ReplayedUpdate::Acp(update, meta) => {
                let mut meta = crate::acp::meta::NotificationMeta::from_json(meta.as_ref());
                meta.is_replay = true;
                child_view
                    .session
                    .handle_update(update, &meta, &mut child_view.scrollback);
            }
            ReplayedUpdate::Xai(update) => {
                crate::app::acp_handler::apply_child_view_session_event(child_view, &update, false);
            }
        });
    child_view.scrollback.end_batch();
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(e) => {
            tracing::warn!(session_id = %child_session_id, error = %e, "failed to read updates for replay");
            return Err(e);
        }
    };

    if outcome == ReplayEmission::Emitted {
        crate::memory_release::release_retained_memory_with("subagent-replay");
    }
    Ok(outcome)
}

/// Counts `updates.jsonl` open attempts (per thread) so a test can assert a path did no disk work.
#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::Cell;

    thread_local! {
        static TRANSCRIPT_READS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn record_transcript_read() {
        TRANSCRIPT_READS.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn transcript_reads() -> usize {
        TRANSCRIPT_READS.with(Cell::get)
    }

    /// Baseline [`super::SubagentInfo`] fixture: a running, foreground, non-resumed "explore" child.
    /// It is shared by the `#[path]`-included test modules (`subagent_tests`, `subagent_format_tests`).
    pub(crate) fn make_info() -> super::SubagentInfo {
        super::SubagentInfo {
            subagent_id: "sa-1".into(),
            child_session_id: "cs-1".into(),
            description: "test task".into(),
            subagent_type: "explore".into(),
            persona: None,
            role: None,
            model: None,
            context_source: None,
            resumed_from: None,
            capability_mode: None,
            workflow_run_id: None,
            context_normalized: false,
            parent_prompt_id: None,
            swarm_id: None,
            started_at: std::time::Instant::now(),
            last_progress_at: std::time::Instant::now(),
            finished: false,
            status: None,
            error: None,
            duration_ms: None,
            tool_calls: None,
            turns: None,
            turn_count: None,
            tool_call_count: None,
            tokens_used: None,
            context_window_tokens: None,
            context_usage_pct: None,
            tools_used: Vec::new(),
            error_count: None,
            activity_label: None,
            is_background: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            prompt: None,
            child_cwd: None,
            worktree_path: None,
            transcript: Default::default(),
        }
    }
}

/// True when a scrollback holds nothing beyond injected task prompts.
fn scrollback_is_prompt_only(scrollback: &crate::scrollback::state::ScrollbackState) -> bool {
    let len = scrollback.len();
    if len == 0 {
        return true;
    }
    for i in 0..len {
        let Some(entry) = scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            crate::scrollback::block::RenderBlock::UserPrompt(_) => {}
            _ => return false,
        }
    }
    true
}

/// True when a scrollback holds only injected prompts plus the `TurnCompleted` footer.
/// A rebuild recreates that content, so it must not pin the view `MemoryOnly`.
fn scrollback_is_prompt_and_footer_only(
    scrollback: &crate::scrollback::state::ScrollbackState,
) -> bool {
    for i in 0..scrollback.len() {
        let Some(entry) = scrollback.entry(i) else {
            continue;
        };
        match &entry.block {
            crate::scrollback::block::RenderBlock::UserPrompt(_) => {}
            crate::scrollback::block::RenderBlock::SessionEvent(b)
                if matches!(
                    b.event,
                    crate::scrollback::blocks::SessionEvent::TurnCompleted { .. }
                ) => {}
            _ => return false,
        }
    }
    true
}

/// What [`ensure_subagent_child_replayed`] did with a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum ChildReplayOutcome {
    /// The replay emitted content; the disk copy is now recorded `DiskBacked`.
    Replayed,
    /// The read succeeded but found nothing on disk yet; the transcript stays unsettled so a later open retries.
    FoundNothingOnDisk,
    /// The read failed; the transcript stays `NeedsReplay` to retry.
    ReadFailed,
    /// The transcript is already accounted for, so nothing was read.
    NothingToRead,
    /// A running or background view already holds live blocks; disk is not read.
    ViewHoldsLiveBlocks,
    /// No `SubagentInfo` or no view under this id (pruned tab, stale id).
    UnknownChild,
}

/// Replay child `updates.jsonl` on fullscreen open (and dashboard attach) when not yet read.
/// A finished foreground child always rebuilds from disk.
/// A running or background view is filled only while it still shows nothing but the task prompt.
pub(crate) fn ensure_subagent_child_replayed(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) -> ChildReplayOutcome {
    let Some(info) = parent.subagent_sessions.get(child_sid) else {
        return ChildReplayOutcome::UnknownChild;
    };
    if !info.transcript.needs_replay() {
        return ChildReplayOutcome::NothingToRead;
    }
    let finished = info.finished;
    let is_background = info.is_background;
    let resumed = is_resumed_child(info);
    let finished_elapsed = finished
        .then_some(info.duration_ms)
        .flatten()
        .map(std::time::Duration::from_millis);
    let Some(child_view) = parent.subagent_views.get(child_sid) else {
        return ChildReplayOutcome::UnknownChild;
    };
    if (!finished || is_background) && !scrollback_is_prompt_only(&child_view.scrollback) {
        tracing::debug!(
            child_session_id = %child_sid,
            finished,
            is_background,
            "skipping child transcript replay: the view already holds live blocks"
        );
        return ChildReplayOutcome::ViewHoldsLiveBlocks;
    }
    let detached_state = if finished && !is_background {
        let detached_state = reset_child_view_to_prompt(parent, child_sid);
        debug_assert!(
            parent
                .subagent_views
                .get(child_sid)
                .is_none_or(|view| scrollback_is_prompt_only(&view.scrollback)),
            "the reset must leave the view showing nothing but the task prompt, \
             or the replay below appends disk history after live blocks"
        );
        detached_state
    } else {
        None
    };
    let fallback = if (finished && !is_background) || resumed {
        ReplayLookupFallback::Relocation
    } else {
        ReplayLookupFallback::HintedOnly
    };
    let outcome = replay_child_and_record_outcome(parent, child_sid, fallback);
    restore_or_finalize_after_replay(
        parent,
        child_sid,
        &outcome,
        detached_state,
        finished_elapsed,
    );
    match outcome {
        Ok(ReplayEmission::Emitted) => ChildReplayOutcome::Replayed,
        Ok(ReplayEmission::Empty) => ChildReplayOutcome::FoundNothingOnDisk,
        Err(_) => ChildReplayOutcome::ReadFailed,
    }
}

/// The tail of [`ensure_subagent_child_replayed`].
/// Given the replay outcome and the pre-reset detached content, it either restores that content or stamps the finished footer.
/// Content is restored when the read emitted nothing but the view held real blocks.
/// A read error, or a detached view that was only a prompt plus footer, is left dropped and `NeedsReplay` so the next open retries.
fn restore_or_finalize_after_replay(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
    outcome: &std::io::Result<ReplayEmission>,
    detached_state: Option<crate::app::agent_view::ReplayRebuiltState>,
    finished_elapsed: Option<std::time::Duration>,
) {
    let restore = match outcome {
        Ok(ReplayEmission::Emitted) => false,
        Ok(ReplayEmission::Empty) => detached_state
            .as_ref()
            .is_some_and(|t| !scrollback_is_prompt_and_footer_only(&t.scrollback)),
        Err(_) => true,
    };
    let mut restored = false;
    if restore
        && let Some(detached_state) = detached_state
        && let Some(child_view) = parent.subagent_views.get_mut(child_sid)
    {
        child_view.restore_replay_rebuilt_state(detached_state);
        restored = true;
        if matches!(outcome, Ok(ReplayEmission::Empty))
            && let Some(info) = parent.subagent_sessions.get_mut(child_sid)
        {
            info.transcript.discovered_memory_only();
        }
    }
    let parent_turn_running =
        parent.session.state.is_turn_running() || parent.session.state.is_cancelling();
    if let Some(child_view) = parent.subagent_views.get_mut(child_sid) {
        match finished_elapsed {
            Some(elapsed) if outcome.is_ok() && !restored => {
                finalize_finished_child_view(child_view, elapsed)
            }
            Some(_) => {}
            None if !parent_turn_running => {
                child_view.scrollback.finish_all_running();
            }
            None => {}
        }
    }
}

fn is_resumed_child(info: &SubagentInfo) -> bool {
    info.resumed_from.is_some() || info.context_source.as_deref() == Some("resumed")
}

/// Read a resumed child's inherited transcript into its view before the first live block lands.
/// A resumed child's source transcript is copied into its session dir, and the live stream never repeats it.
/// The first live block would therefore close the replay window for good.
/// A non-resumed child needs nothing: its `updates.jsonl` only ever holds blocks the live stream already delivered.
///
/// Idempotent and self-gating: only a resumed child still in `NeedsReplay` with a prompt-only view is filled.
/// Its sole caller is [`child_view_for_live_update_mut`](crate::app::agent_view::AgentView::child_view_for_live_update_mut).
/// Every apply that can be a resumed child's *first* live block routes through that accessor.
/// Any new code path that can push a resumed child's first block MUST go through it too.
pub(crate) fn replay_resumed_child_before_live_block(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) {
    let Some(info) = parent.subagent_sessions.get(child_sid) else {
        return;
    };
    if !info.transcript.needs_replay() || !is_resumed_child(info) {
        return;
    }
    if !parent
        .subagent_views
        .get(child_sid)
        .is_some_and(|view| scrollback_is_prompt_only(&view.scrollback))
    {
        return;
    }
    let _ = ensure_subagent_child_replayed(parent, child_sid);
}

/// Replay the child's on-disk transcript and record what the read proved on [`SubagentInfo::transcript`] (see [`ChildTranscript::record_replay`]).
fn replay_child_and_record_outcome(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
    fallback: ReplayLookupFallback,
) -> std::io::Result<ReplayEmission> {
    let parent_cwd = parent.session.cwd.clone();
    let child_cwd = parent
        .subagent_sessions
        .get(child_sid)
        .and_then(|info| info.child_cwd.clone());
    let mut outcome = Ok(ReplayEmission::Empty);
    if let Some(child_view) = parent.subagent_views.get_mut(child_sid) {
        outcome = replay_inherited_updates(
            child_view,
            child_sid,
            &parent_cwd,
            child_cwd.as_deref().map(std::path::Path::new),
            fallback,
        );
    }
    if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
        let lifecycle = if info.finished {
            ChildLifecycle::Finished
        } else {
            ChildLifecycle::Running
        };
        let origin = if is_resumed_child(info) {
            ChildOrigin::Resumed
        } else {
            ChildOrigin::Fresh
        };
        info.transcript.record_replay(&outcome, lifecycle, origin);
    }
    outcome
}

/// Reset a child view to the resume-state baseline: detach every replay-rebuilt field, drop the media caches, and re-inject the task prompt.
/// `expect_user_echo` lets a later replay dedup the persisted echo against this injected prompt.
///
/// Returns the detached state so a rebuild that emitted nothing can restore it losslessly (eviction drops it instead).
#[must_use = "dropping the detached state destroys the only in-memory copy; eviction must drop it explicitly"]
fn reset_child_view_to_prompt(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) -> Option<crate::app::agent_view::ReplayRebuiltState> {
    let prompt = parent
        .subagent_sessions
        .get(child_sid)
        .and_then(|info| info.prompt.clone())
        .filter(|p| !p.trim().is_empty());
    let child_view = parent.subagent_views.get_mut(child_sid)?;
    let detached = child_view.take_replay_rebuilt_state();
    child_view.inline_media_cache = Default::default();
    child_view.inline_media_load_failed = Default::default();
    if let Some(prompt) = prompt {
        child_view
            .scrollback
            .push_block(crate::scrollback::block::RenderBlock::user_prompt(
                prompt.as_ref(),
            ));
        child_view.session.tracker.expect_user_echo();
    }
    Some(detached)
}

/// Whether [`evict_finished_child_view`] dropped the retained view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub(crate) enum EvictOutcome {
    /// The retained transcript was dropped; the first open rebuilds from disk.
    Evicted,
    /// A guard applied (open fullscreen, unfinished or background, memory-only, or an unproven disk probe); the caller must finalize in place.
    Retained,
}

/// Evict a finished child view's retained transcript (scrollback, tracker, caches); the first open rebuilds it from disk, footer included.
/// Without this every finished child is retained for the whole process.
///
/// Returns [`EvictOutcome::Retained`] when a guard applies and the caller must finalize in place.
/// The guards: the child open fullscreen, unfinished or background children, and memory-only transcripts.
/// A view holding content is dropped only once a disk probe proves the persisted transcript would emit.
/// A raced or missing flush therefore cannot lose the only copy.
pub(crate) fn evict_finished_child_view(
    parent: &mut crate::app::agent_view::AgentView,
    child_sid: &str,
) -> EvictOutcome {
    if parent.active_subagent.as_deref() == Some(child_sid) {
        return EvictOutcome::Retained;
    }
    let Some(info) = parent.subagent_sessions.get(child_sid) else {
        return EvictOutcome::Retained;
    };
    if !info.finished
        || info.is_background
        || matches!(info.transcript, ChildTranscript::MemoryOnly)
    {
        return EvictOutcome::Retained;
    }
    let Some(child_view) = parent.subagent_views.get(child_sid) else {
        if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
            info.transcript.evicted();
        }
        return EvictOutcome::Evicted;
    };
    let had_content = !scrollback_is_prompt_only(&child_view.scrollback)
        || !child_view.inline_media_cache.is_empty();
    if !info.transcript.evictable() && had_content {
        let child_cwd = info.child_cwd.clone();
        let hint = ReplayPathHint {
            parent_cwd: Some(&parent.session.cwd),
            child_cwd: child_cwd.as_deref().map(std::path::Path::new),
            fallback: ReplayLookupFallback::HintedOnly,
        };
        if !replay_would_emit(child_sid, &effective_grok_home(), hint).unwrap_or(false) {
            return EvictOutcome::Retained;
        }
    }
    if let Some(info) = parent.subagent_sessions.get_mut(child_sid) {
        info.transcript.evicted();
    }
    drop(reset_child_view_to_prompt(parent, child_sid));
    if had_content {
        crate::memory_release::request_release_after_draw_with("subagent-evict");
    }
    EvictOutcome::Evicted
}

/// Finalize a finished child view: end the turn and append the `TurnCompleted` footer.
///
/// Idempotent on the *trailing* footer: a re-finalized child must not get a second completed line.
/// An earlier turn's `TurnCompleted` deeper in the transcript must not suppress a later turn's footer.
pub(crate) fn finalize_finished_child_view(
    child_view: &mut crate::app::agent_view::AgentView,
    elapsed: std::time::Duration,
) {
    child_view
        .session
        .tracker
        .finish_turn(&mut child_view.scrollback);
    child_view.scrollback.finish_all_running();
    let already_has_trailing_completed_footer = child_view.scrollback.last().is_some_and(|e| {
        matches!(
            &e.block,
            crate::scrollback::block::RenderBlock::SessionEvent(seb)
                if matches!(
                    seb.event,
                    crate::scrollback::blocks::SessionEvent::TurnCompleted { .. }
                )
        )
    });
    if already_has_trailing_completed_footer {
        return;
    }
    child_view
        .scrollback
        .push_block(crate::scrollback::block::RenderBlock::session_event(
            crate::scrollback::blocks::SessionEvent::TurnCompleted {
                elapsed: Some(elapsed),
            },
        ));
}

fn join_meta_parts(parts: &[Option<&str>]) -> String {
    let non_empty: Vec<&str> = parts.iter().copied().flatten().collect();
    if non_empty.is_empty() {
        String::new()
    } else {
        non_empty.join(" \u{00b7} ")
    }
}

/// Collapse `(persona, role)` to one label when both name the same title.
/// Whitespace-only input counts as absent; the compare is ASCII (registry slugs).
fn dedup_persona_role<'a, 'b>(
    persona: Option<&'a str>,
    role: Option<&'b str>,
) -> (Option<&'a str>, Option<&'b str>) {
    let persona = persona.filter(|s| !s.trim().is_empty());
    let role = role.filter(|s| !s.trim().is_empty());
    match (persona, role) {
        (Some(p), Some(r)) if p.trim().eq_ignore_ascii_case(r.trim()) => (Some(p), None),
        _ => (persona, role),
    }
}

pub(crate) fn format_type_label(subagent_type: &str) -> &str {
    match subagent_type {
        "general-purpose" => "general",
        other => other,
    }
}

pub(crate) fn format_context_badge(info: &SubagentInfo) -> &str {
    match info.context_source.as_deref() {
        Some("resumed") => "resumed",
        Some("forked") => "forked",
        _ => "",
    }
}

/// Returns `(Some(tag), rest_after_close_bracket)` when the description begins with `[<non-empty>]`, else `(None, description)` unchanged.
pub(crate) fn parse_tag_prefix(description: &str) -> (Option<&str>, &str) {
    if let Some(rest) = description.strip_prefix('[')
        && let Some(close) = rest.find(']')
    {
        let tag = rest[..close].trim();
        if !tag.is_empty() {
            return (Some(tag), rest[close + 1..].trim_start());
        }
    }
    (None, description)
}

/// Single consolidated label and display description for a subagent row.
/// The description always has the `[tag]` prefix stripped, used as the label or not, so callers never render bracket noise inline.
pub(crate) fn format_subagent_label(info: &SubagentInfo) -> (String, String) {
    let (tag, clean_desc) = parse_tag_prefix(&info.description);

    let raw_label = if let Some(p) = info
        .persona
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        p.to_string()
    } else if let Some(r) = info
        .role
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        r.to_string()
    } else if info.subagent_type.as_ref() != "general-purpose" {
        format_type_label(&info.subagent_type).to_string()
    } else if let Some(tag) = tag {
        tag.to_string()
    } else {
        "general".to_string()
    };

    let mut chars = raw_label.chars();
    let label = match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => raw_label,
    };

    (label, clean_desc.to_string())
}

pub(crate) fn format_subagent_meta(
    persona: Option<&str>,
    role: Option<&str>,
    model: Option<&str>,
) -> String {
    let (persona, role) = dedup_persona_role(persona, role);
    let bare = join_meta_parts(&[persona, role, model]);
    if bare.is_empty() {
        bare
    } else {
        format!(" ({bare})")
    }
}

/// Concise display label for the subagent scrollback block and the fullscreen title bar.
/// Callers handle the `None` activity separately.
pub(crate) fn format_activity_label(activity: &crate::acp::tracker::TurnActivity) -> String {
    use crate::acp::tracker::TurnActivity;
    match activity {
        TurnActivity::Thinking => "Thinking".to_string(),
        TurnActivity::Responding => "Responding".to_string(),
        TurnActivity::ToolRunning { title, description } => {
            if let Some(desc) = description
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                crate::acp::tracker::format_waiting_for_subject(desc)
            } else if title.is_empty() {
                "Running tool".to_string()
            } else {
                let first_line = title.lines().next().unwrap_or(title);
                let max_len = crate::acp::tracker::MAX_ACTIVITY_SUBJECT_CHARS;
                if first_line.len() <= max_len {
                    format!("Running: {first_line}")
                } else {
                    let char_count = first_line.chars().count();
                    if char_count <= max_len {
                        format!("Running: {first_line}")
                    } else {
                        let truncated: String = first_line.chars().take(max_len).collect();
                        format!("Running: {truncated}\u{2026}")
                    }
                }
            }
        }
        TurnActivity::AutoCompacting => "Compacting".to_string(),
        TurnActivity::Retrying {
            attempt,
            max_retries,
            ..
        } => crate::app::error_display::retry_clause(
            *attempt,
            *max_retries,
            crate::app::error_display::RetryLabelStyle::Compact,
        ),
        TurnActivity::WritingToolCall(writing) => writing.label(),
        TurnActivity::Waiting(reason) => reason.label(),
    }
}

#[cfg(test)]
#[path = "subagent_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "subagent_format_tests.rs"]
mod format_tests;

/// Compare two strings after collapsing internal whitespace (no allocation).
pub(crate) fn subagent_prompt_text_eq(a: &str, b: &str) -> bool {
    let mut aw = a.split_whitespace();
    let mut bw = b.split_whitespace();
    loop {
        match (aw.next(), bw.next()) {
            (Some(x), Some(y)) if x == y => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}
/// True when replay (or prior injection) already surfaced the subagent task prompt.
pub(crate) fn child_scrollback_already_shows_prompt(
    scrollback: &crate::scrollback::state::ScrollbackState,
    prompt: &str,
) -> bool {
    if prompt.trim().is_empty() {
        return false;
    }
    for i in 0..scrollback.len() {
        let Some(entry) = scrollback.entry(i) else {
            continue;
        };
        let block_text = match &entry.block {
            crate::scrollback::block::RenderBlock::UserPrompt(b) => Some(b.text.as_str()),
            _ => None,
        };
        if let Some(t) = block_text
            && subagent_prompt_text_eq(t, prompt)
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
fn subagent_child_needs_replay(child_view: &crate::app::agent_view::AgentView) -> bool {
    scrollback_is_prompt_only(&child_view.scrollback)
}

#[cfg(test)]
mod legacy_tests {
    use super::*;
    use crate::acp::meta::NotificationMeta;
    use crate::acp::model_state::ModelState;
    use crate::acp::tracker::AcpUpdateTracker;
    use crate::app::agent::{AgentId, AgentSession, AgentState};
    use crate::app::agent_view::AgentView;
    use crate::scrollback::block::RenderBlock;
    use crate::scrollback::state::ScrollbackState;
    use agent_client_protocol as acp;
    use std::collections::{BTreeMap, HashMap, VecDeque};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Instant;
    fn make_info() -> SubagentInfo {
        SubagentInfo {
            subagent_id: "sa-1".into(),
            child_session_id: "cs-1".into(),
            description: "test task".into(),
            subagent_type: "explore".into(),
            persona: None,
            role: None,
            model: None,
            context_source: None,
            resumed_from: None,
            capability_mode: None,
            workflow_run_id: None,
            context_normalized: false,
            parent_prompt_id: None,
            swarm_id: None,
            started_at: Instant::now(),
            last_progress_at: Instant::now(),
            finished: false,
            status: None,
            error: None,
            duration_ms: None,
            tool_calls: None,
            turns: None,
            turn_count: None,
            tool_call_count: None,
            tokens_used: None,
            context_window_tokens: None,
            context_usage_pct: None,
            tools_used: Vec::new(),
            error_count: None,
            activity_label: None,
            is_background: false,
            pending_kill: false,
            kill_requested_at: None,
            scrollback_entry_id: None,
            prompt: None,
            child_cwd: None,
            worktree_path: None,
            transcript: ChildTranscript::NeedsReplay,
        }
    }
    fn make_min_child_view() -> AgentView {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let session = AgentSession {
            id: AgentId(0),
            acp_tx: tx,
            session_id: Some(acp::SessionId::new(Arc::from("child"))),
            models: ModelState::default(),
            state: AgentState::Idle,
            tracker: AcpUpdateTracker::new(),
            cwd: PathBuf::from("/tmp"),
            is_worktree: false,
            forked_from: None,
            pending_prompts: VecDeque::new(),
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
            hook_block_hold: false,
            blocked_prompt: None,
            provider_rebind_pending: false,
            user_model_preference: None,
            deferred_model_switch: None,
            bg_tasks: BTreeMap::new(),
            bg_tool_call_to_task: HashMap::new(),
            scheduled_tasks: HashMap::new(),
            in_flight_prompt: None,
            compact_held_prompt: None,
            current_prompt_id: None,
            created_via_new: false,
        };
        AgentView::new(session, ScrollbackState::new())
    }
    fn seed_tool_call(view: &mut AgentView) {
        view.session.tracker.handle_update(
            acp::SessionUpdate::ToolCall(
                acp::ToolCall::new(acp::ToolCallId::new(Arc::from("tc1")), "Read foo")
                    .kind(acp::ToolKind::Other)
                    .status(acp::ToolCallStatus::Pending)
                    .content(vec![])
                    .locations(vec![]),
            ),
            &NotificationMeta::default(),
            &mut view.scrollback,
        );
    }
    #[test]
    fn child_scrollback_already_shows_prompt_matches_user_prompt() {
        let mut view = make_min_child_view();
        view.scrollback
            .push_block(RenderBlock::user_prompt("  scan src/  \n"));
        assert!(child_scrollback_already_shows_prompt(
            &view.scrollback,
            "scan src/"
        ));
    }
    #[test]
    fn child_scrollback_already_shows_prompt_false_when_absent() {
        let view = make_min_child_view();
        assert!(!child_scrollback_already_shows_prompt(
            &view.scrollback,
            "scan src/"
        ));
    }
    #[test]
    fn child_scrollback_already_shows_prompt_false_for_empty_needle() {
        let mut view = make_min_child_view();
        view.scrollback
            .push_block(RenderBlock::user_prompt("anything"));
        assert!(!child_scrollback_already_shows_prompt(&view.scrollback, ""));
        assert!(!child_scrollback_already_shows_prompt(
            &view.scrollback,
            "   "
        ));
    }
    #[test]
    fn subagent_child_needs_replay_empty_scrollback() {
        let view = make_min_child_view();
        assert!(subagent_child_needs_replay(&view));
    }
    #[test]
    fn subagent_child_needs_replay_prompt_only() {
        let mut view = make_min_child_view();
        view.scrollback
            .push_block(RenderBlock::user_prompt("scan src/"));
        assert!(subagent_child_needs_replay(&view));
    }
    #[test]
    fn subagent_child_needs_replay_false_when_tool_call_present() {
        let mut view = make_min_child_view();
        seed_tool_call(&mut view);
        assert!(!subagent_child_needs_replay(&view));
    }
    #[test]
    fn subagent_child_needs_replay_false_when_prompt_and_tool_call() {
        let mut view = make_min_child_view();
        view.scrollback
            .push_block(RenderBlock::user_prompt("scan src/"));
        seed_tool_call(&mut view);
        assert!(!subagent_child_needs_replay(&view));
    }
    #[test]
    fn ensure_subagent_child_replayed_skips_when_spawn_flag_set() {
        let mut parent = make_min_child_view();
        let child_sid = "child-skip";
        let mut child = make_min_child_view();
        child
            .scrollback
            .push_block(RenderBlock::user_prompt("task only"));
        parent
            .subagent_views
            .insert(child_sid.to_string(), Box::new(child));
        let mut info = make_info();
        info.child_session_id = child_sid.into();
        info.transcript = ChildTranscript::DiskBacked;
        parent.subagent_sessions.insert(child_sid.to_string(), info);
        ensure_subagent_child_replayed(&mut parent, child_sid);
        let child = parent.subagent_views.get(child_sid).unwrap();
        assert_eq!(child.scrollback.len(), 1);
        assert!(matches!(
            child.scrollback.entry(0).unwrap().block,
            RenderBlock::UserPrompt(_)
        ));
    }
    /// The child-transcript replay purges exactly once when it actually
    /// parsed an `updates.jsonl` transient — and never when the load no-ops
    /// (missing file) or the open takes the already-replayed skip path. The
    /// purge lives inside `replay_inherited_updates` so BOTH producers (the
    /// eager live-spawn path and this deferred first-open path) are covered.
    #[test]
    fn ensure_subagent_child_replayed_releases_retained_memory_once() {
        use crate::memory_release::test_support;
        test_support::install_counting_hook();
        let child_sid = "child-purge-real";
        let home = tempfile::tempdir().unwrap();
        let session_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        let tool_line = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"tool_call","toolCallId":"tc1","title":"Read foo","kind":"read","locations":[{{"path":"/tmp/foo"}}]}}}}}}"#
        );
        std::fs::write(session_dir.join("updates.jsonl"), tool_line + "\n").unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut parent = make_min_child_view();
        parent
            .subagent_views
            .insert(child_sid.to_string(), Box::new(make_min_child_view()));
        let mut info = make_info();
        info.child_session_id = child_sid.into();
        parent.subagent_sessions.insert(child_sid.to_string(), info);
        let before = test_support::calls();
        ensure_subagent_child_replayed(&mut parent, child_sid);
        assert_eq!(
            test_support::calls(),
            before + 1,
            "a real replay must purge after the parsed transient drops"
        );
        assert!(
            !parent.subagent_sessions[child_sid]
                .transcript
                .needs_replay(),
            "fixture sanity: the replay attempt must mark the child replayed"
        );
        let before = test_support::calls();
        ensure_subagent_child_replayed(&mut parent, child_sid);
        assert_eq!(
            test_support::calls(),
            before,
            "the skip path allocates nothing and must not purge"
        );
        let ghost_sid = "child-purge-ghost";
        parent
            .subagent_views
            .insert(ghost_sid.to_string(), Box::new(make_min_child_view()));
        let mut ghost = make_info();
        ghost.child_session_id = ghost_sid.into();
        parent
            .subagent_sessions
            .insert(ghost_sid.to_string(), ghost);
        let before = test_support::calls();
        ensure_subagent_child_replayed(&mut parent, ghost_sid);
        assert_eq!(
            test_support::calls(),
            before,
            "a no-op replay (missing transcript) must not purge"
        );
        assert!(
            !parent.subagent_sessions[ghost_sid]
                .transcript
                .needs_replay()
        );
        let empty_sid = "child-purge-empty";
        let empty_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(empty_sid);
        std::fs::create_dir_all(&empty_dir).unwrap();
        std::fs::write(empty_dir.join("summary.json"), "{}").unwrap();
        std::fs::write(empty_dir.join("updates.jsonl"), "").unwrap();
        parent
            .subagent_views
            .insert(empty_sid.to_string(), Box::new(make_min_child_view()));
        let mut empty = make_info();
        empty.child_session_id = empty_sid.into();
        parent
            .subagent_sessions
            .insert(empty_sid.to_string(), empty);
        let before = test_support::calls();
        ensure_subagent_child_replayed(&mut parent, empty_sid);
        assert_eq!(
            test_support::calls(),
            before,
            "an empty replay (zero updates parsed) must not purge"
        );
        assert!(
            !parent.subagent_sessions[empty_sid]
                .transcript
                .needs_replay()
        );
        set_replay_grok_home_for_tests(None);
    }
    #[test]
    fn replay_inherited_updates_batches_and_collapses_tools() {
        let home = tempfile::tempdir().unwrap();
        let child_sid = "child-batch";
        let session_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        let user = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"user_message_chunk","content":{{"type":"text","text":"go"}}}}}}}}"#
        );
        let tool = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"tool_call","toolCallId":"t1","title":"bash","kind":"execute","status":"pending"}}}}}}"#
        );
        let ip = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"in_progress","content":[{{"type":"text","text":"out"}}]}}}}}}"#
        );
        let done = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{{"type":"text","text":"out"}}]}}}}}}"#
        );
        let agent_msg = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"ok"}}}}}}}}"#
        );
        std::fs::write(
            session_dir.join("updates.jsonl"),
            format!("{user}\n{tool}\n{ip}\n{done}\n{agent_msg}\n"),
        )
        .unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut view = make_min_child_view();
        let _ = replay_inherited_updates(
            &mut view,
            child_sid,
            std::path::Path::new("/tmp"),
            None,
            ReplayLookupFallback::Relocation,
        );
        assert!(
            !view.scrollback.in_batch(),
            "end_batch must run after streamed apply"
        );
        assert_eq!(
            view.scrollback.turn_count(),
            1,
            "end_batch must rebuild turns once after the stream"
        );
        let tools = (0..view.scrollback.len())
            .filter(|i| {
                view.scrollback
                    .entry(*i)
                    .is_some_and(|e| matches!(e.block, RenderBlock::ToolCall(_)))
            })
            .count();
        assert_eq!(tools, 1, "ToolCall+updates must collapse to one block");
        set_replay_grok_home_for_tests(None);
    }
    #[test]
    fn replay_inherited_updates_ends_batch_on_read_error() {
        let home = tempfile::tempdir().unwrap();
        let child_sid = "child-read-err";
        let session_dir = home
            .path()
            .join("sessions")
            .join(urlencoding::encode("/tmp").as_ref())
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        std::fs::create_dir(session_dir.join("updates.jsonl")).unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut view = make_min_child_view();
        let _ = replay_inherited_updates(
            &mut view,
            child_sid,
            std::path::Path::new("/tmp"),
            None,
            ReplayLookupFallback::Relocation,
        );
        assert!(
            !view.scrollback.in_batch(),
            "end_batch must run after a read error"
        );
        set_replay_grok_home_for_tests(None);
    }
    #[test]
    fn replay_inherited_updates_uses_child_cwd_hint() {
        let home = tempfile::tempdir().unwrap();
        let child_sid = "child-wt-hint";
        let child_cwd = "/work/wt";
        let session_dir = home
            .path()
            .join("sessions")
            .join(xai_grok_config::encode_cwd_dirname(child_cwd))
            .join(child_sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.json"), "{}").unwrap();
        let user = format!(
            r#"{{"method":"session/update","params":{{"sessionId":"{child_sid}","update":{{"sessionUpdate":"user_message_chunk","content":{{"type":"text","text":"from-wt"}}}}}}}}"#
        );
        std::fs::write(session_dir.join("updates.jsonl"), format!("{user}\n")).unwrap();
        set_replay_grok_home_for_tests(Some(home.path().to_path_buf()));
        let mut view = make_min_child_view();
        replay_inherited_updates(
            &mut view,
            child_sid,
            std::path::Path::new("/tmp"),
            Some(std::path::Path::new(child_cwd)),
            ReplayLookupFallback::Relocation,
        );
        assert_ne!(
            view.scrollback.len(),
            0,
            "child_cwd hint must locate the worktree transcript"
        );
        set_replay_grok_home_for_tests(None);
    }
    #[test]
    fn subagent_meta_empty() {
        assert_eq!(format_subagent_meta(None, None, None), "");
    }
    #[test]
    fn subagent_meta_all_fields() {
        assert_eq!(
            format_subagent_meta(Some("researcher"), Some("analyst"), Some("grok-3")),
            " (researcher \u{00b7} analyst \u{00b7} grok-3)"
        );
    }
    #[test]
    fn subagent_meta_partial_skips_nones() {
        assert_eq!(
            format_subagent_meta(Some("researcher"), None, Some("grok-3")),
            " (researcher \u{00b7} grok-3)"
        );
    }
    #[test]
    fn type_label_abbreviates_general_purpose() {
        assert_eq!(format_type_label("general-purpose"), "general");
    }
    #[test]
    fn type_label_passes_through_known_types() {
        assert_eq!(format_type_label("explore"), "explore");
        assert_eq!(format_type_label("plan"), "plan");
    }
    #[test]
    fn type_label_passes_through_unknown() {
        assert_eq!(format_type_label("custom-agent"), "custom-agent");
    }
    #[test]
    fn context_badge_resumed() {
        let mut info = make_info();
        info.context_source = Some("resumed".into());
        assert_eq!(format_context_badge(&info), "resumed");
    }
    #[test]
    fn context_badge_forked() {
        let mut info = make_info();
        info.context_source = Some("forked".into());
        assert_eq!(format_context_badge(&info), "forked");
    }
    #[test]
    fn context_badge_new_returns_empty() {
        let mut info = make_info();
        info.context_source = Some("new".into());
        assert_eq!(format_context_badge(&info), "");
    }
    #[test]
    fn context_badge_none_returns_empty() {
        assert_eq!(format_context_badge(&make_info()), "");
    }
    #[test]
    fn subagent_meta_collapses_duplicate_persona_role() {
        assert_eq!(
            format_subagent_meta(Some("reviewer"), Some("reviewer"), Some("grok-3")),
            " (reviewer \u{00b7} grok-3)"
        );
    }
    #[test]
    fn subagent_meta_keeps_distinct_persona_role() {
        assert_eq!(
            format_subagent_meta(Some("researcher"), Some("analyst"), None),
            " (researcher \u{00b7} analyst)"
        );
    }
    #[test]
    fn subagent_meta_only_role_when_persona_absent() {
        assert_eq!(
            format_subagent_meta(None, Some("reviewer"), None),
            " (reviewer)"
        );
    }
    #[test]
    fn subagent_meta_only_persona_when_role_absent() {
        assert_eq!(
            format_subagent_meta(Some("reviewer"), None, None),
            " (reviewer)"
        );
    }
    #[test]
    fn subagent_meta_drops_both_empty_persona_role() {
        assert_eq!(
            format_subagent_meta(Some(""), Some(" "), Some("grok-3")),
            " (grok-3)"
        );
    }
    #[test]
    fn label_uses_persona_when_set() {
        let mut info = make_info();
        info.persona = Some("implementer".into());
        info.role = Some("any".into());
        info.subagent_type = "general-purpose".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "Implementer");
        assert_eq!(desc, "test task");
    }
    #[test]
    fn label_falls_back_to_role_when_no_persona() {
        let mut info = make_info();
        info.role = Some("analyst".into());
        info.subagent_type = "general-purpose".into();
        let (label, _) = format_subagent_label(&info);
        assert_eq!(label, "Analyst");
    }
    #[test]
    fn label_uses_subagent_type_when_meaningful() {
        let mut info = make_info();
        info.subagent_type = "explore".into();
        info.description = "[deep-dive] find auth code".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "Explore");
        assert_eq!(desc, "find auth code");
    }
    #[test]
    fn label_falls_back_to_tag_when_general_purpose() {
        let mut info = make_info();
        info.subagent_type = "general-purpose".into();
        info.description = "[security-fix] patch XSS".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "Security-fix");
        assert_eq!(desc, "patch XSS");
    }
    #[test]
    fn label_final_fallback_general() {
        let mut info = make_info();
        info.subagent_type = "general-purpose".into();
        info.description = "do a thing".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "General");
        assert_eq!(desc, "do a thing");
    }
    #[test]
    fn label_strips_tag_prefix_even_when_unused() {
        let mut info = make_info();
        info.persona = Some("reviewer".into());
        info.subagent_type = "general-purpose".into();
        info.description = "[review] check the diff".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "Reviewer");
        assert_eq!(desc, "check the diff");
    }
    #[test]
    fn label_treats_whitespace_persona_as_absent() {
        let mut info = make_info();
        info.persona = Some("   ".into());
        info.role = Some("analyst".into());
        info.subagent_type = "general-purpose".into();
        let (label, _) = format_subagent_label(&info);
        assert_eq!(label, "Analyst");
    }
    #[test]
    fn label_treats_empty_tag_as_absent() {
        let mut info = make_info();
        info.subagent_type = "general-purpose".into();
        info.description = "[] do something".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "General");
        assert_eq!(desc, "[] do something");
    }
    #[test]
    fn label_unclosed_bracket_leaves_description_alone() {
        let mut info = make_info();
        info.subagent_type = "general-purpose".into();
        info.description = "[broken description".into();
        let (label, desc) = format_subagent_label(&info);
        assert_eq!(label, "General");
        assert_eq!(desc, "[broken description");
    }
    #[test]
    fn label_custom_subagent_type_passes_through_with_capitalization() {
        let mut info = make_info();
        info.subagent_type = "custom-agent".into();
        let (label, _) = format_subagent_label(&info);
        assert_eq!(label, "Custom-agent");
    }
    #[test]
    fn label_preserves_already_capitalized_persona() {
        let mut info = make_info();
        info.persona = Some("Reviewer".into());
        let (label, _) = format_subagent_label(&info);
        assert_eq!(label, "Reviewer");
    }
    fn write_meta_json(dir: &std::path::Path, subagent_id: &str, json: &str) {
        let meta_dir = dir.join("subagents").join(subagent_id);
        std::fs::create_dir_all(&meta_dir).unwrap();
        std::fs::write(meta_dir.join("meta.json"), json).unwrap();
    }
    /// Build a session dir matching the path formula used by `enrich_from_meta_with_home`.
    fn setup_enrichment_dir(
        grok_home: &std::path::Path,
        cwd: &std::path::Path,
        session_id: &str,
    ) -> std::path::PathBuf {
        let sessions_dir = grok_home
            .join("sessions")
            .join(urlencoding::encode(&cwd.to_string_lossy()).as_ref())
            .join(session_id);
        std::fs::create_dir_all(&sessions_dir).unwrap();
        sessions_dir
    }
    #[test]
    fn enrich_from_meta_populates_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::path::Path::new("/home/user/project");
        let session_id = "sess-abc";
        let session_dir = setup_enrichment_dir(tmp.path(), cwd, session_id);
        let json = r#"{"prompt":"do stuff","child_cwd":"/tmp/work","worktree_path":"/tmp/wt"}"#;
        write_meta_json(&session_dir, "sa-1", json);
        let mut info = make_info();
        enrich_from_meta_with_home(&mut info, tmp.path(), cwd, session_id);
        assert_eq!(info.prompt.as_deref(), Some("do stuff"));
        assert_eq!(info.child_cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(info.worktree_path.as_deref(), Some("/tmp/wt"));
    }
    #[test]
    fn enrich_from_meta_missing_file_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut info = make_info();
        enrich_from_meta_with_home(
            &mut info,
            tmp.path(),
            std::path::Path::new("/nowhere"),
            "no-session",
        );
        assert!(info.prompt.is_none());
        assert!(info.child_cwd.is_none());
        assert!(info.worktree_path.is_none());
    }
    #[test]
    fn enrich_from_meta_malformed_json_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::path::Path::new("/home/user");
        let session_dir = setup_enrichment_dir(tmp.path(), cwd, "sess-x");
        write_meta_json(&session_dir, "sa-1", "not json{{{");
        let mut info = make_info();
        enrich_from_meta_with_home(&mut info, tmp.path(), cwd, "sess-x");
        assert!(info.prompt.is_none());
    }
    #[test]
    fn deserialize_meta_slice_ignores_unknown_fields() {
        let json = r#"{"prompt":"hi","unknown_field":42,"nested":{"a":1}}"#;
        let meta: SubagentMetaSlice = serde_json::from_str(json).unwrap();
        assert_eq!(meta.prompt.as_deref(), Some("hi"));
        assert!(meta.child_cwd.is_none());
        assert!(meta.worktree_path.is_none());
    }
    #[test]
    fn enrich_from_meta_partial_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = std::path::Path::new("/home/user");
        let session_dir = setup_enrichment_dir(tmp.path(), cwd, "sess-p");
        write_meta_json(&session_dir, "sa-1", r#"{"prompt":"only prompt"}"#);
        let mut info = make_info();
        enrich_from_meta_with_home(&mut info, tmp.path(), cwd, "sess-p");
        assert_eq!(info.prompt.as_deref(), Some("only prompt"));
        assert!(info.child_cwd.is_none());
        assert!(info.worktree_path.is_none());
    }
    #[test]
    fn activity_label_thinking() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(format_activity_label(&TurnActivity::Thinking), "Thinking");
    }
    #[test]
    fn activity_label_responding() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::Responding),
            "Responding",
        );
    }
    #[test]
    fn activity_label_auto_compacting() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::AutoCompacting),
            "Compacting",
        );
    }
    #[test]
    fn activity_label_retrying() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::Retrying {
                attempt: 2,
                max_retries: 5,
                reason: "rate limited".into(),
                error_type: None,
            }),
            "Retrying (2/5)",
        );
    }
    #[test]
    fn activity_label_waiting_reasons() {
        use crate::acp::tracker::{TurnActivity, WaitingReason};
        assert_eq!(
            format_activity_label(&TurnActivity::Waiting(WaitingReason::subagent())),
            "Waiting on subagent…",
        );
        assert_eq!(
            format_activity_label(&TurnActivity::Waiting(WaitingReason::task_output())),
            "Waiting on task output…",
        );
        assert_eq!(
            format_activity_label(&TurnActivity::Waiting(WaitingReason::TaskOutput {
                task_ids: vec!["t1".into()],
                subject: Some("run tests".into()),
                waits: false,
            })),
            "run tests…",
        );
    }
    #[test]
    fn activity_label_tool_running_empty_title() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::ToolRunning {
                title: String::new(),
                description: None
            }),
            "Running tool",
        );
    }
    #[test]
    fn activity_label_tool_running_short_title() {
        use crate::acp::tracker::TurnActivity;
        assert_eq!(
            format_activity_label(&TurnActivity::ToolRunning {
                title: "cargo build".into(),
                description: None
            }),
            "Running: cargo build",
        );
    }
    #[test]
    fn activity_label_tool_running_exactly_at_limit() {
        use crate::acp::tracker::TurnActivity;
        let title = "a".repeat(40);
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title: title.clone(),
            description: None,
        });
        assert_eq!(result, format!("Running: {title}"));
        assert!(!result.contains('\u{2026}'), "no ellipsis at boundary");
    }
    #[test]
    fn activity_label_tool_running_truncates_long_title() {
        use crate::acp::tracker::TurnActivity;
        let title = "a".repeat(60);
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title,
            description: None,
        });
        let expected_prefix = "Running: ".to_string() + "a".repeat(40).as_str();
        assert!(result.starts_with(&expected_prefix));
        assert!(result.ends_with('\u{2026}'), "truncated with ellipsis");
    }
    #[test]
    fn activity_label_tool_running_multibyte_under_char_limit() {
        use crate::acp::tracker::TurnActivity;
        let title: String = "\u{00e9}".repeat(35);
        assert!(title.len() > 40, "byte length exceeds threshold");
        assert!(title.chars().count() <= 40, "char count within limit");
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title: title.clone(),
            description: None,
        });
        assert_eq!(result, format!("Running: {title}"));
        assert!(!result.contains('\u{2026}'), "no spurious ellipsis");
    }
    #[test]
    fn activity_label_tool_running_multibyte_over_char_limit() {
        use crate::acp::tracker::TurnActivity;
        let title: String = "\u{00e9}".repeat(45);
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title,
            description: None,
        });
        assert!(result.ends_with('\u{2026}'), "truncated with ellipsis");
        let after_prefix = result.strip_prefix("Running: ").unwrap();
        let content_chars: Vec<char> = after_prefix.chars().collect();
        assert_eq!(content_chars.len(), 41);
    }
    #[test]
    fn activity_label_tool_running_multiline_uses_first_line() {
        use crate::acp::tracker::TurnActivity;
        let result = format_activity_label(&TurnActivity::ToolRunning {
            title: "first line\nsecond line".into(),
            description: None,
        });
        assert_eq!(result, "Running: first line");
    }
}
