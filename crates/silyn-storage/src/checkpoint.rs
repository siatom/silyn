//! Checkpoints for crash recovery.
//!
//! A checkpoint is only usable when its schema version matches the runtime and
//! its checksum matches the stored state reference. Mismatches fail loudly rather
//! than being guessed.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use silyn_core::SilynError;
use silyn_core::ids::RunId;
use sqlx::{Row, SqlitePool};

use crate::error::{db_error, parse_id};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointRecord {
    pub run_id: RunId,
    pub seq: i64,
    pub state_ref: String,
    pub schema_version: i64,
    pub checksum: String,
    pub created_at: DateTime<Utc>,
}

impl CheckpointRecord {
    /// Builds a checkpoint with a computed checksum.
    #[must_use]
    pub fn new(run_id: RunId, seq: i64, state_ref: impl Into<String>, schema_version: i64) -> Self {
        let state_ref = state_ref.into();
        let checksum = Self::checksum_for(&state_ref);
        Self {
            run_id,
            seq,
            state_ref,
            schema_version,
            checksum,
            created_at: Utc::now(),
        }
    }

    /// Checksum over the state reference. Once a blob store lands this covers
    /// the blob content instead of its reference.
    #[must_use]
    pub fn checksum_for(state_ref: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(state_ref.as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

/// Checkpoint reader and writer.
pub struct CheckpointStore<'a> {
    pool: &'a SqlitePool,
}

impl<'a> CheckpointStore<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Inserts a checkpoint, rejecting a checksum that does not match its state.
    pub async fn insert(&self, record: &CheckpointRecord) -> Result<(), SilynError> {
        if record.checksum != CheckpointRecord::checksum_for(&record.state_ref) {
            return Err(SilynError::Permanent(
                "checkpoint checksum does not match its state reference".to_owned(),
            ));
        }

        sqlx::query(
            "INSERT INTO checkpoints (run_id, seq, state_ref, schema_version, checksum, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(record.run_id.to_string())
        .bind(record.seq)
        .bind(&record.state_ref)
        .bind(record.schema_version)
        .bind(&record.checksum)
        .bind(record.created_at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    /// Loads the newest checkpoint and validates version and checksum.
    pub async fn latest(
        &self,
        run_id: RunId,
        expected_schema_version: i64,
    ) -> Result<Option<CheckpointRecord>, SilynError> {
        let row = sqlx::query(
            "SELECT run_id, seq, state_ref, schema_version, checksum, created_at
             FROM checkpoints WHERE run_id = ?1 ORDER BY seq DESC LIMIT 1",
        )
        .bind(run_id.to_string())
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        let Some(row) = row else {
            return Ok(None);
        };

        let record = CheckpointRecord {
            run_id: parse_id(
                &row.try_get::<String, _>("run_id").map_err(db_error)?,
                "checkpoints.run_id",
            )?,
            seq: row.try_get("seq").map_err(db_error)?,
            state_ref: row.try_get("state_ref").map_err(db_error)?,
            schema_version: row.try_get("schema_version").map_err(db_error)?,
            checksum: row.try_get("checksum").map_err(db_error)?,
            created_at: row.try_get("created_at").map_err(db_error)?,
        };

        if record.schema_version != expected_schema_version {
            return Err(SilynError::Permanent(format!(
                "checkpoint schema version {} does not match runtime version {expected_schema_version}",
                record.schema_version
            )));
        }
        if record.checksum != CheckpointRecord::checksum_for(&record.state_ref) {
            return Err(SilynError::Permanent(
                "stored checkpoint checksum does not match its state reference".to_owned(),
            ));
        }

        Ok(Some(record))
    }
}
