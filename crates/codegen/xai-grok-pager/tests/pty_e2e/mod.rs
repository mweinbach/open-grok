//! End-to-end tests for the built `xai-grok-pager` binary, driven through a PTY.
//!
//! Mirrors `xai-grok-shell/tests/test_built_binary_e2e.rs`: every test is
//! `#[ignore]` so `cargo test` doesn't run them by default, and CI opts in
//! via `-- --ignored`. The pager binary path is resolved from:
//!
//! 1. `PAGER_BINARY` env var (set by CI after downloading the release artifact)
//! 2. `GROK_BINARY` env var (shared convention with the shell crate)
//! 3. `CARGO_BIN_EXE_xai-grok-pager` (set by `cargo test` for in-tree runs)
//! 4. A locally-built debug binary — built on first run if missing
//!
//! The harness (spawn / screen state / frame timing / mock inference server)
//! lives in the [`xai_grok_pager_pty_harness`] crate, not here, so its deps
//! never land in the production pager binary.
//!
//! Run locally:
//! ```bash
//! cargo test -p xai-grok-pager --test pty_e2e -- --ignored --nocapture
//! ```
//!
//! Run against a pre-built CI artifact:
//! ```bash
//! PAGER_BINARY=./artifacts/grok-${VERSION}-linux-x86_64 \
//!   cargo test -p xai-grok-pager --test pty_e2e -- --ignored --nocapture
//! ```

mod common;
// Scroll-test support (wheel-burst drivers, viewport markers) — not a test.
mod scroll;

mod agent_response;
mod agent_type_mismatch_modal_on_model_switch;
mod agent_type_mismatch_no_keeps_current_session;
mod agent_type_mismatch_yes_starts_new_session;
mod ansi_scrollback_content_integrity;
mod auto_compact_top_row;
mod auto_wake_cancel_preserves_queued_user_prompt;
mod background_task_reaped_on_quit;
mod basename_path_demo_pty;
mod bash_full_output_double_click_fold_pty;
mod bash_mode_file_completion_shell_like;
mod bash_mode_strips_redundant_session_cd_from_chrome;
mod bash_mode_tab_completion_dropdown;
mod bash_queued_mid_turn_drains_as_bash;
mod bracketed_ime_paste_skips_clipboard_image_linux;
mod bracketed_ime_paste_skips_clipboard_image_macos;
mod campaign_nudges_default_until_dismissed_by_model_pick;
mod campaign_remote_settings_nudge_and_dismiss;
mod cancel_discards_buffered_interjection;
mod cancel_then_resend_prompt_appears_once;
mod continue_resumes_session_with_history;
mod critical_announcement_session_banner_pty;
mod ctrl_c_cancel_during_stream_recovers_cleanly;
mod ctrlc_after_activity_no_rewind_prompt_once;
mod ctrlc_with_queued_prompt_no_dup;
mod dashboard_overlay_tab_esc_backout_and_ctrl_backslash;
mod doubled_lines_out_of_band_repro;
mod drag_autoscroll_no_bounce_pty;
mod drag_enters_content_from_gap_pty;
mod drag_from_above_prompt_strip_pty;
mod drag_from_chrome_stays_block_pty;
mod drag_over_gap_rows_does_not_freeze_head_pty;
mod drag_select_autoscroll_full_scrollout_copy_pty;
mod drag_select_wheel_scroll_extends_pty;
mod edit_collapsed_oneliner_pty;
mod edit_hl_inplace_refresh_pty;
mod edit_interject_lone_queued_row_keeps_tui_alive;
mod empty_enter_force_sends_top_queued;
mod empty_enter_sends_top_not_last_of_two;
mod endline_park_two_static_markers;
mod endline_wake_markers_close_each_wakeup;
mod esc_esc_clears_idle_prompt_and_records_history;
mod esc_esc_opens_rewind_picker_silent_first_press;
mod esc_idle_empty_no_messages_is_swallowed_noop;
mod esc_mid_turn_from_prompt_is_swallowed_preserves_draft;
mod esc_mid_turn_from_scrollback_is_swallowed;
mod extensions_modal_copy_hints_pty;
mod file_path_with_space_emits_full_osc8_hyperlink;
mod folder_trust_cwd_is_home_git_repo_no_prompt;
mod folder_trust_decline_quits_without_grant;
mod folder_trust_feature_off_shows_no_question;
mod folder_trust_home_git_repo_subdir_keys_on_subdir;
mod folder_trust_question_renders_and_accept_persists_grant;
mod forced_wheel_mode_env_scrolls_exact_rows;
mod image_chip_preview_path_free_pty;
mod initial_prompt_positional_auto_submits;
mod input_echoes_at_idle_prompt;
mod interjection_reaches_model_ctrl_l_in_vscode_family;
mod interjection_reaches_model_in_same_turn;
mod keep_text_selection_settings_visible_pty;
// Leader-mode cluster cases live in the sibling `leader_pty_e2e` target so
// their multi-process bring-up never contends with this suite.
mod managed_policy_gate_refusal_reaches_real_terminal;
mod mcp_menu_loads_servers_in_non_project_dir;
mod mcp_menu_loads_servers_in_project_dir;
mod mid_text_skill_token_echo_styled_pty;
mod mid_turn_slash_dropdown_esc_dismisses_not_cancel;
mod middle_click_pastes_primary_linux;
// Experimental `--minimal` (scrollback-native) mode e2e tests — grouped in one
// subtree so the full-pager suite isn't interleaved with them (see minimal/mod.rs).
mod minimal;
mod misclassified_wheel_flood_does_not_teleport_viewport;
mod mouse_reporting_toggle_inactive_without_config_pty;
mod mouse_reporting_toggle_sticky_persists_pty;
mod nested_quote_drag_copy_excludes_bars_pty;
mod paste_bracketed_chip_text_sends_full_payload;
mod paste_bracketed_inline_text_echoes_and_sends_intact;
mod paste_bracketed_then_immediate_enter_sends_intact;
mod paste_ctrl_v_image_keeps_ui_responsive_macos;
mod paste_ctrl_v_image_keeps_ui_responsive_windows;
mod paste_ctrl_v_text_echoes_fast_macos;
mod paste_ctrl_v_text_echoes_fast_windows;
mod prompt_suggestion_ghost_tab_accepts;
mod queue_and_interjection_lifecycle;
mod queued_bash_promotion_renders_output_pty;
mod queued_message_renders_once_not_twice;
mod quote_block_drag_copy_excludes_bars_pty;
mod quote_block_raw_mode_copy_keeps_source_pty;
mod read_tool_header_selection_copies_path_only_pty;
mod reasoning_efforts_fallback_menu_matches_builtin;
mod reasoning_efforts_from_config_toml_menu;
mod reasoning_efforts_menu_renders_and_remaps_on_wire;
mod recap_header_not_in_selection_pty;
mod removed_queued_prompt_never_sent;
mod rename_title_shows_in_prompt_border;
mod renders_on_action;
mod reparked_wait_repushes_buried_marker;
mod requirements_version_failure_exits_2_with_guidance;
mod resize_preserves_scroll_position;
mod reverse_agent_type_mismatch_cursor_to_default;
mod same_agent_type_switch_no_modal;
mod scroll_debug_hud_env_toggles_overlay;
mod scroll_does_not_crash;
mod send_now_tip_after_mid_turn_queue;
mod send_then_ctrlc_rewinds_to_composer_no_history_dup;
mod shift_tab_in_session_cycles_mode;
mod shift_tab_on_welcome_starts_session_in_plan_mode;
mod shift_tab_plan_nudge_from_always_approve_enters_plan;
mod show_thinking_blocks_toggle_hides_existing_pty;
mod small_screen_tip_survives_slow_turn;
mod spinner_reappears_after_wait_resumes;
mod storage_upload_parks_on_401_and_drains_after_recovery;
mod stuck_drag_recovers_on_esc_pty;
mod subscription_watch_and_gate_verify_pty;
mod tab_focuses_scrollback_in_vim_and_default_modes;
mod trackpad_flood_does_not_under_travel;
mod undo_tip_resets_each_new_session;
mod undo_tip_seen_count_never_persisted;
mod undo_tip_session_cap_blocks_fourth_show;
mod verb_group_fold_expand_collapse_pty;
mod verb_group_header_drag_copy_pty;
mod verb_group_settings_toggle_pty;
mod verb_group_streaming_fold_pty;
mod verb_group_thinking_fold_pty;
mod verify_bashq_claim2_force_interject;
mod verify_bashq_claim3_edit_keeps_bash;
mod waiting_for_model_label;
mod welcome_screen;
mod welcome_screen_braille_logo_renders_correctly;
mod wheel_burst_scrolls_viewport_without_frame_amplification;
mod wheel_flood_paints_no_ghost_frames;
mod wheel_overscroll_at_bottom_reengages_follow_mid_stream;
mod wheel_scrolls_viewport_during_streaming_turn;
mod word_select_tip_on_double_click_pty;
mod wrap_echo_passthrough_and_exit_code;
mod wrap_explicit_path_not_found_fails_fast;
mod wrap_not_found_alias_routes_via_shell_contract;
mod wrap_osc52_sink_env_advertised_through_shell;
mod wrap_single_string_routes_via_shell;
mod zero_turn_model_switch_no_modal;
