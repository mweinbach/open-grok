//! Unstyled location pieces for the welcome top bar and the dashboard header.
//! The session header reuses [`worktree_badge`] and [`branch_label`] only.
//! Pure over the per-cwd git probe; nothing here spawns `git`.

use std::path::Path;

use ratatui::text::Span;

use crate::git_info;
use crate::theme::Theme;

/// `worktree ` painted one step fainter than the branch so the path stays the strongest word.
pub(crate) fn worktree_badge(theme: &Theme) -> Span<'static> {
    Span::styled("worktree ", theme.faint())
}

/// The branch as the location line shows it: git reports a detached HEAD as an empty name.
pub(crate) fn branch_label(branch: String) -> String {
    if branch.is_empty() {
        "detached".to_owned()
    } else {
        branch
    }
}

/// The unstyled pieces of a location line, so each surface can style them on its own.
#[allow(dead_code)]
pub(crate) struct LocationParts {
    /// The checked-out branch, `detached` for a detached HEAD, `None` outside a git repo.
    pub branch: Option<String>,
    pub is_worktree: bool,
    /// The abbreviated, middle-shortened cwd. Linked worktrees use [`worktree_badge`], not a path suffix.
    pub cwd_display: String,
}

/// Safe to call during render: it reads the per-cwd git cache and never blocks or spawns `git`.
#[allow(dead_code)]
pub(crate) fn location_parts(cwd: &Path) -> LocationParts {
    location_parts_from(cwd, git_info::cwd_git_info_lazy(cwd))
}

/// `info` is taken by value so the branch moves out instead of being cloned on every frame.
fn location_parts_from(cwd: &Path, info: Option<git_info::CwdGitInfo>) -> LocationParts {
    let cwd_display = crate::util::display_location_path(cwd);
    let is_worktree = info.as_ref().is_some_and(|i| i.is_worktree);
    let branch = info.and_then(|i| i.branch).map(branch_label);
    LocationParts {
        branch,
        is_worktree,
        cwd_display,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn format_cwd_plain_repo() {
        let display = crate::util::display_location_path(Path::new("/work/xai"));
        assert_eq!(display, "/work/xai");
        assert!(!display.contains("(worktree of"));
    }

    #[test]
    fn format_cwd_worktree_omits_main_repo_suffix() {
        let info = git_info::CwdGitInfo {
            branch: Some("main".into()),
            is_worktree: true,
            main_repo: Some("~/xai".into()),
            worktree_label: Some("session-1".into()),
        };
        let parts = location_parts_from(Path::new("/work/wt/session-1"), Some(info));
        assert!(parts.is_worktree);
        assert_eq!(parts.cwd_display, "/w/wt/session-1");
        assert!(!parts.cwd_display.contains("(worktree of"));
    }

    #[test]
    fn format_cwd_display_shows_subdir_not_repo_root() {
        assert_eq!(
            crate::util::display_location_path(Path::new("/work/xai/frontend/apps")),
            "/w/x/frontend/apps",
        );
    }

    #[test]
    fn format_cwd_worktree_subdir_omits_main_repo_suffix() {
        let info = git_info::CwdGitInfo {
            branch: Some("kevin/x".into()),
            is_worktree: true,
            main_repo: Some("~/xai".into()),
            worktree_label: Some("location-picker".into()),
        };
        let parts = location_parts_from(Path::new("/work/wt/location-picker/frontend"), Some(info));
        assert!(parts.is_worktree);
        assert_eq!(parts.cwd_display, "/w/w/location-picker/frontend");
        assert!(!parts.cwd_display.contains("(worktree of"));
    }

    #[test]
    fn location_parts_from_maps_each_probe_outcome() {
        let cwd = Path::new("/work/wt/feature");
        let probe = |branch: Option<&str>, is_worktree: bool| git_info::CwdGitInfo {
            branch: branch.map(str::to_owned),
            is_worktree,
            main_repo: is_worktree.then(|| "~/xai".to_owned()),
            worktree_label: None,
        };

        let named = location_parts_from(cwd, Some(probe(Some("main"), false)));
        assert_eq!(named.branch.as_deref(), Some("main"));
        assert!(!named.is_worktree);
        assert_eq!(named.cwd_display, "/w/wt/feature");

        let detached = location_parts_from(cwd, Some(probe(Some(""), true)));
        assert_eq!(detached.branch.as_deref(), Some("detached"));
        assert!(detached.is_worktree);

        let no_head = location_parts_from(cwd, Some(probe(None, false)));
        assert_eq!(no_head.branch, None);

        let miss = location_parts_from(cwd, None);
        assert_eq!(miss.branch, None);
        assert!(!miss.is_worktree);
        assert_eq!(miss.cwd_display, "/w/wt/feature");
    }

    #[test]
    fn collapse_home_does_not_eat_neighbor_profile() {
        let Some(home) = git_info::home_dir() else {
            return;
        };
        if home.is_empty() {
            return;
        }
        let neighbor = PathBuf::from(format!("{home}bar")).join("src");
        let collapsed_neighbor = crate::util::display_location_path(&neighbor);
        assert!(
            !collapsed_neighbor.starts_with('~'),
            "string prefix would produce ~bar/src, got {collapsed_neighbor}"
        );

        let child = PathBuf::from(&home).join("src");
        let expected = format!("~/{}", Path::new("src").display());
        assert_eq!(crate::util::display_location_path(&child), expected);
    }
}
