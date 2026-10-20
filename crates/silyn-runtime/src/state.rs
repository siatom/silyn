//! Checkpointed run state, schema version 1.
//!
//! The whole state is serialized into `checkpoints.state_ref` as JSON. The
//! checksum and schema version are validated by
//! [`silyn_storage::CheckpointStore::latest`] before this type is parsed.

use serde::{Deserialize, Serialize};
use silyn_core::contracts::{Message, ToolCall};
use silyn_core::ids::StepId;

/// Schema version stored in `checkpoints.schema_version`.
pub const RUN_STATE_SCHEMA_VERSION: i64 = 1;

/// A tool action that was checkpointed but not yet resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingAction {
    pub step_id: StepId,
    pub step_seq: u64,
    pub call: ToolCall,
}

/// Everything the run loop needs to continue after a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    pub step_seq: u64,
    pub messages: Vec<Message>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub pending: Option<PendingAction>,
    pub final_answer: Option<String>,
    pub loop_fingerprints: Vec<String>,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            step_seq: 0,
            messages: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            pending: None,
            final_answer: None,
            loop_fingerprints: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use silyn_core::ids::RunId;

    use super::*;

    #[test]
    fn state_round_trips_with_a_pending_action() {
        let pending = PendingAction {
            step_id: StepId::new(),
            step_seq: 3,
            call: ToolCall {
                call_id: format!("{}:3", RunId::new()),
                tool: "fs.read".to_owned(),
                arguments: json!({ "path": "data.csv" }),
            },
        };
        let state = RunState {
            step_seq: 3,
            pending: Some(pending.clone()),
            ..RunState::default()
        };

        let json = serde_json::to_string(&state).expect("serialize");
        let back: RunState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, state);
        assert_eq!(back.pending, Some(pending));
    }

    #[test]
    fn schema_version_is_pinned() {
        assert_eq!(RUN_STATE_SCHEMA_VERSION, 1);
    }
}
