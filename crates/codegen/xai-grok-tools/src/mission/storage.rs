//! Mission file storage service.
//!
//! Manages reads, writes, and updates to mission directories (`state.json`,
//! `features.json`, `progress_log.jsonl`, `mission.md`, `handoffs/`, `skills/`),
//! strictly conforming to Factory Droid conventions.

use crate::mission::types::{
    Feature, FeatureStatus, FeaturesFile, MissionModelSettings, MissionState,
    MissionStateFile, ProgressLogEntry, SavedWorkerHandoff, WorkerHandoff,
};
use anyhow::{Context, Result, anyhow};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const STATE_FILENAME: &str = "state.json";
pub const WORKING_DIR_FILENAME: &str = "working_directory.txt";
pub const MISSION_MD_FILENAME: &str = "mission.md";
pub const FEATURES_FILENAME: &str = "features.json";
pub const PROGRESS_LOG_FILENAME: &str = "progress_log.jsonl";
pub const MODEL_SETTINGS_FILENAME: &str = "model-settings.json";
pub const HANDOFFS_DIR: &str = "handoffs";
pub const SKILLS_DIR: &str = "skills";

pub const SCRUTINY_VALIDATOR_SKILL: &str = "scrutiny-validator";
pub const USER_TESTING_VALIDATOR_SKILL: &str = "user-testing-validator";

/// Manages mission disk artifacts for a specific mission directory.
#[derive(Debug, Clone)]
pub struct MissionFileService {
    mission_dir: PathBuf,
}

impl MissionFileService {
    pub fn new(mission_dir: impl Into<PathBuf>) -> Self {
        Self {
            mission_dir: mission_dir.into(),
        }
    }

    pub fn mission_dir(&self) -> &Path {
        &self.mission_dir
    }

    pub fn state_path(&self) -> PathBuf {
        self.mission_dir.join(STATE_FILENAME)
    }

    pub fn working_directory_path(&self) -> PathBuf {
        self.mission_dir.join(WORKING_DIR_FILENAME)
    }

    pub fn mission_md_path(&self) -> PathBuf {
        self.mission_dir.join(MISSION_MD_FILENAME)
    }

    pub fn features_path(&self) -> PathBuf {
        self.mission_dir.join(FEATURES_FILENAME)
    }

    pub fn progress_log_path(&self) -> PathBuf {
        self.mission_dir.join(PROGRESS_LOG_FILENAME)
    }

    pub fn model_settings_path(&self) -> PathBuf {
        self.mission_dir.join(MODEL_SETTINGS_FILENAME)
    }

    pub fn handoffs_dir(&self) -> PathBuf {
        self.mission_dir.join(HANDOFFS_DIR)
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.mission_dir.join(SKILLS_DIR)
    }

    pub fn exists(&self) -> bool {
        self.state_path().exists() || self.mission_md_path().exists()
    }

    /// Ensure the mission directory and subdirectories (`handoffs/`, `skills/`) exist.
    pub fn initialize_mission_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.mission_dir)
            .with_context(|| format!("Failed to create mission dir {:?}", self.mission_dir))?;
        fs::create_dir_all(self.handoffs_dir())
            .with_context(|| format!("Failed to create handoffs dir {:?}", self.handoffs_dir()))?;
        fs::create_dir_all(self.skills_dir())
            .with_context(|| format!("Failed to create skills dir {:?}", self.skills_dir()))?;
        Ok(())
    }

    // --- State ---

    pub fn read_state(&self) -> Result<MissionStateFile> {
        let content = fs::read_to_string(self.state_path())
            .with_context(|| format!("Failed to read state from {:?}", self.state_path()))?;
        let state: MissionStateFile = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse state from {:?}", self.state_path()))?;
        Ok(state)
    }

    pub fn write_state(&self, state: &MissionStateFile) -> Result<()> {
        let content = serde_json::to_string_pretty(state)
            .context("Failed to serialize MissionStateFile")?;
        fs::write(self.state_path(), content)
            .with_context(|| format!("Failed to write state to {:?}", self.state_path()))?;
        Ok(())
    }

    pub fn update_state_status(&self, new_state: MissionState) -> Result<MissionStateFile> {
        let mut state = self.read_state()?;
        state.state = new_state;
        state.updated_at = chrono::Utc::now().to_rfc3339();
        self.write_state(&state)?;
        Ok(state)
    }

    // --- Working Directory ---

    pub fn read_working_directory(&self) -> Result<String> {
        if let Ok(content) = fs::read_to_string(self.working_directory_path()) {
            let trimmed = content.trim().to_string();
            if !trimmed.is_empty() {
                return Ok(trimmed);
            }
        }
        if let Ok(state) = self.read_state() {
            if !state.working_directory.is_empty() {
                return Ok(state.working_directory);
            }
        }
        Err(anyhow!("No working directory configured for mission"))
    }

    pub fn write_working_directory(&self, path: &str) -> Result<()> {
        fs::write(self.working_directory_path(), path.trim())
            .with_context(|| format!("Failed to write working directory to {:?}", self.working_directory_path()))?;
        Ok(())
    }

    // --- Mission.md ---

    pub fn read_mission_md(&self) -> Result<String> {
        fs::read_to_string(self.mission_md_path())
            .with_context(|| format!("Failed to read mission md from {:?}", self.mission_md_path()))
    }

    pub fn write_mission_md(&self, title: &str, body: &str) -> Result<()> {
        let content = format!("# {}\n\n{}", title.trim(), body.trim());
        fs::write(self.mission_md_path(), content)
            .with_context(|| format!("Failed to write mission md to {:?}", self.mission_md_path()))?;
        Ok(())
    }

    pub fn read_mission_title(&self) -> Option<String> {
        let content = fs::read_to_string(self.mission_md_path()).ok()?;
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("# ") {
                let t = rest.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        None
    }

    // --- Features ---

    pub fn read_features(&self) -> Result<FeaturesFile> {
        let content = fs::read_to_string(self.features_path())
            .with_context(|| format!("Failed to read features from {:?}", self.features_path()))?;
        let features: FeaturesFile = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse features from {:?}", self.features_path()))?;
        Ok(features)
    }

    pub fn write_features(&self, file: &FeaturesFile) -> Result<()> {
        let content = serde_json::to_string_pretty(file)
            .context("Failed to serialize FeaturesFile")?;
        fs::write(self.features_path(), content)
            .with_context(|| format!("Failed to write features to {:?}", self.features_path()))?;
        Ok(())
    }

    pub fn get_in_progress_feature(&self) -> Result<Option<Feature>> {
        let file = self.read_features()?;
        Ok(file.features.into_iter().find(|f| f.status == FeatureStatus::InProgress))
    }

    pub fn get_next_pending_feature(&self) -> Result<Option<Feature>> {
        let file = self.read_features()?;
        Ok(file.features.into_iter().find(|f| f.status == FeatureStatus::Pending))
    }

    pub fn update_feature(&self, updated: &Feature) -> Result<()> {
        let mut file = self.read_features()?;
        let mut found = false;
        for f in &mut file.features {
            if f.id == updated.id {
                *f = updated.clone();
                found = true;
                break;
            }
        }
        if !found {
            file.features.push(updated.clone());
        }
        self.write_features(&file)?;
        Ok(())
    }

    /// Insert a feature at the very top of the queue.
    /// If there was an in-progress feature, callers should revert it to pending first.
    pub fn insert_feature_at_top(&self, feature: Feature) -> Result<()> {
        let mut file = self.read_features().unwrap_or(FeaturesFile { features: Vec::new() });
        file.features.retain(|f| f.id != feature.id);
        file.features.insert(0, feature);
        self.write_features(&file)?;
        Ok(())
    }

    /// Move stranded completed features to the bottom of the features list, keeping pending at top.
    pub fn move_stranded_done_features_to_bottom(&self) -> Result<()> {
        let mut file = self.read_features()?;
        let mut pending = Vec::new();
        let mut in_progress = Vec::new();
        let mut completed = Vec::new();

        for f in file.features {
            match f.status {
                FeatureStatus::Pending => pending.push(f),
                FeatureStatus::InProgress => in_progress.push(f),
                FeatureStatus::Completed => completed.push(f),
            }
        }

        let mut reordered = in_progress;
        reordered.extend(pending);
        reordered.extend(completed);

        file.features = reordered;
        self.write_features(&file)?;
        Ok(())
    }

    pub fn are_all_features_completed(&self) -> Result<bool> {
        let file = self.read_features()?;
        if file.features.is_empty() {
            return Ok(false);
        }
        Ok(file.features.iter().all(|f| f.status == FeatureStatus::Completed))
    }

    // --- Milestone & Validation ---

    pub fn get_milestone_features(&self, milestone: &str) -> Result<Vec<Feature>> {
        let file = self.read_features()?;
        Ok(file
            .features
            .into_iter()
            .filter(|f| f.milestone == milestone)
            .collect())
    }

    /// Check if all implementation features (non-validators) in a milestone are completed.
    pub fn is_milestone_implementation_complete(&self, milestone: &str) -> Result<bool> {
        let features = self.get_milestone_features(milestone)?;
        let impl_features: Vec<_> = features
            .into_iter()
            .filter(|f| {
                f.skill_name != SCRUTINY_VALIDATOR_SKILL && f.skill_name != USER_TESTING_VALIDATOR_SKILL
            })
            .collect();

        if impl_features.is_empty() {
            return Ok(false);
        }
        Ok(impl_features.iter().all(|f| f.status == FeatureStatus::Completed))
    }

    /// Auto-inject milestone validation features if all implementation features are completed.
    pub fn check_and_inject_milestone_validation(&self, milestone: &str) -> Result<bool> {
        let settings = self.read_model_settings().unwrap_or_default();
        if !self.is_milestone_implementation_complete(milestone)? {
            return Ok(false);
        }

        let mut file = self.read_features()?;
        let mut injected = false;

        // 1. Scrutiny validator
        if !settings.skip_scrutiny {
            let scrutiny_id = format!("{}-scrutiny-validation", milestone);
            if !file.features.iter().any(|f| f.id == scrutiny_id) {
                file.features.push(Feature {
                    id: scrutiny_id,
                    description: format!("Programmatic scrutiny and verification audit for milestone {}", milestone),
                    skill_name: SCRUTINY_VALIDATOR_SKILL.to_string(),
                    preconditions: vec![format!("All implementation features for milestone {} are completed", milestone)],
                    expected_behavior: vec!["Run complete test suites, lint, and static verification gates without regressions".to_string()],
                    fulfills: Vec::new(),
                    milestone: milestone.to_string(),
                    status: FeatureStatus::Pending,
                    worker_session_ids: Vec::new(),
                    current_worker_session_id: None,
                    completed_worker_session_id: None,
                });
                injected = true;
            }
        }

        // 2. User testing validator
        if !settings.skip_user_testing {
            let user_testing_id = format!("{}-user-testing-validation", milestone);
            if !file.features.iter().any(|f| f.id == user_testing_id) {
                file.features.push(Feature {
                    id: user_testing_id,
                    description: format!("Interactive user-testing and flow validation for milestone {}", milestone),
                    skill_name: USER_TESTING_VALIDATOR_SKILL.to_string(),
                    preconditions: vec![format!("Scrutiny validation for milestone {} is passing", milestone)],
                    expected_behavior: vec!["Validate real user workflows, CLI commands, and UI interactions".to_string()],
                    fulfills: Vec::new(),
                    milestone: milestone.to_string(),
                    status: FeatureStatus::Pending,
                    worker_session_ids: Vec::new(),
                    current_worker_session_id: None,
                    completed_worker_session_id: None,
                });
                injected = true;
            }
        }

        if injected {
            self.write_features(&file)?;
        }
        Ok(injected)
    }

    // --- Progress Log ---

    pub fn read_progress_log(&self) -> Result<Vec<ProgressLogEntry>> {
        let path = self.progress_log_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&path)
            .with_context(|| format!("Failed to open progress log at {:?}", path))?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for (i, line) in reader.lines().enumerate() {
            let l = line?;
            let trimmed = l.trim();
            if trimmed.is_empty() {
                continue;
            }
            let entry: ProgressLogEntry = serde_json::from_str(trimmed)
                .with_context(|| format!("Failed to parse progress log entry at line {} in {:?}", i + 1, path))?;
            entries.push(entry);
        }
        Ok(entries)
    }

    pub fn append_progress_log(&self, entry: &ProgressLogEntry) -> Result<()> {
        let path = self.progress_log_path();
        let serialized = serde_json::to_string(entry)
            .context("Failed to serialize ProgressLogEntry")?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open progress log for append at {:?}", path))?;
        writeln!(file, "{}", serialized)
            .with_context(|| format!("Failed to append to progress log at {:?}", path))?;
        Ok(())
    }

    // --- Handoffs ---

    pub fn write_worker_handoff(&self, handoff: &SavedWorkerHandoff) -> Result<PathBuf> {
        let dir = self.handoffs_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create handoffs dir at {:?}", dir))?;

        // Format: <timestamp>__<featureId>__<workerSessionId>.json
        let safe_timestamp = handoff.timestamp.replace(':', "-");
        let filename = format!("{}__{}__{}.json", safe_timestamp, handoff.feature_id, handoff.worker_session_id);
        let path = dir.join(filename);

        let content = serde_json::to_string_pretty(handoff)
            .context("Failed to serialize SavedWorkerHandoff")?;
        fs::write(&path, content)
            .with_context(|| format!("Failed to write handoff file to {:?}", path))?;
        Ok(path)
    }

    pub fn read_worker_handoff(&self, worker_session_id: &str) -> Result<Option<WorkerHandoff>> {
        let dir = self.handoffs_dir();
        if dir.exists() {
            if let Ok(entries) = fs::read_dir(&dir) {
                let mut matches = Vec::new();
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".json") && name.contains(worker_session_id) {
                        matches.push(entry.path());
                    }
                }
                matches.sort();
                if let Some(latest) = matches.last() {
                    let content = fs::read_to_string(latest)?;
                    let saved: SavedWorkerHandoff = serde_json::from_str(&content)?;
                    return Ok(Some(saved.handoff));
                }
            }
        }

        // Fallback: check progress log
        let logs = self.read_progress_log()?;
        for entry in logs.into_iter().rev() {
            if let ProgressLogEntry::WorkerCompleted {
                worker_session_id: sid,
                handoff,
                ..
            } = entry
            {
                if sid == worker_session_id {
                    return Ok(Some(handoff));
                }
            }
        }
        Ok(None)
    }

    // --- Model Settings ---

    pub fn read_model_settings(&self) -> Result<MissionModelSettings> {
        let path = self.model_settings_path();
        if !path.exists() {
            return Ok(MissionModelSettings::default());
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read model settings from {:?}", path))?;
        let settings: MissionModelSettings = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse model settings from {:?}", path))?;
        Ok(settings)
    }

    pub fn write_model_settings(&self, settings: &MissionModelSettings) -> Result<()> {
        let content = serde_json::to_string_pretty(settings)
            .context("Failed to serialize MissionModelSettings")?;
        fs::write(self.model_settings_path(), content)
            .with_context(|| format!("Failed to write model settings to {:?}", self.model_settings_path()))?;
        Ok(())
    }

    // --- Skills ---

    pub fn ensure_default_skills(&self) -> Result<()> {
        let skills_root = self.skills_dir();
        fs::create_dir_all(&skills_root)?;

        let foundation = skills_root.join("foundation-worker");
        fs::create_dir_all(&foundation)?;
        let foundation_skill = foundation.join("SKILL.md");
        if !foundation_skill.exists() {
            fs::write(
                &foundation_skill,
                r#"---
name: foundation-worker
description: Repair test and startup foundations and build repeatable current-source validation without weakening production boundaries.
---

# Foundation worker

Focus on repairing test hermeticity, baseline execution, and solid foundational code. Run relevant tests before and after changes, never weaken existing invariants or authorization rules, and report complete verification evidence upon completion.
"#,
            )?;
        }

        let refactor = skills_root.join("refactoring-playbook");
        fs::create_dir_all(&refactor)?;
        let refactor_skill = refactor.join("SKILL.md");
        if !refactor_skill.exists() {
            fs::write(
                &refactor_skill,
                r#"---
name: refactoring-playbook
description: Playbook for code modernization, architecture migrations, and large-scale refactoring.
---

# Refactoring Playbook

Ensure incremental migrations, behavior preservation, and characterization testing. Always verify that existing test suites pass before modifying core components.
"#,
            )?;
        }

        Ok(())
    }
}
