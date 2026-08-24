use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceCategory {
    SuccessfulPattern,
    FailureAntiPattern,
    EnvironmentalFact,
    ToolProcessLesson,
    ArchitecturalLesson,
    #[default]
    #[serde(other)]
    UncertainHypothesis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceScope {
    ExactFile,
    Module,
    Framework,
    TaskType,
    Global,
    #[default]
    #[serde(other)]
    Repository,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceStatus {
    #[default]
    Active,
    LowConfidence,
    Superseded,
    Deprecated,
    #[serde(other)]
    Invalidated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    CommandExit,
    Test,
    Compile,
    Lint,
    TypeCheck,
    Benchmark,
    Runtime,
    Judge,
    CodeReview,
    UserFeedback,
    Regression,
    Diff,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceVerdict {
    Passed,
    Failed,
    #[default]
    #[serde(other)]
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    DatabaseLock,
    Build,
    Test,
    Lint,
    TypeCheck,
    GeneratedFile,
    Permission,
    Timeout,
    Dependency,
    Regression,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvidenceSignal {
    pub kind: EvidenceKind,
    pub verdict: EvidenceVerdict,
    pub command: Option<String>,
    pub summary: String,
    pub score: Option<f64>,
    pub observed_at: i64,
    pub source_run_id: Option<String>,
}

impl EvidenceSignal {
    pub fn is_objective(&self) -> bool {
        matches!(
            self.kind,
            EvidenceKind::CommandExit
                | EvidenceKind::Test
                | EvidenceKind::Compile
                | EvidenceKind::Lint
                | EvidenceKind::TypeCheck
                | EvidenceKind::Benchmark
                | EvidenceKind::Runtime
                | EvidenceKind::Regression
                | EvidenceKind::Diff
        )
    }

    pub fn is_verification(&self) -> bool {
        matches!(
            self.kind,
            EvidenceKind::Test
                | EvidenceKind::Compile
                | EvidenceKind::Lint
                | EvidenceKind::TypeCheck
                | EvidenceKind::Benchmark
                | EvidenceKind::Regression
                | EvidenceKind::Diff
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct OutcomeDimensions {
    pub functional_correctness: Option<f64>,
    pub completeness: Option<f64>,
    pub code_quality: Option<f64>,
    pub maintainability: Option<f64>,
    pub architectural_fit: Option<f64>,
    pub efficiency: Option<f64>,
    pub regression_risk: Option<f64>,
    pub test_coverage: Option<f64>,
    pub user_preference: Option<f64>,
    pub judge_score: Option<f64>,
}

impl OutcomeDimensions {
    pub fn quality_score(&self) -> f64 {
        let weighted_dimensions = [
            (self.functional_correctness, 0.24),
            (self.completeness, 0.14),
            (self.code_quality, 0.15),
            (self.maintainability, 0.09),
            (self.architectural_fit, 0.09),
            (self.efficiency, 0.05),
            (self.regression_risk.map(|risk| 1.0 - risk), 0.12),
            (self.test_coverage, 0.07),
            (self.user_preference, 0.07),
            (self.judge_score, 0.10),
        ];

        let (weighted_score, observed_weight) = weighted_dimensions
            .into_iter()
            .filter_map(|(value, weight)| {
                value
                    .filter(|value| value.is_finite())
                    .map(|value| (value.clamp(0.0, 1.0) * weight, weight))
            })
            .fold((0.0, 0.0), |(score, weight), (next_score, next_weight)| {
                (score + next_score, weight + next_weight)
            });

        if observed_weight == 0.0 {
            return 0.5;
        }

        let mut score = weighted_score / observed_weight;

        if self.functional_correctness.is_none() {
            score = score.min(0.65);
        }

        if self
            .functional_correctness
            .is_some_and(|correctness| correctness.is_finite() && correctness < 0.5)
        {
            score = score.min(0.45);
        }

        if self
            .regression_risk
            .is_some_and(|risk| risk.is_finite() && risk >= 0.6)
        {
            score = score.min(0.4);
        }

        if self
            .code_quality
            .is_some_and(|quality| quality.is_finite() && quality < 0.35)
            || self
                .judge_score
                .is_some_and(|quality| quality.is_finite() && quality < 0.35)
        {
            score = score.min(0.5);
        }

        score.clamp(0.0, 1.0)
    }

    pub fn clamp_scores(&mut self) {
        for score in [
            &mut self.functional_correctness,
            &mut self.completeness,
            &mut self.code_quality,
            &mut self.maintainability,
            &mut self.architectural_fit,
            &mut self.efficiency,
            &mut self.regression_risk,
            &mut self.test_coverage,
            &mut self.user_preference,
            &mut self.judge_score,
        ] {
            *score = score.filter(|value| value.is_finite()).map(clamp_unit);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperienceMemory {
    pub id: String,
    pub category: ExperienceCategory,
    pub task_type: String,
    pub task_summary: String,
    pub context: String,
    pub environment: String,
    pub repository_id: String,
    pub repository_revision: Option<String>,
    pub scope: ExperienceScope,
    pub strategy: String,
    pub strategy_rationale: String,
    pub key_decisions: Vec<String>,
    pub implementation_pattern: Option<String>,
    pub outcome: OutcomeDimensions,
    pub success: Option<bool>,
    pub tests_run: Vec<String>,
    pub test_results: Vec<EvidenceSignal>,
    pub evaluator_scores: BTreeMap<String, f64>,
    pub judge_feedback: Option<String>,
    pub failure_reason: Option<String>,
    pub what_worked: Vec<String>,
    pub what_failed: Vec<String>,
    pub lesson: String,
    pub recommendation: Option<String>,
    pub anti_pattern: Option<String>,
    pub confidence: f64,
    pub generalizability: f64,
    pub novelty: f64,
    pub source_run_ids: Vec<String>,
    pub evidence: Vec<EvidenceSignal>,
    pub evidence_count: u32,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_used_at: Option<i64>,
    pub retrieved_count: u32,
    pub followed_count: u32,
    pub successful_reuse_count: u32,
    pub failed_reuse_count: u32,
    pub status: ExperienceStatus,
    pub superseded_by: Option<String>,
}

impl Default for ExperienceMemory {
    fn default() -> Self {
        Self {
            id: String::new(),
            category: ExperienceCategory::default(),
            task_type: String::new(),
            task_summary: String::new(),
            context: String::new(),
            environment: String::new(),
            repository_id: String::new(),
            repository_revision: None,
            scope: ExperienceScope::default(),
            strategy: String::new(),
            strategy_rationale: String::new(),
            key_decisions: Vec::new(),
            implementation_pattern: None,
            outcome: OutcomeDimensions::default(),
            success: None,
            tests_run: Vec::new(),
            test_results: Vec::new(),
            evaluator_scores: BTreeMap::new(),
            judge_feedback: None,
            failure_reason: None,
            what_worked: Vec::new(),
            what_failed: Vec::new(),
            lesson: String::new(),
            recommendation: None,
            anti_pattern: None,
            confidence: 0.2,
            generalizability: 0.15,
            novelty: 0.5,
            source_run_ids: Vec::new(),
            evidence: Vec::new(),
            evidence_count: 0,
            created_at: 0,
            updated_at: 0,
            last_used_at: None,
            retrieved_count: 0,
            followed_count: 0,
            successful_reuse_count: 0,
            failed_reuse_count: 0,
            status: ExperienceStatus::Active,
            superseded_by: None,
        }
    }
}

impl ExperienceMemory {
    pub fn new(
        category: ExperienceCategory,
        lesson: impl Into<String>,
        source_run_id: impl Into<String>,
        now: i64,
    ) -> Self {
        let lesson = lesson.into();
        let source_run_id = source_run_id.into();
        let digest =
            blake3::hash(format!("{category:?}\n{lesson}\n{source_run_id}\n{now}").as_bytes());

        Self {
            id: digest.to_hex().to_string(),
            category,
            lesson,
            source_run_ids: if source_run_id.is_empty() {
                Vec::new()
            } else {
                vec![source_run_id]
            },
            created_at: now,
            updated_at: now,
            ..Self::default()
        }
    }

    pub fn clamp_scores(&mut self) {
        self.confidence = clamp_unit(self.confidence);
        self.generalizability = clamp_unit(self.generalizability);
        self.novelty = clamp_unit(self.novelty);
        self.outcome.clamp_scores();
        self.evaluator_scores.retain(|_, score| score.is_finite());

        for score in self.evaluator_scores.values_mut() {
            *score = clamp_unit(*score);
        }

        for signal in self.evidence.iter_mut().chain(self.test_results.iter_mut()) {
            signal.score = signal
                .score
                .filter(|score| score.is_finite())
                .map(clamp_unit);
        }
    }

    pub fn evidence_backed_confidence(&self) -> f64 {
        let expects_failure =
            self.category == ExperienceCategory::FailureAntiPattern || self.success == Some(false);
        let fallback_sources: Vec<&str> = self
            .source_run_ids
            .iter()
            .map(String::as_str)
            .filter(|source| !source.trim().is_empty())
            .collect();
        let mut objective_sources: BTreeMap<String, (f64, f64)> = BTreeMap::new();
        let mut unattributed_index = 0_usize;
        let mut subjective_support: f64 = 0.0;
        let mut subjective_contradiction: f64 = 0.0;

        for signal in &self.evidence {
            if signal.verdict == EvidenceVerdict::Neutral || signal.kind == EvidenceKind::Unknown {
                continue;
            }

            let supports_lesson = match signal.verdict {
                EvidenceVerdict::Failed => expects_failure,
                EvidenceVerdict::Passed => !expects_failure,
                EvidenceVerdict::Neutral => false,
            };

            if signal.is_objective() {
                let source = signal
                    .source_run_id
                    .as_deref()
                    .filter(|source| !source.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        let fallback = fallback_sources
                            .get(unattributed_index.min(fallback_sources.len().saturating_sub(1)))
                            .copied()
                            .unwrap_or("__unattributed__")
                            .to_owned();
                        unattributed_index = unattributed_index.saturating_add(1);
                        fallback
                    });
                let weight = if signal.is_verification() { 1.0 } else { 0.6 };
                let weights = objective_sources.entry(source).or_insert((0.0, 0.0));

                if supports_lesson {
                    weights.0 = weights.0.max(weight);
                } else {
                    weights.1 = weights.1.max(weight);
                }
            } else if supports_lesson {
                subjective_support = subjective_support.max(0.35);
            } else {
                subjective_contradiction = subjective_contradiction.max(0.35);
            }
        }

        let mut support = objective_sources
            .values()
            .map(|(support, _)| support)
            .sum::<f64>()
            + subjective_support;
        let mut contradiction = objective_sources
            .values()
            .map(|(_, contradiction)| contradiction)
            .sum::<f64>()
            + subjective_contradiction;
        let objective_observations = u32::try_from(objective_sources.len()).unwrap_or(u32::MAX);

        support += f64::from(self.successful_reuse_count) * 0.9;
        contradiction += f64::from(self.failed_reuse_count) * 1.1;

        if support + contradiction == 0.0 {
            return 0.2;
        }

        let posterior = (support + 1.0) / (support + contradiction + 2.0);
        let observation_count = objective_observations
            .saturating_add(self.successful_reuse_count)
            .saturating_add(self.failed_reuse_count);
        let evidence_strength = 0.55 + 0.45 * (1.0 - (-support / 3.0_f64).exp());
        let mut confidence = posterior * evidence_strength;

        if objective_observations == 0 && self.successful_reuse_count == 0 {
            confidence = confidence.min(0.35);
        }

        if observation_count <= 1 {
            confidence = confidence.min(0.65);
        }

        if self.category == ExperienceCategory::UncertainHypothesis {
            confidence = confidence.min(0.55);
        }

        confidence.clamp(0.0, 0.98)
    }

    pub fn refresh_confidence(&mut self) {
        self.evidence_count = u32::try_from(self.evidence.len()).unwrap_or(u32::MAX);
        self.confidence = self.evidence_backed_confidence();
        self.clamp_scores();

        if matches!(
            self.status,
            ExperienceStatus::Superseded
                | ExperienceStatus::Deprecated
                | ExperienceStatus::Invalidated
        ) {
            return;
        }

        self.status = if self.confidence < 0.25 && self.failed_reuse_count > 0 {
            ExperienceStatus::LowConfidence
        } else {
            ExperienceStatus::Active
        };
    }

    pub fn record_retrieval(&mut self, now: i64) {
        self.retrieved_count = self.retrieved_count.saturating_add(1);
        self.last_used_at = Some(now);
        self.updated_at = now;
    }

    pub fn record_followed_outcome(&mut self, successful: bool, now: i64) {
        self.followed_count = self.followed_count.saturating_add(1);

        if successful {
            self.successful_reuse_count = self.successful_reuse_count.saturating_add(1);
        } else {
            self.failed_reuse_count = self.failed_reuse_count.saturating_add(1);
        }

        self.last_used_at = Some(now);
        self.updated_at = now;
        self.refresh_confidence();
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperienceQuery {
    pub text: String,
    pub task_type: Option<String>,
    pub repository_id: Option<String>,
    pub repository_revision: Option<String>,
    pub environment: Option<String>,
    pub scope: Option<ExperienceScope>,
    pub failure_context: Option<String>,
    pub limit: usize,
    pub now: i64,
    pub min_confidence: f64,
    pub include_low_confidence: bool,
}

impl Default for ExperienceQuery {
    fn default() -> Self {
        Self {
            text: String::new(),
            task_type: None,
            repository_id: None,
            repository_revision: None,
            environment: None,
            scope: None,
            failure_context: None,
            limit: 6,
            now: chrono::Utc::now().timestamp(),
            min_confidence: 0.0,
            include_low_confidence: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RankedExperience {
    pub memory: ExperienceMemory,
    pub score: f64,
    pub relevance: f64,
    pub context_match: f64,
    pub reuse_utility: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExperienceContradiction {
    pub topic: String,
    pub positive_id: String,
    pub negative_id: String,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ExperienceBriefing {
    pub recommended: Vec<RankedExperience>,
    pub avoid: Vec<RankedExperience>,
    pub uncertain: Vec<RankedExperience>,
    pub contradictions: Vec<ExperienceContradiction>,
}

fn clamp_unit(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(verdict: EvidenceVerdict) -> EvidenceSignal {
        EvidenceSignal {
            kind: EvidenceKind::Test,
            verdict,
            command: Some("cargo test".to_string()),
            summary: "focused test suite".to_string(),
            score: None,
            observed_at: 100,
            source_run_id: None,
        }
    }

    #[test]
    fn categories_round_trip_as_stable_snake_case() {
        let encoded = serde_json::to_string(&ExperienceCategory::FailureAntiPattern).unwrap();

        assert_eq!(encoded, "\"failure_anti_pattern\"");
        assert_eq!(
            serde_json::from_str::<ExperienceCategory>(&encoded).unwrap(),
            ExperienceCategory::FailureAntiPattern
        );
    }

    #[test]
    fn unknown_categories_and_statuses_fail_conservatively() {
        assert_eq!(
            serde_json::from_str::<ExperienceCategory>("\"future_category\"").unwrap(),
            ExperienceCategory::UncertainHypothesis
        );
        assert_eq!(
            serde_json::from_str::<ExperienceStatus>("\"future_status\"").unwrap(),
            ExperienceStatus::Invalidated
        );
        assert_eq!(
            serde_json::from_str::<ExperienceScope>("\"future_scope\"").unwrap(),
            ExperienceScope::Repository
        );
        assert_eq!(
            serde_json::from_str::<EvidenceKind>("\"future_evidence\"").unwrap(),
            EvidenceKind::Unknown
        );
        assert!(
            !EvidenceSignal {
                kind: EvidenceKind::Unknown,
                verdict: EvidenceVerdict::Passed,
                ..EvidenceSignal::default()
            }
            .is_objective()
        );
    }

    #[test]
    fn inspection_evidence_preserves_existing_objective_and_verification_contracts() {
        for kind in [EvidenceKind::Diff, EvidenceKind::Lint] {
            let signal = EvidenceSignal {
                kind,
                verdict: EvidenceVerdict::Passed,
                ..EvidenceSignal::default()
            };

            assert!(signal.is_objective());
            assert!(signal.is_verification());
        }
    }

    #[test]
    fn partial_legacy_records_receive_conservative_defaults() {
        let memory: ExperienceMemory =
            serde_json::from_str(r#"{"id":"legacy","lesson":"Respect generated files"}"#).unwrap();

        assert_eq!(memory.id, "legacy");
        assert_eq!(memory.scope, ExperienceScope::Repository);
        assert_eq!(memory.category, ExperienceCategory::UncertainHypothesis);
        assert_eq!(memory.confidence, 0.2);
        assert!(memory.evidence.is_empty());
    }

    #[test]
    fn legacy_evidence_without_provenance_remains_compatible() {
        let signal: EvidenceSignal =
            serde_json::from_str(r#"{"kind":"test","verdict":"passed","summary":"verified"}"#)
                .unwrap();

        assert_eq!(signal.source_run_id, None);
        assert_eq!(signal.kind, EvidenceKind::Test);
    }

    #[test]
    fn new_memory_ids_are_deterministic_and_source_traceable() {
        let first = ExperienceMemory::new(
            ExperienceCategory::ToolProcessLesson,
            "Run focused tests",
            "run-7",
            123,
        );
        let second = ExperienceMemory::new(
            ExperienceCategory::ToolProcessLesson,
            "Run focused tests",
            "run-7",
            123,
        );

        assert_eq!(first.id, second.id);
        assert_eq!(first.source_run_ids, vec!["run-7"]);
        assert_eq!(first.created_at, 123);
        assert_eq!(first.updated_at, 123);
    }

    #[test]
    fn empty_source_run_is_not_recorded() {
        let memory = ExperienceMemory::new(
            ExperienceCategory::UncertainHypothesis,
            "Potential optimization",
            "",
            123,
        );

        assert!(memory.source_run_ids.is_empty());
    }

    #[test]
    fn missing_quality_dimensions_do_not_imply_excellence() {
        assert_eq!(OutcomeDimensions::default().quality_score(), 0.5);
        assert!(
            OutcomeDimensions {
                code_quality: Some(1.0),
                ..OutcomeDimensions::default()
            }
            .quality_score()
                <= 0.65
        );
    }

    #[test]
    fn correctness_regression_and_poor_review_cap_quality() {
        let failing = OutcomeDimensions {
            functional_correctness: Some(0.2),
            code_quality: Some(1.0),
            ..OutcomeDimensions::default()
        };
        let risky = OutcomeDimensions {
            functional_correctness: Some(1.0),
            regression_risk: Some(0.8),
            ..OutcomeDimensions::default()
        };
        let badly_reviewed = OutcomeDimensions {
            functional_correctness: Some(1.0),
            judge_score: Some(0.2),
            ..OutcomeDimensions::default()
        };

        assert!(failing.quality_score() <= 0.45);
        assert!(risky.quality_score() <= 0.4);
        assert!(badly_reviewed.quality_score() <= 0.5);
    }

    #[test]
    fn score_normalization_rejects_nonfinite_values() {
        let mut memory = ExperienceMemory {
            confidence: f64::NAN,
            generalizability: 4.0,
            novelty: -1.0,
            outcome: OutcomeDimensions {
                functional_correctness: Some(f64::INFINITY),
                code_quality: Some(3.0),
                ..OutcomeDimensions::default()
            },
            ..ExperienceMemory::default()
        };
        memory.evaluator_scores.insert("invalid".into(), f64::NAN);
        memory.evaluator_scores.insert("valid".into(), 3.0);
        memory.clamp_scores();

        assert_eq!(memory.confidence, 0.0);
        assert_eq!(memory.generalizability, 1.0);
        assert_eq!(memory.novelty, 0.0);
        assert_eq!(memory.outcome.functional_correctness, None);
        assert_eq!(memory.outcome.code_quality, Some(1.0));
        assert!(!memory.evaluator_scores.contains_key("invalid"));
        assert_eq!(memory.evaluator_scores["valid"], 1.0);
    }

    #[test]
    fn single_observation_cannot_create_high_confidence() {
        let mut memory = ExperienceMemory::new(
            ExperienceCategory::SuccessfulPattern,
            "Extend existing visitor",
            "run-1",
            1,
        );
        memory.success = Some(true);
        memory.evidence.push(signal(EvidenceVerdict::Passed));
        memory.refresh_confidence();

        assert!(memory.confidence <= 0.65);
        assert!(memory.confidence > 0.25);
        assert_eq!(memory.evidence_count, 1);
    }

    #[test]
    fn repeated_checks_from_one_run_do_not_inflate_confidence() {
        let mut memory = ExperienceMemory::new(
            ExperienceCategory::SuccessfulPattern,
            "Extend existing visitor",
            "run-1",
            1,
        );
        memory.success = Some(true);
        let mut first = signal(EvidenceVerdict::Passed);
        first.source_run_id = Some("run-1".to_string());
        memory.evidence.push(first.clone());
        let single_run_confidence = memory.evidence_backed_confidence();

        memory.evidence.extend(std::iter::repeat_n(first, 20));

        assert_eq!(memory.evidence_backed_confidence(), single_run_confidence);
    }

    #[test]
    fn independent_source_runs_increase_confidence() {
        let mut memory = ExperienceMemory::new(
            ExperienceCategory::SuccessfulPattern,
            "Extend existing visitor",
            "run-1",
            1,
        );
        memory.success = Some(true);
        let mut first = signal(EvidenceVerdict::Passed);
        first.source_run_id = Some("run-1".to_string());
        memory.evidence.push(first.clone());
        let single_run_confidence = memory.evidence_backed_confidence();

        first.source_run_id = Some("run-2".to_string());
        memory.evidence.push(first);

        assert!(memory.evidence_backed_confidence() > single_run_confidence);
    }

    #[test]
    fn legacy_run_ids_bound_unattributed_independent_evidence() {
        let mut memory = ExperienceMemory::new(
            ExperienceCategory::SuccessfulPattern,
            "Extend existing visitor",
            "run-1",
            1,
        );
        memory.success = Some(true);
        memory
            .evidence
            .extend(std::iter::repeat_n(signal(EvidenceVerdict::Passed), 10));
        let single_run_confidence = memory.evidence_backed_confidence();

        memory.source_run_ids.push("run-2".to_string());

        assert!(memory.evidence_backed_confidence() > single_run_confidence);
    }

    #[test]
    fn contradictions_lower_confidence_below_supported_lesson() {
        let mut supported = ExperienceMemory::new(
            ExperienceCategory::SuccessfulPattern,
            "Extend existing visitor",
            "run-1",
            1,
        );
        supported.success = Some(true);
        supported.evidence.push(signal(EvidenceVerdict::Passed));

        let mut contradicted = supported.clone();
        contradicted.evidence.push(signal(EvidenceVerdict::Failed));

        assert!(contradicted.evidence_backed_confidence() < supported.evidence_backed_confidence());
    }

    #[test]
    fn failed_evidence_supports_anti_patterns() {
        let mut memory = ExperienceMemory::new(
            ExperienceCategory::FailureAntiPattern,
            "Avoid editing generated files",
            "run-1",
            1,
        );
        memory.success = Some(false);
        memory.evidence.push(signal(EvidenceVerdict::Failed));

        assert!(memory.evidence_backed_confidence() > 0.25);
    }

    #[test]
    fn reuse_updates_confidence_and_distinguishes_retrieval_from_following() {
        let mut memory = ExperienceMemory::new(
            ExperienceCategory::SuccessfulPattern,
            "Extend existing visitor",
            "run-1",
            1,
        );
        memory.success = Some(true);
        memory.evidence.push(signal(EvidenceVerdict::Passed));
        memory.refresh_confidence();
        let initial_confidence = memory.confidence;

        memory.record_retrieval(2);
        assert_eq!(memory.retrieved_count, 1);
        assert_eq!(memory.followed_count, 0);

        memory.record_followed_outcome(true, 3);
        assert_eq!(memory.followed_count, 1);
        assert_eq!(memory.successful_reuse_count, 1);
        assert!(memory.confidence > initial_confidence);

        let successful_confidence = memory.confidence;
        memory.record_followed_outcome(false, 4);
        assert_eq!(memory.failed_reuse_count, 1);
        assert!(memory.confidence < successful_confidence);
    }

    #[test]
    fn superseded_status_survives_confidence_refresh() {
        let mut memory = ExperienceMemory {
            status: ExperienceStatus::Superseded,
            ..ExperienceMemory::default()
        };
        memory.refresh_confidence();

        assert_eq!(memory.status, ExperienceStatus::Superseded);
    }

    #[test]
    fn experience_query_defaults_are_compact_and_current() {
        let before = chrono::Utc::now().timestamp();
        let query = ExperienceQuery::default();
        let after = chrono::Utc::now().timestamp();

        assert_eq!(query.limit, 6);
        assert!(!query.include_low_confidence);
        assert!((before..=after).contains(&query.now));
    }
}
