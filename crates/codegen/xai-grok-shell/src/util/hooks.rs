//! Host bindings for [`xai_grok_hooks::discovery`]. Discovery itself lives in the hooks crate.
//! Bound here are the two inputs that crate cannot read: the Claude import cutoff and the managed-settings hooks pin.

use std::path::Path;

use xai_grok_hooks::error::HookError;

/// The `[claude_compat] imported = true` cutoff, read once per process in [`crate::claude_import`].
/// Every entry point below passes it down so the hooks crate stays free of shell state.
fn claude_import_marked() -> bool {
    crate::claude_import::is_claude_import_marked_with_log("discover_hook_source_paths")
}

/// The disabled-hooks file plus the resolved `allow_managed_hooks_only` pin.
pub(crate) fn disabled_hooks_snapshot() -> xai_grok_hooks::trust::DisabledHooks {
    let managed_only = xai_grok_workspace::permission::resolution::managed_settings()
        .non_managed_hooks
        .is_disabled();
    xai_grok_hooks::trust::DisabledHooks::load(managed_only)
}

/// Single load entry point: [`xai_grok_hooks::discovery::discover_hooks`] with the Claude cutoff applied.
/// Every session-startup and mid-session reload site routes through here so the source policy stays in one place.
pub(crate) fn discover_hooks(
    git_root: Option<&Path>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
    trusted: bool,
) -> (xai_grok_hooks::discovery::HookRegistry, Vec<HookError>) {
    xai_grok_hooks::discovery::discover_hooks(git_root, compat, claude_import_marked(), trusted)
}

/// [`xai_grok_hooks::discovery::assemble_hooks`] with the Claude cutoff applied, for callers that
/// supply their own config layers.
pub(crate) fn assemble_hooks(
    config_layers: &[xai_grok_config::HookConfigLayer],
    git_root: Option<&Path>,
    compat: &xai_grok_tools::types::compat::CompatConfig,
    trusted: bool,
) -> (xai_grok_hooks::discovery::HookRegistry, Vec<HookError>) {
    xai_grok_hooks::discovery::assemble_hooks(
        config_layers,
        git_root,
        compat,
        claude_import_marked(),
        trusted,
    )
}
