//! Transactional outbox.
//!
//! Events that need asynchronous derivation (indexing, experience extraction,
//! reflection) are enqueued in the same transaction as the event. Consumers are
//! idempotent on `(consumer, event_id)`.

use chrono::{DateTime, Utc};
use silyn_core::SilynError;
use silyn_core::ids::EventId;
use sqlx::{Row, SqlitePool};

use crate::error::{db_error, parse_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxMessage {
    pub id: i64,
    pub event_id: EventId,
    pub consumer: String,
    pub available_at: DateTime<Utc>,
    pub attempts: i64,
    pub processed_at: Option<DateTime<Utc>>,
}

/// Outbox queue.
pub struct Outbox<'a> {
    pool: &'a SqlitePool,
}

impl<'a> Outbox<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Enqueues a message. Returns `false` when the consumer already has it.
    pub async fn enqueue(
        &self,
        event_id: EventId,
        consumer: &str,
        available_at: DateTime<Utc>,
    ) -> Result<bool, SilynError> {
        let result = sqlx::query(
            "INSERT INTO outbox (event_id, consumer, available_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (consumer, event_id) DO NOTHING",
        )
        .bind(event_id.to_string())
        .bind(consumer)
        .bind(available_at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() > 0)
    }

    /// Returns the oldest due, unprocessed message for a consumer.
    pub async fn claim_next(
        &self,
        consumer: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<OutboxMessage>, SilynError> {
        let row = sqlx::query(
            "SELECT id, event_id, consumer, available_at, attempts, processed_at
             FROM outbox
             WHERE consumer = ?1 AND processed_at IS NULL AND available_at <= ?2
             ORDER BY id ASC LIMIT 1",
        )
        .bind(consumer)
        .bind(now)
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        row.map(|row| {
            Ok(OutboxMessage {
                id: row.try_get("id").map_err(db_error)?,
                event_id: parse_id(
                    &row.try_get::<String, _>("event_id").map_err(db_error)?,
                    "outbox.event_id",
                )?,
                consumer: row.try_get("consumer").map_err(db_error)?,
                available_at: row.try_get("available_at").map_err(db_error)?,
                attempts: row.try_get("attempts").map_err(db_error)?,
                processed_at: row.try_get("processed_at").map_err(db_error)?,
            })
        })
        .transpose()
    }

    /// Marks a message processed. Returns `false` when it was already processed.
    pub async fn mark_processed(
        &self,
        id: i64,
        processed_at: DateTime<Utc>,
    ) -> Result<bool, SilynError> {
        let result = sqlx::query(
            "UPDATE outbox SET processed_at = ?2 WHERE id = ?1 AND processed_at IS NULL",
        )
        .bind(id)
        .bind(processed_at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn pending_count(&self, consumer: &str) -> Result<i64, SilynError> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM outbox WHERE consumer = ?1 AND processed_at IS NULL",
        )
        .bind(consumer)
        .fetch_one(self.pool)
        .await
        .map_err(db_error)
    }
}
