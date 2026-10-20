//! SQLite connection ownership and migration entry point.

use std::path::Path;
use std::time::Duration;

use silyn_core::SilynError;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::error::db_error;

/// Embedded migrations. The path is relative to this crate and resolves to the
/// workspace `migrations/` directory at compile time.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

/// Owns the SQLite pool and guarantees the schema is migrated before use.
#[derive(Debug, Clone)]
pub struct Storage {
    pool: SqlitePool,
}

impl Storage {
    /// Opens (or creates) a database on a local disk and applies migrations.
    ///
    /// Network shares are rejected by the caller in the CLI; this layer only
    /// enforces SQLite settings.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, SilynError> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(db_error)?;

        Self::finish(pool).await
    }

    /// Opens a single-connection in-memory database. Intended for tests and
    /// short-lived evaluation runs.
    pub async fn connect_in_memory() -> Result<Self, SilynError> {
        let options = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .map_err(db_error)?;

        Self::finish(pool).await
    }

    async fn finish(pool: SqlitePool) -> Result<Self, SilynError> {
        MIGRATOR
            .run(&pool)
            .await
            .map_err(|error| SilynError::Storage(format!("migration failed: {error}")))?;
        let storage = Self { pool };
        if !storage.fts5_available().await? {
            return Err(SilynError::Permanent(
                "SQLite was built without FTS5; memory retrieval cannot work".to_owned(),
            ));
        }
        Ok(storage)
    }

    /// Shared pool for repositories and transactional code.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Returns true when the SQLite build exposes FTS5.
    pub async fn fts5_available(&self) -> Result<bool, SilynError> {
        let enabled: i64 = sqlx::query_scalar("SELECT sqlite_compileoption_used('ENABLE_FTS5')")
            .fetch_one(&self.pool)
            .await
            .map_err(db_error)?;
        Ok(enabled == 1)
    }

    /// Applies any pending migrations. Safe to call repeatedly.
    pub async fn migrate(&self) -> Result<(), SilynError> {
        MIGRATOR
            .run(&self.pool)
            .await
            .map_err(|error| SilynError::Storage(format!("migration failed: {error}")))
    }

    /// Closes the pool, waiting for in-flight queries.
    pub async fn close(self) {
        self.pool.close().await;
    }
}
