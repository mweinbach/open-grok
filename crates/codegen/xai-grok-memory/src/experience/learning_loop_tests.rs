use super::evaluation::{EvaluationRun, RetrievalAblation, evaluate_ablations};
use super::retrieval::{build_briefing, render_briefing};
use super::store::ExperienceStore;
use super::types::{
    EvidenceKind, EvidenceSignal, EvidenceVerdict, ExperienceCategory, ExperienceMemory,
    ExperienceQuery,
};
use crate::{MemoryIndex, MemoryStorage};

fn observed_experience(
    category: ExperienceCategory,
    lesson: &str,
    source_run: &str,
    successful: bool,
    timestamp: i64,
) -> ExperienceMemory {
    let mut experience = ExperienceMemory::new(category, lesson, source_run, timestamp);
    experience.repository_id = "repository-a".to_owned();
    experience.task_type = "database-migration".to_owned();
    experience.task_summary = "Fix database lock contention during schema migrations".to_owned();
    experience.success = Some(successful);
    experience.confidence = 0.8;
    experience.evidence_count = 1;
    experience.outcome.functional_correctness = Some(if successful { 1.0 } else { 0.0 });
    experience.evidence.push(EvidenceSignal {
        kind: EvidenceKind::Test,
        verdict: if successful {
            EvidenceVerdict::Passed
        } else {
            EvidenceVerdict::Failed
        },
        command: Some("cargo test schema_migrations".to_owned()),
        summary: if successful {
            "Schema migration checks passed".to_owned()
        } else {
            "Parallel schema migrations failed with database lock contention".to_owned()
        },
        score: None,
        observed_at: timestamp,
        source_run_id: Some(source_run.to_owned()),
    });

    if successful {
        experience.recommendation =
            Some("Run schema migrations serially before starting parallel workers".to_owned());
        experience.tests_run = vec!["cargo test schema_migrations".to_owned()];
    } else {
        experience.anti_pattern = Some(
            "Avoid concurrent schema migrations that cause database lock contention".to_owned(),
        );
        experience.failure_reason = Some("database is locked".to_owned());
    }

    experience
}

#[test]
fn additive_migration_preserves_existing_workspace_database_content() {
    let directory = tempfile::tempdir().unwrap();
    let workspace_directory = directory.path().join("memory");
    std::fs::create_dir_all(&workspace_directory).unwrap();
    let database_path = workspace_directory.join("index.sqlite");

    {
        let legacy_connection = rusqlite::Connection::open(&database_path).unwrap();
        legacy_connection
            .execute_batch(
                "CREATE TABLE legacy_canary (value TEXT NOT NULL);\n\
                 INSERT INTO legacy_canary VALUES ('existing memory survives');",
            )
            .unwrap();
    }

    let storage = MemoryStorage::new_flat(directory.path(), &workspace_directory);
    let legacy_index =
        MemoryIndex::open_or_create(&database_path, storage, Default::default(), 1_536).unwrap();
    drop(legacy_index);

    let store = ExperienceStore::open(&database_path).unwrap();
    let experience = observed_experience(
        ExperienceCategory::SuccessfulPattern,
        "Run schema migrations serially to avoid database locks",
        "first-run",
        true,
        1_000,
    );
    store.upsert(&experience).unwrap();

    let connection = rusqlite::Connection::open(&database_path).unwrap();
    let original: String = connection
        .query_row("SELECT value FROM legacy_canary", [], |row| row.get(0))
        .unwrap();
    assert_eq!(original, "existing memory survives");
    assert_eq!(store.all().unwrap().len(), 1);
}

#[test]
fn evidence_guides_future_planning_and_only_followed_lessons_are_reinforced() {
    let directory = tempfile::tempdir().unwrap();
    let store = ExperienceStore::open(&directory.path().join("index.sqlite")).unwrap();
    let timestamp = chrono::Utc::now().timestamp();

    let successful = observed_experience(
        ExperienceCategory::SuccessfulPattern,
        "Serial schema migrations avoid database lock contention",
        "successful-run",
        true,
        timestamp,
    );
    let failure = observed_experience(
        ExperienceCategory::FailureAntiPattern,
        "Parallel schema migrations cause database lock contention",
        "failed-run",
        false,
        timestamp,
    );
    let successful_id = store.upsert(&successful).unwrap();
    let failure_id = store.upsert(&failure).unwrap();

    let ranked = store
        .retrieve(&ExperienceQuery {
            text: "Resolve database lock contention in schema migrations".to_owned(),
            task_type: Some("database-migration".to_owned()),
            repository_id: Some("repository-a".to_owned()),
            failure_context: Some("database lock contention".to_owned()),
            now: timestamp,
            limit: 6,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(ranked.len(), 2);

    let briefing = build_briefing(&ranked, 6);
    let rendered = render_briefing(&briefing, 2_000);
    assert!(rendered.contains("Recommended"));
    assert!(rendered.contains("Avoid"));
    assert!(rendered.contains("database lock contention"));

    let retrieved_ids = ranked
        .iter()
        .map(|experience| experience.memory.id.clone())
        .collect::<Vec<_>>();
    store
        .record_retrieval("future-run", &retrieved_ids)
        .unwrap();
    store.record_followed("future-run", &successful_id).unwrap();
    store.finalize_run("future-run", true).unwrap();

    let reinforced = store.get(&successful_id).unwrap().unwrap();
    assert_eq!(reinforced.retrieved_count, 1);
    assert_eq!(reinforced.followed_count, 1);
    assert_eq!(reinforced.successful_reuse_count, 1);

    let warning = store.get(&failure_id).unwrap().unwrap();
    assert_eq!(warning.retrieved_count, 1);
    assert_eq!(warning.followed_count, 0);
    assert_eq!(warning.successful_reuse_count, 0);
}

#[test]
fn matched_ablation_reports_known_failure_avoidance_without_fabricating_quality() {
    let baseline = EvaluationRun {
        task_id: "migration-lock".to_owned(),
        repository_id: "repository-a".to_owned(),
        task_type: "database-migration".to_owned(),
        ablation: RetrievalAblation::SemanticOnly,
        success: false,
        relevant_prior_failures: 1,
        repeated_known_failures: 1,
        retries: 2,
        ..Default::default()
    };
    let improved = EvaluationRun {
        ablation: RetrievalAblation::FullExperience,
        success: true,
        repeated_known_failures: 0,
        retries: 0,
        recommendation_followed: true,
        ..baseline.clone()
    };

    let summaries = evaluate_ablations(&[baseline, improved]);
    let semantic_only = &summaries[&RetrievalAblation::SemanticOnly];
    let full_experience = &summaries[&RetrievalAblation::FullExperience];

    assert_eq!(semantic_only.repeated_failure_avoidance_rate, 0.0);
    assert_eq!(full_experience.repeated_failure_avoidance_rate, 1.0);
    assert!(full_experience.failure_avoidance_available);
    assert_eq!(full_experience.recommendation_paired_task_count, 1);
    assert!(full_experience.recommendation_utility.unwrap() > 0.0);
    assert!(full_experience.exposure_utility.unwrap() > 0.0);
    assert_eq!(full_experience.code_quality_score, None);
}
