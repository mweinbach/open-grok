//! File search state: owns the fuzzy matcher daemon, results, and dropdown state.
//!
//! This is the core engine for @-completion. It manages:
//! - A background [`FuzzyFileMatcherDaemon`] that walks the directory tree
//! - The current [`AtContext`] (parsed from prompt text + cursor)
//! - Cached fuzzy match results (polled on tick)
//! - Dropdown selection state (selected index, scroll offset)
//! - Text replacement logic when a result is accepted

use std::path::{Path, PathBuf};
use std::sync::Arc;

use xai_grok_workspace::file_system::{
    FuzzyFileMatcher, FuzzyFileMatcherDaemon, FuzzyMatchResult, FuzzyMatcherDaemonResults,
};

use super::context::{self, AtContext, normalize_display_path};

/// Top-K results to request from the fuzzy matcher.
const MATCHER_TOP_K: usize = 1000;

/// Replacement to apply to the prompt text after accepting a fuzzy result.
#[derive(Debug, Clone)]
pub struct FileSearchReplacement {
    /// Byte range in the prompt text to replace (excludes the `@`).
    pub range: std::ops::Range<usize>,
    /// Replacement text (the normalized path, possibly with trailing space or `/`).
    pub text: String,
    /// Where to place the cursor after replacement.
    pub cursor: usize,
    /// Whether the @-context should be cleared (file accepted, not dir drill-down).
    pub dismiss: bool,
}

/// File search state for @-completion.
pub struct FileSearchState {
    /// Directory the matcher walks. Mirrors the daemon's root (which is
    /// otherwise moved into its worker thread) so callers can introspect
    /// where `@`-completion is currently pointed.
    root: PathBuf,
    /// Background fuzzy matcher daemon.
    daemon: FuzzyFileMatcherDaemon,
    /// Latest results snapshot from the daemon.
    results: FuzzyMatcherDaemonResults,
    /// Current @-context (if cursor is inside an @-token).
    context: Option<AtContext>,
    /// Selected index in the dropdown list (keyboard-driven).
    selected: usize,
    /// Hovered index in the dropdown list (mouse-driven).
    /// `None` when the mouse is not over any item.
    hovered: Option<usize>,
    /// Scroll offset for the dropdown list.
    scroll_offset: usize,
    /// Generation counter to prevent stale results from flickering in.
    min_generation: usize,
    /// Directory being drilled into; keeps the @-token alive when its name has
    /// whitespace (`my dir`). Self-validating — applies only while the path matches.
    drill_prefix: Option<String>,
}

impl FileSearchState {
    /// Create a new file search state rooted at the given path.
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            daemon: FuzzyFileMatcherDaemon::new(FuzzyFileMatcher::new(root), MATCHER_TOP_K),
            results: FuzzyMatcherDaemonResults::default(),
            context: None,
            selected: 0,
            hovered: None,
            scroll_offset: 0,
            min_generation: 0,
            drill_prefix: None,
        }
    }

    /// Replace the underlying matcher with a new one rooted at `root`.
    ///
    /// Used after worktree creation to point @-completion at the new tree.
    pub fn retarget(&mut self, root: &Path) {
        self.root = root.to_owned();
        self.daemon = FuzzyFileMatcherDaemon::new(FuzzyFileMatcher::new(root), MATCHER_TOP_K);
        self.results = FuzzyMatcherDaemonResults::default();
        self.context = None;
        self.selected = 0;
        self.hovered = None;
        self.scroll_offset = 0;
        self.min_generation = 0;
        self.drill_prefix = None;
    }

    /// The directory the matcher currently walks (the `@`-completion root).
    pub fn root(&self) -> &Path {
        &self.root
    }

    // ── Visibility ──────────────────────────────────────────────────────

    /// Whether the dropdown should be visible.
    pub fn is_visible(&self) -> bool {
        self.context.is_some() && !self.results.topk.is_empty()
    }

    /// The current @-context, if any.
    pub fn context(&self) -> Option<&AtContext> {
        self.context.as_ref()
    }

    /// The current results snapshot.
    pub fn results(&self) -> &FuzzyMatcherDaemonResults {
        &self.results
    }

    /// Currently selected index in the results.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Scroll offset for the dropdown.
    pub fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    /// Currently hovered index (mouse-driven), if any.
    pub fn hovered(&self) -> Option<usize> {
        self.hovered
    }

    /// Set the hovered index. Returns `true` if changed.
    pub fn set_hovered(&mut self, index: Option<usize>) -> bool {
        let clamped = index.and_then(|i| {
            if i < self.results.topk.len() {
                Some(i)
            } else {
                None
            }
        });
        let changed = clamped != self.hovered;
        self.hovered = clamped;
        changed
    }

    /// Whether the current query is in directory-only mode.
    pub fn is_dir_mode(&self) -> bool {
        self.context.as_ref().is_some_and(|c| c.is_dir_mode())
    }

    // ── Context updates ─────────────────────────────────────────────────

    /// Anchor (or clear) the drilled directory for whitespace-aware detection.
    pub fn set_drill_prefix(&mut self, prefix: Option<String>) {
        self.drill_prefix = prefix;
    }

    /// Recompute the @-context from the current prompt text and cursor position.
    ///
    /// Called after every text change or cursor movement.
    pub fn update_context(&mut self, text: &str, cursor: usize) {
        let new_ctx = context::detect_with_drill(text, cursor, self.drill_prefix.as_deref());

        match (&self.context, &new_ctx) {
            (None, Some(ctx)) => {
                // Fresh `@` token is never a drill — drop any stale anchor.
                self.drill_prefix = None;
                // Entering @-mode: restart the directory walk.
                self.daemon.restart_walk(ctx.is_hidden_mode());
                // A trailing `/` scopes the query to a folder; it must not hide
                // that folder's files, so never filter to directories only.
                self.daemon.set_query(ctx.matcher_query(), false);
                self.min_generation += 1;
                self.selected = 0;
                self.hovered = None;
                self.scroll_offset = 0;
            }
            (Some(old), Some(new)) => {
                // Drop a stale anchor once the @-token's path content no longer
                // starts with it (e.g. undo/paste reverted the drill), so it
                // can't silently re-match on a later edit.
                let anchor_stale = self.drill_prefix.as_deref().is_some_and(|prefix| {
                    !text
                        .get(new.path_range().start..)
                        .is_some_and(|rest| rest.starts_with(prefix))
                });
                if anchor_stale {
                    self.drill_prefix = None;
                }
                // Staying in @-mode: check if hidden mode toggled (needs re-walk).
                if old.is_hidden_mode() != new.is_hidden_mode() {
                    self.daemon.restart_walk(new.is_hidden_mode());
                }
                self.daemon.set_query(new.matcher_query(), false);
                self.min_generation += 1;
                // Reset selection when query changes to avoid showing stale
                // matches from an obscure position in the list.
                self.selected = 0;
                self.hovered = None;
                self.scroll_offset = 0;
            }
            (Some(_), None) => {
                // Leaving @-mode: clear results and the drill anchor.
                self.context = None;
                self.drill_prefix = None;
                self.results = FuzzyMatcherDaemonResults::default();
                return;
            }
            (None, None) => return,
        }

        self.context = new_ctx;
    }

    /// Clear the context (e.g., on Esc).
    pub fn clear_context(&mut self) {
        self.context = None;
        self.drill_prefix = None;
        self.results = FuzzyMatcherDaemonResults::default();
    }

    // ── Tick / polling ──────────────────────────────────────────────────

    /// Poll the daemon for new results. Returns `true` if results changed.
    ///
    /// Should be called on every tick (~4ms) while the dropdown is potentially visible.
    pub fn poll(&mut self) -> bool {
        if self.context.is_none() {
            return false;
        }

        let results = self.daemon.get();

        // Check if results actually changed (pointer comparison on Arc).
        if Arc::ptr_eq(&results.topk, &self.results.topk) {
            return false;
        }

        // Avoid flickering: skip empty intermediate results unless matching is done.
        if !results.topk.is_empty() || results.status.done {
            // Skip stale generations (e.g., from a previous @-context).
            if results.generation >= self.min_generation {
                self.min_generation = results.generation;
                self.results = results;
                // Clamp selection to new result count.
                if !self.results.topk.is_empty() {
                    self.selected = self.selected.min(self.results.topk.len() - 1);
                }
                return true;
            }
        }

        false
    }

    // ── Navigation ──────────────────────────────────────────────────────

    /// Move selection by `delta` items (negative = up, positive = down).
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.results.topk.len();
        if len == 0 {
            return;
        }
        let max_idx = len - 1;
        let current = self.selected.min(max_idx);
        self.selected = (current as isize + delta).clamp(0, max_idx as isize) as usize;
    }

    /// Move selection by a page (half of visible height).
    pub fn page_move(&mut self, delta: isize, visible_rows: usize) {
        let half = (visible_rows / 2).max(1) as isize;
        self.move_selection(delta * half);
    }

    /// Ensure the selected item is visible in the dropdown viewport.
    pub fn ensure_visible(&mut self, visible_rows: usize) {
        if visible_rows == 0 {
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + visible_rows {
            self.scroll_offset = self.selected + 1 - visible_rows;
        }
    }

    // ── Selection / replacement ─────────────────────────────────────────

    /// Select the hovered item (for click-to-accept).
    /// Returns `true` if there was a valid hovered item to select.
    pub fn select_hovered(&mut self) -> bool {
        if let Some(idx) = self.hovered
            && idx < self.results.topk.len()
        {
            self.selected = idx;
            return true;
        }
        false
    }

    /// Get the currently selected fuzzy match result.
    pub fn selected_result(&self) -> Option<&FuzzyMatchResult> {
        self.results.topk.get(self.selected)
    }

    /// Compute the text replacement for accepting the currently selected result.
    ///
    /// The `src` parameter is the full prompt text (needed to detect edge cases
    /// like "replacement is a no-op" for directory drill-down).
    pub fn try_replace(&mut self, src: &str) -> Option<FileSearchReplacement> {
        let ctx = self.context.as_ref()?;
        let res = self.results.topk.get(self.selected)?;

        let path_str = res.path.to_string();
        let mut text = normalize_display_path(&path_str).to_owned();

        // Replace only the path portion of the @-token (preserving `@`
        // and any hidden-mode `!` marker). See `AtContext::path_range`.
        let range = ctx.path_range();

        let mut cursor = range.start + text.len() + 1;
        let dismiss;

        if ctx.is_dir_mode() {
            // Directory mode: append `/` and stay in completion for drill-down.
            text = format!("{text}/");
            if range.end <= src.len() && src[range.clone()] == text[..] {
                // No-op replacement (same text already there) — treat as "done".
                cursor += 1;
                if range.end == src.len() {
                    text = format!("{text} ");
                }
                dismiss = true;
            } else {
                dismiss = false; // Stay in completion mode (drill-down).
            }
        } else {
            // File mode: append trailing space if at end of input.
            if range.end == src.len() {
                text = format!("{text} ");
            }
            dismiss = true;
        }

        if dismiss {
            self.context = None;
            self.drill_prefix = None;
        }

        Some(FileSearchReplacement {
            range,
            text,
            cursor,
            dismiss,
        })
    }

    /// Number of result items.
    pub fn result_count(&self) -> usize {
        self.results.topk.len()
    }

    /// Total items the matcher knows about (for "k/n" display).
    pub fn total_items(&self) -> usize {
        self.results.num_items
    }

    /// Test-only: install a fake context + results snapshot so tests can drive
    /// acceptance flows without spinning up the background fuzzy daemon.
    ///
    /// **Mixing with daemon polling is unsupported.** This helper assigns
    /// `generation = self.min_generation` without bumping `min_generation`,
    /// which means a real daemon poll occurring after `set_test_state` could
    /// deliver same-generation results that overwrite the seeded fake state
    /// non-deterministically. Tests that use this helper must not also drive
    /// real daemon polls; if a future test needs both, bump
    /// `self.min_generation` here so any in-flight daemon results are
    /// rejected.
    #[cfg(test)]
    pub(crate) fn set_test_state(
        &mut self,
        context: AtContext,
        results: Vec<FuzzyMatchResult>,
        selected: usize,
    ) {
        self.context = Some(context);
        self.results = FuzzyMatcherDaemonResults {
            topk: Arc::from(results),
            num_items: 0,
            status: Default::default(),
            generation: self.min_generation,
        };
        self.selected = selected;
    }
}
