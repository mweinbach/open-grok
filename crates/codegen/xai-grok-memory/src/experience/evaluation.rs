//! Deterministic, evidence-preserving evaluation for experience-memory retrieval.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Retrieval configurations used for controlled experience-memory ablations.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalAblation {
    #[default]
    SemanticOnly,
    SemanticOutcome,
    SemanticPositive,
    SemanticPositiveNegative,
    FullExperience,
}

/// The independently observable results of one coding-task execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvaluationRun {
    pub task_id: String,
    pub repository_id: String,
    pub repository_revision: Option<String>,
    pub environment: Option<String>,
    pub difficulty: Option<String>,
    pub task_type: String,
    pub ablation: RetrievalAblation,
    pub success: bool,
    pub test_pass_rate: Option<f64>,
    pub judge_score: Option<f64>,
    pub code_quality_score: Option<f64>,
    pub retries: u64,
    pub repeated_known_failures: u64,
    pub relevant_prior_failures: u64,
    pub tokens_consumed: u64,
    pub wall_clock_ms: u64,
    pub memory_context_tokens: u64,
    pub regression_count: u64,
    pub recommendation_followed: bool,
    pub recommendation_exposed: bool,
}

/// Aggregated evaluation results without collapsing their underlying signals.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EvaluationSummary {
    pub task_count: usize,
    pub success_rate: f64,
    pub test_pass_rate: Option<f64>,
    pub test_sample_count: usize,
    pub judge_score: Option<f64>,
    pub judge_sample_count: usize,
    pub code_quality_score: Option<f64>,
    pub code_quality_sample_count: usize,
    pub retries: f64,
    pub repeated_failure_rate: f64,
    pub repeated_failure_avoidance_rate: f64,
    pub failure_avoidance_available: bool,
    pub repeated_failure_opportunity_count: u64,
    pub repeated_failure_count: u64,
    pub tokens_consumed: f64,
    pub wall_clock_ms: f64,
    pub memory_context_tokens: f64,
    pub regression_rate: f64,
    pub regression_count: u64,
    pub recommendation_utility: Option<f64>,
    pub recommendation_paired_task_count: usize,
    pub recommendation_paired_sample_count: usize,
    pub recommendation_paired_baseline_count: usize,
    pub recommendation_paired_exposed_count: usize,
    pub recommendation_paired_followed_count: usize,
    pub exposure_utility: Option<f64>,
    pub exposure_paired_task_count: usize,
    pub exposure_paired_sample_count: usize,
    pub exposure_paired_baseline_count: usize,
    pub exposure_paired_exposed_count: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct ObservedMean {
    value: Option<f64>,
    sample_count: usize,
}

#[derive(Debug, Default)]
struct MatchedTask<'run> {
    baseline: Vec<&'run EvaluationRun>,
    exposed: Vec<&'run EvaluationRun>,
}

#[derive(Debug, Clone, Copy, Default)]
struct RecommendationStatistics {
    utility: Option<f64>,
    paired_task_count: usize,
    paired_sample_count: usize,
    paired_baseline_count: usize,
    paired_exposed_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TreatmentMode {
    Followed,
    Exposed,
}

/// Aggregate independently observed outcomes for a collection of task runs.
pub fn evaluate_runs(runs: &[EvaluationRun]) -> EvaluationSummary {
    summarize_runs(runs.iter())
}

/// Aggregate each ablation and compare its recommendations with matched baselines.
pub fn evaluate_ablations(
    runs: &[EvaluationRun],
) -> BTreeMap<RetrievalAblation, EvaluationSummary> {
    let mut grouped_runs: BTreeMap<RetrievalAblation, Vec<&EvaluationRun>> = BTreeMap::new();

    for run in runs {
        grouped_runs.entry(run.ablation).or_default().push(run);
    }

    grouped_runs
        .into_iter()
        .map(|(ablation, ablation_runs)| {
            let mut summary = summarize_runs(ablation_runs.iter().copied());

            if ablation != RetrievalAblation::SemanticOnly {
                let matching_runs = || {
                    runs.iter().filter(|run| {
                        run.ablation == RetrievalAblation::SemanticOnly || run.ablation == ablation
                    })
                };
                let followed =
                    matched_recommendation_statistics(matching_runs(), TreatmentMode::Followed);
                let exposed =
                    matched_recommendation_statistics(matching_runs(), TreatmentMode::Exposed);
                apply_recommendation_statistics(&mut summary, followed);
                apply_exposure_statistics(&mut summary, exposed);
            }

            (ablation, summary)
        })
        .collect()
}

/// Estimate recommendation-associated change using exactly matched task strata.
///
/// This is an observational association, not a claim of causal improvement.
pub fn recommendation_utility(runs: &[EvaluationRun]) -> Option<f64> {
    matched_recommendation_statistics(runs.iter(), TreatmentMode::Followed).utility
}

/// Estimate retrieval-exposure association regardless of recommendation follow-through.
///
/// This intent-to-treat association must not be interpreted as guidance effectiveness.
pub fn exposure_utility(runs: &[EvaluationRun]) -> Option<f64> {
    matched_recommendation_statistics(runs.iter(), TreatmentMode::Exposed).utility
}

fn summarize_runs<'run>(runs: impl IntoIterator<Item = &'run EvaluationRun>) -> EvaluationSummary {
    let runs: Vec<&EvaluationRun> = runs.into_iter().collect();
    if runs.is_empty() {
        return EvaluationSummary::default();
    }

    let task_count = runs.len();
    let task_count_float = task_count as f64;
    let successful_tasks = runs.iter().filter(|run| run.success).count();
    let regressed_tasks = runs.iter().filter(|run| run.regression_count > 0).count();
    let test_scores = observed_mean(&runs, |run| run.test_pass_rate, true);
    let judge_scores = observed_mean(
        &runs,
        |run| run.judge_score.and_then(normalize_judge_score),
        true,
    );
    let quality_scores = observed_mean(&runs, |run| run.code_quality_score, true);

    let mut retry_count = 0_u64;
    let mut token_count = 0_u64;
    let mut elapsed_ms = 0_u64;
    let mut memory_token_count = 0_u64;
    let mut regression_count = 0_u64;
    let mut failure_opportunities = 0_u64;
    let mut repeated_failures = 0_u64;

    for run in &runs {
        retry_count = retry_count.saturating_add(run.retries);
        token_count = token_count.saturating_add(run.tokens_consumed);
        elapsed_ms = elapsed_ms.saturating_add(run.wall_clock_ms);
        memory_token_count = memory_token_count.saturating_add(run.memory_context_tokens);
        regression_count = regression_count.saturating_add(run.regression_count);
        failure_opportunities = failure_opportunities.saturating_add(run.relevant_prior_failures);
        repeated_failures = repeated_failures
            .saturating_add(run.repeated_known_failures.min(run.relevant_prior_failures));
    }

    let repeated_failure_rate = if failure_opportunities == 0 {
        0.0
    } else {
        repeated_failures as f64 / failure_opportunities as f64
    };

    let mut summary = EvaluationSummary {
        task_count,
        success_rate: successful_tasks as f64 / task_count_float,
        test_pass_rate: test_scores.value,
        test_sample_count: test_scores.sample_count,
        judge_score: judge_scores.value,
        judge_sample_count: judge_scores.sample_count,
        code_quality_score: quality_scores.value,
        code_quality_sample_count: quality_scores.sample_count,
        retries: retry_count as f64 / task_count_float,
        repeated_failure_rate,
        repeated_failure_avoidance_rate: if failure_opportunities == 0 {
            0.0
        } else {
            1.0 - repeated_failure_rate
        },
        failure_avoidance_available: failure_opportunities > 0,
        repeated_failure_opportunity_count: failure_opportunities,
        repeated_failure_count: repeated_failures,
        tokens_consumed: token_count as f64 / task_count_float,
        wall_clock_ms: elapsed_ms as f64 / task_count_float,
        memory_context_tokens: memory_token_count as f64 / task_count_float,
        regression_rate: regressed_tasks as f64 / task_count_float,
        regression_count,
        ..EvaluationSummary::default()
    };

    let followed = matched_recommendation_statistics(runs.iter().copied(), TreatmentMode::Followed);
    let exposed = matched_recommendation_statistics(runs.iter().copied(), TreatmentMode::Exposed);
    apply_recommendation_statistics(&mut summary, followed);
    apply_exposure_statistics(&mut summary, exposed);
    summary
}

fn apply_recommendation_statistics(
    summary: &mut EvaluationSummary,
    statistics: RecommendationStatistics,
) {
    summary.recommendation_utility = statistics.utility;
    summary.recommendation_paired_task_count = statistics.paired_task_count;
    summary.recommendation_paired_sample_count = statistics.paired_sample_count;
    summary.recommendation_paired_baseline_count = statistics.paired_baseline_count;
    summary.recommendation_paired_exposed_count = statistics.paired_exposed_count;
    summary.recommendation_paired_followed_count = statistics.paired_exposed_count;
}

fn apply_exposure_statistics(
    summary: &mut EvaluationSummary,
    statistics: RecommendationStatistics,
) {
    summary.exposure_utility = statistics.utility;
    summary.exposure_paired_task_count = statistics.paired_task_count;
    summary.exposure_paired_sample_count = statistics.paired_sample_count;
    summary.exposure_paired_baseline_count = statistics.paired_baseline_count;
    summary.exposure_paired_exposed_count = statistics.paired_exposed_count;
}

fn observed_mean(
    runs: &[&EvaluationRun],
    select_score: impl Fn(&EvaluationRun) -> Option<f64>,
    unit_interval_only: bool,
) -> ObservedMean {
    let mut mean = 0.0_f64;
    let mut sample_count = 0_usize;

    for run in runs {
        let Some(score) = select_score(run) else {
            continue;
        };

        if !score.is_finite() || (unit_interval_only && !(0.0..=1.0).contains(&score)) {
            continue;
        }

        sample_count = sample_count.saturating_add(1);
        let weight = 1.0 / sample_count as f64;
        mean = mean.mul_add(1.0 - weight, score * weight);
    }

    ObservedMean {
        value: (sample_count > 0 && mean.is_finite()).then_some(mean),
        sample_count,
    }
}

fn matched_recommendation_statistics<'run>(
    runs: impl IntoIterator<Item = &'run EvaluationRun>,
    treatment_mode: TreatmentMode,
) -> RecommendationStatistics {
    let mut matched_tasks: BTreeMap<
        (&str, &str, &str, Option<&str>, Option<&str>, Option<&str>),
        MatchedTask<'run>,
    > = BTreeMap::new();

    for run in runs {
        if run.repository_id.is_empty() || run.task_type.is_empty() || run.task_id.is_empty() {
            continue;
        }

        let key = (
            run.repository_id.as_str(),
            run.task_type.as_str(),
            run.task_id.as_str(),
            run.repository_revision.as_deref(),
            run.environment.as_deref(),
            run.difficulty.as_deref(),
        );
        let recommendation_exposed = run.recommendation_followed || run.recommendation_exposed;
        let qualifies_for_treatment = match treatment_mode {
            TreatmentMode::Followed => run.recommendation_followed,
            TreatmentMode::Exposed => recommendation_exposed,
        };

        if run.ablation == RetrievalAblation::SemanticOnly && !recommendation_exposed {
            matched_tasks.entry(key).or_default().baseline.push(run);
        } else if run.ablation != RetrievalAblation::SemanticOnly && qualifies_for_treatment {
            matched_tasks.entry(key).or_default().exposed.push(run);
        }
    }

    let mut statistics = RecommendationStatistics::default();
    let mut average_delta = 0.0_f64;

    for matched_task in matched_tasks.values() {
        if matched_task.baseline.is_empty() || matched_task.exposed.is_empty() {
            continue;
        }

        let Some(delta) = matched_task_delta(matched_task) else {
            continue;
        };

        statistics.paired_task_count = statistics.paired_task_count.saturating_add(1);
        statistics.paired_sample_count = statistics
            .paired_sample_count
            .saturating_add(matched_task.baseline.len().min(matched_task.exposed.len()));
        statistics.paired_baseline_count = statistics
            .paired_baseline_count
            .saturating_add(matched_task.baseline.len());
        statistics.paired_exposed_count = statistics
            .paired_exposed_count
            .saturating_add(matched_task.exposed.len());

        let weight = 1.0 / statistics.paired_task_count as f64;
        average_delta = average_delta.mul_add(1.0 - weight, delta * weight);
    }

    if statistics.paired_task_count > 0 && average_delta.is_finite() {
        statistics.utility = Some(average_delta.clamp(-1.0, 1.0));
    }

    statistics
}

fn matched_task_delta(matched_task: &MatchedTask<'_>) -> Option<f64> {
    let mut weighted_delta = 0.0;
    let mut total_weight = 0.0;

    append_weighted_delta(
        &mut weighted_delta,
        &mut total_weight,
        0.50,
        proportion(&matched_task.exposed, |run| run.success)
            - proportion(&matched_task.baseline, |run| run.success),
    );

    append_optional_delta(
        &mut weighted_delta,
        &mut total_weight,
        0.20,
        observed_mean(&matched_task.baseline, |run| run.test_pass_rate, true).value,
        observed_mean(&matched_task.exposed, |run| run.test_pass_rate, true).value,
    );

    append_optional_delta(
        &mut weighted_delta,
        &mut total_weight,
        0.20,
        observed_mean(&matched_task.baseline, |run| run.code_quality_score, true).value,
        observed_mean(&matched_task.exposed, |run| run.code_quality_score, true).value,
    );

    append_optional_delta(
        &mut weighted_delta,
        &mut total_weight,
        0.10,
        observed_mean(
            &matched_task.baseline,
            |run| run.judge_score.and_then(normalize_judge_score),
            true,
        )
        .value,
        observed_mean(
            &matched_task.exposed,
            |run| run.judge_score.and_then(normalize_judge_score),
            true,
        )
        .value,
    );

    append_weighted_delta(
        &mut weighted_delta,
        &mut total_weight,
        0.15,
        proportion(&matched_task.exposed, |run| run.regression_count == 0)
            - proportion(&matched_task.baseline, |run| run.regression_count == 0),
    );

    let baseline_retry_efficiency = bounded_retry_efficiency(&matched_task.baseline);
    let exposed_retry_efficiency = bounded_retry_efficiency(&matched_task.exposed);
    append_weighted_delta(
        &mut weighted_delta,
        &mut total_weight,
        0.05,
        exposed_retry_efficiency - baseline_retry_efficiency,
    );

    (total_weight > 0.0 && weighted_delta.is_finite()).then_some(weighted_delta / total_weight)
}

fn append_optional_delta(
    weighted_delta: &mut f64,
    total_weight: &mut f64,
    weight: f64,
    baseline: Option<f64>,
    exposed: Option<f64>,
) {
    if let (Some(baseline), Some(exposed)) = (baseline, exposed) {
        append_weighted_delta(weighted_delta, total_weight, weight, exposed - baseline);
    }
}

fn normalize_judge_score(score: f64) -> Option<f64> {
    if !score.is_finite() || score < 0.0 {
        None
    } else if score <= 1.0 {
        Some(score)
    } else if score <= 100.0 {
        Some(score / 100.0)
    } else {
        None
    }
}

fn append_weighted_delta(
    weighted_delta: &mut f64,
    total_weight: &mut f64,
    weight: f64,
    delta: f64,
) {
    if delta.is_finite() {
        *weighted_delta = weight.mul_add(delta, *weighted_delta);
        *total_weight += weight;
    }
}

fn proportion(runs: &[&EvaluationRun], predicate: impl Fn(&EvaluationRun) -> bool) -> f64 {
    if runs.is_empty() {
        return 0.0;
    }

    runs.iter().filter(|run| predicate(run)).count() as f64 / runs.len() as f64
}

fn bounded_retry_efficiency(runs: &[&EvaluationRun]) -> f64 {
    if runs.is_empty() {
        return 0.0;
    }

    let mut mean = 0.0_f64;

    for (index, run) in runs.iter().enumerate() {
        let efficiency = 1.0 / (1.0 + run.retries as f64);
        let weight = 1.0 / (index + 1) as f64;
        mean = mean.mul_add(1.0 - weight, efficiency * weight);
    }

    mean
}

#[cfg(test)]
mod tests {
    use super::{
        EvaluationRun, RetrievalAblation, evaluate_ablations, evaluate_runs, exposure_utility,
        recommendation_utility,
    };

    fn run(task_id: &str, ablation: RetrievalAblation) -> EvaluationRun {
        EvaluationRun {
            task_id: task_id.to_owned(),
            repository_id: "repository-a".to_owned(),
            task_type: "schema-migration".to_owned(),
            ablation,
            ..EvaluationRun::default()
        }
    }

    fn assert_approximately(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-10,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn empty_evaluation_has_no_fabricated_evidence() {
        let summary = evaluate_runs(&[]);

        assert_eq!(summary.task_count, 0);
        assert_eq!(summary.success_rate, 0.0);
        assert_eq!(summary.test_pass_rate, None);
        assert_eq!(summary.judge_score, None);
        assert_eq!(summary.code_quality_score, None);
        assert_eq!(summary.repeated_failure_rate, 0.0);
        assert_eq!(summary.repeated_failure_avoidance_rate, 0.0);
        assert!(!summary.failure_avoidance_available);
        assert_eq!(summary.recommendation_utility, None);
        assert_eq!(summary.exposure_utility, None);
        assert!(evaluate_ablations(&[]).is_empty());
    }

    #[test]
    fn summary_preserves_multidimensional_signals_and_observation_counts() {
        let mut passing = run("task-a", RetrievalAblation::SemanticOnly);
        passing.success = true;
        passing.test_pass_rate = Some(1.0);
        passing.judge_score = Some(0.8);
        passing.code_quality_score = Some(0.4);
        passing.retries = 2;
        passing.repeated_known_failures = 1;
        passing.relevant_prior_failures = 4;
        passing.tokens_consumed = 120;
        passing.wall_clock_ms = 200;
        passing.memory_context_tokens = 12;

        let mut failing = run("task-b", RetrievalAblation::SemanticOnly);
        failing.test_pass_rate = Some(0.5);
        failing.code_quality_score = Some(0.9);
        failing.retries = 4;
        failing.repeated_known_failures = 2;
        failing.relevant_prior_failures = 2;
        failing.tokens_consumed = 180;
        failing.wall_clock_ms = 400;
        failing.memory_context_tokens = 18;
        failing.regression_count = 3;

        let summary = evaluate_runs(&[passing, failing]);

        assert_eq!(summary.task_count, 2);
        assert_approximately(summary.success_rate, 0.5);
        assert_approximately(summary.test_pass_rate.unwrap(), 0.75);
        assert_eq!(summary.test_sample_count, 2);
        assert_approximately(summary.judge_score.unwrap(), 0.8);
        assert_eq!(summary.judge_sample_count, 1);
        assert_approximately(summary.code_quality_score.unwrap(), 0.65);
        assert_eq!(summary.code_quality_sample_count, 2);
        assert_approximately(summary.retries, 3.0);
        assert_approximately(summary.repeated_failure_rate, 0.5);
        assert_approximately(summary.repeated_failure_avoidance_rate, 0.5);
        assert!(summary.failure_avoidance_available);
        assert_eq!(summary.repeated_failure_opportunity_count, 6);
        assert_eq!(summary.repeated_failure_count, 3);
        assert_approximately(summary.tokens_consumed, 150.0);
        assert_approximately(summary.wall_clock_ms, 300.0);
        assert_approximately(summary.memory_context_tokens, 15.0);
        assert_approximately(summary.regression_rate, 0.5);
        assert_eq!(summary.regression_count, 3);
    }

    #[test]
    fn repeated_failure_rate_weights_opportunities_not_task_averages() {
        let mut many_opportunities = run("task-a", RetrievalAblation::SemanticOnly);
        many_opportunities.relevant_prior_failures = 9;
        many_opportunities.repeated_known_failures = 9;

        let mut single_opportunity = run("task-b", RetrievalAblation::SemanticOnly);
        single_opportunity.relevant_prior_failures = 1;

        let summary = evaluate_runs(&[many_opportunities, single_opportunity]);

        assert_approximately(summary.repeated_failure_rate, 0.9);
        assert_approximately(summary.repeated_failure_avoidance_rate, 0.1);
    }

    #[test]
    fn impossible_failure_counts_cannot_create_negative_avoidance() {
        let mut invalid = run("task-a", RetrievalAblation::SemanticOnly);
        invalid.relevant_prior_failures = 2;
        invalid.repeated_known_failures = 20;

        let summary = evaluate_runs(&[invalid]);

        assert_eq!(summary.repeated_failure_count, 2);
        assert_approximately(summary.repeated_failure_rate, 1.0);
        assert_approximately(summary.repeated_failure_avoidance_rate, 0.0);
    }

    #[test]
    fn missing_failure_opportunities_do_not_imply_perfect_avoidance() {
        let summary = evaluate_runs(&[run("task-a", RetrievalAblation::SemanticOnly)]);

        assert_eq!(summary.repeated_failure_opportunity_count, 0);
        assert_eq!(summary.repeated_failure_rate, 0.0);
        assert_eq!(summary.repeated_failure_avoidance_rate, 0.0);
        assert!(!summary.failure_avoidance_available);
    }

    #[test]
    fn malformed_scores_are_ignored_without_nan_or_infinity() {
        let mut invalid = run("task-a", RetrievalAblation::SemanticOnly);
        invalid.test_pass_rate = Some(f64::NAN);
        invalid.judge_score = Some(f64::INFINITY);
        invalid.code_quality_score = Some(f64::NEG_INFINITY);

        let mut invalid_rate = run("task-b", RetrievalAblation::SemanticOnly);
        invalid_rate.test_pass_rate = Some(2.0);
        invalid_rate.judge_score = Some(-0.1);
        invalid_rate.code_quality_score = Some(1.1);

        let mut invalid_bounds = run("task-c", RetrievalAblation::SemanticOnly);
        invalid_bounds.test_pass_rate = Some(-0.1);
        invalid_bounds.judge_score = Some(101.0);
        invalid_bounds.code_quality_score = Some(-0.1);

        let mut valid = run("task-d", RetrievalAblation::SemanticOnly);
        valid.test_pass_rate = Some(0.75);
        valid.judge_score = Some(92.0);
        valid.code_quality_score = Some(0.6);

        let summary = evaluate_runs(&[invalid, invalid_rate, invalid_bounds, valid]);

        assert_eq!(summary.test_sample_count, 1);
        assert_approximately(summary.test_pass_rate.unwrap(), 0.75);
        assert_eq!(summary.judge_sample_count, 1);
        assert_approximately(summary.judge_score.unwrap(), 0.92);
        assert_eq!(summary.code_quality_sample_count, 1);
        assert_approximately(summary.code_quality_score.unwrap(), 0.6);
        assert!(summary.success_rate.is_finite());
        assert!(summary.repeated_failure_rate.is_finite());
    }

    #[test]
    fn invalid_score_dimensions_remain_missing_without_observations() {
        let mut below_bounds = run("task-a", RetrievalAblation::SemanticOnly);
        below_bounds.test_pass_rate = Some(-0.1);
        below_bounds.judge_score = Some(-0.1);
        below_bounds.code_quality_score = Some(-0.1);

        let mut above_bounds = run("task-b", RetrievalAblation::SemanticOnly);
        above_bounds.test_pass_rate = Some(1.1);
        above_bounds.judge_score = Some(100.1);
        above_bounds.code_quality_score = Some(1.1);

        let mut nonfinite = run("task-c", RetrievalAblation::SemanticOnly);
        nonfinite.test_pass_rate = Some(f64::INFINITY);
        nonfinite.judge_score = Some(f64::NAN);
        nonfinite.code_quality_score = Some(f64::NEG_INFINITY);

        let summary = evaluate_runs(&[below_bounds, above_bounds, nonfinite]);

        assert_eq!(summary.test_pass_rate, None);
        assert_eq!(summary.test_sample_count, 0);
        assert_eq!(summary.judge_score, None);
        assert_eq!(summary.judge_sample_count, 0);
        assert_eq!(summary.code_quality_score, None);
        assert_eq!(summary.code_quality_sample_count, 0);
    }

    #[test]
    fn grouped_ablation_summaries_cover_every_retrieval_configuration() {
        let configurations = [
            RetrievalAblation::SemanticOnly,
            RetrievalAblation::SemanticOutcome,
            RetrievalAblation::SemanticPositive,
            RetrievalAblation::SemanticPositiveNegative,
            RetrievalAblation::FullExperience,
        ];
        let runs: Vec<EvaluationRun> = configurations
            .into_iter()
            .map(|ablation| run("task-a", ablation))
            .collect();

        let summaries = evaluate_ablations(&runs);

        assert_eq!(summaries.len(), configurations.len());
        for ablation in configurations {
            assert_eq!(summaries[&ablation].task_count, 1);
        }
    }

    #[test]
    fn negative_experience_fixture_improves_repeated_failure_avoidance() {
        let mut runs = Vec::new();

        for task_id in ["generated-schema", "migration-lock", "registry-extension"] {
            let mut semantic = run(task_id, RetrievalAblation::SemanticOnly);
            semantic.relevant_prior_failures = 1;
            semantic.repeated_known_failures = 1;
            semantic.retries = 2;
            runs.push(semantic);

            let mut positive = run(task_id, RetrievalAblation::SemanticPositive);
            positive.recommendation_followed = true;
            positive.relevant_prior_failures = 1;
            positive.repeated_known_failures = u64::from(task_id != "registry-extension");
            positive.success = task_id == "registry-extension";
            runs.push(positive);

            let mut positive_negative = run(task_id, RetrievalAblation::SemanticPositiveNegative);
            positive_negative.recommendation_followed = true;
            positive_negative.relevant_prior_failures = 1;
            positive_negative.success = true;
            positive_negative.code_quality_score = Some(0.85);
            runs.push(positive_negative);
        }

        let summaries = evaluate_ablations(&runs);
        let baseline = &summaries[&RetrievalAblation::SemanticOnly];
        let positive = &summaries[&RetrievalAblation::SemanticPositive];
        let positive_negative = &summaries[&RetrievalAblation::SemanticPositiveNegative];

        assert_approximately(baseline.repeated_failure_avoidance_rate, 0.0);
        assert!(
            positive.repeated_failure_avoidance_rate > baseline.repeated_failure_avoidance_rate
        );
        assert!(
            positive_negative.repeated_failure_avoidance_rate
                > positive.repeated_failure_avoidance_rate
        );
        assert_approximately(positive_negative.repeated_failure_avoidance_rate, 1.0);
        assert_eq!(positive_negative.recommendation_paired_task_count, 3);
        assert!(positive_negative.recommendation_utility.unwrap() > 0.0);
    }

    #[test]
    fn passing_poor_quality_run_does_not_fabricate_positive_utility() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.success = true;
        baseline.test_pass_rate = Some(1.0);
        baseline.judge_score = Some(0.95);
        baseline.code_quality_score = Some(0.95);

        let mut exposed = run("task-a", RetrievalAblation::FullExperience);
        exposed.success = true;
        exposed.test_pass_rate = Some(1.0);
        exposed.judge_score = Some(0.20);
        exposed.code_quality_score = Some(0.10);
        exposed.recommendation_followed = true;

        let summary = evaluate_runs(&[baseline, exposed]);

        assert_approximately(summary.success_rate, 1.0);
        assert!(summary.code_quality_score.unwrap() < 0.6);
        assert!(summary.recommendation_utility.unwrap() < 0.0);
    }

    #[test]
    fn recommendation_utility_requires_exact_complete_task_matching() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.success = false;

        let mut wrong_repository = run("task-a", RetrievalAblation::FullExperience);
        wrong_repository.repository_id = "repository-b".to_owned();
        wrong_repository.success = true;
        wrong_repository.recommendation_followed = true;

        let mut wrong_task_type = run("task-a", RetrievalAblation::FullExperience);
        wrong_task_type.task_type = "frontend".to_owned();
        wrong_task_type.success = true;
        wrong_task_type.recommendation_followed = true;

        let mut wrong_task = run("task-b", RetrievalAblation::FullExperience);
        wrong_task.success = true;
        wrong_task.recommendation_followed = true;

        let mut unidentified = run("", RetrievalAblation::FullExperience);
        unidentified.success = true;
        unidentified.recommendation_followed = true;

        assert_eq!(
            recommendation_utility(&[
                baseline,
                wrong_repository,
                wrong_task_type,
                wrong_task,
                unidentified,
            ]),
            None
        );
    }

    #[test]
    fn recommendation_utility_weights_matched_tasks_equally() {
        let mut runs = Vec::new();

        let mut first_baseline = run("task-a", RetrievalAblation::SemanticOnly);
        first_baseline.success = false;
        runs.push(first_baseline);

        for _ in 0..20 {
            let mut first_exposed = run("task-a", RetrievalAblation::FullExperience);
            first_exposed.success = true;
            first_exposed.recommendation_followed = true;
            runs.push(first_exposed);
        }

        let mut second_baseline = run("task-b", RetrievalAblation::SemanticOnly);
        second_baseline.success = true;
        runs.push(second_baseline);

        let mut second_exposed = run("task-b", RetrievalAblation::FullExperience);
        second_exposed.success = false;
        second_exposed.recommendation_followed = true;
        runs.push(second_exposed);

        let summary = evaluate_runs(&runs);

        assert_approximately(summary.recommendation_utility.unwrap(), 0.0);
        assert_eq!(summary.recommendation_paired_task_count, 2);
        assert_eq!(summary.recommendation_paired_sample_count, 2);
        assert_eq!(summary.recommendation_paired_baseline_count, 2);
        assert_eq!(summary.recommendation_paired_exposed_count, 21);
        assert_eq!(summary.recommendation_paired_followed_count, 21);
    }

    #[test]
    fn unexposed_recommendations_are_not_counted_as_treatment() {
        let baseline = run("task-a", RetrievalAblation::SemanticOnly);
        let mut unexposed = run("task-a", RetrievalAblation::FullExperience);
        unexposed.success = true;

        assert_eq!(recommendation_utility(&[baseline, unexposed]), None);
    }

    #[test]
    fn exposure_without_follow_through_is_not_recommendation_utility() {
        let baseline = run("task-a", RetrievalAblation::SemanticOnly);
        let mut exposed = run("task-a", RetrievalAblation::FullExperience);
        exposed.success = true;
        exposed.recommendation_exposed = true;

        let runs = [baseline, exposed];
        let summary = evaluate_runs(&runs);

        assert_eq!(recommendation_utility(&runs), None);
        assert_eq!(summary.recommendation_utility, None);
        assert_eq!(summary.recommendation_paired_task_count, 0);
        assert_eq!(summary.recommendation_paired_followed_count, 0);
        assert!(exposure_utility(&runs).unwrap() > 0.0);
        assert!(summary.exposure_utility.unwrap() > 0.0);
        assert_eq!(summary.exposure_paired_task_count, 1);
        assert_eq!(summary.exposure_paired_exposed_count, 1);
    }

    #[test]
    fn followed_and_exposed_associations_report_distinct_cohorts() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.success = true;

        let mut merely_exposed = run("task-a", RetrievalAblation::FullExperience);
        merely_exposed.success = false;
        merely_exposed.recommendation_exposed = true;

        let mut followed = run("task-a", RetrievalAblation::FullExperience);
        followed.success = true;
        followed.recommendation_followed = true;

        let summary = evaluate_runs(&[baseline, merely_exposed, followed]);

        assert_approximately(summary.recommendation_utility.unwrap(), 0.0);
        assert!(summary.exposure_utility.unwrap() < 0.0);
        assert_eq!(summary.recommendation_paired_followed_count, 1);
        assert_eq!(summary.exposure_paired_exposed_count, 2);
    }

    #[test]
    fn matching_rejects_different_revisions_environments_and_difficulties() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.repository_revision = Some("revision-a".to_owned());
        baseline.environment = Some("linux-ci".to_owned());
        baseline.difficulty = Some("hard".to_owned());

        let mut wrong_revision = run("task-a", RetrievalAblation::FullExperience);
        wrong_revision.repository_revision = Some("revision-b".to_owned());
        wrong_revision.environment = Some("linux-ci".to_owned());
        wrong_revision.difficulty = Some("hard".to_owned());
        wrong_revision.success = true;
        wrong_revision.recommendation_followed = true;

        let mut wrong_environment = run("task-a", RetrievalAblation::FullExperience);
        wrong_environment.repository_revision = Some("revision-a".to_owned());
        wrong_environment.environment = Some("macos-local".to_owned());
        wrong_environment.difficulty = Some("hard".to_owned());
        wrong_environment.success = true;
        wrong_environment.recommendation_followed = true;

        let mut wrong_difficulty = run("task-a", RetrievalAblation::FullExperience);
        wrong_difficulty.repository_revision = Some("revision-a".to_owned());
        wrong_difficulty.environment = Some("linux-ci".to_owned());
        wrong_difficulty.difficulty = Some("easy".to_owned());
        wrong_difficulty.success = true;
        wrong_difficulty.recommendation_followed = true;

        let mut missing_context = run("task-a", RetrievalAblation::FullExperience);
        missing_context.success = true;
        missing_context.recommendation_followed = true;

        let runs = [
            baseline,
            wrong_revision,
            wrong_environment,
            wrong_difficulty,
            missing_context,
        ];

        assert_eq!(recommendation_utility(&runs), None);
        assert_eq!(exposure_utility(&runs), None);
    }

    #[test]
    fn matching_accepts_identical_revision_environment_and_difficulty() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.repository_revision = Some("revision-a".to_owned());
        baseline.environment = Some("linux-ci".to_owned());
        baseline.difficulty = Some("hard".to_owned());

        let mut followed = run("task-a", RetrievalAblation::FullExperience);
        followed.repository_revision = Some("revision-a".to_owned());
        followed.environment = Some("linux-ci".to_owned());
        followed.difficulty = Some("hard".to_owned());
        followed.success = true;
        followed.recommendation_followed = true;

        let summary = evaluate_runs(&[baseline, followed]);

        assert!(summary.recommendation_utility.unwrap() > 0.0);
        assert_eq!(summary.recommendation_paired_task_count, 1);
    }

    #[test]
    fn ablation_utility_uses_only_its_own_matched_exposures() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.success = false;

        let mut positive = run("task-a", RetrievalAblation::SemanticPositive);
        positive.success = false;
        positive.recommendation_followed = true;

        let mut full = run("task-a", RetrievalAblation::FullExperience);
        full.success = true;
        full.recommendation_followed = true;

        let summaries = evaluate_ablations(&[baseline, positive, full]);

        assert_eq!(
            summaries[&RetrievalAblation::SemanticOnly].recommendation_utility,
            None
        );
        assert_approximately(
            summaries[&RetrievalAblation::SemanticPositive]
                .recommendation_utility
                .unwrap(),
            0.0,
        );
        assert!(
            summaries[&RetrievalAblation::FullExperience]
                .recommendation_utility
                .unwrap()
                > 0.0
        );
    }

    #[test]
    fn percentage_judge_scores_remain_visible_and_reduce_utility() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.judge_score = Some(95.0);

        let mut exposed = run("task-a", RetrievalAblation::FullExperience);
        exposed.judge_score = Some(20.0);
        exposed.recommendation_followed = true;

        let summary = evaluate_runs(&[baseline, exposed]);

        assert_approximately(summary.judge_score.unwrap(), 0.575);
        assert_eq!(summary.judge_sample_count, 2);
        assert!(summary.recommendation_utility.unwrap() < 0.0);
    }

    #[test]
    fn mixed_unit_and_percentage_judge_scores_use_comparable_scales() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.judge_score = Some(0.95);

        let mut followed = run("task-a", RetrievalAblation::FullExperience);
        followed.judge_score = Some(20.0);
        followed.recommendation_followed = true;

        let summary = evaluate_runs(&[baseline, followed]);

        assert_approximately(summary.judge_score.unwrap(), 0.575);
        assert_eq!(summary.judge_sample_count, 2);
        assert!(summary.recommendation_utility.unwrap() < 0.0);
    }

    #[test]
    fn invalid_percentage_judge_scores_do_not_poison_utility() {
        let mut baseline = run("task-a", RetrievalAblation::SemanticOnly);
        baseline.judge_score = Some(95.0);

        let mut followed = run("task-a", RetrievalAblation::FullExperience);
        followed.judge_score = Some(101.0);
        followed.recommendation_followed = true;

        let summary = evaluate_runs(&[baseline, followed]);

        assert_approximately(summary.recommendation_utility.unwrap(), 0.0);
        assert_approximately(summary.judge_score.unwrap(), 0.95);
        assert_eq!(summary.judge_sample_count, 1);
    }

    #[test]
    fn regression_rate_counts_affected_runs_not_total_regressions() {
        let mut regressed = run("task-a", RetrievalAblation::SemanticOnly);
        regressed.regression_count = 8;

        let clean = run("task-b", RetrievalAblation::SemanticOnly);
        let summary = evaluate_runs(&[regressed, clean]);

        assert_approximately(summary.regression_rate, 0.5);
        assert_eq!(summary.regression_count, 8);
    }

    #[test]
    fn saturating_counters_cannot_overflow_summary_aggregation() {
        let mut first = run("task-a", RetrievalAblation::SemanticOnly);
        first.tokens_consumed = u64::MAX;
        first.relevant_prior_failures = u64::MAX;
        first.repeated_known_failures = u64::MAX;

        let mut second = run("task-b", RetrievalAblation::SemanticOnly);
        second.tokens_consumed = u64::MAX;
        second.relevant_prior_failures = u64::MAX;
        second.repeated_known_failures = u64::MAX;

        let summary = evaluate_runs(&[first, second]);

        assert!(summary.tokens_consumed.is_finite());
        assert_approximately(summary.repeated_failure_rate, 1.0);
        assert_eq!(summary.repeated_failure_opportunity_count, u64::MAX);
    }

    #[test]
    fn evaluation_types_round_trip_and_accept_older_missing_fields() {
        let mut original = run("task-a", RetrievalAblation::SemanticPositiveNegative);
        original.success = true;
        original.recommendation_followed = true;

        let serialized = serde_json::to_value(&original).unwrap();
        assert_eq!(serialized["ablation"], "semantic_positive_negative");

        let decoded: EvaluationRun = serde_json::from_value(serialized).unwrap();
        assert_eq!(decoded, original);

        let older: EvaluationRun = serde_json::from_value(serde_json::json!({
            "task_id": "older-task"
        }))
        .unwrap();
        assert_eq!(older.task_id, "older-task");
        assert_eq!(older.ablation, RetrievalAblation::SemanticOnly);
        assert!(!older.recommendation_exposed);
        assert_eq!(older.repository_revision, None);
        assert_eq!(older.environment, None);
        assert_eq!(older.difficulty, None);

        let summary = evaluate_runs(&[original]);
        let encoded_summary = serde_json::to_value(&summary).unwrap();
        let decoded_summary = serde_json::from_value(encoded_summary).unwrap();
        assert_eq!(summary, decoded_summary);
    }
}
