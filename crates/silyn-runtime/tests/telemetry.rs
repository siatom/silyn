//! Telemetry integration test: spans carry IDs, states and metrics only.
//!
//! A local collecting layer captures every span the runtime emits, so the test
//! can assert both the declared field set (the frozen contract) and the values
//! that were actually recorded. No subscriber is installed by the library; this
//! test installs one with a thread-local default guard.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;
use silyn_core::SilynError;
use silyn_core::contracts::{
    AuthorizedToolContext, ModelResponse, OutcomeVerifier, RunSnapshot, ToolCall, ToolExecutor,
    ToolResult,
};
use silyn_core::event::VerifiedOutcome;
use silyn_core::ids::{AgentId, SessionId, TaskId};
use silyn_core::state::TaskStatus;
use silyn_llm::MockProvider;
use silyn_runtime::{RunOptions, RunOutcome, Runtime, telemetry};
use silyn_storage::{
    AgentRecord, AgentRepository, SessionRecord, SessionRepository, Storage, TaskRecord,
    TaskRepository,
};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

// ---------------------------------------------------------------------------
// Collecting layer
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Capture {
    spans: Arc<Mutex<BTreeMap<u64, SpanData>>>,
}

#[derive(Clone, Default)]
struct SpanData {
    name: String,
    declared: Vec<String>,
    values: BTreeMap<String, String>,
}

impl Capture {
    fn all(&self) -> Vec<SpanData> {
        self.spans
            .lock()
            .expect("capture lock")
            .values()
            .cloned()
            .collect()
    }

    fn span(&self, name: &str) -> SpanData {
        self.all()
            .into_iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("span `{name}` was not emitted"))
    }
}

struct FieldVisitor<'a> {
    values: &'a mut BTreeMap<String, String>,
}

impl Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.values
            .insert(field.name().to_owned(), value.to_owned());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.values
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.values
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.values
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.values
            .insert(field.name().to_owned(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.values
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, _context: Context<'_, S>) {
        let mut data = SpanData {
            name: attrs.metadata().name().to_owned(),
            ..SpanData::default()
        };
        {
            let mut visitor = FieldVisitor {
                values: &mut data.values,
            };
            attrs.record(&mut visitor);
        }
        data.declared = attrs
            .metadata()
            .fields()
            .iter()
            .map(|field| field.name().to_owned())
            .collect();
        self.spans
            .lock()
            .expect("capture lock")
            .insert(id.into_u64(), data);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, _context: Context<'_, S>) {
        let mut spans = self.spans.lock().expect("capture lock");
        if let Some(data) = spans.get_mut(&id.into_u64()) {
            let mut visitor = FieldVisitor {
                values: &mut data.values,
            };
            values.record(&mut visitor);
        }
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

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

fn response(content: impl Into<String>) -> ModelResponse {
    ModelResponse {
        content: content.into(),
        model: "mock-model".to_owned(),
        input_tokens: 10,
        output_tokens: 5,
    }
}

fn options() -> RunOptions {
    RunOptions {
        model: "mock-model".to_owned(),
        capabilities: vec!["fs.read".to_owned()],
        ..RunOptions::default()
    }
}

struct CanaryTools {
    output: String,
}

#[async_trait]
impl ToolExecutor for CanaryTools {
    async fn execute(
        &self,
        _context: AuthorizedToolContext,
        _call: ToolCall,
    ) -> Result<ToolResult, SilynError> {
        Ok(ToolResult {
            content: self.output.clone(),
            is_error: false,
        })
    }
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
            evidence_refs: vec!["artifact-1".to_owned()],
            confidence: 0.0,
        })
    }
}

/// Runs one tool turn and one final answer under a collecting subscriber.
async fn run_with_capture(objective: &str, tool_output: &str) -> (Capture, RunOutcome) {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let storage = Storage::connect_in_memory()
        .await
        .expect("in-memory storage");
    let task = seed_task(&storage, objective).await;
    let provider = Arc::new(MockProvider::new(vec![
        response(
            json!({ "action": "tool", "tool": "fs.read", "arguments": { "path": "CANARY-ARG" } })
                .to_string(),
        ),
        response(json!({ "action": "final", "answer": "done" }).to_string()),
    ]));
    let runtime = Runtime::new(
        storage,
        provider,
        Arc::new(CanaryTools {
            output: tool_output.to_owned(),
        }),
        Arc::new(UnknownVerifier),
    );

    let outcome = runtime
        .run_task(task.id, options())
        .await
        .expect("run completes");
    (capture, outcome)
}

fn as_vec(fields: &[&str]) -> Vec<String> {
    fields.iter().map(|field| (*field).to_owned()).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_records_run_id_and_terminal_status() {
    let (capture, outcome) = run_with_capture("telemetry objective", "ok").await;

    let run_id = outcome.run_id.to_string();
    let run = capture.span(telemetry::SPAN_RUN);
    assert_eq!(
        run.values.get(telemetry::FIELD_RUN_ID),
        Some(&run_id),
        "the run span must carry the run id"
    );
    assert_eq!(
        run.values.get(telemetry::FIELD_STATUS),
        Some(&"completed".to_owned())
    );
    assert_eq!(
        run.values.get(telemetry::FIELD_STEP_COUNT),
        Some(&"2".to_owned())
    );
    assert_eq!(
        run.values.get(telemetry::FIELD_INPUT_TOKENS),
        Some(&"20".to_owned())
    );
    assert_eq!(
        run.values.get(telemetry::FIELD_OUTPUT_TOKENS),
        Some(&"10".to_owned())
    );
    assert!(
        run.values.contains_key(telemetry::FIELD_DURATION_MS),
        "the run span records its duration"
    );

    // Every span in the contract is emitted for this run.
    for name in telemetry::SPAN_NAMES {
        let span = capture.span(name);
        assert_eq!(
            span.values.get(telemetry::FIELD_RUN_ID),
            Some(&run_id),
            "span `{name}` is correlated by run id"
        );
    }

    let tool = capture.span(telemetry::SPAN_TOOL);
    assert_eq!(
        tool.values.get(telemetry::FIELD_TOOL_NAME),
        Some(&"fs.read".to_owned())
    );
    assert_eq!(
        tool.values.get(telemetry::FIELD_STATE),
        Some(&"succeeded".to_owned())
    );
    assert_eq!(
        tool.values.get(telemetry::FIELD_CAPABILITY),
        Some(&"fs.read".to_owned())
    );
    assert!(
        !tool.values.contains_key(telemetry::FIELD_RESULT_REF),
        "tool output is never copied into a span"
    );
}

#[tokio::test]
async fn prompt_tool_and_secret_canaries_never_reach_spans() {
    let (capture, _outcome) = run_with_capture(
        "CANARY objective sk-canary0123456789",
        "CANARY tool output sk-toolcanary9876",
    )
    .await;

    for span in capture.all() {
        for (field, value) in &span.values {
            assert!(
                !value.contains("CANARY"),
                "span `{}` field `{field}` leaked content: {value}",
                span.name
            );
            assert!(
                !value.contains("sk-"),
                "span `{}` field `{field}` leaked a key shape: {value}",
                span.name
            );
        }
    }
}

#[tokio::test]
async fn emitted_spans_declare_only_contract_fields() {
    let (capture, _outcome) = run_with_capture("field contract", "ok").await;

    let expected = [
        (telemetry::SPAN_RUN, telemetry::RUN_FIELDS),
        (telemetry::SPAN_STEP, telemetry::STEP_FIELDS),
        (telemetry::SPAN_MODEL, telemetry::MODEL_FIELDS),
        (telemetry::SPAN_TOOL, telemetry::TOOL_FIELDS),
        (telemetry::SPAN_CHECKPOINT, telemetry::CHECKPOINT_FIELDS),
        (telemetry::SPAN_VERIFY, telemetry::VERIFY_FIELDS),
    ];
    for (name, fields) in expected {
        assert_eq!(
            capture.span(name).declared,
            as_vec(fields),
            "declared fields of span `{name}`"
        );
    }
}

#[tokio::test]
async fn no_subscriber_keeps_the_run_behaviour() {
    // A run without any subscriber still completes and returns the same shape.
    let storage = Storage::connect_in_memory()
        .await
        .expect("in-memory storage");
    let task = seed_task(&storage, "no subscriber").await;
    let provider = Arc::new(MockProvider::new(vec![response(
        json!({ "action": "final", "answer": "done" }).to_string(),
    )]));
    let runtime = Runtime::new(
        storage,
        provider,
        Arc::new(CanaryTools {
            output: String::new(),
        }),
        Arc::new(UnknownVerifier),
    );
    let outcome = runtime
        .run_task(task.id, options())
        .await
        .expect("run completes");
    assert_eq!(outcome.status, silyn_core::state::RunStatus::Completed);
}
