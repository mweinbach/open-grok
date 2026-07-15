//! Clipboard providers for copy/paste.
//!
//! Re-exports [`ClipboardProvider`] and [`InternalClipboard`] from
//! `xai-ratatui-textarea`, and adds [`SystemClipboard`] backed by `arboard`.
//!
//! Multi-fire writes (native / tmux / OSC 52); user-facing success is [`trust`].

mod trust;

pub use xai_ratatui_textarea::{ClipboardProvider, InternalClipboard};

use std::sync::OnceLock;

use crate::terminal::{MultiplexerKind, TerminalContext};

/// Cached result of the remote-session check (env vars don't change at runtime).
fn is_remote() -> bool {
    static REMOTE: OnceLock<bool> = OnceLock::new();
    *REMOTE.get_or_init(xai_grok_shared::clipboard::is_remote_session)
}

/// Cached result of the container-without-display check.
fn is_container_no_display() -> bool {
    static CONTAINER: OnceLock<bool> = OnceLock::new();
    *CONTAINER.get_or_init(xai_grok_shared::clipboard::is_containerized_without_display)
}

/// Cached result of the "an upstream OSC 52 sink is capturing our output" check.
///
/// `grok wrap` runs a command inside a local PTY, scans its output for OSC 52
/// clipboard sequences, and writes their payload to the *real* (local) system
/// clipboard (see `xai-grok-pager`'s `pty_wrap` module). It advertises this to
/// the wrapped program via an environment variable so the
/// inner `grok` knows its OSC 52 writes are reliably intercepted and copied,
/// even when the inner terminal brand is misdetected (e.g. over SSH, where only
/// `TERM` propagates and Apple Terminal / unknown brands look OSC-52-incapable).
///
/// Two names are accepted: the canonical `GROK_OSC52_SINK` (inherited by local
/// children) and the `LC_`-prefixed `LC_GROK_OSC52_SINK`, which the default
/// OpenSSH client/server configs forward (`SendEnv LANG LC_*` /
/// `AcceptEnv LANG LC_*`) so the signal survives the hop into a remote `grok`.
pub fn osc52_sink_active() -> bool {
    static SINK: OnceLock<bool> = OnceLock::new();
    *SINK.get_or_init(|| {
        std::env::var_os("GROK_OSC52_SINK").is_some()
            || std::env::var_os("LC_GROK_OSC52_SINK").is_some()
    })
}

/// Cached clipboard route resolved at first use from the terminal context.
pub fn clipboard_route() -> &'static ClipboardRoute {
    static ROUTE: OnceLock<ClipboardRoute> = OnceLock::new();
    ROUTE.get_or_init(|| {
        let ctx = crate::terminal::terminal_context();
        resolve_clipboard_route(ctx)
    })
}

/// Wayland data-control availability as a display/telemetry label:
/// `"yes"`/`"no"` on Wayland sessions, `"n/a"` elsewhere.
pub fn wayland_data_control_label() -> &'static str {
    match crate::host::DisplayServer::current() {
        crate::host::DisplayServer::Wayland => {
            if xai_grok_shared::clipboard::wayland_data_control_supported() {
                "yes"
            } else {
                "no"
            }
        }
        _ => "n/a",
    }
}

/// Describes the clipboard write strategy for the current environment.
///
/// `Display` formats as `+`-separated active legs (e.g. "native+osc52").
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardRoute {
    /// Always attempt a native clipboard write.
    pub native: bool,
    /// Mirror into the tmux paste buffer via `tmux load-buffer`.
    pub tmux_buffer: bool,
    /// Emit an OSC 52 escape sequence toward the outer terminal.
    pub osc52: bool,
    /// Wrap OSC 52 in the tmux DCS passthrough envelope. True only when tmux is
    /// the IMMEDIATE terminal (tmux-backed and not inside an editor `:terminal`).
    /// Not a clipboard "leg" — excluded from `Display`.
    pub osc52_tmux_passthrough: bool,
}

impl std::fmt::Display for ClipboardRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for (flag, label) in [
            (self.native, "native"),
            (self.tmux_buffer, "tmux"),
            (self.osc52, "osc52"),
        ] {
            if flag {
                if !first {
                    f.write_str("+")?;
                }
                f.write_str(label)?;
                first = false;
            }
        }
        Ok(())
    }
}

/// Resolve the clipboard route from a terminal context.
///
/// Note: the `osc52` field depends on [`is_remote()`] and
/// [`is_container_no_display()`] which read ambient env vars / filesystem
/// markers (cached in `OnceLock`s). In tmux-backed environments `osc52` is
/// unconditionally `true` regardless of SSH/container state, so this only
/// matters for non-tmux contexts. Tests that cannot control SSH env vars
/// should skip asserting `osc52` for non-tmux cases.
pub fn resolve_clipboard_route(ctx: &TerminalContext) -> ClipboardRoute {
    let is_tmux = ctx.multiplexer == MultiplexerKind::Tmux;
    ClipboardRoute {
        native: true,
        tmux_buffer: is_tmux,
        // Linux: always emit OSC 52 as a safety net. This matches other
        // terminal agent CLIs which emit OSC 52 on every copy.
        // macOS/Windows: only in tmux/SSH/container contexts, or when an
        // upstream `grok wrap` sink is capturing our output and will forward
        // the sequence to the real clipboard.
        osc52: cfg!(target_os = "linux")
            || is_tmux
            || is_remote()
            || is_container_no_display()
            || osc52_sink_active(),
        // Editor :terminal's immediate emulator is libvterm, not tmux — don't wrap there.
        osc52_tmux_passthrough: is_tmux && ctx.embedded_editor.is_none(),
    }
}

/// Write text into the tmux paste buffer via `tmux load-buffer -`.
///
/// Returns `true` only when spawn and child exit succeed.
fn write_tmux_buffer(text: &str) -> bool {
    use std::process::{Command, Stdio};

    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        // Spooled stdin (not a pipe): a wedged tmux server that stops draining
        // stdin would block the UI thread once the payload exceeds the pipe
        // buffer, and the bounded wait below needs stdin already closed.
        let stdin = xai_grok_shared::clipboard::spool_for_stdin(text.as_bytes())?;
        let mut cmd = Command::new("tmux");
        cmd.args(["load-buffer", "-"])
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        xai_tty_utils::detach_std_command(&mut cmd);
        let mut child = cmd.spawn()?;
        // Bounded wait: a wedged tmux server must not freeze the UI thread.
        let status = xai_grok_shared::clipboard::wait_with_deadline(
            &mut child,
            std::time::Duration::from_secs(2),
        )?;
        if !status.success() {
            return Err(format!("tmux load-buffer exited with {status}").into());
        }
        Ok(())
    })();
    if let Err(e) = &result {
        tracing::debug!("tmux load-buffer failed (best-effort): {e}");
    }
    result.is_ok()
}

/// System clipboard provider.
///
/// Delegates to [`xai_grok_shared::clipboard`] which uses
/// `pbcopy`/`pbpaste` on macOS (to avoid AppKit GPU overhead) and `arboard`
/// on other platforms.
///
/// In tmux-backed environments, clipboard writes follow the full three-leg
/// contract: native clipboard + tmux buffer + OSC 52.
#[derive(Debug)]
pub struct SystemClipboard;

impl SystemClipboard {
    /// Full write route; `true` when a trusted leg succeeded ([`trust`]).
    pub fn try_set(text: &str) -> bool {
        let legs = clipboard_write_with_route(text, clipboard_route());
        toast_for_legs(&legs, text).reported_success()
    }
}

impl ClipboardProvider for SystemClipboard {
    fn get(&mut self) -> Option<String> {
        xai_grok_shared::clipboard::get_text().ok().flatten()
    }

    fn set(&mut self, text: &str) {
        let _ = clipboard_write_with_route(text, clipboard_route());
    }
}

/// Per-leg outcome of a routed clipboard write (for telemetry + trust).
pub(crate) struct ClipboardWriteLegs {
    /// Whether the route enabled the native leg.
    pub(crate) route_native: bool,
    route_label: String,
    cli_tools_tried: String,
    pub(crate) cli_ok_tools: String,
    pub(crate) wl_copy_ok: bool,
    pub(crate) cli_ok: bool,
    pub(crate) arboard_ok: bool,
    /// Wayland data-control was available for the native leg (environment
    /// probe); the arboard write is focus-free and authoritative only when
    /// `arboard_ok` also holds (see `trust::trusted_native`).
    pub(crate) data_control: bool,
    pub(crate) tmux_ok: bool,
    pub(crate) osc52_ok: bool,
}

fn clipboard_write_with_route(text: &str, route: &ClipboardRoute) -> ClipboardWriteLegs {
    let mut legs = ClipboardWriteLegs {
        route_native: route.native,
        route_label: route.to_string(),
        cli_tools_tried: String::new(),
        cli_ok_tools: String::new(),
        wl_copy_ok: false,
        cli_ok: false,
        arboard_ok: false,
        data_control: false,
        tmux_ok: false,
        osc52_ok: false,
    };

    if route.native {
        let outcome = xai_grok_shared::clipboard::set_text_with_outcome(text);
        legs.cli_ok = outcome.cli_ok;
        legs.arboard_ok = outcome.arboard_ok;
        legs.data_control = outcome.data_control;
        legs.wl_copy_ok = outcome.cli_ok_tools.contains(&"wl-copy");
        legs.cli_tools_tried = outcome.cli_tools_tried.join("+");
        legs.cli_ok_tools = outcome.cli_ok_tools.join("+");
        if !outcome.any_ok {
            tracing::debug!("native clipboard write failed on all backends");
        }
    }

    if route.tmux_buffer {
        legs.tmux_ok = write_tmux_buffer(text);
    }

    if route.osc52 {
        match xai_grok_shared::clipboard::set_text_osc52(text, route.osc52_tmux_passthrough) {
            Ok(()) => legs.osc52_ok = true,
            Err(e) => {
                tracing::debug!("OSC 52 clipboard write failed (best-effort): {e}");
            }
        }
    }

    legs
}

/// Result of a clipboard write with toast info for the caller to display.
pub struct CopyResult {
    /// User-facing toast message.
    pub message: &'static str,
    /// Toast duration in ticks (30fps: 30 = ~1s, 120 = ~4s).
    pub ticks: u8,
    pub success: bool,
}

/// Kind of clipboard copy toast (success route or failure).
///
/// Telemetry labels come from `IntoStaticStr` (`snake_case`); user-facing copy
/// lives in [`ClipboardToastKind::message`] (intentionally different).
#[derive(Debug, Clone, Copy, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum ClipboardToastKind {
    /// Plain successful copy (native clipboard).
    Copied,
    /// Successful copy mirrored into the tmux paste buffer.
    CopiedTmux,
    /// Successful copy via OSC 52 in a container without display.
    CopiedOscContainer,
    /// Successful copy via OSC 52 over SSH/remote.
    CopiedOscRemote,
    /// VS Code over SSH/remote + non-ASCII: OSC 52 may mojibake; prefer Shift+select.
    VsCodeSshNonAscii,
    /// All trusted clipboard backends failed.
    Failed,
}

impl ClipboardToastKind {
    /// User-facing toast message for this kind.
    fn message(self) -> &'static str {
        match self {
            Self::Copied => "Copied!",
            Self::CopiedTmux => "Copied to tmux buffer, paste with prefix + ]",
            Self::CopiedOscContainer => {
                "Copied via OSC 52 (container). If paste fails, hold Shift (or Fn) and drag to select & copy natively."
            }
            Self::CopiedOscRemote => {
                "Copied via OSC 52. If paste fails, hold Shift (or Fn) and drag to select & copy natively."
            }
            Self::VsCodeSshNonAscii => {
                "Copied. In case VSCode via SSH garbles non-ASCII text, use native copy (shift+select)."
            }
            Self::Failed => "Copy failed. Try /minimal for terminal native rendering",
        }
    }

    /// Toast duration in ticks (30fps: 30 = ~1s, 120 = ~4s).
    fn ticks(self) -> u8 {
        match self {
            Self::Copied => 30,
            Self::CopiedTmux
            | Self::CopiedOscContainer
            | Self::CopiedOscRemote
            | Self::VsCodeSshNonAscii
            | Self::Failed => 120,
        }
    }

    pub(crate) fn reported_success(self) -> bool {
        !matches!(self, Self::Failed)
    }

    fn to_result(self) -> CopyResult {
        CopyResult {
            message: self.message(),
            ticks: self.ticks(),
            success: self.reported_success(),
        }
    }
}

fn toast_for_legs(legs: &ClipboardWriteLegs, text: &str) -> ClipboardToastKind {
    trust::resolve_copy_toast(
        legs,
        text,
        crate::terminal::terminal_context().brand,
        crate::host::HostOs::current(),
        crate::host::DisplayServer::current(),
        is_remote(),
        is_container_no_display(),
        osc52_sink_active(),
    )
}

/// Write text and return a toast; emits `grok-shell-clipboard_copy` when enabled.
pub fn copy_text(text: &str) -> CopyResult {
    let started = std::time::Instant::now();
    let route = clipboard_route();
    let legs = clipboard_write_with_route(text, route);
    let kind = toast_for_legs(&legs, text);
    let success = kind.reported_success();
    if !success {
        tracing::warn!(
            len = text.len(),
            display_server = %crate::host::DisplayServer::current(),
            "clipboard write failed on all trusted backends"
        );
    }
    let result = kind.to_result();
    let toast_kind: &'static str = kind.into();
    log_clipboard_copy_event(text, route, &legs, success, toast_kind, started);
    result
}

fn log_clipboard_copy_event(
    text: &str,
    route: &ClipboardRoute,
    legs: &ClipboardWriteLegs,
    reported_success: bool,
    toast_kind: &'static str,
    started: std::time::Instant,
) {
    if !xai_grok_telemetry::client::is_enabled() {
        return;
    }
    xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::ClipboardCopy {
        terminal: crate::terminal::terminal_context().telemetry_snapshot(),
        source: "copy_text",
        text_len: text.len() as u64,
        route_native: route.native,
        route_tmux: route.tmux_buffer,
        route_osc52: route.osc52,
        route_label: legs.route_label.clone(),
        cli_tools_tried: legs.cli_tools_tried.clone(),
        cli_ok_tools: legs.cli_ok_tools.clone(),
        cli_ok: legs.cli_ok,
        arboard_ok: legs.arboard_ok,
        data_control: legs.data_control,
        tmux_ok: legs.tmux_ok,
        osc52_ok: legs.osc52_ok,
        reported_success,
        toast_kind,
        duration_ms: started.elapsed().as_millis() as u64,
    });
}

/// Return the parenthetical stats suffix used in clipboard success messages.
/// Format: " (N chars, M lines)" with proper pluralization.
/// Extracted to eliminate the exact duplication between assistant copy and
/// full-conversation export (and any future clipboard users).
pub fn clipboard_stats_suffix(text: &str) -> String {
    let chars = text.len();
    let lines = text.lines().count();
    format!(
        " ({} chars, {} {})",
        chars,
        lines,
        if lines == 1 { "line" } else { "lines" }
    )
}

/// CLIPBOARD text read failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardTextReadError;

/// Read CLIPBOARD text while distinguishing emptiness from failure.
pub fn system_clipboard_read_text() -> Result<Option<String>, ClipboardTextReadError> {
    #[cfg(test)]
    if let Some(text) = test_support::hook_text_result() {
        return text;
    }
    xai_grok_shared::clipboard::get_text().map_err(|error| {
        tracing::debug!("clipboard text read failed: {error}");
        ClipboardTextReadError
    })
}

/// Read CLIPBOARD text for simple callers that treat failure as no text.
pub fn system_clipboard_get() -> Option<String> {
    system_clipboard_read_text().ok().flatten()
}

/// Read X11 PRIMARY text for an unmodified Linux middle-button press.
#[cfg(target_os = "linux")]
pub fn system_primary_selection_get() -> Option<String> {
    #[cfg(test)]
    if let Some(available) = test_support::hook_x11_primary_available() {
        if !available {
            return None;
        }
        return test_support::hook_primary_text()
            .flatten()
            .filter(|text| !text.is_empty());
    }

    if !xai_grok_shared::clipboard::x11_display_env_present() {
        return None;
    }
    xai_grok_shared::clipboard::get_primary_text()
        .ok()
        .flatten()
        .filter(|text| !text.is_empty())
}

#[cfg(any(test, target_os = "linux"))]
fn is_native_x11(display_server: crate::host::DisplayServer) -> bool {
    display_server == crate::host::DisplayServer::X11
}

/// Runtime X11 gate for empty Ctrl+V guidance.
pub fn x11_primary_guidance_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        is_native_x11(crate::host::DisplayServer::current())
            && xai_grok_shared::clipboard::x11_display_env_present()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// Non-empty after trim.
pub fn clipboard_text_is_pasteable(text: Option<&str>) -> bool {
    text.is_some_and(|t| !t.trim().is_empty())
}

/// Telemetry when a paste key was pressed but the host clipboard had nothing
/// pasteable. Behavior is unchanged — callers still consume the key.
/// Emits structured logs and a product analytics event when telemetry is enabled.
pub fn log_paste_key_empty_host_clipboard(surface: &str) {
    let terminal = crate::terminal::terminal_context().telemetry_snapshot();
    // Structured warn for the product telemetry pipeline.
    tracing::warn!(
        terminal.brand = %terminal.brand,
        terminal.multiplexer = %terminal.multiplexer,
        terminal.is_ssh = terminal.is_ssh,
        terminal.term_var = %terminal.term_var,
        terminal.xtversion = %terminal.xtversion,
        terminal.clipboard_route = %terminal.clipboard_route,
        terminal.clipboard_native_tool = %terminal.clipboard_native_tool,
        terminal.host_os = %terminal.host_os,
        terminal.display_server = %terminal.display_server,
        paste.surface = %surface,
        "paste_key_empty_host_clipboard"
    );
    if !xai_grok_telemetry::client::is_enabled() {
        return;
    }
    xai_grok_telemetry::session_ctx::log_event(
        xai_grok_telemetry::events::PasteKeyEmptyHostClipboard {
            terminal,
            surface: surface.to_owned(),
        },
    );
}

fn lone_http_url_trimmed(t: &str) -> bool {
    !t.is_empty() && !t.contains('\n') && (t.starts_with("http://") || t.starts_with("https://"))
}

/// True when a lone `http(s)://` URL with no newlines (normal link paste).
pub fn looks_like_lone_http_url(text: &str) -> bool {
    lone_http_url_trimmed(text.trim())
}

/// Non-empty plain text where the `«class furl»` probe is unnecessary (UTF-8 already
/// carries paths or prose). Does **not** suppress `get_image` — see
/// [`clipboard_attachment_probe_needed`].
pub(crate) fn plain_text_skips_furl_probe(trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    if lone_http_url_trimmed(trimmed) {
        return true;
    }
    if trimmed.contains("file://") {
        return true;
    }
    if trimmed.len() >= 4096 {
        return true;
    }
    if trimmed.lines().count() > 4 && !trimmed.contains("://") {
        return true;
    }
    !trimmed.contains("://")
}

/// Whether `get_image` / `get_file_urls` should run given [`system_clipboard_get`] output.
///
/// Returns false only for a lone `http(s)://` URL (normal link paste). Prose, code,
/// and `file://` text still allow an image probe so "Copy Image" + caption keeps working.
pub fn clipboard_attachment_probe_needed(clipboard_text: Option<&str>) -> bool {
    match clipboard_text {
        None => true,
        Some(text) => {
            let t = text.trim();
            t.is_empty() || !lone_http_url_trimmed(t)
        }
    }
}

/// Whether bracketed-paste payload should probe the system clipboard (macOS/Windows).
///
/// Matches the historical rule (empty, short ≤4 lines, or `://`) except lone `https://`
/// link pastes, which skip the ~100–200 ms macOS `osascript` cost.
pub fn paste_payload_needs_clipboard_attachment_probe(payload: &str) -> bool {
    if payload.is_empty() {
        return true;
    }
    let t = payload.trim();
    if lone_http_url_trimmed(t) {
        return false;
    }
    if t.len() >= 4096 {
        return false;
    }
    let line_count = t.lines().count().max(1);
    line_count <= 4 || t.contains("://")
}

/// Whether a bracketed-paste payload plausibly came from the system clipboard.
///
/// Terminals rewrite newlines on paste (`\n` → `\r`); a non-matching payload
/// is not a clipboard paste (e.g. Otty IME commits as bracketed paste,
/// or a tmux paste-buffer that diverged from the OS clipboard).
///
/// Reads the clipboard text (a `pbpaste` subprocess on macOS) — call only off
/// the event loop.
pub fn bracketed_payload_came_from_clipboard(payload: &str) -> bool {
    bracketed_payload_came_from_clipboard_result(payload).unwrap_or(false)
}

/// Typed version of [`bracketed_payload_came_from_clipboard`].
pub fn bracketed_payload_came_from_clipboard_result(
    payload: &str,
) -> Result<bool, ClipboardTextReadError> {
    system_clipboard_read_text()
        .map(|text| bracketed_payload_matches_clipboard_text(payload, text.as_deref()))
}

/// Pure comparison behind [`bracketed_payload_came_from_clipboard`].
/// `None` = no UTF-8 string on the pasteboard (image-only pasteboards).
pub fn bracketed_payload_matches_clipboard_text(
    payload: &str,
    clipboard_text: Option<&str>,
) -> bool {
    // Terminals rewrite pasted newlines as `\r` (Windows clipboards carry
    // `\r\n`); some copy sources append a trailing newline the paste omits.
    fn normalized(s: &str) -> String {
        s.replace("\r\n", "\n")
            .replace('\r', "\n")
            .trim_end()
            .to_owned()
    }
    match clipboard_text {
        None => payload.trim().is_empty(),
        Some(text) => normalized(payload) == normalized(text),
    }
}

/// Routing plan for attachment pasteboard probes (testable without subprocesses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentProbeRoute {
    /// Lone `http(s)://` link paste — no `osascript`.
    Skip,
    /// Empty pasteboard UTF-8: `furl` first, then image if needed.
    FileUrlsThenImage,
    /// Non-empty UTF-8: image only (caption / Copy Image).
    ImageOnly,
}

/// Decide which pasteboard probes to run for a Cmd+V clipboard read.
pub fn attachment_probe_route(clipboard_text: Option<&str>) -> AttachmentProbeRoute {
    let trimmed = clipboard_text.map(str::trim).unwrap_or("");
    if !trimmed.is_empty() && lone_http_url_trimmed(trimmed) {
        return AttachmentProbeRoute::Skip;
    }
    if trimmed.is_empty() || !plain_text_skips_furl_probe(trimmed) {
        AttachmentProbeRoute::FileUrlsThenImage
    } else {
        AttachmentProbeRoute::ImageOnly
    }
}

/// Whether the heavy `osascript` attachment probe should run for `route`, given
/// the sub-millisecond native pasteboard snapshot.
///
/// `ImageOnly` (non-empty non-URL text) only ever looks for raster bytes, so it
/// is safe to skip when the snapshot is available and reports none. `Skip` never
/// probes; `FileUrlsThenImage` (empty text — the Finder file-URL case) is never
/// gated because the snapshot cannot vouch for `public.file-url` presence.
fn should_run_attachment_probe(
    route: AttachmentProbeRoute,
    snapshot_supported: bool,
    snapshot_available: bool,
    snapshot_has_image: bool,
) -> bool {
    match route {
        AttachmentProbeRoute::Skip => false,
        AttachmentProbeRoute::FileUrlsThenImage => true,
        AttachmentProbeRoute::ImageOnly => {
            // Skip only when the snapshot is available and shows no raster (unavailable can't rule one out).
            let snapshot_rules_out_raster =
                snapshot_supported && snapshot_available && !snapshot_has_image;
            !snapshot_rules_out_raster
        }
    }
}

/// Gate + TOCTOU baseline for a deferred attachment probe: `None` = do not
/// probe; `Some(change_count)` = probe, carrying the pasteboard `changeCount`
/// this gate's OWN snapshot read observed. Enqueue sites thread that baseline
/// into the off-thread probe's staleness check instead of taking a second
/// native read that could land after a clipboard change.
///
/// Cheap (native snapshot only, no subprocess) so paste handlers can call it on
/// the event loop to decide whether to DEFER the heavy probe to a background
/// task instead of blocking inline.
pub fn attachment_probe_gate(clipboard_text: Option<&str>) -> Option<Option<u64>> {
    // One snapshot read; a None change_count is unavailable, so the gate must not
    // rule out a raster (propagates the availability fix into the defer gate).
    let (snapshot_change_count, snapshot_has_image) = clipboard_image_snapshot();
    should_run_attachment_probe(
        attachment_probe_route(clipboard_text),
        clipboard_image_probe_supported(),
        snapshot_change_count.is_some(),
        snapshot_has_image,
    )
    .then_some(snapshot_change_count)
}

/// Whether [`system_clipboard_probe_attachments`] would actually run the
/// osascript probe for `clipboard_text` (thin bool view of
/// [`attachment_probe_gate`]).
pub fn attachment_probe_would_run(clipboard_text: Option<&str>) -> bool {
    attachment_probe_gate(clipboard_text).is_some()
}

/// Attachment probing failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClipboardProbeError;

/// Probe image / file-url pasteboard types per [`attachment_probe_route`].
pub fn system_clipboard_probe_attachments(
    clipboard_text: Option<&str>,
) -> Result<(Option<ImageData>, Option<String>), ClipboardProbeError> {
    // Native snapshot (sub-ms, no subprocess) skips the osascript image probe
    // for a text paste when the pasteboard holds no raster (single gate site).
    if !attachment_probe_would_run(clipboard_text) {
        return Ok((None, None));
    }
    #[cfg(test)]
    if let Some(canned) = test_support::hook_attachments() {
        return canned;
    }
    match attachment_probe_route(clipboard_text) {
        AttachmentProbeRoute::Skip => Ok((None, None)),
        AttachmentProbeRoute::FileUrlsThenImage => {
            let att = system_clipboard_get_attachments()?;
            if att.file_urls.is_some() {
                return Ok((None, att.file_urls));
            }
            Ok((att.image, None))
        }
        AttachmentProbeRoute::ImageOnly => {
            system_clipboard_get_image_result().map(|image| (image, None))
        }
    }
}

/// Emit a `clipboard_image_paste` telemetry event for one clipboard read.
///
/// `outcome`: "image" | "file_urls" | "empty" | "error". No-op (and no
/// terminal-context detection) when telemetry is disabled.
fn log_clipboard_paste_event(
    probe: &str,
    outcome: &str,
    image_mime: &str,
    started: std::time::Instant,
) {
    if !xai_grok_telemetry::client::is_enabled() {
        return;
    }
    xai_grok_telemetry::session_ctx::log_event(xai_grok_telemetry::events::ClipboardImagePaste {
        terminal: crate::terminal::terminal_context().telemetry_snapshot(),
        probe: probe.to_owned(),
        outcome: outcome.to_owned(),
        image_mime: image_mime.to_owned(),
        duration_ms: started.elapsed().as_millis() as u64,
    });
}

/// Read file URLs and image from the system clipboard in one macOS `osascript`.
///
/// On non-macOS this composes separate arboard reads.
fn system_clipboard_get_attachments() -> Result<AttachmentsProbeResult, ClipboardProbeError> {
    let started = std::time::Instant::now();
    match xai_grok_shared::clipboard::get_attachments() {
        Ok(att) => {
            let (outcome, mime) = match (&att.image, &att.file_urls) {
                (Some(img), _) => ("image", img.mime_type.as_str()),
                (None, Some(_)) => ("file_urls", ""),
                (None, None) => ("empty", ""),
            };
            log_clipboard_paste_event("attachments", outcome, mime, started);
            Ok(AttachmentsProbeResult {
                file_urls: att.file_urls,
                image: att.image,
            })
        }
        Err(e) => {
            tracing::debug!("clipboard attachments read failed: {e}");
            log_clipboard_paste_event("attachments", "error", "", started);
            Err(ClipboardProbeError)
        }
    }
}

/// Result of [`system_clipboard_get_attachments`].
#[derive(Debug)]
struct AttachmentsProbeResult {
    file_urls: Option<String>,
    image: Option<ImageData>,
}

/// Re-export [`ImageData`] so pager code does not import the shell directly.
pub use xai_grok_shared::clipboard::ImageData;

/// One pasteboard snapshot `(change_count, has_pasteable_image)` read in a
/// single native pass (macOS native, sub-millisecond, no data read). `(None,
/// false)` off-macOS or when AppKit cannot be loaded.
pub fn clipboard_image_snapshot() -> (Option<u64>, bool) {
    #[cfg(test)]
    if let Some(snapshot) = test_support::hook_image_snapshot() {
        return snapshot;
    }
    xai_grok_shared::clipboard::clipboard_image_snapshot()
}

/// Cheap pasteboard `changeCount` read (one native message, no type scan, no
/// data read). The changeCount-first hot path of the focus-driven
/// clipboard-image tip: a delta here is what gates the heavier
/// [`clipboard_image_snapshot`] classification. `None` off-macOS.
pub fn clipboard_change_count() -> Option<u64> {
    // Seam consistency: a hooked snapshot's change_count is the changeCount.
    #[cfg(test)]
    if let Some((change_count, _)) = test_support::hook_image_snapshot() {
        return change_count;
    }
    xai_grok_shared::clipboard::clipboard_change_count()
}

/// Whether the fast image probe exists on this platform. Gates the
/// focus-driven clipboard-image tip so non-macOS never probes.
pub fn clipboard_image_probe_supported() -> bool {
    #[cfg(test)]
    if let Some(supported) = test_support::hook_image_probe_supported() {
        return supported;
    }
    xai_grok_shared::clipboard::clipboard_image_probe_supported()
}

/// Prime the macOS AppKit `dlopen` ONCE on a detached background thread, so the
/// first synchronous probe (~1s after a focus-gain) is just the cheap metadata
/// read and never stalls a frame on the one-time framework load. The probe
/// itself stays synchronous — this is a single one-time prime, not a per-probe
/// async layer. No-op off-macOS and after the first call.
pub fn prewarm_image_probe() {
    use std::sync::Once;
    static WARMED: Once = Once::new();
    if !clipboard_image_probe_supported() {
        return;
    }
    WARMED.call_once(|| {
        std::thread::spawn(xai_grok_shared::clipboard::clipboard_prewarm);
    });
}

/// Read an image while preserving an empty-versus-error distinction.
fn system_clipboard_get_image_result() -> Result<Option<ImageData>, ClipboardProbeError> {
    let started = std::time::Instant::now();
    match xai_grok_shared::clipboard::get_image() {
        Ok(img) => {
            let (outcome, mime) = match &img {
                Some(img) => ("image", img.mime_type.as_str()),
                None => ("empty", ""),
            };
            log_clipboard_paste_event("image", outcome, mime, started);
            Ok(img)
        }
        Err(e) => {
            tracing::debug!("clipboard image read failed: {e}");
            log_clipboard_paste_event("image", "error", "", started);
            Err(ClipboardProbeError)
        }
    }
}

/// Read an image from the system clipboard; direct callers treat errors as empty.
pub fn system_clipboard_get_image() -> Option<ImageData> {
    system_clipboard_get_image_result().unwrap_or(None)
}

// ===========================================================================
// Test support
// ===========================================================================

/// Injectable clipboard reads for driving the paste handlers in tests without
/// spawning `pbpaste` / `osascript`.
///
/// Callers install a [`ClipboardProbeHook`] via [`set_clipboard_probe_hook`];
/// [`system_clipboard_get`], [`system_clipboard_read_text`],
/// `system_primary_selection_get`,
/// [`system_clipboard_probe_attachments`], [`clipboard_image_snapshot`],
/// [`clipboard_change_count`], and [`clipboard_image_probe_supported`] then
/// return the canned values instead of reading the real pasteboard. PRIMARY
/// reads bump `primary_selection_read_call_count`; every unified-probe call
/// that clears the snapshot gate bumps a counter read back with
/// [`clipboard_probe_call_count`] (`count == 0` proves a gated-out probe,
/// `count == 1` proves the single unified read). The state is thread-local so
/// parallel tests stay isolated, and the real reads run when no hook is set.
///
/// Thread-locality caveat: the hook does NOT propagate to `spawn_blocking`
/// (the off-thread probe reads the REAL pasteboard there), so tests exercise
/// deferral by asserting the enqueued effect and then driving
/// `complete_clipboard_attachment_paste` directly with a canned outcome.
#[cfg(test)]
pub mod test_support {
    use super::{ClipboardProbeError, ClipboardTextReadError, ImageData};
    use std::cell::{Cell, RefCell};

    pub(super) type AttachmentProbeResult =
        Result<(Option<ImageData>, Option<String>), ClipboardProbeError>;

    /// Canned pasteboard contents returned while a hook is installed.
    ///
    /// Snapshot vocabulary (`(changeCount, has_image)`): "no raster" =
    /// `(Some(_), false)`, "has raster" = `(Some(_), true)`, "unavailable"
    /// (AppKit load failure) = `(None, _)`. Prefer the named constructors
    /// below over spelling the tuples out.
    #[derive(Clone, Default)]
    pub struct ClipboardProbeHook {
        /// `pbpaste` text for the Ctrl/Cmd+V key-chord path.
        pub text: Option<String>,
        /// Make the CLIPBOARD text seam return a typed backend failure.
        pub text_read_failed: bool,
        /// X11 PRIMARY text for the unmodified Linux middle-button path.
        pub primary_text: Option<String>,
        /// Explicit X11 availability guard for `primary_text`.
        pub x11_primary_available: bool,
        /// Raster image returned by the unified attachment probe.
        pub image: Option<ImageData>,
        /// File URL(s) returned by the unified attachment probe.
        pub file_urls: Option<String>,
        /// Make the unified attachment seam return a typed backend failure.
        pub attachment_probe_failed: bool,
        /// `(changeCount, has_image)` snapshot; unset defaults to available with
        /// raster iff a canned `image` is set, so text hooks skip and image hooks probe.
        pub snapshot: Option<(Option<u64>, bool)>,
        /// `clipboard_image_probe_supported()`; real platform value when `None`.
        pub snapshot_supported: Option<bool>,
    }

    impl ClipboardProbeHook {
        /// Canned TEXT clipboard whose snapshot is available with no raster
        /// (the gate skips the probe; the text inserts synchronously).
        pub fn no_raster(text: Option<&str>) -> Self {
            Self {
                text: text.map(str::to_owned),
                snapshot: Some((Some(1), false)),
                snapshot_supported: Some(true),
                ..Default::default()
            }
        }

        /// Canned raster clipboard: the snapshot reports an image present so
        /// the gate defers/runs the probe, which returns `image`.
        pub fn with_raster(image: Option<ImageData>) -> Self {
            Self {
                image,
                snapshot: Some((Some(1), true)),
                snapshot_supported: Some(true),
                ..Default::default()
            }
        }

        /// Snapshot unavailable (AppKit load failure): the gate cannot rule
        /// out a raster and must still probe.
        pub fn snapshot_unavailable() -> Self {
            Self {
                snapshot: Some((None, false)),
                snapshot_supported: Some(true),
                ..Default::default()
            }
        }
    }

    thread_local! {
        static HOOK: RefCell<Option<ClipboardProbeHook>> = const { RefCell::new(None) };
        static PROBE_CALLS: Cell<u32> = const { Cell::new(0) };
        static PRIMARY_READS: Cell<u32> = const { Cell::new(0) };
    }

    /// Install a canned clipboard hook and reset the probe counter.
    pub fn set_clipboard_probe_hook(hook: ClipboardProbeHook) {
        HOOK.with(|h| *h.borrow_mut() = Some(hook));
        PROBE_CALLS.with(|c| c.set(0));
        PRIMARY_READS.with(|c| c.set(0));
    }

    /// Remove the canned clipboard hook and reset the probe counter.
    pub fn clear_clipboard_probe_hook() {
        HOOK.with(|h| *h.borrow_mut() = None);
        PROBE_CALLS.with(|c| c.set(0));
        PRIMARY_READS.with(|c| c.set(0));
    }

    /// Unified-probe invocations since the hook was installed.
    pub fn clipboard_probe_call_count() -> u32 {
        PROBE_CALLS.with(|c| c.get())
    }

    /// X11 PRIMARY value reads since the hook was installed.
    pub fn primary_selection_read_call_count() -> u32 {
        PRIMARY_READS.with(|c| c.get())
    }

    /// Canned CLIPBOARD text result while a hook is installed.
    pub(super) fn hook_text_result() -> Option<Result<Option<String>, ClipboardTextReadError>> {
        HOOK.with(|h| {
            h.borrow().as_ref().map(|hook| {
                if hook.text_read_failed {
                    Err(ClipboardTextReadError)
                } else {
                    Ok(hook.text.clone())
                }
            })
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn hook_x11_primary_available() -> Option<bool> {
        HOOK.with(|h| h.borrow().as_ref().map(|hook| hook.x11_primary_available))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn hook_primary_text() -> Option<Option<String>> {
        HOOK.with(|h| {
            h.borrow().as_ref().map(|hook| {
                PRIMARY_READS.with(|c| c.set(c.get() + 1));
                hook.primary_text.clone()
            })
        })
    }

    /// Canned probe attachments while a hook is installed; bumps the counter.
    pub(super) fn hook_attachments() -> Option<AttachmentProbeResult> {
        HOOK.with(|h| {
            h.borrow().as_ref().map(|hook| {
                PROBE_CALLS.with(|c| c.set(c.get() + 1));
                if hook.attachment_probe_failed {
                    Err(ClipboardProbeError)
                } else {
                    Ok((hook.image.clone(), hook.file_urls.clone()))
                }
            })
        })
    }

    /// Canned pasteboard snapshot for an installed hook; unset defaults to
    /// available with raster iff a canned `image` is set (text skips, image probes).
    pub(super) fn hook_image_snapshot() -> Option<(Option<u64>, bool)> {
        HOOK.with(|h| {
            h.borrow()
                .as_ref()
                .map(|hook| hook.snapshot.unwrap_or((Some(1), hook.image.is_some())))
        })
    }

    /// Canned `clipboard_image_probe_supported()` when the hook sets one.
    pub(super) fn hook_image_probe_supported() -> Option<bool> {
        HOOK.with(|h| h.borrow().as_ref().and_then(|hook| hook.snapshot_supported))
    }
}

#[cfg(all(test, target_os = "linux"))]
pub use test_support::primary_selection_read_call_count;
#[cfg(test)]
pub use test_support::{
    ClipboardProbeHook, clear_clipboard_probe_hook, clipboard_probe_call_count,
    set_clipboard_probe_hook,
};

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{
        ByobuBackend, EmbeddedEditor, MultiplexerKind, TerminalContext, TerminalName,
        TmuxClientMeta,
    };

    // -- Context builders for clipboard route tests ---------------------------

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

    fn zellij_ctx() -> TerminalContext {
        TerminalContext {
            brand: TerminalName::Ghostty,
            multiplexer: MultiplexerKind::Zellij,
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

    #[test]
    fn lone_http_url_skips_attachment_probe() {
        assert!(looks_like_lone_http_url("https://example.com/path"));
        assert!(plain_text_skips_furl_probe("https://example.com/path"));
        assert!(!clipboard_attachment_probe_needed(Some(
            "https://example.com/path"
        )));
        assert!(!paste_payload_needs_clipboard_attachment_probe(
            "https://example.com/path"
        ));
    }

    #[test]
    fn prose_skips_furl_but_still_probes_image_on_cmd_v() {
        assert!(plain_text_skips_furl_probe("fn main() {\n}\n"));
        assert!(clipboard_attachment_probe_needed(Some("hello world")));
    }

    #[test]
    fn short_bracketed_caption_without_url_still_probes() {
        assert!(paste_payload_needs_clipboard_attachment_probe(
            "caption\nline2"
        ));
    }

    #[test]
    fn bracketed_two_line_https_still_probes() {
        assert!(paste_payload_needs_clipboard_attachment_probe(
            "https://a.com\nhttps://b.com"
        ));
    }

    #[test]
    fn empty_clipboard_still_probes() {
        assert!(clipboard_attachment_probe_needed(None));
        assert!(clipboard_attachment_probe_needed(Some("")));
        assert!(clipboard_attachment_probe_needed(Some("   ")));
        assert!(paste_payload_needs_clipboard_attachment_probe(""));
    }

    #[test]
    fn file_url_in_text_skips_furl_probe_only() {
        assert!(plain_text_skips_furl_probe("file:///tmp/a.png"));
        assert!(clipboard_attachment_probe_needed(Some("file:///tmp/a.png")));
    }

    #[test]
    fn attachment_probe_route_matrix() {
        use AttachmentProbeRoute::{FileUrlsThenImage, ImageOnly, Skip};

        assert_eq!(attachment_probe_route(None), FileUrlsThenImage);
        assert_eq!(attachment_probe_route(Some("")), FileUrlsThenImage);
        assert_eq!(attachment_probe_route(Some("https://x.com")), Skip);
        assert_eq!(attachment_probe_route(Some("hello")), ImageOnly);
        assert_eq!(attachment_probe_route(Some("file:///tmp/a")), ImageOnly);
        assert_eq!(
            attachment_probe_route(Some("https://a.com\nhttps://b.com")),
            FileUrlsThenImage,
        );
    }

    // -- Bracketed payload ↔ clipboard text match ------------------------------

    #[test]
    fn bracketed_payload_match_exact_and_normalized() {
        assert!(bracketed_payload_matches_clipboard_text(
            "hello world",
            Some("hello world")
        ));
        assert!(bracketed_payload_matches_clipboard_text(
            "line1\rline2",
            Some("line1\nline2")
        ));
        assert!(bracketed_payload_matches_clipboard_text(
            "line1\nline2",
            Some("line1\r\nline2")
        ));
        assert!(bracketed_payload_matches_clipboard_text(
            "hello",
            Some("hello\n")
        ));
    }

    #[test]
    fn bracketed_payload_ime_commit_never_matches() {
        assert!(!bracketed_payload_matches_clipboard_text("中", None));
        assert!(!bracketed_payload_matches_clipboard_text("中文输入", None));
        assert!(!bracketed_payload_matches_clipboard_text(
            "中",
            Some("some caption")
        ));
        assert!(!bracketed_payload_matches_clipboard_text(
            "tmux buffer",
            Some("system clipboard")
        ));
    }

    #[test]
    fn bracketed_payload_empty_matches_textless_clipboard() {
        assert!(bracketed_payload_matches_clipboard_text("", None));
        assert!(bracketed_payload_matches_clipboard_text("  ", None));
        assert!(bracketed_payload_matches_clipboard_text("", Some("")));
        assert!(!bracketed_payload_matches_clipboard_text("", Some("text")));
    }

    #[test]
    fn bracketed_payload_from_clipboard_uses_clipboard_text_seam() {
        set_clipboard_probe_hook(ClipboardProbeHook {
            text: Some("caption".to_owned()),
            ..Default::default()
        });
        assert!(bracketed_payload_came_from_clipboard("caption"));
        assert!(!bracketed_payload_came_from_clipboard("中"));
        clear_clipboard_probe_hook();

        set_clipboard_probe_hook(ClipboardProbeHook {
            text: None,
            ..Default::default()
        });
        assert!(!bracketed_payload_came_from_clipboard("中"));
        assert!(bracketed_payload_came_from_clipboard(""));
        clear_clipboard_probe_hook();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn primary_and_clipboard_test_seams_are_distinct() {
        set_clipboard_probe_hook(ClipboardProbeHook {
            text: Some("CLIPBOARD".to_owned()),
            primary_text: Some("PRIMARY".to_owned()),
            x11_primary_available: true,
            ..Default::default()
        });

        assert_eq!(system_clipboard_get().as_deref(), Some("CLIPBOARD"));
        assert_eq!(system_primary_selection_get().as_deref(), Some("PRIMARY"));
        assert_eq!(primary_selection_read_call_count(), 1);
        clear_clipboard_probe_hook();
    }

    #[test]
    fn clipboard_text_read_preserves_failure_for_typed_callers() {
        set_clipboard_probe_hook(ClipboardProbeHook {
            text_read_failed: true,
            ..Default::default()
        });

        assert_eq!(system_clipboard_read_text(), Err(ClipboardTextReadError));
        assert_eq!(system_clipboard_get(), None);
        clear_clipboard_probe_hook();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unavailable_x11_primary_seam_does_not_read_primary() {
        set_clipboard_probe_hook(ClipboardProbeHook {
            primary_text: Some("must not escape the guard".to_owned()),
            x11_primary_available: false,
            ..Default::default()
        });

        assert_eq!(system_primary_selection_get(), None);
        assert_eq!(primary_selection_read_call_count(), 0);
        clear_clipboard_probe_hook();
    }

    #[test]
    fn primary_guidance_rejects_every_non_x11_display_server() {
        use crate::host::DisplayServer;
        assert!(is_native_x11(DisplayServer::X11));
        for display_server in [
            DisplayServer::Wayland,
            DisplayServer::Unknown,
            DisplayServer::Quartz,
            DisplayServer::Win32,
        ] {
            assert!(
                !is_native_x11(display_server),
                "{display_server:?} must not show X11-specific guidance"
            );
        }
    }

    #[test]
    fn should_run_attachment_probe_matrix() {
        use AttachmentProbeRoute::{FileUrlsThenImage, ImageOnly, Skip};

        // Skip never probes; FileUrlsThenImage always probes — regardless of
        // the native snapshot (supported / available / has_image).
        for supported in [false, true] {
            for available in [false, true] {
                for has_image in [false, true] {
                    assert!(!should_run_attachment_probe(
                        Skip, supported, available, has_image
                    ));
                    assert!(should_run_attachment_probe(
                        FileUrlsThenImage,
                        supported,
                        available,
                        has_image
                    ));
                }
            }
        }

        // ImageOnly skips ONLY when the snapshot is supported, AVAILABLE, and
        // reports no raster; an unavailable snapshot (AppKit failure / non-macOS)
        // can't rule out a raster, so it must probe.
        assert!(!should_run_attachment_probe(ImageOnly, true, true, false));
        assert!(should_run_attachment_probe(ImageOnly, true, true, true));
        // The bug fix: available=false must probe even when has_image=false.
        assert!(should_run_attachment_probe(ImageOnly, true, false, false));
        assert!(should_run_attachment_probe(ImageOnly, true, false, true));
        // Non-macOS (supported=false) always probes.
        assert!(should_run_attachment_probe(ImageOnly, false, true, false));
        assert!(should_run_attachment_probe(ImageOnly, false, false, false));
    }

    /// The gate hands back the changeCount its own snapshot read observed, and
    /// the seam serves the same value through `clipboard_change_count()` — so
    /// enqueue sites need no second native read for the TOCTOU baseline.
    #[test]
    fn attachment_probe_gate_returns_snapshot_baseline() {
        set_clipboard_probe_hook(ClipboardProbeHook {
            snapshot: Some((Some(42), true)),
            snapshot_supported: Some(true),
            ..Default::default()
        });
        assert_eq!(attachment_probe_gate(Some("caption")), Some(Some(42)));
        assert!(attachment_probe_would_run(Some("caption")));
        assert_eq!(clipboard_change_count(), Some(42));
        clear_clipboard_probe_hook();

        // No raster: the gate skips, so there is no baseline to hand back.
        set_clipboard_probe_hook(ClipboardProbeHook::no_raster(Some("hello")));
        assert_eq!(attachment_probe_gate(Some("hello")), None);
        assert!(!attachment_probe_would_run(Some("hello")));
        clear_clipboard_probe_hook();

        // Unavailable snapshot: must still probe, baseline honestly None.
        set_clipboard_probe_hook(ClipboardProbeHook::snapshot_unavailable());
        assert_eq!(attachment_probe_gate(Some("hello")), Some(None));
        clear_clipboard_probe_hook();
    }

    // Non-macOS arboard composition; on macOS this shells to real osascript (live pasteboard).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn system_clipboard_probe_attachments_empty_non_mac() {
        set_clipboard_probe_hook(ClipboardProbeHook::default());
        let (image, urls) = system_clipboard_probe_attachments(None).expect("probe must complete");
        clear_clipboard_probe_hook();
        assert!(image.is_none());
        assert!(urls.is_none());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn system_clipboard_probe_attachments_preserves_backend_failure_non_mac() {
        set_clipboard_probe_hook(ClipboardProbeHook {
            attachment_probe_failed: true,
            ..ClipboardProbeHook::snapshot_unavailable()
        });

        assert_eq!(
            system_clipboard_probe_attachments(None),
            Err(ClipboardProbeError)
        );
        clear_clipboard_probe_hook();
    }

    // Non-macOS arboard composition; on macOS this shells to real osascript (live pasteboard).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn system_clipboard_probe_attachments_skip_and_image_only_non_mac() {
        set_clipboard_probe_hook(ClipboardProbeHook::snapshot_unavailable());
        let (image, urls) = system_clipboard_probe_attachments(Some("https://example.com/path"))
            .expect("probe must complete");
        assert!(image.is_none());
        assert!(urls.is_none());

        let (image, urls) =
            system_clipboard_probe_attachments(Some("hello")).expect("probe must complete");
        clear_clipboard_probe_hook();
        assert!(image.is_none());
        assert!(urls.is_none());
    }

    #[test]
    fn plain_text_skips_furl_probe_size_and_line_boundaries() {
        let prose_4095 = "x".repeat(4095);
        let prose_4096 = "x".repeat(4096);
        let five_lines = "one\ntwo\nthree\nfour\nfive";
        let four_lines = "one\ntwo\nthree\nfour";
        let five_lines_with_url = "one\ntwo\nthree\nfour\nhttps://x.com";

        let cases: [(&str, &str, bool); 5] = [
            ("4095-byte prose", &prose_4095, true),
            ("4096-byte prose", &prose_4096, true),
            ("5 lines without ://", five_lines, true),
            ("4 lines without ://", four_lines, true),
            ("5 lines with https", five_lines_with_url, false),
        ];
        for (name, text, expected) in cases {
            assert_eq!(plain_text_skips_furl_probe(text), expected, "{name}");
        }
    }

    // =====================================================================
    // resolve_clipboard_route: pure routing logic
    // =====================================================================

    #[derive(Debug)]
    struct ClipboardRouteCase {
        name: &'static str,
        ctx: TerminalContext,
        native: bool,
        tmux_buffer: bool,
        // osc52: None means "don't assert" (depends on is_remote()).
        osc52: Option<bool>,
    }

    #[test]
    fn clipboard_route_matrix() {
        let cases = [
            ClipboardRouteCase {
                name: "plain_terminal",
                ctx: plain_terminal_ctx(),
                native: true,
                tmux_buffer: false,
                osc52: None,
            },
            ClipboardRouteCase {
                name: "plain_tmux",
                ctx: plain_tmux_ctx(),
                native: true,
                tmux_buffer: true,
                osc52: Some(true),
            },
            ClipboardRouteCase {
                name: "byobu_tmux",
                ctx: byobu_tmux_ctx(),
                native: true,
                tmux_buffer: true,
                osc52: Some(true),
            },
            ClipboardRouteCase {
                name: "byobu_screen",
                ctx: byobu_screen_ctx(),
                native: true,
                tmux_buffer: false,
                osc52: None,
            },
            ClipboardRouteCase {
                name: "zellij",
                ctx: zellij_ctx(),
                native: true,
                tmux_buffer: false,
                osc52: None,
            },
            ClipboardRouteCase {
                name: "plain_screen",
                ctx: plain_screen_ctx(),
                native: true,
                tmux_buffer: false,
                osc52: None,
            },
        ];

        for case in cases {
            let route = resolve_clipboard_route(&case.ctx);
            assert_eq!(
                route.native, case.native,
                "native mismatch on case '{}'",
                case.name
            );
            assert_eq!(
                route.tmux_buffer, case.tmux_buffer,
                "tmux_buffer mismatch on case '{}'",
                case.name
            );
            if let Some(expected_osc) = case.osc52 {
                assert_eq!(
                    route.osc52, expected_osc,
                    "osc52 mismatch on case '{}'",
                    case.name
                );
            }
        }
    }

    // =====================================================================
    // ClipboardRoute structure
    // =====================================================================

    #[test]
    fn clipboard_route_native_always_true() {
        // Every environment should always attempt native clipboard.
        for ctx in [
            plain_terminal_ctx(),
            plain_tmux_ctx(),
            byobu_tmux_ctx(),
            byobu_screen_ctx(),
            zellij_ctx(),
            plain_screen_ctx(),
        ] {
            let route = resolve_clipboard_route(&ctx);
            assert!(
                route.native,
                "native clipboard should always be enabled for {:?}",
                ctx.multiplexer
            );
        }
    }

    #[test]
    fn clipboard_route_tmux_buffer_only_for_tmux_backed() {
        // tmux_buffer should be true only when multiplexer is Tmux.
        let tmux_cases = [plain_tmux_ctx(), byobu_tmux_ctx()];
        let non_tmux_cases = [
            plain_terminal_ctx(),
            byobu_screen_ctx(),
            zellij_ctx(),
            plain_screen_ctx(),
        ];

        for ctx in tmux_cases {
            let route = resolve_clipboard_route(&ctx);
            assert!(
                route.tmux_buffer,
                "tmux_buffer should be true for {:?}",
                ctx.multiplexer
            );
        }

        for ctx in non_tmux_cases {
            let route = resolve_clipboard_route(&ctx);
            assert!(
                !route.tmux_buffer,
                "tmux_buffer should be false for {:?}",
                ctx.multiplexer
            );
        }
    }

    #[test]
    fn clipboard_route_osc52_always_for_tmux_backed() {
        // In tmux-backed environments, OSC 52 is always emitted regardless of
        // remote session status.
        for ctx in [plain_tmux_ctx(), byobu_tmux_ctx()] {
            let route = resolve_clipboard_route(&ctx);
            assert!(
                route.osc52,
                "OSC 52 should always be emitted in tmux-backed env: {:?}",
                ctx.multiplexer
            );
        }
    }

    #[test]
    fn clipboard_route_osc52_tmux_passthrough_truth_table() {
        // tmux + no editor: wrap (tmux is the immediate terminal).
        assert!(resolve_clipboard_route(&plain_tmux_ctx()).osc52_tmux_passthrough);
        // tmux + embedded editor: don't wrap (libvterm is the immediate terminal).
        let mut tmux_in_editor = plain_tmux_ctx();
        tmux_in_editor.embedded_editor = Some(EmbeddedEditor::Neovim);
        assert!(!resolve_clipboard_route(&tmux_in_editor).osc52_tmux_passthrough);
        // non-tmux: never wrap.
        assert!(!resolve_clipboard_route(&plain_terminal_ctx()).osc52_tmux_passthrough);
    }

    // =====================================================================
    // Extended clipboard route matrix (final hardening)
    // =====================================================================

    // -- Byobu-screen: native only, no tmux buffer, no OSC 52 ----------------

    #[test]
    fn clipboard_route_byobu_screen_no_tmux_buffer_no_osc52() {
        let route = resolve_clipboard_route(&byobu_screen_ctx());
        assert!(route.native, "Byobu-screen should write native clipboard");
        assert!(
            !route.tmux_buffer,
            "Byobu-screen must not write tmux buffer"
        );
        // OSC 52 depends on is_remote(), but tmux_buffer must be false.
    }

    // -- Plain screen: no tmux buffer -----------------------------------------

    #[test]
    fn clipboard_route_plain_screen_no_tmux_buffer() {
        let route = resolve_clipboard_route(&plain_screen_ctx());
        assert!(route.native, "plain screen writes native clipboard");
        assert!(
            !route.tmux_buffer,
            "plain screen should not use tmux buffer"
        );
    }

    // -- Consistency: all environments always have native = true ---------------

    #[test]
    fn clipboard_route_native_never_disabled() {
        let all_contexts = [
            plain_terminal_ctx(),
            plain_tmux_ctx(),
            byobu_tmux_ctx(),
            byobu_screen_ctx(),
            zellij_ctx(),
            plain_screen_ctx(),
        ];
        for ctx in all_contexts {
            let route = resolve_clipboard_route(&ctx);
            assert!(
                route.native,
                "native should always be true, was false for {:?}",
                ctx.multiplexer
            );
        }
    }

    // -- tmux-backed: all three legs are active --------------------------------

    #[test]
    fn clipboard_route_tmux_backed_all_three_legs() {
        for ctx in [plain_tmux_ctx(), byobu_tmux_ctx()] {
            let route = resolve_clipboard_route(&ctx);
            assert!(route.native, "native should be true");
            assert!(route.tmux_buffer, "tmux_buffer should be true");
            assert!(route.osc52, "osc52 should be true for tmux-backed");
        }
    }

    // -- Non-tmux-backed: tmux_buffer always false ----------------------------

    #[test]
    fn clipboard_route_non_tmux_never_tmux_buffer() {
        for ctx in [
            plain_terminal_ctx(),
            byobu_screen_ctx(),
            zellij_ctx(),
            plain_screen_ctx(),
        ] {
            let route = resolve_clipboard_route(&ctx);
            assert!(
                !route.tmux_buffer,
                "tmux_buffer must be false for non-tmux {:?}",
                ctx.multiplexer
            );
        }
    }

    #[test]
    fn clipboard_toast_kind_messages_and_telemetry_match_legacy() {
        let cases: [(ClipboardToastKind, &str, &str, u8); 6] = [
            (ClipboardToastKind::Copied, "Copied!", "copied", 30),
            (
                ClipboardToastKind::CopiedTmux,
                "Copied to tmux buffer, paste with prefix + ]",
                "copied_tmux",
                120,
            ),
            (
                ClipboardToastKind::CopiedOscContainer,
                "Copied via OSC 52 (container). If paste fails, hold Shift (or Fn) and drag to select & copy natively.",
                "copied_osc_container",
                120,
            ),
            (
                ClipboardToastKind::CopiedOscRemote,
                "Copied via OSC 52. If paste fails, hold Shift (or Fn) and drag to select & copy natively.",
                "copied_osc_remote",
                120,
            ),
            (
                ClipboardToastKind::VsCodeSshNonAscii,
                "Copied. In case VSCode via SSH garbles non-ASCII text, use native copy (shift+select).",
                "vs_code_ssh_non_ascii",
                120,
            ),
            (
                ClipboardToastKind::Failed,
                "Copy failed. Try /minimal for terminal native rendering",
                "failed",
                120,
            ),
        ];
        for (kind, message, telemetry, ticks) in cases {
            let label: &'static str = kind.into();
            assert_eq!(kind.message(), message, "message for {kind:?}");
            assert_eq!(label, telemetry, "telemetry for {kind:?}");
            assert_eq!(kind.ticks(), ticks, "ticks for {kind:?}");
            let result = kind.to_result();
            assert_eq!(result.message, message);
            assert_eq!(result.ticks, ticks);
            assert_eq!(result.success, kind.reported_success());
        }
    }
}
