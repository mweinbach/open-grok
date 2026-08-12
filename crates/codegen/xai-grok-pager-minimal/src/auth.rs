//! Minimal-mode sign-in / folder-trust rendering for the live region.
//!
//! Before any agent session exists (unauthenticated / folder-trust pending) the
//! minimal live region shows the sign-in flow itself — device or external-command
//! flow, a sign-in error, the folder-trust question, or a brief "starting"
//! transient once both gates are open — since minimal has no welcome screen.

use std::path::PathBuf;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use xai_grok_pager::app::app_view::{AuthState, PrimaryProvider, TrustState};
use xai_grok_pager::theme::Theme;

/// What the minimal live region shows when there is no active agent yet: the
/// in-region sign-in flow (device or external-command), a sign-in error, or a
/// brief "starting" transient once authenticated. Computed from [`AuthState`]
/// before the draw closure so the closure can own it.
pub(super) enum MinimalAuthHint {
    /// Neither selected provider has a usable credential yet.
    ChooseProvider { error: Option<String> },
    /// Interactive sign-in underway — show the URL (when known) and the device
    /// code (when the URL carries one). Covers device flow and the external
    /// command flow (where the provider opens its own browser; `url` may be
    /// `None`).
    SigningIn {
        provider: PrimaryProvider,
        url: Option<String>,
        code: Option<String>,
    },
    /// The last sign-in attempt failed; show the error.
    Failed(String),
    /// Authenticated, but the cwd has untrusted repo-local config.
    TrustFolder { workspace: PathBuf },
    /// Authenticated — the session is being created (brief transient).
    Starting,
}

/// Resolve the owner of the welcome auth flow independently from the active
/// model provider. Bare `/login` is always xAI—even from a Codex tab—while the
/// first-launch chooser records its explicit selection.
pub(super) fn minimal_auth_provider(
    _active_provider: PrimaryProvider,
    startup_selection: Option<PrimaryProvider>,
) -> PrimaryProvider {
    startup_selection.unwrap_or(PrimaryProvider::Xai)
}

/// Map the app's [`AuthState`] to what the no-agent live region should show.
#[cfg(test)]
pub(super) fn minimal_auth_hint(
    auth: &AuthState,
    primary_provider: PrimaryProvider,
) -> MinimalAuthHint {
    minimal_auth_hint_with_trust(auth, primary_provider, &TrustState::Done, true, false)
}

/// Map auth + trust gates to the minimal live-region surface while preserving
/// the selected provider label used by the fork's multi-provider onboarding.
pub(super) fn minimal_auth_hint_with_trust(
    auth: &AuthState,
    primary_provider: PrimaryProvider,
    trust: &TrustState,
    has_access: bool,
    is_zdr_blocked: bool,
) -> MinimalAuthHint {
    match auth {
        AuthState::ProviderChoice { error } => MinimalAuthHint::ChooseProvider {
            error: error.clone(),
        },
        AuthState::Authenticating { auth_url, .. } => MinimalAuthHint::SigningIn {
            provider: primary_provider,
            url: auth_url.clone(),
            code: auth_url
                .as_deref()
                .and_then(device_user_code)
                .map(str::to_owned),
        },
        AuthState::Pending { error: Some(err) } => MinimalAuthHint::Failed(err.clone()),
        // Login is starting (auto-triggered at startup) — the URL arrives via
        // AuthUrlReady, which flips us to `Authenticating`.
        AuthState::Pending { error: None } => MinimalAuthHint::SigningIn {
            provider: primary_provider,
            url: None,
            code: None,
        },
        AuthState::Done if has_access && !is_zdr_blocked => {
            if let TrustState::Pending { workspace } = trust {
                MinimalAuthHint::TrustFolder {
                    workspace: workspace.clone(),
                }
            } else {
                MinimalAuthHint::Starting
            }
        }
        AuthState::Done => MinimalAuthHint::Starting,
    }
}

/// Rows the no-agent live region needs for `hint` (before path wrap).
pub(super) fn auth_hint_rows(hint: &MinimalAuthHint, width: u16) -> u16 {
    match hint {
        MinimalAuthHint::ChooseProvider { error } => 5 + u16::from(error.is_some()),
        MinimalAuthHint::SigningIn { url: None, .. } => 3,
        MinimalAuthHint::SigningIn {
            url: Some(url),
            code,
            ..
        } => {
            let code_rows = if code.is_some() { 2 } else { 0 };
            3 + wrapped_char_rows(url, width) + code_rows + 2
        }
        MinimalAuthHint::Failed(_) => 3,
        MinimalAuthHint::TrustFolder { workspace } => {
            1 + wrapped_char_rows(&workspace.display().to_string(), width) + 1 + 2 + 1 + 2 + 1 + 1
        }
        MinimalAuthHint::Starting => 1,
    }
}

fn wrapped_char_rows(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let chars = text.chars().filter(|c| !c.is_control()).count();
    chars.max(1).div_ceil(width) as u16
}

/// Parse the device-flow `user_code` from a verification URL (`None` if absent
/// or malformed). Mirrors `views::welcome::extract_user_code`, kept local so
/// minimal does not depend on welcome-screen internals.
fn device_user_code(url: &str) -> Option<&str> {
    let code = url
        .split('?')
        .nth(1)?
        .split('&')
        .find_map(|kv| kv.strip_prefix("user_code="))?;
    (!code.is_empty() && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then_some(code)
}

/// Write `line` at row `y` (when it fits) and return the next row.
fn put_line(buf: &mut Buffer, area: Rect, y: u16, bottom: u16, line: Line<'_>) -> u16 {
    if y < bottom {
        buf.set_line(area.x, y, &line, area.width);
        y + 1
    } else {
        y
    }
}

/// Write `url` character-by-character across as many rows as it needs (no
/// wrap-inserted spaces), so the terminal's native selection copies it verbatim
/// — minimal has no mouse capture, so copy is the terminal's job. Returns the
/// next free row.
fn render_url(
    buf: &mut Buffer,
    area: Rect,
    start_y: u16,
    bottom: u16,
    url: &str,
    style: Style,
) -> u16 {
    let width = area.width.max(1);
    // Snapshot the buffer bounds as values so the `&Rect` borrow doesn't outlive
    // the mutable cell writes below.
    let (max_x, max_y) = {
        let a = buf.area();
        (a.right(), a.bottom())
    };
    let mut col = 0u16;
    let mut y = start_y;
    for ch in url.chars() {
        // Skip control chars to prevent terminal escape injection.
        if ch.is_control() {
            continue;
        }
        if col >= width {
            col = 0;
            y = y.saturating_add(1);
        }
        if y >= bottom {
            return bottom;
        }
        let x = area.x + col;
        if x < max_x && y < max_y {
            buf[(x, y)].set_char(ch).set_style(style);
        }
        col += 1;
    }
    y.saturating_add(1)
}

/// Render the sign-in flow (or transient status) in the live region when no
/// agent exists yet. Top-aligned in `area`; clips to its height.
pub(super) fn render_auth(buf: &mut Buffer, area: Rect, theme: &Theme, hint: &MinimalAuthHint) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let bottom = area.y + area.height;
    let mut y = area.y;
    let gray = theme.muted().bg(Color::Reset);
    let bold = Style::default()
        .fg(theme.text_primary)
        .add_modifier(Modifier::BOLD)
        .bg(Color::Reset);

    match hint {
        MinimalAuthHint::ChooseProvider { error } => {
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled("Choose your provider", bold)),
            );
            if let Some(error) = error {
                y = put_line(
                    buf,
                    area,
                    y,
                    bottom,
                    Line::from(Span::styled(
                        error.clone(),
                        Style::default().fg(theme.warning).bg(Color::Reset),
                    )),
                );
            }
            y = put_line(buf, area, y, bottom, Line::default());
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(vec![
                    Span::styled("[c] ", bold),
                    Span::styled("ChatGPT Codex", gray),
                ]),
            );
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(vec![
                    Span::styled("[x] ", bold),
                    Span::styled("xAI Grok", gray),
                ]),
            );
            let _ = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(vec![Span::styled("[q] ", bold), Span::styled("Quit", gray)]),
            );
        }
        MinimalAuthHint::SigningIn {
            provider,
            url,
            code,
        } => {
            let provider_name = match provider {
                PrimaryProvider::Codex => "ChatGPT Codex",
                PrimaryProvider::Xai => "xAI Grok",
                PrimaryProvider::Kimi => "Kimi",
                PrimaryProvider::Fireworks => "Fireworks AI",
                PrimaryProvider::DeepSeek => "DeepSeek",
                PrimaryProvider::Meta => "Meta API",
                PrimaryProvider::Wafer => "Wafer AI",
                PrimaryProvider::Zai => "Z AI",
                PrimaryProvider::OpenCodeGo => "OpenCode Go",
            };
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled(format!("Sign in to {provider_name}"), bold)),
            );
            y = put_line(buf, area, y, bottom, Line::default());
            match url {
                Some(url) => {
                    y = put_line(
                        buf,
                        area,
                        y,
                        bottom,
                        Line::from(Span::styled(
                            "Open this URL in your browser to approve:",
                            gray,
                        )),
                    );
                    y = render_url(
                        buf,
                        area,
                        y,
                        bottom,
                        url,
                        Style::default().fg(theme.accent_user).bg(Color::Reset),
                    );
                    if let Some(code) = code {
                        y = put_line(buf, area, y, bottom, Line::default());
                        y = put_line(
                            buf,
                            area,
                            y,
                            bottom,
                            Line::from(vec![
                                Span::styled("Code: ", gray),
                                Span::styled(code.clone(), bold),
                            ]),
                        );
                    }
                    y = put_line(buf, area, y, bottom, Line::default());
                    let _ = put_line(
                        buf,
                        area,
                        y,
                        bottom,
                        Line::from(Span::styled("Waiting for approval\u{2026}", gray)),
                    );
                }
                None => {
                    let _ = put_line(
                        buf,
                        area,
                        y,
                        bottom,
                        Line::from(Span::styled(
                            "Opening your browser to sign in\u{2026}",
                            gray,
                        )),
                    );
                }
            }
        }
        MinimalAuthHint::Failed(err) => {
            let warn = Style::default()
                .fg(theme.warning)
                .add_modifier(Modifier::BOLD)
                .bg(Color::Reset);
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled("Sign-in failed", warn)),
            );
            y = put_line(buf, area, y, bottom, Line::default());
            let _ = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled(err.clone(), gray)),
            );
        }
        MinimalAuthHint::TrustFolder { workspace } => {
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled(
                    "Do you trust the contents of this directory?",
                    bold,
                )),
            );
            y = render_url(
                buf,
                area,
                y,
                bottom,
                &workspace.display().to_string(),
                Style::default().fg(theme.accent_user).bg(Color::Reset),
            );
            y = put_line(buf, area, y, bottom, Line::default());
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled(
                    "Open Grok may run or modify contents in this directory,",
                    gray,
                )),
            );
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled("posing security risks.", gray)),
            );
            y = put_line(buf, area, y, bottom, Line::default());
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(vec![
                    Span::styled("y", bold),
                    Span::styled("  Yes, proceed", gray),
                ]),
            );
            y = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(vec![
                    Span::styled("n", bold),
                    Span::styled("  No, quit", gray),
                ]),
            );
            y = put_line(buf, area, y, bottom, Line::default());
            let _ = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled("Enter or y to trust · n or Esc to quit", gray)),
            );
        }
        MinimalAuthHint::Starting => {
            let _ = put_line(
                buf,
                area,
                y,
                bottom,
                Line::from(Span::styled(
                    "Signing in\u{2026} starting your session.",
                    gray,
                )),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_user_code_parses_verification_url() {
        assert_eq!(
            device_user_code("https://accounts.x.ai/oauth2/device?user_code=ABCD-EFGH"),
            Some("ABCD-EFGH")
        );
        assert_eq!(
            device_user_code("https://accounts.x.ai/oauth2/device"),
            None
        );
        assert_eq!(device_user_code("https://x/device?other=1"), None);
    }

    #[test]
    fn auth_hint_maps_auth_state() {
        use xai_grok_pager::app::app_view::AuthMode;

        // Device flow → SigningIn carrying the URL and the parsed code.
        let st = AuthState::Authenticating {
            request_seq: 1,
            handle: None,
            auth_url: Some("https://accounts.x.ai/device?user_code=ABCD-EFGH".into()),
            mode: AuthMode::Device,
        };
        match minimal_auth_hint(&st, PrimaryProvider::Xai) {
            MinimalAuthHint::SigningIn { url, code, .. } => {
                assert_eq!(
                    url.as_deref(),
                    Some("https://accounts.x.ai/device?user_code=ABCD-EFGH")
                );
                assert_eq!(code.as_deref(), Some("ABCD-EFGH"));
            }
            _ => panic!("expected SigningIn"),
        }

        // External command flow with no code → SigningIn, URL but no code.
        let st = AuthState::Authenticating {
            request_seq: 2,
            handle: None,
            auth_url: Some("https://provider.example/login".into()),
            mode: AuthMode::Command,
        };
        match minimal_auth_hint(&st, PrimaryProvider::Xai) {
            MinimalAuthHint::SigningIn { url, code, .. } => {
                assert_eq!(url.as_deref(), Some("https://provider.example/login"));
                assert!(code.is_none());
            }
            _ => panic!("expected SigningIn"),
        }

        assert!(matches!(
            minimal_auth_hint(
                &AuthState::ProviderChoice { error: None },
                PrimaryProvider::Codex,
            ),
            MinimalAuthHint::ChooseProvider { error: None }
        ));
        assert!(matches!(
            minimal_auth_hint(&AuthState::Done, PrimaryProvider::Xai),
            MinimalAuthHint::Starting
        ));
        assert!(matches!(
            minimal_auth_hint(
                &AuthState::Pending {
                    error: Some("nope".into())
                },
                PrimaryProvider::Xai,
            ),
            MinimalAuthHint::Failed(_)
        ));
    }

    #[test]
    fn active_codex_model_does_not_relabel_an_xai_auth_flow() {
        let provider = minimal_auth_provider(PrimaryProvider::Codex, None);
        assert_eq!(provider, PrimaryProvider::Xai);
        let state = AuthState::Authenticating {
            request_seq: 3,
            handle: None,
            auth_url: Some("https://accounts.x.ai/oauth2/device".into()),
            mode: xai_grok_pager::app::app_view::AuthMode::Device,
        };
        let hint = minimal_auth_hint(&state, provider);
        assert!(matches!(
            hint,
            MinimalAuthHint::SigningIn {
                provider: PrimaryProvider::Xai,
                ..
            }
        ));
    }

    #[test]
    fn pending_folder_trust_is_shown_after_auth_without_losing_provider_routing() {
        let trust = TrustState::Pending {
            workspace: PathBuf::from("/tmp/untrusted-repo"),
        };
        assert!(matches!(
            minimal_auth_hint_with_trust(
                &AuthState::Done,
                PrimaryProvider::Xai,
                &trust,
                true,
                false,
            ),
            MinimalAuthHint::TrustFolder { workspace }
                if workspace == PathBuf::from("/tmp/untrusted-repo")
        ));
        assert!(matches!(
            minimal_auth_hint_with_trust(
                &AuthState::Done,
                PrimaryProvider::Xai,
                &trust,
                false,
                false,
            ),
            MinimalAuthHint::Starting
        ));
        assert!(matches!(
            minimal_auth_hint_with_trust(
                &AuthState::Done,
                PrimaryProvider::Xai,
                &trust,
                true,
                true,
            ),
            MinimalAuthHint::Starting
        ));
    }

    #[test]
    fn render_auth_shows_folder_trust_question() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 14);
        let mut buf = Buffer::empty(area);
        render_auth(
            &mut buf,
            area,
            &theme,
            &MinimalAuthHint::TrustFolder {
                workspace: PathBuf::from("/home/agent/project"),
            },
        );
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        assert!(text.contains("Do you trust the contents of this directory?"));
        assert!(text.contains("/home/agent/project"));
        assert!(text.contains("Open Grok may run or modify contents"));
        assert!(text.contains("Yes, proceed"));
        assert!(text.contains("No, quit"));
    }

    #[test]
    fn codex_session_resume_uses_explicit_codex_auth_label() {
        let provider = minimal_auth_provider(PrimaryProvider::Xai, Some(PrimaryProvider::Codex));
        let state = AuthState::Authenticating {
            request_seq: 4,
            handle: None,
            auth_url: None,
            mode: xai_grok_pager::app::app_view::AuthMode::Command,
        };
        assert!(matches!(
            minimal_auth_hint(&state, provider),
            MinimalAuthHint::SigningIn {
                provider: PrimaryProvider::Codex,
                ..
            }
        ));
    }

    #[test]
    fn render_auth_shows_url_and_code() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 12);
        let mut buf = Buffer::empty(area);
        let hint = MinimalAuthHint::SigningIn {
            provider: PrimaryProvider::Xai,
            url: Some("https://accounts.x.ai/device?user_code=ABCD-EFGH".into()),
            code: Some("ABCD-EFGH".into()),
        };
        render_auth(&mut buf, area, &theme, &hint);
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(c) = buf.cell((x, y)) {
                    text.push_str(c.symbol());
                }
            }
        }
        assert!(text.contains("Sign in to xAI Grok"), "header: {text:?}");
        assert!(text.contains("accounts.x.ai/device"), "url: {text:?}");
        assert!(text.contains("ABCD-EFGH"), "device code: {text:?}");
        assert!(
            text.contains("Waiting for approval"),
            "waiting line: {text:?}"
        );
    }

    #[test]
    fn render_auth_names_chatgpt_codex_during_codex_oauth() {
        let theme = Theme::current();
        let area = Rect::new(0, 0, 80, 6);
        let mut buf = Buffer::empty(area);
        let hint = MinimalAuthHint::SigningIn {
            provider: PrimaryProvider::Codex,
            url: None,
            code: None,
        };
        render_auth(&mut buf, area, &theme, &hint);
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        assert!(
            text.contains("Sign in to ChatGPT Codex"),
            "header: {text:?}"
        );
        assert!(!text.contains("Sign in to Grok"), "header: {text:?}");
    }
}
