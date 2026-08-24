//! Evidence-backed, outcome-aware experience memory.

pub mod evaluation;
pub mod extraction;
pub mod retrieval;
pub mod store;
pub mod types;

#[cfg(test)]
mod learning_loop_tests;

pub use evaluation::{
    EvaluationRun, EvaluationSummary, RetrievalAblation, evaluate_ablations, evaluate_runs,
    exposure_utility, recommendation_utility,
};
pub use extraction::{ObservedEvent, RunObservation, extract_experiences};
pub use retrieval::{build_briefing, rank_experiences, render_briefing};
pub use store::ExperienceStore;
pub use types::{
    EvidenceKind, EvidenceSignal, EvidenceVerdict, ExperienceBriefing, ExperienceCategory,
    ExperienceContradiction, ExperienceMemory, ExperienceQuery, ExperienceScope, ExperienceStatus,
    FailureClass, OutcomeDimensions, RankedExperience,
};

/// Return the current repository revision when the workspace belongs to Git.
pub fn current_repository_revision(workspace: &std::path::Path) -> Option<String> {
    git2::Repository::discover(workspace)
        .ok()?
        .head()
        .ok()?
        .target()
        .map(|revision| revision.to_string())
}

/// Describe stable execution-environment characteristics without user data.
pub fn execution_environment() -> String {
    format!("{}:{}", std::env::consts::OS, std::env::consts::ARCH)
}
