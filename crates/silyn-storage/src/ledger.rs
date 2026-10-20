//! Tool-call idempotency ledger.
//!
//! `(run_id, step_id, tool_call_id)` is unique. A resumed run consults the ledger
//! before repeating a side effect; an unknown outcome is recorded as
//! `Indeterminate` and must be resolved explicitly.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use silyn_core::SilynError;
use silyn_core::ids::{RunId, StepId};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::codec::{decode_enum, encode_enum};
use crate::error::{db_error, parse_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallState {
    Started,
    Succeeded,
    Failed,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub run_id: RunId,
    pub step_id: StepId,
    pub tool_call_id: String,
    pub tool_name: String,
    pub state: ToolCallState,
    pub result_ref: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Ledger reader and writer.
pub struct ToolLedger<'a> {
    pool: &'a SqlitePool,
}

impl<'a> ToolLedger<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// Records the start of a tool call. A duplicate key is rejected.
    pub async fn begin(&self, record: &ToolCallRecord) -> Result<(), SilynError> {
        if record.state != ToolCallState::Started {
            return Err(SilynError::Permanent(
                "begin requires the Started state".to_owned(),
            ));
        }

        sqlx::query(
            "INSERT INTO tool_call_ledger
                (run_id, step_id, tool_call_id, tool_name, state, result_ref, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(record.run_id.to_string())
        .bind(record.step_id.to_string())
        .bind(&record.tool_call_id)
        .bind(&record.tool_name)
        .bind(encode_enum(&record.state)?)
        .bind(&record.result_ref)
        .bind(record.created_at)
        .execute(self.pool)
        .await
        .map_err(|error| {
            if let sqlx::Error::Database(database) = &error
                && database.is_unique_violation()
            {
                return SilynError::Permanent(format!(
                    "duplicate tool call {}:{}:{}",
                    record.run_id, record.step_id, record.tool_call_id
                ));
            }
            db_error(error)
        })?;
        Ok(())
    }

    /// Records a terminal or indeterminate state.
    pub async fn complete(
        &self,
        run_id: RunId,
        step_id: StepId,
        tool_call_id: &str,
        state: ToolCallState,
        result_ref: Option<&str>,
    ) -> Result<(), SilynError> {
        if state == ToolCallState::Started {
            return Err(SilynError::Permanent(
                "complete requires a terminal or indeterminate state".to_owned(),
            ));
        }

        let result = sqlx::query(
            "UPDATE tool_call_ledger SET state = ?4, result_ref = ?5
             WHERE run_id = ?1 AND step_id = ?2 AND tool_call_id = ?3",
        )
        .bind(run_id.to_string())
        .bind(step_id.to_string())
        .bind(tool_call_id)
        .bind(encode_enum(&state)?)
        .bind(result_ref)
        .execute(self.pool)
        .await
        .map_err(db_error)?;

        if result.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!(
                "tool call {run_id}:{step_id}:{tool_call_id} not found"
            )));
        }
        Ok(())
    }

    /// Marks a call whose outcome is unknown; resolved explicitly later.
    pub async fn mark_indeterminate(
        &self,
        run_id: RunId,
        step_id: StepId,
        tool_call_id: &str,
    ) -> Result<(), SilynError> {
        self.complete(
            run_id,
            step_id,
            tool_call_id,
            ToolCallState::Indeterminate,
            None,
        )
        .await
    }

    pub async fn get(
        &self,
        run_id: RunId,
        step_id: StepId,
        tool_call_id: &str,
    ) -> Result<Option<ToolCallRecord>, SilynError> {
        let row = sqlx::query(
            "SELECT run_id, step_id, tool_call_id, tool_name, state, result_ref, created_at
             FROM tool_call_ledger
             WHERE run_id = ?1 AND step_id = ?2 AND tool_call_id = ?3",
        )
        .bind(run_id.to_string())
        .bind(step_id.to_string())
        .bind(tool_call_id)
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        row.as_ref().map(Self::from_row).transpose()
    }

    /// Returns every call recorded for a run, oldest first.
    ///
    /// The ordering is `created_at`, then `step_id`, then `tool_call_id`, so a
    /// caller can reconstruct the execution order without knowing the ids.
    pub async fn list_by_run(&self, run_id: RunId) -> Result<Vec<ToolCallRecord>, SilynError> {
        let rows = sqlx::query(
            "SELECT run_id, step_id, tool_call_id, tool_name, state, result_ref, created_at
             FROM tool_call_ledger
             WHERE run_id = ?1
             ORDER BY created_at ASC, step_id ASC, tool_call_id ASC",
        )
        .bind(run_id.to_string())
        .fetch_all(self.pool)
        .await
        .map_err(db_error)?;

        rows.iter().map(Self::from_row).collect()
    }

    fn from_row(row: &SqliteRow) -> Result<ToolCallRecord, SilynError> {
        Ok(ToolCallRecord {
            run_id: parse_id(
                &row.try_get::<String, _>("run_id").map_err(db_error)?,
                "tool_call_ledger.run_id",
            )?,
            step_id: parse_id(
                &row.try_get::<String, _>("step_id").map_err(db_error)?,
                "tool_call_ledger.step_id",
            )?,
            tool_call_id: row.try_get("tool_call_id").map_err(db_error)?,
            tool_name: row.try_get("tool_name").map_err(db_error)?,
            state: decode_enum(&row.try_get::<String, _>("state").map_err(db_error)?)?,
            result_ref: row.try_get("result_ref").map_err(db_error)?,
            created_at: row.try_get("created_at").map_err(db_error)?,
        })
    }
}
