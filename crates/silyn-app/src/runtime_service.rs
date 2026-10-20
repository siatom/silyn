//! Run service: turns a validated request into a runtime execution.
//!
//! This module is the single place where the app layer decides *how* a run
//! executes: which provider answers, which directory the tools are confined to,
//! which verifier judges the result, which memories are injected and which
//! cancel file the loop polls. The CLI only parses flags and formats results;
//! `silyn-runtime` owns the loop and `silyn-growth` owns retrieval and
//! experience extraction.
//!
//! Invariants:
//!
//! - the run id is known before execution starts, so the cancel file can be
//!   `home/run/<run_id>.cancel` (the same path `runs cancel` writes);
//! - memories are scoped to the resolved agent *and* its owner, so one agent's
//!   memory can never appear in another agent's prompt;
//! - without an expected answer the W3 `AnswerVerifier` still reports `Unknown`;
//!   only `--expect-json` enables the composite verifier.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use silyn_core::SilynError;
use silyn_core::contracts::{
    MemoryScope, MemoryType, ModelProvider, ModelResponse, OutcomeEvidence, OutcomeVerifier,
    RunSnapshot,
};
use silyn_core::event::VerifiedOutcome;
use silyn_core::ids::{AgentId, MemoryId, RunId, SessionId, TaskId};
use silyn_core::state::{LifecycleState, RunStatus};
use silyn_growth::{
    CompositeVerifier, ExpectedAnswerChecker, ExperienceRecord, ExperienceStore, MemoryHitRecord,
    MemoryRecord, MemoryScopeFilter, MemoryStore, NewMemory, Provenance,
};
use silyn_llm::{MockProvider, OpenAiCompatibleProvider, ProviderConfig, ReqwestTransport};
use silyn_runtime::{MemoryAuditSink, MemorySnippet, RunOptions, RunOutcome, Runtime, ToolBroker};
use silyn_storage::{AgentRecord, AgentRepository, SessionRepository, TaskRecord, TaskRepository};

use crate::app::App;

/// Directory under the app home that tool calls are confined to.
pub const WORKSPACE_DIR: &str = "workspace";

/// Final answer returned by the offline provider.
pub const OFFLINE_ANSWER: &str = "offline mock run";

/// Number of memories retrieved for one run.
pub const MEMORY_LIMIT: usize = 8;

/// Upper bound for `memory list`, matching the store's retrieval page cap.
pub const MEMORY_LIST_LIMIT: usize = 200;

/// Confidence assigned to user-authored or explicitly stored memories.
const DEFAULT_MEMORY_CONFIDENCE: f32 = 0.5;
/// Importance assigned to explicitly stored memories.
const DEFAULT_MEMORY_IMPORTANCE: f32 = 0.5;

/// One `silyn run` request.
///
/// Exactly one source is allowed: a task id, a run id to resume, or a session
/// plus an objective (which creates the task first).
#[derive(Debug, Clone)]
pub struct RunRequest {
    /// Execute an existing task.
    pub task_id: Option<TaskId>,
    /// Create a task in this session; requires [`objective`](Self::objective).
    pub session_id: Option<SessionId>,
    /// Objective of the task created for [`session_id`](Self::session_id).
    pub objective: Option<String>,
    /// Resume an existing, non-terminal run.
    pub resume: Option<RunId>,
    /// Model, capabilities, budgets, memories and cancel file. The service fills
    /// `price`, `memories`, `agent_id` and `cancel_file`.
    pub options: RunOptions,
    /// Use the deterministic offline provider: no API key, no network. The model
    /// name is still recorded on the run.
    pub offline: bool,
    /// Expected JSON answer. When set, the run is verified by a
    /// `CompositeVerifier` with an `ExpectedAnswerChecker` over the workspace.
    pub expected_json: Option<Value>,
}

impl RunRequest {
    /// Rejects requests that do not name exactly one run source.
    ///
    /// Every violation is a permanent error: no combination of flags becomes
    /// valid by retrying.
    pub fn validate(&self) -> Result<(), SilynError> {
        let has_session_source = self.session_id.is_some() || self.objective.is_some();

        if self.resume.is_some()
            && (self.task_id.is_some() || self.session_id.is_some() || self.objective.is_some())
        {
            return Err(SilynError::Permanent(
                "run --resume cannot be combined with --task, --session or --objective".to_owned(),
            ));
        }
        if self.task_id.is_some() && has_session_source {
            return Err(SilynError::Permanent(
                "run --task cannot be combined with --session or --objective".to_owned(),
            ));
        }
        if self.session_id.is_some() != self.objective.is_some() {
            return Err(SilynError::Permanent(
                "run --session and --objective must be used together".to_owned(),
            ));
        }
        if self.task_id.is_none() && self.resume.is_none() && self.session_id.is_none() {
            return Err(SilynError::Permanent(
                "run requires exactly one of --task, --resume, or --session with --objective"
                    .to_owned(),
            ));
        }
        if self
            .objective
            .as_deref()
            .is_some_and(|objective| objective.trim().is_empty())
        {
            return Err(SilynError::Permanent(
                "task objective must not be empty".to_owned(),
            ));
        }

        Ok(())
    }
}

/// W3 outcome verifier.
///
/// It never returns `Success`: without an expected answer there is no evidence
/// an honest checker could use. W4 supplies [`CompositeVerifier`] when the
/// caller passes `--expect-json`.
#[derive(Debug, Default, Clone, Copy)]
pub struct AnswerVerifier;

#[async_trait]
impl OutcomeVerifier for AnswerVerifier {
    async fn verify(&self, _run: RunSnapshot) -> Result<OutcomeEvidence, SilynError> {
        Ok(OutcomeEvidence {
            outcome: VerifiedOutcome::Unknown,
            evidence_refs: Vec::new(),
            confidence: 0.0,
        })
    }
}

impl App {
    /// Creates a task when `--session/--objective` is used, then runs it.
    ///
    /// A `--resume` target is validated before any provider work, so an unknown
    /// or terminal run fails without an API key lookup or an HTTP client.
    pub async fn run(&self, mut request: RunRequest) -> Result<RunOutcome, SilynError> {
        request.validate()?;
        if let Some(run_id) = request.resume {
            self.ensure_resumable(run_id).await?;
        }

        let provider: Arc<dyn ModelProvider> = if request.offline {
            Arc::new(MockProvider::new(vec![offline_response(
                &request.options.model,
            )]))
        } else {
            let config = ProviderConfig::from_env()?;
            request.options.price = config.price;
            let transport = ReqwestTransport::new(config.timeout)?;
            Arc::new(OpenAiCompatibleProvider::new(config, transport))
        };

        self.run_with_provider(request, provider).await
    }

    /// Runs a request against an injected provider.
    ///
    /// This is the test/API seam: callers can supply a deterministic
    /// [`MockProvider`](silyn_llm::MockProvider) without an API key or network.
    /// Memory retrieval, verifier selection and experience extraction are
    /// identical to [`run`](Self::run).
    pub async fn run_with_provider(
        &self,
        request: RunRequest,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<RunOutcome, SilynError> {
        request.validate()?;
        if let Some(run_id) = request.resume {
            self.ensure_resumable(run_id).await?;
        }

        let (agent_id, objective) = self.resolve_context(&request).await?;
        let memories = self.retrieve_memories(agent_id, &objective).await?;

        let mut options = request.options.clone();
        options.agent_id = Some(agent_id);
        options.memories = memories;

        let (runtime, options) = self.prepare(provider, options, request.expected_json.clone())?;
        let outcome = self.run_prepared(&runtime, &request, options).await?;
        self.record_experience(agent_id, &outcome).await?;
        Ok(outcome)
    }

    /// Resumes an existing run; refuses terminal runs.
    ///
    /// Uses the configured provider. Callers that need an injected provider use
    /// [`resume_with_provider`](Self::resume_with_provider).
    pub async fn resume(
        &self,
        run_id: RunId,
        mut options: RunOptions,
    ) -> Result<RunOutcome, SilynError> {
        self.ensure_resumable(run_id).await?;

        let config = ProviderConfig::from_env()?;
        options.price = config.price;
        let transport = ReqwestTransport::new(config.timeout)?;
        let provider: Arc<dyn ModelProvider> =
            Arc::new(OpenAiCompatibleProvider::new(config, transport));

        self.resume_with_provider(run_id, options, provider).await
    }

    /// Resumes a run against an injected provider.
    ///
    /// The agent is resolved from run -> task -> session, so memories stay
    /// scoped even when the caller only knows the run id.
    pub async fn resume_with_provider(
        &self,
        run_id: RunId,
        options: RunOptions,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<RunOutcome, SilynError> {
        self.ensure_resumable(run_id).await?;
        let request = RunRequest {
            task_id: None,
            session_id: None,
            objective: None,
            resume: Some(run_id),
            options,
            offline: false,
            expected_json: None,
        };
        self.run_with_provider(request, provider).await
    }

    /// Workspace directory that tool calls are confined to.
    #[must_use]
    pub fn workspace_dir(&self) -> std::path::PathBuf {
        self.home().join(WORKSPACE_DIR)
    }

    /// Stores one memory for an agent.
    ///
    /// The owner comes from the agent row, never from the caller, and the store
    /// refuses an agent owned by someone else. `scope` is limited to `agent` or
    /// `session` by the CLI; the store itself accepts any scope it is given.
    pub async fn add_memory(
        &self,
        agent_id: AgentId,
        summary: &str,
        content: &str,
        scope: MemoryScope,
        provenance: Provenance,
    ) -> Result<MemoryRecord, SilynError> {
        let agent = self.get_agent(agent_id).await?;
        let new = NewMemory {
            owner_user_id: agent.owner_id,
            agent_id,
            scope,
            memory_type: MemoryType::Semantic,
            content: content.to_owned(),
            summary: summary.to_owned(),
            source_event_ids: Vec::new(),
            provenance,
            confidence: DEFAULT_MEMORY_CONFIDENCE,
            importance: DEFAULT_MEMORY_IMPORTANCE,
            validity_from: None,
            validity_to: None,
            sensitivity: "private".to_owned(),
        };
        MemoryStore::new(self.storage().pool()).put(new).await
    }

    /// Lists active memories of one agent, newest first.
    pub async fn list_memories(
        &self,
        agent_id: AgentId,
    ) -> Result<Vec<MemoryHitRecord>, SilynError> {
        self.search_memories(agent_id, "", MEMORY_LIST_LIMIT).await
    }

    /// Full-text search over one agent's memories.
    ///
    /// The filter always carries both the owner and the agent, so agent A's
    /// memory can never be returned for agent B.
    pub async fn search_memories(
        &self,
        agent_id: AgentId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryHitRecord>, SilynError> {
        let agent = self.get_agent(agent_id).await?;
        let filter = MemoryScopeFilter {
            owner_user_id: agent.owner_id,
            agent_id,
            scopes: vec![MemoryScope::Agent, MemoryScope::Session],
            types: Vec::new(),
            include_expired: false,
        };
        MemoryStore::new(self.storage().pool())
            .search(&filter, query, limit)
            .await
    }

    /// Soft-deletes one memory after proving it belongs to this agent.
    ///
    /// A foreign or unknown id is a permanent "not found": the caller learns
    /// nothing about another agent's rows.
    pub async fn delete_memory(
        &self,
        agent_id: AgentId,
        memory_id: MemoryId,
    ) -> Result<(), SilynError> {
        let agent = self.get_agent(agent_id).await?;
        let store = MemoryStore::new(self.storage().pool());
        if store
            .get(&agent.owner_id, agent_id, memory_id)
            .await?
            .is_none()
        {
            return Err(SilynError::Permanent(format!(
                "memory {memory_id} not found"
            )));
        }
        store.soft_delete(memory_id, &agent.owner_id).await
    }

    /// Every experience of one agent, active first, then oldest first.
    pub async fn list_experiences(
        &self,
        agent_id: AgentId,
    ) -> Result<Vec<ExperienceRecord>, SilynError> {
        ExperienceStore::new(self.storage().pool())
            .list(agent_id)
            .await
    }

    /// Runs a validated request against an already-built runtime.
    ///
    /// The run is created first so the cancel-file path can name its id; this is
    /// what makes `runs cancel` from another process visible to the loop.
    async fn run_prepared(
        &self,
        runtime: &Runtime,
        request: &RunRequest,
        options: RunOptions,
    ) -> Result<RunOutcome, SilynError> {
        if let Some(run_id) = request.resume {
            let options = RunOptions {
                cancel_file: Some(self.cancel_request_path(run_id)),
                ..options
            };
            return runtime.resume(run_id, options).await;
        }

        let task_id = self.resolve_task(request).await?;
        let record = runtime.create_run(task_id, &options).await?;
        let options = RunOptions {
            cancel_file: Some(self.cancel_request_path(record.id)),
            ..options
        };
        runtime.execute(record.id, options).await
    }

    /// Returns the task to run, creating it for the session form.
    async fn resolve_task(&self, request: &RunRequest) -> Result<TaskId, SilynError> {
        match (
            request.task_id,
            request.session_id,
            request.objective.as_deref(),
        ) {
            (Some(task_id), _, _) => Ok(task_id),
            (None, Some(session_id), Some(objective)) => {
                Ok(self.create_task(session_id, objective).await?.id)
            }
            _ => Err(SilynError::Permanent(
                "run requires exactly one of --task, --resume, or --session with --objective"
                    .to_owned(),
            )),
        }
    }

    /// Resolves the agent and objective for a request.
    ///
    /// Sources: task -> session -> agent, run -> task -> session -> agent, or
    /// the explicit session plus its objective. The objective is used for memory
    /// retrieval before a task is created.
    async fn resolve_context(&self, request: &RunRequest) -> Result<(AgentId, String), SilynError> {
        if let Some(task_id) = request.task_id {
            let task = self.get_task(task_id).await?;
            let agent_id = self.agent_for_session(task.session_id).await?;
            return Ok((agent_id, task.objective));
        }
        if let Some(run_id) = request.resume {
            let run = self.get_run(run_id).await?;
            let task = self.get_task(run.task_id).await?;
            let agent_id = self.agent_for_session(task.session_id).await?;
            return Ok((agent_id, task.objective));
        }
        match (request.session_id, request.objective.as_deref()) {
            (Some(session_id), Some(objective)) => Ok((
                self.agent_for_session(session_id).await?,
                objective.to_owned(),
            )),
            _ => Err(SilynError::Permanent(
                "run requires exactly one of --task, --resume, or --session with --objective"
                    .to_owned(),
            )),
        }
    }

    /// Retrieves agent-scoped memories relevant to the objective.
    ///
    /// Both the owner and the agent are part of the filter; the search can never
    /// widen to another agent's rows.
    async fn retrieve_memories(
        &self,
        agent_id: AgentId,
        objective: &str,
    ) -> Result<Vec<MemorySnippet>, SilynError> {
        let agent = self.get_agent(agent_id).await?;
        let filter = MemoryScopeFilter {
            owner_user_id: agent.owner_id,
            agent_id,
            scopes: vec![MemoryScope::Agent, MemoryScope::Session],
            types: Vec::new(),
            include_expired: false,
        };
        let hits = MemoryStore::new(self.storage().pool())
            .search(&filter, objective, MEMORY_LIMIT)
            .await?;
        Ok(hits.into_iter().map(snippet_from_hit).collect())
    }

    /// Rejects unknown and terminal runs before provider work.
    async fn ensure_resumable(&self, run_id: RunId) -> Result<(), SilynError> {
        let run = self.get_run(run_id).await?;
        if run.status.is_terminal() {
            return Err(SilynError::Permanent(format!(
                "run {run_id} is already {} and cannot be resumed",
                status_name(run.status)
            )));
        }
        Ok(())
    }

    async fn get_task(&self, task_id: TaskId) -> Result<TaskRecord, SilynError> {
        TaskRepository::new(self.storage().pool())
            .get(task_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("task {task_id} not found")))
    }

    async fn get_agent(&self, agent_id: AgentId) -> Result<AgentRecord, SilynError> {
        AgentRepository::new(self.storage().pool())
            .get(agent_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("agent {agent_id} not found")))
    }

    async fn agent_for_session(&self, session_id: SessionId) -> Result<AgentId, SilynError> {
        let session = SessionRepository::new(self.storage().pool())
            .get(session_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("session {session_id} not found")))?;
        Ok(session.agent_id)
    }

    /// Builds the runtime and the effective options for one request.
    ///
    /// The service owns the tool root and the verifier so a caller cannot bypass
    /// them. An expected answer selects `CompositeVerifier` with the workspace
    /// as artifact root; otherwise the W3 `AnswerVerifier` stays in place.
    fn prepare(
        &self,
        provider: Arc<dyn ModelProvider>,
        options: RunOptions,
        expected_json: Option<Value>,
    ) -> Result<(Runtime, RunOptions), SilynError> {
        let workspace = self.workspace_dir();
        std::fs::create_dir_all(&workspace).map_err(|error| {
            SilynError::Storage(format!(
                "cannot create workspace directory {}: {error}",
                workspace.display()
            ))
        })?;

        let audit = Arc::new(MemoryAuditSink::new());
        let tools = Arc::new(ToolBroker::new(&workspace, audit)?);

        let verifier: Arc<dyn OutcomeVerifier> = match expected_json {
            Some(expected) => {
                let checker = Arc::new(ExpectedAnswerChecker::new(expected));
                Arc::new(
                    CompositeVerifier::new(vec![checker]).with_artifact_root(workspace.clone()),
                )
            }
            None => Arc::new(AnswerVerifier),
        };

        let runtime = Runtime::new(self.storage().clone(), provider, tools, verifier);
        Ok((runtime, options))
    }
}

fn snippet_from_hit(hit: MemoryHitRecord) -> MemorySnippet {
    MemorySnippet {
        memory_id: hit.id.to_string(),
        summary: hit.summary,
        content: hit.content,
        provenance: provenance_name(hit.provenance),
        confidence_milli: confidence_milli(hit.confidence),
    }
}

fn provenance_name(provenance: Provenance) -> String {
    serde_json::to_value(provenance)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn confidence_milli(confidence: f32) -> u32 {
    let scaled = (confidence.clamp(0.0, 1.0) * 1000.0).round();
    if scaled.is_finite() { scaled as u32 } else { 0 }
}

/// The single response the offline provider returns.
fn offline_response(model: &str) -> ModelResponse {
    let content = serde_json::json!({
        "action": "final",
        "answer": OFFLINE_ANSWER,
    })
    .to_string();
    ModelResponse {
        content,
        model: model.to_owned(),
        input_tokens: 0,
        output_tokens: 0,
    }
}

/// Names a run status the way the JSON wire format does.
fn status_name(status: RunStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use serde_json::json;
    use silyn_core::contracts::{ModelProvider, ModelRequest, ModelResponse};
    use silyn_core::ids::RunId;
    use silyn_growth::GENERIC_LESSON;
    use silyn_runtime::{
        BudgetSpec, ContextBuilder, MemoryAuditSink, PROTOCOL_PROMPT, RunOptions, ToolBroker,
    };

    use super::*;

    fn options() -> RunOptions {
        RunOptions {
            model: "mock".to_owned(),
            capabilities: Vec::new(),
            budget: BudgetSpec::default(),
            price: None,
            cancel_file: None,
            memories: Vec::new(),
            agent_id: None,
        }
    }

    async fn app_in_tempdir() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = App::open(dir.path().join("home")).await.expect("open app");
        (dir, Arc::new(app))
    }

    async fn agent_and_session(app: &App) -> (AgentId, SessionId) {
        let agent = app.create_agent("local-user").await.expect("create agent");
        let session = app.create_session(agent.id).await.expect("create session");
        (agent.id, session.id)
    }

    async fn session(app: &App) -> SessionId {
        agent_and_session(app).await.1
    }

    fn session_request(session_id: SessionId, objective: &str) -> RunRequest {
        RunRequest {
            task_id: None,
            session_id: Some(session_id),
            objective: Some(objective.to_owned()),
            resume: None,
            options: options(),
            offline: true,
            expected_json: None,
        }
    }

    fn resume_request(run_id: RunId) -> RunRequest {
        RunRequest {
            task_id: None,
            session_id: None,
            objective: None,
            resume: Some(run_id),
            options: options(),
            offline: true,
            expected_json: None,
        }
    }

    fn final_response(content: &str) -> ModelResponse {
        ModelResponse {
            content: json!({ "action": "final", "answer": content }).to_string(),
            model: "mock".to_owned(),
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn mock(responses: Vec<ModelResponse>) -> Arc<MockProvider> {
        Arc::new(MockProvider::new(responses))
    }

    #[test]
    fn request_requires_exactly_one_source() {
        let error = RunRequest {
            task_id: None,
            session_id: None,
            objective: None,
            resume: None,
            options: options(),
            offline: true,
            expected_json: None,
        }
        .validate()
        .expect_err("no source");
        assert!(matches!(error, SilynError::Permanent(_)));
        assert!(
            error.to_string().contains("requires exactly one of"),
            "{error}"
        );

        let error = RunRequest {
            task_id: Some(TaskId::new()),
            resume: Some(RunId::new()),
            ..session_request(SessionId::new(), "do the thing")
        }
        .validate()
        .expect_err("two sources");
        assert!(matches!(error, SilynError::Permanent(_)));
        assert!(error.to_string().contains("--resume"), "{error}");

        let error = RunRequest {
            task_id: Some(TaskId::new()),
            session_id: None,
            objective: None,
            resume: None,
            options: options(),
            offline: true,
            expected_json: None,
        }
        .validate();
        assert!(error.is_ok(), "a task id alone is valid: {error:?}");
    }

    #[test]
    fn session_and_objective_must_be_paired_and_non_empty() {
        let error = RunRequest {
            task_id: None,
            session_id: Some(SessionId::new()),
            objective: None,
            resume: None,
            options: options(),
            offline: true,
            expected_json: None,
        }
        .validate()
        .expect_err("session without objective");
        assert!(error.to_string().contains("used together"), "{error}");

        let error = RunRequest {
            task_id: None,
            session_id: None,
            objective: Some("do the thing".to_owned()),
            resume: None,
            options: options(),
            offline: true,
            expected_json: None,
        }
        .validate()
        .expect_err("objective without session");
        assert!(error.to_string().contains("used together"), "{error}");

        let error = session_request(SessionId::new(), "   ")
            .validate()
            .expect_err("empty objective");
        assert!(error.to_string().contains("objective"), "{error}");
    }

    #[test]
    fn resume_cannot_be_combined_with_other_sources() {
        for request in [
            RunRequest {
                task_id: Some(TaskId::new()),
                ..session_request(SessionId::new(), "do the thing")
            },
            RunRequest {
                resume: Some(RunId::new()),
                ..session_request(SessionId::new(), "do the thing")
            },
        ] {
            let error = request.validate().expect_err("conflicting sources");
            assert!(matches!(error, SilynError::Permanent(_)));
        }
    }

    #[tokio::test]
    async fn answer_verifier_never_claims_success() {
        let evidence = AnswerVerifier
            .verify(RunSnapshot {
                run_id: RunId::new(),
                objective: "summarize the csv".to_owned(),
                artifact_refs: vec!["summary.md".to_owned()],
            })
            .await
            .expect("verify");

        assert_eq!(evidence.outcome, VerifiedOutcome::Unknown);
        assert_ne!(evidence.outcome, VerifiedOutcome::Success);
        assert!(evidence.evidence_refs.is_empty());
        assert_eq!(evidence.confidence, 0.0);
    }

    #[tokio::test]
    async fn offline_run_completes_and_is_recorded() {
        let (_dir, app) = app_in_tempdir().await;
        let session_id = session(&app).await;

        let outcome = app
            .run(session_request(session_id, "summarize the csv"))
            .await
            .expect("offline run");

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.outcome, Some(VerifiedOutcome::Unknown));
        assert_eq!(outcome.final_answer.as_deref(), Some(OFFLINE_ANSWER));
        assert_eq!(outcome.steps, 1);
        assert_eq!(outcome.reason, None);
        assert!(outcome.memory_refs.is_empty());

        let run = app.get_run(outcome.run_id).await.expect("stored run");
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.model_id, "mock");
        assert!(run.started_at.is_some());
        assert!(run.finished_at.is_some());
    }

    #[tokio::test]
    async fn unknown_and_terminal_runs_are_not_resumed() {
        let (_dir, app) = app_in_tempdir().await;

        let error = app
            .run(resume_request(RunId::new()))
            .await
            .expect_err("unknown run");
        assert!(matches!(error, SilynError::Permanent(_)));
        assert!(error.to_string().contains("not found"), "{error}");

        let session_id = session(&app).await;
        let completed = app
            .run(session_request(session_id, "summarize the csv"))
            .await
            .expect("offline run");

        let error = app
            .run(resume_request(completed.run_id))
            .await
            .expect_err("terminal run");
        assert!(matches!(error, SilynError::Permanent(_)));
        assert!(error.to_string().contains("already completed"), "{error}");
    }

    #[tokio::test]
    async fn budget_exhaustion_fails_the_run_without_a_terminal_error() {
        let (_dir, app) = app_in_tempdir().await;
        let session_id = session(&app).await;

        let outcome = app
            .run(RunRequest {
                options: RunOptions {
                    budget: BudgetSpec {
                        max_steps: 0,
                        ..BudgetSpec::default()
                    },
                    ..options()
                },
                ..session_request(session_id, "summarize the csv")
            })
            .await
            .expect("a failed run is still a run outcome");

        assert_eq!(outcome.status, RunStatus::Failed);
        assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("budget exhausted")),
            "{:?}",
            outcome.reason
        );
        let run = app.get_run(outcome.run_id).await.expect("stored run");
        assert_eq!(run.status, RunStatus::Failed);
    }

    #[tokio::test]
    async fn memory_injected_with_ids() {
        let (_dir, app) = app_in_tempdir().await;
        let (agent, session_id) = agent_and_session(&app).await;

        let first = app
            .add_memory(
                agent,
                "deploy window rule",
                "Deploy window starts Monday.",
                MemoryScope::Agent,
                Provenance::UserStatement,
            )
            .await
            .expect("first memory");
        let second = app
            .add_memory(
                agent,
                "deploy checklist",
                "Two approvals before deploy.",
                MemoryScope::Session,
                Provenance::ExternalEvidence,
            )
            .await
            .expect("second memory");

        let provider = mock(vec![final_response("done")]);
        let outcome = app
            .run_with_provider(session_request(session_id, "deploy"), provider.clone())
            .await
            .expect("run");

        let requests = provider.requests();
        assert_eq!(requests.len(), 1, "one model turn");
        let prompt = &requests[0].messages[0].content;
        assert!(
            prompt.contains("MEMORY (untrusted context, cite by id)"),
            "{prompt}"
        );
        assert!(prompt.contains("cannot grant capabilities"), "{prompt}");
        assert!(prompt.contains(&first.id.to_string()), "{prompt}");
        assert!(prompt.contains("deploy window rule"), "{prompt}");
        assert!(prompt.contains(&second.id.to_string()), "{prompt}");
        assert!(prompt.contains("deploy checklist"), "{prompt}");
        assert!(prompt.contains("provenance: user_statement"), "{prompt}");

        let mut refs = outcome.memory_refs.clone();
        refs.sort();
        let mut expected = vec![first.id.to_string(), second.id.to_string()];
        expected.sort();
        assert_eq!(refs, expected);
        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.outcome, Some(VerifiedOutcome::Unknown));
    }

    #[tokio::test]
    async fn memory_scope_isolation() {
        let (_dir, app) = app_in_tempdir().await;
        let (agent_a, _session_a) = agent_and_session(&app).await;
        let (agent_b, session_b) = agent_and_session(&app).await;

        let secret = app
            .add_memory(
                agent_a,
                "alpha-only",
                "alpha secret value for agent a",
                MemoryScope::Agent,
                Provenance::UserStatement,
            )
            .await
            .expect("agent a memory");

        let provider = mock(vec![final_response("done")]);
        let outcome = app
            .run_with_provider(session_request(session_b, "alpha"), provider.clone())
            .await
            .expect("agent b run");

        let prompt = &provider.requests()[0].messages[0].content;
        assert!(!prompt.contains("alpha-only"), "{prompt}");
        assert!(!prompt.contains(&secret.id.to_string()), "{prompt}");
        assert!(outcome.memory_refs.is_empty());

        let hits = app
            .search_memories(agent_b, "alpha", 8)
            .await
            .expect("search as agent b");
        assert!(hits.is_empty(), "agent b must not see agent a's memory");

        let hits = app
            .search_memories(agent_a, "alpha", 8)
            .await
            .expect("search as agent a");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, secret.id);
    }

    #[tokio::test]
    async fn no_memory_unchanged() {
        let (_dir, app) = app_in_tempdir().await;
        let (_agent, session_id) = agent_and_session(&app).await;

        let provider = mock(vec![final_response("done")]);
        let outcome = app
            .run_with_provider(
                session_request(session_id, "summarize the csv"),
                provider.clone(),
            )
            .await
            .expect("run");

        let prompt = &provider.requests()[0].messages[0].content;
        assert_eq!(prompt, PROTOCOL_PROMPT);
        assert_eq!(
            prompt,
            &ContextBuilder::system_prompt("summarize the csv", &[])
        );
        assert!(outcome.memory_refs.is_empty());
    }

    #[tokio::test]
    async fn failed_verification_creates_one_provisional() {
        let (_dir, app) = app_in_tempdir().await;
        let (agent, session_id) = agent_and_session(&app).await;

        let provider = mock(vec![final_response("{\"answer\":\"actual\"}")]);
        let outcome = app
            .run_with_provider(
                RunRequest {
                    expected_json: Some(json!({ "answer": "expected" })),
                    ..session_request(session_id, "compute the answer")
                },
                provider,
            )
            .await
            .expect("run");

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.outcome, Some(VerifiedOutcome::Failed));

        let experiences = app.list_experiences(agent).await.expect("experiences");
        assert_eq!(experiences.len(), 1, "{experiences:?}");
        assert_eq!(experiences[0].lesson, GENERIC_LESSON);
        assert_eq!(
            experiences[0].evidence_status,
            silyn_growth::EvidenceStatus::Provisional
        );
        assert_eq!(experiences[0].source_run_ids, vec![outcome.run_id]);
    }

    #[tokio::test]
    async fn unknown_creates_nothing() {
        let (_dir, app) = app_in_tempdir().await;
        let (agent, session_id) = agent_and_session(&app).await;

        let provider = mock(vec![final_response("plain prose")]);
        let outcome = app
            .run_with_provider(session_request(session_id, "summarize"), provider)
            .await
            .expect("run");

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(outcome.outcome, Some(VerifiedOutcome::Unknown));
        assert!(
            app.list_experiences(agent)
                .await
                .expect("experiences")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cancelled_creates_nothing() {
        let (_dir, app) = app_in_tempdir().await;
        let (agent, _session_id) = agent_and_session(&app).await;

        let outcome = RunOutcome {
            run_id: RunId::new(),
            status: RunStatus::Cancelled,
            outcome: None,
            final_answer: None,
            reason: Some("cancelled by operator".to_owned()),
            steps: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            memory_refs: Vec::new(),
        };

        assert!(
            app.record_experience(agent, &outcome)
                .await
                .expect("cancelled is not an error")
                .is_none()
        );
        assert!(
            app.list_experiences(agent)
                .await
                .expect("experiences")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn failures_deduplicate() {
        let (_dir, app) = app_in_tempdir().await;
        let (agent, session_id) = agent_and_session(&app).await;

        let mut run_ids = Vec::new();
        for _ in 0..2 {
            let provider = mock(vec![final_response("{\"answer\":\"actual\"}")]);
            let outcome = app
                .run_with_provider(
                    RunRequest {
                        expected_json: Some(json!({ "answer": "expected" })),
                        ..session_request(session_id, "compute the answer")
                    },
                    provider,
                )
                .await
                .expect("run");
            assert_eq!(outcome.outcome, Some(VerifiedOutcome::Failed));
            run_ids.push(outcome.run_id);
        }

        let experiences = app.list_experiences(agent).await.expect("experiences");
        assert_eq!(experiences.len(), 1, "{experiences:?}");
        assert_eq!(experiences[0].lesson, GENERIC_LESSON);
        assert_eq!(experiences[0].source_run_ids, run_ids);
    }

    #[tokio::test]
    async fn provider_injection_uses_mock_without_network_or_key() {
        let (_dir, app) = app_in_tempdir().await;
        let (_agent, session_id) = agent_and_session(&app).await;

        // MockProvider answers from memory only; nothing here reads an API key
        // or opens a socket.
        let provider = mock(vec![final_response("offline by injection")]);
        let outcome = app
            .run_with_provider(
                RunRequest {
                    offline: false,
                    ..session_request(session_id, "do the thing")
                },
                provider,
            )
            .await
            .expect("injected provider run");

        assert_eq!(outcome.status, RunStatus::Completed);
        assert_eq!(
            outcome.final_answer.as_deref(),
            Some("offline by injection")
        );
    }

    /// Provider that never answers until a cancel file appears.
    struct CancelWaitingProvider {
        started: Arc<AtomicBool>,
        signal_dir: std::path::PathBuf,
    }

    impl CancelWaitingProvider {
        async fn wait_for_cancel_file(path: &Path) -> bool {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_some()) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        }
    }

    #[async_trait]
    impl ModelProvider for CancelWaitingProvider {
        async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse, SilynError> {
            self.started.store(true, Ordering::SeqCst);
            if Self::wait_for_cancel_file(&self.signal_dir).await {
                // Never answer. The only way out of this call is the run loop's
                // cancel branch, which proves it polls the documented path.
                std::future::pending::<()>().await;
            }
            Err(SilynError::Permanent(
                "test provider timed out waiting for a cancel file".to_owned(),
            ))
        }
    }

    #[tokio::test]
    async fn run_passes_the_documented_cancel_file_to_the_loop() {
        let (dir, app) = app_in_tempdir().await;
        let session_id = session(&app).await;

        let signal_dir = dir.path().join("home").join(crate::RUN_SIGNAL_DIR);
        std::fs::create_dir_all(&signal_dir).expect("signal directory");
        let provider = Arc::new(CancelWaitingProvider {
            started: Arc::new(AtomicBool::new(false)),
            signal_dir,
        });
        let started = Arc::clone(&provider.started);

        let workspace = app.workspace_dir();
        std::fs::create_dir_all(&workspace).expect("workspace");
        let tools = Arc::new(
            ToolBroker::new(&workspace, Arc::new(MemoryAuditSink::new())).expect("tool broker"),
        );
        let runtime = Runtime::new(
            app.storage().clone(),
            provider,
            tools,
            Arc::new(AnswerVerifier),
        );

        let request = session_request(session_id, "summarize the csv");
        let app_for_run = Arc::clone(&app);
        let runner = tokio::spawn(async move {
            let options = request.options.clone();
            app_for_run.run_prepared(&runtime, &request, options).await
        });

        tokio::time::timeout(Duration::from_secs(10), async {
            while !started.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the model call must start");

        // The run id is discoverable while the model call is still blocked.
        let runs = app.list_runs().await.expect("list runs");
        let run = runs.first().expect("the run row exists").clone();
        assert_eq!(run.status, RunStatus::Running);

        // Only a cancel file at `<home>/run/<run_id>.cancel` can unblock this
        // provider, so observing the cancellation proves the service wired that
        // exact path into `RunOptions::cancel_file`.
        let request = app.request_cancel(run.id).await.expect("cancel request");
        assert_eq!(request.path, app.cancel_request_path(run.id));
        assert!(!request.already_requested);

        let outcome = tokio::time::timeout(Duration::from_secs(10), runner)
            .await
            .expect("the run must observe the cancel file")
            .expect("join")
            .expect("run outcome");

        assert_eq!(outcome.run_id, run.id);
        assert_eq!(outcome.status, RunStatus::Cancelled);
        assert!(
            outcome
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("cancel")),
            "{:?}",
            outcome.reason
        );
        let stored = app.get_run(run.id).await.expect("stored run");
        assert_eq!(stored.status, RunStatus::Cancelled);
    }
}
