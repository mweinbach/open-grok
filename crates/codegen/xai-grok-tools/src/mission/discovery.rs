//! Discovery of Open Grok and Factory Droid missions across the filesystem.
//!
//! Scans canonical Open Grok locations (`$OPENGROK_HOME/missions` and `.opengrok/missions`)
//! as well as Factory Droid (`~/.factory/missions`), presenting a unified catalog of
//! active, paused, and completed missions.

use crate::mission::storage::MissionFileService;
use crate::mission::types::{FeatureStatus, MissionState};
use std::path::{Path, PathBuf};

/// Origin source of a mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionSource {
    /// Native Open Grok mission (`$OPENGROK_HOME/missions` or project `.opengrok/missions`).
    OpenGrok,
    /// Factory Droid mission (`~/.factory/missions`).
    FactoryDroid,
}

impl std::fmt::Display for MissionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenGrok => write!(f, "Open Grok"),
            Self::FactoryDroid => write!(f, "Factory Droid"),
        }
    }
}

/// High-level summary of a discovered mission.
#[derive(Debug, Clone)]
pub struct MissionSummary {
    /// Directory identifier / session ID (e.g. "6542d338-44c1-43e7-98e7-746950896862").
    pub id: String,
    /// Internal mission ID if set in `state.json` (e.g. "mis_24348f5c").
    pub mission_id: Option<String>,
    /// Title from `mission.md` or fallback to ID.
    pub title: String,
    /// Target repository path.
    pub working_directory: Option<String>,
    /// Source of the mission.
    pub source: MissionSource,
    /// Absolute path to the mission directory.
    pub dir: PathBuf,
    /// Current mission state.
    pub state: MissionState,
    /// Total features defined.
    pub total_features: usize,
    /// Completed features.
    pub completed_features: usize,
    /// Currently in-progress features.
    pub in_progress_features: usize,
    /// Pending features.
    pub pending_features: usize,
    /// ISO 8601 created timestamp.
    pub created_at: Option<String>,
    /// ISO 8601 updated timestamp.
    pub updated_at: Option<String>,
}

/// Returns the canonical Open Grok missions directory (`$OPENGROK_HOME/missions` or `~/.opengrok/missions`).
pub fn opengrok_missions_dir() -> PathBuf {
    if let Some(val) = std::env::var_os("OPENGROK_HOME") {
        PathBuf::from(val).join("missions")
    } else if let Some(home) = dirs::home_dir() {
        home.join(".opengrok").join("missions")
    } else {
        PathBuf::from(".opengrok").join("missions")
    }
}

/// Returns the Factory Droid missions directory (`~/.factory/missions`) if home dir is resolved.
pub fn factory_droid_missions_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".factory").join("missions"))
}

/// Discover all missions across Open Grok and Factory Droid installations.
pub fn discover_all_missions() -> Vec<MissionSummary> {
    let mut summaries = Vec::new();

    // 1. Open Grok canonical dir
    let grok_dir = opengrok_missions_dir();
    if grok_dir.is_dir() {
        scan_missions_in_dir(&grok_dir, MissionSource::OpenGrok, &mut summaries);
    }

    // 2. Factory Droid dir
    if let Some(droid_dir) = factory_droid_missions_dir() {
        if droid_dir.is_dir() {
            scan_missions_in_dir(&droid_dir, MissionSource::FactoryDroid, &mut summaries);
        }
    }

    // Sort by updated_at descending (most recent first)
    summaries.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    summaries
}

/// Discover missions matching a specific working directory workspace.
pub fn discover_missions_for_workspace(workspace_root: &Path) -> Vec<MissionSummary> {
    let canonical_ws = workspace_root.canonicalize().unwrap_or_else(|_| workspace_root.to_path_buf());
    discover_all_missions()
        .into_iter()
        .filter(|m| {
            if let Some(wd) = &m.working_directory {
                let p = Path::new(wd);
                let can = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                can == canonical_ws
            } else {
                false
            }
        })
        .collect()
}

/// Find a specific mission by directory name, ID, or prefix.
pub fn find_mission(query: &str) -> Option<MissionSummary> {
    let all = discover_all_missions();
    let q = query.trim().to_lowercase();

    // 1. Exact match on directory ID
    if let Some(m) = all.iter().find(|m| m.id.to_lowercase() == q) {
        return Some(m.clone());
    }

    // 2. Exact match on internal mission_id
    if let Some(m) = all.iter().find(|m| m.mission_id.as_deref().map(str::to_lowercase) == Some(q.clone())) {
        return Some(m.clone());
    }

    // 3. Prefix match on directory ID or mission_id
    if let Some(m) = all.iter().find(|m| {
        m.id.to_lowercase().starts_with(&q)
            || m.mission_id.as_deref().map(|id| id.to_lowercase().starts_with(&q)).unwrap_or(false)
    }) {
        return Some(m.clone());
    }

    // 4. Substring match on title
    if let Some(m) = all.iter().find(|m| m.title.to_lowercase().contains(&q)) {
        return Some(m.clone());
    }

    None
}

fn scan_missions_in_dir(parent_dir: &Path, source: MissionSource, out: &mut Vec<MissionSummary>) {
    let entries = match std::fs::read_dir(parent_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let svc = MissionFileService::new(&path);
        if !svc.exists() {
            continue;
        }

        let id = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let state_file = svc.read_state().ok();
        let title = svc
            .read_mission_title()
            .or_else(|| state_file.as_ref().and_then(|s| (!s.mission_id.is_empty()).then(|| s.mission_id.clone())))
            .unwrap_or_else(|| id.clone());

        let working_dir = svc
            .read_working_directory()
            .ok()
            .or_else(|| state_file.as_ref().map(|s| s.working_directory.clone()));

        let state = state_file.as_ref().map(|s| s.state).unwrap_or(MissionState::AwaitingInput);
        let created_at = state_file.as_ref().map(|s| s.created_at.clone());
        let updated_at = state_file.as_ref().map(|s| s.updated_at.clone());
        let mission_id = state_file.as_ref().map(|s| s.mission_id.clone());

        let (total, completed, in_progress, pending) = if let Ok(features_file) = svc.read_features() {
            let total = features_file.features.len();
            let completed = features_file.features.iter().filter(|f| f.status == FeatureStatus::Completed).count();
            let in_prog = features_file.features.iter().filter(|f| f.status == FeatureStatus::InProgress).count();
            let pend = features_file.features.iter().filter(|f| f.status == FeatureStatus::Pending).count();
            (total, completed, in_prog, pend)
        } else {
            (0, 0, 0, 0)
        };

        out.push(MissionSummary {
            id,
            mission_id,
            title,
            working_directory: working_dir,
            source,
            dir: path,
            state,
            total_features: total,
            completed_features: completed,
            in_progress_features: in_progress,
            pending_features: pending,
            created_at,
            updated_at,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_scan_real_factory_droid_mission_if_exists() {
        if let Some(droid_dir) = factory_droid_missions_dir() {
            if droid_dir.is_dir() {
                let mut summaries = Vec::new();
                scan_missions_in_dir(&droid_dir, MissionSource::FactoryDroid, &mut summaries);
                // On this machine, ~/.factory/missions has at least 1 mission
                assert!(!summaries.is_empty(), "Should discover existing Factory Droid missions");
                for s in &summaries {
                    assert_eq!(s.source, MissionSource::FactoryDroid);
                    assert!(!s.id.is_empty());
                    if s.id == "6542d338-44c1-43e7-98e7-746950896862" {
                        assert_eq!(s.total_features, 116);
                        assert_eq!(s.state, MissionState::Paused);
                        assert_eq!(s.working_directory.as_deref(), Some("/Users/mweinbach/Projects/agent-coworker"));
                    }
                }
            }
        }
    }
}
