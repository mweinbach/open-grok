//! Settings modal keyboard and mouse input handling.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::layout::Rect;

use super::render::int_step_sizes;
use super::state::{
    RowEntry, SettingsEntryPoint, SettingsKeyOutcome, SettingsModalState, SettingsMode,
    SettingsModeKind, action_for_bool, action_for_enum, action_for_enum_commit, action_for_int,
    action_for_string, dynamic_group_choices, effective_enum_choices, filtered_group_choices,
    group_children, validate_string,
};
use crate::app::actions::Action;
use crate::input::line_editor::LineEditOutcome;
use crate::settings::{
    SecretInput, SettingKey, SettingKind, SettingValue, StringValidator, dynamic_enum_choices,
};

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
    match state.state.mode_kind() {
        SettingsModeKind::Browse => handle_browse(state, key),
        SettingsModeKind::FilterFocused => handle_filter_focused(state, key),
        SettingsModeKind::PickingEnum => handle_picking_enum(state, key),
        SettingsModeKind::PickingGroup => handle_picking_group(state, key),
        SettingsModeKind::EditingString | SettingsModeKind::EditingInt => {
            handle_editing_value(state, key)
        }
        SettingsModeKind::EditingSecret => handle_editing_secret(state, key),
    }
}

pub fn handle_settings_paste(state: &mut SettingsModalState, text: &str) -> SettingsKeyOutcome {
    match state.state.mode_kind() {
        SettingsModeKind::FilterFocused => {
            let outcome = state.state.filter.insert_paste(text);
            apply_filter_edit(state, outcome)
        }
        // Sub-sheet search box (OpenRouter / OpenCode Go / custom model
        // catalogs) accepts pasted queries too.
        SettingsModeKind::PickingGroup if state.group_filter_focused => {
            let group_key = match &state.state.mode {
                SettingsMode::PickingGroup { key, .. } => *key,
                _ => unreachable!("mode kind changed before paste"),
            };
            let outcome = state
                .group_filter
                .insert_paste_with_policy(text, safe_settings_char, usize::MAX);
            apply_group_filter_edit(state, group_key, outcome)
        }
        SettingsModeKind::EditingString => {
            let (validator, outcome) = {
                let SettingsMode::EditingString {
                    editor, validator, ..
                } = &mut state.state.mode
                else {
                    unreachable!("mode kind changed before paste")
                };
                (
                    *validator,
                    editor.insert_paste_with_policy(text, safe_settings_char, usize::MAX),
                )
            };
            apply_string_edit(state, validator, outcome)
        }
        SettingsModeKind::Browse
        | SettingsModeKind::PickingEnum
        | SettingsModeKind::PickingGroup
        | SettingsModeKind::EditingInt
        | SettingsModeKind::EditingSecret => SettingsKeyOutcome::Unchanged,
    }
}

/// Enum chooser key routing. Up/Down dispatches preview actions,
/// Enter commits current choice, Esc reverts to original value.
fn handle_picking_enum(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    let (setting_key, choices_idx, original_value, supports_preview) = match &state.state.mode {
        SettingsMode::PickingEnum {
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
        _ => unreachable!("picker handler requires PickingEnum state"),
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
            let close = std::mem::take(&mut state.close_on_picker_exit);
            if !close {
                state.transition_to_browse();
            }
            let kind_is_dynamic = matches!(
                state.registry.find(setting_key).map(|m| &m.kind),
                Some(SettingKind::DynamicEnum { .. })
            );
            let commit = if kind_is_dynamic {
                picker_choice_at_owned(state, setting_key, choices_idx).and_then(|canonical| {
                    action_for_string(setting_key, canonical, &state.pager_snapshot)
                })
            } else {
                picker_choice_at(state, setting_key, choices_idx).and_then(|c| {
                    custom_model_action_for_enum_commit(setting_key, c)
                        .or_else(|| action_for_enum_commit(setting_key, c))
                })
            };
            let kimi_provider_login = !close
                && state.entry_point == SettingsEntryPoint::ProviderLogin
                && setting_key == "kimi_api_endpoint";
            if kimi_provider_login {
                // Advance to the matching zeroizing secret editor. Deep-link
                // armed pickers never take this route (they close instead).
                let Some(current_canonical) = picker_choice_at(state, setting_key, choices_idx)
                else {
                    return SettingsKeyOutcome::Changed;
                };
                if !state.advance_kimi_provider_login_to_secret(current_canonical) {
                    tracing::error!(
                        target: "settings",
                        endpoint = current_canonical,
                        "Kimi provider login could not open the matching secret editor",
                    );
                    return SettingsKeyOutcome::Changed;
                }
                return commit
                    .map(SettingsKeyOutcome::Action)
                    .unwrap_or(SettingsKeyOutcome::Changed);
            }
            match (close, commit) {
                (true, Some(action)) => SettingsKeyOutcome::ActionThenClose(action),
                (true, None) => SettingsKeyOutcome::Close,
                (false, Some(action)) => SettingsKeyOutcome::Action(action),
                (false, None) => SettingsKeyOutcome::Changed,
            }
        }
        KeyCode::Esc => {
            if state.entry_point == SettingsEntryPoint::ProviderLogin
                && setting_key == "kimi_api_endpoint"
            {
                return SettingsKeyOutcome::Close;
            }
            let close = std::mem::take(&mut state.close_on_picker_exit);
            if !close {
                state.transition_to_browse();
            }
            // Revert preview and return to Browse. Non-preview Enums
            // skip the revert (no live visual was applied).
            if let SettingValue::Enum(orig) = &original_value
                && let Some(action) = action_for_enum(setting_key, orig)
            {
                return if close {
                    SettingsKeyOutcome::ActionThenClose(action)
                } else {
                    SettingsKeyOutcome::Action(action)
                };
            }
            if close {
                SettingsKeyOutcome::Close
            } else {
                SettingsKeyOutcome::Changed
            }
        }
        // `d` reset: close picker, revert preview if applicable,
        // then open the reset-confirm overlay. Consent choosers opt out of
        // this entirely (no footer hint, no hidden shortcut) — reset stays
        // reachable from the browse row.
        KeyCode::Char('d')
            if key.modifiers.is_empty() && !crate::settings::is_consent_chooser(setting_key) =>
        {
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

/// Group sub-sheet key routing. Up/Down moves between the child toggles;
/// Space/Enter toggles the focused child in place (the sheet stays open);
/// Esc returns to Browse.
///
/// Dynamic multi-select sheets (OpenRouter / OpenCode Go / custom model
/// catalogs) additionally support `/` search: typing edits the query,
/// which filters the list case-insensitively; Enter commits (unfocuses
/// the query so toggles work); Esc clears a non-empty query first and
/// exits the sheet on the second press. `child_idx` indexes the
/// *filtered* list while a dynamic sheet is open.
fn handle_picking_group(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    let (group_key, child_idx) = match &state.state.mode {
        SettingsMode::PickingGroup { key, child_idx } => (*key, *child_idx),
        _ => unreachable!("group handler requires PickingGroup state"),
    };
    let children = group_children(state, group_key);
    let dynamic_choices = dynamic_group_choices(state, group_key);
    let has_dynamic = !dynamic_choices.is_empty();
    let filtered_len = if has_dynamic {
        filtered_group_choices(state, group_key).len()
    } else {
        0
    };
    let choice_len = if has_dynamic {
        filtered_len
    } else {
        children.len()
    };
    if choice_len == 0 && !(has_dynamic && state.group_filter_focused) {
        // Defensive: a group with no children can't be navigated — back out.
        // (A focused filter on an empty result set stays open so Esc can
        // clear the query instead of closing the sheet.)
        state.transition_to_browse();
        return SettingsKeyOutcome::Changed;
    }

    match key.code {
        KeyCode::Down | KeyCode::Char('j')
            if !state.group_filter_focused || !has_dynamic =>
        {
            if child_idx + 1 >= choice_len {
                return SettingsKeyOutcome::Unchanged;
            }
            state.transition_to_picking_group(group_key, child_idx + 1);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Up | KeyCode::Char('k') if !state.group_filter_focused || !has_dynamic => {
            if child_idx == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            state.transition_to_picking_group(group_key, child_idx - 1);
            SettingsKeyOutcome::Changed
        }
        KeyCode::PageDown if has_dynamic && !state.group_filter_focused => {
            let next = (child_idx + 10).min(choice_len.saturating_sub(1));
            if next == child_idx {
                return SettingsKeyOutcome::Unchanged;
            }
            state.transition_to_picking_group(group_key, next);
            SettingsKeyOutcome::Changed
        }
        KeyCode::PageUp if has_dynamic && !state.group_filter_focused => {
            let next = child_idx.saturating_sub(10);
            if next == child_idx {
                return SettingsKeyOutcome::Unchanged;
            }
            state.transition_to_picking_group(group_key, next);
            SettingsKeyOutcome::Changed
        }
        // `/` focuses the sub-sheet search box (dynamic catalogs only).
        KeyCode::Char('/') if has_dynamic => {
            state.group_filter_focused = true;
            SettingsKeyOutcome::Changed
        }
        // Space/Enter toggle the focused child Bool and stay in the sheet so the
        // user can flip several tips in a row. The dispatcher refreshes the
        // modal snapshot, so the new value paints on the next frame.
        KeyCode::Char(' ') | KeyCode::Enter => {
            if has_dynamic {
                // Enter while the search box is focused commits the query
                // (unfocuses it) without toggling — matching Browse's
                // FilterFocused Enter semantics.
                if state.group_filter_focused && key.code == KeyCode::Enter {
                    state.group_filter_focused = false;
                    return SettingsKeyOutcome::Changed;
                }
                let choices = filtered_group_choices(state, group_key);
                let Some((orig_idx, _)) = choices.get(child_idx) else {
                    return SettingsKeyOutcome::Unchanged;
                };
                let orig_idx = *orig_idx;
                return toggle_dynamic_multi_select(state, group_key, orig_idx, &dynamic_choices);
            }
            let Some(child_key) = children.get(child_idx).copied() else {
                return SettingsKeyOutcome::Unchanged;
            };
            activate_group_child(state, child_key)
        }
        KeyCode::Esc => {
            // Forgiving exit: a non-empty query is cleared first (and focus
            // clamps to the top of the now-unfiltered list); the second Esc
            // leaves the sheet.
            if has_dynamic && !state.group_filter.text().trim().is_empty() {
                state.group_filter.reset();
                state.group_filter_focused = false;
                state.transition_to_picking_group(group_key, 0);
                return SettingsKeyOutcome::Changed;
            }
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        _ => {
            if has_dynamic && state.group_filter_focused {
                let outcome = state
                    .group_filter
                    .handle_key_with_insert_policy(key, safe_settings_char);
                return apply_group_filter_edit(state, group_key, outcome);
            }
            SettingsKeyOutcome::Unchanged
        }
    }
}

/// Apply a `LineEditor` outcome for the group sub-sheet search box:
/// text changes re-filter the list and clamp focus into the filtered set.
fn apply_group_filter_edit(
    state: &mut SettingsModalState,
    group_key: SettingKey,
    outcome: LineEditOutcome,
) -> SettingsKeyOutcome {
    match outcome {
        LineEditOutcome::TextChanged => {
            let len = filtered_group_choices(state, group_key).len();
            let cur = match &state.state.mode {
                SettingsMode::PickingGroup { child_idx, .. } => *child_idx,
                _ => 0,
            };
            let new_idx = cur.min(len.saturating_sub(1));
            state.transition_to_picking_group(group_key, new_idx);
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::HandledNoChange | LineEditOutcome::CursorChanged => {
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::Unhandled => SettingsKeyOutcome::Unchanged,
    }
}

fn toggle_dynamic_multi_select(
    state: &SettingsModalState,
    group_key: SettingKey,
    choice_idx: usize,
    choices: &[crate::settings::OwnedMultiSelectChoice],
) -> SettingsKeyOutcome {
    let Some(choice) = choices.get(choice_idx) else {
        return SettingsKeyOutcome::Unchanged;
    };
    if group_key == "custom_models.list" || group_key == "custom_models" {
        if !choice.selected {
            return SettingsKeyOutcome::Unchanged;
        }
        return SettingsKeyOutcome::Action(Action::DeleteCustomModel {
            key: choice.canonical.clone(),
        });
    }
    if group_key == "openrouter_models" {
        let enabled = toggle_openrouter_enabled_models(
            &state.pager_snapshot.openrouter_models,
            &state.pager_snapshot.openrouter_enabled_models,
            &choice.canonical,
            choice.selected,
        );
        return SettingsKeyOutcome::Action(Action::SetOpenRouterEnabledModels { models: enabled });
    }
    if group_key != "opencode_go_models" {
        return SettingsKeyOutcome::Unchanged;
    }
    let mut enabled = state.pager_snapshot.opencode_go_enabled_models.clone();
    enabled.retain(|value| {
        value != &choice.canonical
            && !state
                .pager_snapshot
                .opencode_go_models
                .iter()
                .any(|model| model.id == choice.canonical && &model.key == value)
    });
    if !choice.selected {
        enabled.push(choice.canonical.clone());
    }
    enabled.sort();
    enabled.dedup();
    SettingsKeyOutcome::Action(Action::SetOpenCodeGoEnabledModels { models: enabled })
}

fn toggle_openrouter_enabled_models(
    catalog: &[xai_grok_shell::openrouter_models::OpenRouterModelDescriptor],
    current: &[String],
    canonical: &str,
    currently_selected: bool,
) -> Vec<String> {
    let listed =
        |value: &str, model: &xai_grok_shell::openrouter_models::OpenRouterModelDescriptor| {
            value == model.id || value == model.key
        };
    let mut enabled = if current.is_empty() {
        catalog.iter().map(|model| model.id.clone()).collect()
    } else {
        current.to_vec()
    };
    enabled.retain(|value| {
        value != canonical
            && !catalog
                .iter()
                .any(|model| model.id == canonical && value == &model.key)
    });
    if !currently_selected {
        enabled.push(canonical.to_owned());
    }
    enabled.sort();
    enabled.dedup();
    if !catalog.is_empty()
        && catalog
            .iter()
            .all(|model| enabled.iter().any(|value| listed(value, model)))
    {
        enabled.clear();
    }
    enabled
}

pub(crate) fn custom_model_action_for_bool(key: SettingKey, new: bool) -> Option<Action> {
    match key {
        "custom_model_save" => Some(Action::SetCustomModelSave(new)),
        _ => None,
    }
}

pub(crate) fn custom_model_action_for_string(key: SettingKey, value: String) -> Option<Action> {
    match key {
        "custom_model_id" => Some(Action::SetCustomModelId(value)),
        "custom_model_slug" => Some(Action::SetCustomModelSlug(value)),
        "custom_model_name" => Some(Action::SetCustomModelName(value)),
        "custom_model_base_url" => Some(Action::SetCustomModelBaseUrl(value)),
        "custom_model_env_key" => Some(Action::SetCustomModelEnvKey(value)),
        _ => None,
    }
}

pub(crate) fn custom_model_action_for_int(key: SettingKey, value: i64) -> Option<Action> {
    match key {
        "custom_model_context_window" => Some(Action::SetCustomModelContextWindow(value)),
        _ => None,
    }
}

pub(crate) fn custom_model_action_for_enum_commit(
    key: SettingKey,
    choice: &'static str,
) -> Option<Action> {
    match key {
        "custom_model_provider" => Some(Action::SetCustomModelProvider(choice.to_owned())),
        "custom_model_backend" => Some(Action::SetCustomModelBackend(choice.to_owned())),
        _ => None,
    }
}

/// Common nav body for Up/Down (and j/k aliases) in the picker:
/// update `choices_idx` in-place, look up the new canonical, fire
/// the preview dispatch via `action_for_enum`. Extracted from the
/// Update picker index and optionally dispatch a preview action.
/// `new_idx` must be in-bounds. Preview only fires for Enums with
/// `supports_preview: true`; side-effecting Enums skip preview.
pub(super) fn set_picker_idx(
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
    state.transition_to_picking_enum(setting_key, new_idx, original_value, supports_preview);
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
    // Int settings dispatch through a stepper-only
    // handler. All char-input / cursor-pan / Backspace / Delete /
    // Home / End keys are rejected; only Up/Down/Left/Right (and
    // j/k/h/l aliases), Enter, and Esc do anything.
    if let SettingsMode::EditingInt {
        key: setting_key,
        buffer,
        min,
        max,
    } = &state.state.mode
    {
        let setting_key = *setting_key;
        let buffer = buffer.clone();
        return handle_int_stepper(state, key, setting_key, &buffer, *min, *max);
    }

    let (setting_key, validator) = match &state.state.mode {
        SettingsMode::EditingString { key, validator, .. } => (*key, *validator),
        _ => unreachable!("editing handler requires String or Int state"),
    };

    if key.code == KeyCode::Enter {
        let SettingsMode::EditingString { editor, .. } = &state.state.mode else {
            unreachable!("String editor state changed during commit");
        };
        let text = editor.text().to_owned();
        let error = validate_string(validator, &text, &state.pager_snapshot.available_models);
        if error.is_some() {
            let SettingsMode::EditingString {
                validation_error, ..
            } = &mut state.state.mode
            else {
                unreachable!("String editor state changed during validation");
            };
            *validation_error = error;
            return SettingsKeyOutcome::Unchanged;
        }
        let action = custom_model_action_for_string(setting_key, text.clone())
            .or_else(|| action_for_string(setting_key, text, &state.pager_snapshot));
        state.transition_to_browse();
        return match action {
            Some(action) => SettingsKeyOutcome::Action(action),
            None => {
                tracing::error!(
                    target: "settings",
                    key = setting_key,
                    "EditingValue commit has no action_for_string arm — registry skew",
                );
                SettingsKeyOutcome::Changed
            }
        };
    }

    if key.code == KeyCode::Esc {
        state.transition_to_browse();
        return SettingsKeyOutcome::Changed;
    }

    if matches!(
        key.code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Tab
            | KeyCode::BackTab
    ) {
        return SettingsKeyOutcome::Unchanged;
    }

    let outcome = {
        let SettingsMode::EditingString { editor, .. } = &mut state.state.mode else {
            unreachable!("String editor state changed before key handling");
        };
        editor.handle_key_with_insert_policy(key, safe_settings_char)
    };
    apply_string_edit(state, validator, outcome)
}

fn apply_string_edit(
    state: &mut SettingsModalState,
    validator: StringValidator,
    outcome: LineEditOutcome,
) -> SettingsKeyOutcome {
    match outcome {
        LineEditOutcome::TextChanged => {
            let SettingsMode::EditingString { editor, .. } = &state.state.mode else {
                unreachable!("String editor state changed after text mutation");
            };
            let error = validate_string(
                validator,
                editor.text(),
                &state.pager_snapshot.available_models,
            );
            let SettingsMode::EditingString {
                validation_error, ..
            } = &mut state.state.mode
            else {
                unreachable!("String editor state changed during validation");
            };
            *validation_error = error;
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::HandledNoChange | LineEditOutcome::CursorChanged => {
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::Unhandled => SettingsKeyOutcome::Unchanged,
    }
}

const MAX_SECRET_INPUT_BYTES: usize = 4096;

fn validate_secret(buffer: &str) -> Option<String> {
    if buffer.len() > MAX_SECRET_INPUT_BYTES {
        Some(format!(
            "Key is too long (maximum {MAX_SECRET_INPUT_BYTES} bytes)"
        ))
    } else if buffer.chars().any(char::is_whitespace) {
        Some("Key cannot contain whitespace".to_owned())
    } else if buffer
        .chars()
        .any(crate::render::line_utils::is_unsafe_display_char)
    {
        Some("Key contains unsupported control characters".to_owned())
    } else {
        None
    }
}

fn update_secret_validation(state: &mut SettingsModalState, error: Option<String>) {
    if let SettingsMode::EditingSecret {
        validation_error, ..
    } = &mut state.state.mode
    {
        *validation_error = error;
    }
}

/// Dedicated secret editor routing. Unlike the generic String path, an empty
/// Enter is a preserve/no-op and the committed value remains wrapped.
fn handle_editing_secret(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    let (setting_key, cursor_byte) = match &state.state.mode {
        SettingsMode::EditingSecret {
            key, cursor_byte, ..
        } => (*key, *cursor_byte),
        _ => return SettingsKeyOutcome::Unchanged,
    };

    match key.code {
        KeyCode::Esc => {
            if state.entry_point == SettingsEntryPoint::ProviderLogin {
                return SettingsKeyOutcome::Close;
            }
            state.transition_to_browse();
            SettingsKeyOutcome::Changed
        }
        KeyCode::Enter => {
            let error = match &state.state.mode {
                SettingsMode::EditingSecret { buffer, .. } if buffer.is_empty() => {
                    if state.entry_point == SettingsEntryPoint::ProviderLogin {
                        return SettingsKeyOutcome::Close;
                    }
                    state.transition_to_browse();
                    return SettingsKeyOutcome::Changed;
                }
                SettingsMode::EditingSecret { buffer, .. } => validate_secret(buffer.expose()),
                _ => return SettingsKeyOutcome::Unchanged,
            };
            if error.is_some() {
                update_secret_validation(state, error);
                return SettingsKeyOutcome::Unchanged;
            }
            let secret = match &mut state.state.mode {
                SettingsMode::EditingSecret { buffer, .. } => std::mem::take(buffer),
                _ => return SettingsKeyOutcome::Unchanged,
            };
            let endpoint = match setting_key {
                "kimi_api_key" => Some(xai_grok_shell::kimi_models::KimiApiEndpoint::Platform),
                "kimi_code_api_key" => Some(xai_grok_shell::kimi_models::KimiApiEndpoint::Code),
                _ => None,
            };
            if setting_key == "perplexity_api_key" {
                state.transition_to_browse();
                return SettingsKeyOutcome::Action(Action::SetPerplexityApiKey { key: secret });
            }
            if setting_key == "fireworks_api_key" {
                let action = Action::SetFireworksApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if setting_key == "deepseek_api_key" {
                let action = Action::SetDeepSeekApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if setting_key == "meta_api_key" {
                let action = Action::SetMetaApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if setting_key == "opencode_go_api_key" {
                let action = Action::SetOpenCodeGoApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if setting_key == "openrouter_api_key" {
                let action = Action::SetOpenRouterApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if setting_key == "wafer_api_key" {
                let action = Action::SetWaferApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if setting_key == "zai_api_key" {
                let action = Action::SetZaiApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if setting_key == "runinfra_api_key" {
                let action = Action::SetRuninfraApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if setting_key == "gemini_api_key" {
                let action = Action::SetGeminiApiKey { key: secret };
                return if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                };
            }
            if let Some(endpoint) = endpoint {
                let action = Action::SetKimiApiKey {
                    endpoint,
                    key: secret,
                };
                if state.entry_point == SettingsEntryPoint::ProviderLogin {
                    SettingsKeyOutcome::ActionAndClose(action)
                } else {
                    state.transition_to_browse();
                    SettingsKeyOutcome::Action(action)
                }
            } else {
                state.transition_to_browse();
                tracing::error!(
                    target: "settings",
                    key = setting_key,
                    "Secret editor commit has no typed action — registry skew",
                );
                SettingsKeyOutcome::Changed
            }
        }
        KeyCode::Backspace => {
            if cursor_byte == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            let SettingsMode::EditingSecret {
                buffer,
                cursor_byte: cursor,
                validation_error,
                ..
            } = &mut state.state.mode
            else {
                return SettingsKeyOutcome::Unchanged;
            };
            let previous = (0..*cursor)
                .rev()
                .find(|&index| buffer.expose().is_char_boundary(index))
                .unwrap_or(0);
            buffer.expose_mut().replace_range(previous..*cursor, "");
            *cursor = previous;
            *validation_error = validate_secret(buffer.expose());
            SettingsKeyOutcome::Changed
        }
        KeyCode::Delete => {
            let SettingsMode::EditingSecret {
                buffer,
                cursor_byte: cursor,
                validation_error,
                ..
            } = &mut state.state.mode
            else {
                return SettingsKeyOutcome::Unchanged;
            };
            if *cursor >= buffer.len() {
                return SettingsKeyOutcome::Unchanged;
            }
            let next = (*cursor + 1..=buffer.len())
                .find(|&index| buffer.expose().is_char_boundary(index))
                .unwrap_or(buffer.len());
            buffer.expose_mut().replace_range(*cursor..next, "");
            *validation_error = validate_secret(buffer.expose());
            SettingsKeyOutcome::Changed
        }
        KeyCode::Left => {
            if cursor_byte == 0 {
                return SettingsKeyOutcome::Unchanged;
            }
            let SettingsMode::EditingSecret {
                buffer,
                cursor_byte: cursor,
                ..
            } = &mut state.state.mode
            else {
                return SettingsKeyOutcome::Unchanged;
            };
            *cursor = (0..*cursor)
                .rev()
                .find(|&index| buffer.expose().is_char_boundary(index))
                .unwrap_or(0);
            SettingsKeyOutcome::Changed
        }
        KeyCode::Right => {
            let SettingsMode::EditingSecret {
                buffer,
                cursor_byte: cursor,
                ..
            } = &mut state.state.mode
            else {
                return SettingsKeyOutcome::Unchanged;
            };
            if *cursor >= buffer.len() {
                return SettingsKeyOutcome::Unchanged;
            }
            *cursor = (*cursor + 1..=buffer.len())
                .find(|&index| buffer.expose().is_char_boundary(index))
                .unwrap_or(buffer.len());
            SettingsKeyOutcome::Changed
        }
        KeyCode::Home => {
            if let SettingsMode::EditingSecret { cursor_byte, .. } = &mut state.state.mode {
                *cursor_byte = 0;
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        KeyCode::End => {
            if let SettingsMode::EditingSecret {
                buffer,
                cursor_byte,
                ..
            } = &mut state.state.mode
            {
                *cursor_byte = buffer.len();
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            if c.is_whitespace() || crate::render::line_utils::is_unsafe_display_char(c) {
                update_secret_validation(state, validate_secret(&c.to_string()));
                return SettingsKeyOutcome::Unchanged;
            }
            let SettingsMode::EditingSecret {
                buffer,
                cursor_byte,
                validation_error,
                ..
            } = &mut state.state.mode
            else {
                return SettingsKeyOutcome::Unchanged;
            };
            if buffer.len().saturating_add(c.len_utf8()) > MAX_SECRET_INPUT_BYTES {
                *validation_error = Some(format!(
                    "Key is too long (maximum {MAX_SECRET_INPUT_BYTES} bytes)"
                ));
                return SettingsKeyOutcome::Unchanged;
            }
            buffer.expose_mut().insert(*cursor_byte, c);
            *cursor_byte += c.len_utf8();
            *validation_error = None;
            SettingsKeyOutcome::Changed
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

/// Insert bracketed-paste content into the dedicated secret editor. Newlines
/// copied by password managers are trimmed at the boundary; internal
/// whitespace/control characters are rejected without echoing the payload.
pub fn handle_settings_secret_paste(
    state: &mut SettingsModalState,
    pasted: SecretInput,
) -> SettingsKeyOutcome {
    if !matches!(state.state.mode, SettingsMode::EditingSecret { .. }) {
        return SettingsKeyOutcome::Unchanged;
    }
    let pasted = pasted.expose().trim();
    if pasted.is_empty() {
        return SettingsKeyOutcome::Unchanged;
    }
    if let Some(error) = validate_secret(pasted) {
        update_secret_validation(state, Some(error));
        return SettingsKeyOutcome::Changed;
    }
    let SettingsMode::EditingSecret {
        buffer,
        cursor_byte,
        validation_error,
        ..
    } = &mut state.state.mode
    else {
        return SettingsKeyOutcome::Unchanged;
    };
    if buffer.len().saturating_add(pasted.len()) > MAX_SECRET_INPUT_BYTES {
        *validation_error = Some(format!(
            "Key is too long (maximum {MAX_SECRET_INPUT_BYTES} bytes)"
        ));
        return SettingsKeyOutcome::Changed;
    }
    buffer.expose_mut().insert_str(*cursor_byte, pasted);
    *cursor_byte += pasted.len();
    *validation_error = None;
    SettingsKeyOutcome::Changed
}

/// Int stepper handler. Steps by range-aware small (Up/Down) or large
/// (Left/Right) deltas from [`int_step_sizes`], clamped to [min,max].
/// Non-stepper keys are rejected.
fn handle_int_stepper(
    state: &mut SettingsModalState,
    key: &KeyEvent,
    setting_key: SettingKey,
    buffer: &str,
    min: i64,
    max: i64,
) -> SettingsKeyOutcome {
    let (small_step, large_step) = int_step_sizes(min, max);
    let step_delta = |dir: i64, large: bool| -> i64 {
        let magnitude = if large { large_step } else { small_step };
        dir * magnitude
    };

    let apply_step = |state: &mut SettingsModalState, delta: i64| -> SettingsKeyOutcome {
        let cur = buffer.parse::<i64>().unwrap_or(min);
        let new = cur.saturating_add(delta).clamp(min, max);
        if new == cur {
            // Already clamped — no visible change. Report
            // Unchanged so the test for `clamps_to_min/max` can
            // distinguish a no-op from a step.
            return SettingsKeyOutcome::Unchanged;
        }
        let new_buf = new.to_string();
        update_int_buffer(state, new_buf);
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
            let action_opt = buffer.parse::<i64>().ok().and_then(|i| {
                custom_model_action_for_int(setting_key, i)
                    .or_else(|| action_for_int(setting_key, i))
            });
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

fn update_int_buffer(state: &mut SettingsModalState, new_buffer: String) {
    let SettingsMode::EditingInt { buffer, .. } = &mut state.state.mode else {
        unreachable!("Int update requires EditingInt state");
    };
    *buffer = new_buffer;
}

/// Number of choices for the picker. Handles both
/// `SettingKind::Enum` (static catalog) and `SettingKind::DynamicEnum`
/// (catalog built from the snapshot at picker-open time).
pub(super) fn picker_choices_len(state: &SettingsModalState, key: SettingKey) -> usize {
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
pub(super) fn picker_choice_at(
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
            state.focus_filter();
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
                // A locked row isn't the user's to change, by `d` any more
                // than by Enter (which `try_enter_picking_enum` refuses).
                // The dispatch-time guard would catch it either way, but
                // only after walking the user through a confirm dialog for
                // a change that cannot happen.
                Some((key, _meta)) if state.row_lock(key).is_some() => {
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
            // Continue editing a committed query without refocusing the filter.
            if state.query().is_empty() {
                return SettingsKeyOutcome::Unchanged;
            }
            let outcome = state.state.filter.delete_last_grapheme();
            apply_filter_edit(state, outcome)
        }
        _ => SettingsKeyOutcome::Unchanged,
    }
}

fn handle_filter_focused(state: &mut SettingsModalState, key: &KeyEvent) -> SettingsKeyOutcome {
    match key.code {
        KeyCode::Esc => {
            if !state.query().is_empty() {
                state.state.filter.reset();
                state.invalidate_filter();
                state.clamp_selected_to_visible();
            }
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
        KeyCode::Tab => SettingsKeyOutcome::Unchanged,
        KeyCode::Char('u') if key.modifiers == KeyModifiers::CONTROL => {
            if !state.query().is_empty() {
                state.state.filter.reset();
                state.invalidate_filter();
                state.clamp_selected_to_visible();
            }
            SettingsKeyOutcome::Changed
        }
        _ => {
            let outcome = state
                .state
                .filter
                .handle_key_with_insert_policy(key, safe_settings_char);
            apply_filter_edit(state, outcome)
        }
    }
}

fn safe_settings_char(character: char) -> bool {
    !crate::render::line_utils::is_unsafe_display_char(character)
}

#[cfg(test)]
pub(super) fn set_filter_cursor(state: &mut SettingsModalState, cursor_byte: usize) {
    let _ = state.state.filter.set_cursor_byte(cursor_byte);
}

fn apply_filter_edit(
    state: &mut SettingsModalState,
    outcome: LineEditOutcome,
) -> SettingsKeyOutcome {
    match outcome {
        LineEditOutcome::TextChanged => {
            state.invalidate_filter();
            state.clamp_selected_to_visible();
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::HandledNoChange | LineEditOutcome::CursorChanged => {
            SettingsKeyOutcome::Changed
        }
        LineEditOutcome::Unhandled => SettingsKeyOutcome::Unchanged,
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
        state.close_on_picker_exit = false;
        let synthetic = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        match state.state.mode_kind() {
            SettingsModeKind::PickingEnum => {
                return handle_picking_enum(state, &synthetic);
            }
            SettingsModeKind::PickingGroup => {
                return handle_picking_group(state, &synthetic);
            }
            SettingsModeKind::EditingString | SettingsModeKind::EditingInt => {
                return handle_editing_value(state, &synthetic);
            }
            SettingsModeKind::EditingSecret => {
                return handle_editing_secret(state, &synthetic);
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
    if matches!(
        state.state.mode_kind(),
        SettingsModeKind::EditingString
            | SettingsModeKind::EditingInt
            | SettingsModeKind::EditingSecret
    ) {
        let outcome = handle_editor_mouse(state, kind, column, row);
        return upgrade_if_breadcrumb_flipped(outcome, breadcrumb_hover_flipped);
    }

    // PickingEnum: click-to-pick on choice rects, scroll wheel is a
    // no-op (the picker is bounded; scroll there could surprise).
    if state.state.mode_kind() == SettingsModeKind::PickingEnum {
        let outcome = handle_picker_mouse(state, kind, column, row);
        return upgrade_if_breadcrumb_flipped(outcome, breadcrumb_hover_flipped);
    }

    // PickingGroup: hover tracks the child rects; a click toggles the clicked
    // child in place (same bounded-viewport, scroll-is-a-no-op contract).
    if state.state.mode_kind() == SettingsModeKind::PickingGroup {
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
    // field as the row-list path; the field is mode-aware).
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
    // Snapshot the picker payload before mutating the state.
    let (setting_key, current_idx, original_value, supports_preview) = match &state.state.mode {
        SettingsMode::PickingEnum {
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
        _ => unreachable!("picker mouse handler requires PickingEnum state"),
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
/// commit-on-Enter). The scroll wheel scrolls the sub-sheet viewport (dynamic
/// catalogs like OpenRouter can be hundreds of rows long); clicks on the
/// search bar focus the query editor; clicks on list rows map through the
/// active filter to the underlying choice.
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
    // Scroll wheel pans the sub-sheet viewport by ~3 rows per tick. The
    // offset is clamped against the real item count at render time.
    match kind {
        MouseEventKind::ScrollDown => {
            let next = state.group_scroll.saturating_add(3);
            if next == state.group_scroll {
                return SettingsKeyOutcome::Unchanged;
            }
            state.group_scroll = next;
            return SettingsKeyOutcome::Changed;
        }
        MouseEventKind::ScrollUp => {
            let next = state.group_scroll.saturating_sub(3);
            if next == state.group_scroll {
                return SettingsKeyOutcome::Unchanged;
            }
            state.group_scroll = next;
            return SettingsKeyOutcome::Changed;
        }
        _ => {}
    }
    let MouseEventKind::Down(crossterm::event::MouseButton::Left) = kind else {
        return SettingsKeyOutcome::Unchanged;
    };
    // Click on the search bar focuses the query editor.
    if rect_contains(state.group_search_rect, column, row) {
        state.group_filter_focused = true;
        return SettingsKeyOutcome::Changed;
    }
    let group_key = match &state.state.mode {
        SettingsMode::PickingGroup { key, .. } => *key,
        _ => unreachable!("group mouse handler requires PickingGroup state"),
    };
    let children = group_children(state, group_key);
    let dynamic_choices = dynamic_group_choices(state, group_key);
    let clicked_idx = state
        .picker_choice_rects
        .iter()
        .position(|r| r.height > 0 && rect_contains(*r, column, row));
    let Some(idx) = clicked_idx else {
        return SettingsKeyOutcome::Unchanged;
    };
    state.transition_to_picking_group(group_key, idx);
    if !dynamic_choices.is_empty() {
        // Map the clicked *display* row through the active filter back to
        // its catalog entry before toggling.
        let choices = filtered_group_choices(state, group_key);
        let Some((orig_idx, _)) = choices.get(idx) else {
            return SettingsKeyOutcome::Unchanged;
        };
        let orig_idx = *orig_idx;
        return toggle_dynamic_multi_select(state, group_key, orig_idx, &dynamic_choices);
    }
    let Some(child_key) = children.get(idx).copied() else {
        return SettingsKeyOutcome::Changed;
    };
    activate_group_child(state, child_key)
}

fn activate_group_child(
    state: &mut SettingsModalState,
    child_key: SettingKey,
) -> SettingsKeyOutcome {
    let Some(meta) = state.registry.find(child_key) else {
        return SettingsKeyOutcome::Unchanged;
    };
    match &meta.kind {
        SettingKind::DynamicMultiSelect { .. } | SettingKind::Group { .. } => {
            state.transition_to_picking_group(child_key, 0);
            SettingsKeyOutcome::Changed
        }
        SettingKind::Bool { .. } => {
            let cur = matches!(state.value_for(child_key), Some(SettingValue::Bool(true)));
            custom_model_action_for_bool(child_key, !cur)
                .or_else(|| action_for_bool(child_key, !cur))
                .map(SettingsKeyOutcome::Action)
                .unwrap_or(SettingsKeyOutcome::Unchanged)
        }
        SettingKind::Enum { .. } | SettingKind::DynamicEnum { .. } => {
            if enter_child_enum_picker(state, child_key) {
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        SettingKind::String { .. } | SettingKind::Int { .. } => {
            if enter_child_editor(state, child_key) {
                SettingsKeyOutcome::Changed
            } else {
                SettingsKeyOutcome::Unchanged
            }
        }
        SettingKind::Secret => SettingsKeyOutcome::Unchanged,
    }
}

fn enter_child_enum_picker(state: &mut SettingsModalState, key: SettingKey) -> bool {
    let Some(meta) = state.registry.find(key) else {
        return false;
    };
    let SettingKind::Enum {
        choices,
        supports_preview,
        ..
    } = &meta.kind
    else {
        return false;
    };
    let current = state.value_for(key);
    let choices_idx = match &current {
        Some(SettingValue::Enum(cur)) => choices
            .iter()
            .position(|choice| choice.canonical == *cur)
            .unwrap_or(0),
        _ => 0,
    };
    let original = current.unwrap_or(SettingValue::Enum(
        choices.first().map_or("", |c| c.canonical),
    ));
    state.transition_to_picking_enum(key, choices_idx, original, *supports_preview);
    true
}

fn enter_child_editor(state: &mut SettingsModalState, key: SettingKey) -> bool {
    let Some(meta) = state.registry.find(key) else {
        return false;
    };
    let value = state.value_for(key);
    match &meta.kind {
        SettingKind::String {
            default, validator, ..
        } => {
            let text = match value {
                Some(SettingValue::String(text)) => text,
                _ => (*default).to_string(),
            };
            let mut editor = crate::input::line_editor::LineEditor::default();
            editor.set_text(text);
            let validation_error = validate_string(
                *validator,
                editor.text(),
                &state.pager_snapshot.available_models,
            );
            state.transition_to_editing_string(key, editor, *validator, validation_error);
            true
        }
        SettingKind::Int {
            default, min, max, ..
        } => {
            let buffer = match value {
                Some(SettingValue::Int(value)) => value.to_string(),
                _ => default.to_string(),
            };
            state.transition_to_editing_int(key, buffer, *min, *max);
            true
        }
        _ => false,
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
