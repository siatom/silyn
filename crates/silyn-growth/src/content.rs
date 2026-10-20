//! Content-addressed storage for memory payloads.
//!
//! A payload is stored once per sha256 digest; memory rows only keep the
//! digest. Identical text is therefore never duplicated, and deletion can tell
//! whether a payload is still referenced by another memory.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use silyn_core::SilynError;
use silyn_storage::db_error;
use sqlx::{Sqlite, SqlitePool};

/// Read/write access to `content_blobs`.
pub struct ContentStore<'a> {
    pool: &'a SqlitePool,
}

impl<'a> ContentStore<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Stores `content` and returns its sha256 hex digest.
    ///
    /// Idempotent: storing the same bytes twice keeps one row. Empty or
    /// whitespace-only content is rejected, so a stored memory always has a
    /// payload.
    pub async fn put(&self, content: &str) -> Result<String, SilynError> {
        insert_blob(self.pool, content, Utc::now()).await
    }

    /// Returns the payload for `id`, or `None` when it was purged or never
    /// existed.
    pub async fn get(&self, id: &str) -> Result<Option<String>, SilynError> {
        sqlx::query_scalar("SELECT content FROM content_blobs WHERE id = ?1")
            .bind(id)
            .fetch_optional(self.pool)
            .await
            .map_err(db_error)
    }

    /// Number of memories, in any status, that still point at this blob.
    ///
    /// `deleted` memories keep their reference until [`crate::MemoryStore::purge`]
    /// removes them, which is what keeps audit history readable.
    pub async fn ref_count(&self, id: &str) -> Result<u64, SilynError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM memories WHERE content_ref = ?1")
            .bind(id)
            .fetch_one(self.pool)
            .await
            .map_err(db_error)?;
        u64::try_from(count)
            .map_err(|_| SilynError::Storage("negative content reference count".to_owned()))
    }
}

/// Inserts a blob and returns its digest, using any SQLx executor so callers
/// can keep the write in their own transaction.
pub(crate) async fn insert_blob<'e, E>(
    executor: E,
    content: &str,
    created_at: DateTime<Utc>,
) -> Result<String, SilynError>
where
    E: sqlx::Executor<'e, Database = Sqlite>,
{
    if content.trim().is_empty() {
        return Err(SilynError::Schema(
            "memory content must not be empty".to_owned(),
        ));
    }

    let digest = Sha256::digest(content.as_bytes());
    let id = format!("{digest:x}");
    sqlx::query(
        "INSERT INTO content_blobs (id, sha256, content, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT DO NOTHING",
    )
    .bind(&id)
    .bind(&id)
    .bind(content)
    .bind(created_at)
    .execute(executor)
    .await
    .map_err(db_error)?;
    Ok(id)
}
