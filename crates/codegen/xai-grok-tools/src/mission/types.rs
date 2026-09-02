//! Mission data models and types.
//!
//! Provides 100% data schema compatibility with Factory Droid missions,
//! allowing Open Grok to read, execute, create, and resume missions interchangeably.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The lifecycle state of a mission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionState {
    /// Waiting for user or orchestrator input / setup.
    AwaitingInput,
    /// Actively executing features via workers.
    Running,
    /// Paused by user or runner (e.g. usage limit, stall, or max retries).
    Paused,
    /// Paused and yielded back to the orchestrator for review / triage.
    OrchestratorTurn,
    /// All implementation and validation features completed.
    Completed,
}

impl std::fmt::Display for MissionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AwaitingInput => write!(f, "awaiting_input"),
            Self::Running => write!(f, "running"),
            Self::Paused => write!(f, "paused"),
            Self::OrchestratorTurn => write!(f, "orchestrator_turn"),
            Self::Completed => write!(f, "completed"),
        }
    }
}

/// Represents the content of `state.json` in a mission directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionStateFile {
    /// Public mission identifier, e.g. "mis_24348f5c".
    pub mission_id: String,
    /// Current state of the mission.
    pub state: MissionState,
    /// Target repository/workspace directory.
    pub working_directory: String,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// ISO 8601 last update timestamp.
    pub updated_at: String,
    /// Initial count of implementation features when run began (for scope creep checks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_feature_count: Option<usize>,
    /// Number of handoffs reviewed by the orchestrator so far.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reviewed_handoff_count: Option<usize>,
    /// Extra retry budget granted per feature id.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub feature_retry_budget_bonus: HashMap<String, u32>,
}

/// Status of an individual feature in `features.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureStatus {
    Pending,
    InProgress,
    Completed,
}

impl std::fmt::Display for FeatureStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Completed => write!(f, "completed"),
        }
    }
}

/// A feature defined in `features.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Feature {
    /// Unique feature slug/ID, e.g. "startup-baseline-repair".
    pub id: String,
    /// Detailed description of the feature requirements and implementation tasks.
    pub description: String,
    /// Name of the specialized skill in `{missionDir}/skills/<skillName>/SKILL.md`.
    pub skill_name: String,
    /// Preconditions that must be met before starting this feature.
    #[serde(default)]
    pub preconditions: Vec<String>,
    /// Contract invariants, test commands, and expected behaviors.
    #[serde(default)]
    pub expected_behavior: Vec<String>,
    /// Validation contract assertion IDs fulfilled by this feature (e.g. `["VAL-DEV-001"]`).
    #[serde(default)]
    pub fulfills: Vec<String>,
    /// Milestone identifier this feature belongs to (e.g. "startup-baseline").
    #[serde(default = "default_milestone")]
    pub milestone: String,
    /// Current status in the queue.
    #[serde(default = "default_feature_status")]
    pub status: FeatureStatus,
    /// Historical list of worker session IDs that worked on this feature.
    #[serde(default)]
    pub worker_session_ids: Vec<String>,
    /// Active worker session ID if in progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_worker_session_id: Option<String>,
    /// Worker session ID that completed this feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_worker_session_id: Option<String>,
}

fn default_milestone() -> String {
    "default".to_string()
}

fn default_feature_status() -> FeatureStatus {
    FeatureStatus::Pending
}

/// Content of `features.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturesFile {
    pub features: Vec<Feature>,
}

/// Result state returned by a worker when finishing a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSuccessState {
    Success,
    Failure,
    Partial,
}

/// Discovered issue reported in a worker handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredIssue {
    /// "blocking" or "non_blocking".
    pub severity: String,
    /// Description of the discovered problem.
    pub description: String,
    /// Recommended resolution or allocation.
    pub suggested_fix: String,
}

/// Structured record of commands executed during verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandRunRecord {
    pub command: String,
    pub exit_code: i32,
    pub observation: String,
}

/// Interactive manual/PTY check executed during verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveCheckRecord {
    pub action: String,
    pub observed: String,
}

/// Verification section of a worker handoff.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationSection {
    #[serde(default)]
    pub commands_run: Vec<CommandRunRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interactive_checks: Vec<InteractiveCheckRecord>,
}

/// Test case description inside a test file report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCaseRecord {
    pub name: String,
    pub verifies: String,
}

/// Test file additions reported in a worker handoff.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestFileRecord {
    pub file: String,
    #[serde(default)]
    pub cases: Vec<TestCaseRecord>,
}

/// Tests section of a worker handoff.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestsSection {
    #[serde(default)]
    pub added: Vec<TestFileRecord>,
    #[serde(default)]
    pub updated: Vec<String>,
    #[serde(default)]
    pub coverage: String,
}

/// Worker skill feedback.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillFeedback {
    #[serde(default)]
    pub followed_procedure: bool,
    #[serde(default)]
    pub deviations: Vec<String>,
    #[serde(default)]
    pub suggested_changes: Vec<String>,
}

/// The structured handoff produced by a worker via `end_feature_run`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerHandoff {
    /// Concise executive summary of what was accomplished.
    pub salient_summary: String,
    /// Detailed description of what was implemented, files touched, commits/stashes made.
    pub what_was_implemented: String,
    /// Outstanding gaps, blockers, or remaining requirements.
    pub what_was_left_undone: String,
    /// Verification evidence.
    #[serde(default)]
    pub verification: VerificationSection,
    /// Tests added or updated.
    #[serde(default)]
    pub tests: TestsSection,
    /// Issues uncovered outside or within scope.
    #[serde(default)]
    pub discovered_issues: Vec<DiscoveredIssue>,
    /// Feedback on the worker skill procedure.
    #[serde(default)]
    pub skill_feedback: Option<SkillFeedback>,
}

/// Full record saved under `{missionDir}/handoffs/<timestamp>__<featureId>__<workerSessionId>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedWorkerHandoff {
    pub timestamp: String,
    pub worker_session_id: String,
    pub feature_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub milestone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    pub success_state: WorkerSuccessState,
    pub return_to_orchestrator: bool,
    pub handoff: WorkerHandoff,
}

/// A handoff dismissal entry in `progress_log.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoffDismissal {
    /// "discovered_issue" or "incomplete_work".
    pub r#type: String,
    pub source_feature_id: String,
    pub summary: String,
    pub justification: String,
}

/// Chronological event entry in `progress_log.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProgressLogEntry {
    MissionAccepted {
        timestamp: String,
        title: String,
    },
    MissionRunStarted {
        timestamp: String,
        message: String,
    },
    WorkerSelectedFeature {
        timestamp: String,
        #[serde(rename = "workerSessionId")]
        worker_session_id: String,
        #[serde(rename = "featureId")]
        feature_id: String,
    },
    WorkerStarted {
        timestamp: String,
        #[serde(rename = "workerSessionId")]
        worker_session_id: String,
        #[serde(rename = "spawnId")]
        spawn_id: String,
        #[serde(rename = "featureId")]
        feature_id: String,
        #[serde(rename = "modelId", default, skip_serializing_if = "Option::is_none")]
        model_id: Option<String>,
    },
    WorkerCompleted {
        timestamp: String,
        #[serde(rename = "workerSessionId")]
        worker_session_id: String,
        #[serde(rename = "featureId")]
        feature_id: String,
        #[serde(rename = "successState")]
        success_state: WorkerSuccessState,
        #[serde(rename = "returnToOrchestrator")]
        return_to_orchestrator: bool,
        #[serde(rename = "commitId", default, skip_serializing_if = "Option::is_none")]
        commit_id: Option<String>,
        #[serde(rename = "repoPath", default, skip_serializing_if = "Option::is_none")]
        repo_path: Option<String>,
        #[serde(rename = "exitCode", default)]
        exit_code: i32,
        #[serde(rename = "validatorsPassed", default)]
        validators_passed: bool,
        handoff: WorkerHandoff,
    },
    WorkerFailed {
        timestamp: String,
        #[serde(rename = "workerSessionId", default, skip_serializing_if = "Option::is_none")]
        worker_session_id: Option<String>,
        #[serde(rename = "featureId", default, skip_serializing_if = "Option::is_none")]
        feature_id: Option<String>,
        reason: String,
    },
    MissionPaused {
        timestamp: String,
        #[serde(rename = "pauseReason", default, skip_serializing_if = "Option::is_none")]
        pause_reason: Option<String>,
    },
    MissionResumed {
        timestamp: String,
    },
    HandoffItemsDismissed {
        timestamp: String,
        dismissals: Vec<HandoffDismissal>,
    },
    MilestoneCompleted {
        timestamp: String,
        milestone: String,
    },
}

/// Content of `model-settings.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionModelSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_worker_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_worker_reasoning_effort: Option<String>,
    #[serde(default)]
    pub skip_scrutiny: bool,
    #[serde(default)]
    pub skip_user_testing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_json_roundtrip() {
        let raw = r#"{
  "missionId": "mis_24348f5c",
  "state": "paused",
  "workingDirectory": "/Users/mweinbach/Projects/agent-coworker",
  "createdAt": "2026-08-28T21:54:02.929Z",
  "updatedAt": "2026-08-31T22:07:17.043Z",
  "initialFeatureCount": 111,
  "lastReviewedHandoffCount": 21,
  "featureRetryBudgetBonus": {
    "startup-server-fixture-restart-settlement": 5
  }
}"#;
        let parsed: MissionStateFile = serde_json::from_str(raw).expect("valid state.json");
        assert_eq!(parsed.mission_id, "mis_24348f5c");
        assert_eq!(parsed.state, MissionState::Paused);
        assert_eq!(parsed.initial_feature_count, Some(111));
        assert_eq!(parsed.feature_retry_budget_bonus.get("startup-server-fixture-restart-settlement"), Some(&5));

        let reserialized = serde_json::to_string_pretty(&parsed).expect("reserialize");
        let parsed_again: MissionStateFile = serde_json::from_str(&reserialized).expect("reparse");
        assert_eq!(parsed_again.mission_id, parsed.mission_id);
        assert_eq!(parsed_again.state, parsed.state);
    }

    #[test]
    fn test_progress_log_worker_completed_roundtrip() {
        let raw = r#"{"timestamp":"2026-08-29T15:43:43.787Z","type":"worker_completed","workerSessionId":"83751be3-2997-4552-aad8-ec0bba7305d0","featureId":"startup-baseline-repair","successState":"success","returnToOrchestrator":false,"commitId":"e54fef0e22acf4f962ccf76b6b1a41308f8a69f8","repoPath":"/Users/mweinbach/Projects/agent-coworker","exitCode":0,"validatorsPassed":true,"handoff":{"salientSummary":"Committed the complete baseline repair","whatWasImplemented":"Applied stash","whatWasLeftUndone":"","verification":{"commandsRun":[{"command":"bun run test","exitCode":0,"observation":"passed"}]},"tests":{"added":[],"updated":[],"coverage":"full"},"discoveredIssues":[],"skillFeedback":{"followedProcedure":true,"deviations":[],"suggestedChanges":[]}}}"#;
        let parsed: ProgressLogEntry = serde_json::from_str(raw).expect("valid progress log entry");
        match parsed {
            ProgressLogEntry::WorkerCompleted {
                feature_id,
                success_state,
                return_to_orchestrator,
                handoff,
                ..
            } => {
                assert_eq!(feature_id, "startup-baseline-repair");
                assert_eq!(success_state, WorkerSuccessState::Success);
                assert!(!return_to_orchestrator);
                assert_eq!(handoff.salient_summary, "Committed the complete baseline repair");
                assert_eq!(handoff.verification.commands_run.len(), 1);
            }
            _ => panic!("Expected WorkerCompleted entry"),
        }
    }
}
