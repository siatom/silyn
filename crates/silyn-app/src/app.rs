//! Service layer shared by the CLI and the future HTTP API.
//!
//! [`App`] owns the storage handle and the resolved home directory. It exposes
//! the operations both frontends need and decides validation that is not a
//! database constraint. It deliberately contains no run loop: executing a task
//! is the runtime's job (W3).

use std::path::{Path, PathBuf};

use chrono::Utc;
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, RunId, SessionId, TaskId};
use silyn_core::state::TaskStatus;
use silyn_storage::error::parse_id;
use silyn_storage::{
    AgentRecord, AgentRepository, RunRecord, RunRepository, SessionRecord, SessionRepository,
    Storage, TaskRecord, TaskRepository,
};

/// Profile version stamped on new agents.
///
/// Versioned agent profiles (persona, policies) arrive with the growth wave;
/// until then every agent points at this placeholder version.
pub const DEFAULT_PROFILE_VERSION: &str = "profile-v1";

/// Context version stamped on new sessions. Replaced by the context builder (W3).
pub const DEFAULT_CONTEXT_VERSION: &str = "ctx-v1";

/// Upper bound for `runs list`. Runs grow without bound, so the CLI shows a
/// bounded, newest-first window instead of the whole table.
pub const RECENT_RUNS_LIMIT: i64 = 50;

/// Paths created or verified by `silyn init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    /// Directory holding the database, blobs and artifacts.
    pub home: PathBuf,
    /// Absolute-or-relative path of the SQLite database file.
    pub database: PathBuf,
}

/// Application service root.
#[derive(Debug)]
pub struct App {
    storage: Storage,
    home: PathBuf,
}

impl App {
    /// Opens the Silyn home directory and its database, applying migrations.
    ///
    /// The directory is created when missing, so the same call backs both
    /// `silyn init` and every other command.
    pub async fn open(home: impl Into<PathBuf>) -> Result<Self, SilynError> {
        let home = home.into();
        std::fs::create_dir_all(&home).map_err(|error| {
            SilynError::Storage(format!(
                "cannot create home directory {}: {error}",
                home.display()
            ))
        })?;

        let database = home.join("silyn.db");
        let storage = Storage::connect(&database).await?;
        Ok(Self { storage, home })
    }

    /// Directory holding the database and future artifacts.
    #[must_use]
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Path of the SQLite database file.
    #[must_use]
    pub fn database(&self) -> PathBuf {
        self.home.join("silyn.db")
    }

    /// Paths to report after initializing the home directory.
    #[must_use]
    pub fn init_report(&self) -> InitReport {
        InitReport {
            home: self.home.clone(),
            database: self.database(),
        }
    }

    /// Shared storage handle. Frontends must not reach past it into the pool.
    #[must_use]
    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Creates a persistent agent identity owned by `owner`.
    ///
    /// `owner` is a local label (for example a user name); it is not
    /// authenticated and must not be treated as an authorization decision.
    pub async fn create_agent(&self, owner: &str) -> Result<AgentRecord, SilynError> {
        let owner = owner.trim();
        if owner.is_empty() {
            return Err(SilynError::Permanent(
                "agent owner must not be empty".to_owned(),
            ));
        }

        let record = AgentRecord {
            id: AgentId::new(),
            owner_id: owner.to_owned(),
            profile_version: DEFAULT_PROFILE_VERSION.to_owned(),
            active_policy_version: None,
            created_at: Utc::now(),
        };
        AgentRepository::new(self.storage.pool())
            .insert(&record)
            .await?;
        Ok(record)
    }

    /// Lists every agent, newest first.
    ///
    /// The id-then-fetch shape keeps row decoding in the storage crate; the
    /// number of agents is small by construction (one per local owner).
    pub async fn list_agents(&self) -> Result<Vec<AgentRecord>, SilynError> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM agents ORDER BY created_at DESC, id DESC")
                .fetch_all(self.storage.pool())
                .await
                .map_err(silyn_storage::db_error)?;

        let repository = AgentRepository::new(self.storage.pool());
        let mut agents = Vec::with_capacity(ids.len());
        for raw in ids {
            let id: AgentId = parse_id(&raw, "agents.id")?;
            let record = repository.get(id).await?.ok_or_else(|| {
                SilynError::Permanent(format!("agent {id} disappeared while listing"))
            })?;
            agents.push(record);
        }
        Ok(agents)
    }

    /// Starts a conversation thread for an existing agent.
    pub async fn create_session(&self, agent_id: AgentId) -> Result<SessionRecord, SilynError> {
        if AgentRepository::new(self.storage.pool())
            .get(agent_id)
            .await?
            .is_none()
        {
            return Err(SilynError::Permanent(format!("agent {agent_id} not found")));
        }

        let record = SessionRecord {
            id: SessionId::new(),
            agent_id,
            context_version: DEFAULT_CONTEXT_VERSION.to_owned(),
            created_at: Utc::now(),
        };
        SessionRepository::new(self.storage.pool())
            .insert(&record)
            .await?;
        Ok(record)
    }

    /// Records a user objective in the `created` state. It is not scheduled here.
    pub async fn create_task(
        &self,
        session_id: SessionId,
        objective: &str,
    ) -> Result<TaskRecord, SilynError> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err(SilynError::Permanent(
                "task objective must not be empty".to_owned(),
            ));
        }

        if SessionRepository::new(self.storage.pool())
            .get(session_id)
            .await?
            .is_none()
        {
            return Err(SilynError::Permanent(format!(
                "session {session_id} not found"
            )));
        }

        let now = Utc::now();
        let record = TaskRecord {
            id: TaskId::new(),
            session_id,
            objective: objective.to_owned(),
            status: TaskStatus::Created,
            created_at: now,
            updated_at: now,
        };
        TaskRepository::new(self.storage.pool())
            .insert(&record)
            .await?;
        Ok(record)
    }

    /// Lists the most recent runs, newest first, capped at [`RECENT_RUNS_LIMIT`].
    pub async fn list_runs(&self) -> Result<Vec<RunRecord>, SilynError> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM runs ORDER BY created_at DESC, id DESC LIMIT ?1")
                .bind(RECENT_RUNS_LIMIT)
                .fetch_all(self.storage.pool())
                .await
                .map_err(silyn_storage::db_error)?;

        let repository = RunRepository::new(self.storage.pool());
        let mut runs = Vec::with_capacity(ids.len());
        for raw in ids {
            let id: RunId = parse_id(&raw, "runs.id")?;
            let record = repository.get(id).await?.ok_or_else(|| {
                SilynError::Permanent(format!("run {id} disappeared while listing"))
            })?;
            runs.push(record);
        }
        Ok(runs)
    }

    /// Fetches one run by id, or fails permanently when it does not exist.
    pub async fn get_run(&self, run_id: RunId) -> Result<RunRecord, SilynError> {
        RunRepository::new(self.storage.pool())
            .get(run_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("run {run_id} not found")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn app_in_tempdir() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = App::open(dir.path().join("home")).await.expect("open app");
        (dir, app)
    }

    #[tokio::test]
    async fn open_creates_home_and_database() {
        let (dir, app) = app_in_tempdir().await;
        let report = app.init_report();

        assert_eq!(report.home, dir.path().join("home"));
        assert_eq!(report.database, dir.path().join("home").join("silyn.db"));
        assert!(report.home.is_dir());
        assert!(report.database.is_file());
        assert_eq!(app.home(), report.home);
        assert_eq!(app.database(), report.database);
    }

    #[tokio::test]
    async fn open_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let first = App::open(home.clone()).await.expect("first open");
        drop(first);

        let second = App::open(home.clone()).await.expect("second open");
        let agents = second.list_agents().await.expect("list agents");
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn agent_session_task_round_trip() {
        let (_dir, app) = app_in_tempdir().await;

        let agent = app
            .create_agent(" local-user ")
            .await
            .expect("create agent");
        assert_eq!(agent.owner_id, "local-user");
        assert_eq!(agent.profile_version, DEFAULT_PROFILE_VERSION);
        assert_eq!(agent.active_policy_version, None);

        let agents = app.list_agents().await.expect("list agents");
        assert_eq!(agents, vec![agent.clone()]);

        let session = app.create_session(agent.id).await.expect("create session");
        assert_eq!(session.agent_id, agent.id);
        assert_eq!(session.context_version, DEFAULT_CONTEXT_VERSION);

        let task = app
            .create_task(session.id, " summarize the csv ")
            .await
            .expect("create task");
        assert_eq!(task.session_id, session.id);
        assert_eq!(task.objective, "summarize the csv");
        assert_eq!(task.status, TaskStatus::Created);
        assert_eq!(task.created_at, task.updated_at);
    }

    #[tokio::test]
    async fn empty_inputs_are_permanent_errors() {
        let (_dir, app) = app_in_tempdir().await;

        let error = app.create_agent("   ").await.expect_err("empty owner");
        assert!(matches!(error, SilynError::Permanent(_)));
        assert!(app.list_agents().await.expect("list agents").is_empty());

        let agent = app.create_agent("local-user").await.expect("create agent");
        let session = app.create_session(agent.id).await.expect("create session");
        let error = app
            .create_task(session.id, "\t")
            .await
            .expect_err("empty objective");
        assert!(matches!(error, SilynError::Permanent(_)));
    }

    #[tokio::test]
    async fn unknown_parents_are_permanent_errors() {
        let (_dir, app) = app_in_tempdir().await;

        let error = app
            .create_session(AgentId::new())
            .await
            .expect_err("missing agent");
        assert!(matches!(error, SilynError::Permanent(_)));

        let error = app
            .create_task(SessionId::new(), "do the thing")
            .await
            .expect_err("missing session");
        assert!(matches!(error, SilynError::Permanent(_)));
    }

    #[tokio::test]
    async fn runs_start_empty_and_missing_runs_are_permanent() {
        let (_dir, app) = app_in_tempdir().await;

        assert!(app.list_runs().await.expect("list runs").is_empty());

        let error = app.get_run(RunId::new()).await.expect_err("missing run");
        assert!(matches!(error, SilynError::Permanent(_)));
        assert!(error.to_string().contains("not found"));
    }
}
