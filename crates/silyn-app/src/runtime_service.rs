//! Run service: turns a validated request into a runtime execution.
//!
//! This module is the single place where the app layer decides *how* a run
//! executes: which provider answers, which directory the tools are confined to,
//! which verifier judges the result and which cancel file the loop polls. The
//! CLI only parses flags and formats results; `silyn-runtime` owns the loop.
//!
//! Two invariants matter for operators:
//!
//! - the run id is known before execution starts, so the cancel file can be
//!   `home/run/<run_id>.cancel` (the same path `runs cancel` writes);
//! - the W3 verifier never claims success. Every run is `Unknown` until the W4
//!   verifier lands.

use std::sync::Arc;

use async_trait::async_trait;
use silyn_core::SilynError;
use silyn_core::contracts::{
    ModelProvider, ModelResponse, OutcomeEvidence, OutcomeVerifier, RunSnapshot,
};
use silyn_core::event::VerifiedOutcome;
use silyn_core::ids::{RunId, SessionId, TaskId};
use silyn_core::state::{LifecycleState, RunStatus};
use silyn_llm::{MockProvider, OpenAiCompatibleProvider, ProviderConfig, ReqwestTransport};
use silyn_runtime::{MemoryAuditSink, RunOptions, RunOutcome, Runtime, ToolBroker};

use crate::app::App;

/// Directory under the app home that tool calls are confined to.
pub const WORKSPACE_DIR: &str = "workspace";

/// Final answer returned by the offline provider.
pub const OFFLINE_ANSWER: &str = "offline mock run";

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
    /// Model, capabilities, budgets and cancel file. The service fills `price`
    /// from the provider configuration and `cancel_file` from the run id.
    pub options: RunOptions,
    /// Use the deterministic offline provider: no API key, no network. The model
    /// name is still recorded on the run.
    pub offline: bool,
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
/// It never returns `Success`: the run loop passes a [`RunSnapshot`] (objective
/// and artifact references), not the final answer, so there is no evidence a
/// heuristic could honestly promote. A non-empty final answer and an empty one
/// both map to [`VerifiedOutcome::Unknown`]. W4 replaces this type.
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
    pub async fn run(&self, request: RunRequest) -> Result<RunOutcome, SilynError> {
        request.validate()?;
        if let Some(run_id) = request.resume {
            self.ensure_resumable(run_id).await?;
        }

        let (runtime, options) = self.prepare(request.offline, &request.options)?;
        self.run_prepared(&runtime, &request, options).await
    }

    /// Resumes an existing run; refuses terminal runs.
    ///
    /// Uses the configured provider. Callers that need the offline provider go
    /// through [`run`](Self::run) with [`RunRequest::offline`], which shares the
    /// same implementation.
    pub async fn resume(
        &self,
        run_id: RunId,
        options: RunOptions,
    ) -> Result<RunOutcome, SilynError> {
        self.ensure_resumable(run_id).await?;
        let (runtime, options) = self.prepare(false, &options)?;
        let request = RunRequest {
            task_id: None,
            session_id: None,
            objective: None,
            resume: Some(run_id),
            options,
            offline: false,
        };
        self.run_prepared(&runtime, &request, request.options.clone())
            .await
    }

    /// Workspace directory that tool calls are confined to.
    #[must_use]
    pub fn workspace_dir(&self) -> std::path::PathBuf {
        self.home().join(WORKSPACE_DIR)
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

    /// Rejects unknown and terminal runs before any provider work.
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

    /// Builds the runtime and the effective options for one request.
    ///
    /// The service owns provider selection, price lookup, the tool root and the
    /// verifier so a caller cannot bypass them. Offline mode never reads an API
    /// key and never builds an HTTP client.
    fn prepare(
        &self,
        offline: bool,
        options: &RunOptions,
    ) -> Result<(Runtime, RunOptions), SilynError> {
        let mut options = options.clone();

        let provider: Arc<dyn ModelProvider> = if offline {
            Arc::new(MockProvider::new(vec![offline_response(&options.model)]))
        } else {
            let config = ProviderConfig::from_env()?;
            options.price = config.price;
            let transport = ReqwestTransport::new(config.timeout)?;
            Arc::new(OpenAiCompatibleProvider::new(config, transport))
        };

        let workspace = self.workspace_dir();
        std::fs::create_dir_all(&workspace).map_err(|error| {
            SilynError::Storage(format!(
                "cannot create workspace directory {}: {error}",
                workspace.display()
            ))
        })?;

        let audit = Arc::new(MemoryAuditSink::new());
        let tools = Arc::new(ToolBroker::new(&workspace, audit)?);
        let runtime = Runtime::new(
            self.storage().clone(),
            provider,
            tools,
            Arc::new(AnswerVerifier),
        );

        Ok((runtime, options))
    }
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
    use silyn_core::contracts::{ModelProvider, ModelRequest, ModelResponse};
    use silyn_core::ids::RunId;
    use silyn_runtime::{BudgetSpec, MemoryAuditSink, RunOptions, ToolBroker};

    use super::*;

    fn options() -> RunOptions {
        RunOptions {
            model: "mock".to_owned(),
            capabilities: Vec::new(),
            budget: BudgetSpec::default(),
            price: None,
            cancel_file: None,
        }
    }

    async fn app_in_tempdir() -> (tempfile::TempDir, Arc<App>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = App::open(dir.path().join("home")).await.expect("open app");
        (dir, Arc::new(app))
    }

    async fn session(app: &App) -> SessionId {
        let agent = app.create_agent("local-user").await.expect("create agent");
        app.create_session(agent.id)
            .await
            .expect("create session")
            .id
    }

    fn session_request(session_id: SessionId, objective: &str) -> RunRequest {
        RunRequest {
            task_id: None,
            session_id: Some(session_id),
            objective: Some(objective.to_owned()),
            resume: None,
            options: options(),
            offline: true,
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
        }
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
