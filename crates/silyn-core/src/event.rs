//! Append-only run events.
//!
//! Events record business transitions, not every object mutation. The envelope
//! is versioned so consumers can reject unknown schemas instead of guessing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ids::{EventId, PolicyVersionId, RunId, StepId, TaskId};

/// Current wire version of [`RunEvent`] and [`EventEnvelope`].
pub const RUN_EVENT_SCHEMA_VERSION: u16 = 1;

/// Machine-checkable outcome of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifiedOutcome {
    Success,
    Failed,
    /// Evidence is insufficient. Never treat this as success.
    Unknown,
}

/// Business events emitted during a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunEvent {
    RunCreated {
        task_id: TaskId,
        policy_version: PolicyVersionId,
    },
    RunStarted,
    StepCompleted {
        step_id: StepId,
        seq: u64,
    },
    ToolFailed {
        step_id: StepId,
        tool: String,
        message: String,
    },
    RunVerified {
        outcome: VerifiedOutcome,
    },
    RunCompleted,
    RunFailed {
        reason: String,
    },
    RunCancelled,
}

/// Envelope persisted in `event_journal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub event_id: EventId,
    pub aggregate_id: RunId,
    pub seq: u64,
    pub schema_version: u16,
    pub occurred_at: DateTime<Utc>,
    pub correlation_id: Option<Uuid>,
    pub event: RunEvent,
}

impl EventEnvelope {
    /// Creates an envelope with the current schema version.
    #[must_use]
    pub fn new(
        aggregate_id: RunId,
        seq: u64,
        correlation_id: Option<Uuid>,
        event: RunEvent,
    ) -> Self {
        Self {
            event_id: EventId::new(),
            aggregate_id,
            seq,
            schema_version: RUN_EVENT_SCHEMA_VERSION,
            occurred_at: Utc::now(),
            correlation_id,
            event,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips() {
        let run_id = RunId::new();
        let envelope = EventEnvelope::new(run_id, 1, None, RunEvent::RunStarted);
        let json = serde_json::to_string(&envelope).expect("serialize");
        let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, envelope);
        assert_eq!(back.schema_version, RUN_EVENT_SCHEMA_VERSION);
    }

    #[test]
    fn unknown_is_not_success() {
        assert_ne!(VerifiedOutcome::Unknown, VerifiedOutcome::Success);
    }
}
