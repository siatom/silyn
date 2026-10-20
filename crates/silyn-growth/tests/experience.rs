//! Experience extraction acceptance tests: provisional lessons, deduplication,
//! confirmation by independent evidence, retirement and agent isolation.
//!
//! Every test runs against a fresh in-memory database with the real migrations
//! applied. Agents, sessions, tasks and runs are created through the storage
//! repositories, so foreign keys and ownership joins are exercised for real.

use chrono::Utc;
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, PolicyVersionId, RunId, SessionId, TaskId};
use silyn_core::state::{RunStatus, TaskStatus};
use silyn_growth::{
    CheckResult, EvidenceStatus, ExperienceStatus, ExperienceStore, SUCCESS_LESSON,
};
use silyn_storage::{
    AgentRecord, AgentRepository, RunRecord, RunRepository, SessionRecord, SessionRepository,
    Storage, TaskRecord, TaskRepository,
};

async fn setup() -> (Storage, AgentId) {
    let storage = Storage::connect_in_memory()
        .await
        .expect("in-memory storage with migrations");
    let agent = insert_agent(&storage, "user-a").await;
    (storage, agent)
}

async fn insert_agent(storage: &Storage, owner: &str) -> AgentId {
    let record = AgentRecord {
        id: AgentId::new(),
        owner_id: owner.to_owned(),
        profile_version: "profile-v1".to_owned(),
        active_policy_version: None,
        created_at: Utc::now(),
    };
    AgentRepository::new(storage.pool())
        .insert(&record)
        .await
        .expect("insert agent");
    record.id
}

/// Creates a session, task and run owned by `agent`.
async fn create_run(storage: &Storage, agent: AgentId) -> RunId {
    let now = Utc::now();
    let session = SessionRecord {
        id: SessionId::new(),
        agent_id: agent,
        context_version: "ctx-v1".to_owned(),
        created_at: now,
    };
    SessionRepository::new(storage.pool())
        .insert(&session)
        .await
        .expect("insert session");

    let task = TaskRecord {
        id: TaskId::new(),
        session_id: session.id,
        objective: "count the distinct rows".to_owned(),
        status: TaskStatus::Created,
        created_at: now,
        updated_at: now,
    };
    TaskRepository::new(storage.pool())
        .insert(&task)
        .await
        .expect("insert task");

    let run = RunRecord {
        id: RunId::new(),
        task_id: task.id,
        policy_version: PolicyVersionId::new(),
        memory_snapshot_version: None,
        model_id: "deepseek-flash".to_owned(),
        status: RunStatus::Created,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: 0.0,
        started_at: None,
        finished_at: None,
        created_at: now,
    };
    RunRepository::new(storage.pool())
        .insert(&run)
        .await
        .expect("insert run");
    run.id
}

fn failed(code: &str) -> CheckResult {
    CheckResult::Failed {
        code: code.to_owned(),
        reason: format!("{code} was detected by an independent check"),
        evidence: vec![format!("artifact:{code}")],
    }
}

fn success(evidence: &str) -> CheckResult {
    CheckResult::Success {
        evidence: vec![evidence.to_owned()],
    }
}

fn unknown() -> CheckResult {
    CheckResult::Unknown {
        reason: "the report artifact was unavailable".to_owned(),
    }
}

async fn evidence_count(storage: &Storage, experience_id: &str, outcome: Option<&str>) -> i64 {
    match outcome {
        Some(outcome) => sqlx::query_scalar(
            "SELECT COUNT(*) FROM experience_evidence WHERE experience_id = ?1 AND outcome = ?2",
        )
        .bind(experience_id)
        .bind(outcome)
        .fetch_one(storage.pool())
        .await
        .expect("count evidence"),
        None => {
            sqlx::query_scalar("SELECT COUNT(*) FROM experience_evidence WHERE experience_id = ?1")
                .bind(experience_id)
                .fetch_one(storage.pool())
                .await
                .expect("count evidence")
        }
    }
}

async fn total_experiences(storage: &Storage) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM experiences")
        .fetch_one(storage.pool())
        .await
        .expect("count experiences")
}

#[tokio::test]
async fn failed_run_creates_provisional() {
    let (storage, agent) = setup().await;
    let run = create_run(&storage, agent).await;
    let store = ExperienceStore::new(storage.pool());

    let record = store
        .extract(agent, run, &failed("wrong_count"))
        .await
        .expect("extract")
        .expect("a failed run produces a lesson");

    assert_eq!(record.agent_id, agent);
    assert_eq!(record.lesson, "Recompute row counts from distinct rows.");
    assert_eq!(record.evidence_status, EvidenceStatus::Provisional);
    assert_eq!(record.status, ExperienceStatus::Active);
    assert!(
        (record.confidence - 0.3).abs() < f32::EPSILON,
        "a single failure is weak evidence, got {}",
        record.confidence
    );
    assert_eq!(record.source_run_ids, vec![run]);
    assert_eq!(
        evidence_count(&storage, &record.id.to_string(), Some("failed")).await,
        1
    );
    assert_eq!(
        evidence_count(&storage, &record.id.to_string(), None).await,
        1
    );
}

#[tokio::test]
async fn success_creates_positive_provisional() {
    let (storage, agent) = setup().await;
    let run = create_run(&storage, agent).await;
    let store = ExperienceStore::new(storage.pool());

    let record = store
        .extract(agent, run, &success("row_count:42"))
        .await
        .expect("extract")
        .expect("a verified run produces a lesson");

    assert_eq!(record.lesson, SUCCESS_LESSON);
    assert_eq!(record.evidence_status, EvidenceStatus::Provisional);
    assert!(
        (record.confidence - 0.4).abs() < f32::EPSILON,
        "one verified run is still provisional, got {}",
        record.confidence
    );
    assert_eq!(
        evidence_count(&storage, &record.id.to_string(), Some("success")).await,
        1
    );
}

#[tokio::test]
async fn unknown_creates_no_success() {
    let (storage, agent) = setup().await;
    let run = create_run(&storage, agent).await;
    let store = ExperienceStore::new(storage.pool());

    assert!(
        store
            .extract(agent, run, &unknown())
            .await
            .expect("extract")
            .is_none(),
        "unknown evidence must not produce a lesson"
    );
    assert!(store.list(agent).await.expect("list").is_empty());
    assert_eq!(total_experiences(&storage).await, 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM experience_evidence")
            .fetch_one(storage.pool())
            .await
            .expect("count evidence"),
        0
    );
}

#[tokio::test]
async fn deduplicates_lesson() {
    let (storage, agent) = setup().await;
    let first_run = create_run(&storage, agent).await;
    let second_run = create_run(&storage, agent).await;
    let store = ExperienceStore::new(storage.pool());

    let first = store
        .extract(agent, first_run, &failed("wrong_count"))
        .await
        .expect("extract")
        .expect("lesson");
    // The same run twice must not add a second evidence row.
    let repeated = store
        .extract(agent, first_run, &failed("wrong_count"))
        .await
        .expect("extract")
        .expect("lesson");
    assert_eq!(repeated.id, first.id);
    assert_eq!(repeated.source_run_ids, vec![first_run]);

    let second = store
        .extract(agent, second_run, &failed("wrong_count"))
        .await
        .expect("extract")
        .expect("lesson");

    assert_eq!(second.id, first.id, "the active lesson is reused");
    assert_eq!(
        second.source_run_ids.len(),
        2,
        "two distinct runs are recorded"
    );
    assert_eq!(
        evidence_count(&storage, &first.id.to_string(), None).await,
        2,
        "one evidence row per run"
    );
    let lessons = store.list(agent).await.expect("list");
    assert_eq!(lessons.len(), 1, "one active row per normalized lesson");
    assert_eq!(lessons[0].evidence_status, EvidenceStatus::Provisional);
}

#[tokio::test]
async fn confirm_requires_two_runs() {
    let (storage, agent) = setup().await;
    let store = ExperienceStore::new(storage.pool());

    // One run is never enough.
    let single_run = create_run(&storage, agent).await;
    let single = store
        .extract(agent, single_run, &failed("unit_mismatch"))
        .await
        .expect("extract")
        .expect("lesson");
    let error = store
        .confirm(single.id)
        .await
        .expect_err("one run cannot confirm");
    assert_eq!(error.class(), silyn_core::ErrorClass::Permanent);

    // Two failures confirm nothing: there is no success evidence.
    let second_run = create_run(&storage, agent).await;
    store
        .extract(agent, second_run, &failed("unit_mismatch"))
        .await
        .expect("extract")
        .expect("lesson");
    let error = store
        .confirm(single.id)
        .await
        .expect_err("failures alone cannot confirm");
    assert!(
        error.to_string().contains("successful evidence"),
        "the error names the missing success, got {error}"
    );

    // Two independent successes do confirm.
    let first_success = create_run(&storage, agent).await;
    let positive = store
        .extract(agent, first_success, &success("row_count:42"))
        .await
        .expect("extract")
        .expect("lesson");
    let error = store
        .confirm(positive.id)
        .await
        .expect_err("a single success run cannot confirm itself");
    assert_eq!(error.class(), silyn_core::ErrorClass::Permanent);

    let second_success = create_run(&storage, agent).await;
    store
        .extract(agent, second_success, &success("unit:usd"))
        .await
        .expect("extract")
        .expect("lesson");
    let confirmed = store.confirm(positive.id).await.expect("two runs confirm");

    assert_eq!(confirmed.id, positive.id);
    assert_eq!(confirmed.evidence_status, EvidenceStatus::Confirmed);
    assert!((confirmed.confidence - 0.7).abs() < f32::EPSILON);
    assert_eq!(confirmed.source_run_ids.len(), 2);
    assert!(confirmed.updated_at >= confirmed.created_at);
}

#[tokio::test]
async fn unknown_evidence_never_confirms() {
    let (storage, agent) = setup().await;
    let store = ExperienceStore::new(storage.pool());

    let success_run = create_run(&storage, agent).await;
    let record = store
        .extract(agent, success_run, &success("row_count:42"))
        .await
        .expect("extract")
        .expect("lesson");

    // Seed an `unknown` evidence row directly: extraction never writes one, but
    // the schema keeps them for audit and confirmation must ignore them.
    let unknown_run = create_run(&storage, agent).await;
    sqlx::query(
        "INSERT INTO experience_evidence (experience_id, run_id, outcome, evidence_ref, created_at)
         VALUES (?1, ?2, 'unknown', NULL, ?3)",
    )
    .bind(record.id.to_string())
    .bind(unknown_run.to_string())
    .bind(Utc::now())
    .execute(storage.pool())
    .await
    .expect("seed unknown evidence");

    let error = store
        .confirm(record.id)
        .await
        .expect_err("unknown evidence must not count as an independent run");
    assert!(
        error.to_string().contains("two distinct evidence runs"),
        "the error reports the counted runs only, got {error}"
    );

    // A second real success still confirms, ignoring the unknown row.
    let second_success = create_run(&storage, agent).await;
    store
        .extract(agent, second_success, &success("unit:usd"))
        .await
        .expect("extract")
        .expect("lesson");
    let confirmed = store
        .confirm(record.id)
        .await
        .expect("two successes confirm");
    assert_eq!(confirmed.evidence_status, EvidenceStatus::Confirmed);
    assert_eq!(
        evidence_count(&storage, &record.id.to_string(), Some("unknown")).await,
        1
    );
}

#[tokio::test]
async fn cross_agent_isolation() {
    let (storage, agent_a) = setup().await;
    let agent_b = insert_agent(&storage, "user-a").await;
    let run_a = create_run(&storage, agent_a).await;
    let run_b = create_run(&storage, agent_b).await;
    let store = ExperienceStore::new(storage.pool());

    let record = store
        .extract(agent_a, run_a, &failed("wrong_count"))
        .await
        .expect("extract")
        .expect("lesson");
    store
        .extract(agent_b, run_b, &failed("wrong_count"))
        .await
        .expect("extract")
        .expect("lesson");

    let lessons_a = store.list(agent_a).await.expect("list agent a");
    let lessons_b = store.list(agent_b).await.expect("list agent b");

    assert_eq!(lessons_a.len(), 1);
    assert_eq!(lessons_b.len(), 1);
    assert_eq!(lessons_a[0].agent_id, agent_a);
    assert_eq!(lessons_b[0].agent_id, agent_b);
    assert_ne!(
        lessons_a[0].id, lessons_b[0].id,
        "one agent's experience is never merged into another's"
    );
    assert_eq!(lessons_a[0].source_run_ids, vec![run_a]);
    assert_eq!(lessons_b[0].source_run_ids, vec![run_b]);
    assert_eq!(
        record.agent_id, agent_a,
        "the extracted record belongs to the requesting agent"
    );
    assert_eq!(total_experiences(&storage).await, 2);
}

#[tokio::test]
async fn retire_then_reextract_creates_new_active() {
    let (storage, agent) = setup().await;
    let first_run = create_run(&storage, agent).await;
    let second_run = create_run(&storage, agent).await;
    let store = ExperienceStore::new(storage.pool());

    let first = store
        .extract(agent, first_run, &failed("missing_values"))
        .await
        .expect("extract")
        .expect("lesson");
    store.retire(first.id).await.expect("retire");
    store.retire(first.id).await.expect("retire is idempotent");

    let retired = store.list(agent).await.expect("list");
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].status, ExperienceStatus::Retired);

    let second = store
        .extract(agent, second_run, &failed("missing_values"))
        .await
        .expect("extract")
        .expect("lesson");

    assert_ne!(second.id, first.id, "a retired lesson may be reborn");
    assert_eq!(second.status, ExperienceStatus::Active);
    assert_eq!(second.lesson, "Check for missing values before counting.");

    let lessons = store.list(agent).await.expect("list");
    assert_eq!(lessons.len(), 2);
    let active = lessons
        .iter()
        .filter(|lesson| lesson.status == ExperienceStatus::Active)
        .count();
    assert_eq!(
        active, 1,
        "the partial unique index keeps one active lesson"
    );

    let missing = store
        .retire(silyn_core::ids::ExperienceId::new())
        .await
        .expect_err("retiring an unknown experience is permanent");
    assert_eq!(missing.class(), silyn_core::ErrorClass::Permanent);
}

#[tokio::test]
async fn retired_experience_cannot_be_confirmed() {
    let (storage, agent) = setup().await;
    let first_run = create_run(&storage, agent).await;
    let second_run = create_run(&storage, agent).await;
    let store = ExperienceStore::new(storage.pool());

    let record = store
        .extract(agent, first_run, &success("row_count:42"))
        .await
        .expect("extract")
        .expect("lesson");
    store
        .extract(agent, second_run, &success("unit:usd"))
        .await
        .expect("extract")
        .expect("lesson");
    store.retire(record.id).await.expect("retire");

    let error = store
        .confirm(record.id)
        .await
        .expect_err("a withdrawn lesson cannot be confirmed");
    assert_eq!(error.class(), silyn_core::ErrorClass::Permanent);
}

#[tokio::test]
async fn extract_rejects_foreign_run() {
    let (storage, agent_a) = setup().await;
    let agent_b = insert_agent(&storage, "user-b").await;
    let run_b = create_run(&storage, agent_b).await;
    let store = ExperienceStore::new(storage.pool());

    let error = store
        .extract(agent_a, run_b, &failed("wrong_count"))
        .await
        .expect_err("a run of another agent cannot become evidence");
    assert_eq!(error.class(), silyn_core::ErrorClass::PolicyDenied);
    assert_eq!(total_experiences(&storage).await, 0);

    let missing = store
        .extract(agent_a, RunId::new(), &failed("wrong_count"))
        .await
        .expect_err("an unknown run cannot become evidence");
    assert!(matches!(missing, SilynError::Permanent(_)));
    assert_eq!(total_experiences(&storage).await, 0);
}
