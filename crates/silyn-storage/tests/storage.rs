use chrono::{Duration, Utc};
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, PolicyVersionId, RunId, SessionId, StepId, TaskId};
use silyn_core::state::{RunStatus, TaskStatus};
use silyn_storage::{
    AgentRecord, AgentRepository, RunRecord, RunRepository, SessionRecord, SessionRepository,
    StepRecord, StepRepository, Storage, TaskRecord, TaskRepository,
};

async fn setup() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::connect(dir.path().join("silyn.db"))
        .await
        .expect("connect and migrate");
    (dir, storage)
}

fn agent_record() -> AgentRecord {
    AgentRecord {
        id: AgentId::new(),
        owner_id: "local-user".to_owned(),
        profile_version: "profile-v1".to_owned(),
        active_policy_version: Some(PolicyVersionId::new()),
        created_at: Utc::now(),
    }
}

fn session_record(agent_id: AgentId) -> SessionRecord {
    SessionRecord {
        id: SessionId::new(),
        agent_id,
        context_version: "ctx-v1".to_owned(),
        created_at: Utc::now(),
    }
}

fn task_record(session_id: SessionId) -> TaskRecord {
    let now = Utc::now();
    TaskRecord {
        id: TaskId::new(),
        session_id,
        objective: "summarize the csv".to_owned(),
        status: TaskStatus::Created,
        created_at: now,
        updated_at: now,
    }
}

fn run_record(task_id: TaskId) -> RunRecord {
    RunRecord {
        id: RunId::new(),
        task_id,
        policy_version: PolicyVersionId::new(),
        memory_snapshot_version: Some("mem-v1".to_owned()),
        model_id: "deepseek-flash".to_owned(),
        status: RunStatus::Created,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        started_at: None,
        finished_at: None,
        created_at: Utc::now(),
    }
}

fn step_record(run_id: RunId, seq: i64) -> StepRecord {
    StepRecord {
        id: StepId::new(),
        run_id,
        seq,
        action_type: "model.complete".to_owned(),
        result_ref: Some(format!("blob:step-{seq}")),
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn migrations_apply_once() {
    let (_dir, storage) = setup().await;

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(storage.pool())
        .await
        .expect("count migrations");
    let migration_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    let expected = std::fs::read_dir(migration_dir)
        .expect("read migrations directory")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("sql"))
        .count() as i64;
    assert_eq!(
        count, expected,
        "every migration file must be applied exactly once"
    );

    storage.migrate().await.expect("migrate is idempotent");
    let count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(storage.pool())
        .await
        .expect("count migrations");
    assert_eq!(count_after, count);
}

#[tokio::test]
async fn fts5_is_available() {
    let (_dir, storage) = setup().await;
    assert!(
        storage.fts5_available().await.expect("probe"),
        "bundled SQLite must expose FTS5 for the memory engine"
    );
}

#[tokio::test]
async fn foreign_keys_are_enforced() {
    let (_dir, storage) = setup().await;
    let orphan = session_record(AgentId::new());

    let error = SessionRepository::new(storage.pool())
        .insert(&orphan)
        .await
        .expect_err("orphan session must be rejected");

    assert!(
        matches!(error, SilynError::Permanent(_)),
        "expected a permanent integrity error, got {error:?}"
    );
}

#[tokio::test]
async fn records_round_trip() {
    let (_dir, storage) = setup().await;
    let agents = AgentRepository::new(storage.pool());
    let sessions = SessionRepository::new(storage.pool());
    let tasks = TaskRepository::new(storage.pool());
    let runs = RunRepository::new(storage.pool());
    let steps = StepRepository::new(storage.pool());

    let agent = agent_record();
    agents.insert(&agent).await.expect("insert agent");
    assert_eq!(
        agents.get(agent.id).await.expect("get agent"),
        Some(agent.clone())
    );

    let session = session_record(agent.id);
    sessions.insert(&session).await.expect("insert session");
    assert_eq!(
        sessions.get(session.id).await.expect("get session"),
        Some(session.clone())
    );

    let mut task = task_record(session.id);
    tasks.insert(&task).await.expect("insert task");
    assert_eq!(
        tasks.get(task.id).await.expect("get task"),
        Some(task.clone())
    );

    let updated_at = task.updated_at + Duration::seconds(1);
    tasks
        .update_status(task.id, TaskStatus::Ready, updated_at)
        .await
        .expect("update status");
    task.status = TaskStatus::Ready;
    task.updated_at = updated_at;
    assert_eq!(
        tasks.get(task.id).await.expect("get task"),
        Some(task.clone())
    );

    let run = run_record(task.id);
    runs.insert(&run).await.expect("insert run");
    assert_eq!(runs.get(run.id).await.expect("get run"), Some(run.clone()));

    let first = step_record(run.id, 1);
    let second = step_record(run.id, 2);
    steps.insert(&second).await.expect("insert step 2");
    steps.insert(&first).await.expect("insert step 1");

    assert_eq!(
        steps.get(first.id).await.expect("get step"),
        Some(first.clone())
    );
    assert_eq!(
        steps.list_by_run(run.id).await.expect("list steps"),
        vec![first, second]
    );
}

#[tokio::test]
async fn duplicate_ids_are_rejected() {
    let (_dir, storage) = setup().await;
    let agents = AgentRepository::new(storage.pool());
    let agent = agent_record();

    agents.insert(&agent).await.expect("first insert");
    let error = agents
        .insert(&agent)
        .await
        .expect_err("duplicate id must be rejected");

    assert!(matches!(error, SilynError::Permanent(_)));
}

#[tokio::test]
async fn transaction_rollback_leaves_no_rows() {
    let (_dir, storage) = setup().await;
    let agent = agent_record();
    AgentRepository::new(storage.pool())
        .insert(&agent)
        .await
        .expect("insert agent");
    let session = session_record(agent.id);
    SessionRepository::new(storage.pool())
        .insert(&session)
        .await
        .expect("insert session");

    let mut transaction = storage.pool().begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO tasks (id, session_id, objective, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(TaskId::new().to_string())
    .bind(session.id.to_string())
    .bind("rolled back")
    .bind("created")
    .bind(Utc::now())
    .bind(Utc::now())
    .execute(&mut *transaction)
    .await
    .expect("insert inside transaction");
    transaction.rollback().await.expect("rollback");

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
        .fetch_one(storage.pool())
        .await
        .expect("count tasks");
    assert_eq!(count, 0);
}
