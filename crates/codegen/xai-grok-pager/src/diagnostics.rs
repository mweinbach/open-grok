//! Route-aware terminal diagnostics engine.
//!
//! Classifies startup warnings by category using [`TerminalContext`] and an
//! injectable [`TmuxOptionQuery`] trait. Warnings are data-only; the engine
//! returns `Vec<TerminalWarning>` for downstream banner rendering.

use std::process::Command;

use crate::notifications::NotificationCondition;
use crate::notifications::protocol::NotificationProtocol;
use crate::terminal::{ByobuBackend, MultiplexerKind, TerminalContext, TerminalName};
use crate::theme::ThemeKind;
use crate::theme::color_support::ColorLevel;

/// Broad classification of a startup warning.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WarningCategory {
    /// OSC 52 clipboard passthrough is misconfigured in tmux.
    Clipboard,
    /// DCS passthrough is disabled (nested clipboard path).
    DcsPassthrough,
    /// tmux control-mode degrades the fullscreen experience.
    ControlMode,
    /// The session is running inside Byobu backed by GNU screen — best-effort
    /// support only.
    ByobuScreen,
    /// The terminal emulator (Apple Terminal.app) does not support OSC 52
    /// clipboard escape sequences.
    UnsupportedTerminal,
    /// tmux 3.3+ has `extended-keys` set to `off`, so kitty CSI-u responses
    /// would be stripped before they reach the pager.
    TmuxExtendedKeysOff,
    /// The terminal is unrecognised and the notification protocol fell back to
    /// BEL (audible bell only).
    NotificationProtocolFallback,
    /// The terminal does not reliably support CSI focus-tracking events, so
    /// `condition = "unfocused"` will never fire.
    FocusTrackingUnavailable,
    /// WezTerm with the Kitty keyboard protocol inactive (its
    /// `enable_kitty_keyboard` option defaults to `false`), so Shift+Enter
    /// is byte-identical to Enter and can't insert newlines — and WezTerm's
    /// default Alt+Enter binding (ToggleFullScreen) eats the fallback chord.
    WezTermKittyKeyboardOff,
    /// Wayland session whose compositor lacks the data-control clipboard
    /// protocol (GNOME ≤ 47), so every native copy rides a focus-dependent
    /// path (arboard via the XWayland selection bridge, `wl-copy`'s
    /// no-data-control fallback) and fails if the terminal loses focus
    /// mid-copy.
    WaylandNoDataControl,
    /// Below truecolor: truecolor themes hidden. `/terminal-setup` only.
    LimitedColorSupport,
}

/// A structured startup warning carrying category, human-readable description,
/// and optional fix guidance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalWarning {
    /// The warning category for downstream grouping and filtering.
    pub category: WarningCategory,
    /// A short human-readable description of the problem.
    pub message: String,
    /// Optional fix instruction (e.g. a tmux config line to add).
    pub fix: Option<String>,
    /// The config file path where the fix should be applied, when applicable.
    pub config_path: Option<String>,
    /// Optional note about trade-offs or side effects of the fix.
    pub note: Option<String>,
}

impl TerminalWarning {
    /// Create a new warning with all fields.
    fn new(
        category: WarningCategory,
        message: &str,
        fix: Option<&str>,
        config_path: Option<&str>,
    ) -> Self {
        Self {
            category,
            message: message.to_string(),
            fix: fix.map(|s| s.to_string()),
            config_path: config_path.map(|s| s.to_string()),
            note: None,
        }
    }
}

/// Summarize a list of terminal warnings into a single [`StartupWarning`]
/// for the welcome screen. Returns `None` if there are no warnings or if
/// none of the warnings are in the allow-list of safe-to-surface categories.
///
/// The welcome screen doesn't need to know what's wrong -- only that
/// something is wrong and where to go for details.
pub fn summarize_warnings(warnings: &[TerminalWarning]) -> Option<crate::startup::StartupWarning> {
    // Only surface warnings over SSH — locally these tmux misconfigurations
    // don't actually break clipboard, so showing the banner would be noise.
    let is_ssh = xai_grok_shell::util::clipboard::is_remote_session();
    summarize_warnings_inner(warnings, is_ssh)
}

/// Inner implementation of [`summarize_warnings`] that accepts the SSH flag
/// as a parameter so tests can exercise both paths without manipulating
/// environment variables.
fn summarize_warnings_inner(
    warnings: &[TerminalWarning],
    is_ssh: bool,
) -> Option<crate::startup::StartupWarning> {
    if !is_ssh {
        return None;
    }
    // Allow-list of categories where detection is a direct tmux subprocess
    // query that only triggers on an explicit non-good value, and the fix is
    // a single config line. Other categories stay suppressed until their
    // false-positive rate is characterized.
    warnings.iter().find(|w| {
        matches!(
            w.category,
            WarningCategory::TmuxExtendedKeysOff | WarningCategory::DcsPassthrough
        )
    })?;
    Some(crate::startup::StartupWarning {
        severity: crate::startup::WarningSeverity::Warning,
        message: "Clipboard may be unreachable.".to_string(),
        action: Some("See /terminal-setup for potential fixes.".to_string()),
    })
}

/// Abstraction over tmux option queries so diagnostic logic can be tested
/// without a live tmux server.
///
/// The two methods mirror the subprocess helpers [`tmux_show_option`] and
/// [`tmux_option_exists`] but can be replaced with deterministic fixtures in
/// tests.
pub trait TmuxOptionQuery {
    /// Return the value of a global tmux option, or `None` if the query fails
    /// or the value is empty.
    fn show_option(&self, option: &str) -> Option<String>;

    /// Return `true` if the tmux server recognises `option` as a valid global
    /// option name.
    fn option_exists(&self, option: &str) -> bool;
}

/// The real, subprocess-backed implementation used at runtime.
pub struct LiveTmuxQuery;

impl TmuxOptionQuery for LiveTmuxQuery {
    fn show_option(&self, option: &str) -> Option<String> {
        tmux_show_option(option)
    }

    fn option_exists(&self, option: &str) -> bool {
        tmux_option_exists(option)
    }
}

/// Collect all applicable startup warnings for the current terminal context.
///
/// This is the primary entry point for the diagnostics engine. It returns
/// structured warnings as data — no stderr output, no sleep, no side effects.
///
/// # Arguments
///
/// * `ctx` — The resolved terminal context (multiplexer, Byobu state, etc.).
/// * `query` — A tmux option query implementation (live or fake for tests).
/// * `is_control_mode` — Whether the tmux session is in control mode.
/// * `fullscreen_active` — Whether fullscreen (alt-screen) is effectively
///   active. Used to tailor the control-mode warning message.
pub fn collect_startup_warnings(
    ctx: &TerminalContext,
    query: &dyn TmuxOptionQuery,
    is_control_mode: bool,
    fullscreen_active: bool,
) -> Vec<TerminalWarning> {
    let mut warnings = Vec::new();

    // Apple Terminal.app does not support OSC 52. Over SSH, this means
    // clipboard writes can never reach the user's local machine.
    if ctx.brand == TerminalName::AppleTerminal
        && xai_grok_shell::util::clipboard::is_remote_session()
    {
        warnings.push(TerminalWarning::new(
            WarningCategory::UnsupportedTerminal,
            "macOS Terminal does not support clipboard escape sequences (OSC 52) \
             -- copy over SSH will not work.",
            None,
            None,
        ));
    }

    // Byobu-on-screen: best-effort warning, no further tmux-specific checks.
    if ctx.byobu == Some(ByobuBackend::Screen) {
        warnings.push(TerminalWarning::new(
            WarningCategory::ByobuScreen,
            "Byobu with GNU screen backend -- clipboard and display support is best-effort",
            None,
            None,
        ));
        return warnings;
    }

    // tmux control-mode warning — the message reflects the effective
    // fullscreen state so callers that force fullscreen in control mode
    // (e.g. `alt_screen = "always"`) see an accurate warning rather than a
    // blanket "inline mode" claim.
    if ctx.is_tmux_backed() && is_control_mode {
        let message = if fullscreen_active {
            "tmux control mode detected -- fullscreen may be unreliable in control mode"
        } else {
            "tmux control mode detected -- running in degraded inline mode"
        };
        warnings.push(TerminalWarning::new(
            WarningCategory::ControlMode,
            message,
            None,
            None,
        ));
    }

    // Resolve tmux config path once for all tmux-related warnings below.
    let config_path = ctx.tmux_config_path();

    // tmux-backed clipboard and DCS passthrough checks.
    if ctx.is_tmux_backed() {
        let clipboard_warnings = diagnose_clipboard_with_query(query, &config_path);
        warnings.extend(clipboard_warnings);
    }

    if ctx.kitty_skip_reason() == Some("tmux_extended_keys_off") {
        let mut warning = TerminalWarning::new(
            WarningCategory::TmuxExtendedKeysOff,
            "tmux extended-keys is off -- modifier key combinations may not reach the pager",
            Some("set -g extended-keys on"),
            Some(&config_path),
        );
        // Existing tmux sessions cache the option; without an explicit
        // reload the user will edit the config, see no change, and
        // conclude the fix is broken.
        warning.note = Some(format!(
            "Then reload tmux: `tmux source-file {config_path}` (or detach and reattach)."
        ));
        warnings.push(warning);
    }

    warnings
}

/// Warn when WezTerm is running without the Kitty keyboard protocol.
///
/// WezTerm ships `enable_kitty_keyboard = false` by default, so the pager's
/// runtime probe fails and no enhancement flags are pushed. Without KKP,
/// Shift+Enter arrives as a bare `CR` (submits instead of inserting a
/// newline), and WezTerm's default Alt+Enter binding (ToggleFullScreen)
/// swallows the usual fallback chord before it reaches the PTY.
///
/// WezTerm is recognized two ways:
/// - env detection (`ctx.brand`) for local sessions, and
/// - the async XTVERSION self-report (`xtversion_payload`) for SSH
///   sessions, where `TERM_PROGRAM` isn't forwarded and the brand falls
///   back to `Unknown`. The reply arrives through the event loop after
///   startup, so this path lights up for `/terminal-setup` (and any
///   warning pass re-run after the reply landed) rather than the very
///   first startup banner.
///
/// `kitty_flags_pushed` is the runtime negotiation outcome from
/// `init_terminal` (passed in so this stays a pure, testable function).
/// Returns `None` when KKP is active, when the terminal isn't WezTerm, or
/// when a non-WezTerm [`TerminalContext::kitty_skip_reason`] applies (e.g.
/// tmux) — in that case the wezterm.lua fix alone wouldn't help and other
/// warnings cover it.
pub fn wezterm_kitty_keyboard_warning(
    ctx: &TerminalContext,
    kitty_flags_pushed: bool,
    xtversion_payload: Option<&str>,
) -> Option<TerminalWarning> {
    let is_wezterm_by_env = ctx.brand == TerminalName::WezTerm;
    // SSH shape: brand Unknown, no multiplexer, over SSH, but the terminal
    // identified itself as WezTerm via XTVERSION. (Under tmux the reply
    // would describe tmux itself, and the brand gate below rejects it.)
    //
    // The `is_ssh` gate matters: a *local* WezTerm with a stripped
    // `TERM_PROGRAM` (brand falls back to Unknown) can still answer
    // XTVERSION with "WezTerm". Without this gate it would receive the
    // "over SSH" copy below, which is both wrong (it's local) and would
    // drop the actionable `wezterm.lua` fix. We only treat XTVERSION as
    // the WezTerm signal when env detection genuinely can't apply, i.e.
    // over SSH where `TERM_PROGRAM` isn't forwarded.
    let is_wezterm_by_xtversion = ctx.brand == TerminalName::Unknown
        && ctx.multiplexer == MultiplexerKind::Undetected
        && ctx.is_ssh
        && xtversion_payload.is_some_and(|v| v.trim_start().starts_with("WezTerm"));
    if !(is_wezterm_by_env || is_wezterm_by_xtversion) || kitty_flags_pushed {
        return None;
    }
    // For env-detected WezTerm, a skip reason (tmux etc.) means KKP was
    // never even probed and the wezterm.lua fix alone wouldn't help.
    if is_wezterm_by_env && ctx.kitty_skip_reason().is_some() {
        return None;
    }
    if is_wezterm_by_xtversion {
        // Over SSH the pager skips KKP for Unknown brands entirely (no
        // positive evidence policy — see `kitty_skip_reason`), so the
        // wezterm.lua change cannot clear this warning for SSH sessions.
        // Be honest: lead with the workaround that does work.
        let mut warning = TerminalWarning::new(
            WarningCategory::WezTermKittyKeyboardOff,
            "WezTerm over SSH: Shift+Enter can't insert newlines",
            None,
            None,
        );
        warning.note = Some(
            "Type `\\` then Enter to insert a newline. The pager doesn't negotiate the \
             kitty keyboard protocol over SSH yet; `enable_kitty_keyboard = true` in \
             wezterm.lua fixes local WezTerm sessions only."
                .to_string(),
        );
        return Some(warning);
    }
    let mut warning = TerminalWarning::new(
        WarningCategory::WezTermKittyKeyboardOff,
        "WezTerm: Shift+Enter can't insert newlines (kitty keyboard protocol is off)",
        Some("config.enable_kitty_keyboard = true"),
        Some("~/.config/wezterm/wezterm.lua"),
    );
    warning.note = Some(
        "Restart WezTerm after the change. Until then, type `\\` then Enter to insert \
         a newline."
            .to_string(),
    );
    Some(warning)
}

/// Assemble the welcome-screen startup warning list.
///
/// The WezTerm kitty-keyboard warning (when present) goes **first**: the
/// welcome screen renders only the first entry, and a broken-local-input
/// warning outranks the SSH clipboard advisories from
/// [`summarize_warnings`]. The Wayland no-data-control warning follows the
/// same bypass (surfaced locally — [`summarize_warnings`] is SSH-gated — but
/// after WezTerm: broken input outranks focus-dependent copies). Keeping the
/// banner copy here (instead of at the call site) ties it to the warnings so
/// the surfaces can't drift.
pub fn assemble_startup_warnings(
    wezterm_warning: Option<&TerminalWarning>,
    wayland_clipboard_warning: Option<&TerminalWarning>,
    mut summarized: Vec<crate::startup::StartupWarning>,
) -> Vec<crate::startup::StartupWarning> {
    if wayland_clipboard_warning.is_some() {
        summarized.insert(
            0,
            crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: "Copies need this terminal to stay focused.".to_string(),
                action: Some("See /terminal-setup for details.".to_string()),
            },
        );
    }
    if wezterm_warning.is_some() {
        summarized.insert(
            0,
            crate::startup::StartupWarning {
                severity: crate::startup::WarningSeverity::Warning,
                message: "Shift+Enter newlines need a WezTerm config change.".to_string(),
                action: Some("See /terminal-setup for the fix.".to_string()),
            },
        );
    }
    summarized
}

/// Returns `true` if the terminal brand is known to support CSI focus-tracking
/// events (`\x1b[?1004h` / focus-in / focus-out sequences).
fn supports_focus_tracking(brand: TerminalName) -> bool {
    brand != TerminalName::AppleTerminal && !brand.is_capability_unclassified()
}

/// Collect notification-specific startup warnings.
///
/// These complement the general terminal warnings from
/// [`collect_startup_warnings`] and depend on the resolved notification
/// protocol and condition.
pub fn collect_notification_warnings(
    ctx: &TerminalContext,
    protocol: NotificationProtocol,
    condition: NotificationCondition,
    query: &dyn TmuxOptionQuery,
) -> Vec<TerminalWarning> {
    let mut warnings = Vec::new();

    // Protocol fallback: BEL selected for an unknown terminal in auto mode.
    if protocol == NotificationProtocol::Bel && ctx.brand == TerminalName::Unknown {
        warnings.push(TerminalWarning::new(
            WarningCategory::NotificationProtocolFallback,
            "notification protocol fell back to BEL -- terminal not recognized",
            None,
            None,
        ));
    }

    // tmux + OSC protocol: allow-passthrough must be on or OSC notification
    // sequences wrapped in DCS passthrough will be silently dropped.
    if ctx.is_tmux_backed()
        && matches!(
            protocol,
            NotificationProtocol::Osc9 | NotificationProtocol::Osc99 | NotificationProtocol::Osc777
        )
        && query.option_exists("allow-passthrough")
        && let Some(val) = query.show_option("allow-passthrough")
        && !matches!(val.as_str(), "on" | "all")
    {
        let config_path = ctx.tmux_config_path();
        warnings.push(TerminalWarning::new(
            WarningCategory::DcsPassthrough,
            "tmux allow-passthrough is off -- OSC notifications will not reach the terminal",
            Some("set -g allow-passthrough on"),
            Some(&config_path),
        ));
    }

    // Focus tracking: if the terminal doesn't support it and the condition
    // is "unfocused", notifications will never fire because the pager will
    // always think the window is focused.
    if condition == NotificationCondition::Unfocused && !supports_focus_tracking(ctx.brand) {
        warnings.push(TerminalWarning::new(
            WarningCategory::FocusTrackingUnavailable,
            "focus tracking may not be supported -- unfocused notifications may not fire",
            Some("set condition = \"always\" in [ui.notifications]"),
            None,
        ));
    }

    warnings
}

/// Query tmux clipboard settings via the given [`TmuxOptionQuery`] and return
/// structured clipboard/DCS warnings.
fn diagnose_clipboard_with_query(
    query: &dyn TmuxOptionQuery,
    config_path: &str,
) -> Vec<TerminalWarning> {
    let set_clipboard = query.show_option("set-clipboard");
    let passthrough_exists = query.option_exists("allow-passthrough");
    let allow_passthrough = if passthrough_exists {
        query.show_option("allow-passthrough")
    } else {
        None
    };

    diagnose_clipboard_from_values(
        set_clipboard.as_deref(),
        passthrough_exists,
        allow_passthrough.as_deref(),
        config_path,
    )
}

/// Pure clipboard diagnostic logic — determines which tmux clipboard settings
/// are misconfigured and returns structured warnings.
///
/// **Query-failure semantics:** `None` for `set_clipboard` or
/// `allow_passthrough` means the tmux query could not obtain a value (e.g. the
/// tmux server is unreachable or the option is unset).  This is *not* proof
/// that the setting is disabled, so `None` does **not** trigger a
/// misconfiguration warning.  Only an explicit non-good value (e.g. `"off"`)
/// produces a warning with remediation guidance.
///
/// - `set_clipboard`: value of `set-clipboard` (`None` = query unavailable).
/// - `passthrough_exists`: whether the tmux server knows the `allow-passthrough`
///   option (introduced in tmux 3.3; older versions don't have it).
/// - `allow_passthrough`: value of `allow-passthrough` when it exists.
/// - `config_path`: the tmux config file path for fix guidance.
pub fn diagnose_clipboard_from_values(
    set_clipboard: Option<&str>,
    passthrough_exists: bool,
    allow_passthrough: Option<&str>,
    config_path: &str,
) -> Vec<TerminalWarning> {
    let mut warnings = Vec::new();

    // set-clipboard: required for OSC 52 passthrough so the pager can write to
    // the user's local clipboard.
    //
    // `None` = query failed or value unavailable → do not claim it is disabled.
    // Only warn when the query returned an explicit non-good value.
    if let Some(val) = set_clipboard
        && !matches!(val, "on" | "external")
    {
        warnings.push(TerminalWarning::new(
            WarningCategory::Clipboard,
            "OSC 52 clipboard passthrough is disabled",
            Some("set -g set-clipboard on"),
            Some(config_path),
        ));
    }

    // allow-passthrough: needed for DCS passthrough of OSC 52 in nested tmux.
    // This option was introduced in tmux 3.3. Before 3.3, DCS passthrough
    // worked unconditionally, so we only warn when the option actually exists.
    //
    // Same query-failure semantics: `None` does not produce a warning.
    if passthrough_exists
        && let Some(val) = allow_passthrough
        && !matches!(val, "on" | "all")
    {
        warnings.push(TerminalWarning::new(
            WarningCategory::DcsPassthrough,
            "DCS passthrough is disabled (needed for nested clipboard)",
            Some("set -g allow-passthrough on"),
            Some(config_path),
        ));
    }

    warnings
}

/// Pure Wayland clipboard diagnostic — flags the focus-dependent copy shape.
///
/// Warns only when the session is Wayland AND the compositor lacks the
/// data-control protocol: without it every native write (arboard via the
/// XWayland bridge, `wl-copy`'s fallback) needs the terminal focused until the
/// write completes, so alt-tabbing mid-copy loses the copy. When `wl-copy` is
/// also missing, the fix suggests installing wl-clipboard — a partial
/// mitigation (its verified write is the most reliable non-data-control
/// route). No warning when data-control is present (copies are focus-free) or
/// off Wayland.
pub fn diagnose_wayland_data_control(
    is_wayland: bool,
    data_control: bool,
    wl_copy_available: bool,
) -> Option<TerminalWarning> {
    if !is_wayland || data_control {
        return None;
    }
    let fix = (!wl_copy_available)
        .then_some("sudo apt install wl-clipboard  (or your distro's equivalent)");
    Some(TerminalWarning::new(
        WarningCategory::WaylandNoDataControl,
        "Wayland compositor without the data-control clipboard protocol -- \
         keep the terminal focused while copying until the copy toast confirms",
        fix,
        None,
    ))
}

/// Live-environment wrapper for [`diagnose_wayland_data_control`], called from
/// the two warning surfaces (startup event loop and `/terminal-setup`) — NOT
/// from `collect_startup_warnings`, whose integration tests stay hermetic on
/// Wayland dev boxes that way (pattern: `wezterm_kitty_keyboard_warning`).
///
/// The `is_wayland` short-circuits keep the compositor probe and the tool
/// probe off non-Wayland sessions. On a Wayland session `native_tool_name()`
/// resolves to "wl-copy" exactly when wl-copy is installed (it is the first
/// probe candidate).
pub fn diagnose_wayland_data_control_live() -> Option<TerminalWarning> {
    let is_wayland = crate::host::DisplayServer::current() == crate::host::DisplayServer::Wayland;
    diagnose_wayland_data_control(
        is_wayland,
        is_wayland && xai_grok_shell::util::clipboard::wayland_data_control_supported(),
        is_wayland && xai_grok_shell::util::clipboard::native_tool_name() == "wl-copy",
    )
}

/// `/terminal-setup` Environment `color` row.
pub fn format_color_env_line(level: ColorLevel) -> String {
    format!("  color        {}\n", level.as_str())
}

/// `/terminal-setup` Environment `themes` row (mirrors [`ThemeKind::available`]).
pub fn format_themes_env_line(level: ColorLevel) -> String {
    if level.has_truecolor() {
        return "  themes       all\n".to_string();
    }
    let names: Vec<&str> = ThemeKind::ALL
        .iter()
        .filter(|k| !k.requires_truecolor())
        .map(|k| k.display_name())
        .collect();
    format!(
        "  themes       {}/{}: {}\n",
        names.len(),
        ThemeKind::ALL.len(),
        names.join(", ")
    )
}

/// `/terminal-setup` warning when truecolor themes are locked out.
///
/// Not in `collect_startup_warnings` — limited color is normal on some
/// emulators and would spam the welcome banner.
pub fn color_support_warning(
    level: ColorLevel,
    brand: TerminalName,
    is_tmux_backed: bool,
    tmux_config_path: &str,
) -> Option<TerminalWarning> {
    if level.has_truecolor() {
        return None;
    }

    if level == ColorLevel::None {
        let mut warning = TerminalWarning::new(
            WarningCategory::LimitedColorSupport,
            "NO_COLOR set -- themed colors disabled",
            None,
            None,
        );
        warning.note = Some("Unset NO_COLOR and restart Grok.".to_string());
        return Some(warning);
    }

    let level_label = level.as_str();

    if brand == TerminalName::AppleTerminal {
        let mut warning = TerminalWarning::new(
            WarningCategory::LimitedColorSupport,
            "Terminal.app is 256-color -- truecolor themes unavailable",
            None,
            None,
        );
        warning.note = Some("Switch to a truecolor terminal (e.g. Ghostty).".to_string());
        return Some(warning);
    }

    if is_tmux_backed {
        let mut warning = TerminalWarning::new(
            WarningCategory::LimitedColorSupport,
            &format!("Color level is {level_label} -- truecolor themes unavailable"),
            Some("set -as terminal-features \",*:RGB\""),
            Some(tmux_config_path),
        );
        warning.note = Some(format!(
            "Also: set -g default-terminal \"tmux-256color\"; export COLORTERM=truecolor; \
             then `tmux source-file {tmux_config_path}`."
        ));
        return Some(warning);
    }

    let mut warning = TerminalWarning::new(
        WarningCategory::LimitedColorSupport,
        &format!("Color level is {level_label} -- truecolor themes unavailable"),
        Some("export COLORTERM=truecolor"),
        None,
    );
    warning.note = Some("Persist in ~/.zshrc / ~/.bashrc and restart Grok.".to_string());
    Some(warning)
}

/// Returns `true` if the tmux server recognises this option name.
///
/// Uses the non-quiet form (`tmux show-option -gv`) which returns non-zero for
/// unknown options, as opposed to `-q` which always returns 0.
fn tmux_option_exists(option: &str) -> bool {
    let mut cmd = Command::new("tmux");
    cmd.args(["show-option", "-gv", option])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null());
    xai_tty_utils::detach_std_command(&mut cmd);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

#[cfg(test)]
pub(crate) use crate::terminal::parse_tmux_show_option_output;
pub(crate) use crate::terminal::tmux_show_option;

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{
        ByobuBackend, MultiplexerKind, TerminalContext, TerminalName, TmuxClientMeta,
    };

    // -- Deterministic TmuxOptionQuery fixture --------------------------------

    /// A fake [`TmuxOptionQuery`] that returns pre-configured values without
    /// touching the tmux subprocess. Use this in all diagnostic tests to avoid
    /// host-state dependencies.
    struct FakeTmuxQuery {
        set_clipboard: Option<String>,
        allow_passthrough_exists: bool,
        allow_passthrough: Option<String>,
    }

    impl FakeTmuxQuery {
        /// Create a healthy modern tmux fixture (all options correct).
        fn healthy_modern() -> Self {
            Self {
                set_clipboard: Some("on".to_owned()),
                allow_passthrough_exists: true,
                allow_passthrough: Some("on".to_owned()),
            }
        }

        /// Create a fixture where no tmux server is reachable.
        fn unavailable() -> Self {
            Self {
                set_clipboard: None,
                allow_passthrough_exists: false,
                allow_passthrough: None,
            }
        }
    }

    impl TmuxOptionQuery for FakeTmuxQuery {
        fn show_option(&self, option: &str) -> Option<String> {
            match option {
                "set-clipboard" => self.set_clipboard.clone(),
                "allow-passthrough" => self.allow_passthrough.clone(),
                _ => None,
            }
        }

        fn option_exists(&self, option: &str) -> bool {
            match option {
                "allow-passthrough" => self.allow_passthrough_exists,
                "set-clipboard" => self.set_clipboard.is_some(),
                _ => false,
            }
        }
    }

    // -- Test context builders ------------------------------------------------

    fn plain_terminal_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Ghostty,
            ..Default::default()
        }
    }

    fn plain_tmux_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Iterm2,
            multiplexer: MultiplexerKind::Tmux,
            tmux_meta: TmuxClientMeta {
                tmux_env: Some("/tmp/tmux-501/default,12345,0".to_owned()),
                tmux_pane: Some("%0".to_owned()),
            },
            ..Default::default()
        }
    }

    fn byobu_tmux_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Unknown,
            multiplexer: MultiplexerKind::Tmux,
            byobu: Some(ByobuBackend::Tmux),
            tmux_meta: TmuxClientMeta {
                tmux_env: Some("/tmp/tmux-501/default,12345,0".to_owned()),
                tmux_pane: Some("%1".to_owned()),
            },
            ..Default::default()
        }
    }

    fn byobu_screen_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Unknown,
            multiplexer: MultiplexerKind::Screen,
            byobu: Some(ByobuBackend::Screen),
            ..Default::default()
        }
    }

    fn plain_screen_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Unknown,
            multiplexer: MultiplexerKind::Screen,
            ..Default::default()
        }
    }

    fn apple_terminal_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::AppleTerminal,
            ..Default::default()
        }
    }

    fn zellij_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Ghostty,
            multiplexer: MultiplexerKind::Zellij,
            ..Default::default()
        }
    }

    // =====================================================================
    // diagnose_clipboard_from_values: pure clipboard logic
    // =====================================================================

    #[test]
    fn clipboard_all_good_modern_tmux() {
        let w = diagnose_clipboard_from_values(Some("on"), true, Some("on"), "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_all_good_external() {
        let w = diagnose_clipboard_from_values(Some("external"), true, Some("all"), "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_all_good_old_tmux() {
        let w = diagnose_clipboard_from_values(Some("on"), false, None, "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_off_is_flagged() {
        let w = diagnose_clipboard_from_values(Some("off"), true, Some("on"), "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[0].fix.as_deref(), Some("set -g set-clipboard on"));
        assert_eq!(w[0].config_path.as_deref(), Some("~/.tmux.conf"));
    }

    #[test]
    fn clipboard_query_unavailable_does_not_warn() {
        // `None` means the query failed — not proof that the setting is
        // disabled.  No warning should be emitted.
        let w = diagnose_clipboard_from_values(None, true, Some("on"), "~/.tmux.conf");
        assert!(
            w.is_empty(),
            "Query-unavailable set-clipboard should not produce a warning"
        );
    }

    #[test]
    fn dcs_passthrough_off_is_flagged() {
        let w = diagnose_clipboard_from_values(Some("on"), true, Some("off"), "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
        assert_eq!(w[0].fix.as_deref(), Some("set -g allow-passthrough on"));
    }

    #[test]
    fn dcs_passthrough_query_unavailable_does_not_warn() {
        // `allow_passthrough` query returned `None` — the probe could not
        // obtain a value, so we must not claim the setting is disabled.
        let w = diagnose_clipboard_from_values(Some("on"), true, None, "~/.tmux.conf");
        assert!(
            w.is_empty(),
            "Query-unavailable allow-passthrough should not produce a warning"
        );
    }

    #[test]
    fn dcs_passthrough_not_checked_on_old_tmux() {
        let w = diagnose_clipboard_from_values(Some("on"), false, None, "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_both_bad_produces_two_warnings() {
        let w = diagnose_clipboard_from_values(Some("off"), true, Some("off"), "~/.tmux.conf");
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[1].category, WarningCategory::DcsPassthrough);
    }

    #[test]
    fn clipboard_both_bad_old_tmux_produces_one_warning() {
        let w = diagnose_clipboard_from_values(Some("off"), false, None, "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
    }

    #[test]
    fn clipboard_byobu_config_path_propagated() {
        let w =
            diagnose_clipboard_from_values(Some("off"), true, Some("on"), "~/.byobu/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
    }

    // =====================================================================
    // diagnose_wayland_data_control: pure Wayland clipboard logic
    // =====================================================================

    #[test]
    fn wayland_no_data_control_warns() {
        let w = diagnose_wayland_data_control(true, false, true).expect("must warn");
        assert_eq!(w.category, WarningCategory::WaylandNoDataControl);
        assert!(w.message.contains("focused"));
        assert!(w.fix.is_none(), "wl-copy present: nothing to install");
    }

    #[test]
    fn wayland_no_data_control_missing_wl_copy_suggests_install() {
        let w = diagnose_wayland_data_control(true, false, false).expect("must warn");
        assert_eq!(w.category, WarningCategory::WaylandNoDataControl);
        assert!(
            w.fix.as_deref().is_some_and(|f| f.contains("wl-clipboard")),
            "fix must suggest installing wl-clipboard, got: {:?}",
            w.fix
        );
    }

    #[test]
    fn wayland_with_data_control_is_quiet() {
        assert!(diagnose_wayland_data_control(true, true, true).is_none());
        assert!(diagnose_wayland_data_control(true, true, false).is_none());
    }

    #[test]
    fn non_wayland_is_quiet() {
        assert!(diagnose_wayland_data_control(false, false, false).is_none());
        assert!(diagnose_wayland_data_control(false, true, true).is_none());
    }

    // =====================================================================
    // collect_startup_warnings: full integration
    // =====================================================================

    // -- Plain terminal: no warnings ------------------------------------------

    #[test]
    fn plain_terminal_no_warnings() {
        let ctx = plain_terminal_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(w.is_empty(), "Plain terminal should produce no warnings");
    }

    // -- Healthy tmux: no warnings --------------------------------------------

    #[test]
    fn healthy_tmux_fullscreen_no_warnings() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(w.is_empty(), "Healthy tmux fullscreen should be quiet");
    }

    #[test]
    fn healthy_tmux_inline_no_warnings() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, false);
        assert!(w.is_empty(), "Healthy tmux inline should be quiet");
    }

    // -- tmux clipboard misconfiguration --------------------------------------

    #[test]
    fn tmux_clipboard_off_warns() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.tmux.conf"));
    }

    #[test]
    fn tmux_dcs_passthrough_off_warns() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.tmux.conf"));
    }

    #[test]
    fn tmux_both_clipboard_issues_warns_twice() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[1].category, WarningCategory::DcsPassthrough);
    }

    // -- tmux control mode ----------------------------------------------------

    #[test]
    fn tmux_control_mode_inline_warns_degraded() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        // control_mode=true, fullscreen_active=false → inline degraded message.
        let w = collect_startup_warnings(&ctx, &query, true, false);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ControlMode);
        assert!(
            w[0].message.contains("degraded inline"),
            "Inline control-mode should mention degraded inline mode"
        );
    }

    #[test]
    fn tmux_control_mode_fullscreen_warns_unreliable() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        // control_mode=true, fullscreen_active=true → fullscreen unreliable message.
        let w = collect_startup_warnings(&ctx, &query, true, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ControlMode);
        assert!(
            w[0].message.contains("unreliable"),
            "Fullscreen control-mode should warn about unreliable fullscreen"
        );
        assert!(
            !w[0].message.contains("degraded inline"),
            "Fullscreen control-mode should NOT mention degraded inline mode"
        );
    }

    #[test]
    fn tmux_control_mode_with_clipboard_issue_shows_both() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, true, false);
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::ControlMode));
        assert!(categories.contains(&WarningCategory::Clipboard));
    }

    // -- Byobu-on-tmux -------------------------------------------------------

    #[test]
    fn byobu_tmux_healthy_no_warnings() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(w.is_empty(), "Healthy Byobu-tmux should be quiet");
    }

    #[test]
    fn byobu_tmux_clipboard_off_uses_byobu_config_path() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
    }

    // -- Byobu-on-screen ------------------------------------------------------

    #[test]
    fn byobu_screen_warns_best_effort() {
        let ctx = byobu_screen_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ByobuScreen);
        assert!(w[0].fix.is_none(), "Byobu-screen has no actionable fix");
    }

    #[test]
    fn byobu_screen_does_not_show_tmux_warnings() {
        let ctx = byobu_screen_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        // Only the ByobuScreen warning, no clipboard/DCS warnings.
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ByobuScreen);
    }

    // -- Plain screen (no Byobu) ----------------------------------------------

    #[test]
    fn plain_screen_no_warnings() {
        let ctx = plain_screen_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Plain screen should not inherit Byobu or tmux warnings"
        );
    }

    // -- Zellij ---------------------------------------------------------------

    #[test]
    fn zellij_no_warnings() {
        let ctx = zellij_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, false);
        assert!(
            w.is_empty(),
            "Zellij should not show tmux or Byobu warnings"
        );
    }

    // -- Apple Terminal (unsupported OSC 52) ----------------------------------

    // Note: the Apple Terminal warning is gated on `is_remote_session()` which
    // reads ambient env vars. In CI / local dev, SSH_CONNECTION is typically
    // unset, so `collect_startup_warnings` won't fire this warning. We test
    // the rendering and category independently.

    #[test]
    fn apple_terminal_warning_category_exists() {
        // Verify the warning renders correctly via the banner.
        let w = [TerminalWarning::new(
            WarningCategory::UnsupportedTerminal,
            "macOS Terminal does not support clipboard escape sequences (OSC 52) \
             — copy over SSH will not work. Use iTerm2 or Ghostty instead",
            None,
            None,
        )];
        assert_eq!(w[0].category, WarningCategory::UnsupportedTerminal);
        assert!(w[0].fix.is_none(), "No fix — user must switch terminals");
    }

    #[test]
    fn apple_terminal_non_ssh_no_warning() {
        // When not over SSH, Apple Terminal can use pbcopy — no warning needed.
        // Since we can't inject `is_remote_session()`, we verify that the
        // context builder with AppleTerminal + no SSH env doesn't produce
        // the warning in a typical local environment.
        let ctx = apple_terminal_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        // In a non-SSH test environment, this should be empty.
        // (If CI sets SSH_CONNECTION, this test would see the warning — acceptable.)
        let unsupported: Vec<_> = w
            .iter()
            .filter(|w| w.category == WarningCategory::UnsupportedTerminal)
            .collect();
        // We can't assert empty because CI might have SSH_CONNECTION set,
        // but we CAN assert that if the warning fires, it has no fix.
        for warning in &unsupported {
            assert!(warning.fix.is_none());
        }
    }

    // -- Multi-warning coalescing ---------------------------------------------

    #[test]
    fn tmux_clipboard_and_dcs_both_warn() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 2);
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::Clipboard));
        assert!(categories.contains(&WarningCategory::DcsPassthrough));
    }

    #[test]
    fn tmux_control_mode_with_all_issues() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, true, false);
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::ControlMode));
        assert!(categories.contains(&WarningCategory::Clipboard));
        assert!(categories.contains(&WarningCategory::DcsPassthrough));
    }

    // -- Query unavailable: tmux server unreachable ---------------------------

    #[test]
    fn tmux_query_unavailable_produces_no_clipboard_warnings() {
        // When the tmux server is unreachable, all queries return `None`.
        // The diagnostics engine must not claim settings are disabled.
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::unavailable();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Unavailable tmux queries should not produce warnings"
        );
    }

    #[test]
    fn tmux_query_unavailable_in_control_mode_only_shows_control_warning() {
        // Even with all queries unavailable, the control-mode warning is
        // purely env-based and should still appear.
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::unavailable();
        let w = collect_startup_warnings(&ctx, &query, true, false);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ControlMode);
    }

    #[test]
    fn clipboard_query_unavailable_all_none_no_warnings() {
        // All probes return `None` → no warnings from clipboard diagnostics.
        let w = diagnose_clipboard_from_values(None, false, None, "~/.tmux.conf");
        assert!(w.is_empty());
    }

    #[test]
    fn clipboard_query_unavailable_passthrough_exists_but_value_none() {
        // `option_exists` returned true but `show_option` returned `None`:
        // the server knows the option but couldn't read it.  No warning.
        let w = diagnose_clipboard_from_values(Some("on"), true, None, "~/.tmux.conf");
        assert!(w.is_empty());
    }

    // -- tmux_option_exists: deterministic known-bad option --------------------

    #[test]
    fn tmux_option_exists_returns_false_for_nonexistent_option() {
        assert!(!tmux_option_exists("nonexistent-option-xyz"));
    }

    // =====================================================================
    // Extended diagnostic matrix (final hardening)
    // =====================================================================

    // -- Non-standard option values trigger warnings --------------------------

    #[test]
    fn clipboard_disabled_string_is_flagged() {
        // Some tmux configurations return "disabled" instead of "off".
        let w = diagnose_clipboard_from_values(Some("disabled"), true, Some("on"), "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
    }

    #[test]
    fn passthrough_disabled_string_is_flagged() {
        let w = diagnose_clipboard_from_values(Some("on"), true, Some("disabled"), "~/.tmux.conf");
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
    }

    // -- Zellij produces no tmux-specific warnings ----------------------------

    #[test]
    fn zellij_fullscreen_active_no_warnings() {
        // Zellij with fullscreen_active=true: no tmux warnings should appear.
        let ctx = zellij_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Zellij should not produce warnings even with fullscreen active"
        );
    }

    #[test]
    fn zellij_with_bad_tmux_options_still_quiet() {
        // Even if the fake tmux query reports bad options, Zellij context
        // should not produce tmux-specific warnings because it is not tmux-backed.
        let ctx = zellij_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Zellij should not inherit tmux option warnings"
        );
    }

    // -- Plain terminal with bad tmux options: no warnings --------------------

    #[test]
    fn plain_terminal_with_bad_tmux_options_still_quiet() {
        let ctx = plain_terminal_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Plain terminal should not show tmux option warnings"
        );
    }

    // -- Plain screen with bad tmux options: no warnings ----------------------

    #[test]
    fn plain_screen_with_bad_tmux_options_no_warnings() {
        let ctx = plain_screen_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert!(
            w.is_empty(),
            "Plain screen should not produce tmux-specific warnings"
        );
    }

    // -- WezTerm without the Kitty keyboard protocol ---------------------------

    fn wezterm_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::WezTerm,
            ..Default::default()
        }
    }

    #[test]
    fn wezterm_no_kkp_warns_with_config_fix() {
        let w = wezterm_kitty_keyboard_warning(&wezterm_ctx(), false, None)
            .expect("WezTerm without pushed KKP flags must warn");
        assert_eq!(w.category, WarningCategory::WezTermKittyKeyboardOff);
        assert_eq!(
            w.fix.as_deref(),
            Some("config.enable_kitty_keyboard = true")
        );
        assert_eq!(
            w.config_path.as_deref(),
            Some("~/.config/wezterm/wezterm.lua")
        );
        assert!(
            w.note.as_deref().is_some_and(|n| n.contains("\\")),
            "note must mention the backslash+Enter workaround"
        );
    }

    #[test]
    fn wezterm_with_kkp_active_no_warning() {
        // Probe passed and flags were pushed (e.g. enable_kitty_keyboard =
        // true already set) — Shift+Enter works, stay quiet.
        assert!(wezterm_kitty_keyboard_warning(&wezterm_ctx(), true, None).is_none());
    }

    #[test]
    fn non_wezterm_no_kkp_no_warning() {
        // Other brands without KKP are handled by their own paths (hint
        // substitution, brand skip lists) — this warning is WezTerm-only.
        let ctx = plain_terminal_ctx();
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, None).is_none());
    }

    #[test]
    fn wezterm_inside_tmux_with_skip_reason_no_warning() {
        // A kitty skip reason (old tmux) means KKP was never probed; the
        // wezterm.lua change alone wouldn't help, so don't advertise it.
        let ctx = TerminalContext {
            brand: TerminalName::WezTerm,
            multiplexer: MultiplexerKind::Tmux,
            ..Default::default()
        };
        assert!(ctx.kitty_skip_reason().is_some(), "fixture must skip KKP");
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, None).is_none());
    }

    #[test]
    fn wezterm_over_ssh_via_xtversion_warns() {
        // SSH shape: env brand Unknown (TERM_PROGRAM not forwarded), but
        // the terminal self-reported as WezTerm via XTVERSION. KKP is
        // skipped for Unknown brands, so flags were never pushed — warn.
        let ctx = TerminalContext {
            is_ssh: true,
            ..Default::default()
        };
        assert_eq!(ctx.brand, TerminalName::Unknown);
        let w = wezterm_kitty_keyboard_warning(&ctx, false, Some("WezTerm 20240203-110809"))
            .expect("XTVERSION-identified WezTerm over SSH must warn");
        assert_eq!(w.category, WarningCategory::WezTermKittyKeyboardOff);
        // The pager never negotiates KKP for Unknown brands, so the
        // wezterm.lua change cannot fix SSH sessions — the SSH variant
        // must NOT advertise it as the fix, and must lead with the
        // backslash+Enter workaround instead.
        assert!(
            w.fix.is_none(),
            "SSH variant must not advertise a config fix it can't honor"
        );
        assert!(w.message.contains("over SSH"));
        assert!(
            w.note
                .as_deref()
                .is_some_and(|n| n.starts_with("Type `\\`")),
            "SSH note must lead with the backslash+Enter workaround"
        );
    }

    #[test]
    fn wezterm_over_ssh_with_kkp_active_no_warning() {
        // Hypothetical future where KKP was negotiated despite the
        // Unknown brand — flags pushed wins over the XTVERSION report.
        let ctx = TerminalContext::default();
        assert!(wezterm_kitty_keyboard_warning(&ctx, true, Some("WezTerm 20240203")).is_none());
    }

    #[test]
    fn unknown_brand_non_wezterm_xtversion_no_warning() {
        // Self-report names a different terminal — stay quiet.
        let ctx = TerminalContext::default();
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, Some("kitty 0.35.2")).is_none());
        // tmux answering XTVERSION must not be mistaken for a brand.
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, Some("tmux 3.4")).is_none());
    }

    #[test]
    fn xtversion_wezterm_under_multiplexer_no_warning() {
        // Under tmux the XTVERSION reply describes tmux; even if a stale
        // "WezTerm" payload appeared, the multiplexer gate rejects it.
        let ctx = TerminalContext {
            multiplexer: MultiplexerKind::Tmux,
            ..Default::default()
        };
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, Some("WezTerm 2024")).is_none());
    }

    #[test]
    fn xtversion_wezterm_local_not_ssh_no_warning() {
        // Local WezTerm with TERM_PROGRAM stripped (brand falls back to
        // Unknown) can still answer XTVERSION with "WezTerm". The
        // XTVERSION path is SSH-only, so without is_ssh we must NOT emit
        // the "over SSH" copy here -- that would be wrong (it's local) and
        // would drop the actionable wezterm.lua fix. Env-based detection
        // covers the actionable local case; stay quiet otherwise.
        let ctx = TerminalContext {
            is_ssh: false,
            ..Default::default()
        };
        assert_eq!(ctx.brand, TerminalName::Unknown);
        assert!(wezterm_kitty_keyboard_warning(&ctx, false, Some("WezTerm 20240203")).is_none());
    }

    // -- assemble_startup_warnings: banner ordering ----------------------------

    fn clipboard_banner() -> crate::startup::StartupWarning {
        crate::startup::StartupWarning {
            severity: crate::startup::WarningSeverity::Warning,
            message: "Clipboard may be unreachable.".to_string(),
            action: None,
        }
    }

    #[test]
    fn wezterm_banner_goes_first() {
        // The welcome screen renders only the first warning; the WezTerm
        // banner (broken local input) must displace clipboard advisories.
        let w = wezterm_kitty_keyboard_warning(&wezterm_ctx(), false, None).unwrap();
        let out = assemble_startup_warnings(Some(&w), None, vec![clipboard_banner()]);
        assert_eq!(out.len(), 2);
        assert!(
            out[0].message.contains("WezTerm"),
            "WezTerm banner must be first, got: {}",
            out[0].message
        );
        assert!(out[1].message.contains("Clipboard"));
    }

    #[test]
    fn no_wezterm_warning_leaves_summarized_untouched() {
        let out = assemble_startup_warnings(None, None, vec![clipboard_banner()]);
        assert_eq!(out.len(), 1);
        assert!(out[0].message.contains("Clipboard"));
    }

    #[test]
    fn wayland_banner_surfaces_without_ssh_gate() {
        // `summarize_warnings` is SSH-gated, so the Wayland warning reaches the
        // welcome banner through the assemble bypass instead.
        let w = diagnose_wayland_data_control(true, false, true).unwrap();
        let out = assemble_startup_warnings(None, Some(&w), vec![clipboard_banner()]);
        assert_eq!(out.len(), 2);
        assert!(
            out[0].message.contains("focused"),
            "Wayland banner must be first, got: {}",
            out[0].message
        );
        assert!(out[1].message.contains("Clipboard"));
    }

    #[test]
    fn wezterm_banner_outranks_wayland_banner() {
        let wez = wezterm_kitty_keyboard_warning(&wezterm_ctx(), false, None).unwrap();
        let way = diagnose_wayland_data_control(true, false, true).unwrap();
        let out = assemble_startup_warnings(Some(&wez), Some(&way), vec![clipboard_banner()]);
        assert_eq!(out.len(), 3);
        assert!(out[0].message.contains("WezTerm"));
        assert!(out[1].message.contains("focused"));
        assert!(out[2].message.contains("Clipboard"));
    }

    // -- Warning ordering: clipboard before DCS --------------------------------

    #[test]
    fn warning_order_clipboard_then_dcs() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, WarningCategory::Clipboard);
        assert_eq!(w[1].category, WarningCategory::DcsPassthrough);
    }

    #[test]
    fn control_mode_warning_comes_before_clipboard() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, true, false);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].category, WarningCategory::ControlMode);
        assert_eq!(w[1].category, WarningCategory::Clipboard);
    }

    // -- Byobu-tmux DCS passthrough uses Byobu config path --------------------

    #[test]
    fn byobu_tmux_dcs_passthrough_uses_byobu_config_path() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
    }

    // -- Byobu-screen ignores control mode flag (no tmux to be in control mode)

    #[test]
    fn byobu_screen_ignores_control_mode_flag() {
        let ctx = byobu_screen_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        // Even with control_mode=true, Byobu-screen produces only ByobuScreen warning.
        let w = collect_startup_warnings(&ctx, &query, true, true);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::ByobuScreen);
    }

    // -- Byobu-tmux with all issues: complete coalesced set -------------------

    #[test]
    fn byobu_tmux_all_issues_fullscreen() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery {
            set_clipboard: Some("off".to_owned()),
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_startup_warnings(&ctx, &query, false, true);
        assert_eq!(w.len(), 2);
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::Clipboard));
        assert!(categories.contains(&WarningCategory::DcsPassthrough));
        // All should point to Byobu config.
        for warning in &w {
            assert_eq!(warning.config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
        }
    }

    // -- tmux extended-keys off warning ---------------------------------------

    fn extended_keys_ctx(base: TerminalContext, val: Option<&str>) -> TerminalContext {
        TerminalContext {
            tmux_version: Some("tmux 3.4".to_owned()),
            tmux_extended_keys: val.map(str::to_owned),
            ..base
        }
    }

    fn collect_extended_keys_warnings(ctx: &TerminalContext) -> Vec<TerminalWarning> {
        let query = FakeTmuxQuery::healthy_modern();
        collect_startup_warnings(ctx, &query, false, true)
            .into_iter()
            .filter(|w| w.category == WarningCategory::TmuxExtendedKeysOff)
            .collect()
    }

    fn assert_no_extended_keys_warning(val: Option<&str>) {
        let ctx = extended_keys_ctx(plain_tmux_ctx(), val);
        assert!(collect_extended_keys_warnings(&ctx).is_empty());
    }

    #[test]
    fn tmux_extended_keys_off_emits_warning() {
        let ctx = extended_keys_ctx(plain_tmux_ctx(), Some("off"));
        let warnings = collect_extended_keys_warnings(&ctx);
        let extended = warnings.first().expect("warning must fire");
        assert_eq!(
            extended.message,
            "tmux extended-keys is off -- modifier key combinations may not reach the pager"
        );
        assert_eq!(extended.fix.as_deref(), Some("set -g extended-keys on"));
        assert_eq!(extended.config_path.as_deref(), Some("~/.tmux.conf"));
        let note = extended.note.as_deref().expect("note must be present");
        assert!(note.contains("source-file") || note.contains("reattach"));
        assert!(note.contains("~/.tmux.conf"));
    }

    #[test]
    fn tmux_extended_keys_off_uses_byobu_config_path() {
        let ctx = extended_keys_ctx(byobu_tmux_ctx(), Some("off"));
        let warnings = collect_extended_keys_warnings(&ctx);
        let extended = warnings.first().expect("warning must fire");
        assert_eq!(extended.config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
        let note = extended.note.as_deref().expect("note must be present");
        assert!(note.contains("~/.byobu/.tmux.conf"));
        // `~/.byobu/.tmux.conf` does not contain `~/.tmux.conf` as a
        // contiguous substring, so this catches the hardcoded-path
        // bug.
        assert!(!note.contains("~/.tmux.conf"));
    }

    #[test]
    fn tmux_extended_keys_no_warning_for_non_off_values() {
        assert_no_extended_keys_warning(None);
        assert_no_extended_keys_warning(Some("on"));
        assert_no_extended_keys_warning(Some("always"));
    }

    // -- summarize_warnings allow-list -----------------------------------------

    #[test]
    fn summarize_warnings_surfaces_extended_keys_off() {
        let ctx = extended_keys_ctx(plain_tmux_ctx(), Some("off"));
        let warnings = collect_extended_keys_warnings(&ctx);
        let summary =
            summarize_warnings_inner(&warnings, true).expect("welcome banner must surface");
        assert_eq!(summary.severity, crate::startup::WarningSeverity::Warning);
        assert_eq!(summary.message, "Clipboard may be unreachable.");
        assert_eq!(
            summary.action.as_deref(),
            Some("See /terminal-setup for potential fixes.")
        );
    }

    #[test]
    fn summarize_warnings_surfaces_dcs_passthrough_off() {
        let warnings =
            diagnose_clipboard_from_values(Some("on"), true, Some("off"), "~/.tmux.conf");
        let summary =
            summarize_warnings_inner(&warnings, true).expect("welcome banner must surface");
        assert_eq!(summary.severity, crate::startup::WarningSeverity::Warning);
        assert_eq!(summary.message, "Clipboard may be unreachable.");
        assert_eq!(
            summary.action.as_deref(),
            Some("See /terminal-setup for potential fixes.")
        );
    }

    #[test]
    fn summarize_warnings_suppresses_other_categories() {
        // Clipboard warnings stay suppressed: the welcome-banner allow-list
        // is intentionally narrow until each category's false-positive rate
        // is characterized.
        let warnings = diagnose_clipboard_from_values(Some("off"), false, None, "~/.tmux.conf");
        assert!(
            !warnings.is_empty(),
            "fixture sanity: clipboard warnings must fire"
        );
        assert!(summarize_warnings_inner(&warnings, true).is_none());
    }

    #[test]
    fn summarize_warnings_picks_allowed_when_mixed_with_others() {
        // When clipboard (suppressed) AND DCS passthrough (allowed) both
        // fire, the banner still surfaces.
        let warnings =
            diagnose_clipboard_from_values(Some("off"), true, Some("off"), "~/.tmux.conf");
        assert!(
            warnings.len() >= 2,
            "fixture sanity: multiple warnings must fire"
        );
        let summary = summarize_warnings_inner(&warnings, true).expect("surfaces allowed warning");
        assert_eq!(summary.message, "Clipboard may be unreachable.");
    }

    #[test]
    fn summarize_warnings_empty_input_returns_none() {
        assert!(summarize_warnings_inner(&[], true).is_none());
    }

    #[test]
    fn summarize_warnings_suppressed_when_not_ssh() {
        // Even with a valid allow-listed warning, the banner is suppressed
        // when not running over SSH — locally these misconfigurations don't
        // actually break clipboard.
        let ctx = extended_keys_ctx(plain_tmux_ctx(), Some("off"));
        let warnings = collect_extended_keys_warnings(&ctx);
        assert!(
            !warnings.is_empty(),
            "fixture sanity: extended-keys warning must fire"
        );
        assert!(summarize_warnings_inner(&warnings, false).is_none());
    }

    // -- parse_tmux_show_option_output (pure helper) --------------------------

    #[test]
    fn parse_tmux_show_option_output_success_cases() {
        assert_eq!(
            parse_tmux_show_option_output(true, b"on\n"),
            Some("on".to_owned()),
        );
        assert_eq!(
            parse_tmux_show_option_output(true, b"off"),
            Some("off".to_owned()),
        );
        // Symmetric trim: leading whitespace is stripped too.
        assert_eq!(
            parse_tmux_show_option_output(true, b"  on"),
            Some("on".to_owned()),
        );
        assert_eq!(
            parse_tmux_show_option_output(true, b"\tor"),
            Some("or".to_owned()),
        );
    }

    #[test]
    fn parse_tmux_show_option_output_collapses_to_none() {
        assert_eq!(parse_tmux_show_option_output(true, b""), None);
        assert_eq!(parse_tmux_show_option_output(true, b"\n"), None);
        assert_eq!(parse_tmux_show_option_output(true, b"   "), None);
        // Subprocess failure ignores stdout entirely.
        assert_eq!(parse_tmux_show_option_output(false, b"on"), None);
    }

    // =====================================================================
    // collect_notification_warnings
    // =====================================================================

    use crate::notifications::NotificationCondition;
    use crate::notifications::protocol::NotificationProtocol;

    #[test]
    fn notification_bel_fallback_for_unknown_terminal() {
        let ctx = plain_terminal_ctx();
        let ctx = TerminalContext {
            brand: TerminalName::Unknown,
            ..ctx
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Always,
            &query,
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::NotificationProtocolFallback);
        assert!(w[0].message.contains("BEL"));
    }

    #[test]
    fn notification_bel_for_known_terminal_no_warning() {
        let ctx = TerminalContext {
            brand: TerminalName::Alacritty,
            ..Default::default()
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Always,
            &query,
        );
        assert!(w.is_empty(), "BEL on known terminal should not warn");
    }

    #[test]
    fn notification_osc_protocol_no_warning_without_tmux() {
        let ctx = plain_terminal_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc99,
            NotificationCondition::Always,
            &query,
        );
        assert!(w.is_empty());
    }

    #[test]
    fn notification_tmux_passthrough_off_warns_for_osc_protocol() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc9,
            NotificationCondition::Always,
            &query,
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].category, WarningCategory::DcsPassthrough);
        assert!(w[0].message.contains("notification"));
        assert_eq!(w[0].fix.as_deref(), Some("set -g allow-passthrough on"));
    }

    #[test]
    fn notification_tmux_passthrough_on_no_warning() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc9,
            NotificationCondition::Always,
            &query,
        );
        assert!(w.is_empty());
    }

    #[test]
    fn notification_tmux_passthrough_bel_protocol_no_warning() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Always,
            &query,
        );
        assert!(w.is_empty(), "BEL does not use DCS passthrough");
    }

    #[test]
    fn notification_focus_tracking_unavailable_unknown_terminal() {
        let ctx = TerminalContext {
            brand: TerminalName::Unknown,
            ..Default::default()
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Unfocused,
            &query,
        );
        let focus_warnings: Vec<_> = w
            .iter()
            .filter(|w| w.category == WarningCategory::FocusTrackingUnavailable)
            .collect();
        assert_eq!(focus_warnings.len(), 1);
        assert!(focus_warnings[0].message.contains("focus tracking"));
        assert!(focus_warnings[0].fix.as_deref().unwrap().contains("always"));
    }

    #[test]
    fn notification_focus_tracking_unavailable_apple_terminal() {
        let ctx = apple_terminal_ctx();
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Unfocused,
            &query,
        );
        let focus_warnings: Vec<_> = w
            .iter()
            .filter(|w| w.category == WarningCategory::FocusTrackingUnavailable)
            .collect();
        assert_eq!(focus_warnings.len(), 1);
    }

    #[test]
    fn notification_focus_tracking_no_warning_when_condition_always() {
        let ctx = TerminalContext {
            brand: TerminalName::Unknown,
            ..Default::default()
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Always,
            &query,
        );
        assert!(
            !w.iter()
                .any(|w| w.category == WarningCategory::FocusTrackingUnavailable),
            "condition=always does not need focus tracking"
        );
    }

    #[test]
    fn notification_focus_tracking_no_warning_for_supported_terminal() {
        let ctx = plain_terminal_ctx(); // Ghostty supports focus tracking
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc777,
            NotificationCondition::Unfocused,
            &query,
        );
        assert!(
            !w.iter()
                .any(|w| w.category == WarningCategory::FocusTrackingUnavailable),
            "Ghostty supports focus tracking"
        );
    }

    #[test]
    fn notification_multiple_warnings_can_coexist() {
        // Unknown terminal in tmux with passthrough off and unfocused condition
        let ctx = TerminalContext {
            brand: TerminalName::Unknown,
            multiplexer: MultiplexerKind::Tmux,
            tmux_meta: TmuxClientMeta {
                tmux_env: Some("/tmp/tmux-501/default,12345,0".to_owned()),
                tmux_pane: Some("%0".to_owned()),
            },
            ..Default::default()
        };
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        // BEL on unknown + unfocused → fallback + focus tracking warnings
        // (BEL doesn't use passthrough, so no passthrough warning)
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Bel,
            NotificationCondition::Unfocused,
            &query,
        );
        let categories: Vec<_> = w.iter().map(|w| w.category).collect();
        assert!(categories.contains(&WarningCategory::NotificationProtocolFallback));
        assert!(categories.contains(&WarningCategory::FocusTrackingUnavailable));
    }

    #[test]
    fn notification_tmux_passthrough_uses_byobu_config_path() {
        let ctx = byobu_tmux_ctx();
        let query = FakeTmuxQuery {
            allow_passthrough: Some("off".to_owned()),
            ..FakeTmuxQuery::healthy_modern()
        };
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc9,
            NotificationCondition::Always,
            &query,
        );
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
    }

    #[test]
    fn notification_query_unavailable_no_passthrough_warning() {
        let ctx = plain_tmux_ctx();
        let query = FakeTmuxQuery::unavailable();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::Osc99,
            NotificationCondition::Always,
            &query,
        );
        assert!(
            w.is_empty(),
            "Unavailable tmux queries should not produce notification warnings"
        );
    }

    #[test]
    fn notification_none_protocol_no_warnings() {
        let ctx = TerminalContext {
            brand: TerminalName::GrokDesktop,
            ..Default::default()
        };
        let query = FakeTmuxQuery::healthy_modern();
        let w = collect_notification_warnings(
            &ctx,
            NotificationProtocol::None,
            NotificationCondition::Unfocused,
            &query,
        );
        assert!(w.is_empty(), "None protocol should produce no warnings");
    }

    #[test]
    fn supports_focus_tracking_known_terminals() {
        assert!(supports_focus_tracking(TerminalName::Kitty));
        assert!(supports_focus_tracking(TerminalName::Ghostty));
        assert!(supports_focus_tracking(TerminalName::Iterm2));
        assert!(supports_focus_tracking(TerminalName::WezTerm));
        assert!(supports_focus_tracking(TerminalName::Alacritty));
        assert!(supports_focus_tracking(TerminalName::Vte));
        assert!(supports_focus_tracking(TerminalName::Terminator));
        assert!(supports_focus_tracking(TerminalName::WarpTerminal));
        assert!(supports_focus_tracking(TerminalName::VsCode));
        assert!(supports_focus_tracking(TerminalName::GrokDesktop));
        assert!(!supports_focus_tracking(TerminalName::AppleTerminal));
        assert!(!supports_focus_tracking(TerminalName::Unknown));
        assert!(!supports_focus_tracking(TerminalName::Otty));
    }

    // -- Color / theme rows + LimitedColorSupport warnings --------------------

    #[test]
    fn format_color_env_line_uses_canonical_level_label() {
        assert_eq!(
            format_color_env_line(ColorLevel::TrueColor),
            "  color        truecolor\n"
        );
        assert_eq!(
            format_color_env_line(ColorLevel::Ansi256),
            "  color        256\n"
        );
        assert_eq!(
            format_color_env_line(ColorLevel::Basic),
            "  color        basic\n"
        );
        assert_eq!(
            format_color_env_line(ColorLevel::None),
            "  color        none\n"
        );
    }

    #[test]
    fn format_themes_env_line_all_on_truecolor() {
        assert_eq!(
            format_themes_env_line(ColorLevel::TrueColor),
            "  themes       all\n"
        );
    }

    #[test]
    fn format_themes_env_line_lists_available_below_truecolor() {
        let n = ThemeKind::ALL
            .iter()
            .filter(|k| !k.requires_truecolor())
            .count();
        let total = ThemeKind::ALL.len();
        for level in [ColorLevel::Ansi256, ColorLevel::Basic, ColorLevel::None] {
            let line = format_themes_env_line(level);
            assert!(
                line.starts_with(&format!("  themes       {n}/{total}: ")),
                "level {level:?}: {line}"
            );
            assert!(line.contains("groknight") && line.contains("grokday"));
            assert!(!line.contains("tokyonight"));
        }
    }

    #[test]
    fn color_support_warning_none_on_truecolor() {
        assert!(
            color_support_warning(
                ColorLevel::TrueColor,
                TerminalName::Ghostty,
                false,
                "~/.tmux.conf"
            )
            .is_none()
        );
    }

    #[test]
    fn color_support_warning_no_color() {
        let w = color_support_warning(
            ColorLevel::None,
            TerminalName::Ghostty,
            false,
            "~/.tmux.conf",
        )
        .expect("warn");
        assert_eq!(w.category, WarningCategory::LimitedColorSupport);
        assert!(w.message.contains("NO_COLOR"));
        assert!(w.note.as_deref().is_some_and(|n| n.contains("NO_COLOR")));
    }

    #[test]
    fn color_support_warning_apple_terminal() {
        let w = color_support_warning(
            ColorLevel::Ansi256,
            TerminalName::AppleTerminal,
            false,
            "~/.tmux.conf",
        )
        .expect("warn");
        assert_eq!(w.category, WarningCategory::LimitedColorSupport);
        assert!(w.message.contains("Terminal.app"));
        assert!(w.fix.is_none());
        assert!(
            w.note
                .as_deref()
                .is_some_and(|n| n.contains("e.g. Ghostty"))
        );
    }

    #[test]
    fn color_support_warning_tmux() {
        let w = color_support_warning(
            ColorLevel::Ansi256,
            TerminalName::Unknown,
            true,
            "~/.byobu/.tmux.conf",
        )
        .expect("warn");
        assert_eq!(w.category, WarningCategory::LimitedColorSupport);
        assert_eq!(
            w.fix.as_deref(),
            Some("set -as terminal-features \",*:RGB\"")
        );
        assert_eq!(w.config_path.as_deref(), Some("~/.byobu/.tmux.conf"));
        let note = w.note.as_deref().expect("note");
        assert!(note.contains("COLORTERM=truecolor") && note.contains("tmux-256color"));
    }

    #[test]
    fn color_support_warning_colorterm() {
        let w = color_support_warning(
            ColorLevel::Basic,
            TerminalName::Unknown,
            false,
            "~/.tmux.conf",
        )
        .expect("warn");
        assert_eq!(w.fix.as_deref(), Some("export COLORTERM=truecolor"));
        assert!(w.config_path.is_none());
    }

    #[test]
    fn summarize_warnings_suppresses_limited_color_support() {
        let w = color_support_warning(
            ColorLevel::Ansi256,
            TerminalName::Unknown,
            false,
            "~/.tmux.conf",
        )
        .expect("fixture");
        assert!(summarize_warnings_inner(&[w], true).is_none());
    }
}
