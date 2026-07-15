//! OS-level rescue for the modified-Enter chord.
//!
//! Apple Terminal can't deliver Shift/Opt/Cmd + Enter modifier flags via
//! crossterm. We side-channel through the same OS probe used by
//! [`super::keyboard_normalizer`] and gate on
//! [`crate::terminal::KeyboardCapabilities::enter_needs_rescue`] so the
//! per-brand truth lives in one place.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::terminal::terminal_context;

/// Returns `true` when the user is holding a modifier that should turn a
/// bare `Enter` into a newline, and the active terminal is classified as
/// dropping that information.
pub fn is_apple_terminal_newline_modifier_held() -> bool {
    let ctx = terminal_context();
    if !ctx.keyboard_capabilities().enter_needs_rescue() {
        return false;
    }
    os_any_newline_modifier_held()
}

/// Shift/Alt+Enter, or bare Enter while a newline modifier is held and the
/// terminal drops those flags ([`is_apple_terminal_newline_modifier_held`]).
/// Always requires `KeyCode::Enter` so Shift+Tab / Shift+letters never match.
pub fn is_mod_enter(key: &KeyEvent) -> bool {
    key.code == KeyCode::Enter
        && (key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
            || is_apple_terminal_newline_modifier_held())
}

#[cfg(target_os = "macos")]
fn os_any_newline_modifier_held() -> bool {
    let s = super::macos_modifiers::snapshot();
    s.shift || s.option || s.command
}

#[cfg(not(target_os = "macos"))]
fn os_any_newline_modifier_held() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn is_mod_enter_requires_enter_code() {
        assert!(is_mod_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT
        )));
        assert!(is_mod_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::ALT
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));
        // Shift+Tab must never match (BackTab or Tab+SHIFT).
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::NONE
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::BackTab,
            KeyModifiers::SHIFT
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::Tab,
            KeyModifiers::SHIFT
        )));
        assert!(!is_mod_enter(&KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::SHIFT
        )));
    }
}
