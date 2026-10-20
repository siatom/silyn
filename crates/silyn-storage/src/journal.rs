//! Append-only event journal with optimistic concurrency.
//!
//! `(aggregate_id, seq)` is unique. A duplicate sequence is a transient conflict:
//! the caller reloads the aggregate and retries instead of overwriting history.

use chrono::{DateTime, Utc};
use silyn_core::SilynError;
use silyn_core::event::{EventEnvelope, RunEvent};
use silyn_core::ids::{RunId, TaskId};
use silyn_core::state::TaskStatus;
use sqlx::{Row, Sqlite, SqlitePool};

use crate::codec::encode_enum;
use crate::error::{db_error, parse_id};

/// Event journal writer and reader.
pub struct Journal<'a> {
    pool: &'a SqlitePool,
}

impl<'a> Journal<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Appends one event. Fails with `Transient` when the sequence is taken.
    pub async fn append(&self, envelope: &EventEnvelope) -> Result<(), SilynError> {
        insert_event(self.pool, envelope).await
    }

    /// Updates a task status and appends an event in one transaction.
    ///
    /// If the append fails, the status update is rolled back. The runtime must
    /// use this path instead of a bare `UPDATE`.
    pub async fn append_task_transition(
        &self,
        task_id: TaskId,
        status: TaskStatus,
        updated_at: DateTime<Utc>,
        envelope: &EventEnvelope,
    ) -> Result<(), SilynError> {
        let mut transaction = self.pool.begin().await.map_err(db_error)?;

        let updated = sqlx::query("UPDATE tasks SET status = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(task_id.to_string())
            .bind(encode_enum(&status)?)
            .bind(updated_at)
            .execute(&mut *transaction)
            .await
            .map_err(db_error)?;

        if updated.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!("task {task_id} not found")));
        }

        insert_event(&mut *transaction, envelope).await?;
        transaction.commit().await.map_err(db_error)?;
        Ok(())
    }

    /// Returns events for one run ordered by sequence.
    pub async fn list_for_aggregate(
        &self,
        aggregate_id: RunId,
    ) -> Result<Vec<EventEnvelope>, SilynError> {
        let rows = sqlx::query(
            "SELECT payload_json FROM event_journal
             WHERE aggregate_id = ?1 ORDER BY seq ASC",
        )
        .bind(aggregate_id.to_string())
        .fetch_all(self.pool)
        .await
        .map_err(db_error)?;

        rows.iter()
            .map(|row| {
                let payload: Option<String> = row.try_get("payload_json").map_err(db_error)?;
                let payload = payload.ok_or_else(|| {
                    SilynError::Permanent("event payload_json is missing".to_owned())
                })?;
                serde_json::from_str(&payload).map_err(Into::into)
            })
            .collect()
    }

    /// Highest stored sequence for an aggregate, or 0 when empty.
    pub async fn latest_seq(&self, aggregate_id: RunId) -> Result<u64, SilynError> {
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) FROM event_journal WHERE aggregate_id = ?1",
        )
        .bind(aggregate_id.to_string())
        .fetch_one(self.pool)
        .await
        .map_err(db_error)?;

        u64::try_from(seq).map_err(|_| SilynError::Storage("negative event sequence".to_owned()))
    }

    /// Counts events for an aggregate.
    pub async fn count(&self, aggregate_id: RunId) -> Result<i64, SilynError> {
        sqlx::query_scalar("SELECT COUNT(*) FROM event_journal WHERE aggregate_id = ?1")
            .bind(aggregate_id.to_string())
            .fetch_one(self.pool)
            .await
            .map_err(db_error)
    }
}

async fn insert_event<'e, E>(executor: E, envelope: &EventEnvelope) -> Result<(), SilynError>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    let payload = serde_json::to_string(envelope)?;
    let seq = i64::try_from(envelope.seq)
        .map_err(|_| SilynError::Schema("event sequence exceeds i64".to_owned()))?;
    sqlx::query(
        "INSERT INTO event_journal (
            event_id, aggregate_id, seq, event_type, schema_version,
            occurred_at, correlation_id, payload_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(envelope.event_id.to_string())
    .bind(envelope.aggregate_id.to_string())
    .bind(seq)
    .bind(event_type(&envelope.event)?)
    .bind(i64::from(envelope.schema_version))
    .bind(envelope.occurred_at)
    .bind(envelope.correlation_id.map(|id| id.to_string()))
    .bind(payload)
    .execute(executor)
    .await
    .map_err(append_error)?;
    Ok(())
}

fn event_type(event: &RunEvent) -> Result<String, SilynError> {
    let value = serde_json::to_value(event)?;
    value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| SilynError::Schema("run event has no type tag".to_owned()))
}

fn append_error(error: sqlx::Error) -> SilynError {
    if let sqlx::Error::Database(database) = &error
        && database.is_unique_violation()
    {
        return SilynError::Transient(
            "event seq conflict; reload the aggregate and retry".to_owned(),
        );
    }
    db_error(error)
}

/// Parses an aggregate id stored in the journal. Exposed for recovery code.
pub fn parse_aggregate(value: &str) -> Result<RunId, SilynError> {
    parse_id(value, "event_journal.aggregate_id")
}
