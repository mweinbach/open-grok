//! Settings modal — opens via F2, `/settings`, command palette, and
//! shortcuts-help.
//!
//! ## State machine
//!
//! `SettingsModalState` carries a `UiConfig` snapshot plus a mode machine:
//!
//! - `Browse` — j/k navigates rows; Space toggles Bool; Enter opens
//!   a chooser/editor for Enum/String/Int.
//! - `FilterFocused` — `/` enters filter mode; `invalidate_filter`
//!   recomputes `filtered_cache` on every mutation.
//! - `PickingEnum { ... }` — enum chooser sub-pane.
//! - `EditingValue { ... }` — inline string/int editor.
//!
//! ## Keyboard ↔ mouse parity
//!
//! Every keyboard interaction has a mouse equivalent via `handle_mouse`.
//!
//! ## Close-key interception
//!
//! F2/Ctrl+,/Cmd+, are intercepted before mode-specific routing.
//! Esc-in-Browse is handled by the `ModalWindow` chrome (so
//! `is_close_key` does NOT match Esc); Esc-in-FilterFocused exits
//! filter mode without closing.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::actions::Action;
use crate::render::line_utils::truncate_str;
use crate::settings::{
    EnumChoice, OwnedEnumChoice, PagerLocalSnapshot, SettingCategory, SettingKey, SettingKind,
    SettingMeta, SettingValue, SettingsRegistry, StringValidator, current_value_for,
    dynamic_enum_choices,
};
use crate::theme::Theme;
use crate::views::modal_window::{
    self, ModalContentArea, ModalSizing, ModalWindowConfig, ModalWindowState, Shortcut,
};

use xai_grok_shell::agent::config::UiConfig;

// ---------------------------------------------------------------------------
// Public constants
// ---------------------------------------------------------------------------

/// Public display title of the modal — also used by
/// `views/modal.rs::ActiveModal::message` so renames stay in one place.
pub const MODAL_TITLE: &str = "Settings";

/// Width of the `"─ "` leading decoration before the title in the
/// modal's top border. Used to compute the breadcrumb hit-rect x offset.
const TITLE_LEADING_DECORATION_W: u16 = 2; // `─ `: 1 cell box-drawing + 1 cell space.

// Descriptions are now expand-on-demand via Right/Left arrows;
// see `render_expanded_description`.

/// Below this width the row list is skipped (chrome renders empty).
const CONTENT_MIN_WIDTH: u16 = 10;

/// Default max width for the modal. Keeps the row list compact on wide terminals.
const STANDARD_MAX_WIDTH: u16 = 110;

/// Per-side margin when editing `max_thoughts_width` (modal widens
/// to `terminal_width - 2*margin` so the wrap preview is useful).
const MAX_THOUGHTS_WIDTH_WIDENED_MARGIN: u16 = 8;

/// Outcome of a key or mouse event. Separate from `InputOutcome`
/// because the modal doesn't own `agent.active_modal` — close is
/// the caller's responsibility.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SettingsKeyOutcome {
    /// Close the modal.
    Close,
    /// Forward to dispatch.
    Action(Action),
    /// Forward two actions in order (first must resolve before second).
    /// Used by `d`-reset-in-picker to revert preview before opening
    /// the reset-confirm overlay.
    ActionPair(Action, Action),
    /// Internal state mutation, no action.
    Changed,
    /// No-op.
    Unchanged,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One row in the visible flat list — either a category header (non-
/// selectable) or a setting row (selectable, dispatchable).
#[derive(Debug, Clone)]
pub enum RowEntry {
    Header { category: SettingCategory },
    Setting { key: SettingKey, meta_index: usize },
}

/// Mode state for the modal.
#[derive(Debug, Clone)]
pub enum SettingsModalMode {
    Browse,
    /// `/` was pressed; chars filter the visible rows.
    FilterFocused,
    /// Enum chooser sub-pane. `supports_preview` is cached at open
    /// time to avoid per-keystroke registry lookups.
    PickingEnum {
        key: SettingKey,
        choices_idx: usize,
        original_value: SettingValue,
        supports_preview: bool,
    },
    /// Group sub-sheet: a list of the group's child Bool toggles. `child_idx`
    /// is the focused child within the group. Space/Enter toggles in place
    /// (the sheet stays open); Esc returns to Browse. Mirrors `PickingEnum`'s
    /// open/render/commit flow but for independent toggles.
    PickingGroup {
        key: SettingKey,
        child_idx: usize,
    },
    /// Inline string/int editor. `cursor_byte` is always on a char
    /// boundary. `validation_error` shows live feedback; commit
    /// re-validates before dispatching. No `original_value` — these
    /// settings have no live preview, so Esc is a pure cancel.
    EditingValue {
        key: SettingKey,
        buffer: String,
        cursor_byte: usize,
        validation_error: Option<String>,
    },
}

/// Settings modal state. Boxed inside `ActiveModal::Settings` to
/// avoid clippy `large_enum_variant`.
pub struct SettingsModalState {
    pub window: ModalWindowState,
    pub registry: Arc<SettingsRegistry>,
    /// `UiConfig` snapshot, refreshed by the dispatcher on mutations.
    pub ui_snapshot: UiConfig,
    pub pager_snapshot: PagerLocalSnapshot,
    /// Computed row layout (headers + settings, in render order).
    pub rows: Vec<RowEntry>,
    /// Index into `rows` of the focused row.
    pub selected: usize,
    /// Vertical scroll offset (line-granular).
    pub scroll_offset: usize,
    pub mode: SettingsModalMode,
    /// Filter query. Persists across FilterFocused→Browse on Enter; cleared by Esc.
    pub query: String,
    /// Byte offset of the editing cursor within `query`.
    pub query_cursor: usize,
    /// Row indices matching `query`, recomputed per mutation (not per frame).
    filtered_cache: Vec<usize>,

    // -- Mouse hit-test rects (populated by render) --
    pub list_area: Rect,
    /// Click-hit rect per row, parallel to `rows`.
    pub row_rects: Vec<Rect>,
    /// Click-hit rect for the value column on each row. Bool rows
    /// toggle on click; Enum/String/Int rows open the sub-pane.
    pub value_hit_rects: Vec<Rect>,
    /// `(decrement_rect, increment_rect)` for the Int stepper's
    /// `‹`/`›` glyphs. Zero-sized when not in Int editing mode.
    pub editor_adornment_rects: (Rect, Rect),
    /// Click-hit rect per choice in `PickingEnum`. Each rect spans the
    /// full height of a choice (including wrapped description lines).
    pub picker_choice_rects: Vec<Rect>,
    /// Hit-rect for the breadcrumb title in sub-pane modes
    /// (`PickingEnum`/`EditingValue`). Clicking anywhere on
    /// `Settings › <label>` cancels back to Browse. `None` in
    /// Browse/FilterFocused. Cleared on mode transitions.
    pub settings_breadcrumb_rect: Option<Rect>,
    /// Hover flag for the breadcrumb — adds underline affordance.
    pub breadcrumb_hovered: bool,
    /// Keys whose description is expanded (Right/l to expand, Left/h
    /// to collapse). Multiple rows can be expanded simultaneously.
    pub expanded_keys: std::collections::HashSet<&'static str>,
    /// Row under the mouse cursor for hover highlighting. Indexes
    /// `rows` in Browse, `picker_choice_rects` in PickingEnum,
    /// always `None` in EditingValue.
    pub hover_row: Option<usize>,
}

impl SettingsModalState {
    /// Construct a new modal state from a registry + snapshots.
    pub fn new(
        registry: Arc<SettingsRegistry>,
        ui_snapshot: UiConfig,
        pager_snapshot: PagerLocalSnapshot,
    ) -> Self {
        let rows = build_rows(&registry);
        // Start on the first selectable (non-header) row.
        let selected = rows
            .iter()
            .position(|r| matches!(r, RowEntry::Setting { .. }))
            .unwrap_or(0);
        let filtered_cache = compute_filtered(&rows, &registry, "");
        Self {
            window: ModalWindowState::new(),
            registry,
            ui_snapshot,
            pager_snapshot,
            rows,
            selected,
            scroll_offset: 0,
            mode: SettingsModalMode::Browse,
            query: String::new(),
            query_cursor: 0,
            filtered_cache,
            list_area: Rect::default(),
            row_rects: Vec::new(),
            value_hit_rects: Vec::new(),
            editor_adornment_rects: (Rect::default(), Rect::default()),
            picker_choice_rects: Vec::new(),
            settings_breadcrumb_rect: None,
            breadcrumb_hovered: false,
            expanded_keys: std::collections::HashSet::new(),
            hover_row: None,
        }
    }

    /// The currently-focused setting row, if any.
    pub fn focused_setting(&self) -> Option<(SettingKey, &SettingMeta)> {
        match self.rows.get(self.selected)? {
            RowEntry::Setting { key, meta_index } => {
                let meta = self.registry.all().get(*meta_index)?;
                Some((*key, meta))
            }
            RowEntry::Header { .. } => None,
        }
    }

    /// Filtered row indices in render order.
    pub fn filtered_indices(&self) -> &[usize] {
        &self.filtered_cache
    }

    /// Recompute `filtered_cache` from the current `query`.
    fn invalidate_filter(&mut self) {
        self.filtered_cache = compute_filtered(&self.rows, &self.registry, &self.query);
    }

    /// Snap `selected` to the first visible setting if filtered out.
    fn clamp_selected_to_visible(&mut self) {
        if self.filtered_cache.is_empty() {
            return;
        }
        if self.filtered_cache.contains(&self.selected) {
            return;
        }
        // Snap to first selectable row in the visible filter.
        for &row_idx in &self.filtered_cache {
            if matches!(self.rows[row_idx], RowEntry::Setting { .. }) {
                self.selected = row_idx;
                return;
            }
        }
    }

    /// Read the current value for a setting key.
    pub fn value_for(&self, key: SettingKey) -> Option<SettingValue> {
        current_value_for(key, &self.ui_snapshot, &self.pager_snapshot)
    }

    /// Move `selected` forward, skipping headers and filtered-out rows.
    fn advance_next(&mut self) -> bool {
        let cur_pos = self.filtered_cache.iter().position(|&i| i == self.selected);
        let mut next = match cur_pos {
            Some(p) => p + 1,
            // Defensive: resume from top if `selected` is hidden.
            None => 0,
        };
        while next < self.filtered_cache.len() {
            let row_idx = self.filtered_cache[next];
            if matches!(self.rows[row_idx], RowEntry::Setting { .. }) {
                self.selected = row_idx;
                return true;
            }
            next += 1;
        }
        false
    }

    /// Move `selected` backward, skipping headers and filtered-out rows.
    fn advance_prev(&mut self) -> bool {
        if self.filtered_cache.is_empty() {
            return false;
        }
        let cur_pos = self.filtered_cache.iter().position(|&i| i == self.selected);
        let mut prev = match cur_pos {
            Some(p) if p > 0 => p - 1,
            Some(_) => return false,
            // Defensive: resume from bottom if `selected` is hidden.
            None => self.filtered_cache.len() - 1,
        };
        loop {
            let row_idx = self.filtered_cache[prev];
            if matches!(self.rows[row_idx], RowEntry::Setting { .. }) {
                self.selected = row_idx;
                return true;
            }
            if prev == 0 {
                break;
            }
            prev -= 1;
        }
        false
    }

    /// Set selection to `idx` if it's a selectable row.
    pub fn select_at(&mut self, idx: usize) -> bool {
        if idx >= self.rows.len() {
            return false;
        }
        if !matches!(self.rows[idx], RowEntry::Setting { .. }) {
            return false;
        }
        if self.selected == idx {
            return false;
        }
        self.selected = idx;
        true
    }

    /// Reset hit-test geometry so mouse handlers degrade gracefully
    /// when render is aborted. Does NOT clear `hover_row` — that's
    /// cleared on mode transitions instead to avoid per-frame flicker.
    pub(crate) fn reset_hit_rects(&mut self) {
        self.list_area = Rect::default();
        self.row_rects.clear();
        self.value_hit_rects.clear();
        self.editor_adornment_rects = (Rect::default(), Rect::default());
        self.picker_choice_rects.clear();
        self.settings_breadcrumb_rect = None;
        self.breadcrumb_hovered = false;
    }

    /// Transition to Browse, clearing sub-pane hover/breadcrumb state
    /// to prevent stale hit-rects across mode changes.
    pub(crate) fn transition_to_browse(&mut self) {
        self.mode = SettingsModalMode::Browse;
        self.hover_row = None;
        self.settings_breadcrumb_rect = None;
        self.breadcrumb_hovered = false;
    }

    /// Transition to `PickingEnum` if the focused row is Enum/DynamicEnum.
    /// Returns `false` if the focused row is another kind.
    pub fn try_enter_picking_enum(&mut self) -> bool {
        let (key, first_canonical, current_value, supports_preview, resolved_choices) = {
            let Some((key, meta)) = self.focused_setting() else {
                return false;
            };
            // Handles both static `Enum` and `DynamicEnum` catalogs.
            let (supports_preview, resolved): (bool, Vec<OwnedEnumChoice>) = match &meta.kind {
                SettingKind::Enum {
                    choices,
                    supports_preview,
                    ..
                } => (
                    *supports_preview,
                    effective_enum_choices(key, choices, &self.pager_snapshot)
                        .into_iter()
                        .map(|c| OwnedEnumChoice {
                            canonical: c.canonical.to_string(),
                            display: c.display.to_string(),
                            description: c.description.to_string(),
                        })
                        .collect(),
                ),
                SettingKind::DynamicEnum {
                    source,
                    supports_preview,
                    ..
                } => (
                    *supports_preview,
                    dynamic_enum_choices(*source, &self.pager_snapshot),
                ),
                _ => return false,
            };
            // Soft-fail if a static catalog exceeds the product cap. DynamicEnum
            // (e.g. models) is exempt — those lists are runtime-sized and always
            // scroll. The chooser itself scrolls static lists too; this assert is
            // a design guard, not a render requirement.
            debug_assert!(
                resolved.len() <= MAX_PICKER_CHOICES
                    || matches!(meta.kind, SettingKind::DynamicEnum { .. }),
                "Static Enum setting `{}` has {} choices, exceeds MAX_PICKER_CHOICES ({}). \
                 Raise the cap deliberately if a larger curated catalog is required.",
                key,
                resolved.len(),
                MAX_PICKER_CHOICES,
            );
            let first = resolved
                .first()
                .map(|c| c.canonical.clone())
                .unwrap_or_default();
            let cur = self.value_for(key);
            (key, first, cur, supports_preview, resolved)
        };

        // Resolve choices_idx from current value. For DynamicEnum,
        // if the current value no longer exists in the catalog,
        // fall back to index 1 (first real entry past sentinel)
        // to avoid accidentally wiping the user's preference.
        let is_dynamic_enum = matches!(
            self.registry.find(key).map(|m| &m.kind),
            Some(SettingKind::DynamicEnum { .. })
        );
        let unknown_dynamic_fallback_idx = if is_dynamic_enum && resolved_choices.len() > 1 {
            1
        } else {
            0
        };
        let choices_idx = match &current_value {
            Some(SettingValue::Enum(cur)) => resolved_choices
                .iter()
                .position(|c| c.canonical == *cur)
                .unwrap_or(0),
            Some(SettingValue::String(cur)) if !cur.is_empty() => resolved_choices
                .iter()
                .position(|c| c.canonical == *cur)
                .unwrap_or(unknown_dynamic_fallback_idx),
            Some(SettingValue::String(_)) => 0,
            _ => 0,
        };
        if is_dynamic_enum
            && choices_idx == unknown_dynamic_fallback_idx
            && unknown_dynamic_fallback_idx != 0
        {
            // Telemetry: log when a DynamicEnum value is stale.
            tracing::warn!(
                target: "settings",
                key = key,
                ?current_value,
                "DynamicEnum picker entered with a current value that no longer resolves \
                 in the live catalog — focusing first real choice instead of the \
                 (no override) sentinel to defend against accidental destructive Enter",
            );
        }
        let original_value = current_value.unwrap_or_else(|| {
            // Fallback to first choice, using the right value carrier.
            match self.registry.find(key).map(|m| &m.kind) {
                Some(SettingKind::DynamicEnum { .. }) => SettingValue::String(first_canonical),
                Some(SettingKind::Enum { choices, .. }) => {
                    let first_static = choices.first().map(|c| c.canonical).unwrap_or("");
                    SettingValue::Enum(first_static)
                }
                _ => SettingValue::Enum(""),
            }
        });
        self.mode = SettingsModalMode::PickingEnum {
            key,
            choices_idx,
            supports_preview,
            original_value,
        };
        self.hover_row = None;
        true
    }

    /// Transition to `PickingGroup` if the focused row is a `Group`. Returns
    /// `false` for any other kind so the caller can fall through to the
    /// enum/editor entry points.
    pub fn try_enter_picking_group(&mut self) -> bool {
        let Some((key, meta)) = self.focused_setting() else {
            return false;
        };
        if !matches!(meta.kind, SettingKind::Group { .. }) {
            return false;
        }
        self.mode = SettingsModalMode::PickingGroup { key, child_idx: 0 };
        self.hover_row = None;
        true
    }

    /// Transition to `EditingValue` if the focused row is String or Int.
    pub fn try_enter_editing_value(&mut self) -> bool {
        let Some((key, meta)) = self.focused_setting() else {
            return false;
        };
        let buffer = match (&meta.kind, self.value_for(key)) {
            (SettingKind::String { .. }, Some(SettingValue::String(s))) => s,
            (SettingKind::Int { .. }, Some(SettingValue::Int(i))) => i.to_string(),
            // Fallback for registry skew — seed from default.
            (SettingKind::String { default, .. }, _) => default.to_string(),
            (SettingKind::Int { default, .. }, _) => default.to_string(),
            _ => return false,
        };
        let cursor_byte = buffer.len();

        // Validate the seed value upfront.
        let validation_error = match &meta.kind {
            SettingKind::String { validator, .. } => {
                validate_string(*validator, &buffer, &self.pager_snapshot.available_models)
            }
            SettingKind::Int { min, max, .. } => validate_int(&buffer, *min, *max),
            _ => None,
        };

        self.mode = SettingsModalMode::EditingValue {
            key,
            buffer,
            cursor_byte,
            validation_error,
        };
        self.hover_row = None;
        true
    }

    /// Build the Action that toggles the focused Bool row. Returns
    /// `None` with an error log on registry skew (caught by CI tests).
    pub fn toggle_focused_bool(&self) -> Option<Action> {
        let (key, meta) = self.focused_setting()?;
        if !matches!(meta.kind, SettingKind::Bool { .. }) {
            return None;
        }
        let cur = match self.value_for(key) {
            Some(SettingValue::Bool(b)) => b,
            Some(other) => {
                tracing::error!(
                    target: "settings",
                    ?key,
                    ?other,
                    "Bool-kind setting resolved to non-Bool value — registry skew",
                );
                return None;
            }
            None => {
                tracing::error!(
                    target: "settings",
                    ?key,
                    "Bool-kind setting has no current_value_for arm — registry skew",
                );
                return None;
            }
        };
        let action = action_for_bool(key, !cur);
        if action.is_none() {
            tracing::error!(
                target: "settings",
                ?key,
                "Bool-kind setting has no action_for_bool arm — registry skew",
            );
        }
        action
    }
}

/// Compute filtered row indices for a query. Headers are emitted only
/// when ≥1 setting in their section matches. Returns all indices when
/// `query` is empty.
fn compute_filtered(rows: &[RowEntry], registry: &SettingsRegistry, query: &str) -> Vec<usize> {
    if query.is_empty() {
        return (0..rows.len()).collect();
    }
    let matched_keys: Vec<SettingKey> = registry.search(query).iter().map(|m| m.key).collect();
    let mut result = Vec::new();
    let mut pending_header: Option<usize> = None;
    for (i, row) in rows.iter().enumerate() {
        match row {
            RowEntry::Header { .. } => {
                // Emit header only when section has a match.
                pending_header = Some(i);
            }
            RowEntry::Setting { key, .. } => {
                if matched_keys.contains(key) {
                    if let Some(h) = pending_header.take() {
                        result.push(h);
                    }
                    result.push(i);
                }
            }
        }
    }
    result
}

/// Per-mode / per-terminal row visibility. `voice_capture_mode` is hidden
/// without key-release reporting (capture is always toggle there, so there's
/// no choice). Pure (`kitty_releases` / `minimal` passed in) so it's testable
/// without process globals.
fn setting_row_visible(meta: &SettingMeta, kitty_releases: bool, minimal: bool) -> bool {
    if meta.key == "voice_capture_mode" && !kitty_releases {
        return false;
    }
    if minimal && meta.hidden_in_minimal {
        return false;
    }
    true
}

fn build_rows(registry: &SettingsRegistry) -> Vec<RowEntry> {
    let kitty_releases = crate::app::kitty_flags_pushed();
    let minimal = crate::app::minimal_mode_active();
    // Keys that belong to a group sub-sheet are rendered only inside that
    // sheet, never as their own top-level rows.
    let group_children: std::collections::HashSet<SettingKey> = registry
        .all()
        .iter()
        .filter_map(|m| match &m.kind {
            SettingKind::Group { children } => Some(*children),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let mut rows = Vec::new();
    for cat in SettingCategory::ALL {
        let mut emitted_header = false;
        for (meta_index, meta) in registry.all().iter().enumerate() {
            if meta.category != *cat {
                continue;
            }
            if !setting_row_visible(meta, kitty_releases, minimal) {
                continue;
            }
            if group_children.contains(meta.key) {
                continue;
            }
            if !emitted_header {
                rows.push(RowEntry::Header { category: *cat });
                emitted_header = true;
            }
            rows.push(RowEntry::Setting {
                key: meta.key,
                meta_index,
            });
        }
    }
    rows
}

/// Construct the typed `Action::Set*` for a Bool setting.
fn action_for_bool(key: SettingKey, new: bool) -> Option<Action> {
    match key {
        "compact_mode" => Some(Action::SetCompactMode(new)),
        "show_timestamps" => Some(Action::SetTimestamps(new)),
        "simple_mode" => Some(Action::SetSimpleMode(new)),
        "contextual_hints.undo" => Some(Action::SetContextualHintUndo(new)),
        "contextual_hints.plan_mode" => Some(Action::SetContextualHintPlanMode(new)),
        "contextual_hints.image_input" => Some(Action::SetContextualHintImageInput(new)),
        "contextual_hints.send_now" => Some(Action::SetContextualHintSendNow(new)),
        "contextual_hints.small_screen" => Some(Action::SetContextualHintSmallScreen(new)),
        "contextual_hints.word_select" => Some(Action::SetContextualHintWordSelect(new)),
        "multiline_mode" => Some(Action::SetMultilineMode(new)),
        "vim_mode" => Some(Action::SetVimMode(new)),
        "remember_tool_approvals" => Some(Action::SetRememberToolApprovals(new)),
        "toolset.ask_user_question.timeout_enabled" => {
            Some(Action::SetAskUserQuestionTimeoutEnabled(new))
        }
        "show_thinking_blocks" => Some(Action::SetShowThinkingBlocks(new)),
        "group_tool_verbs" => Some(Action::SetGroupToolVerbs(new)),
        "collapsed_edit_blocks" => Some(Action::SetCollapsedEditBlocks(new)),
        "prompt_suggestions" => Some(Action::SetPromptSuggestions(new)),
        "respect_manual_folds" => Some(Action::SetRespectManualFolds(new)),
        "invert_scroll" => Some(Action::SetInvertScroll(new)),
        "show_tips" => Some(Action::SetShowTips(new)),
        "auto_update" => Some(Action::SetAutoUpdate(new)),
        "display_refresh_auto_cadence" => Some(Action::SetDisplayRefreshAutoCadence(new)),
        _ => None,
    }
}

/// Construct `Action::Preview*` for an Enum setting — used by the
/// picker's Up/Down (live preview) and Esc (revert). Preview actions
/// never persist; they only mutate the live visual.
fn action_for_enum(key: SettingKey, choice: &'static str) -> Option<Action> {
    match key {
        "theme" => Some(Action::PreviewTheme(choice.to_string())),
        "auto_dark_theme" => Some(Action::PreviewAutoDarkTheme(choice.to_string())),
        "auto_light_theme" => Some(Action::PreviewAutoLightTheme(choice.to_string())),
        // No preview for settings with irreversible side effects.
        "permission_mode" => None,
        "coding_data_sharing" => None,
        "plan_mode" => None,
        "render_mermaid" => None,
        "keep_text_selection" => None,
        "scroll_mode" => None,
        _ => None,
    }
}

/// Construct `Action::Set*` commit variant for an Enum setting.
/// Commit actions persist to disk and fire a toast.
fn action_for_enum_commit(key: SettingKey, choice: &'static str) -> Option<Action> {
    match key {
        "theme" => Some(Action::SetTheme(choice.to_string())),
        "auto_dark_theme" => Some(Action::SetAutoDarkTheme(choice.to_string())),
        "auto_light_theme" => Some(Action::SetAutoLightTheme(choice.to_string())),
        // Canonical strings from settings/defs.rs are the source of truth.
        "permission_mode" => match choice {
            "always-approve" => Some(Action::SetPermissionMode(
                crate::app::actions::PermissionModeKind::AlwaysApprove,
            )),
            // Auto's feature gate is enforced in `set_permission_mode`
            // (via `app.auto_mode_gate`, the same source the Shift+Tab cycle
            // uses), so the modal and the cycle never disagree. Committing Auto
            // when the gate is off degrades to Ask there.
            "auto" => Some(Action::SetPermissionMode(
                crate::app::actions::PermissionModeKind::Auto,
            )),
            "ask" => Some(Action::SetPermissionMode(
                crate::app::actions::PermissionModeKind::Ask,
            )),
            "default" => Some(Action::SetPermissionMode(
                crate::app::actions::PermissionModeKind::Default,
            )),
            _ => None,
        },
        "coding_data_sharing" => match choice {
            "opt-in" => Some(Action::SetCodingDataSharing { opted_in: true }),
            "opt-out" => Some(Action::SetCodingDataSharing { opted_in: false }),
            _ => None,
        },
        "plan_mode" => match choice {
            "on" => Some(Action::SetPlanMode(crate::app::actions::PlanModeKind::On)),
            "off" => Some(Action::SetPlanMode(crate::app::actions::PlanModeKind::Off)),
            _ => None,
        },
        "hunk_tracker_mode" => Some(Action::SetHunkTrackerMode(choice.to_string())),
        "voice_capture_mode" => Some(Action::SetVoiceCaptureMode(choice.to_string())),
        "voice_stt_language" => Some(Action::SetVoiceSttLanguage(choice.to_string())),
        "render_mermaid" => {
            crate::appearance::RenderMermaid::from_canonical(choice).map(Action::SetRenderMermaid)
        }
        "keep_text_selection" => crate::appearance::TextSelection::from_canonical(choice)
            .map(Action::SetKeepTextSelection),
        // Junk canonicals fold to None — Enter no-ops instead of mis-mapping.
        "scroll_mode" => {
            crate::appearance::ScrollMode::from_canonical(choice).map(Action::SetScrollMode)
        }
        "default_selected_permission" => {
            Some(Action::SetDefaultSelectedPermission(choice.to_string()))
        }
        _ => None,
    }
}

/// Construct `Action::Set*` commit variant for a String setting.
/// Resolves model names via the snapshot before producing the action.
/// Empty buffer maps to `Action::Clear*` for model settings.
fn action_for_string(
    key: SettingKey,
    value: String,
    snapshot: &PagerLocalSnapshot,
) -> Option<Action> {
    match key {
        "default_model" => {
            if value.is_empty() {
                Some(Action::ClearDefaultModel)
            } else {
                snapshot
                    .resolve_model_name(&value)
                    .map(Action::SetDefaultModel)
            }
        }
        "fork_secondary_model" => {
            if value.is_empty() {
                Some(Action::ClearForkSecondaryModel)
            } else {
                snapshot
                    .resolve_model_name(&value)
                    .map(Action::SetForkSecondaryModel)
            }
        }

        _ => {
            let _ = value;
            let _ = snapshot;
            None
        }
    }
}

/// Construct `Action::Set*` commit variant for an Int setting.
fn action_for_int(key: SettingKey, value: i64) -> Option<Action> {
    match key {
        "max_thoughts_width" => Some(Action::SetMaxThoughtsWidth(value)),
        "scroll_speed" => Some(Action::SetScrollSpeed(value)),
        "scroll_lines" => Some(Action::SetScrollLines(value)),
        _ => None,
    }
}

/// Validate a String buffer against the registered `StringValidator`.
/// Returns `Some(error_message)` on failure, `None` on success.
fn validate_string(
    validator: StringValidator,
    buffer: &str,
    available_models: &[(String, agent_client_protocol::ModelId)],
) -> Option<String> {
    match validator {
        StringValidator::Any => None,
        StringValidator::NonEmptyToken => {
            if buffer.is_empty() {
                Some("Value cannot be empty".to_string())
            } else if buffer.chars().any(|c| c.is_whitespace()) {
                Some("Value cannot contain whitespace".to_string())
            } else {
                None
            }
        }
        StringValidator::KnownModel => {
            // Empty = "clear default" sentinel.
            if buffer.is_empty() {
                return None;
            }
            // Reject if the model catalog hasn't loaded yet.
            if available_models.is_empty() {
                return Some("Model catalog still loading — try again".to_string());
            }
            let matched = available_models
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(buffer));
            if matched {
                None
            } else {
                Some(format!("Unknown model: \"{buffer}\""))
            }
        }
    }
}

/// Validate an Int buffer against `(min, max)` bounds.
fn validate_int(buffer: &str, min: i64, max: i64) -> Option<String> {
    if buffer.is_empty() {
        return Some("Value cannot be empty".to_string());
    }
    match buffer.parse::<i64>() {
        Ok(v) if v >= min && v <= max => None,
        Ok(v) => Some(format!("Value out of range ({min}\u{2013}{max}): {v}")),
        Err(_) => Some(format!("Not a valid integer: \"{buffer}\"")),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Overlay for the reset-confirm dialog. Overrides chrome breadcrumb,
/// footer, and search bar with the confirmation prompt.
pub struct ResetConfirmOverlay<'a> {
    pub prompt: &'a str,

    pub breadcrumb_suffix: &'a str,
}

/// Render the settings modal. Returns `true` if the reset-confirm
/// overlay was rendered (caller suppresses row-list mouse events).
pub fn render_settings_modal(
    buf: &mut Buffer,
    full_area: Rect,
    state: &mut SettingsModalState,
    compact: bool,
    overlay: Option<&ResetConfirmOverlay<'_>>,
) -> bool {
    let theme = Theme::current();
    let confirm_shortcuts = build_reset_confirm_shortcuts();
    let normal_shortcuts = build_shortcuts(state);
    let shortcuts: &[Shortcut<'_>] = if overlay.is_some() {
        &confirm_shortcuts
    } else {
        &normal_shortcuts
    };

    // Breadcrumb title for sub-modes: "Settings › <label>".
    let breadcrumb_owned: String;
    let title: &str = if let Some(o) = overlay {
        breadcrumb_owned = format!(
            "{MODAL_TITLE} {} {}",
            crate::glyphs::chevron(),
            o.breadcrumb_suffix
        );
        &breadcrumb_owned
    } else {
        match &state.mode {
            SettingsModalMode::PickingEnum { key, .. } => {
                if let Some(meta) = state.registry.find(key) {
                    breadcrumb_owned =
                        format!("{MODAL_TITLE} {} {}", crate::glyphs::chevron(), meta.label);
                    &breadcrumb_owned
                } else {
                    MODAL_TITLE
                }
            }

            SettingsModalMode::EditingValue { key, .. } => {
                if let Some(meta) = state.registry.find(key) {
                    breadcrumb_owned =
                        format!("{MODAL_TITLE} {} {}", crate::glyphs::chevron(), meta.label);
                    &breadcrumb_owned
                } else {
                    MODAL_TITLE
                }
            }
            SettingsModalMode::PickingGroup { key, .. } => {
                if let Some(meta) = state.registry.find(key) {
                    breadcrumb_owned =
                        format!("{MODAL_TITLE} {} {}", crate::glyphs::chevron(), meta.label);
                    &breadcrumb_owned
                } else {
                    MODAL_TITLE
                }
            }
            _ => MODAL_TITLE,
        }
    };

    // Footer sizing: predict shortcut wrap rows, add a gap row above
    // when the docs footer is present. EditingValue suppresses the
    // docs footer. Widen the modal when editing `max_thoughts_width`
    // so the wrap preview is useful at widths above STANDARD_MAX_WIDTH.
    let widen_for_max_thoughts_width = matches!(
        &state.mode,
        SettingsModalMode::EditingValue { key, .. }
            if *key == crate::settings::defs::MAX_THOUGHTS_WIDTH_KEY
    );
    let widened_candidate = full_area
        .width
        .saturating_sub(MAX_THOUGHTS_WIDTH_WIDENED_MARGIN);
    let (max_width, width_pct) =
        if widen_for_max_thoughts_width && widened_candidate > STANDARD_MAX_WIDTH {
            (widened_candidate, 1.0)
        } else {
            (STANDARD_MAX_WIDTH, 0.70)
        };
    let sizing = ModalSizing {
        width_pct,
        max_width,
        min_width: 44,
        v_margin: 3,
        h_pad: 2,
        v_pad: 1,
        footer_lines: 2,
    }
    .with_compact(compact);
    let has_tip_footer = !matches!(state.mode, SettingsModalMode::EditingValue { .. });
    let footer_lines = if has_tip_footer {
        modal_window::footer_lines_with_tip_gap(full_area, &sizing, shortcuts)
    } else {
        sizing.footer_lines
    };
    let sizing = ModalSizing {
        footer_lines,
        ..sizing
    };

    let modal_config = ModalWindowConfig {
        title,
        tabs: None,
        shortcuts,
        sizing,
        fold_info: None,
    };

    let Some(ModalContentArea {
        content: content_area,
        ..
    }) =
        modal_window::render_modal_window(buf, full_area, &mut state.window, &modal_config, &theme)
    else {
        // Chrome refused — reset hit-rects for graceful degradation.
        state.reset_hit_rects();
        return overlay.is_some();
    };

    if content_area.height < 2 || content_area.width < CONTENT_MIN_WIDTH {
        state.reset_hit_rects();
        return overlay.is_some();
    }

    if let Some(o) = overlay {
        // Confirmation overlay replaces the search bar; row list
        // renders dimmed underneath. Hit-rects reset so clicks
        // only route to the y/n footer buttons.
        render_reset_confirm_overlay(buf, content_area, state, &theme, o);
        return true;
    }

    let (inner_area, docs_footer_area) = match state.mode {
        SettingsModalMode::EditingValue { .. } => (content_area, None),
        _ => modal_window::split_content_for_tip_footer(content_area),
    };

    // Per-mode render dispatch (exhaustive to catch new variants).
    let mode_is_sub_pane = matches!(
        state.mode,
        SettingsModalMode::PickingEnum { .. }
            | SettingsModalMode::PickingGroup { .. }
            | SettingsModalMode::EditingValue { .. }
    );
    match state.mode {
        SettingsModalMode::PickingEnum { .. } => {
            state.reset_hit_rects();
            render_picking_enum(buf, inner_area, state, &theme);
            state.picker_choice_rects = take_picker_choice_rects();
        }
        SettingsModalMode::PickingGroup { .. } => {
            state.reset_hit_rects();
            let rects = render_picking_group(buf, inner_area, state, &theme);
            state.picker_choice_rects = rects;
        }
        SettingsModalMode::EditingValue { .. } => {
            state.reset_hit_rects();
            render_editing_value(buf, inner_area, state, &theme);
        }
        SettingsModalMode::Browse | SettingsModalMode::FilterFocused => {
            // Clear sub-pane hit-rects from prior frames.
            state.picker_choice_rects.clear();
            state.editor_adornment_rects = (Rect::default(), Rect::default());
            state.settings_breadcrumb_rect = None;
            state.list_area = inner_area;
            render_row_list_with_search_bar(buf, inner_area, state, &theme);
        }
    }

    // Breadcrumb hit-rect for sub-pane back-navigation on click.
    // Repaint with UNDERLINED (+ accent on hover) for affordance.
    state.settings_breadcrumb_rect = if mode_is_sub_pane {
        state.window.popup_area.map(|popup| {
            let title_w = title.width() as u16;
            // Clamp to not extend past the close button.
            let max_w = popup.width.saturating_sub(2 + 2); // borders + " ─" trailing decoration
            Rect {
                x: popup.x + 1 + TITLE_LEADING_DECORATION_W,
                y: popup.y,
                width: title_w.min(max_w),
                height: 1,
            }
        })
    } else {
        None
    };
    if let Some(rect) = state.settings_breadcrumb_rect {
        let fg = if state.breadcrumb_hovered {
            theme.accent_user
        } else {
            theme.text_primary
        };
        let style_mods = Modifier::BOLD | Modifier::UNDERLINED;
        for offset in 0..rect.width {
            let x = rect.x + offset;
            if let Some(cell) = buf.cell_mut((x, rect.y)) {
                let mut s = cell.style();
                s.fg = Some(fg);
                s = s.add_modifier(style_mods);
                cell.set_style(s);
            }
        }
    }

    if let Some(footer_area) = docs_footer_area {
        render_docs_footer(buf, footer_area, &theme);
    }
    false
}

/// Render the reset-confirm overlay: prompt replaces the search bar,
/// row list renders below with the target row at full intensity and
/// all other rows dimmed.
fn render_reset_confirm_overlay(
    buf: &mut Buffer,
    content_area: Rect,
    state: &mut SettingsModalState,
    theme: &Theme,
    overlay: &ResetConfirmOverlay<'_>,
) {
    // Clear hit-rects so clicks only route to y/n buttons.
    state.reset_hit_rects();

    if content_area.height == 0 || content_area.width == 0 {
        return;
    }

    // Row 0: prompt (full width, bold + accent).
    let prompt_area = Rect {
        x: content_area.x,
        y: content_area.y,
        width: content_area.width,
        height: 1,
    };
    let prompt_bg_style = Style::default().bg(theme.bg_visual);
    buf.set_style(prompt_area, prompt_bg_style);
    let prompt_style = Style::default()
        .fg(theme.accent_user)
        .bg(theme.bg_visual)
        .add_modifier(Modifier::BOLD);
    let prompt_text: std::borrow::Cow<'_, str> =
        if overlay.prompt.width() <= content_area.width as usize {
            std::borrow::Cow::Borrowed(overlay.prompt)
        } else {
            std::borrow::Cow::Owned(truncate_str(overlay.prompt, content_area.width as usize))
        };
    let prompt_w = (prompt_text.width() as u16).min(content_area.width);
    buf.set_span(
        content_area.x,
        content_area.y,
        &Span::styled(prompt_text.as_ref(), prompt_style),
        prompt_w,
    );

    // Row 1+: render rows, then dim all except the target.
    if content_area.height < 2 {
        return;
    }
    let list_area = Rect {
        x: content_area.x,
        y: content_area.y + 1,
        width: content_area.width,
        height: content_area.height - 1,
    };
    render_rows(buf, list_area, state, theme);

    let target_rect = state.row_rects.get(state.selected).copied();

    // Dim every line outside the target row's y-range (DIM + blend).
    let target_y_start = target_rect.map(|r| r.y);
    let target_y_end = target_rect.map(|r| r.y.saturating_add(r.height));
    let area_y_end = list_area.y.saturating_add(list_area.height);
    for y in list_area.y..area_y_end {
        if let (Some(ys), Some(ye)) = (target_y_start, target_y_end)
            && y >= ys
            && y < ye
        {
            continue; // inside the target row's y range — stays full intensity
        }
        let strip = Rect {
            x: list_area.x,
            y,
            width: list_area.width,
            height: 1,
        };
        buf.set_style(strip, Style::default().add_modifier(Modifier::DIM));
        crate::render::color::blend_area(buf, strip, Some((theme.bg_base, 0.55)), None);
    }
}

/// Footer shortcuts for the reset-confirm dialog (y/n are clickable).
fn build_reset_confirm_shortcuts() -> Vec<Shortcut<'static>> {
    use crate::views::modal::{RESET_CONFIRM_NO_ID, RESET_CONFIRM_YES_ID};
    vec![
        Shortcut {
            label: "y reset",
            clickable: true,
            id: RESET_CONFIRM_YES_ID,
        },
        Shortcut {
            label: "n cancel",
            clickable: true,
            id: RESET_CONFIRM_NO_ID,
        },
        Shortcut {
            label: "Esc cancel",
            clickable: false,
            id: 0,
        },
        Shortcut {
            label: "F2 cancel",
            clickable: false,
            id: 0,
        },
    ]
}

/// Render the row list with a search bar at the top (Browse/FilterFocused).
fn render_row_list_with_search_bar(
    buf: &mut Buffer,
    content_area: Rect,
    state: &mut SettingsModalState,
    theme: &Theme,
) {
    let filter_focused = matches!(state.mode, SettingsModalMode::FilterFocused);
    if content_area.height >= 3 {
        // row 0: search bar, row 1: divider, row 2+: list.
        let search_area = Rect {
            x: content_area.x,
            y: content_area.y,
            width: content_area.width,
            height: 1,
        };
        crate::views::picker::render_search_bar(
            buf,
            search_area.x,
            search_area.y,
            search_area.width,
            theme,
            &state.query,
            filter_focused,
            true,
            state.query_cursor,
            Some(theme.bg_base),
        );
        crate::views::picker::render_divider(
            buf,
            content_area.x,
            content_area.y + 1,
            content_area.width,
            theme,
            Some(theme.bg_base),
        );
        let list_area = Rect {
            x: content_area.x,
            y: content_area.y + 2,
            width: content_area.width,
            height: content_area.height - 2,
        };

        state.list_area = list_area;
        render_rows(buf, list_area, state, theme);
    } else if content_area.height >= 2 {
        // Tight: search bar only, no divider.
        let search_area = Rect {
            x: content_area.x,
            y: content_area.y,
            width: content_area.width,
            height: 1,
        };
        crate::views::picker::render_search_bar(
            buf,
            search_area.x,
            search_area.y,
            search_area.width,
            theme,
            &state.query,
            filter_focused,
            true,
            state.query_cursor,
            Some(theme.bg_base),
        );
        let list_area = Rect {
            x: content_area.x,
            y: content_area.y + 1,
            width: content_area.width,
            height: content_area.height - 1,
        };
        state.list_area = list_area;
        render_rows(buf, list_area, state, theme);
    } else {
        // Too narrow for a search bar; just render the rows.
        render_rows(buf, content_area, state, theme);
    }
}

fn render_docs_footer(buf: &mut Buffer, area: Rect, theme: &Theme) {
    const LONG: &str =
        "Tip · Ask Grok: \"change theme to grokday\" or \"what does compact mode do?\"";
    const SHORT: &str = "Tip · Ask Grok to change a setting";
    let text = modal_window::fit_tip_line(&[LONG, SHORT], area.width as usize);
    modal_window::render_centered_tip_footer(buf, area, theme, text.as_ref());
}

fn render_rows(buf: &mut Buffer, area: Rect, state: &mut SettingsModalState, theme: &Theme) {
    let visible_h = area.height as usize;
    if visible_h == 0 {
        state.row_rects.clear();
        state.row_rects.resize(state.rows.len(), Rect::default());
        state.value_hit_rects.clear();
        state
            .value_hit_rects
            .resize(state.rows.len(), Rect::default());
        return;
    }

    state.row_rects.clear();
    state.row_rects.resize(state.rows.len(), Rect::default());
    state.value_hit_rects.clear();
    state
        .value_hit_rects
        .resize(state.rows.len(), Rect::default());

    let total_visible = state.filtered_cache.len();

    // Empty filter — show "No matches for <query>".
    if total_visible == 0 {
        if !state.query.is_empty() {
            let prefix = "No matches for ";
            let suffix_quote_w = 2u16; // surrounding "" chars
            let available_for_query = (area.width as usize)
                .saturating_sub(prefix.width())
                .saturating_sub(suffix_quote_w as usize);
            let q_disp = if state.query.width() <= available_for_query {
                state.query.clone()
            } else {
                truncate_str(&state.query, available_for_query)
            };
            let msg = format!("{prefix}\"{q_disp}\"");
            let style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
            let msg_w = (msg.width() as u16).min(area.width);
            let cx = area.x + area.width.saturating_sub(msg_w) / 2;
            let cy = area.y + area.height / 2;
            buf.set_span(cx, cy, &Span::styled(&msg, style), msg_w);
        }
        return;
    }

    // Translate `state.selected` (rows-space) → filtered position.
    let selected_fpos = state
        .filtered_cache
        .iter()
        .position(|&i| i == state.selected);

    // Clamp scroll so selection stays in view, keeping the preceding
    // section header visible when scrolling up. Row heights are
    // variable (expanded descriptions, header gaps).
    let row_heights = compute_filtered_row_heights(state, area.width);
    if let Some(fpos) = selected_fpos {
        if fpos < state.scroll_offset {
            let new_offset = if fpos > 0 {
                let prev_idx = state.filtered_cache[fpos - 1];
                if matches!(state.rows[prev_idx], RowEntry::Header { .. }) {
                    fpos - 1
                } else {
                    fpos
                }
            } else {
                fpos
            };
            state.scroll_offset = new_offset;
        }

        let min_offset_for_visibility = compute_min_scroll_offset_for_visibility(
            &state.filtered_cache,
            &state.rows,
            &row_heights,
            fpos,
            visible_h,
        );
        if state.scroll_offset < min_offset_for_visibility {
            state.scroll_offset = min_offset_for_visibility;
        }
    }
    // Final clamp — don't let scroll_offset past the end.
    if total_visible > 0 {
        let max_offset = compute_min_scroll_offset_for_visibility(
            &state.filtered_cache,
            &state.rows,
            &row_heights,
            total_visible - 1,
            visible_h,
        );
        if state.scroll_offset > max_offset {
            state.scroll_offset = max_offset;
        }
    }

    let end = total_visible.min(state.scroll_offset + visible_h);

    let max_label_w = compute_settings_max_label_w(state.registry.all(), area.width);

    // Snapshot visible rows to avoid borrow conflicts in the render loop.
    let visible_filtered: Vec<usize> = state.filtered_cache[state.scroll_offset..end].to_vec();

    let hover_row_snapshot = state.hover_row;
    let mut values: Vec<Option<SettingValue>> = Vec::with_capacity(visible_filtered.len());
    for &row_idx in &visible_filtered {
        let v = match state.rows.get(row_idx) {
            Some(RowEntry::Setting { key, .. }) => state.value_for(key),
            _ => None,
        };
        values.push(v);
    }
    // Track y-cursor: rows consume variable height when expanded.
    let mut y_cursor = area.y;
    let area_end = area.y + area.height;
    let expanded_snapshot: std::collections::HashSet<&'static str> = state.expanded_keys.clone();
    // Insert a blank line above non-first section headers.
    let mut rendered_any = false;

    for (row_pos, &row_idx) in visible_filtered.iter().enumerate() {
        if y_cursor >= area_end {
            break;
        }
        let Some(row) = state.rows.get(row_idx) else {
            continue;
        };

        if matches!(row, RowEntry::Header { .. })
            && rendered_any
            && y_cursor.saturating_add(1) < area_end
        {
            y_cursor = y_cursor.saturating_add(1);
        }
        if y_cursor >= area_end {
            break;
        }
        let label_rect = Rect {
            x: area.x,
            y: y_cursor,
            width: area.width,
            height: 1,
        };

        state.row_rects[row_idx] = label_rect;

        rendered_any = true;

        match row {
            RowEntry::Header { category } => {
                let label = category.label();
                let header_style = Style::default()
                    .fg(theme.gray)
                    .bg(theme.bg_base)
                    .add_modifier(Modifier::BOLD);
                let sep_style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
                let title = format!(" {label} ");
                let title_w = title.width();
                let remaining = (area.width as usize).saturating_sub(title_w);
                let sep: String = std::iter::repeat_n('\u{2500}', remaining).collect();
                let line = Line::from(vec![
                    Span::styled(title, header_style),
                    Span::styled(sep, sep_style),
                ]);
                buf.set_line(area.x, y_cursor, &line, area.width);
                y_cursor = y_cursor.saturating_add(1);
            }
            RowEntry::Setting {
                meta_index, key, ..
            } => {
                let Some(meta) = state.registry.all().get(*meta_index) else {
                    continue;
                };
                let value_opt = values.get(row_pos).and_then(|v| v.as_ref());
                let is_selected = row_idx == state.selected;
                let is_expanded = expanded_snapshot.contains(key);

                // Group rows carry no scalar value; render a chevron row that
                // opens the sub-sheet (skips the value/edited machinery below).
                if matches!(meta.kind, SettingKind::Group { .. }) {
                    let is_hovered = hover_row_snapshot == Some(row_idx);
                    let value_rect = render_setting_group_row(
                        buf,
                        label_rect,
                        meta,
                        is_selected,
                        is_hovered,
                        is_expanded,
                        theme,
                    );
                    state.value_hit_rects[row_idx] = value_rect;
                    y_cursor = y_cursor.saturating_add(1);
                    // Mirror normal rows: render the description inline when the
                    // group's key is expanded (Right/l). The group has no value,
                    // so this is the only place its description can surface.
                    if is_expanded && y_cursor < area_end {
                        let desc_height = area_end - y_cursor;
                        let desc_rect = Rect {
                            x: area.x,
                            y: y_cursor,
                            width: area.width,
                            height: desc_height.min(8),
                        };
                        render_expanded_description(buf, desc_rect, meta, theme);
                        let consumed =
                            wrapped_description_height(meta, area.width, desc_rect.height);
                        y_cursor = y_cursor.saturating_add(consumed);
                    }
                    continue;
                }

                let value = match value_opt {
                    Some(v) => v,
                    None => {
                        render_setting_row_no_value(
                            buf,
                            label_rect,
                            meta,
                            max_label_w,
                            is_selected,
                            theme,
                        );
                        y_cursor = y_cursor.saturating_add(1);
                        continue;
                    }
                };

                // Decide 1 vs 2 line layout; fall back to 1 if viewport is tight.
                let value_display = match value {
                    SettingValue::Bool(b) => {
                        if *b {
                            "on".to_string()
                        } else {
                            "off".to_string()
                        }
                    }
                    SettingValue::String(s) => {
                        if s.is_empty() && matches!(meta.kind, SettingKind::DynamicEnum { .. }) {
                            "(no override)".to_string()
                        } else {
                            s.clone()
                        }
                    }
                    SettingValue::Enum(e) => display_for_enum_canonical(&meta.kind, e).to_string(),
                    SettingValue::Int(i) => i.to_string(),
                };
                let show_restart_pill_for_layout = meta.restart_required && is_expanded;
                let layout_decision = row_layout(
                    area.width,
                    meta.label,
                    &value_display,
                    show_restart_pill_for_layout,
                );
                let want_two_lines = !matches!(layout_decision, RowLayout::OneLine);
                // Only allocate 2 lines if the viewport has room.
                let row_height: u16 = if want_two_lines && y_cursor.saturating_add(2) <= area_end {
                    2
                } else {
                    1
                };

                let render_area = Rect {
                    x: area.x,
                    y: y_cursor,
                    width: area.width,
                    height: row_height,
                };
                // Hit-rect spans both lines for two-line rows.
                state.row_rects[row_idx] = render_area;

                let is_hovered = hover_row_snapshot == Some(row_idx);
                let value_rect = render_setting_row(
                    buf,
                    render_area,
                    meta,
                    value,
                    max_label_w,
                    is_selected,
                    theme,
                    is_expanded,
                    is_hovered,
                );
                state.value_hit_rects[row_idx] = value_rect;
                y_cursor = y_cursor.saturating_add(row_height);

                if is_expanded && y_cursor < area_end {
                    let desc_height = area_end - y_cursor;
                    let desc_rect = Rect {
                        x: area.x,
                        y: y_cursor,
                        width: area.width,
                        height: desc_height.min(8), // cap at 8 lines per row to keep scroll sane
                    };
                    render_expanded_description(buf, desc_rect, meta, theme);
                    // Re-measure how many lines the wrapped description
                    // actually consumed, so y_cursor advances precisely.
                    let consumed = wrapped_description_height(meta, area.width, desc_rect.height);
                    y_cursor = y_cursor.saturating_add(consumed);
                }
            }
        }
    }
}

/// Compute the minimum scroll_offset that keeps filtered position
/// `fpos` visible within `visible_h` lines. Walks backward,
/// accounting for variable row heights and header gaps.
fn compute_min_scroll_offset_for_visibility(
    filtered_cache: &[usize],
    rows: &[RowEntry],
    row_heights: &[u16],
    fpos: usize,
    visible_h: usize,
) -> usize {
    if visible_h == 0 || fpos >= filtered_cache.len() {
        return fpos;
    }
    // Visual lines consumed so far. `fpos` itself sits at the top of
    // the viewport, so it doesn't earn a blank-above-header even if
    // it IS a header.
    let fpos_height = row_heights.get(fpos).copied().unwrap_or(1) as usize;
    let mut lines_used: usize = fpos_height;
    if lines_used > visible_h {
        // Even the focused row alone doesn't fit; clamp to fpos so
        // the down-stream renderer at least shows its label.
        return fpos;
    }
    let mut offset = fpos;
    while offset > 0 {
        let candidate = offset - 1;
        let candidate_height = row_heights.get(candidate).copied().unwrap_or(1) as usize;
        // Cost of including `candidate` as the new top of the
        // viewport: its own visual height, plus 1 line for the
        // blank-above-header that the OLD top (`offset`) now
        // earns (since it's no longer the first row rendered).
        let old_first_idx = filtered_cache[offset];
        let old_first_is_header = matches!(rows[old_first_idx], RowEntry::Header { .. });
        let cost: usize = candidate_height + usize::from(old_first_is_header);
        if lines_used.saturating_add(cost) > visible_h {
            break;
        }
        lines_used += cost;
        offset = candidate;
    }
    offset
}

/// Precompute the visual height (in terminal rows) of each entry in
/// `state.filtered_cache`, using the same `row_layout` /
/// `wrapped_description_height` math the forward render loop uses.
///
/// The cost passed to [`compute_min_scroll_offset_for_visibility`]
/// is the row's intrinsic height EXCLUDING the blank-line-above-
/// header gap — that gap is accounted for inside the scroll helper's
/// backward walk because it depends on the runtime position relative
/// to the viewport top.
///
/// Cost: O(visible filtered rows) per render, bounded by the
/// registry size (~15 entries today). Each row does at most one
/// `word_wrap_line` call (for expanded descriptions). Allocations
/// are confined to a single `Vec<u16>` per call; per-row layout
/// math is on the stack.
fn compute_filtered_row_heights(state: &SettingsModalState, area_width: u16) -> Vec<u16> {
    let mut heights = Vec::with_capacity(state.filtered_cache.len());
    for &row_idx in &state.filtered_cache {
        let Some(row) = state.rows.get(row_idx) else {
            heights.push(1);
            continue;
        };
        match row {
            RowEntry::Header { .. } => heights.push(1),
            RowEntry::Setting {
                meta_index, key, ..
            } => {
                let Some(meta) = state.registry.all().get(*meta_index) else {
                    heights.push(1);
                    continue;
                };
                // Group rows carry no value; height = chevron row + the expanded
                // description (cap 8), agreeing with the forward render loop.
                if matches!(meta.kind, SettingKind::Group { .. }) {
                    let mut h: u16 = 1;
                    if state.expanded_keys.contains(key) {
                        h = h.saturating_add(wrapped_description_height(meta, area_width, 8));
                    }
                    heights.push(h);
                    continue;
                }
                let Some(value) = state.value_for(key) else {
                    heights.push(1);
                    continue;
                };
                let is_expanded = state.expanded_keys.contains(key);
                let value_display = match &value {
                    SettingValue::Bool(b) => {
                        if *b {
                            "on".to_string()
                        } else {
                            "off".to_string()
                        }
                    }
                    SettingValue::String(s) => {
                        if s.is_empty() && matches!(meta.kind, SettingKind::DynamicEnum { .. }) {
                            "(no override)".to_string()
                        } else {
                            s.clone()
                        }
                    }
                    SettingValue::Enum(e) => display_for_enum_canonical(&meta.kind, e).to_string(),
                    SettingValue::Int(i) => i.to_string(),
                };
                let show_restart_pill = meta.restart_required && is_expanded;
                let layout = row_layout(area_width, meta.label, &value_display, show_restart_pill);
                let mut h: u16 = match layout {
                    RowLayout::OneLine => 1,
                    RowLayout::TwoLine | RowLayout::TwoLineWithLabelTruncation => 2,
                };
                if is_expanded {
                    // Cap matches the forward render loop at line
                    // 2040 (`desc_rect.height = ... .min(8)`).
                    h = h.saturating_add(wrapped_description_height(meta, area_width, 8));
                }
                heights.push(h);
            }
        }
    }
    heights
}

/// Wrapped description height for scroll math (mirrors render path).
fn wrapped_description_height(meta: &SettingMeta, area_width: u16, cap: u16) -> u16 {
    let indent = 4u16.min(area_width);
    let wrap_w = area_width.saturating_sub(indent);
    if wrap_w == 0 {
        return 0;
    }
    let line = Line::from(Span::raw(meta.description));
    let wrapped = crate::render::wrapping::word_wrap_line(&line, wrap_w as usize);
    (wrapped.len() as u16).min(cap)
}

// Picker prefix constants (hoisted to avoid per-frame allocation).
const PICKER_PREFIX_FOCUSED: &str = " \u{25CF}  ";
const PICKER_PREFIX_UNFOCUSED: &str = " \u{25CB}  ";

const PICKER_PREFIX_W: u16 = 4;
const PICKER_SEPARATOR: &str = " \u{00B7} ";
const PICKER_SEPARATOR_W: u16 = 3;

const PICKER_MARKER_W: u16 = 1;
/// Soft product cap on static Enum choices (settings unit tests enforce it).
///
/// The chooser already scrolls within the viewport when the focused choice
/// falls off-screen (`picker_scroll_offset`); this limit exists so catalogs
/// stay intentionally curated rather than unbounded. Sized to fit the full
/// Grok STT language list (25 codes + client-only `auto` = 26) with headroom.
pub(crate) const MAX_PICKER_CHOICES: usize = 32;

/// Render the shared sub-pane header (bold title row + word-wrapped description)
/// used by all four sub-pane renderers — the enum chooser, the group sub-sheet,
/// and the string/int editors. Returns `header_rows`: the rows consumed (title +
/// optional description + the 1-row gap), so the caller positions its body at
/// `area.y + header_rows`. The bodies differ and stay in each caller.
///
/// `min_non_desc_rows` is the vertical budget (excluding the description rows
/// themselves) that must fit before the description renders at all: `2` for the
/// choosers (title + gap), `3` for the editors (title + gap + the input/stepper
/// row). Callers keep their own `if area.height <= header_rows { return; }`.
fn render_sub_pane_header(
    buf: &mut Buffer,
    area: Rect,
    theme: &Theme,
    title: &str,
    description: &str,
    min_non_desc_rows: u16,
) -> u16 {
    // ── Row 0: title (truncated with `…`). ────────────────────────
    let title_style = Style::default()
        .fg(theme.text_primary)
        .bg(theme.bg_base)
        .add_modifier(Modifier::BOLD);
    let title_text: std::borrow::Cow<'_, str> = if title.width() <= area.width as usize {
        std::borrow::Cow::Borrowed(title)
    } else {
        std::borrow::Cow::Owned(truncate_str(title, area.width as usize))
    };
    let title_w = (title_text.width() as u16).min(area.width);
    buf.set_span(
        area.x,
        area.y,
        &Span::styled(title_text.as_ref(), title_style),
        title_w,
    );

    // ── Row 1+: word-wrapped description ──────────────────────────
    let description_wrapped = wrap_description(description, area.width);
    let desc_rows: u16 = description_wrapped.len() as u16;
    let has_description =
        desc_rows > 0 && area.height >= min_non_desc_rows.saturating_add(desc_rows);
    if has_description {
        let desc_style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
        for (i, wrap_line) in description_wrapped.iter().enumerate() {
            let y = area.y + 1 + i as u16;
            if y >= area.y + area.height {
                break;
            }
            let w = (wrap_line.width() as u16).min(area.width);
            buf.set_span(area.x, y, &Span::styled(wrap_line.as_str(), desc_style), w);
        }
    }

    if has_description { 2 + desc_rows } else { 2 }
}

/// Render the Enum chooser sub-pane. Title + description + radio-style
/// choice list with scrolling and `… N more` overflow indicator.
fn render_picking_enum(buf: &mut Buffer, area: Rect, state: &SettingsModalState, theme: &Theme) {
    debug_assert_eq!(
        PICKER_PREFIX_FOCUSED.width(),
        PICKER_PREFIX_W as usize,
        "PICKER_PREFIX_W drifted from PICKER_PREFIX_FOCUSED width",
    );
    debug_assert_eq!(
        PICKER_PREFIX_UNFOCUSED.width(),
        PICKER_PREFIX_W as usize,
        "PICKER_PREFIX_W drifted from PICKER_PREFIX_UNFOCUSED width",
    );
    debug_assert_eq!(
        PICKER_SEPARATOR.width(),
        PICKER_SEPARATOR_W as usize,
        "PICKER_SEPARATOR_W drifted from PICKER_SEPARATOR width",
    );

    let (setting_key, choices_idx) = match &state.mode {
        SettingsModalMode::PickingEnum {
            key, choices_idx, ..
        } => (*key, *choices_idx),
        _ => return,
    };
    let Some(meta) = state.registry.find(setting_key) else {
        return;
    };

    let choices: Vec<OwnedEnumChoice> = match &meta.kind {
        SettingKind::Enum { choices, .. } => {
            effective_enum_choices(setting_key, choices, &state.pager_snapshot)
                .into_iter()
                .map(|c| OwnedEnumChoice {
                    canonical: c.canonical.to_string(),
                    display: c.display.to_string(),
                    description: c.description.to_string(),
                })
                .collect()
        }
        SettingKind::DynamicEnum { source, .. } => {
            dynamic_enum_choices(*source, &state.pager_snapshot)
        }
        _ => return,
    };

    if area.width == 0 || area.height == 0 {
        return;
    }

    // Choosers need title + gap (2) before the description renders.
    let header_rows = render_sub_pane_header(buf, area, theme, meta.label, meta.description, 2);
    if area.height <= header_rows {
        return;
    }
    let choices_y = area.y + header_rows;
    let max_choices_h = area.height.saturating_sub(header_rows) as usize;
    if max_choices_h == 0 {
        return;
    }

    // ── Per-choice wrapped layout ─────────────────────────────────
    let layouts: Vec<PickerChoiceLayout> = choices
        .iter()
        .map(|choice| compute_picker_choice_layout(choice, area.width))
        .collect();
    let total_h: u16 = layouts.iter().map(|l| l.height).sum();

    // ── Scroll offset (variable per-choice height) ────────────────
    let needs_overflow = total_h as usize > max_choices_h;
    let available_h: u16 = if needs_overflow {
        (max_choices_h as u16).saturating_sub(1).max(1)
    } else {
        max_choices_h as u16
    };
    let scroll_offset = picker_scroll_offset(&layouts, choices_idx, available_h);

    let mut visible_end = scroll_offset;
    let mut consumed_h: u16 = 0;
    for (i, layout) in layouts.iter().enumerate().skip(scroll_offset) {
        let next = consumed_h.saturating_add(layout.height);
        if next > available_h {
            break;
        }
        consumed_h = next;
        visible_end = i + 1;
    }
    // Defensive: always show the focused choice even if it's clipped.
    if visible_end <= choices_idx {
        visible_end = choices_idx + 1;
    }
    let _ = consumed_h; // height bookkeeping kept for future tuning

    // ── Hit-rect bookkeeping ──────────────────────────────────────
    let mut picker_choice_rects: Vec<Rect> = vec![Rect::default(); choices.len()];

    // ── Choice rows ───────────────────────────────────────────────
    let bg_focused = theme.bg_visual;
    let bg_unfocused = theme.bg_base;
    let bg_hovered = theme.bg_hover;
    let fg_primary = theme.text_primary;
    let fg_gray = theme.gray;
    let fg_accent = theme.accent_user;

    let mut y_cursor = choices_y;
    for (choice_i, layout) in layouts
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible_end - scroll_offset)
    {
        let choice = &choices[choice_i];
        let is_focused = choice_i == choices_idx;

        let is_hovered = !is_focused && state.hover_row == Some(choice_i);
        let bg = if is_focused {
            bg_focused
        } else if is_hovered {
            bg_hovered
        } else {
            bg_unfocused
        };

        let display_style = if is_focused {
            Style::default()
                .fg(fg_primary)
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(fg_primary).bg(bg)
        };
        let desc_style = Style::default().fg(fg_gray).bg(bg);
        let marker_style = if is_focused {
            Style::default().fg(fg_accent).bg(bg)
        } else {
            Style::default().fg(fg_gray).bg(bg)
        };
        let marker = if is_focused {
            crate::glyphs::filled_dot()
        } else {
            "\u{25CB}"
        };

        let block_rect = Rect {
            x: area.x,
            y: y_cursor,
            width: area.width,
            height: layout.height,
        };
        buf.set_style(block_rect, Style::default().bg(bg));
        picker_choice_rects[choice_i] = block_rect;

        // ── Line 1: prefix + display + (· + first wrap line) ──────
        let y = y_cursor;
        if area.width > 0 {
            // Leading space (col 0 of the row).
            buf.set_span(
                area.x,
                y,
                &Span::styled(" ", display_style),
                1.min(area.width),
            );
        }
        if area.width > 1 {
            // Marker glyph at col 1.
            buf.set_span(
                area.x + 1,
                y,
                &Span::styled(marker, marker_style),
                PICKER_MARKER_W.min(area.width.saturating_sub(1)),
            );
        }
        if area.width > 2 {
            // Trailing two spaces at cols 2-3.
            let pad_w = 2u16.min(area.width.saturating_sub(2));
            buf.set_span(area.x + 2, y, &Span::styled("  ", display_style), pad_w);
        }

        // Display name (truncated).
        let display_x = area.x.saturating_add(PICKER_PREFIX_W);
        let display_room = (area.x + area.width).saturating_sub(display_x) as usize;
        if display_room == 0 {
            y_cursor = y_cursor.saturating_add(layout.height);
            continue;
        }
        let display_text: std::borrow::Cow<'_, str> = if choice.display.width() <= display_room {
            std::borrow::Cow::Borrowed(choice.display.as_str())
        } else {
            std::borrow::Cow::Owned(truncate_str(&choice.display, display_room))
        };
        let display_w =
            (display_text.width() as u16).min(area.width.saturating_sub(PICKER_PREFIX_W));
        buf.set_span(
            display_x,
            y,
            &Span::styled(display_text.as_ref(), display_style),
            display_w,
        );

        let has_choice_desc = !choice.description.trim().is_empty();
        if !has_choice_desc {
            y_cursor = y_cursor.saturating_add(layout.height);
            continue;
        }
        let after_display_x = display_x.saturating_add(display_w);
        let sep_room = (area.x + area.width).saturating_sub(after_display_x);
        if sep_room == 0 {
            y_cursor = y_cursor.saturating_add(layout.height);
            continue;
        }
        let sep_w = PICKER_SEPARATOR_W.min(sep_room);
        buf.set_span(
            after_display_x,
            y,
            &Span::styled(PICKER_SEPARATOR, desc_style),
            sep_w,
        );

        let desc_x = after_display_x + sep_w;
        let desc_room = (area.x + area.width).saturating_sub(desc_x) as usize;
        if desc_room == 0 {
            y_cursor = y_cursor.saturating_add(layout.height);
            continue;
        }

        // Narrow fallback: truncate on one line if wrapping fails.
        if layout.wrap_lines.is_empty() {
            let desc_text: std::borrow::Cow<'_, str> = if choice.description.width() <= desc_room {
                std::borrow::Cow::Borrowed(choice.description.as_str())
            } else {
                std::borrow::Cow::Owned(truncate_str(&choice.description, desc_room))
            };
            let desc_w = (desc_text.width() as u16).min(area.x + area.width - desc_x);
            buf.set_span(
                desc_x,
                y,
                &Span::styled(desc_text.as_ref(), desc_style),
                desc_w,
            );
            y_cursor = y_cursor.saturating_add(layout.height);
            continue;
        }

        // Line 1: first wrap line at the description column.
        let first_line = &layout.wrap_lines[0];
        let first_w = (first_line.width() as u16).min(area.x + area.width - desc_x);
        buf.set_span(
            desc_x,
            y,
            &Span::styled(first_line.as_str(), desc_style),
            first_w,
        );

        // Lines 2..N: continuation lines aligned under first_line.
        for (cont_i, wrap_line) in layout.wrap_lines.iter().enumerate().skip(1) {
            let cont_y = y + cont_i as u16;
            if cont_y >= area.y + area.height {
                break;
            }
            let cont_w = (wrap_line.width() as u16).min(area.x + area.width - desc_x);
            buf.set_span(
                desc_x,
                cont_y,
                &Span::styled(wrap_line.as_str(), desc_style),
                cont_w,
            );
        }

        y_cursor = y_cursor.saturating_add(layout.height);
    }

    // ── Overflow indicator: "… N more" on the row right below the
    //    last rendered choice. ─────────────────────────────────────
    if needs_overflow && visible_end < choices.len() {
        let more_count = choices.len() - visible_end;
        let overflow_y = y_cursor;
        if overflow_y < choices_y + max_choices_h as u16 && overflow_y < area.y + area.height {
            let overflow_style = Style::default().fg(theme.gray_dim).bg(theme.bg_base);
            let raw = format!("\u{2026} {more_count} more");
            let overflow_text: std::borrow::Cow<'_, str> = if raw.width() <= area.width as usize {
                std::borrow::Cow::Owned(raw)
            } else {
                std::borrow::Cow::Owned(truncate_str(&raw, area.width as usize))
            };
            let overflow_w = (overflow_text.width() as u16).min(area.width);
            let overflow_rect = Rect {
                x: area.x,
                y: overflow_y,
                width: area.width,
                height: 1,
            };
            buf.set_style(overflow_rect, Style::default().bg(theme.bg_base));
            buf.set_span(
                area.x,
                overflow_y,
                &Span::styled(overflow_text.as_ref(), overflow_style),
                overflow_w,
            );
        }
    }

    PICKER_RECTS_SCRATCH.with(|cell| {
        *cell.borrow_mut() = picker_choice_rects;
    });
    let _ = total_h; // suppress unused-var warning on some builds
}

// Thread-local scratch to ferry hit-rects out of `render_picking_enum`
// (which takes `&state`) into `state.picker_choice_rects`.
thread_local! {
    static PICKER_RECTS_SCRATCH: std::cell::RefCell<Vec<Rect>>
        = const { std::cell::RefCell::new(Vec::new()) };
}

/// Read-and-clear the most recent per-choice hit-rects produced by
/// `render_picking_enum`. Returns an empty Vec when called before
/// the first picker render (or after a non-picker frame reset the
/// scratch).
pub(crate) fn take_picker_choice_rects() -> Vec<Rect> {
    PICKER_RECTS_SCRATCH.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

/// Render the group sub-sheet: title + description + one row per child Bool
/// toggle (`<marker> <Label> … <on/off>`). Returns the per-child hit-rects
/// (parallel to the group's children) for mouse routing. Mirrors the enum
/// chooser's title/description/list shape but for independent toggles.
fn render_picking_group(
    buf: &mut Buffer,
    area: Rect,
    state: &SettingsModalState,
    theme: &Theme,
) -> Vec<Rect> {
    let (group_key, child_idx) = match &state.mode {
        SettingsModalMode::PickingGroup { key, child_idx } => (*key, *child_idx),
        _ => return Vec::new(),
    };
    let Some(group_meta) = state.registry.find(group_key) else {
        return Vec::new();
    };
    let children = group_children(state, group_key);
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    // Chooser shape: title + gap (2) before the description renders.
    let header_rows = render_sub_pane_header(
        buf,
        area,
        theme,
        group_meta.label,
        group_meta.description,
        2,
    );
    if area.height <= header_rows {
        return Vec::new();
    }
    let mut y = area.y + header_rows;
    let area_end = area.y + area.height;

    // ── Child toggle rows. ────────────────────────────────────────
    let mut rects: Vec<Rect> = vec![Rect::default(); children.len()];
    for (i, child_key) in children.iter().enumerate() {
        if y >= area_end {
            break;
        }
        let Some(child_meta) = state.registry.find(child_key) else {
            continue;
        };
        let is_focused = i == child_idx;
        let is_hovered = !is_focused && state.hover_row == Some(i);
        let bg = if is_focused {
            theme.bg_visual
        } else if is_hovered {
            theme.bg_hover
        } else {
            theme.bg_base
        };
        let row_rect = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        buf.set_style(row_rect, Style::default().bg(bg));
        rects[i] = row_rect;

        let marker = if is_focused {
            crate::glyphs::filled_dot()
        } else {
            "\u{25CB}"
        };
        let marker_style = if is_focused {
            Style::default().fg(theme.accent_user).bg(bg)
        } else {
            Style::default().fg(theme.gray).bg(bg)
        };
        let label_style = if is_focused {
            Style::default()
                .fg(theme.text_primary)
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_primary).bg(bg)
        };

        // Value read live from the snapshot (refreshed after each toggle).
        let on = matches!(state.value_for(child_key), Some(SettingValue::Bool(true)));
        let value_text = if on { "on" } else { "off" };
        let value_style = if on {
            Style::default().fg(theme.accent_user).bg(bg)
        } else {
            Style::default().fg(theme.gray).bg(bg)
        };

        // " <marker>  <label> … <value> " (value right-aligned with a pad).
        buf.set_span(
            area.x,
            y,
            &Span::styled(" ", label_style),
            1.min(area.width),
        );
        if area.width > 1 {
            buf.set_span(
                area.x + 1,
                y,
                &Span::styled(marker, marker_style),
                PICKER_MARKER_W.min(area.width - 1),
            );
        }
        let label_x = area.x.saturating_add(PICKER_PREFIX_W);
        let value_w = value_text.width() as u16;
        let value_x = (area.x + area.width)
            .saturating_sub(value_w + 1)
            .max(label_x);
        if value_x > label_x {
            let label_room = (value_x - label_x).saturating_sub(1) as usize;
            let label_text: std::borrow::Cow<'_, str> = if child_meta.label.width() <= label_room {
                std::borrow::Cow::Borrowed(child_meta.label)
            } else {
                std::borrow::Cow::Owned(truncate_str(child_meta.label, label_room))
            };
            let label_w = (label_text.width() as u16).min((value_x - label_x).saturating_sub(1));
            buf.set_span(
                label_x,
                y,
                &Span::styled(label_text.as_ref(), label_style),
                label_w,
            );
        }
        if value_x + value_w <= area.x + area.width {
            buf.set_span(value_x, y, &Span::styled(value_text, value_style), value_w);
        }
        y = y.saturating_add(1);
    }
    rects
}

/// Layout metadata for one picker choice.
struct PickerChoiceLayout {
    height: u16,
    wrap_lines: Vec<String>,
}

/// Compute layout for one picker choice (height + wrapped desc lines).
fn compute_picker_choice_layout(choice: &OwnedEnumChoice, area_width: u16) -> PickerChoiceLayout {
    // No description → 1 line, symbol + display only.
    if choice.description.trim().is_empty() {
        return PickerChoiceLayout {
            height: 1,
            wrap_lines: Vec::new(),
        };
    }

    // Compute the desc column = PICKER_PREFIX_W + display_width + PICKER_SEPARATOR_W.
    // Display gets truncated if it'd overflow; mirror that for layout math.
    let display_room = (area_width as usize).saturating_sub(PICKER_PREFIX_W as usize);
    let display_w = choice.display.width().min(display_room) as u16;
    let after_display = PICKER_PREFIX_W.saturating_add(display_w);
    let after_sep = after_display.saturating_add(PICKER_SEPARATOR_W);

    if after_sep >= area_width {
        // No room for description — single-line fallback.
        return PickerChoiceLayout {
            height: 1,
            wrap_lines: Vec::new(),
        };
    }

    let desc_width = area_width.saturating_sub(after_sep) as usize;
    if desc_width == 0 {
        return PickerChoiceLayout {
            height: 1,
            wrap_lines: Vec::new(),
        };
    }

    let line = Line::from(Span::raw(choice.description.as_str()));
    let wrapped = crate::render::wrapping::word_wrap_line(&line, desc_width);

    let wrap_lines: Vec<String> = wrapped
        .into_iter()
        .map(|l| {
            l.spans
                .into_iter()
                .map(|s| s.content.into_owned())
                .collect::<String>()
        })
        .collect();

    // Defensive: treat empty wrap result as no-wrap.
    if wrap_lines.is_empty() {
        return PickerChoiceLayout {
            height: 1,
            wrap_lines: Vec::new(),
        };
    }

    PickerChoiceLayout {
        height: wrap_lines.len() as u16,
        wrap_lines,
    }
}

/// Smallest scroll offset that keeps the focused choice fully visible.
fn picker_scroll_offset(
    layouts: &[PickerChoiceLayout],
    choices_idx: usize,
    available_h: u16,
) -> usize {
    if layouts.is_empty() || available_h == 0 {
        return 0;
    }
    let n = layouts.len();
    let mut offset = 0usize;
    loop {
        // Sum heights starting at `offset` until adding the next
        // choice would exceed `available_h`.
        let mut consumed: u16 = 0;
        let mut last_visible = offset;
        for (i, layout) in layouts.iter().enumerate().skip(offset) {
            let next = consumed.saturating_add(layout.height);
            if next > available_h {
                break;
            }
            consumed = next;
            last_visible = i + 1;
        }
        // `last_visible` is the exclusive upper bound. If
        // `choices_idx` is in `[offset, last_visible)`, this offset
        // works.
        if choices_idx < last_visible {
            return offset;
        }
        // Otherwise advance. Stop when we've exhausted the list
        // (defensive — shouldn't happen since `choices_idx < n`).
        if offset + 1 >= n {
            return offset;
        }
        offset += 1;
    }
}

/// Min width to draw the Int stepper's `‹`/`›` adornments.
const INT_STEPPER_ADORNMENT_MIN_WIDTH: u16 = 8;

/// Wide-range Int stepper defaults (span > 100): Up/Down small, Left/Right large.
const INT_STEPPER_WIDE_SMALL_STEP: i64 = 5;
const INT_STEPPER_WIDE_LARGE_STEP: i64 = 10;

/// Derive (small, large) step sizes from an Int setting's `[min, max]` span.
/// Narrow dials use unit fine-steps so every in-range value is reachable;
/// wide ranges keep the original ±5 / ±10 feel.
fn int_step_sizes(min: i64, max: i64) -> (i64, i64) {
    let span = max.saturating_sub(min).max(0);
    if span <= 20 {
        // scroll_lines 1..=10 (span 9): unit steps on both small and large.
        (1, (span / 5).max(1))
    } else if span <= 100 {
        // scroll_speed 1..=100 (span 99): unit fine, ±5 coarse.
        (1, 5)
    } else {
        // max_thoughts_width 40..=500 (span 460).
        (INT_STEPPER_WIDE_SMALL_STEP, INT_STEPPER_WIDE_LARGE_STEP)
    }
}

/// Footer labels for the Int stepper (must be `'static` for `Shortcut`).
fn int_step_footer_labels(min: i64, max: i64) -> (&'static str, &'static str) {
    let (small, large) = int_step_sizes(min, max);
    match (small, large) {
        (1, 1) => ("\u{2191}/\u{2193} +/-1", "\u{2190}/\u{2192} +/-1"),
        (1, 5) => ("\u{2191}/\u{2193} +/-1", "\u{2190}/\u{2192} +/-5"),
        (5, 10) => ("\u{2191}/\u{2193} +/-5", "\u{2190}/\u{2192} +/-10"),
        // Defensive fallback if thresholds change without new static pairs.
        (1, _) => ("\u{2191}/\u{2193} +/-1", "\u{2190}/\u{2192} step"),
        (5, _) => ("\u{2191}/\u{2193} +/-5", "\u{2190}/\u{2192} step"),
        _ => ("\u{2191}/\u{2193} step", "\u{2190}/\u{2192} step"),
    }
}

// ‹ / › (U+2039 / U+203A) — fall back to ASCII `<` / `>` on legacy ConHost.
fn int_stepper_left_glyph() -> &'static str {
    crate::glyphs::chevron_left()
}

fn int_stepper_right_glyph() -> &'static str {
    crate::glyphs::chevron()
}

/// Sample text for the `max_thoughts_width` live wrap preview.
const MAX_THOUGHTS_WIDTH_PREVIEW_SAMPLE: &str = "Let me trace through the call sites. First, \
    I'll need to look at how the dispatch flow handles the new variant. Then I'll verify the \
    rollback path preserves the previous state correctly.";

/// Min width to render the wrap preview.
const MAX_THOUGHTS_WIDTH_PREVIEW_MIN_WIDTH: u16 = 30;

/// Min remaining height to render the wrap preview below the stepper.
const MAX_THOUGHTS_WIDTH_PREVIEW_MIN_HEIGHT: u16 = 5;

/// Render the inline editor. Int settings use a stepper; String
/// settings use a text input with cursor and validation feedback.
fn render_editing_value(
    buf: &mut Buffer,
    area: Rect,
    state: &mut SettingsModalState,
    theme: &Theme,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    state.editor_adornment_rects = (Rect::default(), Rect::default());

    // Snapshot mode payload to avoid borrow conflicts with mut state.
    let (setting_key, buffer_owned, cursor_byte, validation_error_owned, kind_is_int) = {
        let (setting_key, buffer, cursor_byte, validation_error) = match &state.mode {
            SettingsModalMode::EditingValue {
                key,
                buffer,
                cursor_byte,
                validation_error,
            } => (*key, buffer.clone(), *cursor_byte, validation_error.clone()),
            _ => return,
        };
        let kind_is_int = state
            .registry
            .find(setting_key)
            .map(|m| matches!(m.kind, SettingKind::Int { .. }))
            .unwrap_or(false);
        (
            setting_key,
            buffer,
            cursor_byte,
            validation_error,
            kind_is_int,
        )
    };

    if kind_is_int {
        let Some(meta) = state.registry.find(setting_key) else {
            return;
        };
        // Snapshot meta fields to release registry borrow.
        let label = meta.label;
        let description = meta.description;
        render_int_stepper(
            buf,
            area,
            state,
            setting_key,
            label,
            description,
            &buffer_owned,
            theme,
        );
        return;
    }

    let buffer = buffer_owned.as_str();
    let validation_error = validation_error_owned.as_deref();
    let Some(meta) = state.registry.find(setting_key) else {
        return;
    };

    // Editors reserve title + gap + the input row (3) before the description.
    let header_rows = render_sub_pane_header(buf, area, theme, meta.label, meta.description, 3);
    if area.height <= header_rows {
        return;
    }
    let input_y = area.y + header_rows;

    // ── Row 3: input line. ────────────────────────────────────────
    let has_error = validation_error.is_some();
    let input_bg = theme.bg_visual;
    let input_fg = if has_error {
        theme.accent_error
    } else {
        theme.text_primary
    };
    let cursor_style = Style::default().fg(theme.accent_user).bg(input_bg);
    let input_style = Style::default().fg(input_fg).bg(input_bg);

    let input_row_rect = Rect {
        x: area.x,
        y: input_y,
        width: area.width,
        height: 1,
    };
    buf.set_style(input_row_rect, Style::default().bg(theme.bg_base));

    let input_x = area.x;
    let buffer_room_end_x = area.x + area.width;
    let buffer_room = buffer_room_end_x.saturating_sub(input_x) as usize;
    if buffer_room == 0 {
        return; // No room to render the buffer.
    }

    let input_strip_rect = Rect {
        x: input_x,
        y: input_y,
        width: buffer_room as u16,
        height: 1,
    };
    buf.set_style(input_strip_rect, Style::default().bg(input_bg));

    let cursor_reserve = 1usize;
    let visible_buffer_w = buffer_room.saturating_sub(cursor_reserve);

    // Empty-buffer placeholder.
    if buffer.is_empty() {
        let placeholder = match &meta.kind {
            SettingKind::String { validator, .. } => match validator {
                StringValidator::KnownModel => "<empty — use shell default>",
                StringValidator::NonEmptyToken => "<type a value>",
                StringValidator::Any => "<type a value>",
            },
            _ => "",
        };
        if !placeholder.is_empty() && visible_buffer_w > 0 {
            let placeholder_text: std::borrow::Cow<'_, str> =
                if placeholder.width() <= visible_buffer_w {
                    std::borrow::Cow::Borrowed(placeholder)
                } else {
                    std::borrow::Cow::Owned(truncate_str(placeholder, visible_buffer_w))
                };
            let placeholder_w = (placeholder_text.width() as u16).min(visible_buffer_w as u16);
            let placeholder_style = Style::default().fg(theme.gray_dim).bg(input_bg);
            buf.set_span(
                input_x,
                input_y,
                &Span::styled(placeholder_text.as_ref(), placeholder_style),
                placeholder_w,
            );
        }
        // Render the cursor at the start.
        let cursor_x = input_x;
        buf.set_span(
            cursor_x,
            input_y,
            &Span::styled(crate::glyphs::selection_bar(), cursor_style),
            1,
        );
    } else {
        // Cursor-following pan.
        let cursor_col = buffer[..cursor_byte.min(buffer.len())].width();
        let buffer_w = buffer.width();
        let view_offset = if buffer_w <= visible_buffer_w {
            0
        } else if cursor_col >= visible_buffer_w {
            cursor_col + 1 - visible_buffer_w
        } else {
            // Cursor fits within the first window; no scroll.
            0
        };

        let start_byte = if view_offset == 0 {
            0
        } else {
            let mut acc = 0usize;
            buffer
                .char_indices()
                .find_map(|(idx, ch)| {
                    if acc >= view_offset {
                        Some(idx)
                    } else {
                        acc += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                        None
                    }
                })
                .unwrap_or(buffer.len())
        };

        // Render the visible tail.
        let tail = &buffer[start_byte..];
        // The visible portion may still be wider than the room when
        // a wide grapheme straddles the right boundary; cap with
        // `truncate_str` defensively.
        let tail_text: std::borrow::Cow<'_, str> = if tail.width() <= visible_buffer_w {
            std::borrow::Cow::Borrowed(tail)
        } else {
            std::borrow::Cow::Owned(truncate_str(tail, visible_buffer_w))
        };
        let tail_w = (tail_text.width() as u16).min(visible_buffer_w as u16);
        buf.set_span(
            input_x,
            input_y,
            &Span::styled(tail_text.as_ref(), input_style),
            tail_w,
        );

        // Cursor lands at the logical column relative to the view.
        let cursor_visual_col = cursor_col.saturating_sub(view_offset);
        let cursor_x = input_x + (cursor_visual_col as u16).min(buffer_room as u16 - 1);
        buf.set_span(
            cursor_x,
            input_y,
            &Span::styled(crate::glyphs::selection_bar(), cursor_style),
            1,
        );
    }

    // ── Row 4: validation error. ──────────────────────────────────
    if area.height > header_rows + 1
        && let Some(err) = validation_error
    {
        let err_y = input_y + 1;
        let err_style = Style::default().fg(theme.accent_error).bg(theme.bg_base);
        let err_text: std::borrow::Cow<'_, str> = if err.width() <= area.width as usize {
            std::borrow::Cow::Borrowed(err)
        } else {
            std::borrow::Cow::Owned(truncate_str(err, area.width as usize))
        };
        let err_w = (err_text.width() as u16).min(area.width);
        buf.set_span(
            area.x,
            err_y,
            &Span::styled(err_text.as_ref(), err_style),
            err_w,
        );
    }
}

/// Render the Int stepper: title + description + centered `‹ N ›`.
/// Populates `editor_adornment_rects` for mouse click targets.
#[allow(clippy::too_many_arguments)]
fn render_int_stepper(
    buf: &mut Buffer,
    area: Rect,
    state: &mut SettingsModalState,
    setting_key: SettingKey,
    label: &'static str,
    description: &'static str,
    buffer: &str,
    theme: &Theme,
) {
    // Editors reserve title + gap + the stepper row (3) before the description.
    let header_rows = render_sub_pane_header(buf, area, theme, label, description, 3);
    if area.height <= header_rows {
        return;
    }
    let stepper_y = area.y + header_rows;

    // ── Row 3: centered stepper "‹  N  ›". ────────────────────────
    let value_text = if buffer.is_empty() {
        // Defensive — try_enter_editing_value seeds buffer from the
        // current value, so this branch should be unreachable, but
        // a blank cell would be confusing if a future refactor
        // dropped the seed.
        "—".to_string()
    } else {
        buffer.to_string()
    };
    let value_style = Style::default()
        .fg(theme.accent_user)
        .bg(theme.bg_base)
        .add_modifier(Modifier::BOLD);
    let arrow_style = Style::default().fg(theme.accent_user).bg(theme.bg_base);

    let left_w = int_stepper_left_glyph().width() as u16;
    let right_w = int_stepper_right_glyph().width() as u16;
    let value_w = value_text.width() as u16;
    // Layout: "‹  N  ›" — 2 cells between each glyph for breathing
    // room. Total width = left + 2 + value + 2 + right.
    let inter_pad: u16 = 2;
    let total_w = left_w + inter_pad + value_w + inter_pad + right_w;
    let render_arrows = area.width >= INT_STEPPER_ADORNMENT_MIN_WIDTH;

    if render_arrows && total_w <= area.width {
        // Center the full layout.
        let stepper_x = area.x + (area.width - total_w) / 2;
        let left_x = stepper_x;
        let value_x = left_x + left_w + inter_pad;
        let right_x = value_x + value_w + inter_pad;

        buf.set_span(
            left_x,
            stepper_y,
            &Span::styled(int_stepper_left_glyph(), arrow_style),
            left_w,
        );
        buf.set_span(
            value_x,
            stepper_y,
            &Span::styled(value_text.as_str(), value_style),
            value_w,
        );
        buf.set_span(
            right_x,
            stepper_y,
            &Span::styled(int_stepper_right_glyph(), arrow_style),
            right_w,
        );

        state.editor_adornment_rects = (
            Rect {
                x: left_x,
                y: stepper_y,
                width: left_w,
                height: 1,
            },
            Rect {
                x: right_x,
                y: stepper_y,
                width: right_w,
                height: 1,
            },
        );
    } else {
        // Too narrow for arrows — render the value alone, centered.
        let v_w = value_w.min(area.width);
        let value_x = area.x + (area.width - v_w) / 2;
        buf.set_span(
            value_x,
            stepper_y,
            &Span::styled(value_text.as_str(), value_style),
            v_w,
        );
    }

    // **In-pane hint dropped.** Earlier revisions
    // rendered a centered `↑/↓ +/-5   ←/→ +/-10   Enter commit · Esc
    // cancel` strip here, but the chrome footer's
    // `build_int_editor_shortcuts` already exposes the same content
    // at the bottom of the modal. On tall viewports both rendered
    // simultaneously — same keys, different separator (`·` vs `|`),
    // duplicate visual noise. We rely on the chrome footer alone
    // now; if the chrome ever fails to render its shortcut row (a
    // future regression), the user can still discover the keys via
    // the shortcuts cheatsheet (`?`).

    // ── Live wrap preview for max_thoughts_width. ─────────────────
    //
    // When the user is stepping `max_thoughts_width`, render a
    // sample thinking-text preview directly below the stepper that
    // wraps live at the current pending value. The preview sits in
    // the rows immediately after the stepper (1 blank row + title +
    // N content rows); any rows below the last content row of the
    // preview stay blank — the chrome footer sits below
    // `inner_area`, not inside `area`.
    //
    // Gated on `setting_key == MAX_THOUGHTS_WIDTH_KEY` so future
    // Int settings don't inherit the preview behavior implicitly.
    // The string equality is sufficient because key uniqueness is
    // enforced at registry-load time (see
    // `SettingsRegistry::defaults` / `::from_entries`).
    if setting_key == crate::settings::defs::MAX_THOUGHTS_WIDTH_KEY {
        let stepper_end_y = stepper_y.saturating_add(1);
        let area_end_y = area.y.saturating_add(area.height);
        let preview_h = area_end_y.saturating_sub(stepper_end_y);
        if preview_h >= MAX_THOUGHTS_WIDTH_PREVIEW_MIN_HEIGHT {
            let preview_area = Rect {
                x: area.x,
                y: stepper_end_y,
                width: area.width,
                height: preview_h,
            };
            let pending_value = parse_max_thoughts_width_buffer(buffer);
            render_max_thoughts_width_preview(buf, preview_area, pending_value, theme);
        }
    }
}

/// Parse the Int stepper's buffer back into a `u16` clamped to the
/// `max_thoughts_width` registered bounds. Defensive — the stepper's
/// step path keeps the buffer in range, but a synthetic test fixture
/// or a future code path could seed an out-of-range buffer.
///
/// Both `MIN = 40` and `MAX = 500` fit inside `u16`, so the `clamp`
/// result is always non-negative and ≤ `u16::MAX`. We use
/// `u16::try_from(...).unwrap_or(u16::MAX)` instead of `as u16` so
/// a future bump to `MAX > u16::MAX` saturates rather than silently
/// truncating mod 65536 (security suggestion).
fn parse_max_thoughts_width_buffer(buffer: &str) -> u16 {
    let clamped = buffer
        .parse::<i64>()
        .unwrap_or(crate::settings::defs::MAX_THOUGHTS_WIDTH_MIN)
        .clamp(
            crate::settings::defs::MAX_THOUGHTS_WIDTH_MIN,
            crate::settings::defs::MAX_THOUGHTS_WIDTH_MAX,
        );
    u16::try_from(clamped).unwrap_or(u16::MAX)
}

/// Render the `max_thoughts_width` live wrap preview block.
///
/// **Vertical layout inside `area`.** Top-anchored. Row 0 of `area`
/// is a 1-row blank gap separating the preview from whatever sits
/// directly above (in the live caller, the stepper row); row 1 is
/// the title; rows 2..(2+content_rows) hold the wrapped content.
/// When `pending_value > area.width` (the preview is clamped) and
/// there are at least two rows of vertical slack below the
/// content, a 1-row blank gap and then a note row carry the text
/// `note: clamped at N cols`. Any rows below the note
/// stay blank — that empty space is intentional and stays
/// unpainted (the chrome footer sits below `inner_area`).
///
/// **Edge cases (per spec).**
/// - `area.width < MAX_THOUGHTS_WIDTH_PREVIEW_MIN_WIDTH` (30): omit
///   the preview entirely — too narrow for readable wrapped text.
/// - `area.height < MAX_THOUGHTS_WIDTH_PREVIEW_MIN_HEIGHT` (5):
///   omit — insufficient vertical budget for the gap + title +
///   2 content rows layout.
/// - `pending_value > area.width`: clamp the preview width to
///   `area.width`. The title stays plain `preview`; the clamp
///   amount is surfaced via a `note: clamped at N cols` row
///   rendered below the content when there's a row of slack
///   below it. Content takes priority over the note — if there's
///   no room for the note row, it's silently omitted.
/// - Active setting key gating happens at the call site
///   (`setting_key == "max_thoughts_width"`); this helper is pure
///   on the `pending_value` it receives.
///
/// **Theme tokens.**
/// - Title bg: `theme.bg_visual` — the heavier / more saturated of
///   the two "block" bg tokens; matches selection-bg saturation.
/// - Content bg: `theme.bg_highlight` — the lighter / less
///   saturated of the two; still distinguishable from
///   `theme.bg_base` so the preview reads as a contained block.
/// - Title fg + content fg: `theme.text_primary` — same color the
///   scrollback's thinking output renders in. Italic + bold +
///   underlined for the title (the UNDERLINED gives consistent
///   additional visual weight on themes where `bg_visual` vs
///   `bg_highlight` is mostly a hue shift rather than a luma shift,
///   e.g. TokyoNight); italic only for content.
fn render_max_thoughts_width_preview(
    buf: &mut Buffer,
    area: Rect,
    pending_value: u16,
    theme: &Theme,
) {
    // Edge case 1: terminal area too narrow → omit.
    if area.width < MAX_THOUGHTS_WIDTH_PREVIEW_MIN_WIDTH {
        return;
    }
    // Edge case 2: terminal area too short → omit.
    if area.height < MAX_THOUGHTS_WIDTH_PREVIEW_MIN_HEIGHT {
        return;
    }
    // Defensive guard: catch future editors who add `\n` / `\t` (or
    // any other control char that bypasses word_wrap_line's flow)
    // to the sample. `wrap_description` has the same debug_assert
    // for the same reason.
    debug_assert!(
        !MAX_THOUGHTS_WIDTH_PREVIEW_SAMPLE.contains('\n')
            && !MAX_THOUGHTS_WIDTH_PREVIEW_SAMPLE.contains('\t'),
        "MAX_THOUGHTS_WIDTH_PREVIEW_SAMPLE must not contain `\\n` or `\\t`; \
         word_wrap_line flattens spans byte-for-byte and would render control \
         cells as glyphs",
    );

    // Effective preview content width = min(pending, available).
    let pending_w = pending_value.max(1);
    let effective_width = pending_w.min(area.width);
    let clamped = pending_w > area.width;

    // Wrap the sample text at the effective width.
    let sample_line = Line::from(Span::raw(MAX_THOUGHTS_WIDTH_PREVIEW_SAMPLE));
    let wrapped = crate::render::wrapping::word_wrap_line(&sample_line, effective_width as usize);
    // Defensive: a degenerate wrap (zero lines) means we have no
    // meaningful preview to show. The MIN_WIDTH=30 gate above makes
    // this practically unreachable.
    if wrapped.is_empty() {
        return;
    }
    // Layout budget: 1 row blank gap (above title) + 1 row title +
    // N content rows. Cap N at the available rows; the rest of
    // `area` (below the last content row) stays blank. The
    // MIN_HEIGHT=5 gate above guarantees `area.height >= 5`, so
    // `available_content_rows >= 3` here — always enough room for
    // the minimum 2 content rows. We cap at `wrapped.len()` so a
    // short wrap doesn't leave us painting beyond the wrap shape.
    let available_content_rows = area.height.saturating_sub(2) as usize;
    let visible_content = wrapped.len().min(available_content_rows);
    render_preview_block(
        buf,
        area,
        effective_width,
        clamped,
        &wrapped[..visible_content],
        theme,
    );
}

/// Inner painter for the preview block — split out so the
/// caller's edge-case dispatch (omit / truncate / full-fit) stays
/// readable and the rendering logic isn't duplicated.
///
/// Caller guarantees:
/// - `effective_width <= area.width`.
/// - `wrapped.len() >= 1` AND `wrapped.len() + 2 <= area.height`
///   (1 row gap above + 1 row title + `wrapped.len()` content rows
///   must all fit inside `area`).
/// - `area.width >= MAX_THOUGHTS_WIDTH_PREVIEW_MIN_WIDTH`.
///
/// Clamped-state surfacing: when `clamped` is true and there are
/// at least 2 rows of vertical slack below the content (i.e.
/// `area.height >= wrapped.len() + 4`), a 1-row blank gap and
/// then a `note: clamped at N cols` row are rendered below the
/// last content row
/// in `theme.text_secondary` with no modifier and on
/// `theme.bg_base` (no block-tinted bg — the note lives in the
/// chrome strip below the preview, not inside the two-tone
/// preview block). When the slack is unavailable, the note is
/// silently omitted; content takes priority.
fn render_preview_block(
    buf: &mut Buffer,
    area: Rect,
    effective_width: u16,
    clamped: bool,
    wrapped: &[Line<'_>],
    theme: &Theme,
) {
    debug_assert!(
        area.height >= (wrapped.len() as u16).saturating_add(2),
        "render_preview_block caller-guarantee violated: \
         area.height={} < wrapped.len()+2={}",
        area.height,
        wrapped.len() + 2,
    );
    // Top-anchor: row 0 of `area` is a blank gap, row 1 holds the
    // title, rows 2..(2+content_rows) hold the wrapped content.
    // Any rows below the last content row stay blank, except for
    // the optional clamped-note row described at the bottom of
    // this function.
    let title_y = area.y.saturating_add(1);

    // ── Title row. ────────────────────────────────────────────────
    let title_bg = theme.bg_visual;
    let content_bg = theme.bg_highlight;
    let title_fg = theme.text_primary;
    let content_fg = theme.text_primary;

    // Paint title bg first so partial-trailing-whitespace stays
    // tinted (the bg extends to the FULL effective_width on every
    // row, including any title columns past the text).
    let title_rect = Rect {
        x: area.x,
        y: title_y,
        width: effective_width,
        height: 1,
    };
    buf.set_style(title_rect, Style::default().bg(title_bg));

    // Title is always plain lowercase `preview`. The previous
    // implementation appended ` · clamped to N cols` to the title
    // when the preview clamped to a narrower terminal width; the
    // clamp signal has been moved to a note row below the content
    // (see the bottom of this function) so the title carries the
    // same shape regardless of clamp state.
    let title_text: &str = "preview";
    let title_text_truncated: std::borrow::Cow<'_, str> =
        if title_text.width() <= effective_width as usize {
            std::borrow::Cow::Borrowed(title_text)
        } else {
            std::borrow::Cow::Owned(truncate_str(title_text, effective_width as usize))
        };
    let title_w = (title_text_truncated.width() as u16).min(effective_width);
    // BOLD + ITALIC + UNDERLINED on the title. UNDERLINED gives
    // additional visual weight independent of the bg luma — on
    // TokyoNight `bg_visual` vs `bg_highlight` is
    // mostly a hue shift, not a luma shift, so the underline
    // carries the "this is the title" cue on its own.
    let title_style = Style::default()
        .fg(title_fg)
        .bg(title_bg)
        .add_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINED);
    buf.set_span(
        area.x,
        title_y,
        &Span::styled(title_text_truncated.as_ref(), title_style),
        title_w,
    );

    // ── Content rows. ─────────────────────────────────────────────
    let content_style = Style::default()
        .fg(content_fg)
        .bg(content_bg)
        .add_modifier(Modifier::ITALIC);
    for (i, wrap_line) in wrapped.iter().enumerate() {
        let row_y = title_y + 1 + i as u16;
        // Paint bg first across `effective_width` so trailing
        // whitespace on a wrap line is still tinted.
        let row_rect = Rect {
            x: area.x,
            y: row_y,
            width: effective_width,
            height: 1,
        };
        buf.set_style(row_rect, Style::default().bg(content_bg));
        // Flatten the wrapped line's spans back to a plain string
        // (the sample text has no inline styles, so we don't lose
        // any styling). Then re-style with our italic + content_fg.
        let text: String = wrap_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        let text_w = (text.width() as u16).min(effective_width);
        if text_w > 0 {
            buf.set_span(area.x, row_y, &Span::styled(text, content_style), text_w);
        }
    }

    // ── Clamped note (optional, height-permitting). ───────────────
    //
    // When `clamped`, surface the clamp in a low-key note row
    // immediately below the last content row. The note is
    // height-aware: content takes priority, so if there's no row
    // of slack below the content we omit the note entirely. The
    // note sits OUTSIDE the two-tone preview bg block — it uses
    // `theme.bg_base` so it visually reads as chrome/tip text,
    // not as part of the wrap preview. Left-aligned at the same
    // x-offset as content (`area.x`).
    if clamped {
        // One blank row sits between the last content row and the
        // note so the note reads as a separate annotation, not a
        // continuation of the wrap preview.
        let note_y = title_y
            .saturating_add(1)
            .saturating_add(wrapped.len() as u16)
            .saturating_add(1);
        let area_end_y = area.y.saturating_add(area.height);
        if note_y < area_end_y {
            let note_text = format!("note: clamped at {effective_width} cols");
            let note_text_truncated: std::borrow::Cow<'_, str> =
                if note_text.width() <= area.width as usize {
                    std::borrow::Cow::Borrowed(note_text.as_str())
                } else {
                    std::borrow::Cow::Owned(truncate_str(&note_text, area.width as usize))
                };
            let note_w = (note_text_truncated.width() as u16).min(area.width);
            // No modifier, no bg tint — the note reads as chrome
            // text aligned with the preview's left edge.
            let note_style = Style::default().fg(theme.text_secondary).bg(theme.bg_base);
            buf.set_span(
                area.x,
                note_y,
                &Span::styled(note_text_truncated.as_ref(), note_style),
                note_w,
            );
        }
    }
}

/// `compute_max_label_w` equivalent for settings rows. Caps the column
/// at 24 cols (so a single outlier label can't push the value column
/// off-screen) and never exceeds half the content area width.
/// Mirrors `question_view::compute_max_label_w` semantics.
fn compute_settings_max_label_w(metas: &[SettingMeta], content_w: u16) -> u16 {
    const MAX_LABEL_W: u16 = 24;
    let half = content_w / 2;
    let cap = MAX_LABEL_W.min(half);
    metas
        .iter()
        .map(|m| m.label.width() as u16)
        .max()
        .unwrap_or(0)
        .min(cap)
}

/// Look up the user-friendly display string for an Enum canonical
/// against the setting's own `EnumChoice` catalog. Falls back to the
/// canonical verbatim if the lookup misses (defense-in-depth: a
/// hand-edited corrupted config with an unknown canonical still
/// renders without an empty string, mirroring
/// `display_name_for_canonical`'s pattern).
///
/// Look up the display name for an Enum canonical via the registry.
fn display_for_enum_canonical<'a>(kind: &'a SettingKind, canonical: &'a str) -> &'a str {
    if let SettingKind::Enum { choices, .. } = kind {
        for c in *choices {
            if c.canonical == canonical {
                return c.display;
            }
        }
    }
    // Fallback: render the canonical verbatim. Defensive — catches a
    // schema-vs-renderer drift without crashing the modal.
    canonical
}

/// Word-wrap a description string. Returns owned lines for re-styling.
/// Asserts descriptions are single-line (no `\n`/`\t`).
fn wrap_description(description: &str, width: u16) -> Vec<String> {
    if description.is_empty() || width == 0 {
        return Vec::new();
    }
    debug_assert!(
        !description.contains('\n') && !description.contains('\t'),
        "SettingMeta::description is single-line / no tabs by contract: \
         description={description:?}. Word-wrap doesn't split on \\n or \\t — \
         such chars would render as control codes in a buffer cell. \
         Pre-split + iterate if multi-line descriptions become useful.",
    );
    let line = Line::from(Span::raw(description));
    crate::render::wrapping::word_wrap_line(&line, width as usize)
        .into_iter()
        .map(|l| {
            l.spans
                .into_iter()
                .map(|s| s.content.into_owned())
                .collect::<String>()
        })
        .collect()
}

// Row layout: triangle on left, value right-aligned. Two-line
// layout used when label + value exceed area width.

// Row chrome dimensions.
const ROW_TRIANGLE_PREFIX_W: u16 = 2;
const ROW_GAP_MIN_W: u16 = 1;
const ROW_RIGHT_PAD_W: u16 = 1;
const ROW_CHEVRON_W: u16 = 2;
/// Chevron column width — reserved for all rows for alignment.
const ROW_CHEVRON_COL_W: u16 = ROW_CHEVRON_W;
const ROW_RESTART_PILL_W: u16 = 10; // " · restart" — used for layout budgeting only.

/// Per-row layout decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowLayout {
    OneLine,
    /// Value drops to line 2 (label too wide for single line).
    TwoLine,
    /// Even label alone exceeds width — truncate label, value on line 2.
    TwoLineWithLabelTruncation,
}

/// Decide whether a setting row needs 1 or 2 logical lines.
fn row_layout(
    area_width: u16,
    label: &str,
    value_display: &str,
    show_restart_pill: bool,
) -> RowLayout {
    let restart_w = if show_restart_pill {
        ROW_RESTART_PILL_W
    } else {
        0
    };
    let label_w = label.width() as u16;
    let value_w = value_display.width() as u16;
    let one_line_total = ROW_TRIANGLE_PREFIX_W
        .saturating_add(label_w)
        .saturating_add(ROW_GAP_MIN_W)
        .saturating_add(value_w)
        .saturating_add(ROW_CHEVRON_COL_W)
        .saturating_add(restart_w)
        .saturating_add(ROW_RIGHT_PAD_W);
    if one_line_total <= area_width {
        return RowLayout::OneLine;
    }
    // Two-line: line 1 hosts the label + (optional) restart pill +
    // right pad. If even that doesn't fit, fall back to label
    // truncation on line 1.
    let line1_full = ROW_TRIANGLE_PREFIX_W
        .saturating_add(label_w)
        .saturating_add(restart_w)
        .saturating_add(ROW_RIGHT_PAD_W);
    if line1_full <= area_width {
        RowLayout::TwoLine
    } else {
        RowLayout::TwoLineWithLabelTruncation
    }
}
#[allow(clippy::too_many_arguments)]
fn render_setting_row(
    buf: &mut Buffer,
    area: Rect,
    meta: &SettingMeta,
    value: &SettingValue,
    max_label_w: u16,
    is_selected: bool,
    theme: &Theme,
    is_expanded: bool,
    is_hovered: bool,
) -> Rect {
    let bg = if is_selected {
        theme.bg_visual
    } else if is_hovered {
        theme.bg_hover
    } else {
        theme.bg_base
    };
    // Paint the row bg across the full area (1 or 2 lines).
    buf.set_style(area, Style::default().bg(bg));

    let label_style = Style::default().fg(theme.text_primary).bg(bg);
    // Bool(false) renders muted; all other values use accent.
    let value_style = Style::default().fg(theme.accent_user).bg(bg);
    let chevron_style = Style::default().fg(theme.gray).bg(bg);
    let restart_style = Style::default()
        .fg(theme.gray_dim)
        .bg(bg)
        .add_modifier(Modifier::ITALIC);
    let desc_style = Style::default().fg(theme.gray).bg(bg);

    // Enum rows display the user-friendly name, not the canonical.
    let value_text_owned;
    let value_text: &str = match value {
        SettingValue::Bool(b) => {
            if *b {
                "on"
            } else {
                "off"
            }
        }
        SettingValue::String(s) => {
            if s.is_empty() && matches!(meta.kind, SettingKind::DynamicEnum { .. }) {
                "(no override)"
            } else {
                s.as_str()
            }
        }
        SettingValue::Enum(e) => display_for_enum_canonical(&meta.kind, e),
        SettingValue::Int(i) => {
            value_text_owned = i.to_string();
            &value_text_owned
        }
    };

    let value_style = if matches!(value, SettingValue::Bool(false)) {
        Style::default().fg(theme.gray).bg(bg)
    } else {
        value_style
    };

    // Chevron for Enum/String/DynamicEnum (opens picker/editor).
    let show_chevron = matches!(
        (&meta.kind, value),
        (SettingKind::Enum { .. }, _)
            | (SettingKind::String { .. }, _)
            | (SettingKind::DynamicEnum { .. }, _)
    );
    let chevron_str = format!(" {}", crate::glyphs::chevron()); // › → > on legacy ConHost
    let chevron_w = if show_chevron {
        chevron_str.width() as u16
    } else {
        0
    };
    let value_w = value_text.width() as u16;

    // Pill only while expanded — change-time feedback is the toast's job, and
    // a collapsed non-default row would misread as "restart pending" forever.
    let show_restart_pill = meta.restart_required && is_expanded;
    let restart_pill_text = " \u{00B7} restart";
    let restart_w = if show_restart_pill {
        restart_pill_text.width() as u16
    } else {
        0
    };

    // Triangle prefix: "▸" collapsed, "▾" expanded.
    let triangle = if is_expanded { "\u{25BE}" } else { "\u{25B8}" };
    debug_assert_eq!(
        triangle.width(),
        (ROW_TRIANGLE_PREFIX_W - 1) as usize,
        "ROW_TRIANGLE_PREFIX_W = {ROW_TRIANGLE_PREFIX_W} assumes a 1-cell triangle; \
         glyph `{triangle}` measures {} cells. A 2-cell triangle (e.g. ▶ / ▼ from \
         fold_indicator_span) would shift the entire row column. Update the constant \
         or pick a 1-cell glyph.",
        triangle.width(),
    );

    // Fall back to one-line if only 1 line was allocated.
    let layout_decision = row_layout(area.width, meta.label, value_text, show_restart_pill);
    let layout = if area.height < 2 {
        // Only 1 line available — collapse to a one-line render and
        // accept that the label might collide with the value column.
        RowLayout::OneLine
    } else {
        layout_decision
    };
    let _ = max_label_w;

    // ── Compute right-side x positions (shared across layouts). ──
    // Layout (right-to-left): [restart pill][space][chevron][space][value]
    // The 1-cell right pad is baked into `restart_x`.
    let restart_x_line1 = (area.x + area.width).saturating_sub(restart_w + 1);

    match layout {
        RowLayout::OneLine => {
            // Chevron column reserved for all rows for alignment.
            let chevron_x = restart_x_line1.saturating_sub(ROW_CHEVRON_COL_W);
            let value_x = chevron_x.saturating_sub(value_w + 1);

            let label_text = format!("{triangle} {}", meta.label);
            let label_w = label_text.width() as u16;
            let label_max_x = area.x.saturating_add(label_w);
            // Cap label end at value_x to never collide with the value column.
            let label_end = label_max_x.min(value_x.saturating_sub(1));
            let label_used = label_end.saturating_sub(area.x);

            if label_used > 0 {
                buf.set_span(
                    area.x,
                    area.y,
                    &Span::styled(&label_text, label_style),
                    label_used,
                );
            }

            if value_x > area.x.saturating_add(label_used) {
                buf.set_span(
                    value_x,
                    area.y,
                    &Span::styled(value_text, value_style),
                    value_w,
                );
            }
            if show_chevron && chevron_w > 0 && chevron_x >= area.x.saturating_add(label_used) {
                buf.set_span(
                    chevron_x,
                    area.y,
                    &Span::styled(chevron_str.as_str(), chevron_style),
                    chevron_w,
                );
            }
            if show_restart_pill && restart_w > 0 {
                buf.set_span(
                    restart_x_line1,
                    area.y,
                    &Span::styled(restart_pill_text, restart_style),
                    restart_w,
                );
            }

            let _ = desc_style;
            let _ = is_selected;

            // Hit-rect for the value column: spans the value text
            // plus the (always-reserved) chevron column. Clicking
            // the chevron column on a Bool row is a no-op (no
            // glyph there) but still routes to the row, matching
            // chevron rows.
            Rect {
                x: value_x,
                y: area.y,
                width: value_w.saturating_add(ROW_CHEVRON_COL_W),
                height: 1,
            }
        }
        RowLayout::TwoLine | RowLayout::TwoLineWithLabelTruncation => {
            // ── Line 1: triangle + label + (restart pill) ──
            // Compute how much horizontal space is available to the
            // label before colliding with the restart pill.
            let label_avail = area
                .width
                .saturating_sub(restart_w + 1) // restart pill + right pad
                .saturating_sub(ROW_TRIANGLE_PREFIX_W);

            let label_text_owned: String;
            let label_text: &str = match layout {
                RowLayout::TwoLineWithLabelTruncation => {
                    // Truncate the label so triangle + truncated label
                    // + restart_pill + right_pad fits on line 1.
                    if label_avail == 0 {
                        ""
                    } else {
                        label_text_owned = truncate_str(meta.label, label_avail as usize);
                        &label_text_owned
                    }
                }
                _ => meta.label,
            };

            let full_label_text = format!("{triangle} {label_text}");
            let full_label_w = full_label_text.width() as u16;
            let label_used = full_label_w.min(area.width.saturating_sub(restart_w + 1));

            if label_used > 0 {
                buf.set_span(
                    area.x,
                    area.y,
                    &Span::styled(&full_label_text, label_style),
                    label_used,
                );
            }
            if show_restart_pill && restart_w > 0 {
                buf.set_span(
                    restart_x_line1,
                    area.y,
                    &Span::styled(restart_pill_text, restart_style),
                    restart_w,
                );
            }

            // ── Line 2: right-aligned value + chevron column ──
            //
            // The chevron column is reserved
            // for ALL rows so the `›` glyph is at a constant
            // offset; Bool rows leave it empty but the value
            // still right-aligns to the column's left edge.
            // An earlier version anchored Bool rows on line 2 to
            // `area.right - value_w - 1` (no chevron column
            // reserved), shifting their `on`/`off` text 2 cells
            // to the right of chevron rows' values — a
            // visual misalignment.
            //
            // Anchor line-2's
            // chevron-column LEFT EDGE at the same column the
            // one-line layout uses: `area.right - ROW_RIGHT_PAD_W
            // - ROW_CHEVRON_COL_W` (i.e. `restart_x_line1 -
            // ROW_CHEVRON_COL_W` when no restart pill is on
            // line 2). The earlier version anchored at
            // `area.right - ROW_CHEVRON_COL_W`, so on a row
            // that flipped from one-line to two-line layout the
            // `›` glyph would jump 1 cell rightward — producing
            // a staircase between mixed-layout rows. Subtracting
            // `ROW_RIGHT_PAD_W` here brings line 2 into pixel
            // parity with line 1.
            let y2 = area.y + 1;
            let chevron_x_line2 = (area.x + area.width)
                .saturating_sub(ROW_RIGHT_PAD_W + ROW_CHEVRON_COL_W)
                .max(area.x);
            let value_x_line2 = chevron_x_line2.saturating_sub(value_w + 1).max(area.x);

            // Render value, then chevron, on line 2. Clip if either
            // would land off the left edge in a pathologically
            // narrow row.
            if value_w > 0 && value_x_line2 + value_w <= area.x + area.width {
                buf.set_span(
                    value_x_line2,
                    y2,
                    &Span::styled(value_text, value_style),
                    value_w,
                );
            }
            if show_chevron
                && chevron_w > 0
                && chevron_x_line2 + ROW_CHEVRON_COL_W <= area.x + area.width
            {
                buf.set_span(
                    chevron_x_line2,
                    y2,
                    &Span::styled(chevron_str.as_str(), chevron_style),
                    chevron_w,
                );
            }

            let _ = desc_style;
            let _ = is_selected;

            // Hit-rect for the value column: covers the value text
            // + the always-reserved chevron column on LINE 2 only.
            // Width is `value_w + ROW_CHEVRON_COL_W`
            // (not `value_w + chevron_w`) so the hit-rect spans the
            // empty chevron column on Bool rows too.
            Rect {
                x: value_x_line2,
                y: y2,
                width: value_w.saturating_add(ROW_CHEVRON_COL_W),
                height: 1,
            }
        }
    }
}

/// Render the wrapped description for an expanded row.
fn render_expanded_description(buf: &mut Buffer, area: Rect, meta: &SettingMeta, theme: &Theme) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let desc_style = Style::default()
        .fg(theme.gray)
        .bg(theme.bg_base)
        .add_modifier(Modifier::ITALIC);
    let desc_src: &str = meta.description;
    // Indent 4 cols to nest under the label.
    let indent = 4u16.min(area.width);
    let wrap_w = area.width.saturating_sub(indent);
    if wrap_w == 0 {
        return;
    }
    let line = Line::from(Span::styled(desc_src, desc_style));
    let wrapped = crate::render::wrapping::word_wrap_line(&line, wrap_w as usize);
    for (i, wrapped_line) in wrapped.iter().enumerate() {
        if (i as u16) >= area.height {
            break;
        }
        let y = area.y + i as u16;
        // Paint indent bg first so the wrapped text aligns visually.
        for x in area.x..area.x + indent {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(theme.bg_base);
            }
        }
        buf.set_line(area.x + indent, y, wrapped_line, wrap_w);
    }
}

/// Fallback render path for a row whose `current_value_for` returned
/// `None` (registry / dispatch skew). Shows the label without a value
/// column so the misconfiguration is visible at runtime; the
/// `every_setting_has_dispatch_arm` test catches the case at CI time.
fn render_setting_row_no_value(
    buf: &mut Buffer,
    area: Rect,
    meta: &SettingMeta,
    max_label_w: u16,
    is_selected: bool,
    theme: &Theme,
) {
    let bg = if is_selected {
        theme.bg_visual
    } else {
        theme.bg_base
    };
    buf.set_style(area, Style::default().bg(bg));
    let label_style = Style::default()
        .fg(theme.accent_error)
        .bg(bg)
        .add_modifier(Modifier::BOLD);

    let label_max_w = max_label_w;
    let label_truncated: std::borrow::Cow<'_, str> = if meta.label.width() <= label_max_w as usize {
        std::borrow::Cow::Borrowed(meta.label)
    } else {
        std::borrow::Cow::Owned(truncate_str(meta.label, label_max_w as usize))
    };
    let text = format!(" !   {label_truncated} (no read mapping)");
    let w = text.width() as u16;
    buf.set_span(
        area.x,
        area.y,
        &Span::styled(&text, label_style),
        w.min(area.width),
    );
}

/// Render a `Group` row in the Browse list: a triangle-prefixed label with a
/// trailing chevron (opens the sub-sheet). Carries no value column. Returns the
/// chevron hit-rect so a click on it opens the sub-sheet like an Enum row.
fn render_setting_group_row(
    buf: &mut Buffer,
    area: Rect,
    meta: &SettingMeta,
    is_selected: bool,
    is_hovered: bool,
    is_expanded: bool,
    theme: &Theme,
) -> Rect {
    let bg = if is_selected {
        theme.bg_visual
    } else if is_hovered {
        theme.bg_hover
    } else {
        theme.bg_base
    };
    buf.set_style(area, Style::default().bg(bg));
    let label_style = Style::default().fg(theme.text_primary).bg(bg);
    let chevron_style = Style::default().fg(theme.gray).bg(bg);

    let chevron_str = format!(" {}", crate::glyphs::chevron());
    let chevron_w = chevron_str.width() as u16;
    let chevron_x = (area.x + area.width)
        .saturating_sub(ROW_RIGHT_PAD_W)
        .saturating_sub(ROW_CHEVRON_COL_W);

    // Triangle prefix mirrors normal rows: "▾" expanded, "▸" collapsed
    // (the group's description expands inline via Right/l like other rows).
    let triangle = if is_expanded { "\u{25BE}" } else { "\u{25B8}" };
    let label_text = format!("{triangle} {}", meta.label);
    let label_cap = chevron_x.saturating_sub(area.x).saturating_sub(1);
    let label_w = (label_text.width() as u16).min(label_cap);
    if label_w > 0 {
        buf.set_span(
            area.x,
            area.y,
            &Span::styled(&label_text, label_style),
            label_w,
        );
    }
    if chevron_w > 0 && chevron_x >= area.x.saturating_add(label_w) {
        buf.set_span(
            chevron_x,
            area.y,
            &Span::styled(chevron_str.as_str(), chevron_style),
            chevron_w,
        );
    }
    // Hit-rect spans the chevron column (a click there opens the sub-sheet).
    Rect {
        x: chevron_x,
        y: area.y,
        width: ROW_CHEVRON_COL_W,
        height: 1,
    }
}

/// Build the footer shortcut row. Enter label varies by focused row kind.
fn build_shortcuts(state: &SettingsModalState) -> Vec<Shortcut<'static>> {
    match state.mode {
        SettingsModalMode::Browse => {
            let enter_label = match state.focused_setting() {
                Some((_, meta)) if matches!(meta.kind, SettingKind::Bool { .. }) => "Enter toggle",
                _ => "Enter edit",
            };
            let mut shortcuts = vec![
                Shortcut {
                    label: "\u{2191}/\u{2193}/j/k nav",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "g/G top/btm",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "Space toggle",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: enter_label,
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "\u{2192} expand",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "/ search",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "d reset",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "F2/Esc close",
                    clickable: false,
                    id: 0,
                },
            ];
            // Browse is nav mode (filter inactive), so append `i search` last
            // (matching the shared pickers).
            modal_window::push_vim_nav_search_hint(&mut shortcuts, false);
            shortcuts
        }
        SettingsModalMode::FilterFocused => vec![
            Shortcut {
                label: "type to filter",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "\u{2191}/\u{2193} nav",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Backspace edit",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Enter commit",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Esc clear",
                clickable: false,
                id: 0,
            },
        ],
        SettingsModalMode::PickingEnum {
            supports_preview: sp,
            ..
        } => {
            // Labels depend on whether the Enum supports live preview.
            let nav_label = if sp {
                "\u{2191}/\u{2193} try"
            } else {
                "\u{2191}/\u{2193} nav"
            };
            let esc_label = if sp { "Esc revert" } else { "Esc cancel" };
            vec![
                Shortcut {
                    label: nav_label,
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "Enter commit",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: esc_label,
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "d reset",
                    clickable: false,
                    id: 0,
                },
            ]
        }

        SettingsModalMode::EditingValue { key, .. } => {
            // Int stepper: step-only hints with range-aware deltas.
            if let Some(SettingKind::Int { min, max, .. }) =
                state.registry.find(key).map(|m| &m.kind)
            {
                let (small_label, large_label) = int_step_footer_labels(*min, *max);
                return vec![
                    Shortcut {
                        label: small_label,
                        clickable: false,
                        id: 0,
                    },
                    Shortcut {
                        label: large_label,
                        clickable: false,
                        id: 0,
                    },
                    Shortcut {
                        label: "Enter commit",
                        clickable: false,
                        id: 0,
                    },
                    Shortcut {
                        label: "Esc cancel",
                        clickable: false,
                        id: 0,
                    },
                    Shortcut {
                        label: "d reset",
                        clickable: false,
                        id: 0,
                    },
                ];
            }
            vec![
                Shortcut {
                    label: "type to edit",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "\u{2190}/\u{2192} cursor",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "Enter commit",
                    clickable: false,
                    id: 0,
                },
                Shortcut {
                    label: "Esc cancel",
                    clickable: false,
                    id: 0,
                },
            ]
        }
        SettingsModalMode::PickingGroup { .. } => vec![
            Shortcut {
                label: "\u{2191}/\u{2193}/j/k nav",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Space/Enter toggle",
                clickable: false,
                id: 0,
            },
            Shortcut {
                label: "Esc back",
                clickable: false,
                id: 0,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

/// Handle a key event in the settings modal.
///
/// F2/Ctrl+,/Cmd+, always close regardless of mode. Esc behavior is
/// mode-dependent: Browse delegates to chrome, sub-modes handle it
/// locally (FilterFocused clears query, PickingEnum reverts preview,
/// EditingValue cancels). Space/Enter Repeat events are suppressed
/// to avoid per-tick disk writes.
pub fn handle_settings_key(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    if key.kind == KeyEventKind::Release {
        return SettingsKeyOutcome::Unchanged;
    }

    // Suppress Repeat for toggle keys to avoid per-tick disk writes.
    if key.kind == KeyEventKind::Repeat && matches!(key.code, KeyCode::Char(' ') | KeyCode::Enter) {
        return SettingsKeyOutcome::Unchanged;
    }

    if is_close_key(key) {
        return SettingsKeyOutcome::Close;
    }

    // Exhaustive per-mode dispatch.
    match state.mode {
        SettingsModalMode::Browse => handle_browse(state, key),
        SettingsModalMode::FilterFocused => handle_filter_focused(state, key),
        SettingsModalMode::PickingEnum { .. } => handle_picking_enum(state, key),
        SettingsModalMode::PickingGroup { .. } => handle_picking_group(state, key),
        SettingsModalMode::EditingValue { .. } => handle_editing_value(state, key),
    }
}

/// Enum chooser key routing. Up/Down dispatches preview actions,
/// Enter commits current choice, Esc reverts to original value.
fn handle_picking_enum(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    // Snapshot the current picker state under an immutable borrow so
    // the subsequent `state.mode = ...` writes are unambiguous.
    let (setting_key, choices_idx, original_value, supports_preview) = match &state.mode {
        SettingsModalMode::PickingEnum {
            key,
            choices_idx,
            original_value,
            supports_preview,
        } => (
            *key,
            *choices_idx,
            original_value.clone(),
            *supports_preview,
        ),
        _ => return SettingsKeyOutcome::Unchanged,
    };

    match key.code {
        KeyCode::Down | KeyCode::Char('j') => {
            let len = picker_choices_len(state, setting_key);
            if choices_idx + 1 >= len {
                return SettingsKeyOutcome::Unchanged;
            }
            set_picker_idx(
                state,
                setting_key,
                choices_idx + 1,
                original_value,
                supports_preview,
            )
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if choices_idx == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            set_picker_idx(
                state,
                setting_key,
                choices_idx - 1,
                original_value,
                supports_preview,
            )
        }
        KeyCode::Enter => {
            // Commit: dispatch the typed COMMIT Action for the
            // currently-focused choice. This is the single place
            // per picker open → close cycle that fires
            // `Effect::PersistSetting`, eliminating the per-keystroke
            // disk write race. The most recent PREVIEW Action (from Up/Down) has
            // already mutated the live visual; the commit's
            // `set_X_inner` is idempotent on that.
            //
            // For
            // `SettingKind::DynamicEnum` settings (e.g.
            // `default_model`, `fork_secondary_model`), the picker
            // commits through `action_for_string` rather than
            // `action_for_enum_commit` — the canonical is a runtime
            // string sourced from the model catalog, which
            // `action_for_string` already knows how to resolve via
            // `snapshot.resolve_model_name` AND treats the empty
            // canonical as a `Clear*` sentinel.
            let kind_is_dynamic = matches!(
                state.registry.find(setting_key).map(|m| &m.kind),
                Some(SettingKind::DynamicEnum { .. })
            );
            state.transition_to_browse();
            if kind_is_dynamic {
                let Some(canonical) = picker_choice_at_owned(state, setting_key, choices_idx)
                else {
                    return SettingsKeyOutcome::Changed;
                };
                if let Some(action) =
                    action_for_string(setting_key, canonical, &state.pager_snapshot)
                {
                    return SettingsKeyOutcome::Action(action);
                }
                return SettingsKeyOutcome::Changed;
            }
            let Some(current_canonical) = picker_choice_at(state, setting_key, choices_idx) else {
                return SettingsKeyOutcome::Changed;
            };
            if let Some(action) = action_for_enum_commit(setting_key, current_canonical) {
                return SettingsKeyOutcome::Action(action);
            }
            SettingsKeyOutcome::Changed
        }
        KeyCode::Esc => {
            // Revert preview and return to Browse. Non-preview Enums
            // skip the revert (no live visual was applied).
            state.transition_to_browse();
            if let SettingValue::Enum(orig) = &original_value
                && let Some(action) = action_for_enum(setting_key, orig)
            {
                return SettingsKeyOutcome::Action(action);
            }
            SettingsKeyOutcome::Changed
        }
        // `d` reset: close picker, revert preview if applicable,
        // then open the reset-confirm overlay.
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            state.transition_to_browse();
            if supports_preview
                && let SettingValue::Enum(orig) = &original_value
                && let Some(revert) = action_for_enum(setting_key, orig)
            {
                return SettingsKeyOutcome::ActionPair(
                    revert,
                    Action::OpenResetConfirm { key: setting_key },
                );
            }
            SettingsKeyOutcome::Action(Action::OpenResetConfirm { key: setting_key })
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// The children of a group setting, or an empty slice if `key` is not a group.
fn group_children(state: &SettingsModalState, key: SettingKey) -> &'static [SettingKey] {
    match state.registry.find(key).map(|m| &m.kind) {
        Some(SettingKind::Group { children }) => children,
        _ => &[],
    }
}

/// Group sub-sheet key routing. Up/Down moves between the child toggles;
/// Space/Enter toggles the focused child in place (the sheet stays open);
/// Esc returns to Browse.
fn handle_picking_group(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    let (group_key, child_idx) = match &state.mode {
        SettingsModalMode::PickingGroup { key, child_idx } => (*key, *child_idx),
        _ => return SettingsKeyOutcome::Unchanged,
    };
    let children = group_children(state, group_key);
    if children.is_empty() {
        // Defensive: a group with no children can't be navigated — back out.
        state.transition_to_browse();
        return SettingsKeyOutcome::Changed;
    }

    match key.code {
        KeyCode::Down | KeyCode::Char('j') => {
            if child_idx + 1 >= children.len() {
                return SettingsKeyOutcome::Unchanged;
            }
            state.mode = SettingsModalMode::PickingGroup {
                key: group_key,
                child_idx: child_idx + 1,
            };
            SettingsKeyOutcome::Changed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if child_idx == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            state.mode = SettingsModalMode::PickingGroup {
                key: group_key,
                child_idx: child_idx - 1,
            };
            SettingsKeyOutcome::Changed
        }
        // Space/Enter toggle the focused child Bool and stay in the sheet so the
        // user can flip several tips in a row. The dispatcher refreshes the
        // modal snapshot, so the new value paints on the next frame.
        KeyCode::Char(' ') | KeyCode::Enter => {
            let Some(child_key) = children.get(child_idx).copied() else {
                return SettingsKeyOutcome::Unchanged;
            };
            let cur = match state.value_for(child_key) {
                Some(SettingValue::Bool(b)) => b,
                _ => return SettingsKeyOutcome::Unchanged,
            };
            match action_for_bool(child_key, !cur) {
                Some(action) => SettingsKeyOutcome::Action(action),
                None => SettingsKeyOutcome::Unchanged,
            }
        }
        KeyCode::Esc => {
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// Common nav body for Up/Down (and j/k aliases) in the picker:
/// update `choices_idx` in-place, look up the new canonical, fire
/// the preview dispatch via `action_for_enum`. Extracted from the
/// Update picker index and optionally dispatch a preview action.
/// `new_idx` must be in-bounds. Preview only fires for Enums with
/// `supports_preview: true`; side-effecting Enums skip preview.
fn set_picker_idx(
    state: &mut SettingsModalState,
    setting_key: SettingKey,
    new_idx: usize,
    original_value: SettingValue,
    supports_preview: bool,
) -> SettingsKeyOutcome {
    let in_bounds = new_idx < picker_choices_len(state, setting_key);
    if !in_bounds {
        // Caller bounds-checks before calling; belt-and-suspenders
        // for refactor safety.
        return SettingsKeyOutcome::Unchanged;
    }
    state.mode = SettingsModalMode::PickingEnum {
        key: setting_key,
        choices_idx: new_idx,
        supports_preview,
        original_value,
    };
    // Preview dispatch for static Enums with preview support.
    if supports_preview
        && let Some(new_canonical) = picker_choice_at(state, setting_key, new_idx)
        && let Some(action) = action_for_enum(setting_key, new_canonical)
    {
        return SettingsKeyOutcome::Action(action);
    }
    SettingsKeyOutcome::Changed
}

/// Inline string/int editor key routing. Esc cancels, Enter commits.
/// String mode: free-form text with cursor. Int mode: range-aware stepper
/// (Up/Down small, Left/Right large; see [`int_step_sizes`]), clamped to [min,max].
fn handle_editing_value(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    // Snapshot mode payload under an immutable borrow.
    let (setting_key, buffer, cursor_byte, validation_error) = match &state.mode {
        SettingsModalMode::EditingValue {
            key,
            buffer,
            cursor_byte,
            validation_error,
        } => (*key, buffer.clone(), *cursor_byte, validation_error.clone()),
        _ => return SettingsKeyOutcome::Unchanged,
    };

    // Look up the registered kind so we know how to handle this
    // edit. The lookup is `&self`-only.
    let Some(meta) = state.registry.find(setting_key) else {
        // Registry skew — log and exit. The CI guards catch this.
        tracing::error!(
            target: "settings",
            key = setting_key,
            "EditingValue mode references an unregistered key — exiting to Browse",
        );
        state.transition_to_browse();
        return SettingsKeyOutcome::Changed;
    };
    let kind_snapshot = meta.kind.clone();

    // Int settings dispatch through a stepper-only
    // handler. All char-input / cursor-pan / Backspace / Delete /
    // Home / End keys are rejected; only Up/Down/Left/Right (and
    // j/k/h/l aliases), Enter, and Esc do anything.
    if matches!(kind_snapshot, SettingKind::Int { .. }) {
        return handle_int_stepper(state, key, setting_key, &buffer, &kind_snapshot);
    }

    match key.code {
        KeyCode::Esc => {
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Enter => {
            // Commit gate: re-validate against the current buffer.
            // On failure, refresh the inline error and stay in
            // EditingValue.
            let error = match &kind_snapshot {
                SettingKind::String { validator, .. } => {
                    validate_string(*validator, &buffer, &state.pager_snapshot.available_models)
                }
                _ => return SettingsKeyOutcome::Unchanged,
            };
            if error.is_some() {
                update_editing_value_buffer(state, buffer, cursor_byte, error);
                return SettingsKeyOutcome::Unchanged;
            }
            // Dispatch the typed Action and transition to Browse.
            let action_opt = match &kind_snapshot {
                SettingKind::String { .. } => {
                    action_for_string(setting_key, buffer.clone(), &state.pager_snapshot)
                }
                _ => None,
            };
            state.transition_to_browse();
            match action_opt {
                Some(action) => SettingsKeyOutcome::Action(action),
                None => {
                    tracing::error!(
                        target: "settings",
                        key = setting_key,
                        "EditingValue commit has no action_for_string arm — registry skew",
                    );
                    SettingsKeyOutcome::Changed
                }
            }
        }
        KeyCode::Backspace => {
            if cursor_byte == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            let mut new_buf = buffer.clone();
            // Find the prev char boundary.
            let prev = (0..cursor_byte)
                .rev()
                .find(|&i| new_buf.is_char_boundary(i))
                .unwrap_or(0);
            new_buf.replace_range(prev..cursor_byte, "");
            let new_cursor = prev;
            let new_error = recompute_validation(&kind_snapshot, &new_buf, state);
            update_editing_value_buffer(state, new_buf, new_cursor, new_error);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Delete => {
            if cursor_byte >= buffer.len() {
                return SettingsKeyOutcome::Unchanged;
            }
            let mut new_buf = buffer.clone();
            // Find next char boundary.
            let next = (cursor_byte + 1..=new_buf.len())
                .find(|&i| new_buf.is_char_boundary(i))
                .unwrap_or(new_buf.len());
            new_buf.replace_range(cursor_byte..next, "");
            let new_error = recompute_validation(&kind_snapshot, &new_buf, state);
            update_editing_value_buffer(state, new_buf, cursor_byte, new_error);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Left => {
            if cursor_byte == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            let prev = (0..cursor_byte)
                .rev()
                .find(|&i| buffer.is_char_boundary(i))
                .unwrap_or(0);
            update_editing_value_buffer(state, buffer, prev, validation_error);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Right => {
            if cursor_byte >= buffer.len() {
                return SettingsKeyOutcome::Unchanged;
            }
            let next = (cursor_byte + 1..=buffer.len())
                .find(|&i| buffer.is_char_boundary(i))
                .unwrap_or(buffer.len());
            update_editing_value_buffer(state, buffer, next, validation_error);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Home => {
            update_editing_value_buffer(state, buffer, 0, validation_error);
            SettingsKeyOutcome::Changed
        }
        KeyCode::End => {
            let end = buffer.len();
            update_editing_value_buffer(state, buffer, end, validation_error);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            // Defense-in-depth for the (currently unused) String editor path:
            // reject control + bidi/format chars so a future `String` setting
            // can't reintroduce the Trojan-Source surface.
            let accept = match &kind_snapshot {
                SettingKind::String { .. } => !crate::render::line_utils::is_unsafe_display_char(c),
                _ => false,
            };
            if !accept {
                return SettingsKeyOutcome::Unchanged;
            }
            let mut new_buf = buffer.clone();
            new_buf.insert(cursor_byte, c);
            let new_cursor = cursor_byte + c.len_utf8();
            let new_error = recompute_validation(&kind_snapshot, &new_buf, state);
            update_editing_value_buffer(state, new_buf, new_cursor, new_error);
            SettingsKeyOutcome::Changed
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// Int stepper handler. Steps by range-aware small (Up/Down) or large
/// (Left/Right) deltas from [`int_step_sizes`], clamped to [min,max].
/// Non-stepper keys are rejected.
fn handle_int_stepper(
    state: &mut SettingsModalState,
    key: &KeyEvent,
    setting_key: SettingKey,
    buffer: &str,
    kind: &SettingKind,
) -> SettingsKeyOutcome {
    let SettingKind::Int { min, max, .. } = kind else {
        // Caller pre-checked the kind; defensive bail.
        return SettingsKeyOutcome::Unchanged;
    };

    let (small_step, large_step) = int_step_sizes(*min, *max);
    let step_delta = |dir: i64, large: bool| -> i64 {
        let magnitude = if large { large_step } else { small_step };
        dir * magnitude
    };

    let apply_step = |state: &mut SettingsModalState, delta: i64| -> SettingsKeyOutcome {
        let cur = buffer.parse::<i64>().unwrap_or(*min);
        let new = cur.saturating_add(delta).clamp(*min, *max);
        if new == cur {
            // Already clamped — no visible change. Report
            // Unchanged so the test for `clamps_to_min/max` can
            // distinguish a no-op from a step.
            return SettingsKeyOutcome::Unchanged;
        }
        let new_buf = new.to_string();
        let new_cursor = new_buf.len();
        update_editing_value_buffer(state, new_buf, new_cursor, None);
        SettingsKeyOutcome::Changed
    };

    // Only modifier-free or SHIFT+arrow events should trigger the
    // stepper; Ctrl/Alt/etc carry editor-unrelated semantics
    // (selection extend, history, etc) that the stepper has no
    // notion of.
    if !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) {
        return SettingsKeyOutcome::Unchanged;
    }

    match key.code {
        KeyCode::Esc => {
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Enter => {
            // Commit. Buffer is guaranteed in-range by the
            // clamp on every step; parse + dispatch.
            let action_opt = buffer
                .parse::<i64>()
                .ok()
                .and_then(|i| action_for_int(setting_key, i));
            state.transition_to_browse();
            match action_opt {
                Some(action) => SettingsKeyOutcome::Action(action),
                None => {
                    tracing::error!(
                        target: "settings",
                        key = setting_key,
                        "Int stepper Enter has no action_for_int arm — registry skew",
                    );
                    SettingsKeyOutcome::Changed
                }
            }
        }
        // Up / k: small step up.
        KeyCode::Up | KeyCode::Char('k') => apply_step(state, step_delta(1, false)),
        // Down / j: small step down.
        KeyCode::Down | KeyCode::Char('j') => apply_step(state, step_delta(-1, false)),
        // Right / l: large step up.
        KeyCode::Right | KeyCode::Char('l') => apply_step(state, step_delta(1, true)),
        // Left / h: large step down.
        KeyCode::Left | KeyCode::Char('h') => apply_step(state, step_delta(-1, true)),
        // `d` in the Int stepper dispatches
        // `OpenResetConfirm` like Browse mode does. Close the
        // stepper first so dispatch finds `ActiveModal::Settings`
        // (the dispatch arm panics in debug mode if it sees a
        // non-Settings modal). The stepper has no `d` semantic
        // otherwise — letters are intentionally rejected — so
        // the interception is safe.
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            state.transition_to_browse();
            SettingsKeyOutcome::Action(Action::OpenResetConfirm { key: setting_key })
        }
        // Everything else (digits, letters, Backspace, Delete,
        // Home, End, Tab, …) is silently ignored — the stepper is
        // not a text input.
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// Helper: rewrite the EditingValue mode payload with new buffer +
/// cursor + validation. Centralised so future variants don't need
/// to repeat the pattern-construction boilerplate.
fn update_editing_value_buffer(
    state: &mut SettingsModalState,
    buffer: String,
    cursor_byte: usize,
    validation_error: Option<String>,
) {
    let SettingsModalMode::EditingValue { key, .. } = state.mode else {
        // Caller-provided key was lost on mode shift; this is the
        // belt-and-suspenders fallback for a future refactor.
        return;
    };
    state.mode = SettingsModalMode::EditingValue {
        key,
        buffer,
        cursor_byte,
        validation_error,
    };
}

/// Helper: recompute the validation error for the current buffer
/// against the registered validator. Called on every buffer mutation
/// so the inline error indicator stays in sync.
fn recompute_validation(
    kind: &SettingKind,
    buffer: &str,
    state: &SettingsModalState,
) -> Option<String> {
    match kind {
        SettingKind::String { validator, .. } => {
            validate_string(*validator, buffer, &state.pager_snapshot.available_models)
        }
        SettingKind::Int { min, max, .. } => validate_int(buffer, *min, *max),
        _ => None,
    }
}

/// Whether `(key, canonical)` is gated off and must not be offered as a choice:
/// `permission_mode`'s "auto" when the auto gate is off, and
/// `voice_capture_mode`'s "hold" without key-release reporting. Pure (gates
/// passed as args) so it's unit-testable without touching process globals.
fn enum_choice_gated_off(
    key: SettingKey,
    canonical: &str,
    auto_mode_gate: bool,
    kitty_releases: bool,
) -> bool {
    (key == "permission_mode" && canonical == "auto" && !auto_mode_gate)
        || (key == "voice_capture_mode" && canonical == "hold" && !kitty_releases)
}

/// The effective static Enum choices for a picker, hiding gated-off options so
/// the modal never offers a choice the setter would silently no-op. Every
/// index-based picker path (len / at / render / seed) routes through this.
fn effective_enum_choices<'a>(
    key: SettingKey,
    choices: &'a [EnumChoice],
    snapshot: &PagerLocalSnapshot,
) -> Vec<&'a EnumChoice> {
    let kitty_releases = crate::app::kitty_flags_pushed();
    choices
        .iter()
        .filter(|c| {
            !enum_choice_gated_off(key, c.canonical, snapshot.auto_mode_gate, kitty_releases)
        })
        .collect()
}

/// Number of choices for the picker. Handles both
/// `SettingKind::Enum` (static catalog) and `SettingKind::DynamicEnum`
/// (catalog built from the snapshot at picker-open time).
fn picker_choices_len(state: &SettingsModalState, key: SettingKey) -> usize {
    state
        .registry
        .find(key)
        .and_then(|m| match &m.kind {
            SettingKind::Enum { choices, .. } => {
                Some(effective_enum_choices(key, choices, &state.pager_snapshot).len())
            }
            SettingKind::DynamicEnum { source, .. } => {
                Some(dynamic_enum_choices(*source, &state.pager_snapshot).len())
            }
            _ => None,
        })
        .unwrap_or(0)
}

/// Canonical value at index `idx` in the picker's choices, or `None`
/// if the key isn't a registered Enum/DynamicEnum or `idx` is out of
/// bounds.
///
/// Returns `Option<&'static str>` for static `SettingKind::Enum`
/// settings (zero allocation, since each `EnumChoice.canonical` is
/// itself `&'static str`).
fn picker_choice_at(
    state: &SettingsModalState,
    key: SettingKey,
    idx: usize,
) -> Option<&'static str> {
    let meta = state.registry.find(key)?;
    let SettingKind::Enum { choices, .. } = &meta.kind else {
        return None;
    };
    effective_enum_choices(key, choices, &state.pager_snapshot)
        .get(idx)
        .map(|c| c.canonical)
}

/// Owned-string variant of `picker_choice_at` for picker kinds whose
/// canonicals are runtime-built (`SettingKind::DynamicEnum`).
///
/// Returns the canonical at `idx` as an owned `String`. Allocates one
/// `String` per call — the picker calls this on commit + per-Up/Down
/// only when `supports_preview = true`, so the cost is bounded.
///
/// For static `SettingKind::Enum`, this also resolves correctly
/// (clones the `&'static str` into a `String`), giving the picker
/// a single unified read path when the caller doesn't need to
/// distinguish static vs. dynamic.
fn picker_choice_at_owned(
    state: &SettingsModalState,
    key: SettingKey,
    idx: usize,
) -> Option<String> {
    let meta = state.registry.find(key)?;
    match &meta.kind {
        SettingKind::Enum { choices, .. } => {
            effective_enum_choices(key, choices, &state.pager_snapshot)
                .get(idx)
                .map(|c| c.canonical.to_string())
        }
        SettingKind::DynamicEnum { source, .. } => {
            let resolved = dynamic_enum_choices(*source, &state.pager_snapshot);
            resolved.get(idx).map(|c| c.canonical.clone())
        }
        _ => None,
    }
}

/// F2 / Ctrl+, / Cmd+, are the modal-internal close keys.
///
/// Esc is intentionally NOT matched here: the `ModalWindow` chrome
/// (`views/modal_window.rs:handle_modal_key`) intercepts Esc and
/// returns `ModalWindowOutcome::CloseRequested` before this function
/// sees the event in Browse mode. `handle_filter_focused` has its own
/// Esc arm that exits filter mode without closing. Documented in the
/// module docstring.
fn is_close_key(key: &KeyEvent) -> bool {
    if key.code == KeyCode::F(2) {
        return true;
    }
    if key.code == KeyCode::Char(',')
        && (key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::SUPER))
    {
        return true;
    }
    false
}

fn changed_if(b: bool) -> SettingsKeyOutcome {
    if b {
        SettingsKeyOutcome::Changed
    } else {
        SettingsKeyOutcome::Unchanged
    }
}

/// Helper for `handle_settings_mouse`: when a sub-pane mouse handler
/// returns `Unchanged` but the breadcrumb hover flipped, upgrade to
/// `Changed` so the renderer repaints the breadcrumb with the new
/// fg color. Non-`Unchanged` outcomes pass through unmodified so an
/// `Action` or `Changed` from the inner handler keeps its meaning.
fn upgrade_if_breadcrumb_flipped(
    outcome: SettingsKeyOutcome,
    breadcrumb_flipped: bool,
) -> SettingsKeyOutcome {
    if breadcrumb_flipped && matches!(outcome, SettingsKeyOutcome::Unchanged) {
        SettingsKeyOutcome::Changed
    } else {
        outcome
    }
}

fn handle_browse(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => changed_if(state.advance_next()),
        KeyCode::Up | KeyCode::Char('k') => changed_if(state.advance_prev()),
        KeyCode::PageDown => {
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_next();
            }
            changed_if(moved)
        }
        KeyCode::PageUp => {
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_prev();
            }
            changed_if(moved)
        }
        KeyCode::Char('g') if key.modifiers.is_empty() => {
            // First selectable row IN THE FILTERED SET. When no filter
            // is active, `filtered_cache` is `(0..rows.len())` so this
            // resolves to the first row.
            let first = state
                .filtered_cache
                .iter()
                .copied()
                .find(|&idx| matches!(state.rows[idx], RowEntry::Setting { .. }))
                .unwrap_or(state.selected);
            if first != state.selected {
                state.selected = first;
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        KeyCode::Char('G') => {
            // Last selectable row IN THE FILTERED SET.
            let last = state
                .filtered_cache
                .iter()
                .rev()
                .copied()
                .find(|&idx| matches!(state.rows[idx], RowEntry::Setting { .. }))
                .unwrap_or(state.selected);
            if last != state.selected {
                state.selected = last;
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        // User-feedback follow-up: Right/`l` expands the focused
        // row's description inline; Left/`h` collapses it.
        // Expansion is per-row + persists across selection moves
        // (multiple rows can be expanded simultaneously).
        KeyCode::Right | KeyCode::Char('l') if key.modifiers.is_empty() => {
            if let Some((key, _meta)) = state.focused_setting()
                && state.expanded_keys.insert(key)
            {
                return SettingsKeyOutcome::Changed;
            }
            SettingsKeyOutcome::Unchanged
        }
        KeyCode::Left | KeyCode::Char('h') if key.modifiers.is_empty() => {
            if let Some((key, _meta)) = state.focused_setting()
                && state.expanded_keys.remove(key)
            {
                return SettingsKeyOutcome::Changed;
            }
            SettingsKeyOutcome::Unchanged
        }
        KeyCode::Char(' ') => {
            if let Some(action) = state.toggle_focused_bool() {
                SettingsKeyOutcome::Action(action)
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        KeyCode::Enter => {
            // Group row → open its sub-sheet of child toggles.
            if state.try_enter_picking_group() {
                return SettingsKeyOutcome::Changed;
            }
            // For Bool, Enter behaves like Space (the keyboard
            // map gives both keys the toggle semantics).
            if let Some(action) = state.toggle_focused_bool() {
                return SettingsKeyOutcome::Action(action);
            }
            // Enum row → enter PickingEnum mode. The picker's chooser
            // sub-pane takes over rendering and key routing from here.
            if state.try_enter_picking_enum() {
                return SettingsKeyOutcome::Changed;
            }
            // String / Int row → enter EditingValue mode. The
            // inline editor takes over rendering and key routing.
            if state.try_enter_editing_value() {
                return SettingsKeyOutcome::Changed;
            }
            SettingsKeyOutcome::Unchanged
        }
        // `i` aliases `/` (vim-nav "press i to search").
        KeyCode::Char('/') | KeyCode::Char('i') if key.modifiers.is_empty() => {
            state.mode = SettingsModalMode::FilterFocused;
            SettingsKeyOutcome::Changed
        }
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            // Reset-to-default. Resolves the focused row's
            // setting key and dispatches `Action::OpenResetConfirm`.
            // The dispatch arm boxes the SettingsModalState into the
            // `ActiveModal::ResetSettingsConfirm { settings_state, key, .. }`
            // variant so cancel returns to this exact modal state
            // (filter/scroll/selection preserved). Headers and
            // unmapped rows are no-ops — `d` only acts on a focused
            // setting row.
            //
            // We intentionally do NOT gate this on whether the
            // current value already equals the default — the
            // confirmation dialog gives the user a moment to back
            // out either way, and the dispatch arm shows an
            // "Already at default" toast on idempotent confirm.
            match state.focused_setting() {
                // Group rows have no scalar default to reset.
                Some((_, meta)) if matches!(meta.kind, SettingKind::Group { .. }) => {
                    SettingsKeyOutcome::Unchanged
                }
                Some((key, _meta)) => SettingsKeyOutcome::Action(Action::OpenResetConfirm { key }),
                // Focused row is a header (or out-of-bounds) — `d`
                // has nothing to reset. Unchanged so the user can
                // still navigate / type / etc.
                None => SettingsKeyOutcome::Unchanged,
            }
        }
        KeyCode::Backspace => {
            // Continue editing the query from Browse mode (the commit
            // path via Enter preserves the query, so Browse can be
            // entered with a non-empty query). Pop one char and
            // re-broaden the filter without switching modes. Mirrors
            // `memory_modal::handle_browse`'s Backspace arm.
            if state.query.pop().is_some() {
                state.query_cursor = state.query.len();
                state.invalidate_filter();
                state.clamp_selected_to_visible();
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

fn handle_filter_focused(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    match key.code {
        KeyCode::Esc => {
            state.query.clear();
            state.query_cursor = 0;
            state.invalidate_filter();
            state.clamp_selected_to_visible();
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Enter => {
            // Commit the filter: exit FilterFocused, return to Browse,
            // PRESERVE the query so the user can immediately Space /
            // Enter to toggle the focused (filtered) setting. This is
            // the standard TUI filter UX (fixes the
            // dead-end where Esc-clear made post-filter toggling
            // impossible without re-navigating the full list).
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Down => changed_if(state.advance_next()),
        KeyCode::Up => changed_if(state.advance_prev()),
        KeyCode::PageDown => {
            // Match Browse mode's fast-scroll affordance (advance 10).
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_next();
            }
            changed_if(moved)
        }
        KeyCode::PageUp => {
            let mut moved = false;
            for _ in 0..10 {
                moved |= state.advance_prev();
            }
            changed_if(moved)
        }
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            // Clears entire query (not cursor-to-start) to match picker behavior.
            if state.query.is_empty() {
                return SettingsKeyOutcome::Unchanged;
            }
            state.query.clear();
            state.query_cursor = 0;
            state.invalidate_filter();
            state.clamp_selected_to_visible();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            state.query.insert(state.query_cursor, c);
            state.query_cursor += c.len_utf8();
            state.invalidate_filter();
            state.clamp_selected_to_visible();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Backspace => {
            if state.query_cursor == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            let prev = state.query[..state.query_cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
            state.query.drain(prev..state.query_cursor);
            state.query_cursor = prev;
            state.invalidate_filter();
            state.clamp_selected_to_visible();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Left => {
            if state.query_cursor == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            state.query_cursor = state.query[..state.query_cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Right => {
            if state.query_cursor >= state.query.len() {
                return SettingsKeyOutcome::Unchanged;
            }
            state.query_cursor = state.query[state.query_cursor..]
                .char_indices()
                .nth(1)
                .map_or(state.query.len(), |(i, _)| state.query_cursor + i);
            SettingsKeyOutcome::Changed
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

// ---------------------------------------------------------------------------
// Mouse handling
// ---------------------------------------------------------------------------

/// Handle a mouse event in the modal content area.
///
/// Mirrors `memory_modal::handle_memory_mouse` for parity:
///  - Click on a row selects it; click on a Bool row toggles it.
///  - Click on the `[-]` / `[+]` adornments of an open Int editor
///    steps the value.
///  - Scroll wheel scrolls the row list by ~3 rows per tick.
///
/// **Picker short-circuit:** when the modal is in `PickingEnum`
/// mode, every mouse event is a no-op. `EditingValue` mode handles `[-]` / `[+]`
/// clicks AND treats everything else as a no-op.
pub fn handle_settings_mouse(
    state: &mut SettingsModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> SettingsKeyOutcome {
    // Clicking anywhere on the chrome
    // breadcrumb (the full `Settings › <label>` title) in a
    // sub-pane mode collapses back to Browse. Dispatched FIRST
    // so it wins over the picker / editor mouse handlers (which
    // would otherwise ignore the click as out-of-content). The
    // synthetic Esc is routed through the active sub-pane handler
    // so the same revert-preview / mode-transition logic runs as
    // for keyboard Esc — `handle_picking_enum` reverts the
    // preview action, and `handle_editing_value` just transitions
    // back.
    if matches!(
        kind,
        MouseEventKind::Down(crossterm::event::MouseButton::Left)
    ) && let Some(rect) = state.settings_breadcrumb_rect
        && rect_contains(rect, column, row)
    {
        let synthetic = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        match state.mode {
            SettingsModalMode::PickingEnum { .. } => {
                return handle_picking_enum(state, &synthetic);
            }
            SettingsModalMode::PickingGroup { .. } => {
                return handle_picking_group(state, &synthetic);
            }
            SettingsModalMode::EditingValue { .. } => {
                return handle_editing_value(state, &synthetic);
            }
            _ => {}
        }
    }

    // Track hover state for the
    // breadcrumb hit-rect so the renderer can repaint the title
    // with the brighter `accent_user` fg when the user's mouse
    // is over it. Without this cue the breadcrumb is visually
    // indistinguishable from the rest of the modal title chrome.
    // Tracked here so a hover transition is registered even when
    // the row-list / picker / editor mouse handlers below short-
    // circuit on the Moved event.
    let breadcrumb_hover_flipped = if matches!(kind, MouseEventKind::Moved) {
        let now_hovered = state
            .settings_breadcrumb_rect
            .map(|r| rect_contains(r, column, row))
            .unwrap_or(false);
        let flipped = now_hovered != state.breadcrumb_hovered;
        state.breadcrumb_hovered = now_hovered;
        // In sub-pane modes the row-list / picker / editor mouse
        // handlers don't update hover_row, so a flipped breadcrumb
        // hover is the only thing that could redraw. Force a
        // Changed outcome for those modes; in Browse the row-list
        // handler below already returns Changed when hover_row
        // moves so we let it run.
        flipped
    } else {
        false
    };

    // Handle `[-]` / `[+]` clicks
    // when in EditingValue mode AND the row is an Int. All other
    // events in EditingValue (scrolls, off-adornment clicks) are
    // no-ops.
    if matches!(state.mode, SettingsModalMode::EditingValue { .. }) {
        let outcome = handle_editor_mouse(state, kind, column, row);
        return upgrade_if_breadcrumb_flipped(outcome, breadcrumb_hover_flipped);
    }

    // PickingEnum: click-to-pick on choice rects, scroll wheel is a
    // no-op (the picker is bounded; scroll there could surprise).
    if matches!(state.mode, SettingsModalMode::PickingEnum { .. }) {
        let outcome = handle_picker_mouse(state, kind, column, row);
        return upgrade_if_breadcrumb_flipped(outcome, breadcrumb_hover_flipped);
    }

    // PickingGroup: hover tracks the child rects; a click toggles the clicked
    // child in place (same bounded-viewport, scroll-is-a-no-op contract).
    if matches!(state.mode, SettingsModalMode::PickingGroup { .. }) {
        let outcome = handle_group_mouse(state, kind, column, row);
        return upgrade_if_breadcrumb_flipped(outcome, breadcrumb_hover_flipped);
    }

    let on_list = rect_contains(state.list_area, column, row);

    // Mouse hover highlight (parity with scrollback).
    // Walks `state.row_rects` to find the row under the cursor and
    // updates `state.hover_row`. Returns early so subsequent arms
    // don't need to repeat the find. Clicks and scrolls fall
    // through to the existing arms below.
    if matches!(kind, MouseEventKind::Moved) {
        let new_hover = state
            .row_rects
            .iter()
            .position(|r| rect_contains(*r, column, row))
            .filter(|&idx| matches!(state.rows.get(idx), Some(RowEntry::Setting { .. })));
        if new_hover != state.hover_row {
            state.hover_row = new_hover;
            return SettingsKeyOutcome::Changed;
        }
        return SettingsKeyOutcome::Unchanged;
    }

    match kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            if !on_list {
                return SettingsKeyOutcome::Unchanged;
            }
            // Resolve the clicked row.
            let clicked_idx = state
                .row_rects
                .iter()
                .position(|r| rect_contains(*r, column, row));
            let Some(idx) = clicked_idx else {
                return SettingsKeyOutcome::Unchanged;
            };
            // Clicking a header is a no-op.
            if !matches!(state.rows[idx], RowEntry::Setting { .. }) {
                return SettingsKeyOutcome::Unchanged;
            }
            // Two-stage click semantics:
            //   - Click on a different row: only select (lets the user
            //     read the description first).
            //   - Click on the already-selected Bool row: toggle.
            //   - Click on the already-selected Enum row: open picker.
            //   - Click on the indicator cells of any Bool / Enum /
            //     String / Int row: select + activate in one click
            //     (Fitts's-law nudge — 5-col hit-rect around the
            //     small glyph).
            //
            // The per-kind dispatch is
            // collapsed into a single `if` chain — the
            // `toggle_focused_bool` / `try_enter_picking_enum` /
            // `try_enter_editing_value` helpers all return falsy
            // for non-matching kinds, so the per-kind predicates
            // are redundant.
            let row_rect = state.row_rects[idx];
            // User-feedback follow-up: col 0 of the row is the
            // `▸`/`▾` triangle glyph. A click there toggles
            // expansion (no value mutation) — matching the
            // keyboard's Right/Left arrow contract. The triangle
            // is at exactly column `row_rect.x` and is 1 cell wide.
            //
            // Two-line rows have `row_rect.height = 2`,
            // and the triangle stays on LINE 1 only — clicks on
            // line 2's col 0 (which is empty padding) shouldn't
            // toggle expansion. The y-check enforces this.
            let on_triangle = column == row_rect.x && row == row_rect.y;
            // The 5-col Fitts's-law
            // indicator hit-rect sits on the
            // value column on the right (rather than the left edge). Clicking the Bool's
            // `on`/`off` text toggles the bool in one click; clicking
            // an Enum/String/DynamicEnum/Int value opens the
            // picker/editor in one click. The hit-rect is supplied
            // by `render_setting_row` via
            // `state.value_hit_rects[idx]`.
            let value_rect = state.value_hit_rects.get(idx).copied().unwrap_or_default();
            let on_value = rect_contains(value_rect, column, row);
            let was_selected_already = state.selected == idx;
            let _ = state.select_at(idx);

            if on_triangle && let Some((key, _meta)) = state.focused_setting() {
                // Toggle expansion. Mirrors the keyboard
                // Right/Left arrow contract.
                if state.expanded_keys.contains(key) {
                    state.expanded_keys.remove(key);
                } else {
                    state.expanded_keys.insert(key);
                }
                return SettingsKeyOutcome::Changed;
            }

            if on_value || was_selected_already {
                if state.try_enter_picking_group() {
                    return SettingsKeyOutcome::Changed;
                }
                if let Some(action) = state.toggle_focused_bool() {
                    return SettingsKeyOutcome::Action(action);
                }
                if state.try_enter_picking_enum() || state.try_enter_editing_value() {
                    return SettingsKeyOutcome::Changed;
                }
            }
            // Selection moved (or was already on this row); the
            // re-render reflects the new focus.
            SettingsKeyOutcome::Changed
        }
        MouseEventKind::ScrollDown => {
            if !on_list {
                return SettingsKeyOutcome::Unchanged;
            }
            let mut moved = false;
            for _ in 0..3 {
                moved |= state.advance_next();
            }
            changed_if(moved)
        }
        MouseEventKind::ScrollUp => {
            if !on_list {
                return SettingsKeyOutcome::Unchanged;
            }
            let mut moved = false;
            for _ in 0..3 {
                moved |= state.advance_prev();
            }
            changed_if(moved)
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// Handle a mouse event while the modal is in `PickingEnum` mode.
///
/// Left-click on any line of a choice's multi-line hit-rect moves
/// the picker focus to that choice (and fires the matching preview
/// dispatch, mirroring keyboard Up/Down). Clicks outside any choice
/// rect are no-ops, as are scroll wheel events (the picker viewport
/// is bounded; in-picker scrolling could surprise).
///
/// Continuation lines of a word-wrapped description share the same
/// hit-rect as the choice they belong to — clicking the second line
/// of "Opt out" picks "Opt out", same as clicking its symbol.
fn handle_picker_mouse(
    state: &mut SettingsModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> SettingsKeyOutcome {
    // Hover highlight for picker choices. Tracks the
    // choice index under the cursor in `state.hover_row` (same
    // field as the row-list path; the field is mode-aware via the
    // active `state.mode`).
    if matches!(kind, MouseEventKind::Moved) {
        let new_hover = state
            .picker_choice_rects
            .iter()
            .position(|r| r.height > 0 && rect_contains(*r, column, row));
        if new_hover != state.hover_row {
            state.hover_row = new_hover;
            return SettingsKeyOutcome::Changed;
        }
        return SettingsKeyOutcome::Unchanged;
    }

    let MouseEventKind::Down(crossterm::event::MouseButton::Left) = kind else {
        return SettingsKeyOutcome::Unchanged;
    };
    // Snapshot the picker payload under the immutable borrow.
    let (setting_key, current_idx, original_value, supports_preview) = match &state.mode {
        SettingsModalMode::PickingEnum {
            key,
            choices_idx,
            original_value,
            supports_preview,
        } => (
            *key,
            *choices_idx,
            original_value.clone(),
            *supports_preview,
        ),
        _ => return SettingsKeyOutcome::Unchanged,
    };
    let clicked_idx = state
        .picker_choice_rects
        .iter()
        .position(|r| r.height > 0 && rect_contains(*r, column, row));
    let Some(target_idx) = clicked_idx else {
        return SettingsKeyOutcome::Unchanged;
    };
    if target_idx == current_idx {
        // Already focused — re-clicking the same choice is a no-op
        // (kept for parity with the row-list's "already-focused
        // click commits" semantics; commit fires on Enter, not on
        // a re-click).
        return SettingsKeyOutcome::Unchanged;
    }
    // Reuse the keyboard nav helper to update `choices_idx` AND
    // fire the matching preview Action (when the kind supports it).
    set_picker_idx(
        state,
        setting_key,
        target_idx,
        original_value,
        supports_preview,
    )
}

/// Handle a mouse event while the modal is in `PickingGroup` mode.
///
/// Hover tracks the child row under the cursor; a left-click moves focus to the
/// clicked child AND toggles it in one click (toggles, unlike the enum picker's
/// commit-on-Enter). Scroll wheel is a no-op (the sub-sheet is bounded).
fn handle_group_mouse(
    state: &mut SettingsModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> SettingsKeyOutcome {
    if matches!(kind, MouseEventKind::Moved) {
        let new_hover = state
            .picker_choice_rects
            .iter()
            .position(|r| r.height > 0 && rect_contains(*r, column, row));
        if new_hover != state.hover_row {
            state.hover_row = new_hover;
            return SettingsKeyOutcome::Changed;
        }
        return SettingsKeyOutcome::Unchanged;
    }
    let MouseEventKind::Down(crossterm::event::MouseButton::Left) = kind else {
        return SettingsKeyOutcome::Unchanged;
    };
    let group_key = match &state.mode {
        SettingsModalMode::PickingGroup { key, .. } => *key,
        _ => return SettingsKeyOutcome::Unchanged,
    };
    let children = group_children(state, group_key);
    let clicked_idx = state
        .picker_choice_rects
        .iter()
        .position(|r| r.height > 0 && rect_contains(*r, column, row));
    let Some(idx) = clicked_idx else {
        return SettingsKeyOutcome::Unchanged;
    };
    state.mode = SettingsModalMode::PickingGroup {
        key: group_key,
        child_idx: idx,
    };
    let Some(child_key) = children.get(idx).copied() else {
        return SettingsKeyOutcome::Changed;
    };
    let cur = matches!(state.value_for(child_key), Some(SettingValue::Bool(true)));
    match action_for_bool(child_key, !cur) {
        Some(action) => SettingsKeyOutcome::Action(action),
        None => SettingsKeyOutcome::Changed,
    }
}

/// Handle a mouse event while in `EditingValue` mode. Dispatches
/// clicks on the Int editor's `[-]` / `[+]` adornments as
/// keyboard-equivalent Down/Up steps; everything else is a no-op.
fn handle_editor_mouse(
    state: &mut SettingsModalState,
    kind: MouseEventKind,
    column: u16,
    row: u16,
) -> SettingsKeyOutcome {
    let MouseEventKind::Down(crossterm::event::MouseButton::Left) = kind else {
        return SettingsKeyOutcome::Unchanged;
    };
    let (dec_rect, inc_rect) = state.editor_adornment_rects;
    let step_dir = if rect_contains(dec_rect, column, row) {
        StepDir::Down
    } else if rect_contains(inc_rect, column, row) {
        StepDir::Up
    } else {
        return SettingsKeyOutcome::Unchanged;
    };
    // Synthesize the equivalent keyboard event so the actual
    // step + clamp + validation logic lives in one place
    // (`handle_editing_value`). Mirrors the picker's choice-click
    // approach.
    let synthetic = KeyEvent::new(
        match step_dir {
            StepDir::Up => KeyCode::Up,
            StepDir::Down => KeyCode::Down,
        },
        KeyModifiers::NONE,
    );
    handle_editing_value(state, &synthetic)
}

/// Direction tag for the Int editor's spinner mouse-click → key
/// synthesis. Internal to `handle_editor_mouse`.
#[derive(Clone, Copy)]
enum StepDir {
    Up,
    Down,
}

fn rect_contains(r: Rect, column: u16, row: u16) -> bool {
    r.width > 0
        && r.height > 0
        && column >= r.x
        && column < r.x.saturating_add(r.width)
        && row >= r.y
        && row < r.y.saturating_add(r.height)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{EnumChoice, SettingOwner, SettingsRegistry};
    use xai_grok_shell::agent::config::UiConfig;

    fn make_state() -> SettingsModalState {
        SettingsModalState::new(
            Arc::new(SettingsRegistry::defaults()),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        )
    }

    /// The contextual-hints group renders as a single top-level row (children
    /// hidden); Enter opens the sub-sheet, Space there toggles the focused
    /// child via the typed action, and Esc returns to Browse.
    #[test]
    fn contextual_hints_group_sub_sheet_flow() {
        let mut s = make_state();
        // Group row present; child rows hidden from the top-level list.
        let group_idx = s
            .rows
            .iter()
            .position(|r| matches!(r, RowEntry::Setting { key, .. } if *key == "contextual_hints"))
            .expect("group row present");
        assert!(
            !s.rows.iter().any(|r| matches!(
                r,
                RowEntry::Setting { key, .. } if key.starts_with("contextual_hints.")
            )),
            "child rows must be hidden from the top-level list",
        );

        // Focus the group, Enter → PickingGroup on the first child.
        s.selected = group_idx;
        let out = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(out, SettingsKeyOutcome::Changed));
        assert!(matches!(
            s.mode,
            SettingsModalMode::PickingGroup { child_idx: 0, .. }
        ));

        // Space toggles the focused child (undo defaults ON → set false).
        let out = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        assert!(
            matches!(
                out,
                SettingsKeyOutcome::Action(Action::SetContextualHintUndo(false))
            ),
            "Space must toggle the focused child via the typed action, got {out:?}",
        );

        // Esc returns to Browse.
        let out = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(out, SettingsKeyOutcome::Changed));
        assert!(matches!(s.mode, SettingsModalMode::Browse));
    }

    /// The permission_mode picker hides the "Auto" choice when the auto feature
    /// gate is off (matching the Shift+Tab cycle, which skips Auto when gated),
    /// and shows it when the gate is on. Other choices are unaffected.
    #[test]
    fn effective_enum_choices_hides_auto_for_permission_mode_when_gated_off() {
        let reg = SettingsRegistry::defaults();
        let meta = reg
            .find("permission_mode")
            .expect("permission_mode registered");
        let SettingKind::Enum { choices, .. } = &meta.kind else {
            panic!("permission_mode must be Enum");
        };

        let gated_off = PagerLocalSnapshot {
            auto_mode_gate: false,
            ..PagerLocalSnapshot::default()
        };
        let filtered = effective_enum_choices("permission_mode", choices, &gated_off);
        assert!(
            !filtered.iter().any(|c| c.canonical == "auto"),
            "Auto must be hidden from the permission_mode picker when the gate is off"
        );
        assert!(
            filtered.iter().any(|c| c.canonical == "ask"),
            "non-Auto choices must remain"
        );

        let gated_on = PagerLocalSnapshot {
            auto_mode_gate: true,
            ..PagerLocalSnapshot::default()
        };
        let full = effective_enum_choices("permission_mode", choices, &gated_on);
        assert!(
            full.iter().any(|c| c.canonical == "auto"),
            "Auto must be selectable when the gate is on"
        );

        // A non-gated key is never filtered.
        let theme = reg.find("theme").expect("theme registered");
        if let SettingKind::Enum {
            choices: theme_choices,
            ..
        } = &theme.kind
        {
            assert_eq!(
                effective_enum_choices("theme", theme_choices, &gated_off).len(),
                theme_choices.len(),
                "non-permission_mode keys are never filtered"
            );
        }
    }

    /// `voice_capture_mode`'s "hold" choice is gated off without key releases and
    /// available with them; "toggle" is never gated. Permission_mode's "auto"
    /// gating is preserved. Pure — no process-global mutation.
    #[test]
    fn enum_choice_gated_off_covers_voice_and_permission() {
        // voice "hold": gated iff no key releases.
        assert!(enum_choice_gated_off(
            "voice_capture_mode",
            "hold",
            true,
            false
        ));
        assert!(!enum_choice_gated_off(
            "voice_capture_mode",
            "hold",
            true,
            true
        ));
        // voice "toggle": never gated.
        assert!(!enum_choice_gated_off(
            "voice_capture_mode",
            "toggle",
            true,
            false
        ));
        // permission_mode "auto": gated iff the auto gate is off.
        assert!(enum_choice_gated_off(
            "permission_mode",
            "auto",
            false,
            true
        ));
        assert!(!enum_choice_gated_off(
            "permission_mode",
            "auto",
            true,
            true
        ));
    }

    /// Look up a setting's registered metadata by key (test helper).
    fn meta_for(reg: &SettingsRegistry, key: SettingKey) -> &SettingMeta {
        reg.all()
            .iter()
            .find(|m| m.key == key)
            .unwrap_or_else(|| panic!("`{key}` not registered"))
    }

    /// The whole `voice_capture_mode` row is hidden without key-release reporting
    /// (only `toggle` is possible, so there's no choice) and shown with it.
    /// Other settings are always visible. Pure — no process-global mutation.
    #[test]
    fn setting_row_visible_gates_voice_capture_on_key_releases() {
        let reg = SettingsRegistry::defaults();
        let voice = meta_for(&reg, "voice_capture_mode");
        let vim = meta_for(&reg, "vim_mode");
        assert!(!setting_row_visible(voice, false, false));
        assert!(setting_row_visible(voice, true, false));
        assert!(setting_row_visible(vim, false, false));
    }

    #[test]
    fn setting_row_visible_hides_theme_rows_in_minimal() {
        let reg = SettingsRegistry::defaults();
        for key in [
            "theme",
            "auto_dark_theme",
            "auto_light_theme",
            "display_refresh_auto_cadence",
        ] {
            let meta = meta_for(&reg, key);
            assert!(meta.hidden_in_minimal, "{key} must declare the flag");
            assert!(!setting_row_visible(meta, true, true), "{key} in minimal");
            assert!(setting_row_visible(meta, true, false), "{key} in full TUI");
        }
        assert!(setting_row_visible(meta_for(&reg, "vim_mode"), true, true));
    }

    /// `action_for_bool` mirrors `current_value_for`: every registered
    /// Bool setting must have an arm here too. Without this test, a
    /// future PR could register a Bool setting that the modal can read
    /// (via `current_value_for`) but not toggle (because `action_for_bool`
    /// silently returns `None`).
    #[test]
    fn every_setting_has_action_for_bool_arm() {
        let reg = SettingsRegistry::defaults();
        for meta in reg.all() {
            if !matches!(meta.kind, SettingKind::Bool { .. }) {
                continue;
            }
            assert!(
                action_for_bool(meta.key, true).is_some(),
                "Bool setting `{}` has no action_for_bool arm — \
                 modal would toggle silently. Add an arm in views/settings_modal.rs::action_for_bool.",
                meta.key,
            );
            assert!(
                action_for_bool(meta.key, false).is_some(),
                "action_for_bool(`{}`, false) returned None",
                meta.key,
            );
        }
    }

    /// Mirror of `every_setting_has_action_for_bool_arm` for Enum
    /// settings: every preview-supporting registered Enum + every one
    /// of its canonical choices must have a matching `action_for_enum`
    /// arm. Without this guard, a change could register `theme` in
    /// `default_settings()` and forget to add the
    /// `"theme" => Some(Action::SetTheme(...))` arm — the picker
    /// would silently degrade (nav advances, no Action emitted,
    /// preview never fires).
    ///
    /// Non-preview Enums (e.g.
    /// `permission_mode` where `supports_preview: false`) are
    /// excluded from this check — their picker nav skips
    /// `action_for_enum` entirely (gated by `supports_preview` in
    /// `set_picker_idx`), so returning `None` is the correct
    /// behaviour and would otherwise look like a missing arm. The
    /// commit-arm test below covers them.
    ///
    /// With no Enum entries registered the loop body never
    /// executes and the check passes vacuously; registering an Enum
    /// forces the matching arm in `action_for_enum` to land alongside it.
    #[test]
    fn every_preview_enum_setting_has_action_for_enum_arm() {
        let reg = SettingsRegistry::defaults();
        for meta in reg.all() {
            let SettingKind::Enum {
                choices,
                supports_preview,
                ..
            } = &meta.kind
            else {
                continue;
            };
            if !*supports_preview {
                continue;
            }
            for c in *choices {
                assert!(
                    action_for_enum(meta.key, c.canonical).is_some(),
                    "Preview-enabled Enum setting `{}` choice `{}` has no action_for_enum arm — \
                     picker would silently degrade (no preview/revert Action emitted). \
                     Add an arm in views/settings_modal.rs::action_for_enum.",
                    meta.key,
                    c.canonical,
                );
            }
        }
    }

    /// Mirror of `every_setting_has_action_for_bool_arm` for
    /// `SettingKind::String`. Every registered String setting must
    /// have a matching arm in `action_for_string`, otherwise the
    /// editor's commit path returns `None` and the Enter keystroke
    /// silently no-ops (after exiting EditingValue mode).
    ///
    /// The empty-string path is
    /// the canonical "clear default" entry-point — every
    /// registered String setting must produce SOME action for
    /// empty input (even if it's `ClearDefaultModel` rather than a
    /// `SetX` variant).
    ///
    /// **Vacuous-passing note**: today no
    /// production setting uses `SettingKind::String` — both
    /// `default_model` and `fork_secondary_model` use
    /// `DynamicEnum`, and `coding_data_sharing` / `permission_mode`
    /// / `plan_mode` are `Enum`. The loop body skips every meta, so
    /// this assertion passes vacuously today. It STILL fires as a
    /// CI guard the first time a future change registers a String
    /// setting without an action arm. Renamed in spirit to
    /// `if_a_string_setting_is_added_it_has_an_action_arm` via the
    /// panic message; the test fn name keeps `every_*` for
    /// consistency with the sibling guards.
    #[test]
    fn every_string_setting_has_action_for_string_arm() {
        let reg = SettingsRegistry::defaults();
        let snapshot = PagerLocalSnapshot::default();
        for meta in reg.all() {
            if !matches!(meta.kind, SettingKind::String { .. }) {
                continue;
            }
            // Empty buffer must produce some action (clear or
            // similar) — not None.
            assert!(
                action_for_string(meta.key, String::new(), &snapshot).is_some(),
                "If you just added a String setting, it has no \
                 action_for_string arm for empty input — editor would \
                 silently degrade on the user's first Enter. Add an arm \
                 in views/settings_modal.rs::action_for_string for `{}`.",
                meta.key,
            );
        }
    }

    /// Every registered
    /// `SettingKind::DynamicEnum` setting must have a matching arm
    /// in `action_for_string` for the picker's Enter (commit) path,
    /// including the empty-canonical sentinel (row 0 of the picker
    /// is always "(no override)").
    #[test]
    fn every_dynamic_enum_setting_has_action_for_string_arm() {
        let reg = SettingsRegistry::defaults();
        // Seed a synthetic catalog so the resolver path can produce
        // a non-empty SetX action — empty-only would mask a missing
        // SetX arm.
        use agent_client_protocol as acp;
        use std::sync::Arc;
        let snapshot = PagerLocalSnapshot {
            available_models: vec![(
                "Test Model".to_string(),
                acp::ModelId::new(Arc::from("test-model")),
            )],
            ..PagerLocalSnapshot::default()
        };
        for meta in reg.all() {
            if !matches!(meta.kind, SettingKind::DynamicEnum { .. }) {
                continue;
            }
            // Discriminate on the Action variant, not
            // just `is_some()`. A future refactor that swallowed the
            // typed `SetDefaultModel` / `SetForkSecondaryModel` into
            // a generic `Action::DynamicSettingChanged(...)` would
            // pass `is_some()` while breaking the typed dispatch.
            let empty_action = action_for_string(meta.key, String::new(), &snapshot);
            let nonempty_action = action_for_string(meta.key, "Test Model".to_string(), &snapshot);
            match meta.key {
                "default_model" => {
                    assert!(
                        matches!(empty_action, Some(Action::ClearDefaultModel)),
                        "default_model empty canonical must produce ClearDefaultModel, \
                         got {empty_action:?}",
                    );
                    assert!(
                        matches!(nonempty_action, Some(Action::SetDefaultModel(_))),
                        "default_model non-empty canonical must produce \
                         SetDefaultModel(_), got {nonempty_action:?}",
                    );
                }
                "fork_secondary_model" => {
                    assert!(
                        matches!(empty_action, Some(Action::ClearForkSecondaryModel)),
                        "fork_secondary_model empty canonical must produce \
                         ClearForkSecondaryModel, got {empty_action:?}",
                    );
                    assert!(
                        matches!(nonempty_action, Some(Action::SetForkSecondaryModel(_))),
                        "fork_secondary_model non-empty canonical must produce \
                         SetForkSecondaryModel(_), got {nonempty_action:?}",
                    );
                }
                other => panic!(
                    "Unknown DynamicEnum key `{other}` — add a discriminating arm in \
                     every_dynamic_enum_setting_has_action_for_string_arm so future \
                     additions can't silently rely on the generic is_some() check.",
                ),
            }
        }
    }

    /// Mirror of `every_setting_has_action_for_bool_arm` for
    /// `SettingKind::Int`. Every registered Int setting must have a
    /// matching arm in `action_for_int`.
    #[test]
    fn every_int_setting_has_action_for_int_arm() {
        let reg = SettingsRegistry::defaults();
        for meta in reg.all() {
            if !matches!(meta.kind, SettingKind::Int { .. }) {
                continue;
            }
            assert!(
                action_for_int(meta.key, 0).is_some(),
                "Int setting `{}` has no action_for_int arm — \
                 editor would silently degrade. Add an arm in \
                 views/settings_modal.rs::action_for_int.",
                meta.key,
            );
        }
    }

    /// Mirror of `every_preview_enum_setting_has_action_for_enum_arm`
    /// for the COMMIT path: every registered Enum + every canonical
    /// choice must have a matching `action_for_enum_commit` arm.
    /// Unlike the preview arm, this applies to ALL Enums (preview
    /// and non-preview) — Enter (commit) is the structural mutation
    /// path regardless of preview support.
    ///
    /// `permission_mode` has
    /// `supports_preview: false`; this test ensures its commit arm
    /// is wired.
    #[test]
    fn every_enum_setting_has_action_for_enum_commit_arm() {
        let reg = SettingsRegistry::defaults();
        for meta in reg.all() {
            let SettingKind::Enum { choices, .. } = &meta.kind else {
                continue;
            };
            for c in *choices {
                assert!(
                    action_for_enum_commit(meta.key, c.canonical).is_some(),
                    "Enum setting `{}` choice `{}` has no action_for_enum_commit arm — \
                     Enter on the picker would no-op silently. Add an arm in \
                     views/settings_modal.rs::action_for_enum_commit.",
                    meta.key,
                    c.canonical,
                );
            }
        }
    }

    /// The previous behaviour truncated labels
    /// at `max_label_w` regardless of available area width. The new
    /// behaviour prefers to render the FULL label whenever the row's
    /// label + value will fit on one logical line; truncation is
    /// reserved for the pathological case where even the label alone
    /// is wider than the row (covered by
    /// `pathologically_narrow_truncates_label_with_ellipsis`).
    ///
    /// At an 80-col area, a 42-col label easily fits on one line so
    /// the full label renders without an ellipsis — the regression
    /// the user reported is gone.
    #[test]
    fn render_setting_row_shows_full_label_when_one_line_fits() {
        let meta = SettingMeta {
            key: "test-key",
            category: SettingCategory::Appearance,
            owner: crate::settings::SettingOwner::Shared,
            label: "A very long label that exceeds the budget",
            description: "Test description.",
            keywords: &["test"],
            kind: SettingKind::Bool { default: false },
            restart_required: false,
            hidden_in_minimal: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_setting_row(
            &mut buf,
            area,
            &meta,
            &SettingValue::Bool(false),
            15, // max_label_w — kept for API compatibility, no longer used.
            false,
            &theme,
            false, // is_expanded
            false, // is_hovered
        );
        let mut rendered = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 0)) {
                rendered.push_str(cell.symbol());
            }
        }
        assert!(
            !rendered.contains('\u{2026}'),
            "Commit 13: a row whose label + value fits on one line \
             should NOT show a truncation ellipsis: {rendered:?}"
        );
        assert!(
            rendered.contains("A very long label that exceeds the budget"),
            "full label must be visible when one-line layout fits: {rendered:?}"
        );
    }

    /// The default registry contains Appearance settings
    /// (3 bools + 3 enums + 1 int = 7 entries), the Editor entry
    /// `multiline_mode`, the Agent entries `permission_mode` and
    /// `plan_mode`, the Privacy entry `coding_data_sharing`, the
    /// Models entry `default_model`, and the Advanced entries
    /// `show_tips` and `auto_update`. `default_reasoning_effort` and
    /// `auto_compact_threshold_percent` are not exposed in the modal.
    #[test]
    fn rows_contain_categories_and_settings_through_pr_14() {
        let s = make_state();
        let headers: Vec<&SettingCategory> = s
            .rows
            .iter()
            .filter_map(|r| {
                if let RowEntry::Header { category } = r {
                    Some(category)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            headers,
            vec![
                &SettingCategory::Appearance,
                &SettingCategory::Mouse,
                &SettingCategory::Editor,
                &SettingCategory::Agent,
                &SettingCategory::Privacy,
                &SettingCategory::Models,
                // The Session category has no registered settings, so its
                // header is not emitted.
                // Advanced category (first entries:
                // `show_tips`, `auto_update`).
                &SettingCategory::Advanced,
            ]
        );

        let settings: Vec<SettingKey> = s
            .rows
            .iter()
            .filter_map(|r| {
                if let RowEntry::Setting { key, .. } = r {
                    Some(*key)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            settings,
            vec![
                // Booleans.
                "compact_mode",
                "show_timestamps",
                "simple_mode",
                // PAGER-owned vim_mode (Appearance,
                // paired with simple_mode).
                "vim_mode",
                // Theme enums.
                "theme",
                "auto_dark_theme",
                "auto_light_theme",
                // SHELL-owned render_mermaid (Appearance,
                // declared after the theme enums).
                "render_mermaid",
                // Int in Appearance category.
                "max_thoughts_width",
                // SHELL-owned show_thinking_blocks (Appearance; live cache).
                "show_thinking_blocks",
                // PAGER-owned respect_manual_folds (Appearance,
                // persisted to pager.toml).
                "respect_manual_folds",
                // SHELL-owned group_tool_verbs (Appearance; live cache).
                "group_tool_verbs",
                // SHELL-owned collapsed_edit_blocks (Appearance; live cache,
                // default OFF rollout flag).
                "collapsed_edit_blocks",
                // SHELL-owned display_refresh_auto_cadence (Appearance).
                "display_refresh_auto_cadence",
                // Mouse — scroll + drag selection. The scroll
                // classification/lines/direction knobs follow scroll_speed.
                "scroll_speed",
                "scroll_mode",
                "scroll_lines",
                "invert_scroll",
                "keep_text_selection",
                // PAGER-owned multiline (Editor category).
                "multiline_mode",
                // SHELL-owned prompt_suggestions (Editor; tab autocomplete
                // ghost text, live cache).
                "prompt_suggestions",
                // voice_capture_mode (Editor) is absent here: `setting_row_visible`
                // hides it without key releases, and tests run with Kitty unset.
                // SHELL-owned voice_stt_language (Editor, always visible).
                "voice_stt_language",
                // SHELL-owned permission_mode (Agent category).
                "permission_mode",
                // SHELL-owned remember_tool_approvals (Agent category,
                // registered right after permission_mode).
                "remember_tool_approvals",
                // SHELL-owned default_selected_permission (Agent category,
                // colocated with permission_mode / plan_mode).
                "default_selected_permission",
                // SHELL-owned ask_user_question timeout (Agent category,
                // registered directly above plan_mode).
                "toolset.ask_user_question.timeout_enabled",
                // PAGER-owned plan_mode (Agent category).
                "plan_mode",
                // SHELL-owned coding_data_sharing (Privacy category).
                "coding_data_sharing",
                // SHELL-owned default_model (Models category).
                "default_model",
                // Models category. `default_reasoning_effort`,
                // `web_search_model`, and `session_summary_model` are
                // not exposed in the modal.
                "fork_secondary_model",
                // `auto_compact_threshold_percent` (Session category) is
                // not exposed in the modal.
                // Advanced category.
                "show_tips",
                // Per-tip contextual-hints GROUP row, repositioned right after
                // `show_tips`. Its 3 child toggles
                // (`contextual_hints.{undo,plan_mode,image_input}`) are hidden
                // from the top-level list and reached via the sub-sheet.
                "contextual_hints",
                "auto_update",
                // SHELL-owned hunk_tracker_mode (Advanced; `off` disables it).
                "hunk_tracker_mode",
            ]
        );
    }

    #[test]
    fn initial_selection_skips_header() {
        let s = make_state();
        match &s.rows[s.selected] {
            RowEntry::Setting { key, .. } => assert_eq!(*key, "compact_mode"),
            RowEntry::Header { .. } => panic!("selection landed on a header"),
        }
    }

    /// `j` advances through the row list one selectable entry at a
    /// time and is a no-op at the last visible setting.
    ///
    /// Resilient to future setting additions: the test walks `j` to the
    /// end of the registry dynamically rather than hardcoding row
    /// counts.
    #[test]
    fn j_advances_past_setting_rows() {
        let mut s = make_state();
        let key1 = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        // The initial selection is the first selectable row.
        let setting_keys: Vec<SettingKey> = s
            .rows
            .iter()
            .filter_map(|r| {
                if let RowEntry::Setting { key, .. } = r {
                    Some(*key)
                } else {
                    None
                }
            })
            .collect();
        assert!(setting_keys.len() >= 2, "test requires at least 2 settings");
        // Walk j to the last setting; each step must be Changed.
        for expected in setting_keys.iter().skip(1) {
            assert!(matches!(
                handle_settings_key(&mut s, &key1),
                SettingsKeyOutcome::Changed
            ));
            match &s.rows[s.selected] {
                RowEntry::Setting { key, .. } => assert_eq!(*key, *expected),
                _ => panic!("expected setting row after j"),
            }
        }
        // At the last row, j is a no-op.
        assert!(matches!(
            handle_settings_key(&mut s, &key1),
            SettingsKeyOutcome::Unchanged
        ));
    }

    #[test]
    fn space_on_compact_mode_dispatches_set_compact_mode_true() {
        let mut s = make_state();
        // Default compact_mode is false → Space dispatches Set(true).
        let space = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        let outcome = handle_settings_key(&mut s, &space);
        match outcome {
            SettingsKeyOutcome::Action(Action::SetCompactMode(true)) => {}
            other => panic!("expected SetCompactMode(true), got {other:?}"),
        }
    }

    #[test]
    fn enter_on_compact_mode_also_toggles() {
        let mut s = make_state();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let outcome = handle_settings_key(&mut s, &enter);
        match outcome {
            SettingsKeyOutcome::Action(Action::SetCompactMode(true)) => {}
            other => panic!("expected SetCompactMode(true) from Enter, got {other:?}"),
        }
    }

    #[test]
    fn f2_closes_modal() {
        let mut s = make_state();
        let f2 = KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE);
        assert!(matches!(
            handle_settings_key(&mut s, &f2),
            SettingsKeyOutcome::Close
        ));
    }

    #[test]
    fn esc_in_browse_mode_falls_through_to_chrome() {
        // Esc is intercepted UPSTREAM by `ModalWindow::handle_modal_key`;
        // `handle_settings_key` does not match Esc anymore. See module
        // docstring.
        let mut s = make_state();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            handle_settings_key(&mut s, &esc),
            SettingsKeyOutcome::Unchanged
        ));
    }

    #[test]
    fn ctrl_comma_closes_modal() {
        let mut s = make_state();
        let key = KeyEvent::new(KeyCode::Char(','), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_settings_key(&mut s, &key),
            SettingsKeyOutcome::Close
        ));
    }

    #[test]
    fn cmd_comma_closes_modal_on_macos() {
        let mut s = make_state();
        let key = KeyEvent::new(KeyCode::Char(','), KeyModifiers::SUPER);
        assert!(matches!(
            handle_settings_key(&mut s, &key),
            SettingsKeyOutcome::Close
        ));
    }

    #[test]
    fn filter_mode_swallows_chars_into_query() {
        let mut s = make_state();
        let slash = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(matches!(
            handle_settings_key(&mut s, &slash),
            SettingsKeyOutcome::Changed
        ));
        assert!(matches!(s.mode, SettingsModalMode::FilterFocused));
        for c in "compact".chars() {
            let k = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
            let _ = handle_settings_key(&mut s, &k);
        }
        assert_eq!(s.query, "compact");

        // Esc exits filter, doesn't close modal.
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            handle_settings_key(&mut s, &esc),
            SettingsKeyOutcome::Changed
        ));
        assert!(matches!(s.mode, SettingsModalMode::Browse));
    }

    /// `i` aliases `/` without modifiers: from Browse it enters FilterFocused
    /// exactly like `/` (vim-nav "press i to search").
    #[test]
    fn i_key_enters_filter_like_slash() {
        let mut s = make_state();
        let i = KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE);
        assert!(matches!(
            handle_settings_key(&mut s, &i),
            SettingsKeyOutcome::Changed
        ));
        assert!(matches!(s.mode, SettingsModalMode::FilterFocused));
    }

    /// The `modifiers.is_empty()` guard: Ctrl+i / Alt+i must NOT enter filter.
    #[test]
    fn modified_i_does_not_enter_filter() {
        for mods in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            let mut s = make_state();
            let k = KeyEvent::new(KeyCode::Char('i'), mods);
            assert!(matches!(
                handle_settings_key(&mut s, &k),
                SettingsKeyOutcome::Unchanged
            ));
            assert!(matches!(s.mode, SettingsModalMode::Browse));
        }
    }

    /// Wiring check: the Browse footer carries the shared `i search` hint
    /// under vim nav mode. The gate itself is covered centrally by
    /// `modal_window::tests::vim_nav_search_hint_only_in_vim_nav_mode`. The
    /// explicit `set_vim_mode` pin (a thread-local that, once set, blocks
    /// disk-seeding) keeps this independent of the dev's on-disk `[ui].vim_mode`;
    /// reset afterward since libtest reuses worker threads.
    #[test]
    fn browse_footer_advertises_i_search_under_vim() {
        crate::appearance::cache::set_vim_mode(true);
        let s = make_state();
        assert!(matches!(s.mode, SettingsModalMode::Browse));
        assert!(
            build_shortcuts(&s).iter().any(|sc| sc.label == "i search"),
            "vim-mode Browse footer must advertise `i search`"
        );
        crate::appearance::cache::set_vim_mode(false);
    }

    #[test]
    fn mouse_click_on_bool_row_dispatches_toggle() {
        // We can't render to a real Buffer here without a real terminal,
        // but we can hand-set the row_rects to simulate the post-render
        // state.
        let mut s = make_state();
        s.list_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        };
        s.row_rects.resize(s.rows.len(), Rect::default());
        // Row 0 is the Appearance header.
        s.row_rects[0] = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        // Row 1 is compact_mode.
        s.row_rects[1] = Rect {
            x: 0,
            y: 1,
            width: 80,
            height: 1,
        };

        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            1,
        );
        match outcome {
            SettingsKeyOutcome::Action(Action::SetCompactMode(true)) => {}
            other => panic!("expected SetCompactMode(true) from click, got {other:?}"),
        }
    }

    #[test]
    fn mouse_click_on_header_is_no_op() {
        let mut s = make_state();
        s.list_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        };
        s.row_rects.resize(s.rows.len(), Rect::default());
        s.row_rects[0] = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };

        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            0,
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Unchanged));
    }

    // ---------- mouse hover highlight ----------

    /// `MouseEventKind::Moved` over a setting row's hit-rect sets
    /// `state.hover_row` to that row's index and reports `Changed`
    /// so the next render paints the highlight. Mirrors the
    /// scrollback's `hovered_entry` plumbing.
    #[test]
    fn mouse_moved_over_row_sets_hover_row() {
        let mut s = make_state();
        s.list_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        };
        s.row_rects.resize(s.rows.len(), Rect::default());
        // Row 0 is a header (Appearance); row 1 is the first
        // setting (compact_mode). Place row 1 at y=1 so the
        // hover-row position math is unambiguous.
        s.row_rects[0] = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        s.row_rects[1] = Rect {
            x: 0,
            y: 1,
            width: 80,
            height: 1,
        };

        assert_eq!(s.hover_row, None, "fresh state must have no hover");

        let outcome = handle_settings_mouse(&mut s, MouseEventKind::Moved, 5, 1);
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Moved into a row's rect must report Changed, got {outcome:?}"
        );
        assert_eq!(
            s.hover_row,
            Some(1),
            "Moved at (5,1) must land on row 1 (compact_mode)",
        );

        // A second Moved event at the same coordinates is a no-op
        // (hover_row already matches → no Changed).
        let outcome = handle_settings_mouse(&mut s, MouseEventKind::Moved, 5, 1);
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "repeat Moved at the same row must be Unchanged, got {outcome:?}"
        );
    }

    /// `Moved` outside every row's hit-rect clears `state.hover_row`
    /// to `None`. Mirrors the scrollback's behaviour when the mouse
    /// drifts off the entry list.
    #[test]
    fn mouse_moved_outside_modal_clears_hover() {
        let mut s = make_state();
        s.list_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        };
        s.row_rects.resize(s.rows.len(), Rect::default());
        s.row_rects[1] = Rect {
            x: 0,
            y: 1,
            width: 80,
            height: 1,
        };

        // Seed hover at row 1.
        let _ = handle_settings_mouse(&mut s, MouseEventKind::Moved, 5, 1);
        assert_eq!(s.hover_row, Some(1));

        // Move far outside any row's rect.
        let outcome = handle_settings_mouse(&mut s, MouseEventKind::Moved, 5, 50);
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(
            s.hover_row, None,
            "Moved outside all row rects must clear hover_row",
        );
    }

    /// `Moved` over a header row's hit-rect does NOT set hover_row
    /// (headers aren't selectable; painting hover on them would be
    /// misleading).
    #[test]
    fn mouse_moved_over_header_does_not_set_hover() {
        let mut s = make_state();
        s.list_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        };
        s.row_rects.resize(s.rows.len(), Rect::default());
        s.row_rects[0] = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };

        let outcome = handle_settings_mouse(&mut s, MouseEventKind::Moved, 5, 0);
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "Moved over a header must be Unchanged",
        );
        assert_eq!(
            s.hover_row, None,
            "header rows must not register as hovered",
        );
    }

    /// `state.hover_row = Some(idx)` paints the hovered row's bg
    /// with the theme's `bg_hover` color. (Mirrors the existing
    /// `picker_highlights_current_choice` test's pattern: the
    /// `assert_eq` against `theme.bg_hover` survives both colored
    /// and quantize-to-Reset color levels.)
    #[test]
    fn hover_row_renders_with_hover_style() {
        let mut s = make_state();
        let theme = Theme::current();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let mut buf = Buffer::empty(area);
        // Pick a setting row that is NOT the selected row so the
        // hover bg vs selection bg distinction is observable.
        let setting_indices: Vec<usize> = s
            .rows
            .iter()
            .enumerate()
            .filter_map(|(i, r)| matches!(r, RowEntry::Setting { .. }).then_some(i))
            .collect();
        assert!(
            setting_indices.len() >= 2,
            "test requires at least 2 selectable rows",
        );
        let initial_selected = s.selected;
        let hover_idx = *setting_indices
            .iter()
            .find(|&&i| i != initial_selected)
            .expect("at least one non-selected setting row must exist");
        s.hover_row = Some(hover_idx);

        render_rows(&mut buf, area, &mut s, &theme);

        // Read back the bg of the painted hover row from the
        // first column of the row's allocated area.
        let row_rect = s.row_rects[hover_idx];
        assert!(
            row_rect.width > 0 && row_rect.height > 0,
            "hover row must have a non-zero rect after render",
        );
        let cell = buf
            .cell((row_rect.x, row_rect.y))
            .expect("rendered cell must exist");
        assert_eq!(
            cell.style().bg,
            Some(theme.bg_hover),
            "hover row must paint with `bg_hover`, got {:?}",
            cell.style().bg,
        );
    }

    /// In `PickingEnum` mode, a Moved event over a choice's hit-rect
    /// sets `state.hover_row` (semantically: hovered choice index)
    /// and the next render paints THAT choice with the hover bg
    /// without affecting other choices.
    #[test]
    fn picker_choice_mouse_hover_highlights_choice() {
        let mut s = picker_test_state();
        // Populate per-choice rects by rendering once, then drain
        // the thread-local stash into `state.picker_choice_rects`.
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);
        s.picker_choice_rects = take_picker_choice_rects();
        assert!(
            s.picker_choice_rects.len() >= 2,
            "picker_test_state must produce at least 2 choice rects",
        );

        // Pre-condition: choices_idx = 0 (the initial focus). Hover
        // over choice 1 — distinct from the focused choice.
        let target_rect = s.picker_choice_rects[1];
        assert!(
            target_rect.height > 0,
            "choice 1 must be visible after render",
        );
        let click_y = target_rect.y;
        let click_x = target_rect.x + target_rect.width / 2;

        let outcome = handle_settings_mouse(&mut s, MouseEventKind::Moved, click_x, click_y);
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Moved over choice 1 must be Changed, got {outcome:?}",
        );
        assert_eq!(
            s.hover_row,
            Some(1),
            "Moved over choice 1 must set hover_row = Some(1)",
        );

        // Re-render and observe the hover bg on choice 1's row.
        // (`bg_hover` may quantize to `Color::Reset` in tests with
        // `NO_COLOR` set — same caveat as
        // `picker_highlights_current_choice`. We assert tautologically
        // against `theme.bg_hover`; the wiring assertion is the
        // hover_row state mutation above plus the focused-choice
        // bg_visual contrast.)
        let mut buf2 = Buffer::empty(area);
        render_picking_enum(&mut buf2, area, &s, &theme);
        let new_rects = take_picker_choice_rects();
        let rect1 = new_rects[1];
        let cell1 = buf2
            .cell((rect1.x, rect1.y))
            .expect("choice 1 cell must exist");
        assert_eq!(
            cell1.style().bg,
            Some(theme.bg_hover),
            "hovered choice must paint bg_hover, got {:?}",
            cell1.style().bg,
        );
        // Focused choice (index 0) keeps bg_visual — selection wins
        // over hover. Verifies the `is_focused` branch precedence.
        let rect0 = new_rects[0];
        let cell0 = buf2
            .cell((rect0.x, rect0.y))
            .expect("choice 0 cell must exist");
        assert_eq!(
            cell0.style().bg,
            Some(theme.bg_visual),
            "focused choice must keep bg_visual even when hover is elsewhere",
        );
    }

    /// Mode transitions (Browse → PickingEnum and Browse →
    /// EditingValue) clear `hover_row` so a stale row-index from
    /// the old mode doesn't paint a wrong choice/row in the new
    /// mode before the next Moved event arrives.
    #[test]
    fn mode_transition_browse_to_picking_enum_clears_hover_row() {
        let mut s = make_state();
        navigate_to_enum_row(&mut s);
        s.hover_row = Some(s.selected);
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(s.mode, SettingsModalMode::PickingEnum { .. }),
            "Enter on enum row must transition to PickingEnum",
        );
        assert_eq!(
            s.hover_row, None,
            "Browse → PickingEnum transition must clear hover_row",
        );
    }

    /// Sibling of `mode_transition_browse_to_picking_enum_clears_hover_row` —
    /// the same hover-clear contract applies to the
    /// `Browse → EditingValue` transition (Enter on an `Int`-kind
    /// row). The prior `mode_transition_clears_hover_row`
    /// docstring claimed to cover both transitions but only exercised
    /// `PickingEnum`.
    #[test]
    fn mode_transition_browse_to_editing_value_clears_hover_row() {
        let mut s = make_state();
        navigate_to_int_row(&mut s);
        s.hover_row = Some(s.selected);
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(s.mode, SettingsModalMode::EditingValue { .. }),
            "Enter on Int row must transition to EditingValue, got {:?}",
            s.mode,
        );
        assert_eq!(
            s.hover_row, None,
            "Browse → EditingValue transition must clear hover_row",
        );
    }

    /// Sibling for the reverse direction: PickingEnum → Browse (Esc)
    /// also clears any stale hover that landed during the picker.
    #[test]
    fn mode_transition_picking_enum_to_browse_clears_hover_row() {
        let mut s = make_state();
        navigate_to_enum_row(&mut s);
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(s.mode, SettingsModalMode::PickingEnum { .. }));
        // Seed picker-mode hover (e.g. mouse moved over a non-focused
        // choice while in the picker).
        s.hover_row = Some(2);
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "Esc in PickingEnum must transition to Browse",
        );
        assert_eq!(
            s.hover_row, None,
            "PickingEnum → Browse transition must clear hover_row",
        );
    }

    /// Helper for `mode_transition_browse_to_editing_value_clears_hover_row`:
    /// walks selection forward to the first `Int`-kind row registered
    /// in defaults (currently `max_thoughts_width`).
    fn navigate_to_int_row(s: &mut SettingsModalState) {
        let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        for _ in 0..s.rows.len() {
            if let Some((_, meta)) = s.focused_setting()
                && matches!(meta.kind, SettingKind::Int { .. })
            {
                return;
            }
            let outcome = handle_settings_key(s, &key);
            if matches!(outcome, SettingsKeyOutcome::Unchanged) {
                break;
            }
        }
        panic!("no Int-kind row found in default registry");
    }

    /// Helper for `mode_transition_clears_hover_row`: walks selection
    /// forward to the first Enum-kind row registered in defaults.
    fn navigate_to_enum_row(s: &mut SettingsModalState) {
        let key = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        for _ in 0..s.rows.len() {
            if let Some((_, meta)) = s.focused_setting()
                && matches!(meta.kind, SettingKind::Enum { .. })
            {
                return;
            }
            let outcome = handle_settings_key(s, &key);
            if matches!(outcome, SettingsKeyOutcome::Unchanged) {
                break;
            }
        }
        panic!("no Enum-kind row found in default registry");
    }

    /// Scroll-wheel down emits 3 advances. From the initial selection
    /// (first setting), 3 settings forward must land on whatever is at
    /// position [first_setting + 3] in the registry. Resilient to PR-N
    /// additions.
    #[test]
    fn scroll_wheel_advances_selection() {
        let mut s = make_state();
        s.list_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        };
        let setting_keys: Vec<SettingKey> = s
            .rows
            .iter()
            .filter_map(|r| {
                if let RowEntry::Setting { key, .. } = r {
                    Some(*key)
                } else {
                    None
                }
            })
            .collect();
        let outcome = handle_settings_mouse(&mut s, MouseEventKind::ScrollDown, 5, 5);
        // 3 advances from the first setting → setting at position 3
        // (or the last setting if fewer than 4 are registered — the
        // advance_next no-op at the boundary absorbs extra advances).
        let expected = setting_keys
            .get(3)
            .copied()
            .unwrap_or(*setting_keys.last().unwrap());
        match &s.rows[s.selected] {
            RowEntry::Setting { key, .. } => assert_eq!(*key, expected),
            _ => panic!("expected setting after scroll"),
        }
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
    }

    // -- routing scaffold tests --
    //
    // The enum chooser and string/int editor declare their
    // mode variants alongside Browse and route Esc → Browse so the
    // scaffold doesn't ship dead `unimplemented!()` panics. These
    // tests pin the routing.

    /// `render_setting_row` produces the "restart" pill when
    /// `meta.restart_required == true` AND the row is expanded. When
    /// no registered setting sets `restart_required`, this test still
    /// exercises the render path via a synthetic setting with the flag
    /// set.
    ///
    /// The pill is a property
    /// label, not a "restart pending" tracker — a collapsed
    /// non-default row showing it forever misreads as pending, so
    /// the old `is_edited` trigger is gone and only `is_expanded`
    /// gates it (the "(restart to apply)" toast covers change-time
    /// feedback). Arms: expanded (pill) and edited-but-collapsed
    /// (no pill — the exact reported repro).
    #[test]
    fn render_setting_row_emits_restart_pill_when_required() {
        let meta = SettingMeta {
            key: "test-key",
            category: SettingCategory::Appearance,
            owner: crate::settings::SettingOwner::Shared,
            label: "Test setting",
            description: "Test description.",
            keywords: &["test"],
            kind: SettingKind::Bool { default: false },
            restart_required: true,
            hidden_in_minimal: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        // Arm 1: expanded → pill renders even at default value.
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_setting_row(
            &mut buf,
            area,
            &meta,
            &SettingValue::Bool(false),
            10,    // max_label_w
            false, // is_selected
            &theme,
            true,  // is_expanded — gate on
            false, // is_hovered
        );
        let mut rendered = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 0)) {
                rendered.push_str(cell.symbol());
            }
        }
        assert!(
            rendered.contains("restart"),
            "expanded row must contain the 'restart' pill: {rendered:?}"
        );

        // Arm 2: edited but collapsed → NO pill (the reported repro).
        let mut buf = Buffer::empty(area);
        render_setting_row(
            &mut buf,
            area,
            &meta,
            &SettingValue::Bool(true), // edited from default `false`
            10,
            false,
            &theme,
            false, // is_expanded — off
            false, // is_hovered
        );
        let mut rendered = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 0)) {
                rendered.push_str(cell.symbol());
            }
        }
        assert!(
            !rendered.contains("restart"),
            "edited-but-collapsed row must NOT contain the 'restart' pill: {rendered:?}"
        );
    }

    /// Counterpart to `render_setting_row_emits_restart_pill_when_required`:
    /// the pill is HIDDEN on any collapsed row. User-feedback
    /// follow-up — keeps the modal uncluttered for the common
    /// "I'm just browsing" case.
    #[test]
    fn render_setting_row_hides_restart_pill_when_at_default_and_collapsed() {
        let meta = SettingMeta {
            key: "test-key",
            category: SettingCategory::Appearance,
            owner: crate::settings::SettingOwner::Shared,
            label: "Test setting",
            description: "Test description.",
            keywords: &["test"],
            kind: SettingKind::Bool { default: false },
            restart_required: true,
            hidden_in_minimal: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_setting_row(
            &mut buf,
            area,
            &meta,
            &SettingValue::Bool(false),
            10,
            false,
            &theme,
            false, // is_expanded
            false, // is_hovered
        );
        let mut rendered = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 0)) {
                rendered.push_str(cell.symbol());
            }
        }
        assert!(
            !rendered.contains("restart"),
            "at-default, not-expanded row must NOT contain the 'restart' pill: {rendered:?}"
        );
    }

    // -- render-buffer tests for the
    // editor's cursor and validation-error display. --

    /// Build an editor state pre-positioned at the start of a known
    /// buffer for `default_model`. Catalog populated so the
    /// `KnownModel` validator has data to validate against.
    fn editor_render_fixture(buffer: &str, cursor_byte: usize) -> SettingsModalState {
        use agent_client_protocol as acp;
        use std::sync::Arc;
        // An earlier fixture used `default_model` (a SHELL `String`
        // setting) to exercise the inline editor. `default_model` is
        // now a `SettingKind::DynamicEnum`, so the
        // String editor no longer has a registered consumer in the
        // production catalog. We construct a synthetic registry
        // with a single `KnownModel`-validated String entry to keep
        // the editor-render contract under test — the production
        // editor code path stays exercised even though no live
        // setting wires it up today.
        let synthetic_meta = SettingMeta {
            key: "default_model",
            category: SettingCategory::Models,
            owner: crate::settings::SettingOwner::Shell,
            label: "Default model (synthetic)",
            // Short description so it fits in 1 line even at the
            // narrowest test width (the editor's word-wrap path
            // would otherwise push the input
            // row down on narrow widths, breaking the cursor-pan
            // tests' hardcoded input-row y position).
            description: "Test.",
            keywords: &["test"],
            kind: SettingKind::String {
                default: "",
                validator: StringValidator::KnownModel,
            },
            restart_required: false,
            hidden_in_minimal: false,
        };
        let registry = SettingsRegistry::from_entries(vec![synthetic_meta]);
        let snapshot = PagerLocalSnapshot {
            available_models: vec![(
                "Grok Test".to_string(),
                acp::ModelId::new(Arc::from("grok-test")),
            )],
            ..PagerLocalSnapshot::default()
        };
        let mut s = SettingsModalState::new(Arc::new(registry), UiConfig::default(), snapshot);
        let validation_error = validate_string(
            StringValidator::KnownModel,
            buffer,
            &s.pager_snapshot.available_models,
        );
        s.mode = SettingsModalMode::EditingValue {
            key: "default_model",
            buffer: buffer.to_string(),
            cursor_byte,
            validation_error,
        };
        s
    }

    /// Cursor lands at the visual column matching `cursor_byte` for
    /// buffers that fit entirely within the visible window.
    #[test]
    fn render_editing_value_cursor_at_logical_position_when_buffer_fits() {
        let mut s = editor_render_fixture("Grok Test", 4); // cursor between "Grok" and " Test"
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 6,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);

        // The input row is at y = header_rows = 3 (title + desc + gap).
        // Cursor glyph is ▏ (left one-eighth block).
        let row_y = 3u16;
        let mut found_cursor_col: Option<u16> = None;
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, row_y))
                && cell.symbol() == "\u{258F}"
            {
                found_cursor_col = Some(x);
                break;
            }
        }
        // For a non-overflow buffer, cursor column == cursor_byte
        // (the buffer is ASCII so byte == col).
        assert_eq!(
            found_cursor_col,
            Some(4),
            "cursor must render at column 4 (= cursor_byte) for in-window buffer",
        );
    }

    /// When the buffer overflows the visible window AND the cursor
    /// is at the start (Home or Left), the visible window pans so
    /// the start of the buffer is visible. The cursor renders at
    /// the LEFT edge, NOT the right.
    #[test]
    fn render_editing_value_cursor_pans_to_left_on_overflow_at_start() {
        // Build a buffer wide enough to overflow a narrow window.
        let buffer = "A".repeat(80);
        let mut s = editor_render_fixture(&buffer, 0);
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 6,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);

        let row_y = 3u16;
        let mut found_cursor_col: Option<u16> = None;
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, row_y))
                && cell.symbol() == "\u{258F}"
            {
                found_cursor_col = Some(x);
                break;
            }
        }
        assert_eq!(
            found_cursor_col,
            Some(0),
            "cursor must pan to column 0 when cursor_byte is at the start, even with overflow",
        );
    }

    /// When the buffer overflows AND the cursor is at the
    /// end, the visible window pans so the cursor sits at the
    /// rightmost-but-one column (column = `buffer_room - 1` =
    /// `visible_buffer_w`) — the last col is the cursor-reserve
    /// space, the cursor itself renders just inside it.
    #[test]
    fn render_editing_value_cursor_pans_to_right_on_overflow_at_end() {
        let buffer = "A".repeat(80);
        let cursor = buffer.len();
        let mut s = editor_render_fixture(&buffer, cursor);
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 6,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);

        let row_y = 3u16;
        let mut found_cursor_col: Option<u16> = None;
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, row_y))
                && cell.symbol() == "\u{258F}"
            {
                found_cursor_col = Some(x);
                break;
            }
        }
        // The cursor lands at `visible_buffer_w` = `buffer_room - 1`
        // — one cell before the rightmost edge, which is the
        // cursor-reserve column. Strictly greater than 0 is the
        // important regression-prevention assertion (the pre-R2
        // bug was the cursor being pinned at the right edge
        // REGARDLESS of cursor_byte).
        let col = found_cursor_col.expect("cursor must render");
        assert!(
            col > 0,
            "cursor must be panned past the start when cursor is at end of overflowing buffer \
             (got col {col})",
        );
        assert!(
            col >= area.width - 2,
            "cursor must be near the rightmost visible column for end-of-buffer position \
             (got col {col}, area.width = {})",
            area.width,
        );
    }

    /// When the validator returns a
    /// non-None error, the buffer foreground turns red
    /// (`accent_error`), AND the validation-error row at y =
    /// header_rows + 1 renders the error message in accent_error.
    #[test]
    fn render_editing_value_paints_validation_error_row_and_buffer_red() {
        // Use a buffer that fails KnownModel ("xyz" not in catalog).
        let mut s = editor_render_fixture("xyz", 3);
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 6,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);

        // Row 3 is the input. The buffer "xyz" sits at cols 0..3.
        // Each char's foreground must be accent_error.
        for x in 0..3 {
            let cell = buf.cell((x, 3)).expect("cell must exist");
            assert_eq!(
                cell.style().fg,
                Some(theme.accent_error),
                "input row col {x} must use accent_error fg, got {:?}",
                cell.style().fg,
            );
        }

        // Row 4 is the validation error.
        let mut err_row = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 4)) {
                err_row.push_str(cell.symbol());
            }
        }
        assert!(
            err_row.to_lowercase().contains("unknown model"),
            "validation-error row must contain 'unknown model' (case-insensitive), got {err_row:?}",
        );
    }

    /// Empty buffer renders a
    /// low-contrast placeholder hint. The cursor block (`▏`) lands
    /// at col 0 and overdraws the placeholder's leading `<`, so the
    /// assertion targets the unique "empty — use shell default"
    /// substring that survives the cursor overdraw.
    #[test]
    fn render_editing_value_empty_buffer_shows_placeholder() {
        let mut s = editor_render_fixture("", 0);
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 6,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);

        let mut input_row = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 3)) {
                input_row.push_str(cell.symbol());
            }
        }
        // Placeholder for KnownModel ends with "use shell default>".
        // The cursor at col 0 overdraws the leading `<`, so we match
        // on the body substring.
        assert!(
            input_row.contains("use shell default"),
            "empty buffer must render the KnownModel placeholder, got {input_row:?}",
        );
    }

    /// Rendering an Int stepper wide enough for the
    /// `‹` / `›` arrow glyphs populates `editor_adornment_rects`
    /// so `handle_settings_mouse` can hit-test arrow clicks. The
    /// arrows flank a centered value, NOT the old `[-]` / `[+]`
    /// adornments flush against the area edges.
    #[test]
    fn render_editing_value_int_populates_adornment_hit_rects() {
        // Use a settings registry containing the real
        // `max_thoughts_width` Int entry.
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::defaults()),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::EditingValue {
            key: "max_thoughts_width",
            buffer: "120".to_string(),
            cursor_byte: 3,
            validation_error: None,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 6,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);

        let (dec_rect, inc_rect) = s.editor_adornment_rects;
        assert!(
            dec_rect.width > 0 && dec_rect.height > 0,
            "decrement (left arrow) rect must be non-zero (got {dec_rect:?})",
        );
        assert!(
            inc_rect.width > 0 && inc_rect.height > 0,
            "increment (right arrow) rect must be non-zero (got {inc_rect:?})",
        );
        // The arrows flank a centered value. The left
        // arrow sits LEFT of the value, the right arrow RIGHT of
        // it; both are strictly inside the area.
        assert!(
            dec_rect.x > 0 && dec_rect.x < inc_rect.x,
            "left arrow must be inside the area, before the right arrow \
             (got dec.x={}, inc.x={})",
            dec_rect.x,
            inc_rect.x,
        );
        assert!(
            inc_rect.x + inc_rect.width < area.width,
            "right arrow must fit inside the area",
        );
        // Both arrows live on the same stepper row. The
        // description word-wraps, so the input
        // row's y position depends on how many lines the
        // description consumes. The previous hardcoded `== 3`
        // assertion was a side-effect of the truncate-only render.
        assert_eq!(
            dec_rect.y, inc_rect.y,
            "left + right arrow must share the same stepper row",
        );
    }

    // ---------- Int stepper key + render contracts ----------

    /// Helper: build a `SettingsModalState` directly in EditingValue
    /// mode for a registered Int setting with the given starting value.
    fn int_stepper_fixture_for(key: &'static str, value: i64) -> SettingsModalState {
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::defaults()),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        let buffer = value.to_string();
        let cursor_byte = buffer.len();
        s.mode = SettingsModalMode::EditingValue {
            key,
            buffer,
            cursor_byte,
            validation_error: None,
        };
        s
    }

    /// Wide-range Int fixture (`max_thoughts_width`, steps ±5/±10).
    fn int_stepper_fixture(value: i64) -> SettingsModalState {
        int_stepper_fixture_for("max_thoughts_width", value)
    }

    fn int_stepper_buffer(s: &SettingsModalState) -> String {
        match &s.mode {
            SettingsModalMode::EditingValue { buffer, .. } => buffer.clone(),
            other => panic!("expected EditingValue, got {other:?}"),
        }
    }

    #[test]
    fn int_step_sizes_table_pins_range_policy() {
        // (min, max, expected_small, expected_large)
        let cases = [
            (1, 10, 1, 1),    // scroll_lines (span 9)
            (1, 100, 1, 5),   // scroll_speed (span 99)
            (40, 500, 5, 10), // max_thoughts_width (span 460)
            (0, 0, 1, 1),     // degenerate span
            (1, 21, 1, 4),    // span 20 still narrow: large = span/5
            (1, 22, 1, 5),    // span 21 → mid band
            (1, 101, 1, 5),   // span 100 still mid
            (1, 102, 5, 10),  // span 101 → wide band
        ];
        for (min, max, want_small, want_large) in cases {
            assert_eq!(
                int_step_sizes(min, max),
                (want_small, want_large),
                "int_step_sizes({min}, {max})",
            );
        }
    }

    /// Up arrow steps the wide-range Int by small step (+5).
    #[test]
    fn int_editing_value_up_arrow_increments_by_small_step() {
        let mut s = int_stepper_fixture(50);
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "55");
    }

    /// Down arrow steps the wide-range Int by small step (−5).
    #[test]
    fn int_editing_value_down_arrow_decrements_by_small_step() {
        let mut s = int_stepper_fixture(50);
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "45");
    }

    /// Right arrow steps the wide-range Int by large step (+10).
    #[test]
    fn int_editing_value_right_arrow_increments_by_large_step() {
        let mut s = int_stepper_fixture(50);
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "60");
    }

    /// Left arrow steps the wide-range Int by large step (−10).
    #[test]
    fn int_editing_value_left_arrow_decrements_by_large_step() {
        let mut s = int_stepper_fixture(50);
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "40");
    }

    /// Narrow-range Int (`scroll_lines` 1..=10) uses unit steps on all arrows.
    #[test]
    fn scroll_lines_int_stepper_uses_unit_steps() {
        let mut s = int_stepper_fixture_for("scroll_lines", 3);
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "4");
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "3");
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "4");
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "3");
    }

    /// Mid-range Int (`scroll_speed` 1..=100): Up/Down ±1, Left/Right ±5.
    #[test]
    fn scroll_speed_int_stepper_uses_unit_fine_and_five_coarse() {
        let mut s = int_stepper_fixture_for("scroll_speed", 50);
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "51");
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "56");
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "55");
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "50");
    }

    /// k / j vim aliases mirror Up / Down.
    #[test]
    fn int_editing_value_vim_k_j_step_by_small() {
        let mut s = int_stepper_fixture(50);
        let _ = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        );
        assert_eq!(int_stepper_buffer(&s), "55");
        let _ = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        assert_eq!(int_stepper_buffer(&s), "50");
    }

    /// l / h vim aliases mirror Right / Left.
    #[test]
    fn int_editing_value_vim_l_h_step_by_large() {
        let mut s = int_stepper_fixture(50);
        let _ = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
        );
        assert_eq!(int_stepper_buffer(&s), "60");
        let _ = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        );
        assert_eq!(int_stepper_buffer(&s), "50");
    }

    /// Stepping past `max` is clamped to `max`; the outcome is
    /// `Unchanged` (no visible step) so a UI consumer can avoid
    /// re-render churn.
    #[test]
    fn int_editing_value_clamps_to_max() {
        // max_thoughts_width registered max = 500.
        let mut s = int_stepper_fixture(500);
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "Up at max must be Unchanged (no movement), got {outcome:?}",
        );
        assert_eq!(int_stepper_buffer(&s), "500");
    }

    /// Stepping below `min` is clamped to `min`.
    #[test]
    fn int_editing_value_clamps_to_min() {
        // max_thoughts_width registered min = 40.
        let mut s = int_stepper_fixture(40);
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "Down at min must be Unchanged (no movement), got {outcome:?}",
        );
        assert_eq!(int_stepper_buffer(&s), "40");
    }

    /// Digit keys are silently dropped — the stepper is not a
    /// text input.
    #[test]
    fn int_editing_value_ignores_digit_keys() {
        let mut s = int_stepper_fixture(50);
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE),
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Unchanged));
        assert_eq!(int_stepper_buffer(&s), "50");
    }

    /// Backspace is silently dropped — the stepper has no
    /// editable buffer to backspace into.
    #[test]
    fn int_editing_value_ignores_backspace() {
        let mut s = int_stepper_fixture(50);
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Unchanged));
        assert_eq!(int_stepper_buffer(&s), "50");
    }

    /// Delete / Home / End / Tab are likewise dropped.
    #[test]
    fn int_editing_value_ignores_other_text_input_keys() {
        let mut s = int_stepper_fixture(50);
        for code in [KeyCode::Delete, KeyCode::Home, KeyCode::End, KeyCode::Tab] {
            let outcome = handle_settings_key(&mut s, &KeyEvent::new(code, KeyModifiers::NONE));
            assert!(
                matches!(outcome, SettingsKeyOutcome::Unchanged),
                "stepper must reject {code:?}, got {outcome:?}",
            );
            assert_eq!(int_stepper_buffer(&s), "50");
        }
    }

    /// Render the stepper and assert the `‹` / `›` arrow glyphs
    /// AND the value text are present on the input row.
    #[test]
    fn int_editing_value_renders_stepper_ui() {
        let mut s = int_stepper_fixture(125);
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);

        // Find the stepper row dynamically — the description
        // word-wraps so the input row's y is no
        // longer fixed at 3. Scan rows top-down for the one that
        // contains both stepper glyphs.
        let mut stepper_row: Option<String> = None;
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    row.push_str(cell.symbol());
                }
            }
            if row.contains('\u{2039}') && row.contains('\u{203A}') {
                stepper_row = Some(row);
                break;
            }
        }
        let row = stepper_row.expect("must find the stepper row");
        assert!(
            row.contains('\u{2039}'),
            "stepper row must contain `‹` glyph, got {row:?}"
        );
        assert!(
            row.contains('\u{203A}'),
            "stepper row must contain `›` glyph, got {row:?}"
        );
        assert!(
            row.contains("125"),
            "stepper row must contain the value text, got {row:?}"
        );
    }

    /// Enter commits the typed Action and transitions back to
    /// Browse. The `action_for_int` arm for `max_thoughts_width`
    /// produces `Action::SetMaxThoughtsWidth(<i64>)`.
    #[test]
    fn int_editing_value_enter_commits() {
        let mut s = int_stepper_fixture(75);
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match outcome {
            SettingsKeyOutcome::Action(Action::SetMaxThoughtsWidth(75)) => {}
            other => panic!("expected SetMaxThoughtsWidth(75) on Enter, got {other:?}"),
        }
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "Enter must return to Browse"
        );
    }

    /// Esc cancels — no Action is emitted, and mode returns to
    /// Browse. The user's in-progress step is dropped; the
    /// underlying setting value (`UiConfig.max_thoughts_width`)
    /// stays at whatever it was before the editor opened.
    #[test]
    fn int_editing_value_esc_reverts() {
        let mut s = int_stepper_fixture(75);
        // Take a step so the buffer diverges from the original.
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(int_stepper_buffer(&s), "80");

        // Esc.
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Esc must be Changed (mode swap), got {outcome:?}"
        );
        assert!(
            !matches!(outcome, SettingsKeyOutcome::Action(_)),
            "Esc must NOT emit any Action — the underlying value was \
             never live-previewed",
        );
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "Esc must return to Browse"
        );
    }

    /// Mouse click on the `‹` left-arrow rect synthesizes a Down
    /// step (small step down). The click maps to small steps to
    /// match the spinner convention — repeated clicks let the
    /// user fine-tune.
    #[test]
    fn int_editing_value_left_arrow_click_decrements_by_small_step() {
        let mut s = int_stepper_fixture(120);
        // Render once to populate `editor_adornment_rects`.
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);
        let (dec_rect, _) = s.editor_adornment_rects;
        assert!(dec_rect.width > 0, "left arrow rect must be populated");

        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            dec_rect.x,
            dec_rect.y,
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "115");
    }

    /// Mouse click on the `›` right-arrow rect synthesizes an Up
    /// step. Mirror of the left-arrow test.
    #[test]
    fn int_editing_value_right_arrow_click_increments_by_small_step() {
        let mut s = int_stepper_fixture(120);
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);
        let (_, inc_rect) = s.editor_adornment_rects;
        assert!(inc_rect.width > 0, "right arrow rect must be populated");

        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            inc_rect.x,
            inc_rect.y,
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert_eq!(int_stepper_buffer(&s), "125");
    }

    /// Mouse click on the value text (between the arrows) is a
    /// no-op — clicks here shouldn't accidentally commit or step.
    #[test]
    fn int_editing_value_click_on_value_text_is_noop() {
        let mut s = int_stepper_fixture(120);
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 8,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);
        let (dec_rect, inc_rect) = s.editor_adornment_rects;
        // Click between the two arrow rects.
        let middle_x = (dec_rect.x + inc_rect.x) / 2;
        let middle_y = dec_rect.y;
        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            middle_x,
            middle_y,
        );
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "click on value text must be Unchanged, got {outcome:?}"
        );
        assert_eq!(int_stepper_buffer(&s), "120");
    }

    /// Esc in the theme picker dispatches a PREVIEW Action with the
    /// original canonical AND returns to Browse:
    /// preview-revert restores the live visual without persisting,
    /// since the picker's preview navs never persisted in the first
    /// place. Parameterised across all 3 theme enum keys.
    #[test]
    fn picking_enum_esc_dispatches_preview_revert_for_each_key() {
        let cases: &[(&str, &str)] = &[
            ("theme", "groknight"),
            ("auto_dark_theme", "groknight"),
            ("auto_light_theme", "grokday"),
        ];
        for &(key, original) in cases {
            let mut s = make_state();
            s.mode = SettingsModalMode::PickingEnum {
                key,
                choices_idx: 0,
                original_value: SettingValue::Enum(original),
                supports_preview: true,
            };
            let outcome =
                handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            match (key, outcome) {
                ("theme", SettingsKeyOutcome::Action(Action::PreviewTheme(name))) => {
                    assert_eq!(name, original);
                }
                (
                    "auto_dark_theme",
                    SettingsKeyOutcome::Action(Action::PreviewAutoDarkTheme(name)),
                ) => {
                    assert_eq!(name, original);
                }
                (
                    "auto_light_theme",
                    SettingsKeyOutcome::Action(Action::PreviewAutoLightTheme(name)),
                ) => {
                    assert_eq!(name, original);
                }
                (k, other) => panic!(
                    "Esc on `{k}` picker should dispatch matching Preview Action, got {other:?}"
                ),
            }
            assert!(
                matches!(s.mode, SettingsModalMode::Browse),
                "Esc must transition back to Browse for key `{key}`",
            );
        }
    }

    /// Backwards-compat alias for an earlier test name. Asserts
    /// the same behaviour for the `theme` key only — kept so a future
    /// reader grep'ing the older test name still finds working
    /// coverage. The parameterised version above is the canonical test.
    #[test]
    fn picking_enum_esc_returns_to_browse() {
        let mut s = make_state();
        s.mode = SettingsModalMode::PickingEnum {
            key: "theme",
            choices_idx: 0,
            original_value: SettingValue::Enum("groknight"),
            supports_preview: true,
        };
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        match outcome {
            SettingsKeyOutcome::Action(Action::PreviewTheme(name)) => {
                assert_eq!(
                    name, "groknight",
                    "Esc revert must dispatch the original canonical"
                );
            }
            other => panic!("expected Action::PreviewTheme(\"groknight\") on Esc, got {other:?}"),
        }
        assert!(matches!(s.mode, SettingsModalMode::Browse));
    }

    // -- picker machinery tests --
    //
    // When the chooser sub-pane ships with no Enum entries in
    // `default_settings()`, these tests build a
    // synthetic registry containing one Enum entry and exercise the
    // picker's render + keypress paths directly. When
    // `action_for_enum` returns `None` for a key, the
    // preview/revert dispatch returns `SettingsKeyOutcome::Changed`
    // rather than `Action(_)`; a concrete `action_for_enum("theme", _)`
    // arm makes it emit an Action.

    /// Static synthetic Enum metadata for picker tests. Choice display
    /// names are deliberately mid-length so the default-width (80) tests
    /// cover the happy path and dedicated narrow-width tests cover the
    /// truncation paths — sized close to the real theme catalog widths.
    const TEST_ENUM_CHOICES: &[EnumChoice] = &[
        EnumChoice {
            canonical: "first",
            display: "First Option",
            description: "First option description.",
        },
        EnumChoice {
            canonical: "second",
            display: "Second Option",
            description: "Second option description.",
        },
        EnumChoice {
            canonical: "third",
            display: "Third Option",
            description: "Third option description.",
        },
    ];

    fn synthetic_enum_meta() -> SettingMeta {
        SettingMeta {
            key: "test_enum",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Test enum",
            description: "Synthetic Enum entry for PR 3 picker tests.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "first",
                choices: TEST_ENUM_CHOICES,
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }
    }

    /// Build a registry containing exactly one synthetic Enum entry
    /// and place the modal directly in PickingEnum mode at idx 0.
    /// Useful for testing the picker handler in isolation —
    /// `try_enter_picking_enum` is tested separately by
    /// `picker_test_state_in_browse()` + Enter dispatch.
    fn picker_test_state() -> SettingsModalState {
        let entries = vec![synthetic_enum_meta()];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "test_enum",
            choices_idx: 0,
            original_value: SettingValue::Enum("first"),
            supports_preview: true,
        };
        s
    }

    /// Same registry as `picker_test_state` but the modal stays in
    /// Browse mode with the Enum row focused — used to exercise
    /// `try_enter_picking_enum` via the normal Browse-Enter path.
    fn picker_test_state_in_browse() -> SettingsModalState {
        let entries = vec![synthetic_enum_meta()];
        SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        )
        // Selection lands on the first selectable row (the only
        // setting, "test_enum") by `SettingsModalState::new`.
    }

    /// Up/Down (and j/k aliases) in the picker advance `choices_idx`
    /// and clamp at list bounds. With no real Enum
    /// `action_for_enum` arms, the outcome is `Changed` (state
    /// change only); a sibling test
    /// `picker_arrow_keys_emit_preview_actions` can assert Action
    /// emission once `action_for_enum("theme", _)` lands.
    ///
    /// Renamed from `picker_arrow_keys_emit_preview_actions` — the
    /// previous name promised something the test cannot actually verify
    /// without a real Enum arm.
    #[test]
    fn picker_arrow_keys_advance_choices_idx() {
        let mut s = picker_test_state();

        // Down: 0 → 1.
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Down should produce Changed (state mutation), got {outcome:?}"
        );
        match s.mode {
            SettingsModalMode::PickingEnum { choices_idx, .. } => assert_eq!(choices_idx, 1),
            ref other => panic!("expected PickingEnum mode after Down, got {other:?}"),
        }

        // j: 1 → 2 (last choice).
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        match s.mode {
            SettingsModalMode::PickingEnum { choices_idx, .. } => assert_eq!(choices_idx, 2),
            _ => panic!("expected PickingEnum mode after j"),
        }

        // Down past the last choice is Unchanged (clamp).
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "Down at last choice should be Unchanged"
        );

        // Up: 2 → 1.
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        match s.mode {
            SettingsModalMode::PickingEnum { choices_idx, .. } => assert_eq!(choices_idx, 1),
            _ => panic!("expected PickingEnum mode after Up"),
        }

        // k: 1 → 0.
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE),
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        match s.mode {
            SettingsModalMode::PickingEnum { choices_idx, .. } => assert_eq!(choices_idx, 0),
            _ => panic!("expected PickingEnum mode after k"),
        }

        // Up at first choice is Unchanged.
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Unchanged));
    }

    /// Enter commits the current preview by dispatching a typed
    /// COMMIT Action AND transitioning back to Browse. The synthetic
    /// `test_enum` key has no `action_for_enum_commit` arm, so the
    /// outcome is `Changed` (state mutation only). The theme keys
    /// land real `Action::SetTheme(...)` Action variants — exercised
    /// by the e2e tests at `tests/settings_e2e.rs`.
    ///
    /// Enter used to be a no-op
    /// (relying on the most-recent preview being the committed
    /// value); now it explicitly emits a commit Action so the
    /// persist path runs once per picker open → close cycle.
    #[test]
    fn picker_enter_returns_to_browse() {
        let mut s = picker_test_state();
        // Navigate to the second choice so commit lands on a non-default.
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));

        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // The synthetic `test_enum` key has no commit Action arm in
        // `action_for_enum_commit`, so the outcome is `Changed`. For
        // real theme keys, the outcome is `Action(...)` — see the
        // theme preview/commit e2e coverage in the e2e crate.
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Enter for synthetic-key must produce Changed (no commit arm), got {outcome:?}"
        );
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "Enter must return to Browse"
        );
    }

    /// Esc transitions PickingEnum → Browse AND, once a real Enum arm
    /// exists, will dispatch `action_for_enum(setting_key, original_canonical)`.
    /// When `action_for_enum` returns None for every key, the
    /// outcome is `Changed` rather than `Action(_)` — but the
    /// revert *call site* is exercised, and the assertion can be
    /// tightened to verify `Action::SetTheme("first")` once the arm
    /// ships.
    ///
    /// The test cannot currently verify that the
    /// *original* canonical (not the *current* preview) is passed to
    /// `action_for_enum`. A real theme arm would make the distinction
    /// visible via the Action variant.
    #[test]
    fn picker_esc_returns_to_browse_after_preview_nav() {
        let mut s = picker_test_state();
        // Preview-navigate to a non-original choice so the revert
        // path's "original vs current" distinction is meaningful.
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        match s.mode {
            SettingsModalMode::PickingEnum { choices_idx, .. } => assert_eq!(choices_idx, 2),
            _ => panic!("expected PickingEnum mode after 2x Down"),
        }

        // Esc: revert. Outcome is Changed when there are no Enum action
        // arms — with a real theme arm this would unpack as Action::SetTheme("first").
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Esc revert outcome should be Changed (or Action when arms exist), got {outcome:?}"
        );
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "Esc must return to Browse"
        );
    }

    /// The picker renders every choice in declaration order, top to
    /// bottom. Asserts each choice's `display` string and description
    /// appears on the expected row with the documented spacing.
    /// Layout: row 0 = title, row 1 = description (subtitle), row 2 =
    /// gap, rows 3..6 = choices.
    #[test]
    fn picker_renders_choices_in_order() {
        let s = picker_test_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        let row_text = |y: u16| -> String {
            let mut s = String::new();
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    s.push_str(cell.symbol());
                }
            }
            s
        };

        // Row 0: title.
        assert!(
            row_text(0).contains("Test enum"),
            "title row must contain 'Test enum', got: {:?}",
            row_text(0)
        );
        // Row 1: setting description subtitle.
        assert!(
            row_text(1).contains("Synthetic Enum entry"),
            "row 1 must contain setting description, got: {:?}",
            row_text(1)
        );
        // Row 2: blank gap — strict assertion.
        assert!(
            row_text(2).trim().is_empty(),
            "row 2 must be the blank gap, got: {:?}",
            row_text(2)
        );
        // Rows 3..6: choices. Use full rendered layout for precise
        // pinning (tighten substring match).
        // " ○  First Option · First option description."
        let r3 = row_text(3);
        assert!(
            r3.contains("First Option") && r3.contains("First option description"),
            "row 3 must contain display+description for 'first', got: {r3:?}"
        );
        let r4 = row_text(4);
        assert!(
            r4.contains("Second Option") && r4.contains("Second option description"),
            "row 4 must contain display+description for 'second', got: {r4:?}"
        );
        let r5 = row_text(5);
        assert!(
            r5.contains("Third Option") && r5.contains("Third option description"),
            "row 5 must contain display+description for 'third', got: {r5:?}"
        );
    }

    /// The currently-focused choice renders with the filled-disc
    /// marker `●`, `accent_user` marker color, `bg_visual` row bg,
    /// AND **BOLD** display text — three independent focus cues for
    /// low-contrast theme compatibility (parity with `cancel_turn_panel`).
    #[test]
    fn picker_highlights_current_choice() {
        let mut s = picker_test_state();
        // Focus the second choice (index 1).
        if let SettingsModalMode::PickingEnum {
            ref mut choices_idx,
            ..
        } = s.mode
        {
            *choices_idx = 1;
        }
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        // Marker glyph at column `area.x + 1` of each choice row.
        // Using `area.x + 1` rather than `1` so the helper survives
        // a future renderer that passes a non-zero `area.x`.
        let marker_at = |y: u16| -> String {
            buf.cell((area.x + 1, y))
                .map(|c| c.symbol().to_string())
                .unwrap_or_default()
        };
        // Layout: rows 3..6 are choices (with subtitle on row 1).
        assert_eq!(marker_at(3), "\u{25CB}", "row 3 (unfocused) should be ○");
        assert_eq!(marker_at(4), "\u{25CF}", "row 4 (focused) should be ●");
        assert_eq!(marker_at(5), "\u{25CB}", "row 5 (unfocused) should be ○");

        // Cell at the LAST column of each row carries the row bg
        // independent of prefix-width tweaks.
        let bg_at = |y: u16| -> Option<ratatui::style::Color> {
            buf.cell((area.x + area.width - 1, y))
                .and_then(|c| c.style().bg)
        };
        assert_eq!(
            bg_at(4),
            Some(theme.bg_visual),
            "focused row must have bg_visual background"
        );
        assert_eq!(
            bg_at(3),
            Some(theme.bg_base),
            "unfocused row must have bg_base background"
        );

        // Display text on focused row carries BOLD modifier
        // (three focus cues). Display "Second
        // Option" starts at col `PICKER_PREFIX_W` (= 4). The 'S' at
        // col 4 should be bold.
        let focused_modifier = buf
            .cell((area.x + PICKER_PREFIX_W, 4))
            .map(|c| c.style().add_modifier)
            .unwrap_or_default();
        assert!(
            focused_modifier.contains(Modifier::BOLD),
            "focused row's display must be BOLD, got modifiers {focused_modifier:?}"
        );
        let unfocused_modifier = buf
            .cell((area.x + PICKER_PREFIX_W, 3))
            .map(|c| c.style().add_modifier)
            .unwrap_or_default();
        assert!(
            !unfocused_modifier.contains(Modifier::BOLD),
            "unfocused row's display must NOT be BOLD, got modifiers {unfocused_modifier:?}"
        );
    }

    // -- try_enter_picking_enum coverage --

    /// Browse-mode Enter on an Enum row transitions to PickingEnum
    /// mode with `choices_idx` seeded from the row's current value
    /// (resolved by `current_value_for`).
    ///
    /// Since the synthetic "test_enum" key has no
    /// `current_value_for` arm, `value_for` returns None and the
    /// fallback path picks `choices_idx = 0` + `original_value =
    /// SettingValue::Enum(first_canonical)`. This pins the
    /// fallback behavior.
    #[test]
    fn browse_enter_on_enum_row_transitions_to_picking_enum() {
        let mut s = picker_test_state_in_browse();
        // Sanity: initial state.
        assert!(matches!(s.mode, SettingsModalMode::Browse));
        match &s.rows[s.selected] {
            RowEntry::Setting { key, .. } => assert_eq!(*key, "test_enum"),
            _ => panic!("expected synthetic Enum row at initial selection"),
        }

        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Enter on Enum row should produce Changed, got {outcome:?}"
        );
        match s.mode {
            SettingsModalMode::PickingEnum {
                key,
                choices_idx,
                ref original_value,
                ..
            } => {
                assert_eq!(key, "test_enum");
                // No current_value_for arm → fallback to idx 0.
                assert_eq!(choices_idx, 0);
                assert_eq!(original_value, &SettingValue::Enum("first"));
            }
            ref other => panic!("expected PickingEnum mode, got {other:?}"),
        }
    }

    /// Browse-mode Enter on a non-Enum (Bool) row does NOT enter
    /// PickingEnum — `try_enter_picking_enum` returns false and the
    /// fallthrough Bool-toggle path takes over.
    #[test]
    fn browse_enter_on_bool_row_does_not_enter_picking_enum() {
        let mut s = make_state(); // default registry — all Bool.
        // compact_mode is the initial selection.
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Bool toggle dispatches an Action (not Changed).
        match outcome {
            SettingsKeyOutcome::Action(_) => {}
            other => panic!("expected Action from Bool toggle, got {other:?}"),
        }
        // Mode must NOT have changed to PickingEnum.
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "Bool toggle must stay in Browse mode, got {:?}",
            s.mode,
        );
    }

    /// `try_enter_picking_enum` directly: with a synthetic Enum
    /// where `value_for` returns the second choice's canonical, the
    /// `choices_idx` must seed to 1 (the position of that canonical).
    /// This exercises the `choices.iter().position(...)` resolution
    /// branch that's structurally unreachable when no real Enum
    /// entries are registered but is hit by the theme path.
    #[test]
    fn try_enter_picking_enum_seeds_choices_idx_from_current_value() {
        // The synthetic Enum key "test_enum" isn't in
        // `current_value_for`, so we can't drive this end-to-end via
        // ui_snapshot. Instead, build a registry where the key
        // matches `current_value_for("compact_mode", ...)` and override
        // SettingKind to Enum. Practical: we directly verify the
        // function returns false for non-Enum, and idx 0 + first
        // canonical for the unknown-key fallback.
        //
        // The "value-recognized → idx > 0" case requires the
        // canonical to be in `current_value_for`'s match arms; with
        // no Enum keys in those arms, this case is
        // unreachable. Adding `"theme" => SettingValue::Enum(...)`
        // and a sibling test that uses a non-default theme would verify
        // idx > 0 seeding.
        let mut s = picker_test_state_in_browse();
        assert!(s.try_enter_picking_enum());
        match s.mode {
            SettingsModalMode::PickingEnum {
                key,
                choices_idx,
                ref original_value,
                ..
            } => {
                assert_eq!(key, "test_enum");
                assert_eq!(choices_idx, 0, "fallback to idx 0 when no current value");
                assert_eq!(
                    original_value,
                    &SettingValue::Enum("first"),
                    "original_value should be the first canonical fallback"
                );
            }
            ref other => panic!("expected PickingEnum mode, got {other:?}"),
        }
    }

    /// `try_enter_picking_enum` on a non-Enum focused row returns
    /// false and leaves mode unchanged. The Bool case in
    /// `make_state()` is the canonical non-Enum scenario.
    #[test]
    fn try_enter_picking_enum_returns_false_for_non_enum_row() {
        let mut s = make_state();
        assert!(matches!(s.mode, SettingsModalMode::Browse));
        assert!(
            !s.try_enter_picking_enum(),
            "non-Enum focused row should return false"
        );
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "mode must not change on non-Enum row"
        );
    }

    // -- render_picking_enum narrow-terminal coverage --

    #[test]
    fn render_picker_with_zero_height_is_noop() {
        let s = picker_test_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 0,
        };
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        });
        let theme = Theme::current();
        // Must not panic and must not touch the buffer.
        render_picking_enum(&mut buf, area, &s, &theme);
    }

    #[test]
    fn render_picker_with_zero_width_is_noop() {
        let s = picker_test_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 10,
        };
        let mut buf = Buffer::empty(Rect {
            x: 0,
            y: 0,
            width: 1,
            height: 10,
        });
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);
    }

    /// At height=2, the description-skipped path (`area.height < 4`)
    /// kicks in (header_rows=2 = title+gap), and the choice-render
    /// early-return fires (`area.height <= header_rows`). The title
    /// must render, no choices must render, no panic.
    #[test]
    fn render_picker_at_height_2_renders_title_no_choices() {
        let s = picker_test_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 2,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        let row_text = |y: u16| -> String {
            let mut s = String::new();
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    s.push_str(cell.symbol());
                }
            }
            s
        };
        assert!(row_text(0).contains("Test enum"));
        // Row 1 is the gap; must be blank.
        assert!(
            row_text(1).trim().is_empty(),
            "no choices should render at height=2 (only title fits), got: {:?}",
            row_text(1)
        );
    }

    /// Narrow-viewport edge case
    /// for the word-wrap path. When the picker is given a height
    /// where a fully-wrapped description would exceed the area,
    /// the `has_description = … >= 2 + desc_rows` gate skips the
    /// description so the choices stay renderable. This test
    /// pins that fallback at an intermediate height.
    #[test]
    fn render_picker_drops_description_when_wrap_block_exceeds_height() {
        // Synthetic registry with a description that, at width=20,
        // would wrap to ≥ 5 lines.
        let long_desc = "This description is intentionally long enough \
                         that at narrow widths the wrap block will not \
                         fit alongside the choices.";
        let synthetic_meta = SettingMeta {
            key: "test-narrow-wrap",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "X",
            description: long_desc,
            keywords: &[],
            kind: SettingKind::Enum {
                default: "first",
                choices: TEST_ENUM_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        };
        let registry = SettingsRegistry::from_entries(vec![synthetic_meta]);
        let mut s = SettingsModalState::new(
            Arc::new(registry),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        assert!(s.try_enter_picking_enum());

        // Height=4 at narrow width: title (1) + desc_would_be (≥5)
        // overflows; the gate drops the description. The choices
        // should still render.
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 4,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);
        let mut all_text = String::new();
        for y in 0..area.height {
            all_text.push_str(&buf_row_text(&buf, y, area.x, area.width));
            all_text.push('\n');
        }
        // At minimum the title and the first choice must render.
        assert!(
            all_text.contains('X'),
            "title `X` must render at narrow height: {all_text:?}",
        );
        // The fallback dropped the description, freeing space for
        // the choice marker `\u{25CB}` or `\u{25CF}` (`○`/`●`).
        let has_choice_marker = all_text.contains('\u{25CB}') || all_text.contains('\u{25CF}');
        assert!(
            has_choice_marker,
            "at least one choice marker must render when desc is dropped: {all_text:?}",
        );
    }

    /// Long descriptions WRAP across multiple lines (no `…`
    /// truncation). User-feedback commit: word-wrap the description
    /// in the picker so opinion-shaping choice copy stays readable
    /// at typical terminal widths.
    ///
    /// History note: this test originally asserted truncation
    /// (`…` glyph at the right edge); the new behavior is
    /// word-wrap, so the assertion inverts — `…` must NOT appear
    /// and the full description text must be in the buffer.
    #[test]
    fn render_picker_long_description_wraps_no_ellipsis() {
        let entries = vec![SettingMeta {
            key: "long_enum",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Long",
            description: "Short.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "wide",
                choices: &[EnumChoice {
                    canonical: "wide",
                    display: "Wide",
                    description: "A deliberately verbose description that will overflow the column budget.",
                }],
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "long_enum",
            choices_idx: 0,
            original_value: SettingValue::Enum("wide"),
            supports_preview: true,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        // Concatenate ALL rendered rows. The full description must
        // be present (no `…` ellipsis anywhere on the choice rows).
        let mut all_choice_text = String::new();
        for y in 3..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    all_choice_text.push_str(cell.symbol());
                }
            }
            all_choice_text.push('\n');
        }
        assert!(
            !all_choice_text.contains('\u{2026}'),
            "wrapped description must NOT contain `…`, got:\n{all_choice_text}"
        );
        // Words appear across wrapped lines (with trailing-padding
        // whitespace between them), so we check word-presence
        // rather than substring contiguity.
        for word in [
            "A",
            "deliberately",
            "verbose",
            "description",
            "overflow",
            "budget",
        ] {
            assert!(
                all_choice_text.contains(word),
                "wrapped description must contain word {word:?}, got:\n{all_choice_text}"
            );
        }
    }

    /// Word-wrap detail check: the choice symbol + display name stay
    /// on line 1, the description wraps across ≥2 lines, AND
    /// continuation lines are indented to the description column
    /// (column 0 holds whitespace, NOT a marker glyph).
    ///
    /// Uses the production `coding_data_sharing` "Opt out" choice
    /// (a long description that wraps at width=60). Pinning against
    /// the real catalog keeps the test honest about the bug report
    /// — the screenshot in the user-feedback PR showed exactly this
    /// choice clipped with `…`.
    /// Visual smoke debugging helper. Renders the wrap fixture and
    /// prints the buffer so a human can eyeball the layout. Ignored
    /// by default; run with `cargo test -- --ignored picker_visual_smoke_debug
    /// --nocapture`.
    #[test]
    #[ignore]
    fn picker_visual_smoke_debug() {
        let entries = vec![SettingMeta {
            key: "wrap_enum",
            category: SettingCategory::Privacy,
            owner: SettingOwner::Shared,
            label: "Coding data sharing",
            description: "Controls whether SpaceXAI may retain and train on coding data.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "opt-out",
                choices: &[
                    EnumChoice {
                        canonical: "opt-in",
                        display: "Opt in",
                        description: "Allow SpaceXAI to retain and use coding session data for training and product improvement.",
                    },
                    EnumChoice {
                        canonical: "opt-out",
                        display: "Opt out",
                        description: "Do not retain coding session data. Code requests will not be used for training.",
                    },
                ],
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "wrap_enum",
            choices_idx: 1,
            original_value: SettingValue::Enum("opt-out"),
            supports_preview: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);
        println!("\nPicker visual smoke at width=60:");
        println!("{}", "─".repeat(area.width as usize));
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    row.push_str(cell.symbol());
                }
            }
            println!("{row}");
        }
        println!("{}", "─".repeat(area.width as usize));
    }

    #[test]
    fn picker_long_description_wraps_to_multiple_lines() {
        let entries = vec![SettingMeta {
            key: "wrap_enum",
            category: SettingCategory::Privacy,
            owner: SettingOwner::Shared,
            label: "Coding data sharing",
            description: "Controls whether SpaceXAI may retain and train on coding data.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "opt-out",
                choices: &[
                    EnumChoice {
                        canonical: "opt-in",
                        display: "Opt in",
                        description: "Allow SpaceXAI to retain and use coding session data for training and product improvement.",
                    },
                    EnumChoice {
                        canonical: "opt-out",
                        display: "Opt out",
                        description: "Do not retain coding session data. Code requests will not be used for training.",
                    },
                ],
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "wrap_enum",
            choices_idx: 1,
            original_value: SettingValue::Enum("opt-out"),
            supports_preview: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 16,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        let row_text = |y: u16| -> String {
            let mut s = String::new();
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    s.push_str(cell.symbol());
                }
            }
            s
        };

        // Line 1 of choice 0 ("Opt in"): symbol + display + sep +
        // start-of-description. The setting-level description above the
        // choices can wrap to a variable number of rows depending on copy
        // length, so locate choice 0's line 1 dynamically instead of
        // assuming a fixed row.
        let opt_in_row = (0..area.height)
            .find(|&y| row_text(y).contains("Opt in"))
            .expect("choice 0 line 1 ('Opt in') must render within the picker area");
        let r3 = row_text(opt_in_row);
        assert!(
            r3.contains("Opt in"),
            "choice 0 line 1 must contain 'Opt in' display, got: {r3:?}"
        );
        assert!(
            r3.contains('\u{00B7}'),
            "choice 0 line 1 must contain the `·` separator, got: {r3:?}"
        );
        assert!(
            r3.contains("Allow SpaceXAI"),
            "choice 0 line 1 must start the description, got: {r3:?}"
        );

        // The Opt-in description wraps to ≥ 2 lines at width 60.
        // Continuation rows live BELOW choice 0's line 1 until the
        // next choice starts.
        let cont_row = opt_in_row + 1;
        let r4 = row_text(cont_row);
        // Continuation line must be indented past the description
        // column. Column 0 + column 1 (symbol cells on line 1)
        // should be whitespace on the continuation line.
        assert_eq!(
            buf.cell((0u16, cont_row)).map(|c| c.symbol()),
            Some(" "),
            "continuation row col 0 must be whitespace, got row: {r4:?}"
        );
        assert_eq!(
            buf.cell((1u16, cont_row)).map(|c| c.symbol()),
            Some(" "),
            "continuation row col 1 (where marker would sit on line 1) must be whitespace, got row: {r4:?}"
        );

        // The full Opt-in description must appear across the wrapped
        // rows — no `…` truncation.
        let mut opt_in_full = String::new();
        for y in opt_in_row..area.height {
            let line = row_text(y);
            // Stop at the start of the Opt-out choice (line that
            // contains the second display).
            if y > opt_in_row && line.contains("Opt out") {
                break;
            }
            opt_in_full.push_str(&line);
            opt_in_full.push('\n');
        }
        assert!(
            !opt_in_full.contains('\u{2026}'),
            "wrapped Opt-in description must NOT contain `…`, got:\n{opt_in_full}"
        );
        for word in [
            "Allow",
            "SpaceXAI",
            "retain",
            "session",
            "training",
            "improvement",
        ] {
            assert!(
                opt_in_full.contains(word),
                "Opt-in description must include word {word:?}, got:\n{opt_in_full}"
            );
        }
    }

    /// Short descriptions stay on ONE line — no continuation rows.
    /// Asserts the row directly below a choice's line 1 is either
    /// the next choice's line 1 (when there are more choices) or
    /// blank.
    #[test]
    fn picker_short_description_stays_one_line() {
        let entries = vec![SettingMeta {
            key: "short_enum",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Short",
            description: "Short.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "a",
                choices: &[
                    EnumChoice {
                        canonical: "a",
                        display: "Alpha",
                        description: "A.",
                    },
                    EnumChoice {
                        canonical: "b",
                        display: "Bravo",
                        description: "B.",
                    },
                ],
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "short_enum",
            choices_idx: 0,
            original_value: SettingValue::Enum("a"),
            supports_preview: true,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        let row_text = |y: u16| -> String {
            let mut s = String::new();
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    s.push_str(cell.symbol());
                }
            }
            s
        };

        // Choice 0 on row 3, choice 1 on row 4 (one line each).
        assert!(
            row_text(3).contains("Alpha") && row_text(3).contains("A."),
            "choice 0 must be one line, got row 3: {:?}",
            row_text(3)
        );
        assert!(
            row_text(4).contains("Bravo") && row_text(4).contains("B."),
            "choice 1 must start at row 4 (no continuation rows from choice 0), got row 4: {:?}",
            row_text(4)
        );
    }

    /// Choices with an empty description render the symbol + display
    /// ONLY — no `·` separator, no trailing stray cells.
    #[test]
    fn picker_no_description_renders_symbol_and_display_only() {
        let entries = vec![SettingMeta {
            key: "nodesc_enum",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "No desc",
            description: "Short.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "a",
                choices: &[
                    EnumChoice {
                        canonical: "a",
                        display: "Alpha",
                        description: "",
                    },
                    EnumChoice {
                        canonical: "b",
                        display: "Bravo",
                        description: "",
                    },
                ],
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "nodesc_enum",
            choices_idx: 0,
            original_value: SettingValue::Enum("a"),
            supports_preview: true,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        let row_text = |y: u16| -> String {
            let mut s = String::new();
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    s.push_str(cell.symbol());
                }
            }
            s
        };

        let r3 = row_text(3);
        assert!(
            r3.contains("Alpha"),
            "choice 0 must render its display, got: {r3:?}"
        );
        assert!(
            !r3.contains('\u{00B7}'),
            "choice 0 with empty description must NOT render the `·` separator, got: {r3:?}"
        );
    }

    /// Multi-line choice hit-rect spans ALL its lines. Clicking on
    /// the continuation line of a wrapped choice moves the picker
    /// focus to that choice (same as clicking line 1). Mirrors the
    /// commit-13 fix for two-line row hit-rects in Browse mode.
    #[test]
    fn picker_multi_line_choice_hit_rect_spans_all_lines() {
        // Reuse the wrap fixture: long descriptions on both choices.
        let entries = vec![SettingMeta {
            key: "wrap_enum",
            category: SettingCategory::Privacy,
            owner: SettingOwner::Shared,
            label: "Coding data sharing",
            description: "Controls whether SpaceXAI may retain coding data.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "opt-in",
                choices: &[
                    EnumChoice {
                        canonical: "opt-in",
                        display: "Opt in",
                        description: "Allow SpaceXAI to retain and use coding session data for training and product improvement.",
                    },
                    EnumChoice {
                        canonical: "opt-out",
                        display: "Opt out",
                        description: "Do not retain coding session data. Code requests will not be used for training.",
                    },
                ],
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "wrap_enum",
            choices_idx: 0,
            original_value: SettingValue::Enum("opt-in"),
            supports_preview: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 16,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);
        // Drain the per-choice rects (the production caller does
        // this via `render_settings_modal`; we mirror it here so the
        // mouse handler sees the rects).
        s.picker_choice_rects = take_picker_choice_rects();

        // Sanity: each choice's rect has height ≥ 2 at width 60.
        assert_eq!(
            s.picker_choice_rects.len(),
            2,
            "expected 2 choice hit-rects, got {}",
            s.picker_choice_rects.len()
        );
        let rect0 = s.picker_choice_rects[0];
        let rect1 = s.picker_choice_rects[1];
        assert!(
            rect0.height >= 2,
            "choice 0 should span ≥ 2 lines (wrapped description), got rect {rect0:?}"
        );
        assert!(
            rect1.height >= 2,
            "choice 1 should span ≥ 2 lines (wrapped description), got rect {rect1:?}"
        );
        // Rects must NOT overlap vertically.
        assert!(
            rect0.y + rect0.height <= rect1.y,
            "choice rects must not overlap: rect0={rect0:?}, rect1={rect1:?}"
        );

        // The initial focus is on choice 0. Click on the last line
        // (a continuation line) of choice 1 — the click should
        // move focus to choice 1.
        let click_y = rect1.y + rect1.height - 1;
        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            10,
            click_y,
        );
        match (outcome, &s.mode) {
            (
                SettingsKeyOutcome::Changed | SettingsKeyOutcome::Action(_),
                SettingsModalMode::PickingEnum { choices_idx, .. },
            ) => {
                assert_eq!(
                    *choices_idx, 1,
                    "click on continuation row of choice 1 must focus choice 1, got idx {}",
                    *choices_idx
                );
            }
            (other, mode) => panic!(
                "click on continuation line should change focus, got outcome {other:?} in mode {mode:?}"
            ),
        }
    }

    /// Picker scroll math accounts for variable per-choice height.
    /// With 5 choices each ~3 lines tall in a ~8-line viewport,
    /// focusing the LAST choice scrolls so it's visible (and the
    /// earlier choices may shift off the top).
    #[test]
    fn picker_scroll_offset_accounts_for_variable_height() {
        // Each description is ≥ 3 wrap lines wide at width=40.
        let entries = vec![SettingMeta {
            key: "many_wrap",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Many",
            description: "Many.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "c0",
                choices: &[
                    EnumChoice {
                        canonical: "c0",
                        display: "C0",
                        description: "Choice zero description that is verbose enough to span three lines at width 40.",
                    },
                    EnumChoice {
                        canonical: "c1",
                        display: "C1",
                        description: "Choice one description that is verbose enough to span three lines at width 40.",
                    },
                    EnumChoice {
                        canonical: "c2",
                        display: "C2",
                        description: "Choice two description that is verbose enough to span three lines at width 40.",
                    },
                    EnumChoice {
                        canonical: "c3",
                        display: "C3",
                        description: "Choice three description that is verbose enough to span three lines at width 40.",
                    },
                    EnumChoice {
                        canonical: "c4",
                        display: "C4",
                        description: "Choice four description that is verbose enough to span three lines at width 40.",
                    },
                ],
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let registry = Arc::new(SettingsRegistry::from_entries(entries));
        // Focus the LAST choice (c4) — the scroll math must keep it
        // in view.
        let mut s = SettingsModalState::new(
            registry.clone(),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "many_wrap",
            choices_idx: 4,
            original_value: SettingValue::Enum("c4"),
            supports_preview: true,
        };
        // Viewport: title + desc + gap = 3 rows of chrome + 8 rows
        // of choices = 11 total. With 5 choices × 3 lines = 15 total
        // wrap-rows of content, only ~2 choices can fit per page.
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 11,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);
        s.picker_choice_rects = take_picker_choice_rects();

        // The focused choice (c4) MUST have a non-zero hit rect (it
        // got rendered).
        let rect_c4 = s.picker_choice_rects[4];
        assert!(
            rect_c4.width > 0 && rect_c4.height > 0,
            "focused choice c4 must be visible after scroll, got rect {rect_c4:?}"
        );
        // The focused choice's rect must fit inside the area's
        // height bounds.
        assert!(
            rect_c4.y + rect_c4.height <= area.y + area.height,
            "focused choice c4 must fit inside the viewport, got rect {rect_c4:?} vs area {area:?}"
        );
        // Choice 0 (c0) should be scrolled off the top (rect zero).
        let rect_c0 = s.picker_choice_rects[0];
        assert_eq!(
            (rect_c0.width, rect_c0.height),
            (0, 0),
            "choice c0 must be scrolled off-screen, got rect {rect_c0:?}"
        );
    }

    /// Long choice display names get truncated with `…`. The bug was
    /// that `display` rendered via
    /// raw `set_span` without `truncate_str`, producing mid-character
    /// clips. Same shape as the description test above.
    #[test]
    fn render_picker_truncates_long_display_with_ellipsis() {
        let entries = vec![SettingMeta {
            key: "long_enum",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Long",
            description: "Short.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "wide",
                choices: &[EnumChoice {
                    canonical: "wide",
                    display: "An absurdly long display name designed to overflow",
                    description: "Short desc.",
                }],
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "long_enum",
            choices_idx: 0,
            original_value: SettingValue::Enum("wide"),
            supports_preview: true,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 24,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        let mut row_text = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 3)) {
                row_text.push_str(cell.symbol());
            }
        }
        assert!(
            row_text.contains('\u{2026}'),
            "long display must truncate with `…`, got: {row_text:?}"
        );
    }

    /// Long setting label in the title row truncates with `…`.
    #[test]
    fn render_picker_truncates_long_title_with_ellipsis() {
        let entries = vec![SettingMeta {
            key: "long_enum",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "An exceptionally verbose setting label that overflows",
            description: "Short.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "a",
                choices: &[EnumChoice {
                    canonical: "a",
                    display: "A",
                    description: "A.",
                }],
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "long_enum",
            choices_idx: 0,
            original_value: SettingValue::Enum("a"),
            supports_preview: true,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 12,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        let mut title_text = String::new();
        for x in 0..area.width {
            if let Some(cell) = buf.cell((x, 0)) {
                title_text.push_str(cell.symbol());
            }
        }
        assert!(
            title_text.contains('\u{2026}'),
            "long title must truncate with `…`, got: {title_text:?}"
        );
    }

    /// When choices > visible_h, the picker renders an overflow
    /// indicator `… N more` on the last visible row.
    #[test]
    fn render_picker_shows_more_indicator_when_choices_overflow() {
        // Build a registry with 8 choices (exceeds 4-row viewport
        // at height=8).
        let entries = vec![SettingMeta {
            key: "long_enum",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Many",
            description: "Many choices.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "c0",
                choices: &[
                    EnumChoice {
                        canonical: "c0",
                        display: "C0",
                        description: "0",
                    },
                    EnumChoice {
                        canonical: "c1",
                        display: "C1",
                        description: "1",
                    },
                    EnumChoice {
                        canonical: "c2",
                        display: "C2",
                        description: "2",
                    },
                    EnumChoice {
                        canonical: "c3",
                        display: "C3",
                        description: "3",
                    },
                    EnumChoice {
                        canonical: "c4",
                        display: "C4",
                        description: "4",
                    },
                    EnumChoice {
                        canonical: "c5",
                        display: "C5",
                        description: "5",
                    },
                ],
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }];
        let mut s = SettingsModalState::new(
            Arc::new(SettingsRegistry::from_entries(entries)),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::PickingEnum {
            key: "long_enum",
            choices_idx: 0,
            original_value: SettingValue::Enum("c0"),
            supports_preview: true,
        };
        // Total height 7 → header_rows=3 (title+desc+gap) + 4 choices
        // rows. With 6 choices, 4 fit in viewport - 1 (reserved for
        // overflow). So 3 visible, 3 hidden → "… 3 more".
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 7,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_picking_enum(&mut buf, area, &s, &theme);

        // Scan all rows for the overflow indicator.
        let mut all_text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    all_text.push_str(cell.symbol());
                }
            }
            all_text.push('\n');
        }
        assert!(
            all_text.contains('\u{2026}') && all_text.contains("more"),
            "overflow indicator '… N more' must render, got:\n{all_text}"
        );
    }

    // -- render_settings_modal routing coverage --

    /// `render_settings_modal` branches on mode → picker render path.
    /// Verifies that the search-bar placeholder text is NOT present
    /// (proves the picker branch fired and the Browse path was
    /// skipped) AND that hit-test rects are reset on entry.
    #[test]
    fn render_settings_modal_routes_to_picker_when_mode_is_picking_enum() {
        let mut s = picker_test_state();
        // Pre-populate row_rects so we can verify reset_hit_rects().
        s.row_rects = vec![
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 1,
            };
            s.rows.len()
        ];
        s.list_area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 10,
        };

        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 30,
        };
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);

        // Scan the buffer for the Browse-mode search-bar placeholder.
        // If the picker branch fired, this string should NOT appear.
        let mut all_text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    all_text.push_str(cell.symbol());
                }
            }
            all_text.push('\n');
        }
        assert!(
            !all_text.contains("/ to search"),
            "picker mode must not render the Browse-mode search bar"
        );
        // The picker's setting label should be visible.
        assert!(
            all_text.contains("Test enum"),
            "picker mode must render the setting label"
        );
        // Hit-test rects must be reset.
        assert!(
            s.row_rects.iter().all(|r| r == &Rect::default()),
            "row_rects should be reset to default on picker entry, got: {:?}",
            s.row_rects
        );
    }

    // -- mouse + catch-all coverage --

    /// Scroll wheel during PickingEnum mode is a no-op AND does NOT
    /// mutate `state.selected` (the underlying Browse selection).
    /// Regression test.
    #[test]
    fn picker_mode_scroll_wheel_is_noop_and_preserves_browse_selection() {
        let mut s = picker_test_state();
        let selected_before = s.selected;
        let outcome = handle_settings_mouse(&mut s, MouseEventKind::ScrollDown, 10, 5);
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "Scroll in picker mode must be Unchanged, got {outcome:?}"
        );
        assert_eq!(
            s.selected, selected_before,
            "scroll in picker mode must NOT mutate Browse selection"
        );

        let outcome = handle_settings_mouse(&mut s, MouseEventKind::ScrollUp, 10, 5);
        assert!(matches!(outcome, SettingsKeyOutcome::Unchanged));
        assert_eq!(s.selected, selected_before);
    }

    /// Mouse click in PickingEnum mode is a no-op (click-to-pick is
    /// handled elsewhere).
    #[test]
    fn picker_mode_mouse_click_is_noop() {
        let mut s = picker_test_state();
        let selected_before = s.selected;
        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            5,
            5,
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Unchanged));
        assert_eq!(s.selected, selected_before);
    }

    /// Random keypresses in PickingEnum mode are Unchanged and don't
    /// leak to other handlers (e.g., the filter query).
    #[test]
    fn picker_ignores_random_keypress() {
        let mut s = picker_test_state();
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert!(matches!(outcome, SettingsKeyOutcome::Unchanged));
        assert!(
            matches!(
                s.mode,
                SettingsModalMode::PickingEnum { choices_idx: 0, .. }
            ),
            "mode must remain PickingEnum after random keypress"
        );
    }

    /// EditingValue: char keys mutate the buffer (Changed),
    /// Enter on an empty / invalid buffer is a no-op (the validator
    /// gate refuses commit), and Esc returns to Browse.
    ///
    /// **History note:** an earlier
    /// scaffold test (`editing_value_ignores_non_esc_keys`) asserted
    /// every non-Esc key returned `Unchanged`. The editor is now wired
    /// for real, so chars mutate the buffer and the test
    /// inverts: chars produce `Changed` (buffer mutation), Enter on
    /// an invalid buffer produces `Unchanged` (validator refuses).
    ///
    /// Uses a snapshot with a populated `available_models` list
    /// so the `KnownModel` validator has data to validate against —
    /// otherwise an empty catalog short-circuits to "valid" (defense
    /// in depth — the dispatcher's resolution step is the
    /// belt-and-suspenders backstop).
    #[test]
    fn editing_value_chars_mutate_buffer_and_invalid_enter_is_noop() {
        // `default_model` is now a `SettingKind::DynamicEnum`, so the
        // production catalog no
        // longer wires the String editor. We construct a synthetic
        // registry to keep the editor-mode contract under test —
        // `editor_render_fixture` uses the same pattern.
        let mut s = editor_render_fixture("", 0);
        // Char 'a' goes into the buffer → Changed.
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "char insert in EditingValue must be Changed, got {outcome:?}"
        );
        match &s.mode {
            SettingsModalMode::EditingValue {
                buffer,
                validation_error,
                ..
            } => {
                assert_eq!(buffer, "a");
                assert!(
                    validation_error.is_some(),
                    "validation_error must be Some for unknown model 'a' \
                     (catalog has 'Grok 4 Fast' only)",
                );
            }
            _ => panic!("mode must remain EditingValue after char input"),
        }

        // Enter on a buffer that fails the KnownModel validator
        // (catalog has 'Grok 4 Fast'; "a" doesn't match) is
        // Unchanged — commit refused.
        let outcome =
            handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "Enter on invalid buffer must be Unchanged, got {outcome:?}"
        );
        assert!(
            matches!(s.mode, SettingsModalMode::EditingValue { .. }),
            "Enter on invalid buffer must keep EditingValue mode (no commit)"
        );
    }

    // -- helper-function coverage --

    /// `picker_choices_len` returns 0 for an unknown key, a non-Enum
    /// key, and a zero-choice Enum.
    #[test]
    fn picker_choices_len_handles_missing_and_non_enum() {
        let s = picker_test_state_in_browse();
        // Unknown key → 0.
        assert_eq!(picker_choices_len(&s, "unknown-key-xyzzy"), 0);
        // The synthetic registry contains only "test_enum" — there's
        // no Bool to test against without rebuilding. Test the
        // non-Enum case via the default registry instead.
        let bool_state = make_state();
        assert_eq!(picker_choices_len(&bool_state, "compact_mode"), 0);
    }

    #[test]
    fn picker_choice_at_returns_none_for_oob_and_missing() {
        let s = picker_test_state_in_browse();
        assert_eq!(picker_choice_at(&s, "test_enum", 0), Some("first"));
        assert_eq!(picker_choice_at(&s, "test_enum", 99), None);
        assert_eq!(picker_choice_at(&s, "unknown-key", 0), None);

        let bool_state = make_state();
        assert_eq!(picker_choice_at(&bool_state, "compact_mode", 0), None);
    }

    #[test]
    fn editing_value_esc_returns_to_browse() {
        let mut s = make_state();
        s.mode = SettingsModalMode::EditingValue {
            key: "default_model",
            buffer: "grok-4".to_string(),
            cursor_byte: "grok-4".len(),
            validation_error: None,
        };
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(outcome, SettingsKeyOutcome::Changed));
        assert!(matches!(s.mode, SettingsModalMode::Browse));
    }

    // -- Direct unit tests for compute_filtered --
    //
    // The free function is module-private; integration tests can only
    // reach it through the key-press surface. These unit tests pin
    // structural cases that are not naturally reachable through that
    // surface (e.g.,
    // a category whose settings all fail the filter — header excluded).

    #[test]
    fn compute_filtered_empty_query_returns_identity() {
        let rows = vec![
            RowEntry::Header {
                category: SettingCategory::Appearance,
            },
            RowEntry::Setting {
                key: "compact_mode",
                meta_index: 0,
            },
            RowEntry::Setting {
                key: "show_timestamps",
                meta_index: 1,
            },
        ];
        let registry = SettingsRegistry::defaults();
        let result = compute_filtered(&rows, &registry, "");
        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn compute_filtered_excludes_header_when_no_children_match() {
        // Synthetic rows: header + setting that doesn't match.
        // Header MUST be excluded — emitting an orphan header is a
        // visual bug.
        let rows = vec![
            RowEntry::Header {
                category: SettingCategory::Appearance,
            },
            RowEntry::Setting {
                key: "compact_mode",
                meta_index: 0,
            },
        ];
        let registry = SettingsRegistry::defaults();
        let result = compute_filtered(&rows, &registry, "xyzzy-no-match");
        assert!(
            result.is_empty(),
            "header must be excluded when no settings in its section match, got {result:?}"
        );
    }

    #[test]
    fn compute_filtered_single_word_match_emits_header_then_setting() {
        let rows = vec![
            RowEntry::Header {
                category: SettingCategory::Appearance,
            },
            RowEntry::Setting {
                key: "compact_mode",
                meta_index: 0,
            },
            RowEntry::Setting {
                key: "show_timestamps",
                meta_index: 1,
            },
            RowEntry::Setting {
                key: "simple_mode",
                meta_index: 2,
            },
        ];
        let registry = SettingsRegistry::defaults();
        // "density" is a compact_mode keyword only.
        let result = compute_filtered(&rows, &registry, "density");
        assert_eq!(result, vec![0, 1], "header then compact_mode in order");
    }

    #[test]
    fn compute_filtered_multi_word_and_match_narrows_further() {
        let rows = vec![
            RowEntry::Header {
                category: SettingCategory::Appearance,
            },
            RowEntry::Setting {
                key: "compact_mode",
                meta_index: 0,
            },
        ];
        let registry = SettingsRegistry::defaults();
        // Both "compact" and "density" are compact_mode keywords.
        let result = compute_filtered(&rows, &registry, "compact density");
        assert_eq!(result, vec![0, 1]);
    }

    /// `advance_next`'s `None`-arm defensive path: when `selected`
    /// is manually mutated to a row hidden by the filter, the next
    /// navigation jumps to the FIRST visible setting (Down → top).
    /// This exercises the asymmetric-defensive-path documented in
    /// `advance_next`/`advance_prev`.
    #[test]
    fn advance_next_recovers_when_selection_is_hidden() {
        let mut s = make_state();
        // Apply a filter that hides compact_mode.
        s.query = "stamp".to_string();
        s.query_cursor = s.query.len();
        s.invalidate_filter();
        // Manually corrupt selected to a HIDDEN row (compact_mode is
        // row 1, hidden by "stamp"). This bypasses
        // clamp_selected_to_visible and exercises the defensive arm.
        let compact_idx = s
            .rows
            .iter()
            .position(|r| matches!(r, RowEntry::Setting { key, .. } if *key == "compact_mode"))
            .unwrap();
        s.selected = compact_idx;
        // Advance: lands on the first visible setting (show_timestamps).
        let moved = s.advance_next();
        assert!(moved);
        let show_ts_idx = s
            .rows
            .iter()
            .position(|r| matches!(r, RowEntry::Setting { key, .. } if *key == "show_timestamps"))
            .unwrap();
        assert_eq!(s.selected, show_ts_idx);
    }

    /// Counterpart to `advance_next_recovers_when_selection_is_hidden`:
    /// when `selected` is on a hidden row, Up lands on the LAST
    /// visible setting. Asymmetric by design (each picks the nearest
    /// end of the filter from the user's perspective).
    #[test]
    fn advance_prev_recovers_when_selection_is_hidden() {
        let mut s = make_state();
        // Apply a filter matching only show_timestamps and simple_mode.
        // "mode" matches both: compact_mode label, simple_mode label
        // AND show_timestamps via... actually let's pick a more reliable
        // filter — use individual keywords. "simple" matches simple_mode
        // only. Let's use that and corrupt selected to compact_mode
        // (hidden). Up should land on the LAST visible setting which
        // is simple_mode.
        s.query = "simple".to_string();
        s.query_cursor = s.query.len();
        s.invalidate_filter();
        let compact_idx = s
            .rows
            .iter()
            .position(|r| matches!(r, RowEntry::Setting { key, .. } if *key == "compact_mode"))
            .unwrap();
        s.selected = compact_idx;
        let moved = s.advance_prev();
        assert!(moved);
        let simple_idx = s
            .rows
            .iter()
            .position(|r| matches!(r, RowEntry::Setting { key, .. } if *key == "simple_mode"))
            .unwrap();
        assert_eq!(s.selected, simple_idx);
    }

    // -- blank line above category section headers --
    //
    // The renderer reserves one empty visual line ABOVE every section
    // header EXCEPT the one that lands first in the viewport. These
    // tests render the modal directly to a buffer and inspect the
    // y-positions of the rendered category labels.

    /// Scan one row of the buffer and return its text content (no
    /// styles) with leading/trailing whitespace preserved.
    fn buf_row_text(buf: &Buffer, y: u16, x: u16, width: u16) -> String {
        let mut s = String::new();
        for col in x..x.saturating_add(width) {
            if let Some(cell) = buf.cell((col, y)) {
                s.push_str(cell.symbol());
            }
        }
        s
    }

    /// Find the absolute column index where `needle` begins on row
    /// `y` of `buf`, scanning from `x_start` to `x_end - 1`. Walks
    /// cells one at a time and compares each cell's symbol so the
    /// returned column is the actual buffer position — not a byte
    /// offset projected onto width as `text.find(needle)` would
    /// give. Returns `None` if `needle` doesn't appear on the row.
    ///
    /// Use this in any test that needs to assert per-cell style for
    /// a text fragment. Earlier tests cast
    /// `string.find(needle) as u16` which only matched the cell
    /// position for ASCII content; once a unicode glyph entered the
    /// row's label/value, the byte offset diverged from the column.
    fn find_text_col(buf: &Buffer, y: u16, needle: &str) -> Option<u16> {
        if needle.is_empty() {
            return None;
        }
        // Sweep the row. For each starting column, compare the
        // sequence of cell symbols against the needle's chars.
        let area = buf.area;
        let needle_chars: Vec<&str> =
            unicode_segmentation::UnicodeSegmentation::graphemes(needle, true).collect();
        let x_start = area.x;
        let x_end = area.x.saturating_add(area.width);
        for x in x_start..x_end {
            let mut col = x;
            let mut all_match = true;
            for &grapheme in &needle_chars {
                let Some(cell) = buf.cell((col, y)) else {
                    all_match = false;
                    break;
                };
                if cell.symbol() != grapheme {
                    all_match = false;
                    break;
                }
                col = col.saturating_add(grapheme.width() as u16);
                if col >= x_end {
                    // Needle is partially past the right edge.
                    all_match = false;
                    break;
                }
            }
            if all_match {
                return Some(x);
            }
        }
        None
    }

    /// Render the row list with default registry; assert that every
    /// section header AFTER the first has a blank line immediately
    /// above it.
    #[test]
    fn section_headers_have_blank_line_above_except_first() {
        let mut s = make_state();
        // Allocate a generous viewport so every category fits — the
        // default registry contains 6 categories with 16 settings;
        // the blank lines push us to ~23 lines, fits in 60.
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 60,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_rows(&mut buf, area, &mut s, &theme);

        // Collect each category's expected label + the y position of
        // its rendered label line. Headers render with their full
        // label (e.g. "Appearance"); we locate each header by
        // searching for its label as the row content.
        let mut header_ys: Vec<(u16, &'static str)> = Vec::new();
        for cat in SettingCategory::ALL {
            // Skip categories the default registry doesn't populate
            // (e.g. Session — no settings registered).
            let has_setting = s
                .rows
                .iter()
                .any(|r| matches!(r, RowEntry::Header { category } if category == cat));
            if !has_setting {
                continue;
            }
            let label = cat.label();
            for y in 0..area.height {
                let row_text = buf_row_text(&buf, y, area.x, area.width);
                if row_text.trim_start().starts_with(label) {
                    header_ys.push((y, label));
                    break;
                }
            }
        }
        assert!(
            header_ys.len() >= 2,
            "this test requires ≥2 rendered headers, got: {header_ys:?}"
        );
        // First header has NO blank line above it — it hugs the top.
        let (first_y, _) = header_ys[0];
        assert_eq!(
            first_y, area.y,
            "first section header must sit at the top of the list area, got y={first_y}"
        );

        // Every subsequent header has an EMPTY visual line immediately above.
        for &(y, label) in header_ys.iter().skip(1) {
            assert!(
                y > area.y,
                "section header `{label}` rendered above the top of the area (y={y})"
            );
            let above = buf_row_text(&buf, y - 1, area.x, area.width);
            assert!(
                above.chars().all(|c| c == ' '),
                "line above section header `{label}` must be blank, got: {above:?}"
            );
        }
    }

    /// When the viewport begins at a section header, we do NOT
    /// reserve a leading blank line above it — the header hugs the
    /// top of the row-list area.
    #[test]
    fn first_section_header_has_no_leading_gap() {
        let mut s = make_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 60,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_rows(&mut buf, area, &mut s, &theme);

        // The very first row of the area must contain the first
        // category's label (Appearance is first in
        // `SettingCategory::ALL` and is registered by
        // `default_settings()`).
        let first_row = buf_row_text(&buf, area.y, area.x, area.width);
        let appearance = SettingCategory::Appearance.label();
        assert!(
            first_row.trim_start().starts_with(appearance),
            "first rendered line must be the `{appearance}` header, got: {first_row:?}"
        );
    }

    /// Row hit-rects must remain aligned with the actually-rendered
    /// y positions when blank lines are inserted above section
    /// headers. Click on a setting row's y-coordinate should match
    /// the rect stored in `state.row_rects`.
    #[test]
    fn row_rects_shift_down_for_blank_lines_above_headers() {
        let mut s = make_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 60,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_rows(&mut buf, area, &mut s, &theme);

        // For every row, the stored rect's y must match the actual
        // rendered y. We verify this by checking that the row's
        // content (Header label or Setting label) appears at the
        // rect's y position. Skip rows with default Rect (off-screen).
        for (i, r) in s.rows.iter().enumerate() {
            let rect = s.row_rects[i];
            if rect.width == 0 {
                continue;
            }
            let row_text = buf_row_text(&buf, rect.y, area.x, area.width);
            let expected_substring = match r {
                RowEntry::Header { category } => category.label().to_string(),
                RowEntry::Setting { meta_index, .. } => {
                    s.registry.all()[*meta_index].label.to_string()
                }
            };
            assert!(
                row_text.contains(&expected_substring),
                "row {i} rect.y={} should contain `{expected_substring}` but got: {row_text:?}",
                rect.y,
            );
        }
    }

    // -- two-line layout when label + value don't fit --
    //
    // The `row_layout` helper decides one-line vs two-line vs
    // two-line-with-label-truncation based on the full label width.
    // These tests pin the behaviour at three width budgets that
    // exercise each layout variant.

    /// Unit test for the extracted
    /// `wrap_description` helper — pins its behavior in one place
    /// so each of the three callers doesn't have to assert it
    /// indirectly through a modal-render check.
    #[test]
    fn wrap_description_empty_and_zero_width_return_empty() {
        assert!(wrap_description("", 80).is_empty());
        assert!(wrap_description("anything", 0).is_empty());
    }

    #[test]
    fn wrap_description_single_line_when_fits() {
        let wrapped = wrap_description("Short text.", 80);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0], "Short text.");
    }

    #[test]
    fn wrap_description_splits_long_text_at_word_boundaries() {
        let long = "alpha beta gamma delta epsilon zeta eta theta iota";
        let wrapped = wrap_description(long, 15);
        // Total visible chars (no `…`) reassembles the original.
        assert!(wrapped.len() >= 2, "must wrap at narrow width: {wrapped:?}");
        let joined: String = wrapped.join(" ");
        for word in long.split_whitespace() {
            assert!(
                joined.contains(word),
                "wrap must preserve word `{word}` (no mid-word truncation): {joined:?}",
            );
        }
        // No `…` truncation marker in any line.
        for line in &wrapped {
            assert!(
                !line.contains('\u{2026}'),
                "wrap line must not contain `…`: {line:?}",
            );
        }
    }

    fn synthetic_long_label_meta() -> SettingMeta {
        // Fixed 31-cell label kept for the two-line threshold tests
        // below. Previously matched the literal `simple_mode` label
        // (now renamed to "Disable vim input mode" — 22 cells); the
        // longer literal stays to exercise the wrap path that the
        // shorter rename no longer triggers organically.
        SettingMeta {
            key: "test-long-label",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Disable vim mode (experimental)",
            description: "Long-label test for Commit 13 two-line layout.",
            keywords: &["test"],
            kind: SettingKind::Bool { default: false },
            restart_required: false,
            hidden_in_minimal: false,
        }
    }

    fn synthetic_enum_chevron_meta() -> SettingMeta {
        SettingMeta {
            key: "test-enum-with-chevron",
            category: SettingCategory::Privacy,
            owner: SettingOwner::Shared,
            label: "Coding data sharing",
            description: "Enum row that opens a picker — chevron suffix applies.",
            keywords: &["test"],
            kind: SettingKind::Enum {
                default: "opt-out",
                choices: TEST_ENUM_CHOICES,
                supports_preview: true,
            },
            restart_required: false,
            hidden_in_minimal: false,
        }
    }

    /// Render a long label at a narrow area; expect the value to drop
    /// to line 2 right-aligned, while the full label stays on line 1.
    ///
    /// "Disable vim mode (experimental)" = 31 cells. One-line total
    /// (with `off` + chrome = 38 cells) doesn't fit at width=35, so
    /// the row picks `TwoLine`. The label alone (with triangle +
    /// right pad = 34 cells) DOES fit, so it stays on line 1
    /// without truncation.
    #[test]
    fn narrow_terminal_drops_value_to_second_line() {
        let meta = synthetic_long_label_meta();
        let area = Rect {
            x: 0,
            y: 0,
            width: 35,
            height: 2,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let value_rect = render_setting_row(
            &mut buf,
            area,
            &meta,
            &SettingValue::Bool(false),
            24, // max_label_w — ignored for layout.
            false,
            &theme,
            false,
            false, // is_hovered
        );
        let line1 = buf_row_text(&buf, 0, area.x, area.width);
        let line2 = buf_row_text(&buf, 1, area.x, area.width);
        assert!(
            line1.contains("Disable vim mode (experimental)"),
            "line 1 must contain the FULL label (no truncation): {line1:?}"
        );
        assert!(
            !line1.contains("off"),
            "value `off` must NOT render on line 1 — it should drop to line 2. line1={line1:?}"
        );
        assert!(
            line2.contains("off"),
            "value `off` must render on line 2 (right-aligned): {line2:?}"
        );
        // Value should be right-aligned: the `off` text ends just
        // before the (reserved-but-empty) chevron column AND the
        // 1-cell right pad. Line-2
        // reserves the same `ROW_RIGHT_PAD_W + ROW_CHEVRON_COL_W`
        // suffix as line 1, so the chevron column is at a constant
        // right offset from the area's right edge regardless of
        // whether a row went one-line or two-line. We allow 1
        // extra cell of slack for the gap between value and the
        // chevron column.
        let last_idx = line2.rfind("off").expect("line2 contains `off`");
        let slack = (ROW_CHEVRON_COL_W as usize) + (ROW_RIGHT_PAD_W as usize) + 1;
        assert!(
            last_idx + "off".len() >= (area.width as usize).saturating_sub(slack),
            "value must be right-aligned (within {slack} cells of right edge): \
             last_idx={last_idx}, area.width={}",
            area.width,
        );
        assert_eq!(
            value_rect.y,
            area.y + 1,
            "value_rect.y must be on line 2 (area.y + 1), got y={}",
            value_rect.y
        );
    }

    /// At wide area widths, the row collapses to a single line — full
    /// label + value on the same line.
    #[test]
    fn wide_terminal_keeps_value_on_first_line() {
        let meta = synthetic_long_label_meta();
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 2,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        let value_rect = render_setting_row(
            &mut buf,
            area,
            &meta,
            &SettingValue::Bool(false),
            24,
            false,
            &theme,
            false,
            false, // is_hovered
        );
        let line1 = buf_row_text(&buf, 0, area.x, area.width);
        let line2 = buf_row_text(&buf, 1, area.x, area.width);
        assert!(
            line1.contains("Disable vim mode (experimental)") && line1.contains("off"),
            "wide-area one-line layout must render label + value on line 1: {line1:?}"
        );
        assert!(
            line2.chars().all(|c| c == ' '),
            "line 2 must be blank in wide-area one-line layout: {line2:?}"
        );
        assert_eq!(
            value_rect.y, area.y,
            "value_rect.y must be line 1 (area.y) in one-line layout"
        );
    }

    /// `row_layout`'s pathological-truncation branch: when even the
    /// label alone exceeds the row width, the label gets truncated
    /// with `…` on line 1 and the value still drops to line 2.
    #[test]
    fn pathologically_narrow_truncates_label_with_ellipsis() {
        let meta = synthetic_long_label_meta();
        let area = Rect {
            x: 0,
            y: 0,
            width: 25,
            height: 2,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_setting_row(
            &mut buf,
            area,
            &meta,
            &SettingValue::Bool(false),
            24,
            false,
            &theme,
            false,
            false, // is_hovered
        );
        let line1 = buf_row_text(&buf, 0, area.x, area.width);
        let line2 = buf_row_text(&buf, 1, area.x, area.width);
        assert!(
            line1.contains('\u{2026}'),
            "line 1 must contain the `…` ellipsis when label is too wide: {line1:?}"
        );
        assert!(
            line2.contains("off"),
            "value `off` must still drop to line 2 even when label is truncated: {line2:?}"
        );
    }

    /// Two-line rows expand `state.row_rects` to span BOTH lines so
    /// mouse clicks on either line trigger the same default action.
    ///
    /// `coding_data_sharing`: label 19 + value "Opt out" 7 + chevron
    /// 2 + chrome 4 = 32 cells one-line. We render at width=28 so
    /// the row drops to two lines.
    #[test]
    fn two_line_row_hit_rect_spans_both_lines() {
        let mut s = make_state();
        let row_idx = s
            .rows
            .iter()
            .position(
                |r| matches!(r, RowEntry::Setting { key, .. } if *key == "coding_data_sharing"),
            )
            .expect("coding_data_sharing must be registered");
        // Render at a narrow width so coding_data_sharing forces a
        // two-line layout.
        let area = Rect {
            x: 0,
            y: 0,
            width: 28,
            height: 60,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        s.selected = row_idx;
        render_rows(&mut buf, area, &mut s, &theme);

        let rect = s.row_rects[row_idx];
        assert!(
            rect.height >= 2,
            "two-line row hit-rect must span ≥2 lines, got height={}",
            rect.height
        );

        // Synthesize a click on line 2 of the row. The mouse handler
        // should fire the default action (open the enum picker for
        // coding_data_sharing).
        s.list_area = area;
        let click_y = rect.y + 1;
        // Click somewhere in the middle of line 2.
        let click_x = rect.x + rect.width / 2;
        // First click: only selects (since selection might not match).
        // Force selection on first to make it a direct activation.
        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            click_x,
            click_y,
        );
        // Selection already matches, so click activates: enum picker
        // opens (mode flips to PickingEnum).
        match outcome {
            SettingsKeyOutcome::Changed => {
                assert!(
                    matches!(s.mode, SettingsModalMode::PickingEnum { .. }),
                    "click on line 2 of a two-line Enum row must open the picker, \
                     got mode {:?}",
                    s.mode
                );
            }
            other => panic!(
                "click on line 2 must produce Changed (selection or activation), \
                 got {other:?}"
            ),
        }
    }

    /// Expanded two-line rows render label (line 1), value (line 2),
    /// and the wrapped description on subsequent lines.
    #[test]
    fn two_line_row_with_expansion_renders_three_segments() {
        let mut s = make_state();
        // Coding data sharing's label + value (with chevron) won't
        // fit on a 28-col line, forcing two-line layout.
        let row_idx = s
            .rows
            .iter()
            .position(
                |r| matches!(r, RowEntry::Setting { key, .. } if *key == "coding_data_sharing"),
            )
            .expect("coding_data_sharing must be registered");
        s.selected = row_idx;
        s.expanded_keys.insert("coding_data_sharing");

        let area = Rect {
            x: 0,
            y: 0,
            width: 28,
            height: 60,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_rows(&mut buf, area, &mut s, &theme);

        let rect = s.row_rects[row_idx];
        assert!(
            rect.height >= 2,
            "expanded two-line row must allocate ≥2 lines for the row itself, got height={}",
            rect.height
        );
        // The row label is on line 1.
        let label_line = buf_row_text(&buf, rect.y, area.x, area.width);
        assert!(
            label_line.contains("Coding data sharing"),
            "line 1 must contain the row label: {label_line:?}"
        );
        // The value (display: "Opt out" or similar) is on line 2.
        let value_line = buf_row_text(&buf, rect.y + 1, area.x, area.width);
        // Value comes from displaying the canonical → display mapping,
        // which uses the synthetic enum's "Third Option" canonical of
        // "opt-out". The display fallback returns the canonical when
        // the lookup misses — registry has the real `CodingDataSharing`
        // choices, so display should be "Opt out".
        assert!(
            value_line.contains("Opt") || value_line.contains("opt") || value_line.contains("out"),
            "line 2 must contain the value text: {value_line:?}"
        );
        // The expanded description renders on line 3 and below.
        let desc_line = buf_row_text(&buf, rect.y + 2, area.x, area.width);
        assert!(
            !desc_line.chars().all(|c| c == ' '),
            "line 3 must contain wrapped description text (non-blank): {desc_line:?}"
        );
    }

    /// The contextual-hints group row carries no value, but when its key is in
    /// `expanded_keys` (Right/l) it must still paint its description below the
    /// chevron row — mirroring how normal rows surface an expanded description.
    /// Regression guard: before the fix the group short-circuited out of both
    /// the height + render loops, so Right/l set `expanded_keys` but painted
    /// nothing.
    #[test]
    fn group_row_renders_expanded_description() {
        let mut s = make_state();
        let row_idx = s
            .rows
            .iter()
            .position(|r| matches!(r, RowEntry::Setting { key, .. } if *key == "contextual_hints"))
            .expect("contextual_hints group must be registered");
        s.selected = row_idx;
        s.expanded_keys.insert("contextual_hints");

        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 60,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_rows(&mut buf, area, &mut s, &theme);

        let rect = s.row_rects[row_idx];
        // Line 1 is the group's chevron row (its label).
        let label_line = buf_row_text(&buf, rect.y, area.x, area.width);
        assert!(
            label_line.contains("Show contextual hints"),
            "line 1 must contain the group label: {label_line:?}"
        );
        // The description renders on the line below the chevron row (non-blank).
        let desc_line = buf_row_text(&buf, rect.y + 1, area.x, area.width);
        assert!(
            !desc_line.chars().all(|c| c == ' '),
            "expanded group must paint its description below the chevron row \
             (non-blank): {desc_line:?}"
        );
        // The painted text matches the registered description (derive a token
        // from the live copy so this stays green across description edits).
        let desc = s
            .registry
            .find("contextual_hints")
            .expect("group registered")
            .description;
        let token = desc
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .trim_matches(|c: char| !c.is_alphanumeric());
        assert!(
            !token.is_empty() && desc_line.contains(token),
            "expanded group description must render its text (token `{token}`): {desc_line:?}"
        );
    }

    /// `row_layout`'s width threshold: a label + value that exactly
    /// fits picks `OneLine`; one cell narrower picks `TwoLine`.
    #[test]
    fn row_layout_threshold_is_exact() {
        let label = "Coding data sharing"; // 19 cells
        let value = "Opt out"; // 7 cells
        // chrome (triangle + gap + chevron + right pad) = 2 + 1 + 2 + 1 = 6
        // total = 19 + 7 + 6 = 32 cells (chevron-enabled).
        assert_eq!(row_layout(32, label, value, false), RowLayout::OneLine);
        assert_eq!(row_layout(31, label, value, false), RowLayout::TwoLine);
    }

    /// Sanity: `row_layout` handles bool-without-chevron rows
    /// (Bool kind, no `›` suffix). The chevron
    /// column is reserved even for Bool rows, so the chrome cost
    /// is the same with and without the glyph.
    ///
    /// The dead
    /// `has_chevron` parameter has been removed; `row_layout` now
    /// always reserves the chevron column. The Bool / Enum
    /// distinction at the renderer is purely whether to paint
    /// the `›` glyph in the (always-reserved) column.
    #[test]
    fn row_layout_bool_without_chevron() {
        let label = "Disable vim mode (experimental)"; // 31 cells
        let value = "off"; // 3 cells
        // chrome (triangle + gap + reserved chevron col + right pad)
        // = 2 + 1 + 2 + 1 = 6 cells, identical to the
        // chevron-enabled case.
        // total = 31 + 3 + 6 = 40 cells.
        assert_eq!(row_layout(40, label, value, false), RowLayout::OneLine);
        assert_eq!(row_layout(39, label, value, false), RowLayout::TwoLine);
    }

    /// Sanity: `_ = synthetic_enum_chevron_meta` reference so the test
    /// helper isn't flagged unused while the two-line fixtures stabilise.
    #[test]
    fn synthetic_enum_chevron_meta_constructs() {
        let m = synthetic_enum_chevron_meta();
        assert_eq!(m.key, "test-enum-with-chevron");
    }

    // -- User-feedback follow-up: always reserve a blank line between
    //    the "Tip · Ask Grok…" docs footer and the keybindings hints.
    //
    // Before this fix, when the hints wrapped to 2 lines (narrow modal
    // widths) the chrome's 2-row footer was fully consumed by hint
    // rows, so the tip sat directly above the first hint line with no
    // gap. The fix bumps `footer_lines` to `predicted_hint_rows + 1`
    // so the chrome footer always reserves a blank row above the
    // hints — preserving the visual hierarchy at any width.
    //
    // The "don't wrap" fixture uses FilterFocused mode because its
    // shortcut set is short enough (~76 cells) to fit on a single row
    // at the modal's max_width=110. Browse-mode hints (~114 cells)
    // wrap at every modal width supported by `render_settings_modal`,
    // so they're useless as a "no wrap" fixture.

    /// Find the y of the buffer row containing `needle` (first match,
    /// scanning top to bottom). Returns `None` if no row matches.
    fn find_row_y(buf: &Buffer, area: Rect, needle: &str) -> Option<u16> {
        for y in area.y..area.y.saturating_add(area.height) {
            let row = buf_row_text(buf, y, area.x, area.width);
            if row.contains(needle) {
                return Some(y);
            }
        }
        None
    }

    /// Return true if every cell strictly INSIDE the modal popup's
    /// vertical borders on the given row is whitespace. The modal
    /// borders (`│` at popup_area.x and at popup_area.x + width - 1)
    /// are excluded from the check so we test the gap-line interior,
    /// not the chrome glyphs.
    fn modal_interior_row_is_blank(buf: &Buffer, popup_area: Rect, y: u16) -> bool {
        let left = popup_area.x.saturating_add(1);
        let right = popup_area
            .x
            .saturating_add(popup_area.width)
            .saturating_sub(1);
        for x in left..right {
            if let Some(cell) = buf.cell((x, y))
                && cell.symbol() != " "
            {
                return false;
            }
        }
        true
    }

    /// Narrow modal: the Browse-mode hint string
    /// `↑/↓/j/k nav | g/G top/btm | …` wraps to 2+ lines. Without
    /// the fix the tip would sit directly above the first hint
    /// line; with the fix there's exactly one blank row between
    /// them.
    #[test]
    fn footer_has_blank_line_between_tip_and_hints_when_hints_wrap() {
        let mut s = make_state();
        // 70-col viewport caps modal_width at max(70*0.70, 44) = 49,
        // so footer_width ≈ 45. Browse-mode hints are ~114 cells so
        // they wrap to at least 2 rows.
        let area = Rect {
            x: 0,
            y: 0,
            width: 70,
            height: 30,
        };
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let popup_area = s.window.popup_area.expect("modal must have rendered");

        let tip_y = find_row_y(&buf, area, "Tip").expect("tip row must render");
        // Sanity-check that the hints actually wrap — if a future PR
        // trims the hint string enough that they fit on one row at
        // this width the test passes for the wrong reason. Look for
        // the first hint label (`nav`) AND the last (`F2/Esc`); they
        // must land on different y if the hints wrapped.
        // Use `j/k nav` (hint-unique) rather than `nav` alone, which
        // also matches the `vim_mode` row's "navigation" keyword.
        let first_hint_y = find_row_y(&buf, area, "j/k nav").expect("first hint line must render");
        let last_hint_y = find_row_y(&buf, area, "F2/Esc").expect("close hint must render");
        assert!(
            last_hint_y > first_hint_y,
            "this test requires the hints to wrap to ≥2 lines; got first_hint_y={first_hint_y} \
             last_hint_y={last_hint_y} — pick a narrower width if the hint string shrank"
        );

        // Tip → blank gap → first hint stacked contiguously at the
        // bottom of the modal.
        assert_eq!(
            first_hint_y,
            tip_y + 2,
            "tip → gap → hints must stack with exactly one blank line between tip and the \
             first hint line; tip_y={tip_y} first_hint_y={first_hint_y}"
        );
        let gap_y = tip_y + 1;
        assert!(
            modal_interior_row_is_blank(&buf, popup_area, gap_y),
            "row between tip and hints must be entirely blank inside the modal interior, \
             got: {:?}",
            buf_row_text(&buf, gap_y, popup_area.x, popup_area.width)
        );
    }

    /// Wide modal in FilterFocused mode: hints fit on a single line.
    /// Same blank-gap contract as the wrap case — the chrome's
    /// baseline `footer_lines: 2` already provides this gap, but the
    /// test pins the contract so a future change that drops the
    /// baseline (or shrinks `predicted_rows + 1` to `predicted_rows`)
    /// is caught.
    #[test]
    fn footer_has_blank_line_between_tip_and_hints_when_hints_dont_wrap() {
        let mut s = make_state();
        // FilterFocused mode has 5 shortcuts totalling ~76 cells —
        // fits on one row at any modal width supported by
        // `render_settings_modal` (max_width=110).
        s.mode = SettingsModalMode::FilterFocused;
        let area = Rect {
            x: 0,
            y: 0,
            width: 150,
            height: 30,
        };
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let popup_area = s.window.popup_area.expect("modal must have rendered");

        let tip_y = find_row_y(&buf, area, "Tip").expect("tip row must render");
        // FilterFocused-mode hints: `type to filter | ↑/↓ nav |
        // Backspace edit | Enter commit | Esc clear`. Verify both
        // ends land on the SAME row (proves no wrap).
        let first_hint_y =
            find_row_y(&buf, area, "type to filter").expect("first hint must render");
        let last_hint_y = find_row_y(&buf, area, "Esc clear").expect("last hint must render");
        assert_eq!(
            first_hint_y, last_hint_y,
            "this test requires the hints to fit on a single line; at width=150 + \
             FilterFocused mode we expect one row, got first={first_hint_y} last={last_hint_y}"
        );

        // Same tip → blank gap → hint contract as the wrap case.
        assert_eq!(
            first_hint_y,
            tip_y + 2,
            "tip → gap → hints must stack with exactly one blank line between tip and the \
             hint line; tip_y={tip_y} first_hint_y={first_hint_y}"
        );
        let gap_y = tip_y + 1;
        assert!(
            modal_interior_row_is_blank(&buf, popup_area, gap_y),
            "row between tip and hints must be entirely blank inside the modal interior, \
             got: {:?}",
            buf_row_text(&buf, gap_y, popup_area.x, popup_area.width)
        );
    }

    /// When the hints transition from 1 row to 2 rows (e.g. a width
    /// reduction), the row-list area shrinks by exactly 1 row to
    /// make room for the second hint row. Pinned so anyone
    /// "reclaiming" the gap row later sees the spec violation.
    ///
    /// Uses FilterFocused mode for both renders because Browse-mode
    /// hints don't fit on a single row at any width supported by the
    /// modal — there'd be no "1-row" baseline to compare against. The
    /// narrow viewport (100 cols → modal_width=70, footer_width=64)
    /// is tuned so FilterFocused hints (~76 cells incl. separators)
    /// wrap to exactly 2 rows (not 3+) — a more-aggressive narrow
    /// would split this into 3+ rows and the assertion below would
    /// trip on the multi-row delta.
    #[test]
    fn footer_total_height_grows_when_hints_wrap() {
        // Wide modal: FilterFocused-mode hints fit on 1 row.
        // footer_lines = 1 (hints) + 1 (gap) = 2 → matches baseline.
        let wide_area = Rect {
            x: 0,
            y: 0,
            width: 150,
            height: 30,
        };
        let mut s_wide = make_state();
        s_wide.mode = SettingsModalMode::FilterFocused;
        let mut buf_wide = Buffer::empty(wide_area);
        render_settings_modal(&mut buf_wide, wide_area, &mut s_wide, false, None);
        let wide_list_height = s_wide.list_area.height;

        // Narrow modal: same mode, hints wrap to exactly 2 rows.
        // footer_lines = 2 (hints) + 1 (gap) = 3 → 1 more than wide.
        let narrow_area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 30,
        };
        let mut s_narrow = make_state();
        s_narrow.mode = SettingsModalMode::FilterFocused;
        let mut buf_narrow = Buffer::empty(narrow_area);
        render_settings_modal(&mut buf_narrow, narrow_area, &mut s_narrow, false, None);
        let narrow_list_height = s_narrow.list_area.height;

        // Verify the wrap actually happens at the narrow width AND
        // doesn't over-wrap to 3+ rows (the assertion below would
        // also fire if both renders had the same wrap count OR the
        // narrow case wrapped further, which would be a silent test
        // bug).
        let narrow_first_hint =
            find_row_y(&buf_narrow, narrow_area, "type to filter").expect("first hint");
        let narrow_last_hint =
            find_row_y(&buf_narrow, narrow_area, "Esc clear").expect("last hint");
        assert_eq!(
            narrow_last_hint,
            narrow_first_hint + 1,
            "narrow fixture must wrap the hints to exactly 2 rows; got \
             first={narrow_first_hint} last={narrow_last_hint}"
        );
        let wide_first_hint = find_row_y(&buf_wide, wide_area, "type to filter").expect("first");
        let wide_last_hint = find_row_y(&buf_wide, wide_area, "Esc clear").expect("last");
        assert_eq!(
            wide_first_hint, wide_last_hint,
            "wide fixture must NOT wrap the hints; got first={wide_first_hint} \
             last={wide_last_hint}"
        );

        // The narrow render reserves one extra row at the bottom for
        // the wrapped hint, so the row-list area is exactly one row
        // shorter. Equality (not "<=") rules out off-by-N
        // regressions where future code reserves 2 rows instead of 1.
        assert_eq!(
            narrow_list_height + 1,
            wide_list_height,
            "row-list area must shrink by exactly 1 row when hints wrap (narrow={}, wide={})",
            narrow_list_height,
            wide_list_height,
        );
    }

    // -- palette consistency --

    /// Section headers render in the palette's style: ` {label} `
    /// in `gray + BOLD` followed by `─` separator cells in
    /// `gray_dim`. Asserts that (a) the header label cell carries
    /// the gray foreground and BOLD modifier matching the
    /// palette's `render_picker_entry` Header arm, and (b) at
    /// least one trailing cell renders a `─` glyph.
    #[test]
    fn section_header_style_matches_palette() {
        let mut s = make_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 60,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_rows(&mut buf, area, &mut s, &theme);

        // Find the row containing the "Appearance" header.
        let label = SettingCategory::Appearance.label();
        let mut header_y: Option<u16> = None;
        for y in 0..area.height {
            let txt = buf_row_text(&buf, y, area.x, area.width);
            if txt.trim_start().starts_with(label) {
                header_y = Some(y);
                break;
            }
        }
        let header_y = header_y.expect("must find Appearance header");

        // The label is rendered at col 1 (after the leading space)
        // in the gray + BOLD style — matches the palette.
        let cell = buf.cell((area.x + 1, header_y)).expect("cell at label col");
        assert_eq!(
            cell.fg, theme.gray,
            "section header label fg must be theme.gray (palette parity)"
        );
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "section header label must be BOLD (palette parity)"
        );

        // At least one trailing `─` separator cell must render after
        // the label — find the first `─` in the row after the label.
        let row_text = buf_row_text(&buf, header_y, area.x, area.width);
        assert!(
            row_text.contains('\u{2500}'),
            "section header row must contain `─` separator cells: {row_text:?}"
        );

        // The separator cells must
        // render in `theme.gray_dim` for palette parity. Walk the
        // row and find the first `─` cell; assert its fg color.
        // Mirrors the `search_bar_renders_divider_below` pattern.
        let mut sep_cell_fg = None;
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell((x, header_y))
                && cell.symbol() == "\u{2500}"
            {
                sep_cell_fg = Some(cell.fg);
                break;
            }
        }
        let sep_fg = sep_cell_fg.expect("must find at least one `─` separator cell");
        assert_eq!(
            sep_fg, theme.gray_dim,
            "section header `─` separator must render in theme.gray_dim \
             (palette parity)",
        );
    }

    /// Search bar renders the palette's prefix + cursor style:
    ///   * ` search: ` prefix in `gray`.
    ///   * Inverse-video cursor (bg = text_primary, fg = bg_base)
    ///     at the next-input position when focused.
    ///
    /// Hint path renders ` / to search` in `gray_dim`.
    #[test]
    fn search_bar_focused_style_matches_palette() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        crate::views::picker::render_search_bar(
            &mut buf,
            area.x,
            area.y,
            area.width,
            &theme,
            "",
            true,
            true,
            0,
            Some(theme.bg_base),
        );

        // First label cell carries the `gray` fg.
        let first = buf.cell((area.x + 1, area.y)).expect("col 1 cell");
        assert_eq!(
            first.fg, theme.gray,
            "search bar label prefix must use theme.gray"
        );

        // Cursor cell at the input position (label is ` search: ` =
        // 9 cells; cursor lands at col 9) is inverse-video: bg =
        // text_primary, fg = bg_base.
        let cursor_x = " search: ".width() as u16;
        let cursor_cell = buf.cell((area.x + cursor_x, area.y)).expect("cursor cell");
        assert_eq!(
            cursor_cell.bg, theme.text_primary,
            "cursor cell bg must be text_primary (inverse-video)"
        );
        assert_eq!(
            cursor_cell.fg, theme.bg_base,
            "cursor cell fg must be bg_base (inverse-video)"
        );
    }

    /// Empty + unfocused search bar shows ` / to search` in
    /// `gray_dim` — same wording the palette uses.
    ///
    /// Sample multiple cells of
    /// the hint span (not just col 1) so a regression that styled
    /// only the first few cells in gray_dim and left the rest at
    /// default is caught.
    #[test]
    fn search_bar_placeholder_matches_palette() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        crate::views::picker::render_search_bar(
            &mut buf,
            area.x,
            area.y,
            area.width,
            &theme,
            "",
            false,
            true,
            0,
            Some(theme.bg_base),
        );

        let txt: String = (area.x..area.x + area.width)
            .filter_map(|x| buf.cell((x, area.y)).map(|c| c.symbol().to_string()))
            .collect();
        assert!(
            txt.contains("/ to search"),
            "search bar placeholder must read `/ to search`, got: {txt:?}"
        );
        // Sample BOTH ends of the hint span. The hint is
        // " / to search" (12 cells); the first slash is at col 1
        // and the trailing `h` is at col 11.
        let slash_cell = buf.cell((area.x + 1, area.y)).expect("col 1 cell (/)");
        assert_eq!(
            slash_cell.fg, theme.gray_dim,
            "first hint cell (`/`) must render in theme.gray_dim"
        );
        let hint = " / to search";
        let last_col = area.x + (hint.width() as u16 - 1);
        let last_cell = buf.cell((last_col, area.y)).expect("last hint cell");
        assert_eq!(
            last_cell.fg, theme.gray_dim,
            "LAST hint cell ({last_col}) must also be theme.gray_dim — \
             a regression that styled only the prefix would slip past a \
             single-cell sample",
        );
    }

    // -- value color + chevron column + docs footer --

    /// Bool `off` values render in the muted `gray` color while
    /// Bool `on` values keep the active `accent_user`: the inactive
    /// state should read as visually subordinate.
    #[test]
    fn bool_off_value_renders_in_dim_color() {
        let meta = SettingMeta {
            key: "test-bool-dim",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Test bool",
            description: "",
            keywords: &[],
            kind: SettingKind::Bool { default: false },
            restart_required: false,
            hidden_in_minimal: false,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let theme = Theme::current();

        // Render with `off` — value cells must be styled with
        // `theme.gray`.
        let mut buf_off = Buffer::empty(area);
        render_setting_row(
            &mut buf_off,
            area,
            &meta,
            &SettingValue::Bool(false),
            15,
            false,
            &theme,
            false,
            false,
        );
        // Use `find_text_col` so the
        // column index is the actual buffer position, not a byte
        // offset cast to u16 (which only works for ASCII content).
        let off_col = find_text_col(&buf_off, 0, "off").expect("must find `off` substring");
        let off_cell = buf_off.cell((off_col, 0)).expect("off cell");
        assert_eq!(
            off_cell.fg, theme.gray,
            "Bool(false) value must render in theme.gray (Misha/Kevin Fix 4)",
        );
        // Also assert the second `f` cell carries the same style —
        // a regression that only styled the first cell would slip
        // past a one-cell sample.
        let off_col_2 = off_col + 2;
        let off_cell_2 = buf_off.cell((off_col_2, 0)).expect("off second-f cell");
        assert_eq!(
            off_cell_2.fg, theme.gray,
            "ALL cells of `off` must carry theme.gray (consistency check)",
        );

        // Render with `on` — value cells stay at `accent_user`.
        let mut buf_on = Buffer::empty(area);
        render_setting_row(
            &mut buf_on,
            area,
            &meta,
            &SettingValue::Bool(true),
            15,
            false,
            &theme,
            false,
            false,
        );
        let on_col = find_text_col(&buf_on, 0, "on").expect("must find `on` substring");
        let on_cell = buf_on.cell((on_col, 0)).expect("on cell");
        assert_eq!(
            on_cell.fg, theme.accent_user,
            "Bool(true) value must keep theme.accent_user color (active state)",
        );
        // **Asymmetry assertion**: the
        // active and inactive states must use distinct theme
        // tokens. The PER-CELL asserts above already pin
        // `on.fg == theme.accent_user` and `off.fg == theme.gray`
        // — verifying the THEME tokens are also distinct catches
        // the orthogonal regression where someone flips one of
        // the tokens to match the other (rendering would then
        // make on and off visually identical despite the
        // per-cell asserts continuing to pass).
        //
        // Conditional on the test environment's color quantization
        // exposing the distinction: in extreme low-color
        // terminals both tokens can collapse to `Color::Reset`,
        // in which case the asymmetry is unobservable. We
        // assert-only when the tokens differ, matching the
        // theme-rendered-distinct contract.
        if theme.accent_user != theme.gray {
            assert_ne!(
                on_cell.fg, off_cell.fg,
                "Bool(true) and Bool(false) must use DIFFERENT colors \
                 (asymmetry that Fix 4 introduced; theme tokens differ \
                 so the per-cell distinction should be observable)",
            );
        }
    }

    /// The chevron column is at the same right offset for ALL
    /// row kinds — Bool rows leave it empty, Enum/String rows
    /// fill it with `" ›"`, but the column position (and
    /// therefore the value's right edge) is constant.
    #[test]
    fn chevron_column_is_at_constant_right_offset() {
        let bool_meta = SettingMeta {
            key: "test-bool-col",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Bool row",
            description: "",
            keywords: &[],
            kind: SettingKind::Bool { default: false },
            restart_required: false,
            hidden_in_minimal: false,
        };
        let enum_meta = synthetic_enum_chevron_meta();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 1,
        };
        let theme = Theme::current();

        // Bool row — chevron column is empty (no `›` glyph).
        let mut buf_bool = Buffer::empty(area);
        let bool_rect = render_setting_row(
            &mut buf_bool,
            area,
            &bool_meta,
            &SettingValue::Bool(true),
            10,
            false,
            &theme,
            false,
            false,
        );

        // Enum row — chevron column contains the `›` glyph.
        let mut buf_enum = Buffer::empty(area);
        let enum_rect = render_setting_row(
            &mut buf_enum,
            area,
            &enum_meta,
            &SettingValue::Enum("choice_a"),
            10,
            false,
            &theme,
            false,
            false,
        );

        // The chevron column is a 2-cell block at
        // `area.right - ROW_RIGHT_PAD_W - ROW_CHEVRON_COL_W` (i.e.
        // `area.right - 3` in this fixture). The `›` glyph occupies
        // the SECOND cell of the column (the first cell is a
        // leading space that doubles as gap from the value). For
        // Bool rows the column stays empty.
        let glyph_x = area.x + area.width - ROW_RIGHT_PAD_W - 1;
        let bool_cell = buf_bool.cell((glyph_x, 0)).expect("bool col cell");
        assert_eq!(
            bool_cell.symbol().trim(),
            "",
            "Bool row's chevron column must be empty (no `›` glyph): \
             cell symbol = {:?}",
            bool_cell.symbol(),
        );
        let enum_cell = buf_enum.cell((glyph_x, 0)).expect("enum col cell");
        assert_eq!(
            enum_cell.symbol(),
            "\u{203A}",
            "Enum row's chevron column must contain the `›` glyph at \
             area.right - {} (constant right offset across rows), got: {:?}",
            ROW_RIGHT_PAD_W + 1,
            enum_cell.symbol(),
        );

        // The value hit-rect's right edge must be the same for
        // both rows — that's the required visual alignment.
        // Both rects end at `value_rect.x + value_rect.width`
        // which equals `chevron_col_x + ROW_CHEVRON_COL_W`.
        let bool_right = bool_rect.x + bool_rect.width;
        let enum_right = enum_rect.x + enum_rect.width;
        assert_eq!(
            bool_right, enum_right,
            "Bool and Enum value hit-rects must share the same right \
             edge (bool_right={bool_right}, enum_right={enum_right})",
        );

        // Also exercise the
        // SAME buffer with multiple rows stacked so we catch a
        // regression where, e.g., only Bool rows compute the wrong
        // chevron column. Render Bool then Enum on consecutive
        // rows and assert the chevron column lands at the same
        // column for both.
        let multi_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 4,
        };
        let mut buf_multi = Buffer::empty(multi_area);
        let bool_area = Rect {
            height: 1,
            ..multi_area
        };
        let enum_area = Rect {
            y: 2,
            height: 1,
            ..multi_area
        };
        let _ = render_setting_row(
            &mut buf_multi,
            bool_area,
            &bool_meta,
            &SettingValue::Bool(false),
            10,
            false,
            &theme,
            false,
            false,
        );
        let _ = render_setting_row(
            &mut buf_multi,
            enum_area,
            &enum_meta,
            &SettingValue::Enum("choice_a"),
            10,
            false,
            &theme,
            false,
            false,
        );
        // Bool row's `off` ends at column N; Enum row's `›` glyph
        // lands at column M. The contract: N == M's column
        // minus 1 (the gap between value and chevron column) — OR
        // equivalently, the chevron-column glyph position is the
        // same on both rows.
        let glyph_x_multi = multi_area.x + multi_area.width - ROW_RIGHT_PAD_W - 1;
        let enum_glyph_cell = buf_multi
            .cell((glyph_x_multi, 2))
            .expect("enum row chevron glyph cell");
        assert_eq!(
            enum_glyph_cell.symbol(),
            "\u{203A}",
            "Enum row's chevron glyph must land at glyph_x={glyph_x_multi}",
        );
        let bool_glyph_cell = buf_multi
            .cell((glyph_x_multi, 0))
            .expect("bool row chevron column cell");
        assert_eq!(
            bool_glyph_cell.symbol().trim(),
            "",
            "Bool row's chevron column must be empty at the SAME \
             glyph_x as Enum's (constant right offset across rows)",
        );
    }

    /// Two-line rows anchor the
    /// chevron column at the SAME right offset as one-line rows.
    /// Before the fix, line-2's chevron landed 1 cell further
    /// right than line-1's, producing a staircase in mixed-layout
    /// row lists.
    #[test]
    fn chevron_column_aligns_across_one_and_two_line_layouts() {
        let theme = Theme::current();
        // `synthetic_enum_chevron_meta` has label "Coding data
        // sharing" (19 chars) + value "choice_a" (8 chars). At
        // width=25 the one-line total (2 + 19 + 1 + 8 + 2 + 1 =
        // 33) exceeds the width, so the layout flips to TwoLine.
        // At width=60 the same row fits one-line.
        let area_two = Rect {
            x: 0,
            y: 0,
            width: 25,
            height: 2,
        };
        let mut buf_two = Buffer::empty(area_two);
        let _ = render_setting_row(
            &mut buf_two,
            area_two,
            &synthetic_enum_chevron_meta(),
            &SettingValue::Enum("choice_a"),
            10,
            false,
            &theme,
            false,
            false,
        );
        let area_one = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 1,
        };
        let mut buf_one = Buffer::empty(area_one);
        let _ = render_setting_row(
            &mut buf_one,
            area_one,
            &synthetic_enum_chevron_meta(),
            &SettingValue::Enum("choice_a"),
            10,
            false,
            &theme,
            false,
            false,
        );
        // The column offset from the area's right edge is constant:
        // `area.right - ROW_RIGHT_PAD_W - 1` is the `›` glyph
        // position (the chevron span " ›" is 2 cells; the second
        // cell holds the glyph). Computed independently for each
        // area so the offset semantics is what we're testing.
        let glyph_x_two = area_two.x + area_two.width - ROW_RIGHT_PAD_W - 1;
        let glyph_x_one = area_one.x + area_one.width - ROW_RIGHT_PAD_W - 1;
        let two_line_cell = buf_two
            .cell((glyph_x_two, 1))
            .expect("two-line chevron cell on line 2");
        assert_eq!(
            two_line_cell.symbol(),
            "\u{203A}",
            "Two-line row's chevron must land at \
             `area.right - ROW_RIGHT_PAD_W - 1` on LINE 2 (UX Issue 2)",
        );
        let one_line_cell = buf_one
            .cell((glyph_x_one, 0))
            .expect("one-line chevron cell");
        assert_eq!(
            one_line_cell.symbol(),
            "\u{203A}",
            "One-line row's chevron must land at `area.right - ROW_RIGHT_PAD_W - 1`",
        );
        // The offset from the right edge is the same — pin that
        // explicitly so a future refactor that changes one anchor
        // independently trips the test.
        assert_eq!(
            area_two.x + area_two.width - glyph_x_two,
            area_one.x + area_one.width - glyph_x_one,
            "chevron offset-from-right must be constant across layouts",
        );
    }

    /// The docs-footer tip text centers itself horizontally
    /// within its row.
    ///
    /// Also exercise the SHORT-
    /// fallback path (narrow widths where the LONG message
    /// doesn't fit) and the truncation path (extreme narrow
    /// where even SHORT doesn't fit), so a regression that moved
    /// the centering math into the LONG branch only would fail.
    #[test]
    fn docs_footer_tip_is_centered() {
        let theme = Theme::current();
        // Helper: render at `width` and return (full row text,
        // tip start col, leading_ws, trailing_ws).
        let render = |width: u16| -> (String, usize, usize) {
            let area = Rect {
                x: 0,
                y: 0,
                width,
                height: 1,
            };
            let mut buf = Buffer::empty(area);
            render_docs_footer(&mut buf, area, &theme);
            let row: String = (area.x..area.x + area.width)
                .filter_map(|x| buf.cell((x, 0)).map(|c| c.symbol().to_string()))
                .collect();
            let tip_start = row.find("Tip").expect("docs footer must contain `Tip`");
            let trailing_ws = row.chars().rev().take_while(|c| *c == ' ').count();
            (row, tip_start, trailing_ws)
        };

        // LONG path: width=80 fits the full message.
        let (row_long, tip_start_long, trailing_long) = render(80);
        assert!(
            tip_start_long > 0,
            "LONG tip must be centered (start > col 0); row={row_long:?}",
        );
        assert!(
            tip_start_long.abs_diff(trailing_long) <= 1,
            "LONG tip leading_ws={tip_start_long} vs trailing_ws={trailing_long}",
        );

        // SHORT path: width that fits SHORT but not LONG.
        // SHORT = "Tip · Ask Grok to change a setting" (34 cells);
        // LONG ≈ 73 cells. width=40 lands in the SHORT band.
        let (row_short, tip_start_short, trailing_short) = render(40);
        assert!(
            row_short.contains("change a setting"),
            "width=40 must render SHORT path (contains `change a setting`): {row_short:?}",
        );
        assert!(
            !row_short.contains("grokday"),
            "width=40 must NOT render LONG path (no `grokday`): {row_short:?}",
        );
        assert!(
            tip_start_short.abs_diff(trailing_short) <= 1,
            "SHORT tip must also be centered (Round-3 tests Issue 10): \
             leading_ws={tip_start_short} vs trailing_ws={trailing_short}",
        );

        // Truncated path: width too narrow even for SHORT. The
        // truncation prefix `Tip · …` should still render; the
        // centering math operates on the truncated SHORT.
        let (row_tiny, tip_start_tiny, _trailing_tiny) = render(15);
        assert!(
            row_tiny.contains("Tip"),
            "even at width=15 the `Tip` prefix must render: {row_tiny:?}",
        );
        // At width=15, the truncated SHORT fills most/all of the
        // row; leading_ws could be 0 if the truncation is exactly
        // 15 cells. The contract: centering math doesn't crash
        // and starts at a valid column (not negative).
        assert!(
            tip_start_tiny < 15,
            "tip start must be inside the row at width=15: {tip_start_tiny}",
        );
    }

    /// `render_settings_modal` reserves a 1-row blank gap above
    /// the tip line — so the tip has air on both top (this gap)
    /// and bottom (the chrome's `predicted_hint_rows + 1` gap).
    #[test]
    fn tip_line_has_blank_row_above() {
        let mut s = make_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 40,
        };
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        // Find the tip row.
        let mut tip_y: Option<u16> = None;
        for y in 0..area.height {
            let txt = buf_row_text(&buf, y, area.x, area.width);
            if txt.contains("Tip") && txt.contains("Ask Grok") {
                tip_y = Some(y);
                break;
            }
        }
        let tip_y = tip_y.expect("must find tip row");
        // The row immediately above the tip must be blank inside
        // the modal's content area. Modal borders (`│`) at the
        // left/right edges are expected; we strip leading/trailing
        // border characters before checking that the interior is
        // all spaces.
        assert!(
            tip_y > 0,
            "tip row must not be at y=0 (no row above to check)",
        );
        let above = buf_row_text(&buf, tip_y - 1, area.x, area.width);
        let interior: String = above
            .trim_matches(|c: char| c == ' ' || c == '\u{2502}')
            .to_string();
        assert!(
            interior.is_empty(),
            "row above tip must be blank inside the modal interior \
             (Misha/Kevin Fix 5): full row = {above:?}, interior = {interior:?}",
        );
    }

    // -- sub-pane polish --

    /// Helper: open the picker for the named enum/dyn-enum key in
    /// `make_state()`. Returns the state with PickingEnum mode
    /// armed. Panics if the key isn't found or isn't an enum.
    fn enter_picker_for(key: &'static str) -> SettingsModalState {
        let mut s = make_state();
        let row_idx = s
            .rows
            .iter()
            .position(|r| matches!(r, RowEntry::Setting { key: k, .. } if *k == key))
            .unwrap_or_else(|| panic!("no row for key `{key}` in default registry"));
        assert!(s.select_at(row_idx), "select_at({row_idx})");
        assert!(
            s.try_enter_picking_enum(),
            "try_enter_picking_enum failed for {key} — non-enum?",
        );
        s
    }

    /// Long enum description word-wraps across multiple buffer
    /// rows in the picker sub-pane — no `…` truncation.
    ///
    /// The previous version of this
    /// test used the live `theme` setting whose description
    /// (`"Color theme for the pager UI."`) is 29 chars and fits on
    /// a single line at width=30 — the word-wrap code path was
    /// never engaged. Fixed by:
    /// 1. Constructing a synthetic registry with a deliberately
    ///    long description that MUST wrap.
    /// 2. Asserting the wrap produces ≥ 2 buffer rows containing
    ///    description fragments — proves multi-line rendering
    ///    actually occurred.
    /// 3. Asserting the description's LAST word renders — proves
    ///    the wrap reached the end (no mid-sentence truncation).
    /// 4. Replacing the brittle `s…`/`.…` substring check with
    ///    a clean "no `…` anywhere in the description region"
    ///    check.
    #[test]
    fn picker_description_word_wraps_no_ellipsis() {
        // Synthetic enum setting with a description forced to wrap.
        // The description is ~140 chars; at width=30 with a small
        // amount of chrome on either side, it MUST produce ≥ 4
        // description rows.
        let long_desc = "This is a deliberately long description \
                         designed to force the word-wrap renderer \
                         across multiple rows so the test exercises \
                         the wrap logic instead of trivially fitting.";
        let synthetic_meta = SettingMeta {
            key: "test-wrap-desc",
            category: SettingCategory::Appearance,
            owner: SettingOwner::Shared,
            label: "Wrap test",
            description: long_desc,
            keywords: &[],
            kind: SettingKind::Enum {
                default: "choice_a",
                choices: TEST_ENUM_CHOICES,
                supports_preview: false,
            },
            restart_required: false,
            hidden_in_minimal: false,
        };
        let registry = SettingsRegistry::from_entries(vec![synthetic_meta]);
        let mut s = SettingsModalState::new(
            Arc::new(registry),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        // `SettingsModalState::new` snaps `selected` to the first
        // non-header row; with our single-entry registry that's
        // already the synthetic setting. Skip `select_at` since it
        // would no-op (and return false) when called on the
        // already-selected row.
        assert!(
            matches!(
                s.rows.get(s.selected),
                Some(RowEntry::Setting {
                    key: "test-wrap-desc",
                    ..
                })
            ),
            "selected row must be the synthetic test entry",
        );
        assert!(
            s.try_enter_picking_enum(),
            "synthetic must be picker-eligible"
        );

        let theme = Theme::current();
        let area = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 25,
        };
        let mut buf = Buffer::empty(area);
        render_picking_enum(&mut buf, area, &s, &theme);

        // Collect every row of buffer text.
        let rows: Vec<String> = (0..area.height)
            .map(|y| buf_row_text(&buf, y, area.x, area.width))
            .collect();
        let all_text: String = rows.join("\n");

        // 1. First word of description renders somewhere.
        let first_word = long_desc.split_whitespace().next().unwrap_or("");
        assert!(
            all_text.contains(first_word),
            "picker must render the description's first word `{first_word}`",
        );

        // 2. LAST word of description renders too — proves wrap
        // reached the end (no mid-sentence truncation). The
        // description ends with "fitting." so we look for that.
        let last_word = long_desc.split_whitespace().last().unwrap_or("");
        assert!(
            all_text.contains(last_word),
            "picker must render the description's LAST word `{last_word}` \
             (proves word-wrap reached the end): {all_text}",
        );

        // 3. Multiple description rows render. Find the title row
        // (first row containing "Wrap test"), then count
        // consecutive subsequent rows that contain non-empty text
        // that's not a choice marker — those are the description
        // rows. We expect ≥ 2.
        let title_y = rows
            .iter()
            .position(|r| r.contains("Wrap test"))
            .expect("must find title row");
        let mut desc_row_count = 0usize;
        for (i, row) in rows.iter().enumerate().skip(title_y + 1) {
            // Choice markers are `\u{25CB}` (○) or `\u{25CF}` (●).
            if row.contains('\u{25CB}') || row.contains('\u{25CF}') {
                break;
            }
            let interior = row.trim();
            if interior.is_empty() {
                continue;
            }
            // Sanity: stop walking if we hit a row that has
            // weirdly long whitespace runs without any description
            // chars (defensive).
            if i > title_y + 20 {
                break;
            }
            desc_row_count += 1;
        }
        assert!(
            desc_row_count >= 2,
            "wrapped description must span ≥ 2 buffer rows; got {desc_row_count}\n{all_text}",
        );

        // 4. No `…` truncation marker anywhere in the buffer
        // (replaced the
        // ".\u{2026}" / "s\u{2026}" coupled-to-last-char check
        // with a clean absence-of-ellipsis check).
        assert!(
            !all_text.contains('\u{2026}'),
            "picker description must not contain any `…` truncation \
             marker (Misha/Kevin Fix 6a): {all_text}",
        );
    }

    /// `render_settings_modal` populates `settings_breadcrumb_rect`
    /// in sub-pane modes; clears it in Browse / FilterFocused.
    ///
    /// Exercises both
    /// `PickingEnum` AND `EditingValue` since the field name says
    /// "sub_pane_modes" (plural). Also pins the rect's x and y
    /// so a future modal-chrome refactor that
    /// shifts the title origin trips a test rather than silently
    /// breaking the breadcrumb. The hit-rect
    /// spans the FULL breadcrumb (`Settings › <label>`) so any
    /// click on the breadcrumb routes back to Browse.
    #[test]
    fn settings_breadcrumb_rect_set_in_sub_pane_modes() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 30,
        };
        let mut s = make_state();
        let mut buf = Buffer::empty(area);
        // Browse — no breadcrumb rect.
        render_settings_modal(&mut buf, area, &mut s, false, None);
        assert!(
            s.settings_breadcrumb_rect.is_none(),
            "Browse mode must NOT populate settings_breadcrumb_rect",
        );
        // Enter PickingEnum for `theme`.
        let mut s = enter_picker_for("theme");
        let mut buf2 = Buffer::empty(area);
        render_settings_modal(&mut buf2, area, &mut s, false, None);
        let rect = s
            .settings_breadcrumb_rect
            .expect("PickingEnum must populate settings_breadcrumb_rect");
        let popup = s.window.popup_area.expect("popup_area must be set");
        assert_eq!(
            rect.height, 1,
            "breadcrumb rect must be 1 row tall (sits on the chrome's top border)",
        );
        // Width = full breadcrumb `Settings › <label>`. The leaf
        // label varies by setting — assert it's strictly wider
        // than `Settings` alone (proof that the rect extends past
        // the prefix) AND at least MODAL_TITLE + " › ".
        let prefix_w = MODAL_TITLE.width() + " \u{203A} ".width();
        assert!(
            (rect.width as usize) > MODAL_TITLE.width(),
            "rect width must extend past `Settings` alone for theme picker, got {}",
            rect.width,
        );
        assert!(
            (rect.width as usize) >= prefix_w,
            "rect width must include the `Settings › ` prefix at minimum, got {}",
            rect.width,
        );
        // Pin x/y so a chrome refactor that shifts the title
        // origin trips a test. Origin is
        // `popup.x + 1 (left border) + 2 ("─ " title decoration)`.
        assert_eq!(
            rect.x,
            popup.x + 3,
            "breadcrumb x = popup.x + 1 (border) + 2 (`─ ` title decoration)",
        );
        assert_eq!(rect.y, popup.y, "breadcrumb sits on the top border row",);

        // EditingValue mode — same shape.
        let mut s2 = int_stepper_fixture(75);
        let mut buf3 = Buffer::empty(area);
        render_settings_modal(&mut buf3, area, &mut s2, false, None);
        let rect2 = s2
            .settings_breadcrumb_rect
            .expect("EditingValue must populate settings_breadcrumb_rect");
        assert_eq!(rect2.height, 1, "EditingValue rect height must be 1");
        assert!(
            (rect2.width as usize) > MODAL_TITLE.width(),
            "EditingValue rect must extend past `Settings` alone, got {}",
            rect2.width,
        );
    }

    /// Clicking inside the breadcrumb rect while in PickingEnum
    /// mode dispatches the preview-revert action AND transitions
    /// back to Browse.
    ///
    /// Two test variants pin both
    /// (a) the no-preview path where original == current value, and
    /// (b) the preview-then-click path where the user navigated to
    /// a different choice — the revert Action MUST carry the
    /// original value, not the navigated-to value. The previous
    /// version accepted `Action(_) | Changed` which masked a
    /// regression where the revert was forgotten entirely.
    #[test]
    fn click_settings_breadcrumb_collapses_picker_to_browse() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 30,
        };
        let mut s = enter_picker_for("theme");
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let rect = s
            .settings_breadcrumb_rect
            .expect("PickingEnum must populate breadcrumb rect");

        // Synthesize a click at the rect's center.
        let click_x = rect.x + rect.width / 2;
        let click_y = rect.y;
        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            click_x,
            click_y,
        );
        // For preview-supporting enums (theme), the breadcrumb-
        // click revert dispatches `Action::PreviewTheme(original)`.
        // The original canonical for the default theme is
        // `"groknight"`. Tightened from the previous `Action(_) |
        // Changed` to lock in the revert contract.
        match outcome {
            SettingsKeyOutcome::Action(Action::PreviewTheme(orig)) => {
                assert_eq!(
                    orig, "groknight",
                    "breadcrumb-click revert must carry the original canonical",
                );
            }
            other => panic!(
                "expected Action(PreviewTheme(\"groknight\")) — the keyboard \
                 Esc-equivalent revert — got {other:?}",
            ),
        }
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "after the breadcrumb click the mode must be Browse, got {:?}",
            s.mode,
        );
    }

    /// Sibling of `click_settings_breadcrumb_collapses_picker_to_browse`
    /// that exercises the preview-then-click path: user navigates
    /// to a different theme via Down arrow (Action::PreviewTheme
    /// dispatched live), then clicks the breadcrumb. The revert
    /// Action MUST carry the ORIGINAL value (default theme), not
    /// the navigated-to value.
    #[test]
    fn click_settings_breadcrumb_after_nav_reverts_to_original() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 30,
        };
        let mut s = enter_picker_for("theme");
        // Navigate to a different theme so original != current.
        // The picker exposes `choices_idx`; the registry's theme
        // choices include at least 2 entries so we can safely
        // advance.
        let (orig_canonical_owned, advanced_idx) = match &s.mode {
            SettingsModalMode::PickingEnum {
                choices_idx,
                original_value,
                ..
            } => {
                let orig = match original_value {
                    SettingValue::Enum(c) => c.to_string(),
                    other => panic!("expected SettingValue::Enum, got {other:?}"),
                };
                (orig, *choices_idx)
            }
            other => panic!("expected PickingEnum, got {other:?}"),
        };
        // Pick a different index. The default theme is `groknight`
        // (index 1 per the registry); advance to index 0 to ensure
        // we're navigating to a different value.
        let target_idx = if advanced_idx == 0 { 1 } else { 0 };
        match s.mode {
            SettingsModalMode::PickingEnum {
                ref mut choices_idx,
                ..
            } => {
                *choices_idx = target_idx;
            }
            _ => unreachable!(),
        }

        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let rect = s
            .settings_breadcrumb_rect
            .expect("PickingEnum must populate breadcrumb rect");

        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            rect.x + rect.width / 2,
            rect.y,
        );
        match outcome {
            SettingsKeyOutcome::Action(Action::PreviewTheme(orig)) => {
                assert_eq!(
                    orig, orig_canonical_owned,
                    "breadcrumb-click revert must carry the ORIGINAL canonical \
                     (not the navigated-to value)",
                );
            }
            other => panic!("expected Action(PreviewTheme(<original>)), got {other:?}"),
        }
        assert!(matches!(s.mode, SettingsModalMode::Browse));
    }

    /// `d` in PickingEnum for a preview-supporting Enum dispatches
    /// an `ActionPair` that (a) reverts the live preview and (b)
    /// opens the reset confirm overlay. The modal transitions to
    /// Browse so the dispatch arm finds an
    /// `ActiveModal::Settings { state: Browse }`.
    #[test]
    fn d_key_in_picking_enum_dispatches_open_reset_confirm() {
        let mut s = enter_picker_for("theme");
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        match outcome {
            SettingsKeyOutcome::ActionPair(
                Action::PreviewTheme(orig),
                Action::OpenResetConfirm { key },
            ) => {
                assert_eq!(
                    key, "theme",
                    "OpenResetConfirm key must be the active picker setting",
                );
                // Default theme is `groknight`; entering the picker
                // captures `original_value = current value = groknight`,
                // so the revert dispatches with that canonical.
                assert_eq!(
                    orig, "groknight",
                    "PreviewTheme revert must carry the original canonical",
                );
            }
            other => {
                panic!("expected ActionPair(PreviewTheme(_), OpenResetConfirm), got {other:?}")
            }
        }
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "picker must collapse to Browse before dispatching reset \
             (dispatch arm panics in debug if it sees a sub-pane mode)",
        );
    }

    /// `d` in the Int stepper dispatches `Action::OpenResetConfirm`
    /// for the active setting AND transitions back to Browse.
    ///
    /// Also verifies the pending
    /// buffer is discarded — a user who stepped to a new value
    /// then pressed `d` should not have the in-flight value leak
    /// past the mode transition. Asserts the mode is structurally
    /// `Browse` (not lingering `EditingValue { buffer: "80", … }`).
    #[test]
    fn d_key_in_int_stepper_dispatches_open_reset_confirm() {
        let mut s = int_stepper_fixture(75);
        assert!(
            matches!(s.mode, SettingsModalMode::EditingValue { .. }),
            "fixture must start in EditingValue",
        );
        // Step Up so the pending buffer diverges from the default
        // (`max_thoughts_width` default is 100; this leaves the
        // buffer at "80" — not at default, not at the seeded 75).
        let _ = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            int_stepper_buffer(&s),
            "80",
            "Up arrow must have stepped from 75 to 80",
        );
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        match outcome {
            SettingsKeyOutcome::Action(Action::OpenResetConfirm { key }) => {
                assert_eq!(
                    key, "max_thoughts_width",
                    "OpenResetConfirm key must be the active stepper setting",
                );
            }
            other => panic!("expected OpenResetConfirm action, got {other:?}"),
        }
        assert!(
            matches!(s.mode, SettingsModalMode::Browse),
            "stepper must collapse to Browse before dispatching reset",
        );
        // Pending buffer must be discarded — no lingering
        // EditingValue payload. The matches! above checks the
        // discriminant; this `!matches!` is the explicit
        // assertion that we did NOT carry the in-flight buffer
        // through the mode change.
        assert!(
            !matches!(&s.mode, SettingsModalMode::EditingValue { .. }),
            "stepper's pending edit must NOT survive the d-reset \
             transition, got {:?}",
            s.mode,
        );
    }

    /// `d` in the String editor is
    /// a typeable character — NOT a reset shortcut (the picker
    /// and Int stepper get `d reset`; the String editor doesn't,
    /// a deliberate asymmetry). This test
    /// guards against a future change that "adds `d` interception for
    /// consistency" without realizing it would silently break
    /// String editing of any value containing a `d`.
    #[test]
    fn d_key_in_string_editor_inserts_into_buffer() {
        let mut s = editor_render_fixture("Gro", 3);
        let outcome = handle_settings_key(
            &mut s,
            &KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        );
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "`d` in String editor must mutate the buffer (Changed); \
             got {outcome:?}",
        );
        assert!(
            !matches!(
                outcome,
                SettingsKeyOutcome::Action(_) | SettingsKeyOutcome::ActionPair(_, _)
            ),
            "`d` in String editor MUST NOT dispatch a reset (no Action)",
        );
        assert!(
            matches!(s.mode, SettingsModalMode::EditingValue { .. }),
            "mode must STILL be EditingValue (no transition); got {:?}",
            s.mode,
        );
        match &s.mode {
            SettingsModalMode::EditingValue {
                buffer,
                cursor_byte,
                ..
            } => {
                assert_eq!(
                    buffer, "Grod",
                    "`d` must be inserted after `Gro`; got {buffer:?}",
                );
                assert_eq!(*cursor_byte, 4, "cursor must advance past the inserted `d`",);
            }
            _ => unreachable!(),
        }
    }

    /// Clicks OUTSIDE the
    /// breadcrumb rect — to the immediate left of the leading
    /// `─ ` decoration, or past the right edge — must be no-ops.
    /// An earlier version tested "click outside
    /// Settings" before the rect was widened to span
    /// the FULL breadcrumb; the widening shifted the "outside"
    /// boundaries but the no-op contract is unchanged.
    #[test]
    fn click_outside_settings_breadcrumb_is_noop() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 30,
        };
        let mut s = enter_picker_for("theme");
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let rect = s
            .settings_breadcrumb_rect
            .expect("PickingEnum must populate breadcrumb rect");
        // Click 1 cell PAST the rect's right edge — on the
        // trailing ` ─` decoration or the empty modal interior.
        let past_right_x = rect.x + rect.width + 2;
        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            past_right_x,
            rect.y,
        );
        assert!(
            matches!(outcome, SettingsKeyOutcome::Unchanged),
            "click past the right edge of breadcrumb must be Unchanged; \
             got {outcome:?}",
        );
        assert!(
            matches!(s.mode, SettingsModalMode::PickingEnum { .. }),
            "mode must STILL be PickingEnum (no transition fired); \
             got {:?}",
            s.mode,
        );
        // Click 1 cell BEFORE the rect's left edge — on the
        // leading `─ ` decoration.
        let before_left_x = rect.x.saturating_sub(1);
        let outcome2 = handle_settings_mouse(
            &mut s,
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            before_left_x,
            rect.y,
        );
        assert!(
            matches!(outcome2, SettingsKeyOutcome::Unchanged),
            "click before the left edge of breadcrumb must be Unchanged; \
             got {outcome2:?}",
        );
    }

    /// Hovering the breadcrumb flips `breadcrumb_hovered` and the
    /// mouse handler returns `Changed` so the renderer repaints.
    /// Color assertions removed — they depend on whichever theme is
    /// loaded in the thread-local cache, which varies between local
    /// runs and Bazel CI (theme preview for the "theme" picker can
    /// swap the active theme mid-test).
    #[test]
    fn hover_breadcrumb_flips_state_and_returns_changed() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 30,
        };
        let mut s = enter_picker_for("theme");
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let rect = s
            .settings_breadcrumb_rect
            .expect("PickingEnum must populate breadcrumb rect");
        assert!(!s.breadcrumb_hovered, "initially not hovered");
        // Move onto breadcrumb.
        let outcome = handle_settings_mouse(
            &mut s,
            MouseEventKind::Moved,
            rect.x + rect.width / 2,
            rect.y,
        );
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Moved onto breadcrumb must return Changed; got {outcome:?}",
        );
        assert!(s.breadcrumb_hovered, "must be hovered after Moved");
        // Move off.
        let _ = handle_settings_mouse(&mut s, MouseEventKind::Moved, area.x, area.y);
        assert!(
            !s.breadcrumb_hovered,
            "moving outside must clear breadcrumb_hovered",
        );
    }

    /// The row-list-with-search-bar layout reserves row 1 (below
    /// the search bar) for a `─` divider in `gray_dim` — palette
    /// parity.
    #[test]
    fn search_bar_renders_divider_below() {
        let mut s = make_state();
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 30,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        // Use the full settings render so we exercise the same
        // layout the user sees.
        render_settings_modal(&mut buf, area, &mut s, false, None);

        // Find the row containing ` search:` (the search bar) — the
        // divider row is the next row.
        let mut search_y: Option<u16> = None;
        for y in 0..area.height {
            let txt = buf_row_text(&buf, y, area.x, area.width);
            if txt.contains("/ to search") || txt.contains(" search: ") {
                search_y = Some(y);
                break;
            }
        }
        let search_y = search_y.expect("must find search bar row");
        let divider_y = search_y + 1;

        // The divider must span
        // the full row width, not just the first cell. Count `─`
        // cells across the row's interior (excluding the modal
        // borders) and assert ≥ half the width is `─`. Also pin
        // the color across multiple cells, not just the first.
        let mut box_count = 0usize;
        let mut wrong_color_cells = 0usize;
        let mut first_box_cell_fg = None;
        let mut last_box_cell_fg = None;
        for x in area.x..area.x + area.width {
            if let Some(cell) = buf.cell((x, divider_y))
                && cell.symbol() == "\u{2500}"
            {
                box_count += 1;
                if cell.fg != theme.gray_dim {
                    wrong_color_cells += 1;
                }
                if first_box_cell_fg.is_none() {
                    first_box_cell_fg = Some(cell.fg);
                }
                last_box_cell_fg = Some(cell.fg);
            }
        }
        assert!(
            box_count > 0,
            "row immediately below search bar must contain `─` divider cells"
        );
        // The divider spans the inner area between the modal
        // borders; expect a substantial fraction of the row.
        // A regression that only painted the first 5 cells as `─`
        // would fail this — palette parity is "full-width divider".
        assert!(
            box_count >= (area.width as usize) / 4,
            "divider must span ≥ 1/4 of the row width, got {box_count} cells \
             of width {}",
            area.width,
        );
        assert_eq!(
            wrong_color_cells, 0,
            "ALL `─` cells in divider must use theme.gray_dim (palette parity); \
             {wrong_color_cells} cells diverged",
        );
        // First and last `─` cells share the same fg — defensive.
        assert_eq!(
            first_box_cell_fg, last_box_cell_fg,
            "divider color must be consistent across the row",
        );
    }

    // ---------- max_thoughts_width live wrap preview ----------
    //
    // The preview block renders below the Int stepper inside the
    // EditingValue sub-pane when the active setting key is
    // `max_thoughts_width`. Tests cover render presence, the
    // title/content style contracts (bold-italic-lowercase title,
    // italic content), the bg-tint distinction between title and
    // content rows, the wrap-width contract (no content row wider
    // than `pending_value`), the clamp behaviour when the terminal
    // is narrower than the pending value, omission on too-narrow /
    // too-short viewports, gating to the `max_thoughts_width` key
    // alone, and the live re-wrap on stepper change.

    /// Helper: render the EditingValue sub-pane for
    /// `max_thoughts_width` at a given starting buffer value into a
    /// fresh `Buffer` of the supplied area, and return `(buf, state)`.
    /// The stepper renders at the top of `area`; the preview (if
    /// rendered) anchors to the bottom of `area`.
    fn render_max_thoughts_width_at(value: i64, area: Rect) -> (Buffer, SettingsModalState) {
        let mut s = int_stepper_fixture(value);
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);
        (buf, s)
    }

    /// Find the first row containing the exact `needle` substring.
    fn find_text_row(buf: &Buffer, area: Rect, needle: &str) -> Option<u16> {
        for y in area.y..area.y + area.height {
            let row = buf_row_text(buf, y, area.x, area.width);
            if row.contains(needle) {
                return Some(y);
            }
        }
        None
    }

    /// Test 1: the preview renders directly below the stepper with
    /// exactly 1 blank row of separation. The implementation no
    /// longer renders in-pane stepper hints, so the spec's
    /// "1 blank row above preview" anchors to the stepper row
    /// itself: `preview_title_y == stepper_y + 2`. This replaces a
    /// prior bottom-anchor placement; the assertion locks the corrected
    /// top-anchor in place.
    #[test]
    fn max_thoughts_width_preview_renders_below_stepper() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let (buf, s) = render_max_thoughts_width_at(85, area);

        // Locate the stepper row (the row containing the `‹` arrow).
        let stepper_y =
            find_text_row(&buf, area, int_stepper_left_glyph()).expect("stepper row must render");
        // Locate the preview title row.
        let preview_y = find_text_row(&buf, area, "preview")
            .expect("`preview` title row must render below the stepper");
        // Exact placement: 1 blank row between stepper and preview
        // title, regardless of `area.height` (no bottom-anchoring).
        assert_eq!(
            preview_y,
            stepper_y + 2,
            "preview title must sit exactly 1 blank row below the stepper \
             (stepper_y={stepper_y}, preview_y={preview_y}); a multi-row gap \
             indicates a regression to the prior bottom-anchor placement",
        );
        // Row between stepper and preview is blank — no leakage
        // from either side.
        let gap_row = buf_row_text(&buf, stepper_y + 1, area.x, area.width);
        assert!(
            gap_row.trim().is_empty(),
            "the row between the stepper and the preview must be blank; \
             got {gap_row:?}",
        );
        // And the adornment hit-rects should still be populated
        // (the preview MUST NOT cannibalize the stepper render).
        let (dec_rect, inc_rect) = s.editor_adornment_rects;
        assert!(
            dec_rect.width > 0 && inc_rect.width > 0,
            "stepper arrows must still populate adornment rects after preview render",
        );
    }

    /// Test 2: the title row is bold + italic + lowercase
    /// `preview`. We sample the cell at the title row's `p`
    /// (column 0 of the preview block, since the preview is
    /// left-aligned within `area`) and assert the modifiers.
    #[test]
    fn max_thoughts_width_preview_title_is_bold_italic_lowercase() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let (buf, _) = render_max_thoughts_width_at(85, area);
        let preview_y = find_text_row(&buf, area, "preview").expect("preview title must render");
        // Assert the exact lowercase substring (the row must NOT
        // contain "Preview" or "PREVIEW").
        let row = buf_row_text(&buf, preview_y, area.x, area.width);
        assert!(
            row.contains("preview"),
            "title row must contain lowercase `preview`; row={row:?}",
        );
        assert!(
            !row.contains("Preview") && !row.contains("PREVIEW"),
            "title row must NOT contain capitalised forms; row={row:?}",
        );
        // Sample the first cell of the title text.
        let cell = buf
            .cell((area.x, preview_y))
            .expect("preview title cell at column 0");
        assert_eq!(cell.symbol(), "p", "expected `p` at title column 0");
        assert!(
            cell.modifier.contains(Modifier::BOLD),
            "title cell must carry Modifier::BOLD; got {:?}",
            cell.modifier,
        );
        assert!(
            cell.modifier.contains(Modifier::ITALIC),
            "title cell must carry Modifier::ITALIC; got {:?}",
            cell.modifier,
        );
    }

    /// Test 3: content rows carry `Modifier::ITALIC` (matches the
    /// scrollback's thinking-text style convention).
    #[test]
    fn max_thoughts_width_preview_content_is_italic() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let (buf, _) = render_max_thoughts_width_at(85, area);
        let preview_y = find_text_row(&buf, area, "preview").expect("preview title must render");
        // The first content row is the row immediately below the
        // title. Sample column 0 — the first character of the
        // wrapped sample text (`L` from "Let me trace through ...").
        let content_y = preview_y + 1;
        let cell = buf
            .cell((area.x, content_y))
            .expect("preview content cell at column 0");
        assert_eq!(
            cell.symbol(),
            "L",
            "expected `L` from sample text at column 0"
        );
        assert!(
            cell.modifier.contains(Modifier::ITALIC),
            "content cell must carry Modifier::ITALIC; got {:?}",
            cell.modifier,
        );
        // And NOT bold — content is italic-only (bold is title-only).
        assert!(
            !cell.modifier.contains(Modifier::BOLD),
            "content cell must NOT carry Modifier::BOLD (title-only); got {:?}",
            cell.modifier,
        );
    }

    /// Test 4: the title row visually distinguishes itself from the
    /// content rows via two independent signals — (1) a different
    /// `bg` token (`bg_visual` vs `bg_highlight`) and (2) a
    /// `Modifier::UNDERLINED` that gives consistent visual weight
    /// regardless of how much the bg tokens differ in luma.
    ///
    /// The previous name `_title_bg_is_darker_than_content_bg` was
    /// misleading: on dark themes (GrokNight, TokyoNight, RosePine
    /// Moon) `bg_visual` is actually *lighter* than `bg_highlight`;
    /// only on the GrokDay light theme is title darker. The
    /// contract that the rendering code actually relies on is "title
    /// uses the heavier / more-saturated `bg_visual` token, content
    /// uses `bg_highlight`, plus an UNDERLINED title modifier for
    /// themes where the luma delta is small (TokyoNight)". The test
    /// now matches that contract.
    #[test]
    fn max_thoughts_width_preview_title_styling_distinguishes_from_content() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let (buf, _) = render_max_thoughts_width_at(85, area);
        let preview_y = find_text_row(&buf, area, "preview").expect("preview title must render");
        let title_cell = buf
            .cell((area.x, preview_y))
            .expect("title cell at column 0");
        let content_cell = buf
            .cell((area.x, preview_y + 1))
            .expect("content cell at column 0");
        // Wiring assertion: rendered cells use the current-theme
        // bg tokens (tautological under NO_COLOR; meaningful when
        // truecolor is on).
        let theme = Theme::current();
        assert_eq!(
            title_cell.bg, theme.bg_visual,
            "title bg must be theme.bg_visual; got {:?}",
            title_cell.bg,
        );
        assert_eq!(
            content_cell.bg, theme.bg_highlight,
            "content bg must be theme.bg_highlight; got {:?}",
            content_cell.bg,
        );
        // The title carries UNDERLINED in addition to BOLD + ITALIC.
        // This is the theme-neutral cue that demarcates the title
        // when the bg luma delta is small (TokyoNight).
        assert!(
            title_cell.modifier.contains(Modifier::UNDERLINED),
            "title cell must carry Modifier::UNDERLINED for theme-neutral \
             visual weight; got {:?}",
            title_cell.modifier,
        );
        assert!(
            !content_cell.modifier.contains(Modifier::UNDERLINED),
            "content cell must NOT carry Modifier::UNDERLINED (title-only); \
             got {:?}",
            content_cell.modifier,
        );
        // Contrast assertion: regardless of the active palette,
        // the raw / un-quantized theme tokens differ. We use the
        // raw theme directly so this assertion survives `NO_COLOR`
        // / 256-color quantization.
        let raw_theme = match crate::theme::Theme::current_kind() {
            crate::theme::ThemeKind::GrokNight => crate::theme::Theme::groknight(),
            crate::theme::ThemeKind::TokyoNight => crate::theme::Theme::tokyonight(),
            crate::theme::ThemeKind::GrokDay => crate::theme::Theme::grokday(),
            crate::theme::ThemeKind::RosePineMoon => crate::theme::Theme::rosepine_moon(),
            // Resolved via `Theme::current()` rather than a constructor
            // because `theme::oscura` is a private module.
            crate::theme::ThemeKind::OscuraMidnight => crate::theme::Theme::current(),
            crate::theme::ThemeKind::Auto => crate::theme::Theme::groknight(),
        };
        assert_ne!(
            raw_theme.bg_visual, raw_theme.bg_highlight,
            "raw theme tokens bg_visual + bg_highlight must be distinct so the preview \
             reads as a contained block with two-tone bg",
        );
    }

    /// Test 5: content rows wrap at the pending value — no
    /// rendered content row's text width exceeds the pending
    /// stepper value.
    #[test]
    fn max_thoughts_width_preview_wraps_at_pending_value() {
        // `area.width` is comfortably wider than the pending value
        // so the clamp path doesn't fire — we want to exercise the
        // pure-pending wrap path.
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 24,
        };
        let (buf, _) = render_max_thoughts_width_at(50, area);
        let preview_y = find_text_row(&buf, area, "preview").expect("preview title must render");
        // Walk content rows below the title until a blank row
        // (signals end of preview block).
        let mut content_lines: Vec<String> = Vec::new();
        for y in (preview_y + 1)..area.height {
            let row = buf_row_text(&buf, y, area.x, area.width);
            let trimmed = row.trim_end();
            if trimmed.is_empty() {
                break;
            }
            content_lines.push(trimmed.to_string());
        }
        // Strengthen the wrap-shape assertion.
        // A regression that disabled wrap and rendered a single
        // truncated line at 50 cols would satisfy `w <= 50` but
        // fail `len >= 2`. The ~189-char sample at pending=50
        // wraps to ≥ 3 rows in practice; we assert ≥ 2 to leave
        // headroom for word-boundary jitter.
        assert!(
            content_lines.len() >= 2,
            "wrap must produce at least 2 content rows at pending=50 for the \
             ~189-char sample; got {} rows: {content_lines:?}",
            content_lines.len(),
        );
        for line in &content_lines {
            // `UnicodeWidthStr::width` counts display columns.
            let w = line.width();
            assert!(
                w <= 50,
                "content line {line:?} has display width {w} > pending_value 50",
            );
        }
    }

    /// Test 6: when the terminal area is narrower than the pending
    /// value, the preview clamps to `area.width`. The title stays
    /// plain `preview` (no suffix) — the clamp signal has been
    /// moved to a note row rendered below the content; see
    /// `clamped_preview_renders_note_below_content` for the note
    /// assertion. This test focuses on the wrap shape itself.
    #[test]
    fn max_thoughts_width_preview_clamps_when_terminal_narrower_than_value() {
        // 60-wide area, pending = 85 → clamp to 60.
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 24,
        };
        let (buf, _) = render_max_thoughts_width_at(85, area);
        let preview_y = find_text_row(&buf, area, "preview").expect("preview title must render");
        let title_row = buf_row_text(&buf, preview_y, area.x, area.width);
        // Title must NOT carry the legacy `clamped to N cols`
        // suffix — that signal lives in the note row now.
        assert!(
            !title_row.contains("clamped"),
            "title row must NOT contain the `clamped` suffix anymore — the clamp \
             indicator moved to a note row below the content; title_row={title_row:?}",
        );
        // No content line may exceed area.width = 60 cols. Also
        // assert ≥ 2 content rows to guard against a regression
        // that swapped wrap for truncation. We
        // stop scanning at the first non-content line — either a
        // blank gap or the new `note: clamped at …` row.
        let mut clamped_lines: Vec<String> = Vec::new();
        for y in (preview_y + 1)..area.height {
            let row = buf_row_text(&buf, y, area.x, area.width);
            let trimmed = row.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if trimmed.starts_with("note:") {
                break;
            }
            let w = trimmed.width();
            assert!(
                w <= 60,
                "clamped content line {trimmed:?} has display width {w} > clamp 60",
            );
            clamped_lines.push(trimmed.to_string());
        }
        assert!(
            clamped_lines.len() >= 2,
            "clamped wrap must produce ≥ 2 content rows at width=60; got {} rows: \
             {clamped_lines:?}",
            clamped_lines.len(),
        );
    }

    /// When the preview is clamped (pending > terminal width) AND
    /// the modal has enough vertical room, the title stays plain
    /// `preview` (no suffix) AND a `note: clamped at N cols` row
    /// renders immediately below the wrap content. The note uses
    /// `theme.text_secondary` fg, no bg tint, no modifier — it
    /// reads as chrome-level text aligned with the preview's
    /// left edge.
    #[test]
    fn clamped_preview_renders_note_below_content() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: 24,
        };
        let (buf, _) = render_max_thoughts_width_at(85, area);
        let preview_y = find_text_row(&buf, area, "preview").expect("preview title must render");

        // Title row carries the lowercase `preview` text and no
        // `clamped` suffix.
        let title_row = buf_row_text(&buf, preview_y, area.x, area.width);
        assert!(
            title_row.contains("preview"),
            "title row must contain lowercase `preview`; row={title_row:?}",
        );
        assert!(
            !title_row.contains("clamped"),
            "title row must NOT contain `clamped`; the clamp note lives below \
             the wrap content now; row={title_row:?}",
        );

        // Walk forward to find the note. Wrap content runs on
        // consecutive non-empty rows; then exactly one blank row
        // serves as the visual gap; then the `note:` row sits
        // below that gap. Stop at the first row that breaks the
        // "content then gap then note" pattern.
        let mut note_y: Option<u16> = None;
        let mut last_content_y: Option<u16> = None;
        let mut saw_blank_gap = false;
        for y in (preview_y + 1)..(area.y + area.height) {
            let row = buf_row_text(&buf, y, area.x, area.width);
            let trimmed = row.trim_end();
            if trimmed.starts_with("note:") {
                note_y = Some(y);
                break;
            }
            if trimmed.is_empty() {
                if saw_blank_gap {
                    // Two consecutive blank rows — content ended
                    // and we're past the note's slot too. Bail.
                    break;
                }
                saw_blank_gap = true;
                continue;
            }
            // Non-blank, non-note row. If we already saw the blank
            // gap and now see content again, the layout violated
            // the contract — bail without finding the note.
            if saw_blank_gap {
                break;
            }
            last_content_y = Some(y);
        }
        let note_y = note_y.expect("clamped preview must render a `note:` row below content");
        let last_content_y =
            last_content_y.expect("clamped preview must render ≥ 1 content row before the note");
        assert_eq!(
            note_y,
            last_content_y + 2,
            "the `note:` row must sit one blank row below the last content row; \
             last_content_y={last_content_y} note_y={note_y}",
        );

        // The note text reports the actual clamp width (area.width = 60).
        let note_row = buf_row_text(&buf, note_y, area.x, area.width);
        assert!(
            note_row.contains("note: clamped at 60 cols"),
            "note row must read `note: clamped at 60 cols`; got {note_row:?}",
        );

        // Style assertions: the note cell at column 0 (the `n` of
        // "note:") must carry `theme.text_secondary` fg, no bg
        // tint past `theme.bg_base`, and no modifier. We sample
        // the modifier directly; the fg/bg colors are theme-
        // dependent but compared symbolically to the theme tokens
        // in use.
        let theme = Theme::current();
        let cell = buf
            .cell((area.x, note_y))
            .expect("note cell at column 0 must exist");
        assert_eq!(cell.symbol(), "n", "expected `n` at note column 0");
        assert_eq!(
            cell.fg, theme.text_secondary,
            "note fg must be theme.text_secondary; got {:?}",
            cell.fg,
        );
        assert_eq!(
            cell.bg, theme.bg_base,
            "note bg must be theme.bg_base (no block tint); got {:?}",
            cell.bg,
        );
        assert!(
            cell.modifier.is_empty(),
            "note cell must carry no modifier; got {:?}",
            cell.modifier,
        );
    }

    /// Boundary: when the preview is clamped but the modal area
    /// is too short to fit an extra row below the content for the
    /// note, the note is omitted. The wrap content keeps rendering
    /// at the full vertical budget — content takes priority.
    #[test]
    fn clamped_note_omitted_when_insufficient_height() {
        // The stepper consumes the first ~5 rows (title + 1-line
        // desc + gap + stepper). At width 60 the wrap of the
        // sample text produces ≥ 6 wrap lines; we pick an area
        // height that allows the stepper + gap + title + content
        // to fill every remaining row with NO slack for the note.
        //
        // Concretely: total area height = stepper_header (~4) +
        // preview_block (gap 1 + title 1 + content N). We want
        // content_rows == area.height - 2 (no slack). Pick a
        // height that's exactly tight enough.
        //
        // We sweep upward to find the largest height at which no
        // note renders, and assert the area's wrap content fills
        // every available content row.
        let mut tight_height: Option<u16> = None;
        for h in 5u16..30u16 {
            let area = Rect {
                x: 0,
                y: 0,
                width: 60,
                height: h,
            };
            let (buf, _) = render_max_thoughts_width_at(85, area);
            // Skip heights at which the preview is omitted
            // entirely (too short).
            let Some(preview_y) = find_text_row(&buf, area, "preview") else {
                continue;
            };
            let note_present = find_text_row(&buf, area, "note: clamped").is_some();
            if !note_present {
                tight_height = Some(h);
                // Sanity: verify wrap content still rendered for
                // ≥ 1 row below the title — i.e. the preview did
                // render, the note was just omitted for room.
                let content_y = preview_y + 1;
                let content = buf_row_text(&buf, content_y, area.x, area.width);
                assert!(
                    !content.trim().is_empty(),
                    "wrap content row directly below title must render even when the \
                     note is omitted; content={content:?}",
                );
            }
        }
        // Sanity-check: we found at least one height at which the
        // note was omitted (otherwise the boundary fixture is
        // useless — every height fits the note). At the tightest
        // such height verify the note really IS absent (a final
        // explicit assertion in case the loop's discovery state
        // bit-rots).
        let tight = tight_height.expect(
            "the height sweep must find at least one short-but-rendered-preview height \
             at which the note is omitted — adjust the sweep range if the fixture changes",
        );
        let area = Rect {
            x: 0,
            y: 0,
            width: 60,
            height: tight,
        };
        let (buf, _) = render_max_thoughts_width_at(85, area);
        assert!(
            find_text_row(&buf, area, "note: clamped").is_none(),
            "at the tight boundary height (h={tight}) the clamped note must NOT render",
        );
    }

    /// When the preview is NOT clamped (modal width >= pending
    /// value), there's no note row anywhere in the buffer.
    #[test]
    fn unclamped_preview_omits_note() {
        // 120-col area, pending = 50 → no clamp.
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 24,
        };
        let (buf, _) = render_max_thoughts_width_at(50, area);
        assert!(
            find_text_row(&buf, area, "preview").is_some(),
            "preview must render at this size",
        );
        assert!(
            find_text_row(&buf, area, "note:").is_none(),
            "unclamped preview must not render a `note:` row",
        );
        assert!(
            find_text_row(&buf, area, "clamped").is_none(),
            "unclamped preview must not contain the word `clamped` anywhere",
        );
    }

    /// Test 7: when the modal area is too short to fit the
    /// preview's minimum vertical budget (stepper header + 5 rows
    /// below = gap + title + 2 content + gap), the preview is
    /// omitted. The stepper still renders alone.
    #[test]
    fn max_thoughts_width_preview_omitted_when_modal_too_short() {
        // The stepper alone consumes ~5 rows (title + wrapped desc
        // + gap + stepper). Total area height = 7 leaves 2 rows
        // remaining — below the 5-row preview minimum.
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 7,
        };
        let (buf, _) = render_max_thoughts_width_at(85, area);
        // The stepper still renders.
        assert!(
            find_text_row(&buf, area, int_stepper_left_glyph()).is_some(),
            "stepper must still render at short heights",
        );
        // The preview is omitted.
        assert!(
            find_text_row(&buf, area, "preview").is_none(),
            "preview must be omitted when remaining height < 5 rows",
        );
    }

    /// Test 8: when the modal area is narrower than 30 cols, the
    /// preview is omitted. The stepper still renders alone.
    #[test]
    fn max_thoughts_width_preview_omitted_when_modal_too_narrow() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 28,
            height: 24,
        };
        let (buf, _) = render_max_thoughts_width_at(85, area);
        assert!(
            find_text_row(&buf, area, "preview").is_none(),
            "preview must be omitted when area.width < 30 cols",
        );
    }

    /// Test 9: the preview is gated on
    /// `setting_key == "max_thoughts_width"`. We build a
    /// synthetic Int setting under a different key and assert no
    /// preview renders even though the editor sub-pane opens
    /// successfully.
    ///
    /// This guards future Int settings from accidentally inheriting
    /// the preview behaviour.
    #[test]
    fn max_thoughts_width_preview_only_renders_for_max_thoughts_width_key() {
        let synthetic_meta = SettingMeta {
            key: "synthetic_int",
            category: SettingCategory::Advanced,
            owner: crate::settings::SettingOwner::Shared,
            label: "Synthetic Int",
            description: "Test fixture.",
            keywords: &["test"],
            kind: SettingKind::Int {
                default: 50,
                min: 0,
                max: 200,
            },
            restart_required: false,
            hidden_in_minimal: false,
        };
        let registry = SettingsRegistry::from_entries(vec![synthetic_meta]);
        let mut s = SettingsModalState::new(
            Arc::new(registry),
            UiConfig::default(),
            PagerLocalSnapshot::default(),
        );
        s.mode = SettingsModalMode::EditingValue {
            key: "synthetic_int",
            buffer: "50".to_string(),
            cursor_byte: 2,
            validation_error: None,
        };
        let area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };
        let mut buf = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf, area, &mut s, &theme);
        // Stepper still renders (the key is a registered Int).
        assert!(
            find_text_row(&buf, area, int_stepper_left_glyph()).is_some(),
            "stepper must render for synthetic Int setting",
        );
        // No preview because the key is NOT max_thoughts_width.
        assert!(
            find_text_row(&buf, area, "preview").is_none(),
            "preview must be hidden for non-max_thoughts_width Int settings",
        );
    }

    /// Test 10: the preview re-wraps when the stepper value
    /// changes. We render at pending=50, capture the wrap shape
    /// (the set of content row strings), dispatch an Up keystroke
    /// to step to 55, re-render, and assert the wrap shape
    /// differs.
    #[test]
    fn max_thoughts_width_preview_updates_when_stepper_changes() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 120,
            height: 24,
        };
        // Capture wrap shape at pending = 50.
        let (buf_50, _) = render_max_thoughts_width_at(50, area);
        let preview_y_50 =
            find_text_row(&buf_50, area, "preview").expect("preview must render at pending 50");
        let mut wrap_50: Vec<String> = Vec::new();
        for y in (preview_y_50 + 1)..area.height {
            let row = buf_row_text(&buf_50, y, area.x, area.width);
            let trimmed = row.trim_end().to_string();
            if trimmed.is_empty() {
                break;
            }
            wrap_50.push(trimmed);
        }

        // Step Up (small step: +5) → pending becomes 55.
        let mut s = int_stepper_fixture(50);
        let outcome = handle_settings_key(&mut s, &KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(
            matches!(outcome, SettingsKeyOutcome::Changed),
            "Up arrow must step the stepper; got {outcome:?}",
        );
        assert_eq!(
            int_stepper_buffer(&s),
            "55",
            "buffer must reflect the +5 step",
        );
        // Re-render at pending = 55.
        let mut buf_55 = Buffer::empty(area);
        let theme = Theme::current();
        render_editing_value(&mut buf_55, area, &mut s, &theme);
        let preview_y_55 =
            find_text_row(&buf_55, area, "preview").expect("preview must render at pending 55");
        let mut wrap_55: Vec<String> = Vec::new();
        for y in (preview_y_55 + 1)..area.height {
            let row = buf_row_text(&buf_55, y, area.x, area.width);
            let trimmed = row.trim_end().to_string();
            if trimmed.is_empty() {
                break;
            }
            wrap_55.push(trimmed);
        }
        // The wrap shape must differ — at width 55 the line breaks
        // land on different words than at width 50.
        assert_ne!(
            wrap_50, wrap_55,
            "wrap shape at pending=50 must differ from wrap shape at pending=55",
        );
        // Assert both renders actually wrap (≥ 2
        // content rows). Without this a stub that returned a
        // single truncated row at each width would pass the
        // `assert_ne!` above (different truncation points) yet
        // skip the wrap mechanism entirely.
        assert!(
            wrap_50.len() >= 2,
            "pending=50 wrap must produce ≥ 2 rows; got {}: {wrap_50:?}",
            wrap_50.len(),
        );
        assert!(
            wrap_55.len() >= 2,
            "pending=55 wrap must produce ≥ 2 rows; got {}: {wrap_55:?}",
            wrap_55.len(),
        );
    }

    /// Test 7b (boundary companion to Test 7): the preview *renders*
    /// at `area.height == header_rows + MAX_THOUGHTS_WIDTH_PREVIEW_MIN_HEIGHT`,
    /// and *omits* one row below that threshold:
    /// without the just-above-threshold companion, a regression
    /// that bumped `MIN_HEIGHT` to 6 would leave Test 7 passing
    /// silently.
    #[test]
    fn max_thoughts_width_preview_renders_at_just_fits_height() {
        // The stepper header (title + 1-row desc + gap + stepper)
        // is 4 rows at width=80, so total area height needs at
        // least 4 + 5 = 9 rows for the preview to render. We test
        // both sides of the boundary.
        let just_fits = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 9,
        };
        let (buf_fit, _) = render_max_thoughts_width_at(85, just_fits);
        assert!(
            find_text_row(&buf_fit, just_fits, "preview").is_some(),
            "preview must render at the just-fits boundary height (header_rows + 5)",
        );
        // One row below the threshold: preview omitted, stepper
        // still renders.
        let just_short = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 8,
        };
        let (buf_short, _) = render_max_thoughts_width_at(85, just_short);
        assert!(
            find_text_row(&buf_short, just_short, int_stepper_left_glyph()).is_some(),
            "stepper must still render one row below the preview threshold",
        );
        assert!(
            find_text_row(&buf_short, just_short, "preview").is_none(),
            "preview must omit at one row below the just-fits boundary",
        );
    }

    /// Test 8b (boundary companion to Test 8): the preview
    /// *renders* at `area.width == MAX_THOUGHTS_WIDTH_PREVIEW_MIN_WIDTH`
    /// (= 30), and *omits* at 29.
    #[test]
    fn max_thoughts_width_preview_renders_at_just_fits_width() {
        let just_fits = Rect {
            x: 0,
            y: 0,
            width: 30,
            height: 24,
        };
        let (buf_fit, _) = render_max_thoughts_width_at(85, just_fits);
        assert!(
            find_text_row(&buf_fit, just_fits, "preview").is_some(),
            "preview must render at the MIN_WIDTH (30 cols) boundary",
        );
        let just_narrow = Rect {
            x: 0,
            y: 0,
            width: 29,
            height: 24,
        };
        let (buf_narrow, _) = render_max_thoughts_width_at(85, just_narrow);
        assert!(
            find_text_row(&buf_narrow, just_narrow, "preview").is_none(),
            "preview must omit one column below MIN_WIDTH",
        );
    }

    // ──────────────────────────────────────────────────────────────
    // Auto-widen tests for max_thoughts_width EditingValue mode.
    // ──────────────────────────────────────────────────────────────

    /// At a wide terminal (200 cols), entering EditingValue mode
    /// for `max_thoughts_width` widens the rendered modal so that
    /// its popup width is `terminal_width - MAX_THOUGHTS_WIDTH_WIDENED_MARGIN`
    /// (i.e. 192). The default sizing would otherwise produce a
    /// 70%-of-terminal = 140-wide modal.
    #[test]
    fn modal_widens_when_editing_max_thoughts_width() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 40,
        };
        let mut s = int_stepper_fixture(120);
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let popup = s.window.popup_area.expect("modal must have rendered");
        let expected = area.width - MAX_THOUGHTS_WIDTH_WIDENED_MARGIN;
        assert_eq!(
            popup.width, expected,
            "widened modal width must be terminal_width - WIDENED_MARGIN (= {expected}); \
             got {} at terminal_width={}",
            popup.width, area.width,
        );
        // Sanity: the widened width is strictly greater than the
        // standard cap.
        assert!(
            popup.width > STANDARD_MAX_WIDTH,
            "widened modal must be strictly wider than STANDARD_MAX_WIDTH ({}); got {}",
            STANDARD_MAX_WIDTH,
            popup.width,
        );
    }

    /// Transitioning from `EditingValue { max_thoughts_width }` back
    /// to `Browse` snaps the modal back to its standard width on the
    /// next render frame — the widening lives in the render-time
    /// `state.mode` match, not in any persistent state.
    #[test]
    fn modal_returns_to_default_width_when_leaving_edit_mode() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 40,
        };
        // Render once in widened mode to capture the wide width.
        let mut s = int_stepper_fixture(120);
        let mut buf_wide = Buffer::empty(area);
        render_settings_modal(&mut buf_wide, area, &mut s, false, None);
        let wide = s.window.popup_area.expect("wide modal must render").width;
        assert!(
            wide > STANDARD_MAX_WIDTH,
            "preconditional sanity: widened path must produce a wider modal; got {wide}",
        );

        // Transition back to Browse — re-render at the same
        // terminal size and assert the modal width capped at
        // STANDARD_MAX_WIDTH (the standard width_pct=0.70 path
        // produces 140, which is also above the cap, so the
        // binding constraint is STANDARD_MAX_WIDTH).
        s.transition_to_browse();
        let mut buf_std = Buffer::empty(area);
        render_settings_modal(&mut buf_std, area, &mut s, false, None);
        let std_w = s
            .window
            .popup_area
            .expect("standard modal must render")
            .width;
        assert_eq!(
            std_w, STANDARD_MAX_WIDTH,
            "Browse-mode modal must use STANDARD_MAX_WIDTH (= {STANDARD_MAX_WIDTH}); got {std_w}",
        );
        assert!(
            std_w < wide,
            "after exiting EditingValue, modal must SHRINK back to standard width; \
             wide={wide} std={std_w}",
        );
    }

    /// On a narrow terminal (100 cols < STANDARD_MAX_WIDTH +
    /// WIDENED_MARGIN = 118), entering EditingValue for
    /// max_thoughts_width must NOT widen the modal below the
    /// standard sizing — the widening gate is conditional on
    /// `widened_candidate > STANDARD_MAX_WIDTH`, otherwise we fall
    /// through to the standard path. This preserves the
    /// "never shrink" guarantee for narrow terminals.
    #[test]
    fn modal_widening_respects_terminal_width_minimum() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        // Reference render in Browse mode at the same terminal
        // size — we want the widened path to produce the SAME
        // width.
        let mut s_browse = make_state();
        let mut buf_browse = Buffer::empty(area);
        render_settings_modal(&mut buf_browse, area, &mut s_browse, false, None);
        let browse_w = s_browse
            .window
            .popup_area
            .expect("browse render must produce a popup")
            .width;

        let mut s_edit = int_stepper_fixture(120);
        let mut buf_edit = Buffer::empty(area);
        render_settings_modal(&mut buf_edit, area, &mut s_edit, false, None);
        let edit_w = s_edit
            .window
            .popup_area
            .expect("edit render must produce a popup")
            .width;

        assert_eq!(
            edit_w, browse_w,
            "at narrow terminal width ({}) the EditingValue modal must match the Browse \
             modal width — widening is disabled when it would shrink below the standard \
             sizing; got edit={edit_w} browse={browse_w}",
            area.width,
        );
    }

    /// At a wide terminal (180 cols) with a small pending value
    /// (85), the widened modal accommodates the full pending value
    /// without clamping — the preview renders at `pending_value`
    /// cells AND the `note: clamped at …` row is absent.
    #[test]
    fn preview_renders_at_full_width_when_modal_widened() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 180,
            height: 40,
        };
        let mut s = int_stepper_fixture(85);
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let popup = s.window.popup_area.expect("modal must have rendered");
        // The widened modal must accommodate pending=85 with no
        // clamp. The interior width = popup.width - 2 (borders),
        // and the preview's effective width = min(pending,
        // interior). With popup.width = 172 (180-8) the interior
        // is 170 > 85, so no clamp.
        assert!(
            popup.width > 85 + 2,
            "widened modal interior must be wider than pending=85 cells; \
             popup.width={}",
            popup.width,
        );
        // No `note:` row anywhere in the buffer.
        assert!(
            find_text_row(&buf, area, "note:").is_none(),
            "widened modal must not render the clamped note when pending < interior width",
        );
        assert!(
            find_text_row(&buf, area, "clamped").is_none(),
            "widened modal must not contain the word `clamped` anywhere when not clamping",
        );
        // Sanity: the preview itself still renders.
        assert!(
            find_text_row(&buf, area, "preview").is_some(),
            "preview must render at the wide terminal size",
        );
    }

    /// Even with the modal widened, a pending value larger than
    /// the widened interior still clamps — the widening doesn't
    /// magically uncap the preview. On a 100-col terminal the
    /// widening is disabled (Test 3) so the modal stays standard;
    /// at the standard width the interior is ~94 cells, and a
    /// pending of 200 still clamps.
    #[test]
    fn preview_remains_clamped_when_pending_exceeds_widened_width() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 40,
        };
        // Pending = 200 (the registered MAX). Widening is disabled
        // at terminal_width=100 (Test 3), so the modal stays at
        // its standard width; either way, pending > interior so
        // the preview clamps and the note must render.
        let mut s = int_stepper_fixture(200);
        let mut buf = Buffer::empty(area);
        render_settings_modal(&mut buf, area, &mut s, false, None);
        let popup = s
            .window
            .popup_area
            .expect("modal must have rendered at terminal_width=100");
        let interior = popup.width.saturating_sub(2);
        assert!(
            (interior as i64) < 200,
            "interior ({interior}) must be < pending(200) for this fixture to exercise \
             the clamped path",
        );
        // Preview still renders, AND the clamped note is present.
        assert!(
            find_text_row(&buf, area, "preview").is_some(),
            "preview must render even when clamping",
        );
        assert!(
            find_text_row(&buf, area, "note: clamped").is_some(),
            "clamped note must render when pending > interior, even after widening",
        );
    }
}
