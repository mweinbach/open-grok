//! `/add-dir` working-directory mutations for `SessionActor`.
//!
//! A working directory added mid-session widens the session's file scope
//! alongside the spawn cwd: file Read/Edit permission allow rules are
//! appended for the canonicalized directory (deny/ask rules still outrank
//! them), the set is persisted to `working_dirs.json` for resume, and the
//! model is told about the change through an append-only
//! `<environment-update>` reminder so the prompt-cache prefix stays stable.
//!
//! The session cwd remains the base for relative paths; added directories
//! are extra roots, not a replacement (that is `/cd`'s dashboard job).

use super::*;
use crate::session::commands::WorkingDirectoryChange;
use std::path::{Path, PathBuf};

/// Source tag for working-set environment updates. Shares the
/// `ENVIRONMENT_UPDATE_OPEN` marker so the model treats it like the other
/// append-only environment corrections.
const WORKING_SET_SOURCE: &str = "working_set";

/// Expand a leading `~` (alone or `~/…`) to the user's home directory.
/// Anything else is returned unchanged.
fn expand_home(path: &Path) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path.to_path_buf();
    };
    if text == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

/// Read `working_dirs.json` from a session directory. Absent file (session
/// pre-dates the feature) or corrupt content yields an empty list — the
/// feature fails open to "no additional directories", never blocks spawn.
pub(super) fn read_working_dirs_file(session_dir: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read(session_dir.join(crate::session::storage::WORKING_DIRS_FILE))
    else {
        return Vec::new();
    };
    match serde_json::from_slice(&bytes) {
        Ok(dirs) => dirs,
        Err(e) => {
            tracing::warn!(?e, "failed to parse working_dirs.json; ignoring");
            Vec::new()
        }
    }
}

/// Atomically replace `working_dirs.json` (single writer: the actor).
fn write_working_dirs_file(session_dir: &Path, dirs: &[PathBuf]) {
    let Ok(json) = serde_json::to_vec_pretty(dirs) else {
        return;
    };
    if let Err(e) = crate::session::storage::write_bytes_atomic(
        &session_dir.join(crate::session::storage::WORKING_DIRS_FILE),
        &json,
    ) {
        tracing::warn!(?e, "failed to write working_dirs.json");
    }
}

/// Build the model-facing working-set reminder for a mutation. Lists the
/// full resulting set so a stale earlier reminder never misleads.
fn format_working_set_update(cwd: &str, dirs: &[PathBuf]) -> String {
    let listing = if dirs.is_empty() {
        "None. The session working directory is the only in-scope root.".to_string()
    } else {
        dirs.iter()
            .map(|d| format!("- {}", d.display()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{ENVIRONMENT_UPDATE_OPEN} source=\"{WORKING_SET_SOURCE}\">\n\
         The session's working directories changed. The session working directory \
         ({cwd}) stays the base for relative paths. Additional working directories \
         the user granted file Read/Edit scope for, usable alongside it:\n\
         {listing}\n\
         </environment-update>"
    )
}

impl SessionActor {
    /// Canonicalize and validate a user-supplied directory. Relative paths
    /// resolve against the session cwd (the `/add-dir` UX default); `~`
    /// expands to the user's home.
    fn resolve_working_directory(&self, path: &Path) -> Result<PathBuf, String> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(&self.session_info.cwd).join(path)
        };
        let expanded = expand_home(&joined);
        let canonical = dunce::canonicalize(&expanded)
            .map_err(|e| format!("cannot access {}: {e}", expanded.display()))?;
        if !canonical.is_dir() {
            return Err(format!("not a directory: {}", canonical.display()));
        }
        let cwd = dunce::canonicalize(&self.session_info.cwd)
            .unwrap_or_else(|_| PathBuf::from(&self.session_info.cwd));
        if canonical == cwd {
            return Err(format!(
                "{} is already the session working directory",
                canonical.display()
            ));
        }
        Ok(canonical)
    }

    /// Apply the permission scope for a directory set: one rule batch per
    /// directory. Subagents inherit these automatically through the shared
    /// `PermissionHandle`.
    fn grant_working_directory_rules(&self, dirs: &[PathBuf]) {
        let rules: Vec<_> = dirs
            .iter()
            .flat_map(|dir| xai_grok_workspace::permission::rules::working_directory_rules(dir))
            .collect();
        if !rules.is_empty() {
            self.permissions.add_session_rules(rules);
        }
    }

    /// Persist the set and disclose it to the model. `disclose` is false on
    /// the resume path — the persisted conversation already carries the
    /// reminder history.
    fn persist_and_disclose_working_dirs(&self, dirs: &[PathBuf], disclose: bool) {
        let session_dir = crate::session::persistence::session_dir(&self.session_info);
        write_working_dirs_file(&session_dir, dirs);
        if disclose {
            self.push_system_reminder(&format_working_set_update(&self.session_info.cwd, dirs));
        }
    }

    /// Restore persisted working directories at spawn: repopulate actor
    /// state and re-grant the permission scope. No disclosure — the
    /// conversation already contains the history.
    pub(super) fn restore_working_dirs(&self, dirs: Vec<PathBuf>) {
        if dirs.is_empty() {
            return;
        }
        *self.additional_working_dirs.borrow_mut() = dirs.clone();
        self.grant_working_directory_rules(&dirs);
    }

    /// `SessionCommand::AddWorkingDirectory` handler.
    pub(super) fn handle_add_working_directory(
        &self,
        path: &Path,
        respond_to: oneshot::Sender<Result<WorkingDirectoryChange, String>>,
    ) {
        let reply = self.add_working_directory_inner(path);
        let _ = respond_to.send(reply);
    }

    fn add_working_directory_inner(&self, path: &Path) -> Result<WorkingDirectoryChange, String> {
        let canonical = self.resolve_working_directory(path)?;
        let mut dirs = self.additional_working_dirs.borrow_mut();
        if dirs.contains(&canonical) {
            return Ok(WorkingDirectoryChange {
                changed: false,
                directories: dirs.clone(),
            });
        }
        self.grant_working_directory_rules(std::slice::from_ref(&canonical));
        dirs.push(canonical);
        let out = dirs.clone();
        drop(dirs);
        self.persist_and_disclose_working_dirs(&out, true);
        tracing::info!(
            session_id = %self.session_info.id.0,
            directory = %out.last().unwrap().display(),
            "Added working directory"
        );
        Ok(WorkingDirectoryChange {
            changed: true,
            directories: out,
        })
    }

    /// `SessionCommand::RemoveWorkingDirectory` handler. Accepts the
    /// canonical path or any spelling that resolves to it.
    pub(super) fn handle_remove_working_directory(
        &self,
        path: &Path,
        respond_to: oneshot::Sender<Result<WorkingDirectoryChange, String>>,
    ) {
        let reply = self.remove_working_directory_inner(path);
        let _ = respond_to.send(reply);
    }

    fn remove_working_directory_inner(
        &self,
        path: &Path,
    ) -> Result<WorkingDirectoryChange, String> {
        let canonical = match self.resolve_working_directory(path) {
            Ok(canonical) => Some(canonical),
            // Removing something that no longer exists on disk must still
            // work — match by canonical-equivalent spelling, else by exact
            // stored value, else report unknown.
            Err(_) => path
                .is_absolute()
                .then(|| path.to_path_buf())
                .filter(|candidate| self.additional_working_dirs.borrow().contains(candidate)),
        };
        let Some(canonical) = canonical else {
            return Ok(WorkingDirectoryChange {
                changed: false,
                directories: self.additional_working_dirs.borrow().clone(),
            });
        };
        let mut dirs = self.additional_working_dirs.borrow_mut();
        let before = dirs.len();
        dirs.retain(|d| *d != canonical);
        if dirs.len() == before {
            return Ok(WorkingDirectoryChange {
                changed: false,
                directories: dirs.clone(),
            });
        }
        let out = dirs.clone();
        drop(dirs);
        // Session rules cannot be removed individually, so rebuild the scope
        // from the remaining set. The stale allows for the removed directory
        // are dead entries until the process restarts; they only ever ALLOW
        // file access to a directory the user had granted this session.
        self.permissions.reset_session_rules();
        self.grant_working_directory_rules(&out);
        self.persist_and_disclose_working_dirs(&out, true);
        tracing::info!(
            session_id = %self.session_info.id.0,
            directory = %canonical.display(),
            "Removed working directory"
        );
        Ok(WorkingDirectoryChange {
            changed: true,
            directories: out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_set_update_lists_directories() {
        let text = format_working_set_update(
            "/repo",
            &[PathBuf::from("/other/alpha"), PathBuf::from("/shared/beta")],
        );
        assert!(text.starts_with("<environment-update source=\"working_set\">"));
        assert!(text.contains("/other/alpha"));
        assert!(text.contains("/shared/beta"));
        assert!(text.contains("/repo"));
        assert!(text.ends_with("</environment-update>"));
    }

    #[test]
    fn working_set_update_empty_set_is_explicit() {
        let text = format_working_set_update("/repo", &[]);
        assert!(text.contains("only in-scope root"));
    }
}
