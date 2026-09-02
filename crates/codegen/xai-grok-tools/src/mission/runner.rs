//! Mission runner loop and execution engine.
//!
//! Orchestrates feature sequencing, preemption, retry budgets, milestone validation
//! injection, worker prompt assembly, and state transitions.

use crate::mission::storage::MissionFileService;
use crate::mission::types::{
    Feature, FeatureStatus, MissionState, ProgressLogEntry, SavedWorkerHandoff, WorkerHandoff,
    WorkerSuccessState,
};
use anyhow::{Result, anyhow};
use std::fs;
use std::path::PathBuf;
use std::process::Child;

/// Result of advancing one step in the mission runner.
#[derive(Debug)]
pub enum MissionRunStepResult {
    /// A worker should be launched for the given feature.
    WorkerReady {
        feature: Feature,
        worker_prompt: String,
        skill_name: String,
        skill_instructions: Option<String>,
        architecture_doc: Option<String>,
        validation_contract: Option<String>,
    },
    /// The mission has completed all features and validation gates.
    Completed,
    /// Execution paused because the scope has grown significantly and requires orchestrator review.
    ScopeReviewRequired {
        initial_count: usize,
        current_count: usize,
    },
    /// Execution paused and returned control to the orchestrator (e.g. handoff items, failure, or explicitly requested).
    OrchestratorTurn {
        reason: String,
        feature_id: Option<String>,
    },
    /// Execution is paused (e.g. usage limit, retry budget exhausted, or user pause).
    Paused {
        reason: String,
    },
}

/// Wake-lock to prevent system sleep during autonomous mission runs.
pub struct MissionWakeLock {
    #[cfg(target_os = "macos")]
    process: Option<Child>,
}

impl MissionWakeLock {
    pub fn acquire() -> Self {
        #[cfg(target_os = "macos")]
        {
            let child = std::process::Command::new("caffeinate")
                .args(["-d", "-i", "-m"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok();
            Self { process: child }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {}
        }
    }
}

impl Drop for MissionWakeLock {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(mut child) = self.process.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// The runner driving a mission forward.
pub struct MissionRunner {
    storage: MissionFileService,
    wake_lock: Option<MissionWakeLock>,
}

impl MissionRunner {
    pub fn new(mission_dir: impl Into<PathBuf>) -> Self {
        Self {
            storage: MissionFileService::new(mission_dir),
            wake_lock: None,
        }
    }

    pub fn storage(&self) -> &MissionFileService {
        &self.storage
    }

    /// Prepare to run: sets state to `Running`, initializes baseline counts, and acquires wake-lock.
    pub fn prepare_run(&mut self) -> Result<()> {
        self.storage.initialize_mission_dir()?;
        self.storage.ensure_default_skills()?;

        let mut state = self.storage.read_state()?;
        let features = self.storage.read_features()?.features;

        // Baseline feature count for scope expansion detection
        if state.initial_feature_count.is_none() && !features.is_empty() {
            state.initial_feature_count = Some(features.len());
        }

        state.state = MissionState::Running;
        state.updated_at = chrono::Utc::now().to_rfc3339();
        self.storage.write_state(&state)?;

        self.storage.append_progress_log(&ProgressLogEntry::MissionRunStarted {
            timestamp: chrono::Utc::now().to_rfc3339(),
            message: format!("Started mission run with {} features", features.len()),
        })?;

        self.wake_lock = Some(MissionWakeLock::acquire());
        Ok(())
    }

    /// Advance one execution step in the mission.
    pub fn advance(&mut self) -> Result<MissionRunStepResult> {
        let mut state = self.storage.read_state()?;
        if state.state == MissionState::Paused {
            return Ok(MissionRunStepResult::Paused {
                reason: "Mission is paused".to_string(),
            });
        }
        if state.state == MissionState::OrchestratorTurn {
            return Ok(MissionRunStepResult::OrchestratorTurn {
                reason: "Awaiting orchestrator review".to_string(),
                feature_id: None,
            });
        }
        if state.state == MissionState::Completed {
            return Ok(MissionRunStepResult::Completed);
        }

        let mut features_file = self.storage.read_features()?;

        // 1. Scope creep check: if scope grew > 1.5x of initial count, pause for review
        if let Some(initial) = state.initial_feature_count {
            if initial >= 5 && features_file.features.len() > (initial * 3) / 2 {
                state.state = MissionState::Paused;
                state.updated_at = chrono::Utc::now().to_rfc3339();
                self.storage.write_state(&state)?;
                self.storage.append_progress_log(&ProgressLogEntry::MissionPaused {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    pause_reason: Some("scope_growth_review_required".to_string()),
                })?;
                return Ok(MissionRunStepResult::ScopeReviewRequired {
                    initial_count: initial,
                    current_count: features_file.features.len(),
                });
            }
        }

        // 2. Preemption check:
        // If a feature is currently marked InProgress, but there is a Pending feature placed
        // ahead of it in features.json (e.g. user inserted an urgent fix at the top),
        // revert the in-progress feature to Pending and pick the top one first!
        let in_prog_idx = features_file.features.iter().position(|f| f.status == FeatureStatus::InProgress);
        let first_pending_idx = features_file.features.iter().position(|f| f.status == FeatureStatus::Pending);

        if let (Some(ip_idx), Some(fp_idx)) = (in_prog_idx, first_pending_idx) {
            if fp_idx < ip_idx {
                // Preempt!
                features_file.features[ip_idx].status = FeatureStatus::Pending;
                features_file.features[ip_idx].current_worker_session_id = None;
                self.storage.write_features(&features_file)?;
            }
        }

        // 3. Milestone validation check:
        // Check all milestones to see if any completed their implementation features
        let milestones: Vec<String> = features_file
            .features
            .iter()
            .map(|f| f.milestone.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        for m in milestones {
            if self.storage.check_and_inject_milestone_validation(&m)? {
                // Re-read features since validators were injected
                features_file = self.storage.read_features()?;
            }
        }

        // 4. Select candidate feature:
        // Prefer already InProgress feature (resumed run), or first Pending feature
        let target_feature = if let Some(f) = features_file.features.iter().find(|f| f.status == FeatureStatus::InProgress) {
            f.clone()
        } else if let Some(f) = features_file.features.iter().find(|f| f.status == FeatureStatus::Pending) {
            f.clone()
        } else {
            // No features left to execute!
            state.state = MissionState::Completed;
            state.updated_at = chrono::Utc::now().to_rfc3339();
            self.storage.write_state(&state)?;
            return Ok(MissionRunStepResult::Completed);
        };

        // Mark InProgress if not already
        if target_feature.status != FeatureStatus::InProgress {
            let mut updated = target_feature.clone();
            updated.status = FeatureStatus::InProgress;
            self.storage.update_feature(&updated)?;
        }

        // 5. Read supporting documentation
        let architecture = fs::read_to_string(self.storage.mission_dir().join("architecture.md")).ok();
        let validation = fs::read_to_string(self.storage.mission_dir().join("validation-contract.md")).ok();

        // Read skill
        let skill_file = self.storage.skills_dir().join(&target_feature.skill_name).join("SKILL.md");
        let skill_content = fs::read_to_string(skill_file).ok();

        // Build worker prompt
        let worker_prompt = build_worker_prompt(&target_feature, architecture.as_deref(), validation.as_deref());

        Ok(MissionRunStepResult::WorkerReady {
            feature: target_feature.clone(),
            worker_prompt,
            skill_name: target_feature.skill_name.clone(),
            skill_instructions: skill_content,
            architecture_doc: architecture,
            validation_contract: validation,
        })
    }

    /// Record a worker completion and handoff.
    pub fn handle_worker_completion(
        &mut self,
        worker_session_id: &str,
        feature_id: &str,
        success_state: WorkerSuccessState,
        return_to_orchestrator: bool,
        commit_id: Option<String>,
        exit_code: i32,
        handoff: WorkerHandoff,
    ) -> Result<MissionRunStepResult> {
        let timestamp = chrono::Utc::now().to_rfc3339();

        // Save handoff to disk
        let saved_handoff = SavedWorkerHandoff {
            timestamp: timestamp.clone(),
            worker_session_id: worker_session_id.to_string(),
            feature_id: feature_id.to_string(),
            milestone: None,
            commit_id: commit_id.clone(),
            repo_path: self.storage.read_working_directory().ok(),
            success_state,
            return_to_orchestrator,
            handoff: handoff.clone(),
        };
        self.storage.write_worker_handoff(&saved_handoff)?;

        // Log worker completed in progress_log.jsonl
        self.storage.append_progress_log(&ProgressLogEntry::WorkerCompleted {
            timestamp: timestamp.clone(),
            worker_session_id: worker_session_id.to_string(),
            feature_id: feature_id.to_string(),
            success_state,
            return_to_orchestrator,
            commit_id,
            repo_path: self.storage.read_working_directory().ok(),
            exit_code,
            validators_passed: success_state == WorkerSuccessState::Success,
            handoff,
        })?;

        let mut features_file = self.storage.read_features()?;
        let feature = features_file.features.iter_mut().find(|f| f.id == feature_id)
            .ok_or_else(|| anyhow!("Feature {} not found in features.json", feature_id))?;

        if !feature.worker_session_ids.contains(&worker_session_id.to_string()) {
            feature.worker_session_ids.push(worker_session_id.to_string());
        }
        feature.current_worker_session_id = None;

        match success_state {
            WorkerSuccessState::Success => {
                feature.status = FeatureStatus::Completed;
                feature.completed_worker_session_id = Some(worker_session_id.to_string());
                self.storage.write_features(&features_file)?;

                if return_to_orchestrator {
                    self.storage.update_state_status(MissionState::OrchestratorTurn)?;
                    return Ok(MissionRunStepResult::OrchestratorTurn {
                        reason: "Worker requested orchestrator review upon success".to_string(),
                        feature_id: Some(feature_id.to_string()),
                    });
                }

                // Check if mission is complete
                if self.storage.are_all_features_completed()? {
                    self.storage.update_state_status(MissionState::Completed)?;
                    return Ok(MissionRunStepResult::Completed);
                }

                // Continue running next feature
                self.advance()
            }
            WorkerSuccessState::Failure | WorkerSuccessState::Partial => {
                feature.status = FeatureStatus::Pending;
                self.storage.write_features(&features_file)?;

                self.storage.update_state_status(MissionState::OrchestratorTurn)?;
                Ok(MissionRunStepResult::OrchestratorTurn {
                    reason: format!("Worker ended with state: {:?}", success_state),
                    feature_id: Some(feature_id.to_string()),
                })
            }
        }
    }

    /// Pause the mission execution.
    pub fn pause(&mut self, reason: Option<&str>) -> Result<()> {
        self.wake_lock = None;
        let mut state = self.storage.read_state()?;
        state.state = MissionState::Paused;
        state.updated_at = chrono::Utc::now().to_rfc3339();
        self.storage.write_state(&state)?;

        self.storage.append_progress_log(&ProgressLogEntry::MissionPaused {
            timestamp: chrono::Utc::now().to_rfc3339(),
            pause_reason: reason.map(str::to_string),
        })?;
        Ok(())
    }
}

fn build_worker_prompt(
    feature: &Feature,
    architecture: Option<&str>,
    validation: Option<&str>,
) -> String {
    let mut prompt = format!(
        "# Mission Feature Implementation: {}\n\n## Description\n{}\n\n",
        feature.id, feature.description
    );

    if !feature.preconditions.is_empty() {
        prompt.push_str("## Preconditions\n");
        for p in &feature.preconditions {
            prompt.push_str(&format!("- {}\n", p));
        }
        prompt.push('\n');
    }

    if !feature.expected_behavior.is_empty() {
        prompt.push_str("## Expected Behavior & Contract Invariants\n");
        for eb in &feature.expected_behavior {
            prompt.push_str(&format!("- {}\n", eb));
        }
        prompt.push('\n');
    }

    if !feature.fulfills.is_empty() {
        prompt.push_str("## Fulfills Assertions\n");
        for f in &feature.fulfills {
            prompt.push_str(&format!("- {}\n", f));
        }
        prompt.push('\n');
    }

    if let Some(arch) = architecture {
        prompt.push_str("## Authoritative Architecture Overview\n");
        prompt.push_str(arch);
        prompt.push_str("\n\n");
    }

    if let Some(val) = validation {
        prompt.push_str("## Validation Contract\n");
        prompt.push_str(val);
        prompt.push_str("\n\n");
    }

    prompt.push_str("Execute this feature directly, adhere strictly to all invariants and test requirements, and call `end_feature_run` when finished to record your handoff.\n");

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_runner_flow_and_preemption() {
        let tmp = TempDir::new().unwrap();
        let svc = MissionFileService::new(tmp.path());
        svc.initialize_mission_dir().unwrap();

        let state = crate::mission::types::MissionStateFile {
            mission_id: "mis_test".to_string(),
            state: MissionState::AwaitingInput,
            working_directory: "/tmp".to_string(),
            created_at: "2026-09-02T12:00:00Z".to_string(),
            updated_at: "2026-09-02T12:00:00Z".to_string(),
            initial_feature_count: None,
            last_reviewed_handoff_count: None,
            feature_retry_budget_bonus: Default::default(),
        };
        svc.write_state(&state).unwrap();

        let f1 = Feature {
            id: "feature-1".to_string(),
            description: "First feature".to_string(),
            skill_name: "foundation-worker".to_string(),
            preconditions: vec![],
            expected_behavior: vec![],
            fulfills: vec![],
            milestone: "m1".to_string(),
            status: FeatureStatus::Pending,
            worker_session_ids: vec![],
            current_worker_session_id: None,
            completed_worker_session_id: None,
        };
        svc.write_features(&crate::mission::types::FeaturesFile { features: vec![f1] }).unwrap();

        let mut runner = MissionRunner::new(tmp.path());
        runner.prepare_run().unwrap();

        let step = runner.advance().unwrap();
        match step {
            MissionRunStepResult::WorkerReady { feature, .. } => {
                assert_eq!(feature.id, "feature-1");
            }
            _ => panic!("Expected WorkerReady"),
        }

        // Now test preemption: insert feature-0 at top
        let f0 = Feature {
            id: "feature-0".to_string(),
            description: "Top priority feature".to_string(),
            skill_name: "foundation-worker".to_string(),
            preconditions: vec![],
            expected_behavior: vec![],
            fulfills: vec![],
            milestone: "m1".to_string(),
            status: FeatureStatus::Pending,
            worker_session_ids: vec![],
            current_worker_session_id: None,
            completed_worker_session_id: None,
        };
        runner.storage.insert_feature_at_top(f0).unwrap();

        let step2 = runner.advance().unwrap();
        match step2 {
            MissionRunStepResult::WorkerReady { feature, .. } => {
                assert_eq!(feature.id, "feature-0");
            }
            _ => panic!("Expected WorkerReady for preempting feature-0"),
        }
    }
}
