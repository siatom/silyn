use chrono::{Duration, Utc};
use silyn_core::SilynError;
use silyn_core::event::{EventEnvelope, RunEvent};
use silyn_core::ids::{AgentId, RunId, SessionId, StepId, TaskId};
use silyn_core::state::TaskStatus;
use silyn_storage::{
    AgentRecord, AgentRepository, CheckpointRecord, CheckpointStore, Journal, Outbox,
    SessionRecord, SessionRepository, Storage, TaskRecord, TaskRepository, ToolCallRecord,
    ToolCallState, ToolLedger,
};

async fn storage() -> Storage {
    Storage::connect_in_memory()
        .await
        .expect("in-memory storage")
}

async fn seed_task(storage: &Storage) -> TaskRecord {
    let agent = AgentRecord {
        id: AgentId::new(),
        owner_id: "local-user".to_owned(),
        profile_version: "profile-v1".to_owned(),
        active_policy_version: None,
        created_at: Utc::now(),
    };
    AgentRepository::new(storage.pool())
        .insert(&agent)
        .await
        .expect("insert agent");

    let session = SessionRecord {
        id: SessionId::new(),
        agent_id: agent.id,
        context_version: "ctx-v1".to_owned(),
        created_at: Utc::now(),
    };
    SessionRepository::new(storage.pool())
        .insert(&session)
        .await
        .expect("insert session");

    let now = Utc::now();
    let task = TaskRecord {
        id: TaskId::new(),
        session_id: session.id,
        objective: "test objective".to_owned(),
        status: TaskStatus::Created,
        created_at: now,
        updated_at: now,
    };
    TaskRepository::new(storage.pool())
        .insert(&task)
        .await
        .expect("insert task");
    task
}

#[tokio::test]
async fn event_round_trip() {
    let storage = storage().await;
    let journal = Journal::new(storage.pool());
    let run_id = RunId::new();
    let envelope = EventEnvelope::new(run_id, 1, None, RunEvent::RunStarted);

    journal.append(&envelope).await.expect("append");

    assert_eq!(
        journal.list_for_aggregate(run_id).await.expect("list"),
        vec![envelope]
    );
    assert_eq!(journal.latest_seq(run_id).await.expect("latest"), 1);
    assert_eq!(journal.count(run_id).await.expect("count"), 1);
}

#[tokio::test]
async fn event_seq_conflict_is_transient() {
    let storage = storage().await;
    let journal = Journal::new(storage.pool());
    let envelope = EventEnvelope::new(RunId::new(), 1, None, RunEvent::RunStarted);

    journal.append(&envelope).await.expect("first append");
    let error = journal
        .append(&envelope)
        .await
        .expect_err("duplicate seq must fail");

    assert!(
        matches!(error, SilynError::Transient(_)),
        "conflict must be retryable, got {error:?}"
    );
    assert_eq!(
        journal.count(envelope.aggregate_id).await.expect("count"),
        1
    );
}

#[tokio::test]
async fn state_and_event_share_transaction() {
    let storage = storage().await;
    let task = seed_task(&storage).await;
    let journal = Journal::new(storage.pool());
    let run_id = RunId::new();
    let taken = EventEnvelope::new(run_id, 1, None, RunEvent::RunStarted);
    journal.append(&taken).await.expect("pre-append");

    let error = journal
        .append_task_transition(task.id, TaskStatus::Ready, Utc::now(), &taken)
        .await
        .expect_err("duplicate event must abort the transaction");
    assert!(matches!(error, SilynError::Transient(_)));

    let unchanged = TaskRepository::new(storage.pool())
        .get(task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(unchanged.status, TaskStatus::Created);
    assert_eq!(journal.count(run_id).await.expect("count"), 1);

    let next = EventEnvelope::new(run_id, 2, None, RunEvent::RunCompleted);
    journal
        .append_task_transition(task.id, TaskStatus::Ready, Utc::now(), &next)
        .await
        .expect("commit");

    let updated = TaskRepository::new(storage.pool())
        .get(task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(updated.status, TaskStatus::Ready);
    assert_eq!(journal.count(run_id).await.expect("count"), 2);
}

#[tokio::test]
async fn outbox_is_idempotent() {
    let storage = storage().await;
    let journal = Journal::new(storage.pool());
    let outbox = Outbox::new(storage.pool());
    let envelope = EventEnvelope::new(RunId::new(), 1, None, RunEvent::RunStarted);
    journal.append(&envelope).await.expect("append");

    let due = Utc::now() - Duration::seconds(1);
    assert!(
        outbox
            .enqueue(envelope.event_id, "indexer", due)
            .await
            .expect("first enqueue")
    );
    assert!(
        !outbox
            .enqueue(envelope.event_id, "indexer", due)
            .await
            .expect("duplicate enqueue")
    );
    assert_eq!(outbox.pending_count("indexer").await.expect("pending"), 1);

    let message = outbox
        .claim_next("indexer", Utc::now())
        .await
        .expect("claim")
        .expect("one message");
    assert_eq!(message.event_id, envelope.event_id);
    assert!(
        outbox
            .mark_processed(message.id, Utc::now())
            .await
            .expect("mark")
    );
    assert!(
        !outbox
            .mark_processed(message.id, Utc::now())
            .await
            .expect("second mark")
    );
    assert_eq!(outbox.pending_count("indexer").await.expect("pending"), 0);
    assert!(
        outbox
            .claim_next("indexer", Utc::now())
            .await
            .expect("claim")
            .is_none()
    );
}

#[tokio::test]
async fn ledger_blocks_duplicate_call() {
    let storage = storage().await;
    let ledger = ToolLedger::new(storage.pool());
    let record = ToolCallRecord {
        run_id: RunId::new(),
        step_id: StepId::new(),
        tool_call_id: "call-1".to_owned(),
        tool_name: "fs.read".to_owned(),
        state: ToolCallState::Started,
        result_ref: None,
        created_at: Utc::now(),
    };

    ledger.begin(&record).await.expect("begin");
    let error = ledger
        .begin(&record)
        .await
        .expect_err("duplicate call must be rejected");
    assert!(matches!(error, SilynError::Permanent(_)));

    ledger
        .complete(
            record.run_id,
            record.step_id,
            &record.tool_call_id,
            ToolCallState::Succeeded,
            Some("blob:result"),
        )
        .await
        .expect("complete");

    let stored = ledger
        .get(record.run_id, record.step_id, &record.tool_call_id)
        .await
        .expect("get")
        .expect("record");
    assert_eq!(stored.state, ToolCallState::Succeeded);
    assert_eq!(stored.result_ref.as_deref(), Some("blob:result"));

    let unknown = ToolCallRecord {
        tool_call_id: "call-2".to_owned(),
        state: ToolCallState::Started,
        ..record.clone()
    };
    ledger.begin(&unknown).await.expect("begin unknown");
    ledger
        .mark_indeterminate(unknown.run_id, unknown.step_id, &unknown.tool_call_id)
        .await
        .expect("mark indeterminate");
    let stored = ledger
        .get(unknown.run_id, unknown.step_id, &unknown.tool_call_id)
        .await
        .expect("get")
        .expect("record");
    assert_eq!(stored.state, ToolCallState::Indeterminate);
}

#[tokio::test]
async fn checkpoint_integrity() {
    let storage = storage().await;
    let checkpoints = CheckpointStore::new(storage.pool());
    let run_id = RunId::new();

    let first = CheckpointRecord::new(run_id, 1, "blob:state-1", 1);
    let second = CheckpointRecord::new(run_id, 2, "blob:state-2", 1);
    checkpoints.insert(&first).await.expect("insert first");
    checkpoints.insert(&second).await.expect("insert second");

    let latest = checkpoints
        .latest(run_id, 1)
        .await
        .expect("latest")
        .expect("checkpoint");
    assert_eq!(latest.seq, 2);
    assert_eq!(latest.state_ref, "blob:state-2");

    let error = checkpoints
        .latest(run_id, 2)
        .await
        .expect_err("schema mismatch must fail");
    assert!(matches!(error, SilynError::Permanent(_)));

    let mut corrupt = CheckpointRecord::new(run_id, 3, "blob:state-3", 1);
    corrupt.checksum = "deadbeef".to_owned();
    let error = checkpoints
        .insert(&corrupt)
        .await
        .expect_err("bad checksum must fail");
    assert!(matches!(error, SilynError::Permanent(_)));
}
