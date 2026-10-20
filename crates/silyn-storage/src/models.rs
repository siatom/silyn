//! Persisted records. These mirror the SQL schema and stay separate from the
//! protocol types in `silyn-core`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use silyn_core::ids::{AgentId, PolicyVersionId, RunId, SessionId, StepId, TaskId};
use silyn_core::state::{RunStatus, TaskStatus};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: AgentId,
    pub owner_id: String,
    pub profile_version: String,
    pub active_policy_version: Option<PolicyVersionId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub agent_id: AgentId,
    pub context_version: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: TaskId,
    pub session_id: SessionId,
    pub objective: String,
    pub status: TaskStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: RunId,
    pub task_id: TaskId,
    pub policy_version: PolicyVersionId,
    pub memory_snapshot_version: Option<String>,
    pub model_id: String,
    pub status: RunStatus,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    pub id: StepId,
    pub run_id: RunId,
    pub seq: i64,
    pub action_type: String,
    pub result_ref: Option<String>,
    pub created_at: DateTime<Utc>,
}
