//! Repositories for the core schema.
//!
//! Repositories are thin: they map rows to records and let SQLx enforce
//! constraints. State-machine legality and authorization are decided by callers.

use chrono::{DateTime, Utc};
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, RunId, SessionId, StepId, TaskId};
use silyn_core::state::{RunStatus, TaskStatus};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::codec::{decode_enum, encode_enum};
use crate::error::{db_error, parse_id};
use crate::models::{AgentRecord, RunRecord, SessionRecord, StepRecord, TaskRecord};

fn text(row: &SqliteRow, column: &str) -> Result<String, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn optional_text(row: &SqliteRow, column: &str) -> Result<Option<String>, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn integer(row: &SqliteRow, column: &str) -> Result<i64, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn real(row: &SqliteRow, column: &str) -> Result<f64, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn timestamp(row: &SqliteRow, column: &str) -> Result<DateTime<Utc>, SilynError> {
    row.try_get(column).map_err(db_error)
}

fn optional_timestamp(row: &SqliteRow, column: &str) -> Result<Option<DateTime<Utc>>, SilynError> {
    row.try_get(column).map_err(db_error)
}

/// Agents: persistent identities.
pub struct AgentRepository<'a> {
    pool: &'a SqlitePool,
}

impl<'a> AgentRepository<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, record: &AgentRecord) -> Result<(), SilynError> {
        sqlx::query(
            "INSERT INTO agents (id, owner_id, profile_version, active_policy_version, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(record.id.to_string())
        .bind(&record.owner_id)
        .bind(&record.profile_version)
        .bind(record.active_policy_version.map(|id| id.to_string()))
        .bind(record.created_at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    pub async fn get(&self, id: AgentId) -> Result<Option<AgentRecord>, SilynError> {
        let row = sqlx::query(
            "SELECT id, owner_id, profile_version, active_policy_version, created_at
             FROM agents WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        row.map(|row| {
            Ok(AgentRecord {
                id: parse_id(&text(&row, "id")?, "agents.id")?,
                owner_id: text(&row, "owner_id")?,
                profile_version: text(&row, "profile_version")?,
                active_policy_version: optional_text(&row, "active_policy_version")?
                    .map(|value| parse_id(&value, "agents.active_policy_version"))
                    .transpose()?,
                created_at: timestamp(&row, "created_at")?,
            })
        })
        .transpose()
    }
}

/// Sessions: conversation threads owned by an agent.
pub struct SessionRepository<'a> {
    pool: &'a SqlitePool,
}

impl<'a> SessionRepository<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, record: &SessionRecord) -> Result<(), SilynError> {
        sqlx::query(
            "INSERT INTO sessions (id, agent_id, context_version, created_at)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(record.id.to_string())
        .bind(record.agent_id.to_string())
        .bind(&record.context_version)
        .bind(record.created_at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    pub async fn get(&self, id: SessionId) -> Result<Option<SessionRecord>, SilynError> {
        let row = sqlx::query(
            "SELECT id, agent_id, context_version, created_at FROM sessions WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        row.map(|row| {
            Ok(SessionRecord {
                id: parse_id(&text(&row, "id")?, "sessions.id")?,
                agent_id: parse_id(&text(&row, "agent_id")?, "sessions.agent_id")?,
                context_version: text(&row, "context_version")?,
                created_at: timestamp(&row, "created_at")?,
            })
        })
        .transpose()
    }
}

/// Tasks: user objectives with constraints.
pub struct TaskRepository<'a> {
    pool: &'a SqlitePool,
}

impl<'a> TaskRepository<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, record: &TaskRecord) -> Result<(), SilynError> {
        sqlx::query(
            "INSERT INTO tasks (id, session_id, objective, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(record.id.to_string())
        .bind(record.session_id.to_string())
        .bind(&record.objective)
        .bind(encode_enum(&record.status)?)
        .bind(record.created_at)
        .bind(record.updated_at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    pub async fn get(&self, id: TaskId) -> Result<Option<TaskRecord>, SilynError> {
        let row = sqlx::query(
            "SELECT id, session_id, objective, status, created_at, updated_at
             FROM tasks WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        row.map(|row| {
            Ok(TaskRecord {
                id: parse_id(&text(&row, "id")?, "tasks.id")?,
                session_id: parse_id(&text(&row, "session_id")?, "tasks.session_id")?,
                objective: text(&row, "objective")?,
                status: decode_enum::<TaskStatus>(&text(&row, "status")?)?,
                created_at: timestamp(&row, "created_at")?,
                updated_at: timestamp(&row, "updated_at")?,
            })
        })
        .transpose()
    }

    /// Updates status without an event. The runtime must use the journal path
    /// instead; this method exists for migrations and tests.
    pub async fn update_status(
        &self,
        id: TaskId,
        status: TaskStatus,
        updated_at: DateTime<Utc>,
    ) -> Result<(), SilynError> {
        let result = sqlx::query("UPDATE tasks SET status = ?2, updated_at = ?3 WHERE id = ?1")
            .bind(id.to_string())
            .bind(encode_enum(&status)?)
            .bind(updated_at)
            .execute(self.pool)
            .await
            .map_err(db_error)?;

        if result.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!("task {id} not found")));
        }
        Ok(())
    }
}

/// Runs: one execution attempt of a task.
pub struct RunRepository<'a> {
    pool: &'a SqlitePool,
}

impl<'a> RunRepository<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, record: &RunRecord) -> Result<(), SilynError> {
        sqlx::query(
            "INSERT INTO runs (
                id, task_id, policy_version, memory_snapshot_version, model_id, status,
                input_tokens, output_tokens, cost_usd, started_at, finished_at, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )
        .bind(record.id.to_string())
        .bind(record.task_id.to_string())
        .bind(record.policy_version.to_string())
        .bind(&record.memory_snapshot_version)
        .bind(&record.model_id)
        .bind(encode_enum(&record.status)?)
        .bind(record.input_tokens)
        .bind(record.output_tokens)
        .bind(record.cost_usd)
        .bind(record.started_at)
        .bind(record.finished_at)
        .bind(record.created_at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    /// Replaces the run's usage totals. The runtime calls this after every model
    /// response so the row matches the checkpointed counters.
    pub async fn update_usage(
        &self,
        id: RunId,
        input_tokens: i64,
        output_tokens: i64,
        cost_usd: f64,
    ) -> Result<(), SilynError> {
        let result = sqlx::query(
            "UPDATE runs SET input_tokens = ?2, output_tokens = ?3, cost_usd = ?4 WHERE id = ?1",
        )
        .bind(id.to_string())
        .bind(input_tokens)
        .bind(output_tokens)
        .bind(cost_usd)
        .execute(self.pool)
        .await
        .map_err(db_error)?;

        if result.rows_affected() == 0 {
            return Err(SilynError::Permanent(format!("run {id} not found")));
        }
        Ok(())
    }

    pub async fn get(&self, id: RunId) -> Result<Option<RunRecord>, SilynError> {
        let row = sqlx::query(
            "SELECT id, task_id, policy_version, memory_snapshot_version, model_id, status,
                    input_tokens, output_tokens, cost_usd, started_at, finished_at, created_at
             FROM runs WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        row.map(|row| {
            Ok(RunRecord {
                id: parse_id(&text(&row, "id")?, "runs.id")?,
                task_id: parse_id(&text(&row, "task_id")?, "runs.task_id")?,
                policy_version: parse_id(&text(&row, "policy_version")?, "runs.policy_version")?,
                memory_snapshot_version: optional_text(&row, "memory_snapshot_version")?,
                model_id: text(&row, "model_id")?,
                status: decode_enum::<RunStatus>(&text(&row, "status")?)?,
                input_tokens: integer(&row, "input_tokens")?,
                output_tokens: integer(&row, "output_tokens")?,
                cost_usd: real(&row, "cost_usd")?,
                started_at: optional_timestamp(&row, "started_at")?,
                finished_at: optional_timestamp(&row, "finished_at")?,
                created_at: timestamp(&row, "created_at")?,
            })
        })
        .transpose()
    }
}

/// Run steps ordered by `seq`.
pub struct StepRepository<'a> {
    pool: &'a SqlitePool,
}

impl<'a> StepRepository<'a> {
    #[must_use]
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn insert(&self, record: &StepRecord) -> Result<(), SilynError> {
        sqlx::query(
            "INSERT INTO run_steps (id, run_id, seq, action_type, result_ref, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(record.id.to_string())
        .bind(record.run_id.to_string())
        .bind(record.seq)
        .bind(&record.action_type)
        .bind(&record.result_ref)
        .bind(record.created_at)
        .execute(self.pool)
        .await
        .map_err(db_error)?;
        Ok(())
    }

    pub async fn get(&self, id: StepId) -> Result<Option<StepRecord>, SilynError> {
        let row = sqlx::query(
            "SELECT id, run_id, seq, action_type, result_ref, created_at
             FROM run_steps WHERE id = ?1",
        )
        .bind(id.to_string())
        .fetch_optional(self.pool)
        .await
        .map_err(db_error)?;

        row.map(|row| Self::from_row(&row)).transpose()
    }

    pub async fn list_by_run(&self, run_id: RunId) -> Result<Vec<StepRecord>, SilynError> {
        let rows = sqlx::query(
            "SELECT id, run_id, seq, action_type, result_ref, created_at
             FROM run_steps WHERE run_id = ?1 ORDER BY seq ASC",
        )
        .bind(run_id.to_string())
        .fetch_all(self.pool)
        .await
        .map_err(db_error)?;

        rows.iter().map(Self::from_row).collect()
    }

    fn from_row(row: &SqliteRow) -> Result<StepRecord, SilynError> {
        Ok(StepRecord {
            id: parse_id(&text(row, "id")?, "run_steps.id")?,
            run_id: parse_id(&text(row, "run_id")?, "run_steps.run_id")?,
            seq: integer(row, "seq")?,
            action_type: text(row, "action_type")?,
            result_ref: optional_text(row, "result_ref")?,
            created_at: timestamp(row, "created_at")?,
        })
    }
}
