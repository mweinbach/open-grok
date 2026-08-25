use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Map, Value};

use super::extraction::{is_sensitive_field_name, redact_sensitive_text};
use super::retrieval::rank_experiences;
use super::types::{
    EvidenceSignal, EvidenceVerdict, ExperienceCategory, ExperienceMemory, ExperienceQuery,
    ExperienceScope, ExperienceStatus, RankedExperience,
};

const MAX_ACTIVE_EXPERIENCES: usize = 2_048;
const MAX_TOTAL_EXPERIENCES: usize = 4_096;
const MAX_FINALIZED_REUSE_ROWS: usize = 8_192;
const MAX_FINALIZED_RUN_TOMBSTONES: usize = 16_384;
const MAX_SOURCE_SESSION_REFERENCES: usize = 16_384;
const MAX_PENDING_REUSE_ROWS: usize = 4_096;
const MAX_CANDIDATES: usize = 256;
const MAX_CONSOLIDATION_CANDIDATES: usize = 64;
const MAX_EVIDENCE_ITEMS: usize = 64;
const MAX_SOURCE_RUNS: usize = 128;
const SOURCE_PROVENANCE_BYTES: usize = 512;
const SOURCE_PROVENANCE_HASHES: usize = 4;
const MAX_PERSISTED_FIELD_CHARS: usize = 8_192;
const MAX_INDEXED_FIELD_CHARS: usize = 2_048;
const MAX_IDENTITY_FIELD_CHARS: usize = 512;
const CONSOLIDATION_SIMILARITY: f64 = 0.78;

pub(crate) const EXPERIENCE_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS experiences (
    id TEXT PRIMARY KEY,
    category TEXT NOT NULL,
    repository_id TEXT NOT NULL,
    scope TEXT NOT NULL,
    task_type TEXT NOT NULL,
    lesson TEXT NOT NULL,
    status TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0.0,
    generalizability REAL NOT NULL DEFAULT 0.0,
    evidence_count INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_used_at INTEGER,
    retrieved_count INTEGER NOT NULL DEFAULT 0,
    followed_count INTEGER NOT NULL DEFAULT 0,
    successful_reuse_count INTEGER NOT NULL DEFAULT 0,
    failed_reuse_count INTEGER NOT NULL DEFAULT 0,
    superseded_by TEXT,
    record_json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_experiences_repository_status
    ON experiences(repository_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_experiences_consolidation
    ON experiences(category, repository_id, scope, task_type, status);
CREATE INDEX IF NOT EXISTS idx_experiences_quality
    ON experiences(status, confidence DESC, evidence_count DESC, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_experiences_updated_at
    ON experiences(updated_at DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS experience_fts USING fts5(
    experience_id UNINDEXED,
    lesson,
    task_summary,
    strategy,
    recommendation,
    anti_pattern,
    tokenize = 'unicode61'
);

CREATE TABLE IF NOT EXISTS experience_reuse (
    run_id TEXT NOT NULL,
    experience_id TEXT NOT NULL,
    retrieved_at INTEGER NOT NULL,
    followed_at INTEGER,
    finalized_at INTEGER,
    successful INTEGER,
    PRIMARY KEY (run_id, experience_id)
);

CREATE INDEX IF NOT EXISTS idx_experience_reuse_experience
    ON experience_reuse(experience_id, finalized_at);
CREATE INDEX IF NOT EXISTS idx_experience_reuse_run
    ON experience_reuse(run_id, retrieved_at);
CREATE INDEX IF NOT EXISTS idx_experience_reuse_finalized
    ON experience_reuse(finalized_at, retrieved_at);

CREATE TABLE IF NOT EXISTS experience_finalized_runs (
    run_id TEXT PRIMARY KEY,
    finalized_at INTEGER NOT NULL,
    successful INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_experience_finalized_runs_time
    ON experience_finalized_runs(finalized_at, run_id);

INSERT OR IGNORE INTO experience_finalized_runs (run_id, finalized_at, successful)
    SELECT run_id, MAX(finalized_at), COALESCE(MAX(successful), 0)
      FROM experience_reuse
     WHERE finalized_at IS NOT NULL
     GROUP BY run_id;

CREATE TABLE IF NOT EXISTS experience_source_provenance (
    experience_id TEXT PRIMARY KEY,
    seen_bits BLOB NOT NULL,
    observed_count INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS experience_run_sessions (
    run_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    recorded_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_experience_run_sessions_time
    ON experience_run_sessions(recorded_at, run_id);
"#;

pub struct ExperienceStore {
    connection: Connection,
}

struct SourceProvenance {
    seen_bits: Vec<u8>,
    observed_count: u64,
}

impl Default for SourceProvenance {
    fn default() -> Self {
        Self {
            seen_bits: vec![0; SOURCE_PROVENANCE_BYTES],
            observed_count: 0,
        }
    }
}

impl SourceProvenance {
    fn observe(&mut self, source_run_id: &str) -> bool {
        if source_run_id.is_empty() {
            return false;
        }

        let digest = blake3::hash(source_run_id.as_bytes());
        let mut positions = [0_usize; SOURCE_PROVENANCE_HASHES];
        for (index, chunk) in digest
            .as_bytes()
            .chunks_exact(std::mem::size_of::<u64>())
            .take(SOURCE_PROVENANCE_HASHES)
            .enumerate()
        {
            let mut bytes = [0_u8; std::mem::size_of::<u64>()];
            bytes.copy_from_slice(chunk);
            positions[index] =
                (u64::from_le_bytes(bytes) % (SOURCE_PROVENANCE_BYTES as u64 * 8)) as usize;
        }

        let previously_observed = positions
            .iter()
            .all(|position| self.seen_bits[position / 8] & (1_u8 << (position % 8)) != 0);

        for position in positions {
            self.seen_bits[position / 8] |= 1_u8 << (position % 8);
        }

        if !previously_observed {
            self.observed_count = self.observed_count.saturating_add(1);
        }

        !previously_observed
    }
}

impl ExperienceStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create experience database directory {}",
                    parent.display()
                )
            })?;
        }

        let connection = xai_sqlite_journal::JournalMode::for_db_path(path)
            .open(path)
            .with_context(|| format!("failed to open experience database {}", path.display()))?;
        connection
            .execute_batch(EXPERIENCE_SCHEMA_SQL)
            .context("failed to apply additive experience database migration")?;

        Ok(Self { connection })
    }

    pub fn upsert(&self, memory: &ExperienceMemory) -> Result<String> {
        let mut incoming = serde_json::to_value(memory)
            .context("failed to serialize experience for persistence")?;
        let incoming_object = incoming
            .as_object_mut()
            .context("experience must serialize as a JSON object")?;
        sanitize_experience_record(incoming_object);
        normalize_evidence_backing(incoming_object)?;

        if string_field(incoming_object, "id").is_empty() {
            let fingerprint = format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
                category_label(&memory.category)?,
                string_field(incoming_object, "repository_id"),
                scope_label(&memory.scope)?,
                string_field(incoming_object, "task_type"),
                normalize_text(string_field(incoming_object, "context")),
                normalize_text(string_field(incoming_object, "environment")),
                string_field(incoming_object, "repository_revision"),
                normalize_text(string_field(incoming_object, "lesson")),
                string_array(incoming_object, "source_run_ids").join("\u{1f}"),
            );
            incoming_object.insert(
                "id".to_owned(),
                Value::String(blake3::hash(fingerprint.as_bytes()).to_hex().to_string()),
            );
        }

        let transaction = self.transaction()?;
        let exact_id = string_field(incoming_object, "id").to_owned();
        let exact_match = load_record(&transaction, &exact_id)?;
        let existing = match exact_match {
            Some(record) if records_are_compatible(&record, incoming_object) => Some(record),
            Some(_) => {
                let disambiguated_id = disambiguated_experience_id(&exact_id, incoming_object);
                incoming_object.insert("id".to_owned(), Value::String(disambiguated_id.clone()));
                match load_record(&transaction, &disambiguated_id)? {
                    Some(record) if records_are_compatible(&record, incoming_object) => {
                        Some(record)
                    }
                    Some(_) => bail!("conflicting experience identity remains incompatible"),
                    None => find_consolidation_candidate(&transaction, memory, incoming_object)?,
                }
            }
            None => find_consolidation_candidate(&transaction, memory, incoming_object)?,
        };

        let (record, provenance) = match existing {
            Some(mut existing) => {
                sanitize_experience_record(&mut existing);
                let experience_id = string_field(&existing, "id");
                let mut provenance =
                    load_source_provenance(&transaction, experience_id, &existing)?;
                let record = consolidate_records(existing, incoming_object, &mut provenance)?;
                (record, provenance)
            }
            None => {
                let mut record = incoming_object.clone();
                let mut provenance = SourceProvenance::default();
                for source in validated_objective_sources(&record) {
                    provenance.observe(&source);
                }
                bound_visible_sources(&mut record);
                (record, provenance)
            }
        };
        let experience_id = string_field(&record, "id").to_owned();

        persist_record(&transaction, &record)?;
        persist_source_provenance(&transaction, &experience_id, &provenance)?;
        enforce_active_limit(&transaction, MAX_ACTIVE_EXPERIENCES)?;
        enforce_retention_limits(
            &transaction,
            MAX_TOTAL_EXPERIENCES,
            MAX_FINALIZED_REUSE_ROWS,
            MAX_PENDING_REUSE_ROWS,
        )?;
        transaction
            .commit()
            .context("failed to commit experience")?;

        Ok(experience_id)
    }

    pub fn get(&self, id: &str) -> Result<Option<ExperienceMemory>> {
        load_record(&self.connection, id)?
            .map(deserialize_record)
            .transpose()
    }

    pub fn all(&self) -> Result<Vec<ExperienceMemory>> {
        let mut statement = self
            .connection
            .prepare("SELECT record_json FROM experiences ORDER BY updated_at DESC, id ASC")?;
        let records = statement.query_map([], |row| row.get::<_, String>(0))?;
        records
            .map(|record| deserialize_record_json(&record?))
            .collect()
    }

    /// Bind an activation-scoped source run to its stable session identity.
    ///
    /// Mappings are workspace-local, immutable, and contain only validated
    /// opaque identifiers. Replaying the same mapping is harmless; a conflicting
    /// session never replaces an existing provenance reference.
    pub fn record_source_session(&self, run_id: &str, session_id: &str) -> Result<()> {
        require_run_id(run_id)?;
        if !source_reference_is_safe(run_id) || !source_reference_is_safe(session_id) {
            bail!("experience source references require safe run and session identifiers");
        }

        let transaction = self.transaction()?;
        let existing = transaction
            .query_row(
                "SELECT session_id FROM experience_run_sessions WHERE run_id = ?1",
                params![run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        match existing {
            Some(existing) if existing != session_id => {
                bail!("experience source run is already bound to another session");
            }
            Some(_) => {}
            None => {
                transaction.execute(
                    "INSERT INTO experience_run_sessions (run_id, session_id, recorded_at)
                     VALUES (?1, ?2, ?3)",
                    params![run_id, session_id, current_timestamp()],
                )?;
                enforce_source_session_limit(&transaction, MAX_SOURCE_SESSION_REFERENCES)?;
            }
        }

        transaction
            .commit()
            .context("failed to commit experience source session reference")
    }

    /// Resolve one activation run within this workspace without guessing
    /// identities for records created before session provenance was available.
    pub fn source_session_id(&self, run_id: &str) -> Result<Option<String>> {
        require_run_id(run_id)?;
        if !source_reference_is_safe(run_id) {
            bail!("experience source references require a safe run identifier");
        }

        let session_id = self
            .connection
            .query_row(
                "SELECT session_id FROM experience_run_sessions WHERE run_id = ?1",
                params![run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        if session_id
            .as_deref()
            .is_some_and(|session_id| !source_reference_is_safe(session_id))
        {
            bail!("persisted experience source session identifier is unsafe");
        }

        Ok(session_id)
    }

    pub fn retrieve(&self, query: &ExperienceQuery) -> Result<Vec<RankedExperience>> {
        self.retrieve_filtered(query, None, false)
    }

    /// Retrieve objectively classified, workspace-local experience, optionally
    /// selecting successes or failures before SQLite's bounded candidate scan.
    ///
    /// Unlike general planning retrieval, this explicit search never includes
    /// unknown outcomes or generalized records owned by another repository.
    pub fn retrieve_with_outcome(
        &self,
        query: &ExperienceQuery,
        outcome: Option<bool>,
    ) -> Result<Vec<RankedExperience>> {
        self.retrieve_filtered(query, outcome, true)
    }

    /// Resolve an exact experience, activation-run, or stable-session reference
    /// without relying on full-text indexing of opaque provenance identifiers.
    pub fn retrieve_reference(
        &self,
        query: &ExperienceQuery,
        reference: &str,
        outcome: Option<bool>,
    ) -> Result<Vec<RankedExperience>> {
        if query.limit == 0 {
            return Ok(Vec::new());
        }
        let Some(repository_id) = query
            .repository_id
            .as_deref()
            .filter(|repository_id| !repository_id.trim().is_empty())
        else {
            return Ok(Vec::new());
        };

        let (identifier, predicate) =
            if let Some(identifier) = reference.strip_prefix("experience:") {
                (identifier, "e.id = ?1")
            } else if let Some(identifier) = reference.strip_prefix("run:") {
                (
                    identifier,
                    "EXISTS (
                    SELECT 1 FROM json_each(e.record_json, '$.source_run_ids') AS source
                     WHERE source.value = ?1
                )",
                )
            } else if let Some(identifier) = reference.strip_prefix("session:") {
                (
                    identifier,
                    "EXISTS (
                    SELECT 1 FROM json_each(e.record_json, '$.source_run_ids') AS source
                    JOIN experience_run_sessions AS session_refs
                      ON session_refs.run_id = source.value
                   WHERE session_refs.session_id = ?1
                )",
                )
            } else {
                return Ok(Vec::new());
            };

        if !source_reference_is_safe(identifier) {
            return Ok(Vec::new());
        }

        let active = status_label(&ExperienceStatus::Active)?;
        let low_confidence = status_label(&ExperienceStatus::LowConfidence)?;
        let deprecated = status_label(&ExperienceStatus::Deprecated)?;
        let outcome = outcome.map(i64::from);
        let sql = format!(
            "SELECT e.record_json
               FROM experiences AS e
              WHERE {predicate}
                AND e.repository_id = ?2
                AND json_type(e.record_json, '$.success') IN ('true', 'false')
                AND (?3 IS NULL OR json_extract(e.record_json, '$.success') = ?3)
                AND (e.status = ?4 OR (?5 AND (e.status = ?6 OR e.status = ?7)))
              ORDER BY e.confidence DESC, e.updated_at DESC, e.id ASC
              LIMIT ?8"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let records = statement.query_map(
            params![
                identifier,
                repository_id,
                outcome,
                active,
                query.include_low_confidence,
                low_confidence,
                deprecated,
                MAX_CANDIDATES as i64,
            ],
            |row| row.get::<_, String>(0),
        )?;

        let mut ranked = Vec::new();
        for record in records {
            let memory = deserialize_record_json(&record?)?;
            // Identifiers are intentionally unindexed. Rank each explicitly
            // referenced record against its own redacted applicability context
            // so exact-file/module boundaries and lifecycle rules still apply.
            let mut reference_query = query.clone();
            reference_query.text = format!(
                "{} {} {} {}",
                memory.task_summary, memory.lesson, memory.strategy, memory.context
            );
            reference_query.limit = 1;
            ranked.extend(rank_experiences(vec![memory], &reference_query));
        }

        ranked.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| right.memory.updated_at.cmp(&left.memory.updated_at))
                .then_with(|| left.memory.id.cmp(&right.memory.id))
        });
        ranked.truncate(query.limit);
        Ok(ranked)
    }

    fn retrieve_filtered(
        &self,
        query: &ExperienceQuery,
        outcome: Option<bool>,
        require_known_outcome: bool,
    ) -> Result<Vec<RankedExperience>> {
        if query.limit == 0 {
            return Ok(Vec::new());
        }

        let active = status_label(&ExperienceStatus::Active)?;
        let low_confidence = status_label(&ExperienceStatus::LowConfidence)?;
        let deprecated = status_label(&ExperienceStatus::Deprecated)?;
        let candidate_limit = query.limit.saturating_mul(16).clamp(32, MAX_CANDIDATES);
        let repository_id = query.repository_id.as_deref().unwrap_or_default();
        if require_known_outcome && repository_id.trim().is_empty() {
            return Ok(Vec::new());
        }
        let strict_repository = require_known_outcome && !repository_id.is_empty();
        let outcome = outcome.map(|successful| i64::from(successful));
        let failure_context = query.failure_context.as_deref().unwrap_or_default();
        let searchable_query = format!("{} {failure_context}", query.text);
        let mut candidates = Vec::with_capacity(candidate_limit);
        let mut seen_ids = BTreeSet::new();

        if let Some(fts_query) = sanitize_fts_query(&searchable_query) {
            let fts_result = self.connection.prepare(
                "SELECT e.record_json
                  FROM experience_fts
                   JOIN experiences e ON e.id = experience_fts.experience_id
                  WHERE experience_fts MATCH ?1
                    AND (e.status = ?2 OR (?3 AND (e.status = ?4 OR e.status = ?5)))
                    AND (?7 = 0 OR json_type(e.record_json, '$.success') IN ('true', 'false'))
                    AND (?8 IS NULL OR json_extract(e.record_json, '$.success') = ?8)
                    AND (?9 = 0 OR e.repository_id = ?6)
                  ORDER BY (e.repository_id = ?6) DESC,
                           bm25(experience_fts),
                           e.confidence DESC,
                           e.updated_at DESC
                  LIMIT ?10",
            );

            match fts_result {
                Ok(mut statement) => {
                    let records = statement.query_map(
                        params![
                            fts_query,
                            active,
                            query.include_low_confidence,
                            low_confidence,
                            deprecated,
                            repository_id,
                            require_known_outcome,
                            outcome,
                            strict_repository,
                            MAX_CANDIDATES as i64,
                        ],
                        |row| row.get::<_, String>(0),
                    );

                    match records {
                        Ok(records) => {
                            for record in records {
                                let memory = deserialize_record_json(&record?)?;
                                if !rank_experiences(vec![memory.clone()], query).is_empty()
                                    && seen_ids.insert(memory.id.clone())
                                {
                                    candidates.push(memory);
                                }
                                if candidates.len() >= candidate_limit {
                                    break;
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "experience FTS failed; using lexical fallback");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "experience FTS unavailable; using lexical fallback");
                }
            }
        }

        if candidates.len() < candidate_limit {
            let mut statement = self.connection.prepare(
                "SELECT record_json
                   FROM experiences
                  WHERE (status = ?1 OR (?2 AND (status = ?3 OR status = ?4)))
                    AND (?6 = 0 OR json_type(record_json, '$.success') IN ('true', 'false'))
                    AND (?7 IS NULL OR json_extract(record_json, '$.success') = ?7)
                    AND (?8 = 0 OR repository_id = ?5)
                  ORDER BY (repository_id = ?5) DESC,
                           confidence DESC,
                           evidence_count DESC,
                           updated_at DESC
                  LIMIT ?9",
            )?;
            let records = statement.query_map(
                params![
                    active,
                    query.include_low_confidence,
                    low_confidence,
                    deprecated,
                    repository_id,
                    require_known_outcome,
                    outcome,
                    strict_repository,
                    MAX_CANDIDATES as i64,
                ],
                |row| row.get::<_, String>(0),
            )?;

            for record in records {
                let memory = deserialize_record_json(&record?)?;
                if !rank_experiences(vec![memory.clone()], query).is_empty()
                    && seen_ids.insert(memory.id.clone())
                {
                    candidates.push(memory);
                }
                if candidates.len() >= candidate_limit {
                    break;
                }
            }
        }

        let mut ranked = rank_experiences(candidates, query);
        ranked.truncate(query.limit);
        Ok(ranked)
    }

    pub fn record_retrieval(&self, run_id: &str, ids: &[String]) -> Result<()> {
        require_run_id(run_id)?;
        if ids.is_empty() {
            return Ok(());
        }

        let transaction = self.transaction()?;
        let now = current_timestamp();
        let mut unique_ids = BTreeSet::new();

        for experience_id in ids {
            if !unique_ids.insert(experience_id.as_str()) {
                continue;
            }

            let Some(mut record) = load_record(&transaction, experience_id)? else {
                continue;
            };

            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO experience_reuse
                    (run_id, experience_id, retrieved_at)
                 SELECT ?1, ?2, ?3
                  WHERE NOT EXISTS (
                    SELECT 1 FROM experience_reuse
                     WHERE run_id = ?1 AND finalized_at IS NOT NULL
                  ) AND NOT EXISTS (
                    SELECT 1 FROM experience_finalized_runs WHERE run_id = ?1
                  )",
                params![run_id, experience_id, now],
            )?;

            if inserted != 0 {
                increment_counter(&mut record, "retrieved_count");
                record.insert("last_used_at".to_owned(), Value::from(now));
                persist_record(&transaction, &record)?;
            }
        }

        enforce_reuse_limit(&transaction, false, MAX_PENDING_REUSE_ROWS)?;
        transaction
            .commit()
            .context("failed to commit experience retrieval attribution")
    }

    pub fn retrieved_for_run(&self, run_id: &str) -> Result<Vec<ExperienceMemory>> {
        require_run_id(run_id)?;
        let mut statement = self.connection.prepare(
            "SELECT e.record_json
               FROM experience_reuse r
               JOIN experiences e ON e.id = r.experience_id
              WHERE r.run_id = ?1
              ORDER BY r.retrieved_at ASC, r.experience_id ASC",
        )?;
        let records = statement.query_map(params![run_id], |row| row.get::<_, String>(0))?;
        records
            .map(|record| deserialize_record_json(&record?))
            .collect()
    }

    pub fn record_followed(&self, run_id: &str, experience_id: &str) -> Result<()> {
        require_run_id(run_id)?;
        let transaction = self.transaction()?;
        let now = current_timestamp();
        let followed = transaction.execute(
            "UPDATE experience_reuse
                SET followed_at = ?3
              WHERE run_id = ?1
                AND experience_id = ?2
                AND followed_at IS NULL
                AND finalized_at IS NULL
                AND NOT EXISTS (
                    SELECT 1 FROM experience_finalized_runs WHERE run_id = ?1
                )",
            params![run_id, experience_id, now],
        )?;

        if followed != 0
            && let Some(mut record) = load_record(&transaction, experience_id)?
        {
            increment_counter(&mut record, "followed_count");
            record.insert("last_used_at".to_owned(), Value::from(now));
            persist_record(&transaction, &record)?;
        }

        transaction
            .commit()
            .context("failed to commit experience follow attribution")
    }

    pub fn finalize_run(&self, run_id: &str, success: bool) -> Result<()> {
        require_run_id(run_id)?;
        let transaction = self.transaction()?;
        let now = current_timestamp();
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO experience_finalized_runs
                 (run_id, finalized_at, successful)
             VALUES (?1, ?2, ?3)",
            params![run_id, now, success],
        )?;
        if inserted == 0 {
            transaction.commit()?;
            return Ok(());
        }
        let attributed_ids = {
            let mut statement = transaction.prepare(
                "SELECT experience_id
                   FROM experience_reuse
                  WHERE run_id = ?1
                    AND followed_at IS NOT NULL
                    AND finalized_at IS NULL",
            )?;
            let ids = statement.query_map(params![run_id], |row| row.get::<_, String>(0))?;
            ids.collect::<rusqlite::Result<Vec<_>>>()?
        };

        transaction.execute(
            "UPDATE experience_reuse
                SET finalized_at = ?2,
                    successful = CASE WHEN followed_at IS NOT NULL THEN ?3 ELSE NULL END
              WHERE run_id = ?1
                AND finalized_at IS NULL",
            params![run_id, now, success],
        )?;

        for experience_id in attributed_ids {
            let Some(mut record) = load_record(&transaction, &experience_id)? else {
                continue;
            };

            let confidence = numeric_field(&record, "confidence").clamp(0.0, 1.0);
            if success {
                increment_counter(&mut record, "successful_reuse_count");
                let updated_confidence = (confidence + (1.0 - confidence) * 0.08).min(0.97);
                insert_finite_number(&mut record, "confidence", updated_confidence);

                if updated_confidence >= 0.45
                    && string_field(&record, "status")
                        == status_label(&ExperienceStatus::LowConfidence)?
                {
                    record.insert(
                        "status".to_owned(),
                        serde_json::to_value(ExperienceStatus::Active)?,
                    );
                }
            } else {
                increment_counter(&mut record, "failed_reuse_count");
                insert_finite_number(&mut record, "confidence", (confidence * 0.72).max(0.05));
                let generalizability = numeric_field(&record, "generalizability");
                insert_finite_number(
                    &mut record,
                    "generalizability",
                    (generalizability * 0.75).clamp(0.0, 1.0),
                );

                let failed = counter_field(&record, "failed_reuse_count");
                let successful = counter_field(&record, "successful_reuse_count");
                if failed >= 2 && failed > successful {
                    narrow_scope_when_supported(&mut record)?;
                    let status = if failed >= 4 && failed >= successful.saturating_mul(2) {
                        ExperienceStatus::Deprecated
                    } else {
                        ExperienceStatus::LowConfidence
                    };
                    record.insert("status".to_owned(), serde_json::to_value(status)?);
                }
            }

            record.insert("updated_at".to_owned(), Value::from(now));
            record.insert("last_used_at".to_owned(), Value::from(now));
            persist_record(&transaction, &record)?;
        }

        enforce_reuse_limit(&transaction, true, MAX_FINALIZED_REUSE_ROWS)?;
        enforce_finalized_run_limit(&transaction, MAX_FINALIZED_RUN_TOMBSTONES)?;
        transaction
            .commit()
            .context("failed to finalize experience reuse attribution")
    }

    pub fn invalidate(
        &self,
        id: &str,
        status: ExperienceStatus,
        superseded_by: Option<&str>,
    ) -> Result<()> {
        let transaction = self.transaction()?;
        let Some(mut record) = load_record(&transaction, id)? else {
            transaction.commit()?;
            return Ok(());
        };

        record.insert("status".to_owned(), serde_json::to_value(status)?);
        record.insert(
            "superseded_by".to_owned(),
            superseded_by.map_or(Value::Null, |replacement| {
                Value::String(replacement.to_owned())
            }),
        );
        record.insert("updated_at".to_owned(), Value::from(current_timestamp()));
        persist_record(&transaction, &record)?;
        transaction
            .commit()
            .context("failed to update experience lifecycle state")
    }

    fn transaction(&self) -> Result<Transaction<'_>> {
        Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)
            .context("failed to begin immediate experience database transaction")
    }
}

fn normalize_evidence_backing(record: &mut Map<String, Value>) -> Result<()> {
    let objective_sources = validated_objective_sources(record);
    let objective_count = objective_sources.len();
    record.insert(
        "evidence_count".to_owned(),
        Value::from(objective_count.min(u32::MAX as usize) as u32),
    );

    let confidence_limit = match objective_count {
        0 => 0.35,
        1 => 0.65,
        2 => 0.82,
        _ => 0.97,
    };
    let generalizability_limit = match objective_count {
        0 => 0.35,
        1 => 0.65,
        2 => 0.85,
        _ => 0.97,
    };
    insert_finite_number(
        record,
        "confidence",
        numeric_field(record, "confidence").clamp(0.0, confidence_limit),
    );
    insert_finite_number(
        record,
        "generalizability",
        numeric_field(record, "generalizability").clamp(0.0, generalizability_limit),
    );

    if objective_count == 0
        && !matches!(
            string_field(record, "status"),
            "deprecated" | "superseded" | "invalidated"
        )
    {
        record.insert(
            "status".to_owned(),
            serde_json::to_value(ExperienceStatus::LowConfidence)?,
        );
    }

    Ok(())
}

fn validated_objective_signals(record: &Map<String, Value>) -> Vec<EvidenceSignal> {
    let declared_sources = string_array(record, "source_run_ids")
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut fingerprints = BTreeSet::new();
    ["evidence", "test_results"]
        .into_iter()
        .flat_map(|field| {
            record
                .get(field)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter_map(|value| serde_json::from_value::<EvidenceSignal>(value.clone()).ok())
        .filter(|signal| {
            signal.is_objective()
                && matches!(
                    signal.verdict,
                    EvidenceVerdict::Passed | EvidenceVerdict::Failed
                )
                && signal.source_run_id.as_deref().is_some_and(|source| {
                    identity_is_safe(source, false) && declared_sources.contains(source)
                })
        })
        .filter(|signal| {
            let fingerprint = format!(
                "{:?}\u{1f}{:?}\u{1f}{}\u{1f}{}",
                signal.kind,
                signal.verdict,
                signal.source_run_id.as_deref().unwrap_or_default(),
                signal.command.as_deref().unwrap_or_default(),
            );
            fingerprints.insert(fingerprint)
        })
        .collect()
}

fn validated_objective_sources(record: &Map<String, Value>) -> BTreeSet<String> {
    validated_objective_signals(record)
        .into_iter()
        .filter_map(|signal| signal.source_run_id)
        .collect()
}

fn records_are_compatible(existing: &Map<String, Value>, incoming: &Map<String, Value>) -> bool {
    for field in ["category", "repository_id", "scope", "task_type"] {
        if string_field(existing, field) != string_field(incoming, field) {
            return false;
        }
    }

    let existing_success = existing.get("success").and_then(Value::as_bool);
    let incoming_success = incoming.get("success").and_then(Value::as_bool);
    if existing_success.is_some()
        && incoming_success.is_some()
        && existing_success != incoming_success
    {
        return false;
    }

    let existing_context = string_field(existing, "context");
    let incoming_context = string_field(incoming, "context");
    if !existing_context.is_empty() && !incoming_context.is_empty() {
        let similarity = token_similarity(existing_context, incoming_context);
        let specific_scope = matches!(string_field(existing, "scope"), "exact_file" | "module");
        if (specific_scope && normalize_text(existing_context) != normalize_text(incoming_context))
            || (!specific_scope && similarity < 0.35)
        {
            return false;
        }
    }

    guidance_polarity(existing) == guidance_polarity(incoming)
        && !contradictory_negation(existing, incoming)
        && !incompatible_exception(existing, incoming)
        && !incompatible_revision(existing, incoming)
        && !opposite_objective_verdicts(existing, incoming)
}

fn opposite_objective_verdicts(
    existing: &Map<String, Value>,
    incoming: &Map<String, Value>,
) -> bool {
    let profile = |record: &Map<String, Value>| {
        validated_objective_signals(record).into_iter().fold(
            (false, false),
            |(passed, failed), signal| {
                (
                    passed || signal.verdict == EvidenceVerdict::Passed,
                    failed || signal.verdict == EvidenceVerdict::Failed,
                )
            },
        )
    };

    matches!(
        (profile(existing), profile(incoming)),
        ((true, false), (false, true)) | ((false, true), (true, false))
    )
}

fn disambiguated_experience_id(original_id: &str, record: &Map<String, Value>) -> String {
    let fingerprint = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{:?}\u{1f}{:?}",
        original_id,
        string_field(record, "category"),
        string_field(record, "repository_id"),
        string_field(record, "scope"),
        string_field(record, "task_type"),
        normalize_text(string_field(record, "context")),
        normalize_text(string_field(record, "environment")),
        string_field(record, "repository_revision"),
        normalize_text(string_field(record, "lesson")),
        record.get("success"),
        validated_objective_signals(record)
            .into_iter()
            .map(|signal| signal.verdict)
            .collect::<Vec<_>>(),
    );
    let digest = blake3::hash(fingerprint.as_bytes()).to_hex();
    format!("{}-{}", truncate_chars(original_id, 96), &digest[..24])
}

fn find_consolidation_candidate(
    connection: &Connection,
    incoming_memory: &ExperienceMemory,
    incoming: &Map<String, Value>,
) -> Result<Option<Map<String, Value>>> {
    let mut statement = connection.prepare(
        "SELECT record_json
           FROM experiences
          WHERE category = ?1
            AND repository_id = ?2
            AND scope = ?3
            AND task_type = ?4
            AND (status = ?5 OR status = ?6)
          ORDER BY updated_at DESC
          LIMIT ?7",
    )?;
    let records = statement.query_map(
        params![
            category_label(&incoming_memory.category)?,
            string_field(incoming, "repository_id"),
            scope_label(&incoming_memory.scope)?,
            string_field(incoming, "task_type"),
            status_label(&ExperienceStatus::Active)?,
            status_label(&ExperienceStatus::LowConfidence)?,
            MAX_CONSOLIDATION_CANDIDATES as i64,
        ],
        |row| row.get::<_, String>(0),
    )?;

    let mut strongest: Option<(f64, Map<String, Value>)> = None;
    for record in records {
        let candidate = parse_record(&record?)?;
        if !records_are_compatible(&candidate, incoming) {
            continue;
        }

        let similarity = token_similarity(
            string_field(&candidate, "lesson"),
            string_field(incoming, "lesson"),
        );
        if similarity >= CONSOLIDATION_SIMILARITY
            && strongest
                .as_ref()
                .is_none_or(|(previous, _)| similarity > *previous)
        {
            strongest = Some((similarity, candidate));
        }
    }

    Ok(strongest.map(|(_, record)| record))
}

fn consolidate_records(
    mut existing: Map<String, Value>,
    incoming: &Map<String, Value>,
    provenance: &mut SourceProvenance,
) -> Result<Map<String, Value>> {
    let mut sources = string_array(&existing, "source_run_ids");
    let incoming_sources = validated_objective_sources(incoming);
    let mut independent_sources = 0_usize;
    for source in incoming_sources {
        if provenance.observe(&source) {
            independent_sources = independent_sources.saturating_add(1);
            if !sources.contains(&source) {
                sources.push(source);
            }
        }
    }

    if independent_sources == 0 {
        return Ok(existing);
    }

    existing.insert(
        "source_run_ids".to_owned(),
        Value::Array(sources.into_iter().map(Value::String).collect()),
    );
    bound_visible_sources(&mut existing);

    let existing_evidence_count = counter_field(&existing, "evidence_count");
    let incoming_evidence_count = validated_objective_sources(incoming).len() as u64;
    let previous_evidence_weight = existing_evidence_count.max(1) as f64;
    let incoming_evidence_weight = incoming_evidence_count.max(1) as f64;

    merge_unique_array(&mut existing, incoming, "evidence", MAX_EVIDENCE_ITEMS)?;
    merge_unique_array(&mut existing, incoming, "tests_run", MAX_EVIDENCE_ITEMS)?;
    merge_unique_array(&mut existing, incoming, "test_results", MAX_EVIDENCE_ITEMS)?;
    merge_unique_array(&mut existing, incoming, "what_worked", MAX_EVIDENCE_ITEMS)?;
    merge_unique_array(&mut existing, incoming, "what_failed", MAX_EVIDENCE_ITEMS)?;
    merge_unique_array(&mut existing, incoming, "key_decisions", MAX_EVIDENCE_ITEMS)?;

    merge_score_dimensions(
        &mut existing,
        incoming,
        "outcome",
        previous_evidence_weight,
        incoming_evidence_weight,
    );
    merge_score_dimensions(
        &mut existing,
        incoming,
        "evaluator_scores",
        previous_evidence_weight,
        incoming_evidence_weight,
    );

    let evidence_count = existing_evidence_count.saturating_add(independent_sources as u64);
    existing.insert(
        "evidence_count".to_owned(),
        Value::from(evidence_count.min(u64::from(u32::MAX))),
    );

    let existing_confidence = numeric_field(&existing, "confidence").clamp(0.0, 1.0);
    let incoming_confidence = numeric_field(incoming, "confidence").clamp(0.0, 1.0);
    let expects_failure = string_field(&existing, "category") == "failure_anti_pattern"
        || existing.get("success").and_then(Value::as_bool) == Some(false);
    let contradiction_count = validated_objective_signals(incoming)
        .into_iter()
        .filter(|signal| matches!(signal.verdict, EvidenceVerdict::Failed) != expects_failure)
        .count();
    let updated_confidence = if contradiction_count == 0 {
        let support_gain = (1.0 - existing_confidence)
            * (0.10 * independent_sources.min(3) as f64)
            * incoming_confidence.max(0.25);
        (existing_confidence + support_gain).min(0.97)
    } else {
        (existing_confidence * 0.75_f64.powi(contradiction_count.min(3) as i32)).max(0.05)
    };
    insert_finite_number(&mut existing, "confidence", updated_confidence);
    if contradiction_count != 0
        && updated_confidence < 0.45
        && string_field(&existing, "status") == status_label(&ExperienceStatus::Active)?
    {
        existing.insert(
            "status".to_owned(),
            serde_json::to_value(ExperienceStatus::LowConfidence)?,
        );
    }

    for field in [
        "retrieved_count",
        "followed_count",
        "successful_reuse_count",
        "failed_reuse_count",
    ] {
        existing.insert(
            field.to_owned(),
            Value::from(
                counter_field(&existing, field)
                    .saturating_add(counter_field(incoming, field))
                    .min(u64::from(u32::MAX)),
            ),
        );
    }

    if let Some(incoming_revision) = incoming
        .get("repository_revision")
        .and_then(Value::as_str)
        .filter(|revision| !revision.is_empty())
        && signed_field(incoming, "updated_at") >= signed_field(&existing, "updated_at")
    {
        existing.insert(
            "repository_revision".to_owned(),
            Value::String(incoming_revision.to_owned()),
        );
    }

    for field in [
        "context",
        "environment",
        "strategy",
        "strategy_rationale",
        "implementation_pattern",
        "recommendation",
        "anti_pattern",
        "judge_feedback",
        "failure_reason",
    ] {
        if is_empty_value(existing.get(field))
            && let Some(value) = incoming.get(field)
            && !is_empty_value(Some(value))
        {
            existing.insert(field.to_owned(), value.clone());
        }
    }

    let updated_at =
        signed_field(&existing, "updated_at").max(signed_field(incoming, "updated_at"));
    existing.insert("updated_at".to_owned(), Value::from(updated_at));

    Ok(existing)
}

fn sanitize_experience_record(record: &mut Map<String, Value>) {
    for field in [
        "id",
        "repository_id",
        "repository_revision",
        "superseded_by",
    ] {
        if let Some(Value::String(identity)) = record.get_mut(field) {
            if !identity.is_empty() && !identity_is_safe(identity, field == "repository_id") {
                let digest = blake3::hash(identity.as_bytes()).to_hex();
                *identity = format!("redacted-{}", &digest[..24]);
            }
        }
    }
    if let Some(sources) = record
        .get_mut("source_run_ids")
        .and_then(Value::as_array_mut)
    {
        sources.retain(|source| {
            source
                .as_str()
                .is_some_and(|source| identity_is_safe(source, false))
        });
        sources.truncate(MAX_SOURCE_RUNS);
    }

    for (field, value) in record.iter_mut() {
        if matches!(
            field.as_str(),
            "id" | "repository_id"
                | "repository_revision"
                | "source_run_ids"
                | "category"
                | "scope"
                | "status"
                | "superseded_by"
                | "evaluator_scores"
        ) {
            continue;
        }

        sanitize_semantic_value(value);
    }

    for field in [
        "evidence",
        "test_results",
        "tests_run",
        "key_decisions",
        "what_worked",
        "what_failed",
    ] {
        if let Some(values) = record.get_mut(field).and_then(Value::as_array_mut) {
            values.truncate(MAX_EVIDENCE_ITEMS);
        }
    }
}

fn sanitize_semantic_value(value: &mut Value) {
    match value {
        Value::String(text) => {
            *text = truncate_chars(&redact_sensitive_text(text), MAX_PERSISTED_FIELD_CHARS);
        }
        Value::Array(values) => {
            for value in values {
                sanitize_semantic_value(value);
            }
        }
        Value::Object(values) => {
            for (field, value) in values {
                if field == "source_run_id" {
                    if value
                        .as_str()
                        .is_some_and(|source| !identity_is_safe(source, false))
                    {
                        *value = Value::Null;
                    }
                } else if matches!(field.as_str(), "id" | "repository_id") {
                    if let Value::String(identity) = value
                        && !identity_is_safe(identity, field == "repository_id")
                    {
                        let digest = blake3::hash(identity.as_bytes()).to_hex();
                        *identity = format!("redacted-{}", &digest[..24]);
                    }
                } else if is_sensitive_field_name(field) {
                    *value = Value::String("[REDACTED]".to_owned());
                } else {
                    sanitize_semantic_value(value);
                }
            }
        }
        _ => {}
    }
}

fn identity_is_safe(identity: &str, repository_path: bool) -> bool {
    if identity.trim().is_empty()
        || identity.chars().count() > MAX_IDENTITY_FIELD_CHARS
        || identity.chars().any(char::is_control)
        || (!repository_path && identity.chars().any(char::is_whitespace))
        || identity.contains('=')
        || identity.contains('@')
        || identity.contains('"')
        || identity.contains('\'')
    {
        return false;
    }

    redact_sensitive_text(identity) == identity
}

pub(crate) fn source_reference_is_safe(identity: &str) -> bool {
    identity_is_safe(identity, false)
        && identity
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && identity.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && !identity.contains("..")
}

fn load_source_provenance(
    connection: &Connection,
    experience_id: &str,
    existing: &Map<String, Value>,
) -> Result<SourceProvenance> {
    let persisted = connection
        .query_row(
            "SELECT seen_bits, observed_count
               FROM experience_source_provenance
              WHERE experience_id = ?1",
            params![experience_id],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;

    let mut provenance = match persisted {
        Some((seen_bits, observed_count)) if seen_bits.len() == SOURCE_PROVENANCE_BYTES => {
            SourceProvenance {
                seen_bits,
                observed_count: observed_count.max(0) as u64,
            }
        }
        _ => SourceProvenance::default(),
    };

    for source in validated_objective_sources(existing) {
        provenance.observe(&source);
    }

    Ok(provenance)
}

fn persist_source_provenance(
    connection: &Connection,
    experience_id: &str,
    provenance: &SourceProvenance,
) -> Result<()> {
    connection.execute(
        "INSERT INTO experience_source_provenance
             (experience_id, seen_bits, observed_count, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(experience_id) DO UPDATE SET
             seen_bits = excluded.seen_bits,
             observed_count = excluded.observed_count,
             updated_at = excluded.updated_at",
        params![
            experience_id,
            provenance.seen_bits,
            sqlite_counter(provenance.observed_count),
            current_timestamp(),
        ],
    )?;

    Ok(())
}

fn bound_visible_sources(record: &mut Map<String, Value>) {
    let Some(sources) = record
        .get_mut("source_run_ids")
        .and_then(Value::as_array_mut)
    else {
        return;
    };

    let mut unique = BTreeSet::new();
    sources.retain(|source| {
        source
            .as_str()
            .is_some_and(|source| unique.insert(source.to_owned()))
    });

    if sources.len() > MAX_SOURCE_RUNS {
        let removed = sources.len() - MAX_SOURCE_RUNS;
        sources.drain(1..=removed);
    }
}

fn persist_record(connection: &Connection, record: &Map<String, Value>) -> Result<()> {
    let id = string_field(record, "id");
    if id.is_empty() {
        bail!("cannot persist an experience without an identifier");
    }

    let record_json =
        serde_json::to_string(record).context("failed to encode experience record")?;
    let category = string_field(record, "category");
    let scope = string_field(record, "scope");
    let status = string_field(record, "status");
    let last_used_at = record.get("last_used_at").and_then(Value::as_i64);
    let superseded_by = record.get("superseded_by").and_then(Value::as_str);

    connection.execute(
        "INSERT INTO experiences (
             id, category, repository_id, scope, task_type, lesson, status,
             confidence, generalizability, evidence_count, created_at, updated_at,
             last_used_at, retrieved_count, followed_count,
             successful_reuse_count, failed_reuse_count, superseded_by, record_json
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
             ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19
         )
         ON CONFLICT(id) DO UPDATE SET
             category = excluded.category,
             repository_id = excluded.repository_id,
             scope = excluded.scope,
             task_type = excluded.task_type,
             lesson = excluded.lesson,
             status = excluded.status,
             confidence = excluded.confidence,
             generalizability = excluded.generalizability,
             evidence_count = excluded.evidence_count,
             created_at = excluded.created_at,
             updated_at = excluded.updated_at,
             last_used_at = excluded.last_used_at,
             retrieved_count = excluded.retrieved_count,
             followed_count = excluded.followed_count,
             successful_reuse_count = excluded.successful_reuse_count,
             failed_reuse_count = excluded.failed_reuse_count,
             superseded_by = excluded.superseded_by,
             record_json = excluded.record_json",
        params![
            id,
            category,
            string_field(record, "repository_id"),
            scope,
            string_field(record, "task_type"),
            string_field(record, "lesson"),
            status,
            numeric_field(record, "confidence"),
            numeric_field(record, "generalizability"),
            sqlite_counter(counter_field(record, "evidence_count")),
            signed_field(record, "created_at"),
            signed_field(record, "updated_at"),
            last_used_at,
            sqlite_counter(counter_field(record, "retrieved_count")),
            sqlite_counter(counter_field(record, "followed_count")),
            sqlite_counter(counter_field(record, "successful_reuse_count")),
            sqlite_counter(counter_field(record, "failed_reuse_count")),
            superseded_by,
            record_json,
        ],
    )?;

    connection.execute(
        "DELETE FROM experience_fts WHERE experience_id = ?1",
        params![id],
    )?;
    connection.execute(
        "INSERT INTO experience_fts (
             experience_id, lesson, task_summary, strategy, recommendation, anti_pattern
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            truncate_chars(string_field(record, "lesson"), MAX_INDEXED_FIELD_CHARS),
            truncate_chars(
                string_field(record, "task_summary"),
                MAX_INDEXED_FIELD_CHARS
            ),
            truncate_chars(string_field(record, "strategy"), MAX_INDEXED_FIELD_CHARS),
            truncate_chars(
                string_field(record, "recommendation"),
                MAX_INDEXED_FIELD_CHARS
            ),
            truncate_chars(
                string_field(record, "anti_pattern"),
                MAX_INDEXED_FIELD_CHARS
            ),
        ],
    )?;

    Ok(())
}

fn load_record(connection: &Connection, id: &str) -> Result<Option<Map<String, Value>>> {
    let record = connection
        .query_row(
            "SELECT record_json FROM experiences WHERE id = ?1",
            params![id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;

    record.as_deref().map(parse_record).transpose()
}

fn parse_record(record: &str) -> Result<Map<String, Value>> {
    let value: Value = serde_json::from_str(record).context("invalid stored experience JSON")?;
    match value {
        Value::Object(object) => Ok(object),
        _ => bail!("stored experience record is not a JSON object"),
    }
}

fn deserialize_record(record: Map<String, Value>) -> Result<ExperienceMemory> {
    serde_json::from_value(Value::Object(record))
        .context("failed to deserialize structured experience")
}

fn deserialize_record_json(record: &str) -> Result<ExperienceMemory> {
    serde_json::from_str(record).context("failed to deserialize structured experience")
}

fn category_label(category: &ExperienceCategory) -> Result<String> {
    enum_label(serde_json::to_value(category)?, "experience category")
}

fn scope_label(scope: &ExperienceScope) -> Result<String> {
    enum_label(serde_json::to_value(scope)?, "experience scope")
}

fn status_label(status: &ExperienceStatus) -> Result<String> {
    enum_label(serde_json::to_value(status)?, "experience status")
}

fn enum_label(value: Value, description: &str) -> Result<String> {
    match value {
        Value::String(label) => Ok(label),
        _ => bail!("{description} must serialize as a string"),
    }
}

fn enforce_active_limit(connection: &Connection, maximum: usize) -> Result<()> {
    let active = status_label(&ExperienceStatus::Active)?;
    let low_confidence = status_label(&ExperienceStatus::LowConfidence)?;
    let active_count = connection.query_row(
        "SELECT COUNT(*) FROM experiences WHERE status = ?1 OR status = ?2",
        params![active, low_confidence],
        |row| row.get::<_, i64>(0),
    )?;
    let overflow = (active_count.max(0) as usize).saturating_sub(maximum);
    if overflow == 0 {
        return Ok(());
    }

    let deprecated_ids = {
        let mut statement = connection.prepare(
            "SELECT id
               FROM experiences
              WHERE status = ?1 OR status = ?2
              ORDER BY (
                           confidence
                         + MIN(evidence_count, 12) * 0.025
                         + successful_reuse_count * 0.04
                         - failed_reuse_count * 0.05
                       ) ASC,
                       updated_at ASC,
                       id ASC
              LIMIT ?3",
        )?;
        let ids = statement.query_map(params![active, low_confidence, overflow as i64], |row| {
            row.get::<_, String>(0)
        })?;
        ids.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for id in deprecated_ids {
        if let Some(mut record) = load_record(connection, &id)? {
            record.insert(
                "status".to_owned(),
                serde_json::to_value(ExperienceStatus::Deprecated)?,
            );
            record.insert("updated_at".to_owned(), Value::from(current_timestamp()));
            persist_record(connection, &record)?;
        }
    }

    Ok(())
}

fn enforce_retention_limits(
    connection: &Connection,
    maximum_experiences: usize,
    maximum_finalized_reuse: usize,
    maximum_pending_reuse: usize,
) -> Result<()> {
    let experience_count = connection.query_row("SELECT COUNT(*) FROM experiences", [], |row| {
        row.get::<_, i64>(0)
    })?;
    let overflow = (experience_count.max(0) as usize).saturating_sub(maximum_experiences);

    if overflow > 0 {
        let deprecated = status_label(&ExperienceStatus::Deprecated)?;
        let superseded = status_label(&ExperienceStatus::Superseded)?;
        let invalidated = status_label(&ExperienceStatus::Invalidated)?;
        let removable_ids = {
            let mut statement = connection.prepare(
                "SELECT id
                   FROM experiences
                  WHERE status = ?1 OR status = ?2 OR status = ?3
                  ORDER BY (
                               confidence
                             + MIN(evidence_count, 24) * 0.025
                             + successful_reuse_count * 0.05
                             - failed_reuse_count * 0.05
                           ) ASC,
                           updated_at ASC,
                           id ASC
                  LIMIT ?4",
            )?;
            let ids = statement.query_map(
                params![deprecated, superseded, invalidated, overflow as i64],
                |row| row.get::<_, String>(0),
            )?;
            ids.collect::<rusqlite::Result<Vec<_>>>()?
        };

        for experience_id in removable_ids {
            connection.execute(
                "DELETE FROM experience_fts WHERE experience_id = ?1",
                params![experience_id],
            )?;
            connection.execute(
                "DELETE FROM experience_reuse WHERE experience_id = ?1",
                params![experience_id],
            )?;
            connection.execute(
                "DELETE FROM experience_source_provenance WHERE experience_id = ?1",
                params![experience_id],
            )?;
            connection.execute(
                "DELETE FROM experiences WHERE id = ?1",
                params![experience_id],
            )?;
        }
    }

    enforce_reuse_limit(connection, true, maximum_finalized_reuse)?;
    enforce_reuse_limit(connection, false, maximum_pending_reuse)?;
    enforce_finalized_run_limit(connection, MAX_FINALIZED_RUN_TOMBSTONES)?;
    Ok(())
}

fn enforce_finalized_run_limit(connection: &Connection, maximum: usize) -> Result<()> {
    let count = connection.query_row(
        "SELECT COUNT(*) FROM experience_finalized_runs",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let overflow = (count.max(0) as usize).saturating_sub(maximum);
    if overflow != 0 {
        connection.execute(
            "DELETE FROM experience_finalized_runs
              WHERE run_id IN (
                  SELECT run_id
                    FROM experience_finalized_runs
                   ORDER BY finalized_at ASC, run_id ASC
                   LIMIT ?1
              )",
            params![overflow as i64],
        )?;
    }

    Ok(())
}

fn enforce_source_session_limit(connection: &Connection, maximum: usize) -> Result<()> {
    let count =
        connection.query_row("SELECT COUNT(*) FROM experience_run_sessions", [], |row| {
            row.get::<_, i64>(0)
        })?;
    let overflow = (count.max(0) as usize).saturating_sub(maximum);
    if overflow != 0 {
        connection.execute(
            "DELETE FROM experience_run_sessions
              WHERE run_id IN (
                  SELECT run_id
                    FROM experience_run_sessions
                   ORDER BY recorded_at ASC, run_id ASC
                   LIMIT ?1
              )",
            params![overflow as i64],
        )?;
    }

    Ok(())
}

fn enforce_reuse_limit(connection: &Connection, finalized: bool, maximum: usize) -> Result<()> {
    let count_sql = if finalized {
        "SELECT COUNT(*) FROM experience_reuse WHERE finalized_at IS NOT NULL"
    } else {
        "SELECT COUNT(*) FROM experience_reuse WHERE finalized_at IS NULL"
    };
    let count = connection.query_row(count_sql, [], |row| row.get::<_, i64>(0))?;
    let overflow = (count.max(0) as usize).saturating_sub(maximum);
    if overflow == 0 {
        return Ok(());
    }

    let delete_sql = if finalized {
        "DELETE FROM experience_reuse
          WHERE rowid IN (
              SELECT rowid
                FROM experience_reuse
               WHERE finalized_at IS NOT NULL
               ORDER BY finalized_at ASC, retrieved_at ASC, rowid ASC
               LIMIT ?1
          )"
    } else {
        "DELETE FROM experience_reuse
          WHERE rowid IN (
              SELECT rowid
                FROM experience_reuse
               WHERE finalized_at IS NULL
               ORDER BY retrieved_at ASC, rowid ASC
               LIMIT ?1
          )"
    };
    connection.execute(delete_sql, params![overflow as i64])?;

    Ok(())
}

fn sanitize_fts_query(query: &str) -> Option<String> {
    let mut unique = BTreeSet::new();
    let terms = tokenize(query)
        .into_iter()
        .filter(|token| token.chars().count() >= 2)
        .filter(|token| unique.insert(token.clone()))
        .take(12)
        .map(|token| format!("\"{}\"*", truncate_chars(&token, 64)))
        .collect::<Vec<_>>();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

fn guidance_polarity(record: &Map<String, Value>) -> i8 {
    let recommendation = !string_field(record, "recommendation").is_empty();
    let anti_pattern = !string_field(record, "anti_pattern").is_empty();
    match (recommendation, anti_pattern) {
        (true, false) => 1,
        (false, true) => -1,
        _ => 0,
    }
}

fn contradictory_negation(existing: &Map<String, Value>, incoming: &Map<String, Value>) -> bool {
    has_negation(string_field(existing, "lesson")) != has_negation(string_field(incoming, "lesson"))
}

fn incompatible_exception(existing: &Map<String, Value>, incoming: &Map<String, Value>) -> bool {
    let existing_exception = has_exception(string_field(existing, "lesson"));
    let incoming_exception = has_exception(string_field(incoming, "lesson"));
    if existing_exception != incoming_exception {
        return true;
    }

    let existing_environment = string_field(existing, "environment");
    let incoming_environment = string_field(incoming, "environment");
    if !existing_environment.is_empty()
        && !incoming_environment.is_empty()
        && normalize_text(existing_environment) != normalize_text(incoming_environment)
    {
        return true;
    }

    if existing_exception {
        let existing_context = string_field(existing, "context");
        let incoming_context = string_field(incoming, "context");
        if !existing_context.is_empty()
            && !incoming_context.is_empty()
            && token_similarity(existing_context, incoming_context) < 0.35
        {
            return true;
        }
    }

    false
}

fn incompatible_revision(existing: &Map<String, Value>, incoming: &Map<String, Value>) -> bool {
    let existing_revision = string_field(existing, "repository_revision");
    let incoming_revision = string_field(incoming, "repository_revision");
    if existing_revision.is_empty()
        || incoming_revision.is_empty()
        || existing_revision == incoming_revision
    {
        return false;
    }

    if signed_field(incoming, "updated_at") < signed_field(existing, "updated_at") {
        return true;
    }

    for field in ["lesson", "recommendation", "anti_pattern"] {
        if normalize_text(string_field(existing, field))
            != normalize_text(string_field(incoming, field))
        {
            return true;
        }
    }

    let existing_context = string_field(existing, "context");
    let incoming_context = string_field(incoming, "context");
    !existing_context.is_empty()
        && !incoming_context.is_empty()
        && token_similarity(existing_context, incoming_context) < 0.8
}

fn has_negation(text: &str) -> bool {
    tokenize(text).into_iter().any(|token| {
        matches!(
            token.as_str(),
            "avoid" | "never" | "not" | "without" | "forbid" | "forbidden"
        )
    })
}

fn has_exception(text: &str) -> bool {
    tokenize(text)
        .into_iter()
        .any(|token| matches!(token.as_str(), "except" | "unless" | "however"))
        || normalize_text(text).contains("only when")
}

fn narrow_scope_when_supported(record: &mut Map<String, Value>) -> Result<()> {
    if string_field(record, "repository_id").is_empty() {
        return Ok(());
    }

    let scope = normalize_text(string_field(record, "scope")).replace(' ', "");
    if matches!(scope.as_str(), "global" | "framework" | "tasktype") {
        record.insert(
            "scope".to_owned(),
            serde_json::to_value(ExperienceScope::Repository)?,
        );
    }

    Ok(())
}

fn merge_unique_array(
    existing: &mut Map<String, Value>,
    incoming: &Map<String, Value>,
    field: &str,
    maximum: usize,
) -> Result<()> {
    let incoming_values = incoming
        .get(field)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if incoming_values.is_empty() {
        return Ok(());
    }

    let existing_values = existing
        .entry(field.to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    let Some(existing_values) = existing_values.as_array_mut() else {
        return Ok(());
    };
    let mut known = existing_values
        .iter()
        .map(serde_json::to_string)
        .collect::<serde_json::Result<BTreeSet<_>>>()?;

    for value in incoming_values {
        if existing_values.len() >= maximum {
            break;
        }
        if known.insert(serde_json::to_string(&value)?) {
            existing_values.push(value);
        }
    }

    Ok(())
}

fn merge_score_dimensions(
    existing: &mut Map<String, Value>,
    incoming: &Map<String, Value>,
    field: &str,
    existing_weight: f64,
    incoming_weight: f64,
) {
    let Some(incoming_scores) = incoming.get(field).and_then(Value::as_object) else {
        return;
    };
    let existing_scores = existing
        .entry(field.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(existing_scores) = existing_scores.as_object_mut() else {
        return;
    };

    for (dimension, incoming_value) in incoming_scores {
        match (
            existing_scores.get(dimension).and_then(Value::as_f64),
            incoming_value.as_f64(),
        ) {
            (Some(previous), Some(observed)) if previous.is_finite() && observed.is_finite() => {
                let average = (previous * existing_weight + observed * incoming_weight)
                    / (existing_weight + incoming_weight);
                if let Some(number) = serde_json::Number::from_f64(average) {
                    existing_scores.insert(dimension.clone(), Value::Number(number));
                }
            }
            (None, _) if is_empty_value(existing_scores.get(dimension)) => {
                existing_scores.insert(dimension.clone(), incoming_value.clone());
            }
            _ => {}
        }
    }
}

fn token_similarity(first: &str, second: &str) -> f64 {
    let first_tokens = tokenize(first).into_iter().collect::<BTreeSet<_>>();
    let second_tokens = tokenize(second).into_iter().collect::<BTreeSet<_>>();
    if first_tokens.is_empty() || second_tokens.is_empty() {
        return 0.0;
    }

    let intersection = first_tokens.intersection(&second_tokens).count();
    let union = first_tokens.union(&second_tokens).count();
    intersection as f64 / union as f64
}

fn tokenize(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn normalize_text(text: &str) -> String {
    tokenize(text).join(" ")
}

fn truncate_chars(text: &str, maximum: usize) -> String {
    text.chars().take(maximum).collect()
}

fn string_field<'record>(record: &'record Map<String, Value>, field: &str) -> &'record str {
    record
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn string_array(record: &Map<String, Value>, field: &str) -> Vec<String> {
    record
        .get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn counter_field(record: &Map<String, Value>, field: &str) -> u64 {
    record
        .get(field)
        .and_then(Value::as_u64)
        .unwrap_or_default()
}

fn signed_field(record: &Map<String, Value>, field: &str) -> i64 {
    record
        .get(field)
        .and_then(Value::as_i64)
        .unwrap_or_default()
}

fn numeric_field(record: &Map<String, Value>, field: &str) -> f64 {
    record
        .get(field)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
        .unwrap_or_default()
}

fn increment_counter(record: &mut Map<String, Value>, field: &str) {
    record.insert(
        field.to_owned(),
        Value::from(
            counter_field(record, field)
                .saturating_add(1)
                .min(u64::from(u32::MAX)),
        ),
    );
}

fn insert_finite_number(record: &mut Map<String, Value>, field: &str, value: f64) {
    if let Some(number) = serde_json::Number::from_f64(value) {
        record.insert(field.to_owned(), Value::Number(number));
    }
}

fn sqlite_counter(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn is_empty_value(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => true,
        Some(Value::String(text)) => text.trim().is_empty(),
        Some(Value::Array(values)) => values.is_empty(),
        Some(Value::Object(values)) => values.is_empty(),
        _ => false,
    }
}

fn require_run_id(run_id: &str) -> Result<()> {
    if !identity_is_safe(run_id, false) {
        bail!("experience reuse attribution requires a safe nonempty run identifier");
    }
    Ok(())
}

fn current_timestamp() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ExperienceStore) {
        let temporary = tempfile::tempdir().expect("temporary experience directory");
        let store = ExperienceStore::open(&temporary.path().join("index.sqlite"))
            .expect("open experience store");
        (temporary, store)
    }

    fn memory(
        identifier: &str,
        lesson: &str,
        source_run: &str,
        category: ExperienceCategory,
    ) -> ExperienceMemory {
        let verdict = if category == ExperienceCategory::FailureAntiPattern {
            "failed"
        } else {
            "passed"
        };
        serde_json::from_value(serde_json::json!({
            "id": identifier,
            "category": category,
            "task_type": "parser_change",
            "task_summary": "Modify the existing parser visitor safely",
            "context": "parser module",
            "environment": "rust-1.94",
            "repository_id": "repository-a",
            "repository_revision": "revision-a",
            "scope": ExperienceScope::Repository,
            "strategy": "extend the existing visitor",
            "strategy_rationale": "preserve parser architecture",
            "key_decisions": ["reuse the existing AST visitor"],
            "implementation_pattern": "visitor extension",
            "outcome": {
                "functional_correctness": 0.9,
                "code_quality": 0.8,
                "maintainability": 0.85,
                "regression_risk": 0.1
            },
            "success": category != ExperienceCategory::FailureAntiPattern,
            "tests_run": ["cargo test parser"],
            "test_results": [],
            "evaluator_scores": { "judge": 0.8 },
            "judge_feedback": "fits the existing architecture",
            "failure_reason": null,
            "what_worked": ["existing visitor"],
            "what_failed": [],
            "lesson": lesson,
            "recommendation": "extend the existing visitor",
            "anti_pattern": null,
            "confidence": 0.6,
            "generalizability": 0.55,
            "novelty": 0.4,
            "source_run_ids": [source_run],
            "evidence": [{
                "kind": "test",
                "verdict": verdict,
                "command": "cargo test parser",
                "summary": "focused parser regression tests",
                "observed_at": 1_700_000_000i64,
                "source_run_id": source_run
            }],
            "evidence_count": 1,
            "created_at": 1_700_000_000i64,
            "updated_at": 1_700_000_000i64,
            "last_used_at": null,
            "usage_count": 0,
            "retrieved_count": 0,
            "followed_count": 0,
            "successful_reuse_count": 0,
            "failed_reuse_count": 0,
            "status": ExperienceStatus::Active,
            "superseded_by": null
        }))
        .expect("valid experience fixture")
    }

    fn persisted_text_surfaces(
        store: &ExperienceStore,
        experience_id: &str,
    ) -> (String, String, String) {
        let (record_json, projected_lesson) = store
            .connection
            .query_row(
                "SELECT record_json, lesson FROM experiences WHERE id = ?1",
                params![experience_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .expect("persisted experience and projected lesson");
        let indexed_text = store
            .connection
            .query_row(
                "SELECT lesson || ' ' || task_summary || ' ' || strategy || ' '
                        || recommendation || ' ' || anti_pattern
                   FROM experience_fts
                  WHERE experience_id = ?1",
                params![experience_id],
                |row| row.get::<_, String>(0),
            )
            .expect("persisted experience full-text index");

        (record_json, projected_lesson, indexed_text)
    }

    fn query(text: &str) -> ExperienceQuery {
        serde_json::from_value(serde_json::json!({
            "text": text,
            "task_type": "parser_change",
            "repository_id": "repository-a",
            "repository_revision": "revision-a",
            "environment": "rust-1.94",
            "scope": ExperienceScope::Repository,
            "failure_context": null,
            "limit": 10,
            "now": 1_700_000_100i64,
            "min_confidence": 0.0,
            "include_low_confidence": true
        }))
        .expect("valid experience query fixture")
    }

    #[test]
    fn migration_is_additive_and_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("index.sqlite");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch("CREATE TABLE legacy_chunks (id TEXT PRIMARY KEY);")
                .unwrap();
        }

        let first = ExperienceStore::open(&path).unwrap();
        drop(first);
        let second = ExperienceStore::open(&path).unwrap();

        let legacy_exists = second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'legacy_chunks'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(legacy_exists, 1);
        let source_session_table_exists = second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'table' AND name = 'experience_run_sessions'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(source_session_table_exists, 1);
        assert!(second.all().unwrap().is_empty());
    }

    #[test]
    fn source_session_reference_is_durable_idempotent_and_immutable() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("index.sqlite");
        let store = ExperienceStore::open(&path).unwrap();

        assert_eq!(store.source_session_id("activation-1").unwrap(), None);
        store
            .record_source_session("activation-1", "stable-session-1")
            .unwrap();
        store
            .record_source_session("activation-1", "stable-session-1")
            .unwrap();
        assert_eq!(
            store.source_session_id("activation-1").unwrap(),
            Some("stable-session-1".to_owned())
        );
        assert!(
            store
                .record_source_session("activation-1", "different-session")
                .is_err(),
            "an existing activation must never be rebound to another session"
        );

        drop(store);
        let reopened = ExperienceStore::open(&path).unwrap();
        assert_eq!(
            reopened.source_session_id("activation-1").unwrap(),
            Some("stable-session-1".to_owned())
        );
        assert_eq!(
            reopened
                .connection
                .query_row("SELECT COUNT(*) FROM experience_run_sessions", [], |row| {
                    row.get::<_, i64>(0)
                },)
                .unwrap(),
            1
        );
    }

    #[test]
    fn source_session_references_reject_unsafe_or_credential_like_identities() {
        let (_temporary, store) = store();
        let unsafe_identities = [
            "",
            " ",
            ".",
            "contains whitespace",
            "nested/identity",
            "nested\\identity",
            "../secret",
            "/tmp/session",
            "session..identity",
            "session@example.com",
            "key=AIzaSyDUMMYEXAMPLEFORTESTING1234567890A",
            "Authorization:Basic",
            "Unicode-\u{1F512}",
            "line\nbreak",
        ];

        for unsafe_identity in unsafe_identities {
            assert!(
                store
                    .record_source_session(unsafe_identity, "safe-session")
                    .is_err(),
                "unsafe activation identifier must be rejected"
            );
            assert!(
                store
                    .record_source_session("safe-activation", unsafe_identity)
                    .is_err(),
                "unsafe session identifier must be rejected"
            );
            assert!(
                store.source_session_id(unsafe_identity).is_err(),
                "unsafe lookup identifiers must be rejected"
            );
        }

        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM experience_run_sessions", [], |row| {
                    row.get::<_, i64>(0)
                },)
                .unwrap(),
            0,
            "rejected credentials and traversal strings must never reach SQLite"
        );
    }

    #[test]
    fn source_session_references_remain_workspace_local_and_fail_closed() {
        let temporary = tempfile::tempdir().unwrap();
        let first = ExperienceStore::open(&temporary.path().join("first/index.sqlite")).unwrap();
        let second = ExperienceStore::open(&temporary.path().join("second/index.sqlite")).unwrap();

        first
            .record_source_session("activation-1", "stable-session-1")
            .unwrap();
        assert_eq!(second.source_session_id("activation-1").unwrap(), None);

        second
            .connection
            .execute(
                "INSERT INTO experience_run_sessions (run_id, session_id, recorded_at)
                 VALUES (?1, ?2, ?3)",
                params!["activation-1", "../stolen-session", 1],
            )
            .unwrap();
        assert!(
            second.source_session_id("activation-1").is_err(),
            "a corrupted session reference must never become a navigable path"
        );
    }

    #[test]
    fn source_session_reference_retention_discards_the_oldest_mappings() {
        let (_temporary, store) = store();
        for (run_id, observed_at) in [("run-old", 1), ("run-middle", 2), ("run-new", 3)] {
            store
                .connection
                .execute(
                    "INSERT INTO experience_run_sessions (run_id, session_id, recorded_at)
                     VALUES (?1, ?2, ?3)",
                    params![run_id, format!("session-{run_id}"), observed_at],
                )
                .unwrap();
        }

        enforce_source_session_limit(&store.connection, 2).unwrap();
        assert_eq!(store.source_session_id("run-old").unwrap(), None);
        assert_eq!(
            store.source_session_id("run-middle").unwrap(),
            Some("session-run-middle".to_owned())
        );
        assert_eq!(
            store.source_session_id("run-new").unwrap(),
            Some("session-run-new".to_owned())
        );
    }

    #[test]
    fn outcome_filtered_retrieval_selects_matching_rows_before_candidate_limit() {
        let (_temporary, store) = store();
        let transaction = store.transaction().unwrap();

        for index in 0..(MAX_CANDIDATES + 12) {
            let mut successful = memory(
                &format!("success-{index:03}"),
                "Parser visitor strategy passed its focused regression checks",
                &format!("successful-run-{index}"),
                ExperienceCategory::SuccessfulPattern,
            );
            successful.confidence = 0.9;
            successful.updated_at = 1_700_000_100 + index as i64;
            let record = serde_json::to_value(successful).unwrap();
            persist_record(&transaction, record.as_object().unwrap()).unwrap();
        }

        let mut failure = memory(
            "rare-failure",
            "Parser visitor strategy failed with the original regression",
            "failed-run",
            ExperienceCategory::FailureAntiPattern,
        );
        failure.confidence = 0.1;
        let record = serde_json::to_value(failure).unwrap();
        persist_record(&transaction, record.as_object().unwrap()).unwrap();
        transaction.commit().unwrap();

        let mut query = query("parser visitor strategy regression");
        query.limit = 1;
        let failures = store.retrieve_with_outcome(&query, Some(false)).unwrap();

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].memory.id, "rare-failure");
        assert_eq!(failures[0].memory.success, Some(false));
    }

    #[test]
    fn outcome_filtered_retrieval_rejects_foreign_generalized_and_unknown_records() {
        let (_temporary, store) = store();

        let local = memory(
            "local-success",
            "Parser visitor strategy passed local verification",
            "local-run",
            ExperienceCategory::SuccessfulPattern,
        );
        store.upsert(&local).unwrap();

        let mut foreign = memory(
            "foreign-generalized",
            "Parser visitor strategy passed verification in another repository",
            "foreign-run-1",
            ExperienceCategory::SuccessfulPattern,
        );
        foreign.repository_id = "repository-b".to_owned();
        foreign.scope = ExperienceScope::Global;
        foreign.generalizability = 0.95;
        for source in ["foreign-run-2", "foreign-run-3"] {
            foreign.source_run_ids.push(source.to_owned());
            let mut supporting_signal = foreign.evidence[0].clone();
            supporting_signal.source_run_id = Some(source.to_owned());
            foreign.evidence.push(supporting_signal);
        }
        store.upsert(&foreign).unwrap();

        let mut unknown = memory(
            "unknown-outcome",
            "Parser visitor strategy has unresolved verification results",
            "unknown-run",
            ExperienceCategory::UncertainHypothesis,
        );
        unknown.success = None;
        store.upsert(&unknown).unwrap();

        let query = query("parser visitor strategy verification");
        let general_planning = store.retrieve(&query).unwrap();
        assert!(
            general_planning
                .iter()
                .any(|experience| experience.memory.id == "foreign-generalized"),
            "ordinary planning may use sufficiently generalized cross-repository advice"
        );

        let explicit_search = store.retrieve_with_outcome(&query, None).unwrap();
        assert_eq!(explicit_search.len(), 1);
        assert_eq!(explicit_search[0].memory.id, "local-success");

        for missing_repository in [None, Some(String::new()), Some(" ".to_owned())] {
            let mut unscoped_query = query.clone();
            unscoped_query.repository_id = missing_repository;
            assert!(
                store
                    .retrieve_with_outcome(&unscoped_query, None)
                    .unwrap()
                    .is_empty(),
                "explicit search must fail closed without a workspace identity"
            );
        }
    }

    #[test]
    fn roundtrip_preserves_multidimensional_outcomes() {
        let (_temporary, store) = store();
        let original = memory(
            "experience-1",
            "Extend the existing parser AST visitor",
            "run-1",
            ExperienceCategory::SuccessfulPattern,
        );

        assert_eq!(store.upsert(&original).unwrap(), "experience-1");
        let restored = store.get("experience-1").unwrap().unwrap();
        let original_json = serde_json::to_value(original).unwrap();
        let restored_json = serde_json::to_value(restored).unwrap();

        assert_eq!(restored_json["outcome"], original_json["outcome"]);
        assert_eq!(
            restored_json["evaluator_scores"],
            original_json["evaluator_scores"]
        );
        assert_eq!(
            restored_json["key_decisions"],
            original_json["key_decisions"]
        );
    }

    #[test]
    fn independent_near_duplicates_consolidate_without_double_counting() {
        let (_temporary, store) = store();
        let first = memory(
            "experience-1",
            "Extend the existing parser AST visitor rather than replacing parser internals",
            "run-1",
            ExperienceCategory::SuccessfulPattern,
        );
        let second = memory(
            "experience-2",
            "Extend existing parser AST visitor rather than replacing parser internals",
            "run-2",
            ExperienceCategory::SuccessfulPattern,
        );

        store.upsert(&first).unwrap();
        assert_eq!(store.upsert(&second).unwrap(), "experience-1");
        store.upsert(&second).unwrap();

        let experiences = store.all().unwrap();
        assert_eq!(experiences.len(), 1);
        let consolidated = serde_json::to_value(&experiences[0]).unwrap();
        assert_eq!(
            consolidated["source_run_ids"],
            serde_json::json!(["run-1", "run-2"])
        );
        assert_eq!(consolidated["evidence_count"], 2);
        assert!(consolidated["confidence"].as_f64().unwrap() > 0.6);
    }

    #[test]
    fn contradictions_and_exceptions_remain_separate() {
        let (_temporary, store) = store();
        let positive = memory(
            "positive",
            "Use batching for parser synchronization during ordinary workloads",
            "run-1",
            ExperienceCategory::ToolProcessLesson,
        );
        let mut negative = serde_json::to_value(memory(
            "negative",
            "Avoid batching for parser synchronization during ordinary workloads",
            "run-2",
            ExperienceCategory::ToolProcessLesson,
        ))
        .unwrap();
        negative["recommendation"] = Value::Null;
        negative["anti_pattern"] = Value::String("batching during lock contention".to_owned());
        let negative: ExperienceMemory = serde_json::from_value(negative).unwrap();
        let exception = memory(
            "exception",
            "Use batching for parser synchronization during ordinary workloads except migrations",
            "run-3",
            ExperienceCategory::ToolProcessLesson,
        );

        store.upsert(&positive).unwrap();
        store.upsert(&negative).unwrap();
        store.upsert(&exception).unwrap();

        assert_eq!(store.all().unwrap().len(), 3);
    }

    #[test]
    fn incompatible_environment_does_not_consolidate() {
        let (_temporary, store) = store();
        let first = memory(
            "experience-1",
            "Run generated parser validation before the integration suite",
            "run-1",
            ExperienceCategory::ToolProcessLesson,
        );
        let mut second = serde_json::to_value(memory(
            "experience-2",
            "Run generated parser validation before the integration suite",
            "run-2",
            ExperienceCategory::ToolProcessLesson,
        ))
        .unwrap();
        second["environment"] = Value::String("rust-1.95".to_owned());

        store.upsert(&first).unwrap();
        store
            .upsert(&serde_json::from_value::<ExperienceMemory>(second).unwrap())
            .unwrap();

        assert_eq!(store.all().unwrap().len(), 2);
    }

    #[test]
    fn retrieval_handles_hostile_fts_syntax_and_empty_terms() {
        let (_temporary, store) = store();
        store
            .upsert(&memory(
                "experience-1",
                "Generated parser files are overwritten during build",
                "run-1",
                ExperienceCategory::EnvironmentalFact,
            ))
            .unwrap();

        let hostile = query("generated OR \" parser ) NEAR * -- files");
        assert!(!store.retrieve(&hostile).unwrap().is_empty());
        assert!(store.retrieve(&query("((( *** :::")).is_ok());
    }

    #[test]
    fn retrieval_is_idempotent_per_run() {
        let (_temporary, store) = store();
        store
            .upsert(&memory(
                "experience-1",
                "Extend the existing parser visitor",
                "source-run",
                ExperienceCategory::SuccessfulPattern,
            ))
            .unwrap();

        store
            .record_retrieval(
                "reuse-run",
                &["experience-1".to_owned(), "experience-1".to_owned()],
            )
            .unwrap();
        store
            .record_retrieval("reuse-run", &["experience-1".to_owned()])
            .unwrap();
        assert_eq!(store.retrieved_for_run("reuse-run").unwrap().len(), 1);
        assert_eq!(
            store.get("experience-1").unwrap().unwrap().retrieved_count,
            1
        );

        store
            .record_retrieval("another-run", &["experience-1".to_owned()])
            .unwrap();
        assert_eq!(
            store.get("experience-1").unwrap().unwrap().retrieved_count,
            2
        );
    }

    #[test]
    fn follow_attribution_requires_prior_retrieval_and_is_idempotent() {
        let (_temporary, store) = store();
        store
            .upsert(&memory(
                "experience-1",
                "Extend the existing parser visitor",
                "source-run",
                ExperienceCategory::SuccessfulPattern,
            ))
            .unwrap();

        store.record_followed("reuse-run", "experience-1").unwrap();
        assert_eq!(
            store.get("experience-1").unwrap().unwrap().followed_count,
            0
        );

        store
            .record_retrieval("reuse-run", &["experience-1".to_owned()])
            .unwrap();
        store.record_followed("reuse-run", "experience-1").unwrap();
        store.record_followed("reuse-run", "experience-1").unwrap();

        assert_eq!(
            store.get("experience-1").unwrap().unwrap().followed_count,
            1
        );
    }

    #[test]
    fn successful_finalization_reinforces_only_followed_memories_once() {
        let (_temporary, store) = store();
        let followed = memory(
            "followed",
            "Extend the parser AST visitor",
            "source-run-1",
            ExperienceCategory::SuccessfulPattern,
        );
        let unfollowed = memory(
            "unfollowed",
            "Validate generated schema before integration tests",
            "source-run-2",
            ExperienceCategory::ToolProcessLesson,
        );
        store.upsert(&followed).unwrap();
        store.upsert(&unfollowed).unwrap();

        store
            .record_retrieval(
                "reuse-run",
                &["followed".to_owned(), "unfollowed".to_owned()],
            )
            .unwrap();
        store.record_followed("reuse-run", "followed").unwrap();
        store.finalize_run("reuse-run", true).unwrap();
        store.finalize_run("reuse-run", true).unwrap();

        let followed = serde_json::to_value(store.get("followed").unwrap().unwrap()).unwrap();
        let unfollowed = serde_json::to_value(store.get("unfollowed").unwrap().unwrap()).unwrap();
        assert_eq!(followed["successful_reuse_count"], 1);
        assert!(followed["confidence"].as_f64().unwrap() > 0.6);
        assert_eq!(unfollowed["successful_reuse_count"], 0);
        assert_eq!(unfollowed["confidence"], 0.6);
    }

    #[test]
    fn repeated_failed_reuse_reduces_confidence_and_deprecates() {
        let (_temporary, store) = store();
        store
            .upsert(&memory(
                "experience-1",
                "Use parallel parser synchronization",
                "source-run",
                ExperienceCategory::SuccessfulPattern,
            ))
            .unwrap();

        for attempt in 0..4 {
            let run_id = format!("failed-run-{attempt}");
            store
                .record_retrieval(&run_id, &["experience-1".to_owned()])
                .unwrap();
            store.record_followed(&run_id, "experience-1").unwrap();
            store.finalize_run(&run_id, false).unwrap();
        }

        let record = serde_json::to_value(store.get("experience-1").unwrap().unwrap()).unwrap();
        assert_eq!(record["failed_reuse_count"], 4);
        assert!(record["confidence"].as_f64().unwrap() < 0.6);
        assert!(record["generalizability"].as_f64().unwrap() < 0.55);
        assert_eq!(
            record["status"],
            serde_json::to_value(ExperienceStatus::Deprecated).unwrap()
        );
    }

    #[test]
    fn repeated_failed_reuse_narrows_broad_scope_conservatively() {
        let (_temporary, store) = store();
        let mut broad = serde_json::to_value(memory(
            "experience-1",
            "Use batched parser synchronization across repositories",
            "source-run",
            ExperienceCategory::SuccessfulPattern,
        ))
        .unwrap();
        broad["scope"] = serde_json::to_value(ExperienceScope::Global).unwrap();
        store
            .upsert(&serde_json::from_value::<ExperienceMemory>(broad).unwrap())
            .unwrap();

        for attempt in 0..2 {
            let run_id = format!("failed-run-{attempt}");
            store
                .record_retrieval(&run_id, &["experience-1".to_owned()])
                .unwrap();
            store.record_followed(&run_id, "experience-1").unwrap();
            store.finalize_run(&run_id, false).unwrap();
        }

        let record = serde_json::to_value(store.get("experience-1").unwrap().unwrap()).unwrap();
        assert_eq!(
            record["scope"],
            serde_json::to_value(ExperienceScope::Repository).unwrap()
        );
        assert_eq!(
            record["status"],
            serde_json::to_value(ExperienceStatus::LowConfidence).unwrap()
        );
    }

    #[test]
    fn invalidation_preserves_history_but_excludes_guidance() {
        let (_temporary, store) = store();
        store
            .upsert(&memory(
                "experience-1",
                "Generated parser synchronization must run first",
                "source-run",
                ExperienceCategory::ArchitecturalLesson,
            ))
            .unwrap();

        store
            .invalidate(
                "experience-1",
                ExperienceStatus::Superseded,
                Some("replacement"),
            )
            .unwrap();

        let stored = serde_json::to_value(store.get("experience-1").unwrap().unwrap()).unwrap();
        assert_eq!(stored["superseded_by"], "replacement");
        assert_eq!(store.all().unwrap().len(), 1);
        assert!(
            store
                .retrieve(&query("generated parser synchronization"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn active_cap_deprecates_low_value_memories_without_deleting_them() {
        let (_temporary, store) = store();
        for index in 0..4 {
            let record = memory(
                &format!("experience-{index}"),
                &format!("distinct implementation lesson number {index}"),
                &format!("source-run-{index}"),
                ExperienceCategory::UncertainHypothesis,
            );
            store.upsert(&record).unwrap();
        }

        let transaction = store.transaction().unwrap();
        enforce_active_limit(&transaction, 2).unwrap();
        transaction.commit().unwrap();

        let records = store.all().unwrap();
        assert_eq!(records.len(), 4);
        let active = records
            .iter()
            .map(|record| serde_json::to_value(record).unwrap())
            .filter(|record| {
                record["status"] == serde_json::to_value(ExperienceStatus::Active).unwrap()
            })
            .count();
        assert_eq!(active, 2);
    }

    #[test]
    fn multiple_handles_share_the_existing_database_safely() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("index.sqlite");
        let first = ExperienceStore::open(&path).unwrap();
        let second = ExperienceStore::open(&path).unwrap();

        first
            .upsert(&memory(
                "experience-1",
                "Extend existing parser visitor",
                "source-run",
                ExperienceCategory::SuccessfulPattern,
            ))
            .unwrap();

        assert!(second.get("experience-1").unwrap().is_some());
        second
            .record_retrieval("reuse-run", &["experience-1".to_owned()])
            .unwrap();
        assert_eq!(
            first.get("experience-1").unwrap().unwrap().retrieved_count,
            1
        );
    }

    #[test]
    fn finalized_runs_cannot_gain_new_attribution() {
        let (_temporary, store) = store();
        store
            .upsert(&memory(
                "experience-1",
                "Extend existing parser visitor",
                "source-run",
                ExperienceCategory::SuccessfulPattern,
            ))
            .unwrap();
        store
            .record_retrieval("reuse-run", &["experience-1".to_owned()])
            .unwrap();
        store.finalize_run("reuse-run", true).unwrap();
        store.record_followed("reuse-run", "experience-1").unwrap();
        store.finalize_run("reuse-run", true).unwrap();

        let record = store.get("experience-1").unwrap().unwrap();
        assert_eq!(record.followed_count, 0);
        assert_eq!(record.successful_reuse_count, 0);
    }

    #[test]
    fn direct_upsert_redacts_hostile_semantic_fields_before_json_and_fts() {
        let (_temporary, store) = store();
        let mut hostile = serde_json::to_value(memory(
            "stable-experience-id",
            "Authorization: Bearer bearer-direct-secret",
            "stable-source-run-id",
            ExperienceCategory::FailureAntiPattern,
        ))
        .unwrap();
        hostile["repository_id"] = Value::String("stable-repository-id".to_owned());
        hostile["task_summary"] =
            Value::String("Cookie: session=cookie-direct-secret; theme=dark".to_owned());
        hostile["context"] = Value::String(
            "Connect to https://alice:url-direct-password@example.invalid/private".to_owned(),
        );
        hostile["strategy"] = Value::String("refresh_token=refresh-direct-secret".to_owned());
        hostile["strategy_rationale"] = Value::String(
            "-----BEGIN PRIVATE KEY-----\nprivate-direct-key-material\n-----END PRIVATE KEY-----"
                .to_owned(),
        );
        hostile["key_decisions"] = serde_json::json!(["--api-key decision-direct-secret"]);
        hostile["tests_run"] = serde_json::json!(["cargo test --token command-direct-secret"]);
        hostile["recommendation"] =
            Value::String("API_KEY=recommendation-direct-secret".to_owned());
        hostile["anti_pattern"] = Value::String("Bearer antipattern-direct-secret".to_owned());
        hostile["judge_feedback"] =
            Value::String("refresh_token: feedback-direct-secret".to_owned());
        hostile["what_failed"] = serde_json::json!(["Authorization: Bearer failure-direct-secret"]);
        hostile["evaluator_scores"] = serde_json::json!({"judge/api_token": 0.8});
        hostile["evidence"] = serde_json::json!([{
            "kind": "test",
            "verdict": "failed",
            "command": "curl --password evidence-direct-password",
            "summary": "Cookie: evidence=evidence-direct-cookie",
            "source_run_id": "stable-nested-source-run-id"
        }]);

        store
            .upsert(&serde_json::from_value::<ExperienceMemory>(hostile).unwrap())
            .unwrap();

        let record_json = store
            .connection
            .query_row(
                "SELECT record_json FROM experiences WHERE id = ?1",
                params!["stable-experience-id"],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let indexed_text = store
            .connection
            .query_row(
                "SELECT lesson || ' ' || task_summary || ' ' || strategy || ' '
                        || recommendation || ' ' || anti_pattern
                   FROM experience_fts
                  WHERE experience_id = ?1",
                params!["stable-experience-id"],
                |row| row.get::<_, String>(0),
            )
            .unwrap();

        for secret in [
            "bearer-direct-secret",
            "cookie-direct-secret",
            "url-direct-password",
            "refresh-direct-secret",
            "private-direct-key-material",
            "decision-direct-secret",
            "command-direct-secret",
            "recommendation-direct-secret",
            "antipattern-direct-secret",
            "feedback-direct-secret",
            "failure-direct-secret",
            "evidence-direct-password",
            "evidence-direct-cookie",
        ] {
            assert!(
                !record_json.contains(secret),
                "secret persisted in JSON: {secret}"
            );
            assert!(
                !indexed_text.contains(secret),
                "secret persisted in FTS: {secret}"
            );
        }

        let restored =
            serde_json::to_value(store.get("stable-experience-id").unwrap().unwrap()).unwrap();
        assert_eq!(restored["id"], "stable-experience-id");
        assert_eq!(restored["repository_id"], "stable-repository-id");
        assert_eq!(
            restored["source_run_ids"],
            serde_json::json!(["stable-source-run-id"])
        );
        assert_eq!(
            restored["evidence"][0]["source_run_id"],
            "stable-nested-source-run-id"
        );
        assert_eq!(restored["evaluator_scores"]["judge/api_token"], 0.8);
    }

    #[test]
    fn direct_upsert_redacts_basic_authorization_before_json_projection_and_fts() {
        const LESSON_CREDENTIAL: &str = "dXNlcjpsZXNzb24tZG8tbm90LXBlcnNpc3Q=";
        const TASK_CREDENTIAL: &str = "cHJveHk6dGFzay1kby1ub3QtcGVyc2lzdA==";
        const STRATEGY_CREDENTIAL: &str = "dXNlcjpzdHJhdGVneS1kby1ub3QtcGVyc2lzdA==";
        const RECOMMENDATION_CREDENTIAL: &str = "dXNlcjpyZWNvbW1lbmQtZG8tbm90LXBlcnNpc3Q=";
        const EVIDENCE_CREDENTIAL: &str = "dXNlcjpldmlkZW5jZS1kby1ub3QtcGVyc2lzdA==";

        let (_temporary, store) = store();
        let mut hostile = memory(
            "basic-authorization-experience",
            &format!("Retry Authorization: Basic {LESSON_CREDENTIAL} safely"),
            "basic-authorization-source-run",
            ExperienceCategory::SuccessfulPattern,
        );
        hostile.task_summary = format!("proxy-authorization: BASIC {TASK_CREDENTIAL}");
        hostile.strategy = format!("authorization: bAsIc {STRATEGY_CREDENTIAL}");
        hostile.recommendation = Some(format!(
            "Inspect Authorization Basic {RECOMMENDATION_CREDENTIAL}"
        ));
        hostile.evidence[0].command = Some(format!(
            "curl -H 'Authorization: Basic {EVIDENCE_CREDENTIAL}' https://example.invalid"
        ));

        let identifier = store.upsert(&hostile).unwrap();
        let (record_json, projected_lesson, indexed_text) =
            persisted_text_surfaces(&store, &identifier);

        for secret in [
            LESSON_CREDENTIAL,
            TASK_CREDENTIAL,
            STRATEGY_CREDENTIAL,
            RECOMMENDATION_CREDENTIAL,
            EVIDENCE_CREDENTIAL,
        ] {
            assert!(
                !record_json.contains(secret),
                "Basic credential persisted in JSON: {secret}"
            );
            assert!(
                !projected_lesson.contains(secret),
                "Basic credential persisted in the projected lesson: {secret}"
            );
            assert!(
                !indexed_text.contains(secret),
                "Basic credential persisted in FTS: {secret}"
            );
        }

        let restored = store.get(&identifier).unwrap().unwrap();
        assert!(restored.lesson.contains("Basic"));
        assert!(restored.lesson.contains("[REDACTED]"));
    }

    #[test]
    fn direct_upsert_redacts_extended_authorization_schemes_before_json_projection_and_fts() {
        for (index, scheme) in [
            "Bearer",
            "Basic",
            "Digest",
            "dIgEsT",
            "Negotiate",
            "nEgOtIaTe",
            "NTLM",
            "Signature",
            "AWS4-HMAC-SHA256",
            "Custom-Proof-42",
            "Token",
            "ApiKey",
            "API-Key",
            "OAuth",
            "DPoP",
        ]
        .into_iter()
        .enumerate()
        {
            let (_temporary, store) = store();
            let credential = format!("opaqueauthenticationmaterial{index}donotpersist");
            let mut hostile = memory(
                &format!("extended-authorization-experience-{index}"),
                &format!("Retry Authorization:{scheme} {credential} safely"),
                &format!("extended-authorization-run-{index}"),
                ExperienceCategory::SuccessfulPattern,
            );
            hostile.task_summary =
                format!("Proxy-Authorization:{scheme} {credential} retained-context");
            hostile.strategy = format!("authorization:{scheme} {credential}");
            hostile.recommendation = Some(format!("Authorization {scheme} {credential}"));
            hostile.evidence[0].command = Some(format!(
                "curl -H 'Authorization: {scheme} {credential}' https://example.invalid"
            ));

            let identifier = store.upsert(&hostile).unwrap();
            let (record_json, projected_lesson, indexed_text) =
                persisted_text_surfaces(&store, &identifier);

            for (surface_name, surface) in [
                ("JSON", record_json),
                ("projected lesson", projected_lesson),
                ("FTS", indexed_text),
            ] {
                assert!(
                    !surface.contains(&credential),
                    "{scheme} credential persisted in {surface_name}: {credential}"
                );
            }

            let restored = store.get(&identifier).unwrap().unwrap();
            assert!(restored.lesson.contains(&format!("{scheme} [REDACTED]")));
            assert!(restored.task_summary.contains("retained-context"));
        }
    }

    #[test]
    fn direct_upsert_redacts_digest_auth_parameters_before_json_projection_and_fts() {
        let (_temporary, store) = store();
        let hostile = memory(
            "digest-authorization-experience",
            concat!(
                "Authorization: Digest ",
                "username = \"hidden-digest-user hidden-digest-user-continuation\" , ",
                "realm = \"hidden-digest-realm hidden-digest-realm-continuation\" , ",
                "nonce = hidden-digest-nonce , ",
                "custom_proof = hidden-digest-extension , ",
                "response = hidden-digest-response , ",
                "cnonce = hidden-digest-cnonce ",
                "retained-context"
            ),
            "digest-authorization-source-run",
            ExperienceCategory::SuccessfulPattern,
        );

        let identifier = store.upsert(&hostile).unwrap();
        let (record_json, projected_lesson, indexed_text) =
            persisted_text_surfaces(&store, &identifier);

        for secret in [
            "hidden-digest-user",
            "hidden-digest-user-continuation",
            "hidden-digest-realm",
            "hidden-digest-realm-continuation",
            "hidden-digest-nonce",
            "hidden-digest-extension",
            "hidden-digest-response",
            "hidden-digest-cnonce",
        ] {
            assert!(
                !record_json.contains(secret),
                "Digest credential persisted in JSON: {secret}"
            );
            assert!(
                !projected_lesson.contains(secret),
                "Digest credential persisted in the projected lesson: {secret}"
            );
            assert!(
                !indexed_text.contains(secret),
                "Digest credential persisted in FTS: {secret}"
            );
        }

        let restored = store.get(&identifier).unwrap().unwrap();
        assert!(restored.lesson.contains("Authorization: Digest [REDACTED]"));
        assert!(restored.lesson.contains("retained-context"));
    }

    #[test]
    fn direct_upsert_redacts_oauth_auth_parameters_before_json_projection_and_fts() {
        let (_temporary, store) = store();
        let hostile = memory(
            "oauth-authorization-experience",
            concat!(
                "Authorization: OAuth ",
                "oauth_consumer_key = \"hidden-oauth-consumer hidden-oauth-continuation\" , ",
                "oauth_nonce = hidden-oauth-nonce , ",
                "custom_proof = hidden-oauth-extension , ",
                "oauth_signature = hidden-oauth-signature , ",
                "oauth_body_hash = hidden-oauth-body-hash ",
                "retained-context"
            ),
            "oauth-authorization-source-run",
            ExperienceCategory::SuccessfulPattern,
        );

        let identifier = store.upsert(&hostile).unwrap();
        let (record_json, projected_lesson, indexed_text) =
            persisted_text_surfaces(&store, &identifier);

        for secret in [
            "hidden-oauth-consumer",
            "hidden-oauth-continuation",
            "hidden-oauth-nonce",
            "hidden-oauth-extension",
            "hidden-oauth-signature",
            "hidden-oauth-body-hash",
        ] {
            assert!(
                !record_json.contains(secret),
                "OAuth credential persisted in JSON: {secret}"
            );
            assert!(
                !projected_lesson.contains(secret),
                "OAuth credential persisted in the projected lesson: {secret}"
            );
            assert!(
                !indexed_text.contains(secret),
                "OAuth credential persisted in FTS: {secret}"
            );
        }

        let restored = store.get(&identifier).unwrap().unwrap();
        assert!(restored.lesson.contains("Authorization: OAuth [REDACTED]"));
        assert!(restored.lesson.contains("retained-context"));
    }

    #[test]
    fn direct_upsert_redacts_signature_and_aws_auth_parameters_on_every_surface() {
        for (index, scheme, parameters) in [
            (
                0,
                "Signature",
                concat!(
                    "keyId = \"hidden-signing-key\" , ",
                    "custom_proof = \"hidden-extension\" , ",
                    "signature = \"hidden-signature\""
                ),
            ),
            (
                1,
                "AWS4-HMAC-SHA256",
                concat!(
                    "Credential = \"hidden-signing-key\" , ",
                    "SignedHeaders = hidden-extension , ",
                    "Signature = hidden-signature"
                ),
            ),
        ] {
            let (_temporary, store) = store();
            let hostile = memory(
                &format!("parameterized-authorization-experience-{index}"),
                &format!("Proxy-Authorization:{scheme} {parameters} retained-context"),
                &format!("parameterized-authorization-run-{index}"),
                ExperienceCategory::SuccessfulPattern,
            );

            let identifier = store.upsert(&hostile).unwrap();
            let surfaces = persisted_text_surfaces(&store, &identifier);

            for secret in ["hidden-signing-key", "hidden-extension", "hidden-signature"] {
                assert!(
                    !surfaces.0.contains(secret),
                    "{scheme} secret persisted in JSON"
                );
                assert!(
                    !surfaces.1.contains(secret),
                    "{scheme} secret persisted in the projected lesson"
                );
                assert!(
                    !surfaces.2.contains(secret),
                    "{scheme} secret persisted in FTS"
                );
            }

            assert!(
                store
                    .get(&identifier)
                    .unwrap()
                    .unwrap()
                    .lesson
                    .contains("retained-context")
            );
        }
    }

    #[test]
    fn direct_upsert_redacts_all_cookie_pairs_before_json_projection_and_fts() {
        for (index, header) in ["Cookie", "Set-Cookie"].into_iter().enumerate() {
            let (_temporary, store) = store();
            let hostile = memory(
                &format!("cookie-authorization-experience-{index}"),
                &format!(
                    "{header}: analytics=first-cookie-secret; remember_me=second-cookie-secret; connect.sid=third-cookie-secret retained-context"
                ),
                &format!("cookie-authorization-run-{index}"),
                ExperienceCategory::SuccessfulPattern,
            );

            let identifier = store.upsert(&hostile).unwrap();
            let surfaces = persisted_text_surfaces(&store, &identifier);

            for secret in [
                "first-cookie-secret",
                "second-cookie-secret",
                "third-cookie-secret",
            ] {
                assert!(
                    !surfaces.0.contains(secret),
                    "cookie secret persisted in JSON"
                );
                assert!(
                    !surfaces.1.contains(secret),
                    "cookie secret persisted in the projected lesson"
                );
                assert!(
                    !surfaces.2.contains(secret),
                    "cookie secret persisted in FTS"
                );
            }

            assert!(
                store
                    .get(&identifier)
                    .unwrap()
                    .unwrap()
                    .lesson
                    .contains("retained-context")
            );
        }
    }

    #[test]
    fn direct_upsert_redacts_gemini_query_keys_before_json_projection_and_fts() {
        const LESSON_KEY: &str = "AIzaSyLessonCredentialDoNotPersist11111111";
        const TASK_KEY: &str = "AIzaSyTaskCredentialDoNotPersist2222222222";
        const STRATEGY_KEY: &str = "AIzaSyStrategyCredentialDoNotPersist333333";
        const RECOMMENDATION_KEY: &str = "AIzaSyRecommendationDoNotPersist444444444";
        const EVIDENCE_KEY: &str = "AIzaSyEvidenceCredentialDoNotPersist55555";
        const STANDALONE_KEY: &str = "AIzaSyStandaloneCredentialDoNotPersist666";
        const EMBEDDED_KEY: &str = "AIzaSyEmbeddedCredentialDoNotPersist77777";

        let (_temporary, store) = store();
        let mut hostile = memory(
            "gemini-query-key-experience",
            &format!(
                "Call https://generativelanguage.googleapis.com/v1beta/models/gemini:generateContent?key={LESSON_KEY}&alt=json#safe"
            ),
            "gemini-query-key-source-run",
            ExperienceCategory::SuccessfulPattern,
        );
        hostile.task_summary = format!(
            "Inspect https://example.invalid/generate?alt=json&key={TASK_KEY}&prettyPrint=false"
        );
        hostile.strategy =
            format!("Retry https://example.invalid/generate?alt=json&KEY={STRATEGY_KEY}#fragment");
        hostile.recommendation = Some(format!(
            "Use https://example.invalid/generate?safe=true&key={RECOMMENDATION_KEY}"
        ));
        hostile.anti_pattern = Some(format!("Avoid pasting {STANDALONE_KEY} into logs"));
        hostile.context = "Keep ordinary configuration key=parser-cache unchanged".to_owned();
        hostile.judge_feedback = Some(format!(r#"{{"key":"{EMBEDDED_KEY}","region":"us"}}"#));
        hostile.evidence[0].command = Some(format!(
            "curl 'https://example.invalid/generate?format=json&key={EVIDENCE_KEY}'"
        ));

        let identifier = store.upsert(&hostile).unwrap();
        let (record_json, projected_lesson, indexed_text) =
            persisted_text_surfaces(&store, &identifier);

        for secret in [
            LESSON_KEY,
            TASK_KEY,
            STRATEGY_KEY,
            RECOMMENDATION_KEY,
            EVIDENCE_KEY,
            STANDALONE_KEY,
            EMBEDDED_KEY,
        ] {
            assert!(
                !record_json.contains(secret),
                "Gemini API key persisted in JSON: {secret}"
            );
            assert!(
                !projected_lesson.contains(secret),
                "Gemini API key persisted in the projected lesson: {secret}"
            );
            assert!(
                !indexed_text.contains(secret),
                "Gemini API key persisted in FTS: {secret}"
            );
        }

        let restored = store.get(&identifier).unwrap().unwrap();
        assert!(restored.lesson.contains("key=[REDACTED]"));
        assert!(restored.lesson.contains("alt=json"));
        assert!(restored.lesson.contains("#safe"));
        assert!(restored.context.contains("key=parser-cache"));
    }

    #[test]
    fn duplicate_source_run_does_not_refresh_recency_or_evidence() {
        let (_temporary, store) = store();
        let mut original = memory(
            "experience-1",
            "Extend the existing parser AST visitor",
            "source-run",
            ExperienceCategory::SuccessfulPattern,
        );
        original.updated_at = 100;
        store.upsert(&original).unwrap();

        let mut duplicate = original.clone();
        duplicate.updated_at = 10_000;
        duplicate.confidence = 0.95;
        duplicate.evidence_count = 50;
        store.upsert(&duplicate).unwrap();

        let restored = store.get("experience-1").unwrap().unwrap();
        assert_eq!(restored.updated_at, 100);
        assert_eq!(restored.evidence_count, 1);
        assert_eq!(restored.confidence, 0.6);
    }

    #[test]
    fn bounded_source_provenance_remembers_evicted_source_runs() {
        let (_temporary, store) = store();
        let lesson =
            "Extend the existing parser AST visitor rather than replacing parser internals";

        for index in 0..(MAX_SOURCE_RUNS + 12) {
            let mut observation = memory(
                &format!("experience-{index}"),
                lesson,
                &format!("source-run-{index}"),
                ExperienceCategory::SuccessfulPattern,
            );
            observation.updated_at = 1_700_000_000 + index as i64;
            store.upsert(&observation).unwrap();
        }

        let before = store.get("experience-0").unwrap().unwrap();
        assert_eq!(before.source_run_ids.len(), MAX_SOURCE_RUNS);
        assert!(
            before
                .source_run_ids
                .iter()
                .any(|source| source == "source-run-0")
        );
        assert!(
            !before
                .source_run_ids
                .iter()
                .any(|source| source == "source-run-1")
        );

        let mut evicted_duplicate = memory(
            "duplicate-evicted-source",
            lesson,
            "source-run-1",
            ExperienceCategory::SuccessfulPattern,
        );
        evicted_duplicate.updated_at = 1_900_000_000;
        store.upsert(&evicted_duplicate).unwrap();

        let after = store.get("experience-0").unwrap().unwrap();
        assert_eq!(after.evidence_count, before.evidence_count);
        assert_eq!(after.updated_at, before.updated_at);
        assert_eq!(after.confidence, before.confidence);
        assert_eq!(store.all().unwrap().len(), 1);
    }

    #[test]
    fn revised_near_duplicates_preserve_distinct_revision_context() {
        let (_temporary, store) = store();
        let mut original = memory(
            "revision-a",
            "Extend the existing parser AST visitor rather than replacing parser internals",
            "source-a",
            ExperienceCategory::SuccessfulPattern,
        );
        original.repository_revision = Some("git-revision-a".to_owned());
        original.updated_at = 100;
        store.upsert(&original).unwrap();

        let mut revised = memory(
            "revision-b",
            "Extend existing parser AST visitor rather than replacing parser internals",
            "source-b",
            ExperienceCategory::SuccessfulPattern,
        );
        revised.repository_revision = Some("git-revision-b".to_owned());
        revised.updated_at = 200;
        store.upsert(&revised).unwrap();

        assert_eq!(store.all().unwrap().len(), 2);
    }

    #[test]
    fn exact_reconfirmation_advances_repository_revision_with_evidence() {
        let (_temporary, store) = store();
        let lesson =
            "Extend the existing parser AST visitor rather than replacing parser internals";
        let mut original = memory(
            "revision-a",
            lesson,
            "source-a",
            ExperienceCategory::SuccessfulPattern,
        );
        original.repository_revision = Some("git-revision-a".to_owned());
        original.updated_at = 100;
        store.upsert(&original).unwrap();

        let mut reconfirmed = memory(
            "revision-b",
            lesson,
            "source-b",
            ExperienceCategory::SuccessfulPattern,
        );
        reconfirmed.repository_revision = Some("git-revision-b".to_owned());
        reconfirmed.updated_at = 200;
        assert_eq!(store.upsert(&reconfirmed).unwrap(), "revision-a");

        let restored = store.get("revision-a").unwrap().unwrap();
        assert_eq!(
            restored.repository_revision.as_deref(),
            Some("git-revision-b")
        );
        assert_eq!(restored.updated_at, 200);
        assert_eq!(restored.evidence_count, 2);
        assert_eq!(restored.source_run_ids, ["source-a", "source-b"]);
    }

    #[test]
    fn stale_revision_cannot_refresh_newer_guidance() {
        let (_temporary, store) = store();
        let lesson =
            "Extend the existing parser AST visitor rather than replacing parser internals";
        let mut current = memory(
            "current",
            lesson,
            "source-current",
            ExperienceCategory::SuccessfulPattern,
        );
        current.repository_revision = Some("git-revision-current".to_owned());
        current.updated_at = 200;
        store.upsert(&current).unwrap();

        let mut stale = memory(
            "stale",
            lesson,
            "source-stale",
            ExperienceCategory::SuccessfulPattern,
        );
        stale.repository_revision = Some("git-revision-stale".to_owned());
        stale.updated_at = 100;
        store.upsert(&stale).unwrap();

        assert_eq!(store.all().unwrap().len(), 2);
        assert_eq!(
            store
                .get("current")
                .unwrap()
                .unwrap()
                .repository_revision
                .as_deref(),
            Some("git-revision-current")
        );
    }

    #[test]
    fn total_retention_prunes_low_value_archive_and_orphaned_indexes() {
        let (_temporary, store) = store();
        let mut active = memory(
            "active",
            "Preserve an important active architectural observation",
            "source-active",
            ExperienceCategory::ArchitecturalLesson,
        );
        active.confidence = 0.95;
        store.upsert(&active).unwrap();

        for index in 0..3 {
            let mut archived = memory(
                &format!("archived-{index}"),
                &format!("archived historical observation topic {index}"),
                &format!("source-archived-{index}"),
                ExperienceCategory::UncertainHypothesis,
            );
            archived.status = ExperienceStatus::Deprecated;
            archived.confidence = 0.1 + f64::from(index) * 0.1;
            archived.updated_at = 100 + i64::from(index);
            store.upsert(&archived).unwrap();
        }

        let transaction = store.transaction().unwrap();
        enforce_retention_limits(&transaction, 2, 10, 10).unwrap();
        transaction.commit().unwrap();

        assert_eq!(store.all().unwrap().len(), 2);
        assert!(store.get("active").unwrap().is_some());
        assert!(store.get("archived-2").unwrap().is_some());
        let indexed = store
            .connection
            .query_row("SELECT COUNT(*) FROM experience_fts", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        let provenance = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM experience_source_provenance",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(indexed, 2);
        assert_eq!(provenance, 2);
    }

    #[test]
    fn retention_bounds_finalized_and_pending_reuse_history() {
        let (_temporary, store) = store();
        store
            .upsert(&memory(
                "experience-1",
                "Extend the existing parser AST visitor",
                "source-run",
                ExperienceCategory::SuccessfulPattern,
            ))
            .unwrap();

        for index in 0..5 {
            let run_id = format!("finalized-{index}");
            store
                .record_retrieval(&run_id, &["experience-1".to_owned()])
                .unwrap();
            store.finalize_run(&run_id, true).unwrap();
        }
        for index in 0..4 {
            store
                .record_retrieval(&format!("pending-{index}"), &["experience-1".to_owned()])
                .unwrap();
        }

        let transaction = store.transaction().unwrap();
        enforce_retention_limits(&transaction, 10, 2, 2).unwrap();
        transaction.commit().unwrap();

        let finalized = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM experience_reuse WHERE finalized_at IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let pending = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM experience_reuse WHERE finalized_at IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(finalized, 2);
        assert_eq!(pending, 2);
        assert!(store.get("experience-1").unwrap().is_some());
    }

    #[test]
    fn unsupported_upserts_cannot_fabricate_confidence_or_independent_evidence() {
        let (_temporary, store) = store();
        let mut unsupported = memory(
            "unsupported",
            "Treat parser synchronization as a tentative hypothesis",
            "run-one",
            ExperienceCategory::UncertainHypothesis,
        );
        unsupported.source_run_ids = vec!["run-one".to_owned(), "run-two".to_owned()];
        unsupported.evidence.clear();
        unsupported.evidence_count = 900;
        unsupported.confidence = 0.99;
        unsupported.generalizability = 0.99;
        unsupported.scope = ExperienceScope::Global;

        store.upsert(&unsupported).unwrap();
        let restored = store.get("unsupported").unwrap().unwrap();
        assert_eq!(restored.evidence_count, 0);
        assert!(restored.confidence <= 0.35);
        assert!(restored.generalizability <= 0.35);
        assert_eq!(restored.status, ExperienceStatus::LowConfidence);
        assert_eq!(restored.source_run_ids, ["run-one", "run-two"]);

        let mut mismatched = memory(
            "mismatched",
            "A mismatched signal must remain an unsupported parser claim",
            "declared-run",
            ExperienceCategory::SuccessfulPattern,
        );
        mismatched.evidence[0].source_run_id = Some("undeclared-run".to_owned());
        mismatched.evidence_count = 80;
        mismatched.confidence = 0.95;
        store.upsert(&mismatched).unwrap();

        let restored = store.get("mismatched").unwrap().unwrap();
        assert_eq!(restored.evidence_count, 0);
        assert!(restored.confidence <= 0.35);
        assert_eq!(restored.status, ExperienceStatus::LowConfidence);
    }

    #[test]
    fn conflicting_exact_identifiers_are_disambiguated_without_cross_context_merges() {
        for conflict in [
            "repository",
            "scope",
            "context",
            "environment",
            "verdict",
            "revision",
        ] {
            let (_temporary, store) = store();
            let original = memory(
                "shared-id",
                "Extend the existing parser AST visitor safely",
                "original-run",
                ExperienceCategory::SuccessfulPattern,
            );
            store.upsert(&original).unwrap();

            let mut incoming = memory(
                "shared-id",
                "Extend the existing parser AST visitor safely",
                "incoming-run",
                ExperienceCategory::SuccessfulPattern,
            );
            match conflict {
                "repository" => incoming.repository_id = "repository-b".to_owned(),
                "scope" => incoming.scope = ExperienceScope::Module,
                "context" => incoming.context = "unrelated database migrations".to_owned(),
                "environment" => incoming.environment = "rust-1.95".to_owned(),
                "verdict" => {
                    incoming.success = Some(false);
                    incoming.evidence[0].verdict = EvidenceVerdict::Failed;
                }
                "revision" => {
                    incoming.repository_revision = Some("revision-b".to_owned());
                    incoming.recommendation = Some("replace the visitor entirely".to_owned());
                }
                _ => unreachable!(),
            }

            let disambiguated = store.upsert(&incoming).unwrap();
            assert_ne!(disambiguated, "shared-id", "conflict: {conflict}");
            assert_eq!(store.upsert(&incoming).unwrap(), disambiguated);
            assert_eq!(store.all().unwrap().len(), 2, "conflict: {conflict}");
            assert_eq!(
                store.get("shared-id").unwrap().unwrap().repository_id,
                "repository-a"
            );
        }
    }

    #[test]
    fn fallback_identifiers_include_specific_scope_context() {
        let (_temporary, store) = store();
        let mut first = memory(
            "",
            "Run parser synchronization before committing generated changes",
            "shared-run",
            ExperienceCategory::ToolProcessLesson,
        );
        first.scope = ExperienceScope::ExactFile;
        first.context = "src/parser/first.rs".to_owned();

        let mut second = first.clone();
        second.context = "src/parser/second.rs".to_owned();

        let first_id = store.upsert(&first).unwrap();
        let second_id = store.upsert(&second).unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(store.all().unwrap().len(), 2);
    }

    #[test]
    fn mixed_contradictory_verdicts_reduce_confidence_and_preserve_exception() {
        let (_temporary, store) = store();
        let original = memory(
            "supported",
            "Extend the existing parser AST visitor rather than replacing parser internals",
            "run-one",
            ExperienceCategory::SuccessfulPattern,
        );
        store.upsert(&original).unwrap();

        let mut mixed = memory(
            "mixed",
            "Extend the existing parser AST visitor rather than replacing parser internals",
            "run-two",
            ExperienceCategory::SuccessfulPattern,
        );
        let mut contradiction = mixed.evidence[0].clone();
        contradiction.verdict = EvidenceVerdict::Failed;
        contradiction.command = Some("cargo test parser -- edge_case".to_owned());
        contradiction.summary = "edge case regressed".to_owned();
        mixed.evidence.push(contradiction);

        assert_eq!(store.upsert(&mixed).unwrap(), "supported");
        let restored = store.get("supported").unwrap().unwrap();
        assert!(restored.confidence < original.confidence);
        assert_eq!(restored.evidence_count, 2);
        assert!(
            restored
                .evidence
                .iter()
                .any(|signal| signal.verdict == EvidenceVerdict::Failed)
        );
    }

    #[test]
    fn inapplicable_exact_file_candidates_cannot_hide_repository_warning() {
        let (_temporary, store) = store();
        for index in 0..40 {
            let mut exact = memory(
                &format!("exact-{index}"),
                "parser synchronization parser synchronization parser synchronization",
                &format!("exact-run-{index}"),
                ExperienceCategory::SuccessfulPattern,
            );
            exact.scope = ExperienceScope::ExactFile;
            exact.context = format!("src/parser/wrong-{index}.rs");
            exact.confidence = 0.65;
            store.upsert(&exact).unwrap();
        }

        let warning = memory(
            "repository-warning",
            "Parser synchronization can corrupt generated visitors",
            "warning-run",
            ExperienceCategory::FailureAntiPattern,
        );
        store.upsert(&warning).unwrap();

        let mut request = query("parser synchronization src/parser/right.rs");
        request.limit = 1;
        let ranked = store.retrieve(&request).unwrap();
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].memory.id, "repository-warning");
    }

    #[test]
    fn deprecated_memories_require_explicit_opt_in_in_fts_and_fallback_paths() {
        let (_temporary, store) = store();
        let mut deprecated = memory(
            "deprecated",
            "Historic parser synchronization warning",
            "historic-run",
            ExperienceCategory::ToolProcessLesson,
        );
        deprecated.status = ExperienceStatus::Deprecated;
        store.upsert(&deprecated).unwrap();

        let mut lexical = query("parser synchronization");
        lexical.include_low_confidence = false;
        assert!(store.retrieve(&lexical).unwrap().is_empty());
        lexical.include_low_confidence = true;
        assert_eq!(store.retrieve(&lexical).unwrap()[0].memory.id, "deprecated");

        let mut fallback = query("((( *** :::");
        fallback.include_low_confidence = false;
        assert!(store.retrieve(&fallback).unwrap().is_empty());
        fallback.include_low_confidence = true;
        assert_eq!(
            store.retrieve(&fallback).unwrap()[0].memory.id,
            "deprecated"
        );
    }

    #[test]
    fn finalized_run_tombstones_survive_reuse_pruning_and_bound_retention() {
        let (_temporary, store) = store();
        store
            .upsert(&memory(
                "experience-1",
                "Extend the existing parser AST visitor",
                "source-run",
                ExperienceCategory::SuccessfulPattern,
            ))
            .unwrap();
        store
            .record_retrieval("completed-run", &["experience-1".to_owned()])
            .unwrap();
        store
            .record_followed("completed-run", "experience-1")
            .unwrap();
        store.finalize_run("completed-run", true).unwrap();

        let transaction = store.transaction().unwrap();
        enforce_reuse_limit(&transaction, true, 0).unwrap();
        transaction.commit().unwrap();

        store
            .record_retrieval("completed-run", &["experience-1".to_owned()])
            .unwrap();
        store
            .record_followed("completed-run", "experience-1")
            .unwrap();
        store.finalize_run("completed-run", false).unwrap();
        let restored = store.get("experience-1").unwrap().unwrap();
        assert_eq!(restored.retrieved_count, 1);
        assert_eq!(restored.followed_count, 1);
        assert_eq!(restored.successful_reuse_count, 1);
        assert_eq!(restored.failed_reuse_count, 0);
        assert!(store.retrieved_for_run("completed-run").unwrap().is_empty());

        for index in 0..4 {
            store
                .finalize_run(&format!("other-run-{index}"), true)
                .unwrap();
        }
        let transaction = store.transaction().unwrap();
        enforce_finalized_run_limit(&transaction, 2).unwrap();
        transaction.commit().unwrap();
        let count = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM experience_finalized_runs",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn legacy_finalized_reuse_rows_backfill_durable_run_tombstones() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("index.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE experience_reuse (
                    run_id TEXT NOT NULL,
                    experience_id TEXT NOT NULL,
                    retrieved_at INTEGER NOT NULL,
                    followed_at INTEGER,
                    finalized_at INTEGER,
                    successful INTEGER,
                    PRIMARY KEY (run_id, experience_id)
                 );
                 INSERT INTO experience_reuse
                    (run_id, experience_id, retrieved_at, followed_at, finalized_at, successful)
                 VALUES ('legacy-run', 'old-experience', 10, 11, 12, 1);",
            )
            .unwrap();
        drop(connection);

        let store = ExperienceStore::open(&path).unwrap();
        let finalized = store
            .connection
            .query_row(
                "SELECT finalized_at, successful FROM experience_finalized_runs WHERE run_id = ?1",
                params!["legacy-run"],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, bool>(1)?)),
            )
            .unwrap();
        assert_eq!(finalized, (12, true));
    }

    #[test]
    fn malformed_identity_and_source_provenance_cannot_persist_secrets() {
        let (_temporary, store) = store();
        let mut hostile = memory(
            "Bearer identity-secret-value",
            "Preserve safe repository identity while rejecting malformed provenance",
            "valid-session-01a0322e",
            ExperienceCategory::SuccessfulPattern,
        );
        hostile.repository_id = "/Users/example/projects/normal-repository".to_owned();
        hostile.repository_revision = Some("token=revision-secret-value".to_owned());
        hostile
            .source_run_ids
            .push("token=source-secret-value".to_owned());
        let mut malformed = hostile.evidence[0].clone();
        malformed.source_run_id = Some("Bearer nested-secret-value".to_owned());
        hostile.evidence.push(malformed);
        hostile
            .evaluator_scores
            .insert("judge/api_token".to_owned(), 0.7);

        let identifier = store.upsert(&hostile).unwrap();
        assert!(identifier.starts_with("redacted-"));
        let record_json = store
            .connection
            .query_row(
                "SELECT record_json FROM experiences WHERE id = ?1",
                params![identifier],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        for secret in [
            "identity-secret-value",
            "revision-secret-value",
            "source-secret-value",
            "nested-secret-value",
        ] {
            assert!(!record_json.contains(secret), "secret persisted: {secret}");
        }

        let restored = store.get(&identifier).unwrap().unwrap();
        assert_eq!(
            restored.repository_id,
            "/Users/example/projects/normal-repository"
        );
        assert_eq!(restored.source_run_ids, ["valid-session-01a0322e"]);
        assert_eq!(restored.evidence_count, 1);
        assert_eq!(restored.evaluator_scores["judge/api_token"], 0.7);
    }

    #[test]
    fn empty_run_identifiers_are_rejected() {
        let (_temporary, store) = store();
        assert!(store.record_retrieval("", &[]).is_err());
        assert!(store.retrieved_for_run(" ").is_err());
        assert!(store.record_followed("", "experience").is_err());
        assert!(store.finalize_run("", true).is_err());
        assert!(
            store
                .finalize_run("token=private-run-secret", true)
                .is_err()
        );
    }
}
