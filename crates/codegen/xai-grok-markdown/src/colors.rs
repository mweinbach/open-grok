//! Terminal color support detection and color conversion utilities.
//!
//! This module provides functionality to detect the terminal's color capabilities
//! and downgrade RGB colors to the appropriate level when needed.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use anstyle::{Ansi256Color, AnsiColor, Color, RgbColor};

/// The level of color support detected for the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum ColorLevel {
    /// No color support (monochrome terminals)
    None,
    /// Basic 16-color ANSI support (colors 0-15)
    Basic,
    /// 256-color support (colors 0-255)
    Ansi256,
    /// 24-bit truecolor RGB support (16 million colors)
    #[default]
    TrueColor,
}

impl ColorLevel {
    /// Returns true if at least basic color is supported.
    pub fn has_color(self) -> bool {
        self >= Self::Basic
    }

    /// Returns true if 256-color mode is supported.
    pub fn has_256(self) -> bool {
        self >= Self::Ansi256
    }

    /// Returns true if 24-bit truecolor is supported.
    pub fn has_truecolor(self) -> bool {
        self >= Self::TrueColor
    }
}

static COLOR_LEVEL: OnceLock<ColorLevel> = OnceLock::new();

/// Detect the terminal's color support level.
///
/// This uses the `supports-color` crate which checks:
/// - `COLORTERM` environment variable (for truecolor detection)
/// - `TERM` environment variable
/// - Terminal-specific environment variables (like `ITERM_SESSION_ID`)
/// - Whether stdout is a TTY
///
/// The result is cached after the first call.
pub fn detect_color_level() -> ColorLevel {
    *COLOR_LEVEL.get_or_init(|| {
        // Explicit opt-out via NO_COLOR takes priority.
        if std::env::var_os("NO_COLOR").is_some() {
            return ColorLevel::None;
        }

        let level = match supports_color::on(supports_color::Stream::Stdout) {
            // Not a TTY (tests, piped) — default to TrueColor.
            // The pager is a TUI app that always runs inside a terminal;
            // stdout may not be a TTY when the pager renders to stderr.
            None => ColorLevel::TrueColor,
            Some(level) => {
                if level.has_16m {
                    ColorLevel::TrueColor
                } else if level.has_256 {
                    ColorLevel::Ansi256
                } else if level.has_basic {
                    ColorLevel::Basic
                } else {
                    ColorLevel::None
                }
            }
        };

        // The `supports-color` crate relies on COLORTERM=truecolor, but
        // tmux/SSH/mosh often strip that variable.  When the crate reports
        // only 256-color support, upgrade to TrueColor if we can identify
        // a known truecolor-capable terminal via its env vars.
        if level < ColorLevel::TrueColor && terminal_supports_truecolor() {
            return ColorLevel::TrueColor;
        }

        level
    })
}

/// Check whether the terminal emulator is known to support truecolor.
///
/// Used as a fallback when `COLORTERM` is missing (e.g. inside tmux or over
/// SSH).  Checks terminal-specific env vars that survive session forwarding
/// even when `COLORTERM` and `TERM_PROGRAM` are stripped.
fn terminal_supports_truecolor() -> bool {
    use std::env;

    // TERM_PROGRAM is the most reliable signal (set by the emulator itself).
    if let Ok(prog) = env::var("TERM_PROGRAM") {
        let norm: String = prog
            .trim()
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
            .map(|c| c.to_ascii_lowercase())
            .collect();
        // Every modern terminal except Apple Terminal supports truecolor.
        if matches!(
            norm.as_str(),
            "iterm"
                | "iterm2"
                | "itermapp"
                | "ghostty"
                | "kitty"
                | "wezterm"
                | "alacritty"
                | "warp"
                | "warpterminal"
                | "vscode"
        ) {
            return true;
        }
    }

    // Terminal-specific env vars that often survive tmux/SSH.
    env::var("ITERM_SESSION_ID").is_ok()
        || env::var("ITERM_PROFILE").is_ok()
        || env::var("WEZTERM_VERSION").is_ok()
        || env::var("KITTY_WINDOW_ID").is_ok()
        || env::var("ALACRITTY_SOCKET").is_ok()
}

/// Process-wide upper bound on the effective color level, stored as the
/// `ColorLevel` declaration-order discriminant.
static COLOR_LEVEL_CAP: AtomicU8 = AtomicU8::new(ColorLevel::TrueColor as u8);

/// Set the process-wide upper bound on the effective color level. Pass
/// [`ColorLevel::TrueColor`] to remove the cap.
pub fn set_color_level_cap(cap: ColorLevel) {
    COLOR_LEVEL_CAP.store(cap as u8, Ordering::Relaxed);
}

fn color_level_cap() -> ColorLevel {
    match COLOR_LEVEL_CAP.load(Ordering::Relaxed) {
        0 => ColorLevel::None,
        1 => ColorLevel::Basic,
        2 => ColorLevel::Ansi256,
        _ => ColorLevel::TrueColor,
    }
}

/// Get the current color level (detecting if not already done), bounded by
/// the process-wide cap (see [`set_color_level_cap`]).
pub fn get_color_level() -> ColorLevel {
    detect_color_level().min(color_level_cap())
}

/// Override the color level (useful for testing or user preference).
///
/// Returns `Err` if the color level was already set.
#[allow(dead_code)]
pub fn set_color_level(level: ColorLevel) -> Result<(), ColorLevel> {
    COLOR_LEVEL.set(level)
}

/// Convert an `anstyle::Color` to the appropriate level based on terminal support.
///
/// This will downgrade colors as needed:
/// - TrueColor terminals: pass through unchanged
/// - 256-color terminals: RGB colors are converted to closest ANSI 256 color
/// - Basic terminals: colors are converted to closest ANSI 16 color
/// - No color: returns None
pub fn adapt_color(color: Color) -> Option<Color> {
    let level = get_color_level();

    match level {
        ColorLevel::None => None,
        ColorLevel::TrueColor => Some(color),
        ColorLevel::Ansi256 => Some(match color {
            Color::Rgb(rgb) => Color::Ansi256(rgb_to_ansi256(rgb)),
            other => other,
        }),
        ColorLevel::Basic => Some(match color {
            Color::Rgb(rgb) => Color::Ansi(rgb_to_ansi16(rgb)),
            Color::Ansi256(idx) => Color::Ansi(ansi256_to_ansi16(idx)),
            Color::Ansi(ansi) => Color::Ansi(ansi),
        }),
    }
}

/// Convert an `anstyle::Style` to the appropriate color level.
pub fn adapt_style(style: anstyle::Style) -> anstyle::Style {
    let fg = style.get_fg_color().and_then(adapt_color);
    let bg = style.get_bg_color().and_then(adapt_color);
    let effects = style.get_effects();

    let mut new_style = anstyle::Style::new();
    if let Some(fg) = fg {
        new_style = new_style.fg_color(Some(fg));
    }
    if let Some(bg) = bg {
        new_style = new_style.bg_color(Some(bg));
    }
    new_style | effects
}

/// Convert an RGB color to the closest ANSI 256-color palette entry.
pub fn rgb_to_ansi256(rgb: RgbColor) -> Ansi256Color {
    anstyle_lossy::rgb_to_xterm(rgb)
}

/// Convert an RGB color to the closest basic ANSI 16-color.
pub fn rgb_to_ansi16(rgb: RgbColor) -> AnsiColor {
    anstyle_lossy::rgb_to_ansi(rgb, anstyle_lossy::palette::VGA)
}

/// Convert an ANSI 256-color to the closest basic ANSI 16-color.
pub fn ansi256_to_ansi16(idx: Ansi256Color) -> AnsiColor {
    anstyle_lossy::xterm_to_ansi(idx, anstyle_lossy::palette::VGA)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rgb_to_ansi256_grayscale() {
        // Pure black should map to near-black
        let result = rgb_to_ansi256(RgbColor(0, 0, 0));
        assert!(result.index() == 16 || result.index() >= 232);

        // Pure white should map to near-white
        let result = rgb_to_ansi256(RgbColor(255, 255, 255));
        assert!(result.index() == 231 || result.index() == 255);

        // Medium gray
        let result = rgb_to_ansi256(RgbColor(128, 128, 128));
        assert!(result.index() >= 232); // Should be in grayscale range
    }

    #[test]
    fn test_rgb_to_ansi256_colors() {
        // Pure red
        let result = rgb_to_ansi256(RgbColor(255, 0, 0));
        assert_eq!(result.index(), 196); // Bright red in the cube

        // Pure green
        let result = rgb_to_ansi256(RgbColor(0, 255, 0));
        assert_eq!(result.index(), 46); // Bright green in the cube

        // Pure blue
        let result = rgb_to_ansi256(RgbColor(0, 0, 255));
        assert_eq!(result.index(), 21); // Bright blue in the cube
    }

    #[test]
    fn test_rgb_to_ansi16() {
        // Test basic color mapping
        let red = rgb_to_ansi16(RgbColor(200, 0, 0));
        assert!(matches!(red, AnsiColor::Red | AnsiColor::BrightRed));

        let green = rgb_to_ansi16(RgbColor(0, 200, 0));
        assert!(matches!(green, AnsiColor::Green | AnsiColor::BrightGreen));

        let blue = rgb_to_ansi16(RgbColor(0, 0, 200));
        assert!(matches!(blue, AnsiColor::Blue | AnsiColor::BrightBlue));

        // White
        let white = rgb_to_ansi16(RgbColor(250, 250, 250));
        assert!(matches!(white, AnsiColor::White | AnsiColor::BrightWhite));
    }

    #[test]
    fn test_ansi256_to_ansi16_standard() {
        // First 16 colors should map directly
        assert_eq!(ansi256_to_ansi16(Ansi256Color(0)), AnsiColor::Black);
        assert_eq!(ansi256_to_ansi16(Ansi256Color(1)), AnsiColor::Red);
        assert_eq!(ansi256_to_ansi16(Ansi256Color(7)), AnsiColor::White);
        assert_eq!(ansi256_to_ansi16(Ansi256Color(15)), AnsiColor::BrightWhite);
    }

    #[test]
    fn test_color_level_ordering() {
        assert!(ColorLevel::None < ColorLevel::Basic);
        assert!(ColorLevel::Basic < ColorLevel::Ansi256);
        assert!(ColorLevel::Ansi256 < ColorLevel::TrueColor);
    }
}
