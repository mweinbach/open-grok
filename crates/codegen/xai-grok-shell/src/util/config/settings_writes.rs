use super::persist::update_config;
use anyhow::Result;

// ---------------------------------------------------------------------------

pub async fn set_perplexity_web_search_enabled(value: bool) -> Result<()> {
    update_config(|cfg| cfg.perplexity_web_search.enabled = value).await
}

/// Persist `[toolset.web_search_source].<target>` via `update_config`.
pub async fn set_web_search_source(
    target: crate::tools::config::WebSearchSourceTarget,
    source: crate::tools::config::WebSearchSource,
) -> Result<()> {
    update_config(move |cfg| cfg.web_search_source.set_for(target, Some(source))).await
}

/// Persist `[toolset.x_search].enabled` via `update_config`.
pub async fn set_x_search_enabled(value: bool) -> Result<()> {
    update_config(|cfg| cfg.x_search.enabled = value).await
}

/// Persist `[antigravity].skip_permissions` via `update_config`. The runner
/// resolves `unwrap_or(true)`, so `Some(false)` is the explicit opt-out that
/// forces read-only agy subagents; `Some(true)` restores the full-access
/// default. Read back by `load_antigravity_skip_permissions_sync`.
pub async fn set_antigravity_skip_permissions(value: bool) -> Result<()> {
    update_config(|cfg| cfg.antigravity.skip_permissions = Some(value)).await
}

// Settings helpers — typed disk-write wrappers for each setting.
// All route through `update_config` → `merge_section` → `save_config`.
// ---------------------------------------------------------------------------

/// Persist `[ui].compact_mode` via `update_config`.
pub async fn set_compact_mode(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.compact_mode = value).await
}

/// Persist `[ui].show_timestamps` via `update_config`. `UiConfig::show_timestamps`
/// is `Option<bool>` — pager-side `None` means "use default" — so we wrap.
pub async fn set_show_timestamps(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.show_timestamps = Some(value)).await
}

/// Persist `[ui].show_timeline` via `update_config`. Same `Option<bool>`
/// shape as `show_timestamps`.
pub async fn set_show_timeline(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.show_timeline = Some(value)).await
}

pub async fn set_page_flip_on_send(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.page_flip_on_send = Some(value)).await
}

/// Persist `[ui].combine_queued_prompts` via `update_config`.
pub async fn set_combine_queued_prompts(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.combine_queued_prompts = Some(value)).await
}

/// Persist `[ui].simple_mode` via `update_config`. Same `Option<bool>`
/// shape as `show_timestamps`.
pub async fn set_simple_mode(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.simple_mode = Some(value)).await
}

/// Persist `[ui].swarm_mode` via `update_config`. `None` means the default is off,
/// so explicit settings changes are stored as `Some(value)`.
pub async fn set_swarm_mode(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.swarm_mode = Some(value)).await
}

/// Persist `[ui].antigravity_subagents` via `update_config`. Same `Option<bool>`
/// shape as `swarm_mode` (`None` = default off).
pub async fn set_antigravity_subagents(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.antigravity_subagents = Some(value)).await
}

/// Persist `[ui.contextual_hints].undo` via `update_config`. The nested struct
/// stays out of `config.toml` until a tip is toggled (`skip_serializing_if`).
pub async fn set_contextual_hint_undo(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.contextual_hints.undo = Some(value)).await
}

/// Persist `[ui.contextual_hints].plan_mode` via `update_config`.
pub async fn set_contextual_hint_plan_mode(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.contextual_hints.plan_mode = Some(value)).await
}

/// Persist `[ui.contextual_hints].image_input` via `update_config`.
pub async fn set_contextual_hint_image_input(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.contextual_hints.image_input = Some(value)).await
}

/// Persist `[ui.contextual_hints].send_now` via `update_config`.
pub async fn set_contextual_hint_send_now(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.contextual_hints.send_now = Some(value)).await
}

/// Persist `[ui.contextual_hints].small_screen` via `update_config`.
pub async fn set_contextual_hint_small_screen(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.contextual_hints.small_screen = Some(value)).await
}

/// Persist `[ui.contextual_hints].word_select` via `update_config`.
pub async fn set_contextual_hint_word_select(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.contextual_hints.word_select = Some(value)).await
}

/// Persist `[ui.contextual_hints].ssh_wrap` via `update_config`.
pub async fn set_contextual_hint_ssh_wrap(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.contextual_hints.ssh_wrap = Some(value)).await
}

/// Persist `[ui].theme` via `update_config`. Caller must pass the
/// canonical theme name (`groknight`, `tokyonight`, `auto`, etc.).
pub async fn set_theme(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.theme = Some(value)).await
}

/// Persist `[ui].auto_dark_theme` via `update_config`. `UiConfig::auto_dark_theme`
/// is `Option<String>` (canonical theme name; `auto` is rejected by the
/// pager's `load_auto_theme_config` filter at read time to prevent
/// circular reference).
pub async fn set_auto_dark_theme(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.auto_dark_theme = Some(value)).await
}

/// Persist `[ui].auto_light_theme` via `update_config`. Same shape as
/// [`set_auto_dark_theme`].
pub async fn set_auto_light_theme(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.auto_light_theme = Some(value)).await
}

/// Maximum length (in bytes) accepted by [`set_default_model`].
/// Defense against callers bypassing catalog validation.
pub const MAX_DEFAULT_MODEL_LEN: usize = 256;

fn optional_model_pin(label: &str, value: String) -> Result<Option<String>> {
    if value.len() > MAX_DEFAULT_MODEL_LEN {
        anyhow::bail!(
            "{label} model id too long ({} > {} bytes)",
            value.len(),
            MAX_DEFAULT_MODEL_LEN
        );
    }
    Ok((!value.is_empty()).then_some(value))
}

/// Persist `[models].default` and dismiss any active campaign nudging it (an
/// explicit user pick wins over the soft campaign default).
///
/// This is the only sanctioned writer of `models.default`; it routes through
/// [`super::campaigns::persist_models_default`] so a user pick always dismisses
/// an active campaign. Do not persist `models.default` via raw `update_config`,
/// or a campaign would keep overriding the user's choice.
///
/// Caller must validate `value` against the model catalog first.
/// Empty string clears the field (falls back to remote/built-in default).
/// Length over [`MAX_DEFAULT_MODEL_LEN`] returns `Err`.
pub async fn set_default_model(value: String) -> Result<()> {
    super::campaigns::persist_models_default(
        if value.is_empty() { None } else { Some(value) },
        None,
    )
    .await
}

/// Persist the selected Kimi service as `[models].kimi_endpoint`.
/// Runtime clients apply the same selection through the ACP extension after
/// this write succeeds, so they can own rollback if live application fails.
pub async fn set_kimi_endpoint(endpoint: crate::kimi_models::KimiApiEndpoint) -> Result<()> {
    update_config(|cfg| cfg.models.kimi_endpoint = endpoint).await
}

/// String boundary for Settings clients. Only canonical `platform` and `code`
/// values are accepted; persistence remains typed after validation.
pub async fn set_kimi_api_endpoint(value: String) -> Result<()> {
    let endpoint =
        crate::kimi_models::KimiApiEndpoint::from_canonical(&value).ok_or_else(|| {
            anyhow::anyhow!("invalid Kimi API endpoint `{value}`; expected platform or code")
        })?;
    set_kimi_endpoint(endpoint).await
}

/// Persist an explicit `[models].recap` model id. Empty clears the pin and
/// restores provider-aware Automatic selection.
pub async fn set_recap_model(value: String) -> Result<()> {
    let value = optional_model_pin("recap", value)?;
    update_config(|cfg| {
        cfg.models.recap = value;
    })
    .await
}

/// Persist an explicit `[models].memory` model id. Empty clears the pin and
/// restores provider-aware Automatic selection.
pub async fn set_memory_model(value: String) -> Result<()> {
    let value = optional_model_pin("memory", value)?;
    update_config(|cfg| {
        cfg.models.memory = value;
    })
    .await
}

/// Persist `[privacy].privacy_banner_acked` (RFC 3339 UTC dismiss time).
pub async fn set_privacy_banner_acked(acked_at_rfc3339: String) -> Result<()> {
    update_config(|cfg| {
        cfg.privacy.privacy_banner_acked = Some(acked_at_rfc3339);
    })
    .await
}

/// Persist `[ui].fork_secondary_model` via `update_config`.
///
/// Caller must validate against the model catalog. Empty string
/// restores the built-in default. Length > [`MAX_DEFAULT_MODEL_LEN`] → `Err`.
pub async fn set_fork_secondary_model(value: String) -> Result<()> {
    if value.len() > MAX_DEFAULT_MODEL_LEN {
        anyhow::bail!(
            "fork_secondary_model name too long ({} > {} bytes)",
            value.len(),
            MAX_DEFAULT_MODEL_LEN
        );
    }
    update_config(|cfg| {
        cfg.ui.fork_secondary_model = if value.is_empty() {
            crate::models::default_model().to_string()
        } else {
            value
        };
    })
    .await
}

/// Bounds for [`set_max_thoughts_width`]. Mirrored from the pager's
/// registry consts; a CI test pins the agreement.
const MAX_THOUGHTS_WIDTH_SHELL_MIN: i64 = 40;
const MAX_THOUGHTS_WIDTH_SHELL_MAX: i64 = 500;

/// Persist `[ui].max_thoughts_width` via `update_config`.
/// Defensively clamps to `[40, 500]` at the shell boundary.
pub async fn set_max_thoughts_width(value: i64) -> Result<()> {
    let clamped = value.clamp(MAX_THOUGHTS_WIDTH_SHELL_MIN, MAX_THOUGHTS_WIDTH_SHELL_MAX) as u16;
    update_config(|cfg| cfg.ui.max_thoughts_width = clamped).await
}

/// Persist `[ui].scroll_speed` via `update_config`.
/// Defensively clamps to `[1, 100]` at the shell boundary.
pub async fn set_scroll_speed(value: i64) -> Result<()> {
    let clamped = value.clamp(1, 100) as u8;
    update_config(|cfg| cfg.ui.scroll_speed = Some(clamped)).await
}

/// Persist `[ui].scroll_mode` (`auto` | `wheel` | `trackpad`) via `update_config`.
pub async fn set_scroll_mode(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.scroll_mode = Some(value)).await
}

/// Persist `[ui].invert_scroll` via `update_config`.
pub async fn set_invert_scroll(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.invert_scroll = Some(value)).await
}

/// Persist `[ui.display_refresh].auto_cadence_enabled` via `update_config`.
/// Nested field only — does not replace the whole `display_refresh` object.
pub async fn set_display_refresh_auto_cadence(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.display_refresh.auto_cadence_enabled = Some(value)).await
}

/// Persist `[ui].scroll_lines` via `update_config`.
/// Defensively clamps to `[1, 10]` at the shell boundary.
pub async fn set_scroll_lines(value: i64) -> Result<()> {
    let clamped = value.clamp(1, 10) as u8;
    update_config(|cfg| cfg.ui.scroll_lines = Some(clamped)).await
}

/// Persist `[ui].vim_mode` via `update_config`.
pub async fn set_vim_mode(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.vim_mode = Some(value)).await
}

/// Persist `[ui].remember_tool_approvals` via `update_config`.
pub async fn set_remember_tool_approvals(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.remember_tool_approvals = Some(value)).await
}

/// Persist the restart-required `[ui].code_mode` override.
pub async fn set_code_mode(value: crate::agent::config::ToolModePreference) -> Result<()> {
    update_config(|cfg| cfg.ui.code_mode = Some(value)).await
}

/// Persist `[ui].show_thinking_blocks` via `update_config`.
pub async fn set_show_thinking_blocks(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.show_thinking_blocks = Some(value)).await
}

/// Persist `[ui].prompt_suggestions` via `update_config`.
pub async fn set_prompt_suggestions(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.prompt_suggestions = Some(value)).await
}

/// Persist `[toolset.ask_user_question].timeout_enabled` via `update_config`
/// (the user tier of the shell's tiered resolver; the effective value is
/// re-resolved at agent build).
pub async fn set_ask_user_question_timeout_enabled(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ask_user_question.timeout_enabled = Some(value)).await
}

/// Persist `[ui].group_tool_verbs` via `update_config`.
pub async fn set_group_tool_verbs(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.group_tool_verbs = Some(value)).await
}

/// Persist `[ui].collapsed_edit_blocks` via `update_config`.
pub async fn set_collapsed_edit_blocks(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.collapsed_edit_blocks = Some(value)).await
}

/// Persist `[ui].keep_text_selection` (`flash` | `hold` | `word_select`).
/// Clears the legacy `selection_highlight_duration_ms` and the retired
/// `double_click_action` keys it supersedes so the two can never drift (one-shot
/// disk migration away from the legacy key on any Settings write).
pub async fn set_keep_text_selection(value: String) -> Result<()> {
    update_config(|cfg| {
        cfg.ui.keep_text_selection = Some(value);
        cfg.ui.selection_highlight_duration_ms = None;
        cfg.ui.double_click_action = None;
    })
    .await
}

/// Persist `[ui].render_mermaid` via `update_config`. Value is one of the
/// canonical strings `auto` | `on` | `off`.
pub async fn set_render_mermaid(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.render_mermaid = Some(value)).await
}

/// Persist `[ui].hunk_tracker_mode` via `update_config`. Value is one of the
/// canonical strings `agent_only` | `all_dirty` | `off`.
/// Restart-required: the mode is read once at connect time.
pub async fn set_hunk_tracker_mode(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.hunk_tracker_mode = Some(value)).await
}

/// Persist `[ui].voice_capture_mode` via `update_config`. Value is one of the
/// canonical strings `toggle` | `hold`.
pub async fn set_voice_capture_mode(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.voice_capture_mode = Some(value)).await
}

/// Persist `[ui].voice_stt_language` via `update_config`. Value is a canonical
/// language code from the settings catalog (`en`, `es`, …) or `auto` (system
/// locale, falling back to English).
pub async fn set_voice_stt_language(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.voice_stt_language = Some(value)).await
}

/// Persist `[ui].voice_keybind_enabled` via `update_config`. When `false` the
/// Ctrl+Space / F8 voice chord is ignored (`/voice` still works).
pub async fn set_voice_keybind_enabled(value: bool) -> Result<()> {
    update_config(|cfg| cfg.ui.voice_keybind_enabled = Some(value)).await
}

/// Persist `[ui].default_selected_permission` via `update_config`. Value is
/// one of the canonical strings from `DEFAULT_SELECTED_PERMISSION_CHOICES`
/// (`default` | `allow_once` | `allow_always` | `reject`); `default` is the
/// "no preselection" sentinel.
pub async fn set_default_selected_permission(value: String) -> Result<()> {
    update_config(|cfg| cfg.ui.default_selected_permission = Some(value)).await
}

/// Persist `[ui].cancel_subagents_on_turn_cancel` via `update_config`.
/// Canonical values: `ask` (clear / prompt each time), `always_stop`,
/// `always_continue`.
pub async fn set_cancel_subagents_on_turn_cancel(value: String) -> Result<()> {
    update_config(|cfg| {
        cfg.ui.cancel_subagents_on_turn_cancel = if value == "ask" { None } else { Some(value) };
    })
    .await
}

/// Persist `[ui].screen_mode` (`fullscreen` | `minimal`). Empty clears the key.
pub async fn set_screen_mode(value: String) -> Result<()> {
    update_config(|cfg| {
        cfg.ui.screen_mode = if value.is_empty() { None } else { Some(value) };
    })
    .await
}

/// Persist `[cli].show_tips` via `update_config`.
/// Restart-required: `resolve_tips` reads this once at startup.
pub async fn set_show_tips(value: bool) -> Result<()> {
    update_config(|cfg| cfg.cli.show_tips = Some(value)).await
}

/// Persist `[cli].auto_update` via `update_config`.
/// Restart-required: auto-update check fires once on startup.
pub async fn set_auto_update(value: bool) -> Result<()> {
    update_config(|cfg| cfg.cli.auto_update = Some(value)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auxiliary_model_pin_empty_clears_and_id_is_preserved() {
        assert_eq!(optional_model_pin("recap", String::new()).unwrap(), None);
        assert_eq!(
            optional_model_pin("memory", "gpt-5.6-terra".to_string()).unwrap(),
            Some("gpt-5.6-terra".to_string())
        );
    }

    #[test]
    fn auxiliary_model_pin_rejects_oversized_ids() {
        let err = optional_model_pin("recap", "x".repeat(MAX_DEFAULT_MODEL_LEN + 1))
            .expect_err("oversized model IDs must be rejected");
        assert!(err.to_string().contains("recap model id too long"));
    }

    #[test]
    fn kimi_endpoint_serializes_with_canonical_values() {
        let mut models = crate::agent::config::ModelsConfig::default();
        assert_eq!(
            models.kimi_endpoint,
            crate::kimi_models::KimiApiEndpoint::Platform
        );
        models.kimi_endpoint = crate::kimi_models::KimiApiEndpoint::Code;
        let value = toml::Value::try_from(&models).expect("serialize ModelsConfig");
        assert_eq!(
            value.get("kimi_endpoint").and_then(toml::Value::as_str),
            Some("code")
        );
    }

    #[test]
    fn kimi_endpoint_string_boundary_rejects_noncanonical_values() {
        assert_eq!(
            crate::kimi_models::KimiApiEndpoint::from_canonical("coding"),
            None
        );
    }
}
