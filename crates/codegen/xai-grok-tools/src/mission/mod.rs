//! Mission orchestration and execution engine.
//!
//! Provides data structures, discovery, storage, and a runner loop compatible
//! with Factory Droid missions.

pub mod discovery;
pub mod runner;
pub mod storage;
pub mod types;

pub use discovery::{
    MissionSource, MissionSummary, discover_all_missions, discover_missions_for_workspace,
    factory_droid_missions_dir, find_mission, opengrok_missions_dir,
};
pub use runner::{MissionRunStepResult, MissionRunner, MissionWakeLock};
pub use storage::{
    FEATURES_FILENAME, HANDOFFS_DIR, MISSION_MD_FILENAME, MODEL_SETTINGS_FILENAME,
    MissionFileService, PROGRESS_LOG_FILENAME, SCRUTINY_VALIDATOR_SKILL, SKILLS_DIR,
    STATE_FILENAME, USER_TESTING_VALIDATOR_SKILL, WORKING_DIR_FILENAME,
};
pub use types::{
    CommandRunRecord, DiscoveredIssue, Feature, FeatureStatus, FeaturesFile, HandoffDismissal,
    InteractiveCheckRecord, MissionModelSettings, MissionState, MissionStateFile, ProgressLogEntry,
    SavedWorkerHandoff, SkillFeedback, TestCaseRecord, TestFileRecord, TestsSection,
    VerificationSection, WorkerHandoff, WorkerSuccessState,
};
