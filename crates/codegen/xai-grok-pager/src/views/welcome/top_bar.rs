//! Top bar component — renders cwd and git info.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use std::path::{Path, PathBuf};

use crate::git_info;
use crate::render::line_utils::truncate_line;
use crate::theme::Theme;

pub fn render_top_bar(
    area: Rect,
    buf: &mut Buffer,
    theme: &Theme,
    announcement: Option<&xai_grok_announcements::RemoteAnnouncement>,
) {
    let line = truncate_line(location_line(theme), area.width as usize);
    let line_width = line.width() as u16;
    buf.set_line(area.x, area.y, &line, line_width.min(area.width));

    if let Some(a) = announcement
        && let Some(text) = a.message.as_deref()
        && area.height > 1
    {
        let text_style = Style::default().fg(theme.text_primary);
        let line = Line::from(Span::styled(text, text_style));
        Paragraph::new(line).render(
            Rect {
                y: area.y + 1,
                height: area.height.saturating_sub(1),
                ..area
            },
            buf,
        );
    }
}

/// Build the `{git branch} {worktree} {cwd}` line for the welcome top bar,
/// reading the live process cwd.
pub(crate) fn location_line(theme: &Theme) -> Line<'static> {
    location_line_at(theme, &process_cwd())
}

/// As [`location_line`], but for an explicit `cwd`. The dashboard header
/// passes its staged `app.cwd` so the line tracks a `/cd` immediately,
/// before (or even if) `Effect::SetWorkingDir` moves the process cwd.
///
/// Render-safe: reads the per-cwd git cache; never blocks or spawns `git`.
/// The caller width-truncates the returned line.
pub(crate) fn location_line_at(theme: &Theme, cwd: &Path) -> Line<'static> {
    let info_style = Style::default().fg(theme.gray);

    let info = git_info::cwd_git_info_lazy(cwd);

    let mut parts: Vec<Span> = Vec::new();
    if let Some(branch) = info.as_ref().and_then(|i| i.branch.as_deref()) {
        let icon = git_info::branch_icon();
        let git_text = if branch.is_empty() {
            format!("{icon} detached")
        } else {
            format!("{icon} {branch}")
        };
        let git_style = Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::DIM);
        parts.push(Span::styled(git_text, git_style));
        parts.push(Span::styled(" ", info_style));
    }
    // Worktree badge — matches the session status bar's `worktree ` marker
    // (accent_user) before the path when the cwd is a linked worktree.
    if info.as_ref().is_some_and(|i| i.is_worktree) {
        parts.push(Span::styled(
            "worktree ",
            Style::default().fg(theme.accent_user),
        ));
    }
    let cwd_display = format_cwd_display(cwd, info.as_ref());
    let cwd_style = Style::default().fg(theme.gray_dim);
    parts.push(Span::styled(cwd_display, cwd_style));
    Line::from(parts)
}

fn process_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Format the cwd for the welcome top bar / dashboard header: last-two-component
/// shortening after `~` collapse. Linked worktrees use the `worktree ` badge,
/// not a `(worktree of …)` suffix.
///
/// Pure formatting over the per-cwd git probe — never spawns `git`.
fn format_cwd_display(cwd: &Path, _info: Option<&git_info::CwdGitInfo>) -> String {
    crate::util::display_location_path(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_cwd_plain_repo() {
        assert_eq!(
            format_cwd_display(Path::new("/work/xai"), None),
            "/work/xai"
        );
        assert!(!format_cwd_display(Path::new("/work/xai"), None).contains("(worktree of"));
    }

    /// A linked worktree keeps the `worktree ` badge and drops the main-repo suffix.
    #[test]
    fn format_cwd_worktree_omits_main_repo_suffix() {
        let info = git_info::CwdGitInfo {
            branch: Some("main".into()),
            is_worktree: true,
            main_repo: Some("~/xai".into()),
            worktree_label: Some("session-1".into()),
        };
        let display = format_cwd_display(Path::new("/work/wt/session-1"), Some(&info));
        assert_eq!(display, "/w/wt/session-1");
        assert!(!display.contains("(worktree of"));
    }

    /// The header shows the ACTUAL cwd, not the git repo root: switching
    /// into a subdirectory of a repo reflects the subdirectory, shortened.
    #[test]
    fn format_cwd_display_shows_subdir_not_repo_root() {
        let info = git_info::CwdGitInfo {
            branch: Some("main".into()),
            is_worktree: false,
            main_repo: None,
            worktree_label: None,
        };
        assert_eq!(
            format_cwd_display(Path::new("/work/xai/frontend/apps"), Some(&info)),
            "/w/x/frontend/apps",
        );
    }

    /// A worktree subdirectory still shows the real subdirectory path with no
    /// main-repo suffix.
    #[test]
    fn format_cwd_display_worktree_subdir_omits_main_repo_suffix() {
        let info = git_info::CwdGitInfo {
            branch: Some("kevin/x".into()),
            is_worktree: true,
            main_repo: Some("~/xai".into()),
            worktree_label: Some("location-picker".into()),
        };
        let display =
            format_cwd_display(Path::new("/work/wt/location-picker/frontend"), Some(&info));
        assert_eq!(display, "/w/w/location-picker/frontend");
        assert!(!display.contains("(worktree of"));
    }

    /// On a cache miss (`info == None`) the header still shows the shortened cwd.
    #[test]
    fn format_cwd_display_cache_miss_shows_raw_cwd() {
        assert_eq!(
            format_cwd_display(Path::new("/work/xai/frontend/apps"), None),
            "/w/x/frontend/apps",
        );
    }
}
