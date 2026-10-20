//! Bounded, resumable single-agent run loop.
//!
//! One iteration either calls the model or resolves the pending tool action.
//! Every externally visible step is checkpointed before the side effect and
//! recorded in the tool ledger, so a restart can decide between reuse, failure
//! and explicit indeterminate resolution instead of replaying blindly.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use silyn_core::SilynError;
use silyn_core::contracts::{
    AuthorizedToolContext, Message, ModelProvider, ModelRequest, ModelResponse, OutcomeVerifier,
    Role, RunSnapshot, ToolCall, ToolExecutor, ToolResult,
};
use silyn_core::event::{EventEnvelope, RunEvent, VerifiedOutcome};
use silyn_core::ids::{AgentId, PolicyVersionId, RunId, StepId, TaskId};
use silyn_core::state::{LifecycleState, RunStatus, TaskStatus};
use silyn_llm::Price;
use silyn_storage::Storage;
use silyn_storage::checkpoint::{CheckpointRecord, CheckpointStore};
use silyn_storage::journal::Journal;
use silyn_storage::ledger::{ToolCallRecord, ToolCallState, ToolLedger};
use silyn_storage::models::{RunRecord, StepRecord, TaskRecord};
use silyn_storage::repo::{RunRepository, StepRepository, TaskRepository};

use crate::budget::{BudgetError, BudgetSpec, BudgetTracker};
use crate::context::{ContextBuilder, MemorySnippet};
use crate::control::RunControl;
use crate::detect::{LoopDetector, fingerprint_tool_call};
use crate::retry::RetryPolicy;
use crate::state::{PendingAction, RUN_STATE_SCHEMA_VERSION, RunState};
use crate::tools::{TOOL_DATA_PROFILE, TOOL_FS_LIST, TOOL_FS_READ};

/// Options for one execution attempt.
///
/// `Default` exists so callers that only care about a few fields can use
/// `..RunOptions::default()`; new fields stay backward compatible.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub model: String,
    pub capabilities: Vec<String>,
    pub budget: BudgetSpec,
    pub price: Option<Price>,
    /// When set, the loop polls this file and treats its appearance as a
    /// cancellation request from another process.
    pub cancel_file: Option<PathBuf>,
    /// Untrusted memories retrieved by the app layer. Empty means the system
    /// prompt is exactly the pre-W4 protocol text.
    pub memories: Vec<MemorySnippet>,
    /// Agent the memories belong to. Recorded for call-site clarity; the app
    /// layer is responsible for keeping the snippets scoped to this agent.
    pub agent_id: Option<AgentId>,
}

/// Result of one run. `status` is terminal: completed, failed or cancelled.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunOutcome {
    pub run_id: RunId,
    pub status: RunStatus,
    pub outcome: Option<VerifiedOutcome>,
    pub final_answer: Option<String>,
    pub reason: Option<String>,
    pub steps: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    /// Memories that were rendered into the system prompt. This is provenance
    /// for the context, not a claim that they caused the outcome.
    pub memory_refs: Vec<String>,
}

/// The single-agent kernel.
pub struct Runtime {
    storage: Storage,
    provider: Arc<dyn ModelProvider>,
    tools: Arc<dyn ToolExecutor>,
    verifier: Arc<dyn OutcomeVerifier>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryMode {
    Execute,
    Resume,
}

impl Runtime {
    #[must_use]
    pub fn new(
        storage: Storage,
        provider: Arc<dyn ModelProvider>,
        tools: Arc<dyn ToolExecutor>,
        verifier: Arc<dyn OutcomeVerifier>,
    ) -> Self {
        Self {
            storage,
            provider,
            tools,
            verifier,
        }
    }

    /// Loads the task, creates a fresh run row and executes it.
    pub async fn run_task(
        &self,
        task_id: TaskId,
        options: RunOptions,
    ) -> Result<RunOutcome, SilynError> {
        let record = self.create_run(task_id, &options).await?;
        self.run_existing(record.id, options, RecoveryMode::Execute)
            .await
    }

    /// Creates a run row for `task_id` in the `created` state and appends
    /// `RunCreated`, without executing anything.
    ///
    /// This exists so a caller can learn the run id before execution starts and
    /// pass that run's cross-process cancel file through
    /// [`RunOptions::cancel_file`]. The caller is responsible for calling
    /// [`execute`](Self::execute) afterwards; [`run_task`](Self::run_task) does
    /// both in one step.
    ///
    /// A missing task, a terminal task or an invalid budget configuration is a
    /// permanent error and creates no row.
    pub async fn create_run(
        &self,
        task_id: TaskId,
        options: &RunOptions,
    ) -> Result<RunRecord, SilynError> {
        // Fail before creating a row when the budget configuration is invalid.
        BudgetTracker::new(options.budget.clone(), options.price)?;

        let task = TaskRepository::new(self.storage.pool())
            .get(task_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("task {task_id} not found")))?;
        if task.status.is_terminal() {
            return Err(SilynError::Permanent(format!(
                "task {task_id} is already {}",
                task_status_label(task.status)
            )));
        }

        let run_id = RunId::new();
        let policy_version = PolicyVersionId::new();
        let now = Utc::now();
        let record = RunRecord {
            id: run_id,
            task_id,
            policy_version,
            memory_snapshot_version: None,
            model_id: options.model.clone(),
            status: RunStatus::Created,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            started_at: None,
            finished_at: None,
            created_at: now,
        };
        RunRepository::new(self.storage.pool())
            .insert(&record)
            .await?;

        self.append_event(
            run_id,
            &RunEvent::RunCreated {
                task_id,
                policy_version,
            },
        )
        .await?;

        Ok(record)
    }

    /// Starts a run that has not begun yet. Running runs must go through
    /// [`resume`](Self::resume).
    pub async fn execute(
        &self,
        run_id: RunId,
        options: RunOptions,
    ) -> Result<RunOutcome, SilynError> {
        self.run_existing(run_id, options, RecoveryMode::Execute)
            .await
    }

    /// Continues a created or interrupted run from its newest checkpoint.
    pub async fn resume(
        &self,
        run_id: RunId,
        options: RunOptions,
    ) -> Result<RunOutcome, SilynError> {
        self.run_existing(run_id, options, RecoveryMode::Resume)
            .await
    }

    async fn run_existing(
        &self,
        run_id: RunId,
        options: RunOptions,
        mode: RecoveryMode,
    ) -> Result<RunOutcome, SilynError> {
        let runs = RunRepository::new(self.storage.pool());
        let run = runs
            .get(run_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("run {run_id} not found")))?;

        if run.status.is_terminal() {
            return Err(SilynError::Permanent(format!(
                "run {run_id} is already {}",
                run_status_label(run.status)
            )));
        }
        match mode {
            RecoveryMode::Execute if run.status != RunStatus::Created => {
                return Err(SilynError::Permanent(format!(
                    "run {run_id} is already {}; use resume",
                    run_status_label(run.status)
                )));
            }
            RecoveryMode::Resume
                if !matches!(run.status, RunStatus::Created | RunStatus::Running) =>
            {
                return Err(SilynError::Permanent(format!(
                    "run {run_id} cannot be resumed from {}",
                    run_status_label(run.status)
                )));
            }
            _ => {}
        }

        let task = TaskRepository::new(self.storage.pool())
            .get(run.task_id)
            .await?
            .ok_or_else(|| SilynError::Permanent(format!("task {} not found", run.task_id)))?;

        let checkpoints = CheckpointStore::new(self.storage.pool());
        let latest = checkpoints.latest(run_id, RUN_STATE_SCHEMA_VERSION).await?;
        let checkpoint_seq = latest.as_ref().map_or(0, |record| record.seq);
        let state = match &latest {
            Some(record) => {
                serde_json::from_str::<RunState>(&record.state_ref).map_err(|error| {
                    SilynError::Permanent(format!("checkpoint state is invalid: {error}"))
                })?
            }
            None => RunState::default(),
        };

        let mut tracker = BudgetTracker::new(options.budget.clone(), options.price)?;
        if state.input_tokens > 0 || state.output_tokens > 0 {
            // Rebuild the counters; an already-exhausted budget is caught by the
            // next model response or the run's terminal rules.
            let _ = tracker.record_model(state.input_tokens, state.output_tokens);
        }

        let mut detector = LoopDetector::default();
        for fingerprint in &state.loop_fingerprints {
            detector.observe(fingerprint);
        }

        let control = options
            .cancel_file
            .clone()
            .map_or_else(RunControl::new, RunControl::with_cancel_file);

        let mut session = RunSession {
            runtime: self,
            run_id,
            task,
            options,
            state,
            checkpoint_seq,
            control,
            tracker,
            detector,
            retry: RetryPolicy::default(),
        };

        if run.status == RunStatus::Created {
            session.start().await?;
        }

        let initialized = session.state.messages.is_empty();
        if initialized {
            let rendered = ContextBuilder::system_prompt_with_budget(
                &session.task.objective,
                &session.options.memories,
                session.options.budget.max_input_tokens,
            );
            session.state.memory_refs = rendered.memory_refs;
            session.state.memory_truncated = u32::try_from(rendered.truncated).unwrap_or(u32::MAX);
            session.state.messages.push(Message {
                role: Role::System,
                content: rendered.system_prompt,
            });
            session.state.messages.push(Message {
                role: Role::User,
                content: format!("Objective:\n{}", session.task.objective),
            });
        }
        if latest.is_none() || initialized {
            session.checkpoint().await?;
        }

        session.run().await
    }

    async fn append_event(&self, run_id: RunId, event: &RunEvent) -> Result<(), SilynError> {
        let journal = Journal::new(self.storage.pool());
        let mut last_error = None;

        for attempt in 0..2 {
            let seq = journal.latest_seq(run_id).await? + 1;
            let envelope = EventEnvelope::new(run_id, seq, None, event.clone());
            match journal.append(&envelope).await {
                Ok(()) => return Ok(()),
                Err(SilynError::Transient(message)) if attempt == 0 => {
                    last_error = Some(SilynError::Transient(message));
                }
                Err(error) => return Err(error),
            }
        }

        Err(last_error
            .unwrap_or_else(|| SilynError::Transient("event sequence conflict".to_owned())))
    }

    #[allow(clippy::too_many_arguments)]
    async fn transition(
        &self,
        run_id: RunId,
        run_status: RunStatus,
        started_at: Option<DateTime<Utc>>,
        finished_at: Option<DateTime<Utc>>,
        task: Option<(TaskId, TaskStatus)>,
        event: &RunEvent,
    ) -> Result<(), SilynError> {
        let journal = Journal::new(self.storage.pool());
        let mut last_error = None;

        for attempt in 0..2 {
            let seq = journal.latest_seq(run_id).await? + 1;
            let envelope = EventEnvelope::new(run_id, seq, None, event.clone());
            let updated_at = Utc::now();
            match journal
                .append_run_transition(
                    run_id,
                    run_status,
                    started_at,
                    finished_at,
                    task,
                    updated_at,
                    &envelope,
                )
                .await
            {
                Ok(()) => return Ok(()),
                Err(SilynError::Transient(message)) if attempt == 0 => {
                    last_error = Some(SilynError::Transient(message));
                }
                Err(error) => return Err(error),
            }
        }

        Err(last_error
            .unwrap_or_else(|| SilynError::Transient("event sequence conflict".to_owned())))
    }
}

enum StepResult {
    Continue,
    Finished(RunOutcome),
}

enum ModelTurn {
    Response(ModelResponse),
    Cancelled,
    Failed(String),
}

enum ToolTurn {
    Completed(ToolResult),
    Cancelled,
    Failed(String),
    Indeterminate(String),
}

struct RunSession<'a> {
    runtime: &'a Runtime,
    run_id: RunId,
    task: TaskRecord,
    options: RunOptions,
    state: RunState,
    checkpoint_seq: i64,
    control: RunControl,
    tracker: BudgetTracker,
    detector: LoopDetector,
    retry: RetryPolicy,
}

impl RunSession<'_> {
    async fn run(mut self) -> Result<RunOutcome, SilynError> {
        loop {
            match self.step().await? {
                StepResult::Continue => {}
                StepResult::Finished(outcome) => return Ok(outcome),
            }
        }
    }

    async fn start(&mut self) -> Result<(), SilynError> {
        let now = Utc::now();
        let task_transition = match self.task.status {
            TaskStatus::Created => {
                TaskRepository::new(self.runtime.storage.pool())
                    .update_status(self.task.id, TaskStatus::Ready, now)
                    .await?;
                Some(TaskStatus::Running)
            }
            TaskStatus::Ready => Some(TaskStatus::Running),
            TaskStatus::Running => None,
            other => {
                return Err(SilynError::Permanent(format!(
                    "task {} is in {} and cannot start a run",
                    self.task.id,
                    task_status_label(other)
                )));
            }
        };

        self.runtime
            .transition(
                self.run_id,
                RunStatus::Running,
                Some(now),
                None,
                task_transition.map(|status| (self.task.id, status)),
                &RunEvent::RunStarted,
            )
            .await?;
        self.task.status = TaskStatus::Running;
        Ok(())
    }

    async fn step(&mut self) -> Result<StepResult, SilynError> {
        if self.control.is_cancelled() {
            return self
                .finish_cancelled("cancelled before the next step")
                .await
                .map(StepResult::Finished);
        }
        if let Err(error) = self.tracker.check_wall() {
            return self
                .finish_failed(format!("budget exhausted: {error}"))
                .await
                .map(StepResult::Finished);
        }

        // A pending action belongs to the step that produced it. Resolve it
        // before charging the next step, otherwise a crash during the last
        // allowed step could never be resumed.
        if let Some(pending) = self.state.pending.clone() {
            return self.recover_pending(pending).await;
        }

        let next_step = self.state.step_seq.saturating_add(1);
        let next_step_u32 = u32::try_from(next_step).unwrap_or(u32::MAX);
        if let Err(error) = self.tracker.check_step(next_step_u32) {
            return self
                .finish_failed(format!("budget exhausted: {error}"))
                .await
                .map(StepResult::Finished);
        }

        self.state.step_seq = next_step;
        self.model_step().await
    }

    async fn model_step(&mut self) -> Result<StepResult, SilynError> {
        let first = match self.request_model().await? {
            ModelTurn::Cancelled => {
                return self
                    .finish_cancelled("cancelled during the model call")
                    .await
                    .map(StepResult::Finished);
            }
            ModelTurn::Failed(reason) => {
                return self.finish_failed(reason).await.map(StepResult::Finished);
            }
            ModelTurn::Response(response) => response,
        };

        let budget_error = self.apply_response(&first);
        self.persist_usage().await?;
        if let Some(error) = budget_error {
            return self
                .finish_failed(format!("budget exhausted: {error}"))
                .await
                .map(StepResult::Finished);
        }

        let action = match parse_action(&first.content) {
            Ok(action) => action,
            Err(first_error) => {
                self.state.messages.push(Message {
                    role: Role::User,
                    content: repair_instruction(&first_error),
                });
                self.checkpoint().await?;

                let second = match self.request_model().await? {
                    ModelTurn::Cancelled => {
                        return self
                            .finish_cancelled("cancelled during the repair turn")
                            .await
                            .map(StepResult::Finished);
                    }
                    ModelTurn::Failed(reason) => {
                        return self.finish_failed(reason).await.map(StepResult::Finished);
                    }
                    ModelTurn::Response(response) => response,
                };

                let budget_error = self.apply_response(&second);
                self.persist_usage().await?;
                if let Some(error) = budget_error {
                    return self
                        .finish_failed(format!("budget exhausted: {error}"))
                        .await
                        .map(StepResult::Finished);
                }

                match parse_action(&second.content) {
                    Ok(action) => action,
                    Err(second_error) => {
                        return self
                            .finish_failed(format!(
                                "malformed model action after one repair: {second_error}"
                            ))
                            .await
                            .map(StepResult::Finished);
                    }
                }
            }
        };

        match action {
            ParsedAction::Final(answer) => self.finish_verified(answer).await,
            ParsedAction::Tool { tool, arguments } => self.start_tool_step(tool, arguments).await,
        }
    }

    async fn request_model(&self) -> Result<ModelTurn, SilynError> {
        let request = ModelRequest {
            model: self.options.model.clone(),
            messages: self.state.messages.clone(),
            temperature: Some(0.0),
            max_tokens: None,
        };
        let provider = Arc::clone(&self.runtime.provider);
        let retry = self.retry;
        let timeout = self.options.budget.step_timeout;

        let future = retry.retry(|| {
            let provider = Arc::clone(&provider);
            let request = request.clone();
            async move {
                match tokio::time::timeout(timeout, provider.complete(request)).await {
                    Ok(result) => result,
                    Err(_) => Err(SilynError::Transient(
                        "model call exceeded the step timeout".to_owned(),
                    )),
                }
            }
        });

        tokio::select! {
            biased;
            () = self.control.cancelled() => Ok(ModelTurn::Cancelled),
            result = future => match result {
                Ok(response) => Ok(ModelTurn::Response(response)),
                Err(SilynError::Transient(message)) => {
                    Ok(ModelTurn::Failed(format!("model call timed out or failed: {message}")))
                }
                Err(error) => Ok(ModelTurn::Failed(format!("model error: {error}"))),
            },
        }
    }

    fn apply_response(&mut self, response: &ModelResponse) -> Option<BudgetError> {
        self.state.messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });
        self.state.input_tokens = self
            .state
            .input_tokens
            .saturating_add(response.input_tokens);
        self.state.output_tokens = self
            .state
            .output_tokens
            .saturating_add(response.output_tokens);
        if let Some(price) = self.options.price {
            self.state.cost_usd += (response.input_tokens as f64 / 1_000_000.0)
                * price.input_per_million
                + (response.output_tokens as f64 / 1_000_000.0) * price.output_per_million;
        }
        self.tracker
            .record_model(response.input_tokens, response.output_tokens)
            .err()
    }

    async fn persist_usage(&self) -> Result<(), SilynError> {
        let input = i64::try_from(self.state.input_tokens).unwrap_or(i64::MAX);
        let output = i64::try_from(self.state.output_tokens).unwrap_or(i64::MAX);
        RunRepository::new(self.runtime.storage.pool())
            .update_usage(self.run_id, input, output, self.state.cost_usd)
            .await
    }

    async fn checkpoint(&mut self) -> Result<(), SilynError> {
        self.state.loop_fingerprints = self.detector.fingerprints();
        let json = serde_json::to_string(&self.state)?;
        self.checkpoint_seq += 1;
        let record = CheckpointRecord::new(
            self.run_id,
            self.checkpoint_seq,
            json,
            RUN_STATE_SCHEMA_VERSION,
        );
        CheckpointStore::new(self.runtime.storage.pool())
            .insert(&record)
            .await
    }

    async fn start_tool_step(
        &mut self,
        tool: String,
        arguments: Value,
    ) -> Result<StepResult, SilynError> {
        let call = ToolCall {
            call_id: format!("{}:{}", self.run_id, self.state.step_seq),
            tool,
            arguments,
        };
        let fingerprint = fingerprint_tool_call(&call)?;
        if self.detector.observe(&fingerprint) {
            return self
                .finish_failed(format!(
                    "loop detected: tool `{}` repeated with identical arguments",
                    call.tool
                ))
                .await
                .map(StepResult::Finished);
        }

        let pending = PendingAction {
            step_id: StepId::new(),
            step_seq: self.state.step_seq,
            call,
        };
        self.state.pending = Some(pending.clone());
        // The pending action must be durable before the ledger starts, so a
        // crash between the two can be resolved instead of replayed blindly.
        self.checkpoint().await?;
        self.execute_pending(pending).await
    }

    async fn recover_pending(&mut self, pending: PendingAction) -> Result<StepResult, SilynError> {
        let ledger = ToolLedger::new(self.runtime.storage.pool());
        let record = ledger
            .get(self.run_id, pending.step_id, &pending.call.call_id)
            .await?;

        match record {
            Some(record) if record.state == ToolCallState::Succeeded => {
                let content = record.result_ref.unwrap_or_default();
                self.complete_tool_success(&pending, &content).await?;
                Ok(StepResult::Continue)
            }
            Some(record) if record.state == ToolCallState::Failed => {
                let message = record
                    .result_ref
                    .unwrap_or_else(|| "tool call failed".to_owned());
                self.complete_tool_failure(&pending, &message).await?;
                Ok(StepResult::Continue)
            }
            Some(record) => {
                if record.state == ToolCallState::Started {
                    ledger
                        .mark_indeterminate(self.run_id, pending.step_id, &pending.call.call_id)
                        .await?;
                }
                self.checkpoint().await?;
                self.finish_failed(format!(
                    "tool call {} is indeterminate; refusing to replay",
                    pending.call.call_id
                ))
                .await
                .map(StepResult::Finished)
            }
            None => self.execute_pending(pending).await,
        }
    }

    async fn execute_pending(&mut self, pending: PendingAction) -> Result<StepResult, SilynError> {
        if self.control.is_cancelled() {
            // Nothing has been started, so the ledger stays untouched.
            return self
                .finish_cancelled("cancelled before the tool call started")
                .await
                .map(StepResult::Finished);
        }
        let ledger = ToolLedger::new(self.runtime.storage.pool());
        let record = ToolCallRecord {
            run_id: self.run_id,
            step_id: pending.step_id,
            tool_call_id: pending.call.call_id.clone(),
            tool_name: pending.call.tool.clone(),
            state: ToolCallState::Started,
            result_ref: None,
            created_at: Utc::now(),
        };
        ledger.begin(&record).await?;

        let read_only = is_read_only(&pending.call.tool);
        let policy = if read_only {
            self.retry
        } else {
            RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
            }
        };
        let executor = Arc::clone(&self.runtime.tools);
        let context = AuthorizedToolContext {
            run_id: self.run_id,
            step_id: pending.step_id,
            capabilities: self.options.capabilities.clone(),
        };
        let call = pending.call.clone();
        let timeout = self.options.budget.step_timeout;

        let future = policy.retry(|| {
            let executor = Arc::clone(&executor);
            let context = context.clone();
            let call = call.clone();
            async move {
                match tokio::time::timeout(timeout, executor.execute(context, call)).await {
                    Ok(result) => result,
                    Err(_) => Err(SilynError::Transient(
                        "tool call exceeded the step timeout".to_owned(),
                    )),
                }
            }
        });

        let turn = tokio::select! {
            biased;
            () = self.control.cancelled() => ToolTurn::Cancelled,
            result = future => match result {
                Ok(result) if result.is_error => ToolTurn::Failed(result.content),
                Ok(result) => ToolTurn::Completed(result),
                Err(SilynError::Indeterminate(message)) => ToolTurn::Indeterminate(message),
                Err(SilynError::Transient(message)) if read_only => {
                    ToolTurn::Failed(format!("tool `{}` failed: {message}", call.tool))
                }
                Err(SilynError::Transient(message)) => ToolTurn::Indeterminate(format!(
                    "tool `{}` may have run: {message}",
                    call.tool
                )),
                Err(error) => ToolTurn::Failed(format!("tool `{}` failed: {error}", call.tool)),
            },
        };

        match turn {
            ToolTurn::Completed(result) => {
                self.complete_tool_success(&pending, &result.content)
                    .await?;
                Ok(StepResult::Continue)
            }
            ToolTurn::Failed(message) => {
                self.complete_tool_failure(&pending, &message).await?;
                Ok(StepResult::Continue)
            }
            ToolTurn::Cancelled => {
                ledger
                    .mark_indeterminate(self.run_id, pending.step_id, &pending.call.call_id)
                    .await?;
                self.insert_step(&pending, None).await?;
                self.finish_cancelled(&format!(
                    "cancelled while executing tool call {}",
                    pending.call.call_id
                ))
                .await
                .map(StepResult::Finished)
            }
            ToolTurn::Indeterminate(message) => {
                ledger
                    .mark_indeterminate(self.run_id, pending.step_id, &pending.call.call_id)
                    .await?;
                self.insert_step(&pending, None).await?;
                self.checkpoint().await?;
                self.finish_failed(format!(
                    "tool call {} is indeterminate: {message}",
                    pending.call.call_id
                ))
                .await
                .map(StepResult::Finished)
            }
        }
    }

    async fn complete_tool_success(
        &mut self,
        pending: &PendingAction,
        content: &str,
    ) -> Result<(), SilynError> {
        ToolLedger::new(self.runtime.storage.pool())
            .complete(
                self.run_id,
                pending.step_id,
                &pending.call.call_id,
                ToolCallState::Succeeded,
                Some(content),
            )
            .await?;

        if self.insert_step(pending, Some(content)).await? {
            self.runtime
                .append_event(
                    self.run_id,
                    &RunEvent::StepCompleted {
                        step_id: pending.step_id,
                        seq: pending.step_seq,
                    },
                )
                .await?;
        }

        self.state.messages.push(Message {
            role: Role::Tool,
            content: tool_output(&pending.call.tool, &pending.call.call_id, content),
        });
        self.state.pending = None;
        self.checkpoint().await
    }

    async fn complete_tool_failure(
        &mut self,
        pending: &PendingAction,
        message: &str,
    ) -> Result<(), SilynError> {
        ToolLedger::new(self.runtime.storage.pool())
            .complete(
                self.run_id,
                pending.step_id,
                &pending.call.call_id,
                ToolCallState::Failed,
                Some(message),
            )
            .await?;

        if self.insert_step(pending, Some(message)).await? {
            self.runtime
                .append_event(
                    self.run_id,
                    &RunEvent::ToolFailed {
                        step_id: pending.step_id,
                        tool: pending.call.tool.clone(),
                        message: message.to_owned(),
                    },
                )
                .await?;
        }

        self.state.messages.push(Message {
            role: Role::Tool,
            content: format!(
                "UNTRUSTED TOOL OUTPUT (error, tool `{}` call `{}`):\n{message}",
                pending.call.tool, pending.call.call_id
            ),
        });
        self.state.pending = None;
        self.checkpoint().await
    }

    async fn insert_step(
        &self,
        pending: &PendingAction,
        result_ref: Option<&str>,
    ) -> Result<bool, SilynError> {
        let steps = StepRepository::new(self.runtime.storage.pool());
        if steps.get(pending.step_id).await?.is_some() {
            return Ok(false);
        }
        steps
            .insert(&StepRecord {
                id: pending.step_id,
                run_id: self.run_id,
                seq: i64::try_from(pending.step_seq).unwrap_or(i64::MAX),
                action_type: format!("tool:{}", pending.call.tool),
                result_ref: result_ref.map(str::to_owned),
                created_at: Utc::now(),
            })
            .await?;
        Ok(true)
    }

    async fn finish_verified(&mut self, answer: String) -> Result<StepResult, SilynError> {
        self.state.final_answer = Some(answer.clone());
        self.state.pending = None;
        self.detector.reset();

        // The final answer is itself evidence: `inline:` carries it verbatim so
        // the expected-answer checker can compare it without a file round trip.
        // Step results follow as additional artifacts.
        let mut artifacts = vec![format!("inline:{answer}")];
        artifacts.extend(
            StepRepository::new(self.runtime.storage.pool())
                .list_by_run(self.run_id)
                .await?
                .into_iter()
                .filter_map(|step| step.result_ref),
        );

        let evidence = match self
            .runtime
            .verifier
            .verify(RunSnapshot {
                run_id: self.run_id,
                objective: self.task.objective.clone(),
                artifact_refs: artifacts,
            })
            .await
        {
            Ok(evidence) => evidence,
            Err(error) => {
                return self
                    .finish_failed(format!("outcome verifier failed: {error}"))
                    .await
                    .map(StepResult::Finished);
            }
        };

        self.checkpoint().await?;

        self.runtime
            .transition(
                self.run_id,
                RunStatus::Verifying,
                None,
                None,
                Some((self.task.id, TaskStatus::Verifying)),
                &RunEvent::RunVerified {
                    outcome: evidence.outcome,
                },
            )
            .await?;
        self.task.status = TaskStatus::Verifying;

        let finished_at = Utc::now();
        self.runtime
            .transition(
                self.run_id,
                RunStatus::Completed,
                None,
                Some(finished_at),
                Some((self.task.id, TaskStatus::Completed)),
                &RunEvent::RunCompleted,
            )
            .await?;
        self.task.status = TaskStatus::Completed;

        Ok(StepResult::Finished(self.outcome(
            RunStatus::Completed,
            Some(evidence.outcome),
            Some(answer),
            None,
        )))
    }

    async fn finish_failed(&mut self, reason: String) -> Result<RunOutcome, SilynError> {
        self.checkpoint().await?;
        let task_transition = self
            .task
            .status
            .can_transition_to(TaskStatus::Failed)
            .then_some((self.task.id, TaskStatus::Failed));

        self.runtime
            .transition(
                self.run_id,
                RunStatus::Failed,
                None,
                Some(Utc::now()),
                task_transition,
                &RunEvent::RunFailed {
                    reason: reason.clone(),
                },
            )
            .await?;
        self.task.status = TaskStatus::Failed;

        Ok(self.outcome(
            RunStatus::Failed,
            None,
            self.state.final_answer.clone(),
            Some(reason),
        ))
    }

    async fn finish_cancelled(&mut self, reason: &str) -> Result<RunOutcome, SilynError> {
        self.state.pending = None;
        self.checkpoint().await?;
        let task_transition = self
            .task
            .status
            .can_transition_to(TaskStatus::Cancelled)
            .then_some((self.task.id, TaskStatus::Cancelled));

        self.runtime
            .transition(
                self.run_id,
                RunStatus::Cancelled,
                None,
                Some(Utc::now()),
                task_transition,
                &RunEvent::RunCancelled,
            )
            .await?;
        self.task.status = TaskStatus::Cancelled;

        Ok(self.outcome(
            RunStatus::Cancelled,
            None,
            self.state.final_answer.clone(),
            Some(reason.to_owned()),
        ))
    }

    fn outcome(
        &self,
        status: RunStatus,
        outcome: Option<VerifiedOutcome>,
        final_answer: Option<String>,
        reason: Option<String>,
    ) -> RunOutcome {
        RunOutcome {
            run_id: self.run_id,
            status,
            outcome,
            final_answer,
            reason,
            steps: u32::try_from(self.state.step_seq).unwrap_or(u32::MAX),
            input_tokens: self.state.input_tokens,
            output_tokens: self.state.output_tokens,
            cost_usd: self.state.cost_usd,
            memory_refs: self.state.memory_refs.clone(),
        }
    }
}

enum ParsedAction {
    Final(String),
    Tool { tool: String, arguments: Value },
}

#[derive(Debug, Deserialize)]
struct RawAction {
    action: String,
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

fn parse_action(content: &str) -> Result<ParsedAction, SilynError> {
    let raw: RawAction = serde_json::from_str(content)
        .map_err(|error| SilynError::Schema(format!("response is not one JSON object: {error}")))?;

    match raw.action.as_str() {
        "final" => raw
            .answer
            .map(ParsedAction::Final)
            .ok_or_else(|| SilynError::Schema("final action requires `answer`".to_owned())),
        "tool" => {
            let tool = raw
                .tool
                .filter(|tool| !tool.trim().is_empty())
                .ok_or_else(|| SilynError::Schema("tool action requires `tool`".to_owned()))?;
            Ok(ParsedAction::Tool {
                tool,
                arguments: raw.arguments.unwrap_or(Value::Null),
            })
        }
        other => Err(SilynError::Schema(format!(
            "unknown action `{other}`; expected `final` or `tool`"
        ))),
    }
}

fn repair_instruction(error: &SilynError) -> String {
    format!(
        "Your previous response was rejected: {error}\n\
         Return exactly one JSON object and nothing else. Valid shapes:\n\
         {{\"action\":\"final\",\"answer\":\"...\"}}\n\
         {{\"action\":\"tool\",\"tool\":\"fs.read\",\"arguments\":{{\"path\":\"data.csv\"}}}}"
    )
}

fn tool_output(tool: &str, call_id: &str, content: &str) -> String {
    format!("UNTRUSTED TOOL OUTPUT (tool `{tool}` call `{call_id}`):\n{content}")
}

fn is_read_only(tool: &str) -> bool {
    matches!(tool, TOOL_FS_LIST | TOOL_FS_READ | TOOL_DATA_PROFILE)
}

fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Created => "created",
        RunStatus::Running => "running",
        RunStatus::WaitingApproval => "waiting_approval",
        RunStatus::WaitingTool => "waiting_tool",
        RunStatus::Verifying => "verifying",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
    }
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Created => "created",
        TaskStatus::Ready => "ready",
        TaskStatus::Running => "running",
        TaskStatus::WaitingApproval => "waiting_approval",
        TaskStatus::WaitingTool => "waiting_tool",
        TaskStatus::Verifying => "verifying",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}
