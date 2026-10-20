//! Application service layer.
//!
//! The CLI (W2) and the HTTP API (W7) are thin adapters over this crate. Business
//! rules live here so both frontends share identical behavior.

mod app;
mod observability;
mod runtime_service;

pub use app::{
    App, DEFAULT_CONTEXT_VERSION, DEFAULT_PROFILE_VERSION, InitReport, RECENT_RUNS_LIMIT,
};
pub use observability::{CancelRequest, RUN_SIGNAL_DIR};
pub use runtime_service::{AnswerVerifier, OFFLINE_ANSWER, RunRequest, WORKSPACE_DIR};

// Runtime contracts are re-exported so frontends never depend on `silyn-runtime`
// directly; the CLI only sees the app service surface.
pub use silyn_runtime::{BudgetSpec, RunOptions, RunOutcome};

// Records are re-exported so frontends never depend on `silyn-storage` directly.
pub use silyn_storage::{
    AgentRecord, RunRecord, SessionRecord, TaskRecord, ToolCallRecord, ToolCallState,
};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-readable version line used by the CLI smoke test.
#[must_use]
pub fn version_line() -> String {
    format!("silyn {VERSION}")
}
