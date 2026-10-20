//! End-to-end tests for the bounded, resumable run loop.
//!
//! Everything is offline: the model is a scripted [`MockProvider`] (or a
//! deliberately blocking fake), tools are in-process fakes, and storage is an
//! in-memory SQLite database.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use silyn_core::SilynError;
use silyn_core::contracts::{
    AuthorizedToolContext, ModelProvider, ModelRequest, ModelResponse, OutcomeVerifier, Role,
    RunSnapshot, ToolCall, ToolExecutor, ToolResult,
};
use silyn_core::event::{RunEvent, VerifiedOutcome};
use silyn_core::ids::{AgentId, PolicyVersionId, RunId, SessionId, StepId, TaskId};
use silyn_core::state::{RunStatus, TaskStatus};
use silyn_llm::{MockProvider, Price};
use silyn_runtime::state::{PendingAction, RUN_STATE_SCHEMA_VERSION, RunState};
use silyn_runtime::{RunControl, RunOptions, RunOutcome, Runtime};
use silyn_storage::{
    AgentRecord, AgentRepository, CheckpointRecord, CheckpointStore, Journal, RunRecord,
    RunRepository, SessionRecord, SessionRepository, Storage, TaskRecord, TaskRepository,
    ToolCallRecord, ToolCallState, ToolLedger,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn new_storage() -> Storage {
    Storage::connect_in_memory()
        .await
        .expect("in-memory storage")
}

async fn seed_task(storage: &Storage, objective: &str) -> TaskRecord {
    let agent = AgentRecord {
        id: AgentId::new(),
        owner_id: "local-user".to_owned(),
        profile_version: "profile-v1".to_owned(),
        active_policy_version: None,
        created_at: chrono::Utc::now(),
    };
    AgentRepository::new(storage.pool())
        .insert(&agent)
        .await
        .expect("insert agent");

    let session = SessionRecord {
        id: SessionId::new(),
        agent_id: agent.id,
        context_version: "ctx-v1".to_owned(),
        created_at: chrono::Utc::now(),
    };
    SessionRepository::new(storage.pool())
        .insert(&session)
        .await
        .expect("insert session");

    let now = chrono::Utc::now();
    let task = TaskRecord {
        id: TaskId::new(),
        session_id: session.id,
        objective: objective.to_owned(),
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

async fn create_run(storage: &Storage, task_id: TaskId) -> RunId {
    let run_id = RunId::new();
    let now = chrono::Utc::now();
    RunRepository::new(storage.pool())
        .insert(&RunRecord {
            id: run_id,
            task_id,
            policy_version: PolicyVersionId::new(),
            memory_snapshot_version: None,
            model_id: "mock-model".to_owned(),
            status: RunStatus::Created,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            started_at: None,
            finished_at: None,
            created_at: now,
        })
        .await
        .expect("insert run");
    run_id
}

fn default_options() -> RunOptions {
    RunOptions {
        model: "mock-model".to_owned(),
        capabilities: vec![
            "fs.list".to_owned(),
            "fs.read".to_owned(),
            "data.profile".to_owned(),
            "report.write".to_owned(),
        ],
        budget: silyn_runtime::BudgetSpec::default(),
        price: None,
        cancel_file: None,
        memories: Vec::new(),
        agent_id: None,
    }
}

fn response(content: impl Into<String>) -> ModelResponse {
    ModelResponse {
        content: content.into(),
        model: "mock-model".to_owned(),
        input_tokens: 10,
        output_tokens: 5,
    }
}

fn tokens(content: impl Into<String>, input_tokens: u64, output_tokens: u64) -> ModelResponse {
    ModelResponse {
        content: content.into(),
        model: "mock-model".to_owned(),
        input_tokens,
        output_tokens,
    }
}

fn tool_action(tool: &str, arguments: serde_json::Value) -> String {
    json!({ "action": "tool", "tool": tool, "arguments": arguments }).to_string()
}

fn final_action(answer: &str) -> String {
    json!({ "action": "final", "answer": answer }).to_string()
}

struct UnknownVerifier;

#[async_trait]
impl OutcomeVerifier for UnknownVerifier {
    async fn verify(
        &self,
        _run: RunSnapshot,
    ) -> Result<silyn_core::contracts::OutcomeEvidence, SilynError> {
        Ok(silyn_core::contracts::OutcomeEvidence {
            outcome: VerifiedOutcome::Unknown,
            evidence_refs: vec![],
            confidence: 0.0,
        })
    }
}

#[derive(Default)]
struct TestTools {
    calls: Mutex<Vec<ToolCall>>,
    results: Mutex<VecDeque<Result<ToolResult, SilynError>>>,
}

impl TestTools {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn with(results: Vec<Result<ToolResult, SilynError>>) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            results: Mutex::new(results.into()),
        })
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("calls").len()
    }
}

#[async_trait]
impl ToolExecutor for TestTools {
    async fn execute(
        &self,
        _context: AuthorizedToolContext,
        call: ToolCall,
    ) -> Result<ToolResult, SilynError> {
        self.calls.lock().expect("calls").push(call);
        self.results
            .lock()
            .expect("results")
            .pop_front()
            .unwrap_or(Ok(ToolResult {
                content: "ok".to_owned(),
                is_error: false,
            }))
    }
}

type TestRuntime = (Arc<Runtime>, Arc<MockProvider>, Arc<TestTools>);

fn runtime_with(
    storage: &Storage,
    responses: Vec<ModelResponse>,
    tools: Arc<TestTools>,
    verifier: Arc<dyn OutcomeVerifier>,
) -> TestRuntime {
    let provider = Arc::new(MockProvider::new(responses));
    let runtime = Arc::new(Runtime::new(
        storage.clone(),
        provider.clone(),
        tools.clone(),
        verifier,
    ));
    (runtime, provider, tools)
}

async fn run_with(
    storage: &Storage,
    responses: Vec<ModelResponse>,
    options: RunOptions,
) -> (RunOutcome, Arc<MockProvider>, Arc<TestTools>, TaskRecord) {
    let task = seed_task(storage, "test objective").await;
    let (runtime, provider, tools) = runtime_with(
        storage,
        responses,
        TestTools::new(),
        Arc::new(UnknownVerifier),
    );
    let outcome = runtime
        .run_task(task.id, options)
        .await
        .expect("run completes");
    (outcome, provider, tools, task)
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn budget_exhaustion_fails_run() {
    // Step budget: one tool step is allowed, the second is refused.
    let storage = new_storage().await;
    let options = RunOptions {
        budget: silyn_runtime::BudgetSpec {
            max_steps: 1,
            ..silyn_runtime::BudgetSpec::default()
        },
        ..default_options()
    };
    let (outcome, _provider, tools, _task) = run_with(
        &storage,
        vec![response(tool_action("fs.read", json!({ "path": "x" })))],
        options,
    )
    .await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.reason.unwrap().contains("step budget"));
    assert_eq!(tools.call_count(), 1);

    // Input token budget.
    let storage = new_storage().await;
    let options = RunOptions {
        budget: silyn_runtime::BudgetSpec {
            max_input_tokens: 5,
            ..silyn_runtime::BudgetSpec::default()
        },
        ..default_options()
    };
    let (outcome, _provider, _tools, _task) = run_with(
        &storage,
        vec![tokens(final_action("too many tokens"), 10, 0)],
        options,
    )
    .await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.reason.unwrap().contains("input token budget"));

    // Cost budget requires and uses the price table.
    let storage = new_storage().await;
    let options = RunOptions {
        budget: silyn_runtime::BudgetSpec {
            max_cost_usd: Some(0.01),
            ..silyn_runtime::BudgetSpec::default()
        },
        price: Some(Price {
            input_per_million: 1_000.0,
            output_per_million: 0.0,
        }),
        ..default_options()
    };
    let (outcome, _provider, _tools, _task) = run_with(
        &storage,
        vec![tokens(final_action("expensive"), 10_000, 0)],
        options,
    )
    .await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.reason.unwrap().contains("cost budget"));

    // Wall-clock budget: already exhausted before the first model call.
    let storage = new_storage().await;
    let options = RunOptions {
        budget: silyn_runtime::BudgetSpec {
            max_wall_clock: Duration::ZERO,
            ..silyn_runtime::BudgetSpec::default()
        },
        ..default_options()
    };
    let task = seed_task(&storage, "wall clock").await;
    let (runtime, provider, _tools) = runtime_with(
        &storage,
        vec![response(final_action("never"))],
        TestTools::new(),
        Arc::new(UnknownVerifier),
    );
    let outcome = runtime
        .run_task(task.id, options)
        .await
        .expect("run completes");
    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.reason.unwrap().contains("wall-clock budget"));
    assert!(provider.requests().is_empty());
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancellation_before_first_step_stops_run() {
    let storage = new_storage().await;
    let task = seed_task(&storage, "cancelled objective").await;
    let run_id = create_run(&storage, task.id).await;

    let dir = TempDir::new().expect("temp dir");
    let cancel_file = dir.path().join("run.cancel");
    std::fs::write(&cancel_file, "cancel\n").expect("pre-write cancel file");

    let (runtime, provider, tools) = runtime_with(
        &storage,
        vec![response(final_action("must not run"))],
        TestTools::new(),
        Arc::new(UnknownVerifier),
    );
    let outcome = runtime
        .execute(
            run_id,
            RunOptions {
                cancel_file: Some(cancel_file),
                ..default_options()
            },
        )
        .await
        .expect("run completes");

    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert!(provider.requests().is_empty());
    assert_eq!(tools.call_count(), 0);

    let run = RunRepository::new(storage.pool())
        .get(run_id)
        .await
        .expect("get run")
        .expect("run exists");
    assert_eq!(run.status, RunStatus::Cancelled);
    let stored_task = TaskRepository::new(storage.pool())
        .get(task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(stored_task.status, TaskStatus::Cancelled);

    let events = Journal::new(storage.pool())
        .list_for_aggregate(run_id)
        .await
        .expect("events");
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(RunEvent::RunCancelled)
    ));
}

struct BlockingProvider {
    requests: AtomicUsize,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl BlockingProvider {
    fn new() -> Self {
        Self {
            requests: AtomicUsize::new(0),
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ModelProvider for BlockingProvider {
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, SilynError> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        Ok(response(final_action("released")))
    }
}

#[tokio::test]
async fn cancel_file_interrupts_in_flight_model_call() {
    let storage = new_storage().await;
    let task = seed_task(&storage, "interruptible objective").await;
    let run_id = create_run(&storage, task.id).await;

    let dir = TempDir::new().expect("temp dir");
    let cancel_file = dir.path().join("run").join(format!("{run_id}.cancel"));

    let provider = Arc::new(BlockingProvider::new());
    let runtime = Arc::new(Runtime::new(
        storage.clone(),
        provider.clone(),
        TestTools::new(),
        Arc::new(UnknownVerifier),
    ));
    let run_options = RunOptions {
        cancel_file: Some(cancel_file.clone()),
        ..default_options()
    };
    let handle = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        let run_options = run_options.clone();
        async move { runtime.execute(run_id, run_options).await }
    });

    provider.started.notified().await;
    RunControl::with_cancel_file(cancel_file).cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("cancellation is observed")
        .expect("task joined")
        .expect("run completes");

    assert_eq!(outcome.status, RunStatus::Cancelled);
    assert_eq!(provider.request_count(), 1);

    let run = RunRepository::new(storage.pool())
        .get(run_id)
        .await
        .expect("get run")
        .expect("run exists");
    assert_eq!(run.status, RunStatus::Cancelled);
    let events = Journal::new(storage.pool())
        .list_for_aggregate(run_id)
        .await
        .expect("events");
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(RunEvent::RunCancelled)
    ));
}

// ---------------------------------------------------------------------------
// Resume and idempotency
// ---------------------------------------------------------------------------

struct BlockingTools {
    calls: AtomicUsize,
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl BlockingTools {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            started: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ToolExecutor for BlockingTools {
    async fn execute(
        &self,
        _context: AuthorizedToolContext,
        _call: ToolCall,
    ) -> Result<ToolResult, SilynError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        Ok(ToolResult {
            content: "side effect applied".to_owned(),
            is_error: false,
        })
    }
}

#[tokio::test]
async fn resume_after_crash_does_not_repeat_side_effect() {
    let storage = new_storage().await;
    let task = seed_task(&storage, "crash recovery objective").await;
    let run_id = create_run(&storage, task.id).await;

    let tools = Arc::new(BlockingTools::new());
    let provider = Arc::new(MockProvider::new(vec![response(tool_action(
        "report.write",
        json!({ "path": "output/report.md", "content": "x" }),
    ))]));
    let runtime = Arc::new(Runtime::new(
        storage.clone(),
        provider.clone(),
        tools.clone(),
        Arc::new(UnknownVerifier),
    ));
    let run_options = default_options();

    let handle = tokio::spawn({
        let runtime = Arc::clone(&runtime);
        let run_options = run_options.clone();
        async move { runtime.execute(run_id, run_options).await }
    });

    tools.started.notified().await;
    // Simulate a process kill while the side effect is in flight: the ledger is
    // `Started` and the pending action is checkpointed, but nothing completed.
    handle.abort();
    let _ = handle.await;
    assert_eq!(tools.call_count(), 1);

    let latest = CheckpointStore::new(storage.pool())
        .latest(run_id, RUN_STATE_SCHEMA_VERSION)
        .await
        .expect("latest checkpoint")
        .expect("checkpoint exists");
    let state: RunState = serde_json::from_str(&latest.state_ref).expect("state json");
    let pending = state.pending.clone().expect("pending action");
    assert!(
        state
            .messages
            .iter()
            .any(|message| message.role == Role::User && message.content.contains("crash recovery"))
    );

    let error = runtime
        .execute(run_id, run_options.clone())
        .await
        .expect_err("a running run must go through resume");
    assert!(error.to_string().contains("use resume"));

    let outcome = runtime
        .resume(run_id, run_options)
        .await
        .expect("resume completes");

    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(
        outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains(&pending.call.call_id)),
        "reason must name the indeterminate call"
    );
    assert_eq!(tools.call_count(), 1, "side effect must not run twice");
    assert_eq!(
        provider.requests().len(),
        1,
        "resume must not call the model"
    );

    let ledger = ToolLedger::new(storage.pool())
        .get(run_id, pending.step_id, &pending.call.call_id)
        .await
        .expect("ledger get")
        .expect("ledger row");
    assert_eq!(ledger.state, ToolCallState::Indeterminate);
}

#[tokio::test]
async fn resume_reuses_completed_tool_result() {
    let storage = new_storage().await;
    let task = seed_task(&storage, "recover completed call").await;
    let run_id = create_run(&storage, task.id).await;

    let step_id = StepId::new();
    let call = ToolCall {
        call_id: format!("{run_id}:1"),
        tool: "fs.read".to_owned(),
        arguments: json!({ "path": "data.csv" }),
    };
    let state = RunState {
        step_seq: 1,
        pending: Some(PendingAction {
            step_id,
            step_seq: 1,
            call: call.clone(),
        }),
        ..RunState::default()
    };
    CheckpointStore::new(storage.pool())
        .insert(&CheckpointRecord::new(
            run_id,
            1,
            serde_json::to_string(&state).expect("state json"),
            RUN_STATE_SCHEMA_VERSION,
        ))
        .await
        .expect("insert checkpoint");
    ToolLedger::new(storage.pool())
        .begin(&ToolCallRecord {
            run_id,
            step_id,
            tool_call_id: call.call_id.clone(),
            tool_name: call.tool.clone(),
            state: ToolCallState::Started,
            result_ref: None,
            created_at: chrono::Utc::now(),
        })
        .await
        .expect("ledger begin");
    ToolLedger::new(storage.pool())
        .complete(
            run_id,
            step_id,
            &call.call_id,
            ToolCallState::Succeeded,
            Some("cached file contents"),
        )
        .await
        .expect("ledger complete");

    let tools = TestTools::new();
    let provider = Arc::new(MockProvider::new(vec![response(final_action("done"))]));
    let runtime = Runtime::new(
        storage.clone(),
        provider.clone(),
        tools.clone(),
        Arc::new(UnknownVerifier),
    );

    let outcome = runtime
        .resume(run_id, default_options())
        .await
        .expect("resume completes");

    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(tools.call_count(), 0, "a completed call is not re-executed");
    assert_eq!(provider.requests().len(), 1);
    assert!(provider.requests()[0].messages.iter().any(|message| {
        message.role == Role::Tool && message.content.contains("cached file contents")
    }));

    let steps = silyn_storage::StepRepository::new(storage.pool())
        .list_by_run(run_id)
        .await
        .expect("steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].result_ref.as_deref(), Some("cached file contents"));
}

#[tokio::test]
async fn pending_tool_is_recovered_before_the_next_step_budget() {
    let storage = new_storage().await;
    let task = seed_task(&storage, "pending at the step limit").await;
    let run_id = create_run(&storage, task.id).await;

    let step_id = StepId::new();
    let call = ToolCall {
        call_id: format!("{run_id}:1"),
        tool: "fs.read".to_owned(),
        arguments: json!({ "path": "data.csv" }),
    };
    let state = RunState {
        step_seq: 1,
        pending: Some(PendingAction {
            step_id,
            step_seq: 1,
            call: call.clone(),
        }),
        ..RunState::default()
    };
    CheckpointStore::new(storage.pool())
        .insert(&CheckpointRecord::new(
            run_id,
            1,
            serde_json::to_string(&state).expect("state json"),
            RUN_STATE_SCHEMA_VERSION,
        ))
        .await
        .expect("insert checkpoint");
    ToolLedger::new(storage.pool())
        .begin(&ToolCallRecord {
            run_id,
            step_id,
            tool_call_id: call.call_id.clone(),
            tool_name: call.tool.clone(),
            state: ToolCallState::Started,
            result_ref: None,
            created_at: chrono::Utc::now(),
        })
        .await
        .expect("ledger begin");
    ToolLedger::new(storage.pool())
        .complete(
            run_id,
            step_id,
            &call.call_id,
            ToolCallState::Succeeded,
            Some("cached"),
        )
        .await
        .expect("ledger complete");

    let tools = TestTools::new();
    let provider = Arc::new(MockProvider::new(vec![response(final_action("unused"))]));
    let runtime = Runtime::new(
        storage.clone(),
        provider.clone(),
        tools.clone(),
        Arc::new(UnknownVerifier),
    );
    let options = RunOptions {
        budget: silyn_runtime::BudgetSpec {
            max_steps: 1,
            ..silyn_runtime::BudgetSpec::default()
        },
        ..default_options()
    };

    let outcome = runtime
        .resume(run_id, options)
        .await
        .expect("resume completes");

    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(
        outcome
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("step budget")),
        "the next model step is over budget"
    );
    assert_eq!(
        tools.call_count(),
        0,
        "the completed pending call must be reused, not executed again"
    );
    assert!(
        provider.requests().is_empty(),
        "recovering the pending call does not call the model"
    );

    let events = silyn_storage::Journal::new(storage.pool())
        .list_for_aggregate(run_id)
        .await
        .expect("events");
    assert!(
        events
            .iter()
            .any(|envelope| matches!(envelope.event, RunEvent::StepCompleted { seq: 1, .. })),
        "the reused result must be recorded as the completed step"
    );
}

#[tokio::test]
async fn seeded_started_ledger_blocks_replay() {
    let storage = new_storage().await;
    let task = seed_task(&storage, "seeded indeterminate").await;
    let run_id = create_run(&storage, task.id).await;

    let step_id = StepId::new();
    let call = ToolCall {
        call_id: format!("{run_id}:1"),
        tool: "report.write".to_owned(),
        arguments: json!({ "path": "output/report.md", "content": "x" }),
    };
    let pending = PendingAction {
        step_id,
        step_seq: 1,
        call: call.clone(),
    };
    let state = RunState {
        step_seq: 1,
        pending: Some(pending),
        ..RunState::default()
    };
    CheckpointStore::new(storage.pool())
        .insert(&CheckpointRecord::new(
            run_id,
            1,
            serde_json::to_string(&state).expect("state json"),
            RUN_STATE_SCHEMA_VERSION,
        ))
        .await
        .expect("insert checkpoint");
    ToolLedger::new(storage.pool())
        .begin(&ToolCallRecord {
            run_id,
            step_id,
            tool_call_id: call.call_id.clone(),
            tool_name: call.tool.clone(),
            state: ToolCallState::Started,
            result_ref: None,
            created_at: chrono::Utc::now(),
        })
        .await
        .expect("ledger begin");

    let tools = TestTools::new();
    let provider = Arc::new(MockProvider::new(vec![response(final_action(
        "must not call",
    ))]));
    let runtime = Runtime::new(
        storage.clone(),
        provider.clone(),
        tools.clone(),
        Arc::new(UnknownVerifier),
    );

    let outcome = runtime
        .resume(run_id, default_options())
        .await
        .expect("resume completes");

    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.reason.unwrap().contains(&call.call_id));
    assert_eq!(tools.call_count(), 0);
    assert!(provider.requests().is_empty());

    let ledger = ToolLedger::new(storage.pool())
        .get(run_id, step_id, &call.call_id)
        .await
        .expect("ledger get")
        .expect("ledger row");
    assert_eq!(ledger.state, ToolCallState::Indeterminate);
}

// ---------------------------------------------------------------------------
// Loop detection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_tool_call_fails_run() {
    let storage = new_storage().await;
    let repeated = tool_action("fs.read", json!({ "path": "data.csv" }));
    let (outcome, _provider, tools, _task) = run_with(
        &storage,
        vec![
            response(&repeated),
            response(&repeated),
            response(&repeated),
        ],
        default_options(),
    )
    .await;

    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.reason.unwrap().contains("loop detected"));
    assert_eq!(
        tools.call_count(),
        2,
        "the third identical action must not execute"
    );
}

// ---------------------------------------------------------------------------
// Malformed model output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_action_is_repaired_once() {
    // One repair turn succeeds.
    let storage = new_storage().await;
    let (outcome, provider, _tools, _task) = run_with(
        &storage,
        vec![
            response("no json here"),
            response(final_action("recovered")),
        ],
        default_options(),
    )
    .await;
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.final_answer.as_deref(), Some("recovered"));
    assert_eq!(provider.requests().len(), 2);
    assert!(
        provider.requests()[1]
            .messages
            .iter()
            .any(|message| { message.role == Role::User && message.content.contains("rejected") })
    );

    // Two failures end the run without a third attempt.
    let storage = new_storage().await;
    let (outcome, provider, _tools, _task) = run_with(
        &storage,
        vec![response("still not json"), response("{ broken")],
        default_options(),
    )
    .await;
    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.reason.unwrap().contains("malformed"));
    assert_eq!(provider.requests().len(), 2);
}

// ---------------------------------------------------------------------------
// Checkpoint integrity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn checkpoint_corruption_and_schema_mismatch_fail_loudly() {
    // Corrupted checksum.
    let storage = new_storage().await;
    let task = seed_task(&storage, "corrupt checkpoint").await;
    let run_id = create_run(&storage, task.id).await;
    let state = serde_json::to_string(&RunState::default()).expect("state json");
    CheckpointStore::new(storage.pool())
        .insert(&CheckpointRecord::new(
            run_id,
            1,
            state.clone(),
            RUN_STATE_SCHEMA_VERSION,
        ))
        .await
        .expect("insert checkpoint");
    sqlx::query("UPDATE checkpoints SET checksum = 'deadbeef' WHERE run_id = ?1")
        .bind(run_id.to_string())
        .execute(storage.pool())
        .await
        .expect("corrupt checksum");

    let (runtime, _provider, _tools) = runtime_with(
        &storage,
        vec![response(final_action("unused"))],
        TestTools::new(),
        Arc::new(UnknownVerifier),
    );
    let error = runtime
        .execute(run_id, default_options())
        .await
        .expect_err("corrupted checkpoint must fail");
    assert!(matches!(error, SilynError::Permanent(_)));
    assert!(error.to_string().contains("checksum"));

    // Unknown schema version.
    let storage = new_storage().await;
    let task = seed_task(&storage, "future checkpoint").await;
    let run_id = create_run(&storage, task.id).await;
    CheckpointStore::new(storage.pool())
        .insert(&CheckpointRecord::new(run_id, 1, state, 2))
        .await
        .expect("insert checkpoint");
    let (runtime, _provider, _tools) = runtime_with(
        &storage,
        vec![response(final_action("unused"))],
        TestTools::new(),
        Arc::new(UnknownVerifier),
    );
    let error = runtime
        .execute(run_id, default_options())
        .await
        .expect_err("unknown checkpoint schema must fail");
    assert!(matches!(error, SilynError::Permanent(_)));
    assert!(error.to_string().contains("schema version"));
}

// ---------------------------------------------------------------------------
// Terminal events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn terminal_events_and_rows_are_consistent() {
    let storage = new_storage().await;
    let task = seed_task(&storage, "terminal objective").await;
    let (runtime, provider, _tools) = runtime_with(
        &storage,
        vec![response(final_action("done"))],
        TestTools::new(),
        Arc::new(UnknownVerifier),
    );

    let outcome = runtime
        .run_task(task.id, default_options())
        .await
        .expect("run completes");
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(outcome.outcome, Some(VerifiedOutcome::Unknown));
    assert_eq!(outcome.final_answer.as_deref(), Some("done"));
    assert_eq!(outcome.steps, 1);
    assert_eq!(outcome.input_tokens, 10);
    assert_eq!(outcome.output_tokens, 5);
    assert_eq!(provider.requests().len(), 1);

    let run = RunRepository::new(storage.pool())
        .get(outcome.run_id)
        .await
        .expect("get run")
        .expect("run exists");
    assert_eq!(run.status, RunStatus::Completed);
    assert!(run.started_at.is_some());
    assert!(run.finished_at.is_some());
    assert_eq!(run.input_tokens, 10);
    assert_eq!(run.output_tokens, 5);

    let stored_task = TaskRepository::new(storage.pool())
        .get(task.id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(stored_task.status, TaskStatus::Completed);

    let events = Journal::new(storage.pool())
        .list_for_aggregate(outcome.run_id)
        .await
        .expect("events");
    let kinds: Vec<&RunEvent> = events.iter().map(|event| &event.event).collect();
    assert!(matches!(kinds.first(), Some(RunEvent::RunCreated { .. })));
    assert!(matches!(kinds.get(1), Some(RunEvent::RunStarted)));
    assert!(kinds.iter().any(|event| matches!(
        event,
        RunEvent::RunVerified {
            outcome: VerifiedOutcome::Unknown
        }
    )));
    assert!(matches!(kinds.last(), Some(RunEvent::RunCompleted)));
    assert!(
        events.windows(2).all(|pair| pair[0].seq + 1 == pair[1].seq),
        "event sequence must be contiguous"
    );

    let error = runtime
        .execute(outcome.run_id, default_options())
        .await
        .expect_err("terminal run cannot execute again");
    assert!(error.to_string().contains("already completed"));
}

// ---------------------------------------------------------------------------
// Tool retry classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tool_retry_depends_on_side_effect() {
    // Read-only tools may retry a transient error.
    let storage = new_storage().await;
    let task = seed_task(&storage, "read-only retry").await;
    let tools = TestTools::with(vec![
        Err(SilynError::Transient("temporary read failure".to_owned())),
        Ok(ToolResult {
            content: "file contents".to_owned(),
            is_error: false,
        }),
    ]);
    let provider = Arc::new(MockProvider::new(vec![
        response(tool_action("fs.read", json!({ "path": "data.csv" }))),
        response(final_action("done")),
    ]));
    let runtime = Runtime::new(
        storage.clone(),
        provider,
        tools.clone(),
        Arc::new(UnknownVerifier),
    );
    let outcome = runtime
        .run_task(task.id, default_options())
        .await
        .expect("run completes");
    assert_eq!(outcome.status, RunStatus::Completed);
    assert_eq!(tools.call_count(), 2);

    // A write call is never retried once execution started; its outcome is
    // indeterminate and blocks the run.
    let storage = new_storage().await;
    let task = seed_task(&storage, "write retry").await;
    let tools = TestTools::with(vec![Err(SilynError::Transient(
        "connection dropped after write".to_owned(),
    ))]);
    let (runtime, _provider, _tools) = runtime_with(
        &storage,
        vec![response(tool_action(
            "report.write",
            json!({ "path": "output/report.md", "content": "x" }),
        ))],
        tools.clone(),
        Arc::new(UnknownVerifier),
    );
    let outcome = runtime
        .run_task(task.id, default_options())
        .await
        .expect("run completes");
    assert_eq!(outcome.status, RunStatus::Failed);
    assert!(outcome.reason.unwrap().contains("indeterminate"));
    assert_eq!(tools.call_count(), 1);
}

// ---------------------------------------------------------------------------
// Run control
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_flag_is_shared_between_handles() {
    let control = RunControl::new();
    let observer = control.clone();
    let waiter = tokio::spawn(async move { observer.cancelled().await });

    control.cancel();
    assert!(control.is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("waiter wakes")
        .expect("waiter joined");
}
