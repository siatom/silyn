//! Application service layer.
//!
//! The CLI (W2) and the HTTP API (W7) are thin adapters over this crate. Business
//! rules live here so both frontends share identical behavior.

mod app;

pub use app::{
    App, DEFAULT_CONTEXT_VERSION, DEFAULT_PROFILE_VERSION, InitReport, RECENT_RUNS_LIMIT,
};

// Records are re-exported so frontends never depend on `silyn-storage` directly.
pub use silyn_storage::{AgentRecord, RunRecord, SessionRecord, TaskRecord};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-readable version line used by the CLI smoke test.
#[must_use]
pub fn version_line() -> String {
    format!("silyn {VERSION}")
}
