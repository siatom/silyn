//! SQLite persistence layer (W1).
//!
//! Schema is defined by append-only migrations in `migrations/`. Merged
//! migrations are immutable; changes always add a new file.

/// Directory holding SQL migrations, relative to the workspace root.
pub const MIGRATIONS_DIR: &str = "migrations";

/// Verifies that the SQLite build exposes FTS5. Implemented in W1.
pub fn fts5_available() -> bool {
    true
}
