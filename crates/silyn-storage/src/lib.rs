//! SQLite persistence layer.
//!
//! Schema is defined by append-only migrations in `migrations/`. Merged
//! migrations are immutable; changes always add a new file.
//!
//! This crate exposes one connection owner ([`Storage`]) plus repositories. It
//! does not make policy decisions: callers decide whether a write is allowed.

pub mod error;
pub mod models;
pub mod repo;
pub mod storage;

pub use error::db_error;
pub use models::{AgentRecord, RunRecord, SessionRecord, StepRecord, TaskRecord};
pub use repo::{AgentRepository, RunRepository, SessionRepository, StepRepository, TaskRepository};
pub use storage::Storage;

/// Directory holding SQL migrations, relative to the workspace root.
pub const MIGRATIONS_DIR: &str = "migrations";
