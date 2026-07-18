//! Environment-based delivery and toast policy for clipboard writes.
//!
//! Writes still multi-fire every backend; this module classifies whether a
//! successful leg is known to reach the destination named by the UI.

use crate::host::{DisplayServer, HostOs};
use crate::terminal::TerminalName;

use super::{ClipboardFeedback, ClipboardWriteLegs};

/// Grok's evidence that a clipboard write reached its intended destination.
#[derive(Debug, Clone, Copy, Eq, PartialEq, strum::IntoStaticStr)]
#[strum(serialize_all = "snake_case")]
pub enum ClipboardDelivery {
    /// A successful write leg has a destination trusted by the environment policy.
    Confirmed,
    /// OSC 52 was emitted, but the outer terminal's clipboard support is unknown.
    Unverified,
    /// No usable write leg succeeded, or the destination is known not to support it.
    Failed,
}

impl ClipboardDelivery {
    pub fn is_failed(self) -> bool {
        self == Self::Failed
    }

    pub fn reported_success(self) -> bool {
        !self.is_failed()
    }

    pub fn telemetry_label(self) -> &'static str {
        self.into()
    }
}

/// Native clipboard route evidence available before a copy is attempted.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NativeClipboardPreflight {
    Disabled,
    LocalAvailable,
    RemoteOnly,
    Unavailable,
}

fn trusted_wayland_native(wl_copy: bool, arboard: bool, data_control: bool) -> bool {
    wl_copy || (arboard && data_control)
}

/// Classify the configured native route without claiming that a write succeeded.
pub fn native_clipboard_preflight(
    route_native: bool,
    host_os: HostOs,
    display_server: DisplayServer,
    remote: bool,
    container: bool,
    wayland_data_control: bool,
    wl_copy_available: bool,
) -> NativeClipboardPreflight {
    if !route_native {
        return NativeClipboardPreflight::Disabled;
    }
    if remote || container {
        return NativeClipboardPreflight::RemoteOnly;
    }
    match host_os {
        HostOs::Linux => match display_server {
            DisplayServer::Wayland
                if trusted_wayland_native(wl_copy_available, true, wayland_data_control) =>
            {
                NativeClipboardPreflight::LocalAvailable
            }
            DisplayServer::Wayland | DisplayServer::Unknown => {
                NativeClipboardPreflight::Unavailable
            }
            DisplayServer::X11 => NativeClipboardPreflight::LocalAvailable,
            DisplayServer::Quartz | DisplayServer::Win32 => NativeClipboardPreflight::Unavailable,
        },
        HostOs::Macos | HostOs::Windows => NativeClipboardPreflight::LocalAvailable,
        HostOs::Other => NativeClipboardPreflight::Unavailable,
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) struct ClipboardDecision {
    pub(crate) delivery: ClipboardDelivery,
    pub(crate) feedback: ClipboardFeedback,
}

/// Classify one emitted OSC 52 write using the existing environment policy.
pub(crate) fn osc52_delivery(
    brand: TerminalName,
    remote: bool,
    container: bool,
    osc52_sink: bool,
) -> ClipboardDelivery {
    if osc52_sink || brand.supports_osc52_clipboard() {
        ClipboardDelivery::Confirmed
    } else if brand == TerminalName::Unknown && (remote || container) {
        ClipboardDelivery::Unverified
    } else {
        ClipboardDelivery::Failed
    }
}

/// Expected preflight confidence for an enabled clipboard route.
pub fn expected_delivery(
    native: NativeClipboardPreflight,
    route_tmux: bool,
    route_osc52: bool,
    brand: TerminalName,
    remote: bool,
    container: bool,
    osc52_sink: bool,
) -> ClipboardDelivery {
    if native == NativeClipboardPreflight::LocalAvailable {
        return ClipboardDelivery::Confirmed;
    }
    let osc52 = route_osc52.then(|| osc52_delivery(brand, remote, container, osc52_sink));
    if osc52 == Some(ClipboardDelivery::Confirmed) || route_tmux {
        return ClipboardDelivery::Confirmed;
    }
    if osc52 == Some(ClipboardDelivery::Unverified) {
        return ClipboardDelivery::Unverified;
    }
    ClipboardDelivery::Failed
}

/// True when native legs wrote the local OS clipboard rather than a remote host.
pub(crate) fn trusted_native(
    legs: &ClipboardWriteLegs,
    host_os: HostOs,
    display_server: DisplayServer,
    remote: bool,
    container: bool,
) -> bool {
    if remote || container || !legs.route_native {
        return false;
    }
    match host_os {
        HostOs::Linux => match display_server {
            DisplayServer::Wayland => {
                trusted_wayland_native(legs.wl_copy_ok, legs.arboard_ok, legs.data_control)
            }
            _ => legs.cli_ok || legs.arboard_ok,
        },
        HostOs::Macos | HostOs::Windows | HostOs::Other => legs.cli_ok || legs.arboard_ok,
    }
}

/// Resolve the user-visible branch and delivery classification together.
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_copy_decision(
    legs: &ClipboardWriteLegs,
    text: &str,
    brand: TerminalName,
    host_os: HostOs,
    display_server: DisplayServer,
    remote: bool,
    container: bool,
    osc52_sink: bool,
) -> ClipboardDecision {
    let decision = |delivery, feedback| ClipboardDecision { delivery, feedback };
    if trusted_native(legs, host_os, display_server, remote, container) {
        return decision(ClipboardDelivery::Confirmed, ClipboardFeedback::Copied);
    }
    if legs.osc52_ok {
        match osc52_delivery(brand, remote, container, osc52_sink) {
            ClipboardDelivery::Confirmed => {
                let feedback = if remote && brand.is_vscode_family() && !text.is_ascii() {
                    ClipboardFeedback::VsCodeSshNonAscii
                } else if container {
                    ClipboardFeedback::CopiedOscContainer
                } else if remote {
                    ClipboardFeedback::CopiedOscRemote
                } else {
                    ClipboardFeedback::Copied
                };
                return decision(ClipboardDelivery::Confirmed, feedback);
            }
            ClipboardDelivery::Unverified if !legs.tmux_ok => {
                let feedback = if remote {
                    ClipboardFeedback::UnverifiedOscRemote
                } else {
                    ClipboardFeedback::UnverifiedOscContainer
                };
                return decision(ClipboardDelivery::Unverified, feedback);
            }
            ClipboardDelivery::Unverified | ClipboardDelivery::Failed => {}
        }
    }
    if legs.tmux_ok {
        return decision(ClipboardDelivery::Confirmed, ClipboardFeedback::CopiedTmux);
    }
    decision(ClipboardDelivery::Failed, ClipboardFeedback::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legs(
        cli_ok: bool,
        arboard_ok: bool,
        data_control: bool,
        tmux_ok: bool,
        osc52_ok: bool,
        cli_ok_tools: &str,
    ) -> ClipboardWriteLegs {
        ClipboardWriteLegs {
            route_native: true,
            route_label: "test".into(),
            cli_tools_tried: String::new(),
            cli_ok_tools: cli_ok_tools.into(),
            wl_copy_ok: cli_ok_tools.split('+').any(|tool| tool == "wl-copy"),
            cli_ok,
            arboard_ok,
            data_control,
            tmux_ok,
            osc52_ok,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve(
        legs: &ClipboardWriteLegs,
        text: &str,
        brand: TerminalName,
        host_os: HostOs,
        display_server: DisplayServer,
        remote: bool,
        container: bool,
        osc52_sink: bool,
    ) -> ClipboardDecision {
        resolve_copy_decision(
            legs,
            text,
            brand,
            host_os,
            display_server,
            remote,
            container,
            osc52_sink,
        )
    }

    #[test]
    fn telemetry_projection_labels_and_historical_boolean_are_pinned() {
        for (delivery, label, reported_success) in [
            (ClipboardDelivery::Confirmed, "confirmed", true),
            (ClipboardDelivery::Unverified, "unverified", true),
            (ClipboardDelivery::Failed, "failed", false),
        ] {
            assert_eq!(delivery.telemetry_label(), label);
            assert_eq!(delivery.reported_success(), reported_success);
        }
    }

    #[test]
    fn local_trusted_native_is_confirmed() {
        let decision = resolve(
            &legs(true, false, false, false, false, "pbcopy"),
            "hello",
            TerminalName::Ghostty,
            HostOs::Macos,
            DisplayServer::Quartz,
            false,
            false,
            false,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Confirmed);
        assert_eq!(decision.feedback, ClipboardFeedback::Copied);
    }

    #[test]
    fn wayland_native_requires_verified_destination() {
        let unverified = legs(false, true, false, false, false, "");
        assert!(!trusted_native(
            &unverified,
            HostOs::Linux,
            DisplayServer::Wayland,
            false,
            false
        ));
        let data_control = legs(false, true, true, false, false, "");
        assert!(trusted_native(
            &data_control,
            HostOs::Linux,
            DisplayServer::Wayland,
            false,
            false
        ));
        let wl_copy = legs(true, false, false, false, false, "wl-copy");
        assert!(trusted_native(
            &wl_copy,
            HostOs::Linux,
            DisplayServer::Wayland,
            false,
            false
        ));
    }

    #[test]
    fn remote_native_write_only_is_failed() {
        let decision = resolve(
            &legs(true, true, false, false, false, "xclip"),
            "hello",
            TerminalName::Ghostty,
            HostOs::Linux,
            DisplayServer::X11,
            true,
            false,
            false,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Failed);
    }

    #[test]
    fn known_osc_capable_terminal_is_confirmed() {
        let decision = resolve(
            &legs(false, false, false, false, true, ""),
            "hello",
            TerminalName::Ghostty,
            HostOs::Linux,
            DisplayServer::Unknown,
            true,
            false,
            false,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Confirmed);
        assert_eq!(decision.feedback, ClipboardFeedback::CopiedOscRemote);
    }

    #[test]
    fn ssh_unknown_brand_osc_is_unverified() {
        let decision = resolve(
            &legs(false, false, false, false, true, ""),
            "hello",
            TerminalName::Unknown,
            HostOs::Linux,
            DisplayServer::Unknown,
            true,
            false,
            false,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Unverified);
        assert_eq!(decision.feedback, ClipboardFeedback::UnverifiedOscRemote);
    }

    #[test]
    fn container_unknown_brand_osc_is_unverified() {
        let decision = resolve(
            &legs(false, false, false, false, true, ""),
            "hello",
            TerminalName::Unknown,
            HostOs::Linux,
            DisplayServer::Unknown,
            false,
            true,
            false,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Unverified);
        assert_eq!(decision.feedback, ClipboardFeedback::UnverifiedOscContainer);
    }

    #[test]
    fn known_unsupported_terminal_osc_is_failed() {
        for brand in [TerminalName::AppleTerminal, TerminalName::Vte] {
            let decision = resolve(
                &legs(false, false, false, false, true, ""),
                "hello",
                brand,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false,
                false,
            );
            assert_eq!(decision.delivery, ClipboardDelivery::Failed, "{brand:?}");
        }
    }

    #[test]
    fn active_wrap_sink_with_osc_is_confirmed_for_any_brand() {
        for brand in [TerminalName::Unknown, TerminalName::AppleTerminal] {
            let decision = resolve(
                &legs(false, false, false, false, true, ""),
                "hello",
                brand,
                HostOs::Linux,
                DisplayServer::Unknown,
                true,
                false,
                true,
            );
            assert_eq!(decision.delivery, ClipboardDelivery::Confirmed, "{brand:?}");
        }
    }

    #[test]
    fn wrap_sink_without_osc_write_is_failed() {
        let decision = resolve(
            &legs(false, false, false, false, false, ""),
            "hello",
            TerminalName::Unknown,
            HostOs::Linux,
            DisplayServer::Unknown,
            true,
            false,
            true,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Failed);
    }

    #[test]
    fn tmux_success_wins_over_unverified_osc() {
        let decision = resolve(
            &legs(false, false, false, true, true, ""),
            "hello",
            TerminalName::Unknown,
            HostOs::Linux,
            DisplayServer::Unknown,
            true,
            false,
            false,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Confirmed);
        assert_eq!(decision.feedback, ClipboardFeedback::CopiedTmux);
    }

    #[test]
    fn no_successful_leg_is_failed() {
        let decision = resolve(
            &legs(false, false, false, false, false, ""),
            "hello",
            TerminalName::Ghostty,
            HostOs::Linux,
            DisplayServer::Unknown,
            true,
            false,
            false,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Failed);
        assert_eq!(decision.feedback, ClipboardFeedback::Failed);
    }

    #[test]
    fn vscode_ssh_non_ascii_stays_confirmed_with_warning_toast() {
        let decision = resolve(
            &legs(false, false, false, false, true, ""),
            "café",
            TerminalName::VsCode,
            HostOs::Linux,
            DisplayServer::Unknown,
            true,
            false,
            false,
        );
        assert_eq!(decision.delivery, ClipboardDelivery::Confirmed);
        assert_eq!(decision.feedback, ClipboardFeedback::VsCodeSshNonAscii);
    }

    #[test]
    fn native_preflight_matches_observed_wayland_trust_matrix() {
        for (data_control, wl_copy, expected) in [
            (false, false, NativeClipboardPreflight::Unavailable),
            (false, true, NativeClipboardPreflight::LocalAvailable),
            (true, false, NativeClipboardPreflight::LocalAvailable),
            (true, true, NativeClipboardPreflight::LocalAvailable),
        ] {
            assert_eq!(
                native_clipboard_preflight(
                    true,
                    HostOs::Linux,
                    DisplayServer::Wayland,
                    false,
                    false,
                    data_control,
                    wl_copy,
                ),
                expected,
                "data_control={data_control} wl_copy={wl_copy}"
            );
        }
        assert_eq!(
            native_clipboard_preflight(
                true,
                HostOs::Linux,
                DisplayServer::Wayland,
                true,
                false,
                true,
                true,
            ),
            NativeClipboardPreflight::RemoteOnly
        );
    }

    #[test]
    fn expected_delivery_matches_preflight_routes() {
        assert_eq!(
            expected_delivery(
                NativeClipboardPreflight::RemoteOnly,
                false,
                true,
                TerminalName::Unknown,
                true,
                false,
                false,
            ),
            ClipboardDelivery::Unverified
        );
        assert_eq!(
            expected_delivery(
                NativeClipboardPreflight::RemoteOnly,
                false,
                true,
                TerminalName::Vte,
                true,
                false,
                false,
            ),
            ClipboardDelivery::Failed
        );
        assert_eq!(
            expected_delivery(
                NativeClipboardPreflight::RemoteOnly,
                false,
                true,
                TerminalName::Vte,
                true,
                false,
                true,
            ),
            ClipboardDelivery::Confirmed
        );
        assert_eq!(
            expected_delivery(
                NativeClipboardPreflight::RemoteOnly,
                true,
                false,
                TerminalName::Unknown,
                true,
                false,
                false,
            ),
            ClipboardDelivery::Confirmed
        );
        assert_eq!(
            expected_delivery(
                NativeClipboardPreflight::Unavailable,
                false,
                false,
                TerminalName::Vte,
                false,
                false,
                false,
            ),
            ClipboardDelivery::Failed
        );
    }
}
