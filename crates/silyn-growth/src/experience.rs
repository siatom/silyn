//! Deterministic experience extraction from verified runs.
//!
//! A run produces at most one lesson. `Success` yields a positive provisional
//! lesson, `Failed` yields a remediation lesson keyed by the failure code, and
//! `Unknown` yields nothing: missing evidence never becomes a remembered
//! success. No model is called here; the mapping is a fixed table.
//!
//! Lessons are provisional until independent evidence confirms them: at least
//! two distinct runs and at least one success. Isolation is per agent, both on
//! extraction and on reads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, ExperienceId, RunId};
use silyn_storage::db_error;
use silyn_storage::error::parse_id;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqlitePool};

use crate::verifier::CheckResult;

/// Lesson used for a positive run. Fixed so extraction stays deterministic.
pub const SUCCESS_LESSON: &str = "Verified result using independent checks.";
/// Lesson used when a failure code has no specific entry.
pub const GENERIC_LESSON: &str = "Verify the result before reporting it.";

const PROVISIONAL_SUCCESS_CONFIDENCE: f32 = 0.4;
const PROVISIONAL_FAILURE_CONFIDENCE: f32 = 0.3;
const CONFIRMED_CONFIDENCE: f32 = 0.7;

/// Longest `evidence_ref` stored; evidence strings come from checkers and are
/// truncated so one verbose check cannot bloat the row.
const MAX_EVIDENCE_REF: usize = 512;

const EXPERIENCE_COLUMNS: &str = "id, agent_id, lesson, evidence_status, confidence, \
     source_run_ids, created_at, updated_at, status";

/// How strongly an experience is supported by evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStatus {
    /// From one run, not yet confirmed.
    Provisional,
    /// Confirmed by independent evidence.
    Confirmed,
    /// Contradicted by later evidence.
    Rejected,
    /// Evidence was insufficient.
    Unknown,
}

/// Lifecycle of an experience row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperienceStatus {
    Active,
    Retired,
}

/// One stored experience and its supporting run ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperienceRecord {
    pub id: ExperienceId,
    pub agent_id: AgentId,
    pub lesson: String,
    pub evidence_status: EvidenceStatus,
    pub confidence: f32,
    pub source_run_ids: Vec<RunId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: ExperienceStatus,
}

/// Experience reads and lifecycle writes for one SQLite pool.
pub struct ExperienceStore<'a> {
    pool: &'a SqlitePool,
}

impl<'a> ExperienceStore<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Turns one verified run into at most one provisional experience.
    ///
    /// `Unknown` returns `Ok(None)` and writes nothing. An existing active
    /// lesson is reused: a new run adds one evidence row, a repeated run is a
    /// no-op, and `source_run_ids` is rebuilt from the evidence rows so the two
    /// stay in sync.
    pub async fn extract(
        &self,
        agent_id: AgentId,
        run_id: RunId,
        result: &CheckResult,
    ) -> Result<Option<ExperienceRecord>, SilynError> {
        let Some((outcome, lesson, confidence, evidence_ref)) = lesson_for(result) else {
            return Ok(None);
        };

        let normalized_lesson = normalize_lesson(lesson);
        if normalized_lesson.is_empty() {
            return Err(SilynError::Schema(
                "experience lesson must not be empty".to_owned(),
            ));
        }

        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        ensure_run_belongs_to_agent(&mut transaction, agent_id, run_id).await?;

        let now = Utc::now();
        let sql = format!(
            "SELECT {EXPERIENCE_COLUMNS} FROM experiences \
             WHERE agent_id = ?1 AND normalized_lesson = ?2 AND status = 'active'"
        );
        let existing = sqlx::query(&sql)
            .bind(agent_id.to_string())
            .bind(&normalized_lesson)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(db_error)?;

        let record = match existing {
            Some(row) => {
                let record = record_from_row(&row)?;
                let inserted = insert_evidence(
                    &mut transaction,
                    record.id,
                    run_id,
                    outcome,
                    evidence_ref,
                    now,
                )
                .await?;
                if inserted {
                    let runs = evidence_runs(&mut transaction, record.id).await?;
                    sqlx::query(
                        "UPDATE experiences SET source_run_ids = ?2, updated_at = ?3 WHERE id = ?1",
                    )
                    .bind(record.id.to_string())
                    .bind(serde_json::to_string(&runs)?)
                    .bind(now)
                    .execute(&mut *transaction)
                    .await
                    .map_err(db_error)?;

                    ExperienceRecord {
                        source_run_ids: runs,
                        updated_at: now,
                        ..record
                    }
                } else {
                    record
                }
            }
            None => {
                let id = ExperienceId::new();
                sqlx::query(
                    "INSERT INTO experiences (
                        id, agent_id, lesson, normalized_lesson, evidence_status,
                        confidence, source_run_ids, created_at, updated_at,
                        last_used_at, status
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .bind(id.to_string())
                .bind(agent_id.to_string())
                .bind(lesson)
                .bind(&normalized_lesson)
                .bind(encode_enum(&EvidenceStatus::Provisional)?)
                .bind(f64::from(confidence))
                .bind(serde_json::to_string(&[run_id])?)
                .bind(now)
                .bind(now)
                .bind(Option::<DateTime<Utc>>::None)
                .bind(encode_enum(&ExperienceStatus::Active)?)
                .execute(&mut *transaction)
                .await
                .map_err(db_error)?;

                insert_evidence(&mut transaction, id, run_id, outcome, evidence_ref, now).await?;

                ExperienceRecord {
                    id,
                    agent_id,
                    lesson: lesson.to_owned(),
                    evidence_status: EvidenceStatus::Provisional,
                    confidence,
                    source_run_ids: vec![run_id],
                    created_at: now,
                    updated_at: now,
                    status: ExperienceStatus::Active,
                }
            }
        };

        transaction.commit().await.map_err(db_error)?;
        Ok(Some(record))
    }

    /// Every experience of one agent, active first, then oldest first.
    ///
    /// Retired rows stay visible so callers can audit what was withdrawn; an
    /// experience of another agent is never returned.
    pub async fn list(&self, agent_id: AgentId) -> Result<Vec<ExperienceRecord>, SilynError> {
        let sql = format!(
            "SELECT {EXPERIENCE_COLUMNS} FROM experiences WHERE agent_id = ?1 \
             ORDER BY created_at ASC, id ASC"
        );
        let rows = sqlx::query(&sql)
            .bind(agent_id.to_string())
            .fetch_all(self.pool)
            .await
            .map_err(db_error)?;
        rows.iter().map(record_from_row).collect()
    }

    /// Confirms an experience backed by independent evidence.
    ///
    /// Requires at least two distinct `success`/`failed` evidence runs and at
    /// least one `success` among them. `Unknown` evidence never counts, and a
    /// single run can never confirm itself.
    pub async fn confirm(&self, id: ExperienceId) -> Result<ExperienceRecord, SilynError> {
        let mut transaction = self.pool.begin().await.map_err(db_error)?;
        let record = load(&mut transaction, id).await?;

        match record.status {
            ExperienceStatus::Retired => {
                return Err(SilynError::Permanent(format!(
                    "experience {id} is retired and cannot be confirmed"
                )));
            }
            ExperienceStatus::Active => {}
        }
        if record.evidence_status == EvidenceStatus::Confirmed {
            return Ok(record);
        }
        if record.evidence_status == EvidenceStatus::Rejected {
            return Err(SilynError::Permanent(format!(
                "experience {id} was rejected and cannot be confirmed"
            )));
        }

        let (runs, successes): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(DISTINCT run_id),
                    COALESCE(SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END), 0)
             FROM experience_evidence
             WHERE experience_id = ?1 AND outcome IN ('success', 'failed')",
        )
        .bind(id.to_string())
        .fetch_one(&mut *transaction)
        .await
        .map_err(db_error)?;

        if runs < 2 {
            return Err(SilynError::Permanent(format!(
                "experience {id} needs at least two distinct evidence runs, found {runs}"
            )));
        }
        if successes < 1 {
            return Err(SilynError::Permanent(format!(
                "experience {id} needs at least one successful evidence run"
            )));
        }

        let now = Utc::now();
        sqlx::query(
            "UPDATE experiences SET evidence_status = ?2, confidence = ?3, updated_at = ?4 \
             WHERE id = ?1",
        )
        .bind(id.to_string())
        .bind(encode_enum(&EvidenceStatus::Confirmed)?)
        .bind(f64::from(CONFIRMED_CONFIDENCE))
        .bind(now)
        .execute(&mut *transaction)
        .await
        .map_err(db_error)?;

        transaction.commit().await.map_err(db_error)?;

        Ok(ExperienceRecord {
            evidence_status: EvidenceStatus::Confirmed,
            confidence: CONFIRMED_CONFIDENCE,
            updated_at: now,
            ..record
        })
    }

    /// Withdraws an experience. Idempotent for an already retired row.
    ///
    /// The partial unique index only covers `active` rows, so the same lesson
    /// can be extracted again afterwards as a new active experience.
    pub async fn retire(&self, id: ExperienceId) -> Result<(), SilynError> {
        let now = Utc::now();
        let updated = sqlx::query(
            "UPDATE experiences SET status = ?2, updated_at = ?3 \
             WHERE id = ?1 AND status <> 'retired'",
        )
        .bind(id.to_string())
        .bind(encode_enum(&ExperienceStatus::Retired)?)
        .bind(now)
        .execute(self.pool)
        .await
        .map_err(db_error)?;

        if updated.rows_affected() == 0 {
            let exists: Option<String> =
                sqlx::query_scalar("SELECT id FROM experiences WHERE id = ?1")
                    .bind(id.to_string())
                    .fetch_optional(self.pool)
                    .await
                    .map_err(db_error)?;
            if exists.is_none() {
                return Err(SilynError::Permanent(format!("experience {id} not found")));
            }
        }
        Ok(())
    }
}

/// Deterministic lesson for one check result.
///
/// Returns `None` for `Unknown`: no evidence means no lesson.
fn lesson_for(result: &CheckResult) -> Option<(&'static str, &'static str, f32, Option<String>)> {
    match result {
        CheckResult::Success { evidence } => Some((
            "success",
            SUCCESS_LESSON,
            PROVISIONAL_SUCCESS_CONFIDENCE,
            first_evidence(evidence),
        )),
        CheckResult::Failed {
            code,
            reason,
            evidence,
        } => Some((
            "failed",
            lesson_for_code(code),
            PROVISIONAL_FAILURE_CONFIDENCE,
            first_evidence(evidence).or_else(|| Some(truncate(&format!("{code}: {reason}")))),
        )),
        CheckResult::Unknown { .. } => None,
    }
}

/// Fixed failure-code to lesson table.
#[must_use]
pub fn lesson_for_code(code: &str) -> &'static str {
    match code {
        "unit_mismatch" => "Check units before computing aggregates.",
        "missing_values" => "Check for missing values before counting.",
        "duplicate_rows" => "Check for duplicate rows before counting.",
        "wrong_count" => "Recompute row counts from distinct rows.",
        "tool_error" => "Inspect the tool result before assuming success.",
        _ => GENERIC_LESSON,
    }
}

/// Trims, collapses whitespace and lowercases a lesson.
///
/// The normalized form is the deduplication key, so whitespace and case
/// differences never create a second row.
#[must_use]
pub fn normalize_lesson(lesson: &str) -> String {
    let mut normalized = String::with_capacity(lesson.len());
    for word in lesson.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.extend(word.chars().flat_map(char::to_lowercase));
    }
    normalized
}

fn first_evidence(evidence: &[String]) -> Option<String> {
    evidence.first().map(|item| truncate(item))
}

fn truncate(value: &str) -> String {
    if value.chars().count() <= MAX_EVIDENCE_REF {
        return value.to_owned();
    }
    let mut truncated: String = value.chars().take(MAX_EVIDENCE_REF).collect();
    truncated.push('…');
    truncated
}

async fn ensure_run_belongs_to_agent(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    agent_id: AgentId,
    run_id: RunId,
) -> Result<(), SilynError> {
    let owner: Option<String> = sqlx::query_scalar(
        "SELECT s.agent_id FROM runs r
         JOIN tasks t ON t.id = r.task_id
         JOIN sessions s ON s.id = t.session_id
         WHERE r.id = ?1",
    )
    .bind(run_id.to_string())
    .fetch_optional(&mut **transaction)
    .await
    .map_err(db_error)?;

    match owner {
        None => Err(SilynError::Permanent(format!("run {run_id} not found"))),
        Some(owner) if owner != agent_id.to_string() => Err(SilynError::PolicyDenied(format!(
            "run {run_id} does not belong to agent {agent_id}"
        ))),
        Some(_) => Ok(()),
    }
}

async fn insert_evidence(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    experience_id: ExperienceId,
    run_id: RunId,
    outcome: &str,
    evidence_ref: Option<String>,
    now: DateTime<Utc>,
) -> Result<bool, SilynError> {
    let inserted = sqlx::query(
        "INSERT INTO experience_evidence (experience_id, run_id, outcome, evidence_ref, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT DO NOTHING",
    )
    .bind(experience_id.to_string())
    .bind(run_id.to_string())
    .bind(outcome)
    .bind(evidence_ref)
    .bind(now)
    .execute(&mut **transaction)
    .await
    .map_err(db_error)?
    .rows_affected();

    Ok(inserted > 0)
}

async fn evidence_runs(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    experience_id: ExperienceId,
) -> Result<Vec<RunId>, SilynError> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT run_id FROM experience_evidence WHERE experience_id = ?1 \
         ORDER BY created_at ASC, run_id ASC",
    )
    .bind(experience_id.to_string())
    .fetch_all(&mut **transaction)
    .await
    .map_err(db_error)?;

    rows.iter()
        .map(|value| parse_id(value, "experience_evidence.run_id"))
        .collect()
}

async fn load(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    id: ExperienceId,
) -> Result<ExperienceRecord, SilynError> {
    let sql = format!("SELECT {EXPERIENCE_COLUMNS} FROM experiences WHERE id = ?1");
    let row = sqlx::query(&sql)
        .bind(id.to_string())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(db_error)?;

    match row {
        Some(row) => record_from_row(&row),
        None => Err(SilynError::Permanent(format!("experience {id} not found"))),
    }
}

fn record_from_row(row: &SqliteRow) -> Result<ExperienceRecord, SilynError> {
    let runs: String = row.try_get("source_run_ids").map_err(db_error)?;
    let run_ids: Vec<String> = serde_json::from_str(&runs)?;

    Ok(ExperienceRecord {
        id: parse_id(
            &row.try_get::<String, _>("id").map_err(db_error)?,
            "experiences.id",
        )?,
        agent_id: parse_id(
            &row.try_get::<String, _>("agent_id").map_err(db_error)?,
            "experiences.agent_id",
        )?,
        lesson: row.try_get("lesson").map_err(db_error)?,
        evidence_status: decode_enum(
            &row.try_get::<String, _>("evidence_status")
                .map_err(db_error)?,
        )?,
        confidence: row.try_get::<f64, _>("confidence").map_err(db_error)? as f32,
        source_run_ids: run_ids
            .iter()
            .map(|value| parse_id(value, "experiences.source_run_ids"))
            .collect::<Result<_, _>>()?,
        created_at: row.try_get("created_at").map_err(db_error)?,
        updated_at: row.try_get("updated_at").map_err(db_error)?,
        status: decode_enum(&row.try_get::<String, _>("status").map_err(db_error)?)?,
    })
}

fn encode_enum<T: Serialize>(value: &T) -> Result<String, SilynError> {
    let encoded = serde_json::to_value(value)?;
    encoded
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| SilynError::Schema("enum did not encode as a string".to_owned()))
}

fn decode_enum<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, SilynError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_collapses_whitespace_and_case() {
        assert_eq!(
            normalize_lesson("  Check   UNITS\tbefore\ncomputing aggregates. "),
            "check units before computing aggregates."
        );
        assert_eq!(normalize_lesson("   "), "");
    }

    #[test]
    fn lesson_map_is_fixed() {
        assert_eq!(
            lesson_for_code("unit_mismatch"),
            "Check units before computing aggregates."
        );
        assert_eq!(
            lesson_for_code("wrong_count"),
            "Recompute row counts from distinct rows."
        );
        assert_eq!(lesson_for_code("anything_else"), GENERIC_LESSON);
    }
}
