//! Runner backends and the per-task budget.
//!
//! A [`RunnerBackend`] is the only place a task is handed to candidate code.
//! The CLI uses [`crate::isolation::SubprocessBackend`] so a candidate never
//! shares the harness process; unit tests use [`InProcessBackend`], which still
//! converts every solver error (including a panic) into a task failure.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use silyn_core::SilynError;

use crate::evaluator::Solver;
use crate::task::{Submission, TaskInput};

/// Resources granted to one task attempt. Every arm runs with the same budget.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBudget {
    /// Wall-clock limit for a single task, including process start-up.
    pub timeout_ms: u64,
    /// Maximum agent steps the runtime may take for one task.
    pub max_steps: u32,
    /// Maximum metered spend for one task.
    pub max_cost_usd: f64,
}

impl TaskBudget {
    /// W6 experiment cap: 8 steps, 10 minutes and 2.00 USD per task.
    pub const DEFAULT_TIMEOUT_MS: u64 = 600_000;

    #[must_use]
    pub const fn new(timeout_ms: u64, max_steps: u32, max_cost_usd: f64) -> Self {
        Self {
            timeout_ms,
            max_steps,
            max_cost_usd,
        }
    }

    /// Rejects budgets that would let the harness or a candidate run unbounded.
    pub fn validate(&self) -> Result<(), SilynError> {
        if self.timeout_ms == 0 {
            return Err(SilynError::Permanent(
                "task timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=64).contains(&self.max_steps) {
            return Err(SilynError::Permanent(format!(
                "max_steps must be 1..=64, got {}",
                self.max_steps
            )));
        }
        if !(self.max_cost_usd.is_finite() && self.max_cost_usd >= 0.0) {
            return Err(SilynError::Permanent(format!(
                "max_cost_usd must be finite and non-negative, got {}",
                self.max_cost_usd
            )));
        }
        Ok(())
    }
}

impl Default for TaskBudget {
    fn default() -> Self {
        Self {
            timeout_ms: Self::DEFAULT_TIMEOUT_MS,
            max_steps: 8,
            max_cost_usd: 2.0,
        }
    }
}

/// Why a single attempt produced no usable submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskFailureKind {
    /// The worker exceeded [`TaskBudget::timeout_ms`] and was killed.
    Timeout,
    /// The worker exited with a non-zero status (including a panic).
    NonZeroExit,
    /// The worker exited cleanly but stdout was not exactly one `Submission`.
    InvalidOutput,
    /// The in-process solver returned an error or panicked.
    SolverError,
    /// The worker process could not be started at all.
    SpawnError,
}

impl TaskFailureKind {
    /// Stable wire name used by the statistics document.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::NonZeroExit => "non_zero_exit",
            Self::InvalidOutput => "invalid_output",
            Self::SolverError => "solver_error",
            Self::SpawnError => "spawn_error",
        }
    }
}

/// A task failure with a bounded human-readable detail. Never a harness crash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFailure {
    pub kind: TaskFailureKind,
    pub detail: String,
}

impl TaskFailure {
    #[must_use]
    pub fn new(kind: TaskFailureKind, detail: impl Into<String>) -> Self {
        let mut detail = detail.into();
        detail.truncate(240);
        Self { kind, detail }
    }

    #[must_use]
    pub fn kind(&self) -> TaskFailureKind {
        self.kind
    }

    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }
}

/// Result of one task attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskAttempt {
    submission: Option<Submission>,
    duration_ms: f64,
    unauthorized_actions: u32,
    tool_errors: u32,
    failure: Option<TaskFailure>,
}

impl TaskAttempt {
    #[must_use]
    pub fn succeeded(submission: Submission, duration_ms: f64) -> Self {
        Self {
            submission: Some(submission),
            duration_ms,
            unauthorized_actions: 0,
            tool_errors: 0,
            failure: None,
        }
    }

    #[must_use]
    pub fn failed(failure: TaskFailure, duration_ms: f64) -> Self {
        Self {
            submission: None,
            duration_ms,
            unauthorized_actions: 0,
            tool_errors: 1,
            failure: Some(failure),
        }
    }

    #[must_use]
    pub fn submission(&self) -> Option<&Submission> {
        self.submission.as_ref()
    }

    #[must_use]
    pub fn duration_ms(&self) -> f64 {
        self.duration_ms
    }

    #[must_use]
    pub fn unauthorized_actions(&self) -> u32 {
        self.unauthorized_actions
    }

    #[must_use]
    pub fn tool_errors(&self) -> u32 {
        self.tool_errors
    }

    #[must_use]
    pub fn failure(&self) -> Option<&TaskFailure> {
        self.failure.as_ref()
    }

    /// Records an action the policy gate refused. A nonzero count forces a
    /// `reject` verdict regardless of the measured gain.
    #[must_use]
    pub fn with_unauthorized_actions(mut self, count: u32) -> Self {
        self.unauthorized_actions = count;
        self
    }
}

/// How the harness executes one task.
pub trait RunnerBackend: Send + Sync {
    /// Stable backend name recorded in the statistics document.
    fn name(&self) -> &'static str;

    /// Runs one attempt. Implementations must not panic and must never return a
    /// submission for a failed attempt.
    fn run(&self, input: &TaskInput, budget: &TaskBudget) -> TaskAttempt;
}

/// In-process backend used by unit tests. The CLI never selects it by default.
pub struct InProcessBackend<'a> {
    solver: &'a dyn Solver,
}

impl<'a> InProcessBackend<'a> {
    #[must_use]
    pub fn new(solver: &'a dyn Solver) -> Self {
        Self { solver }
    }
}

impl RunnerBackend for InProcessBackend<'_> {
    fn name(&self) -> &'static str {
        "in_process"
    }

    fn run(&self, input: &TaskInput, _budget: &TaskBudget) -> TaskAttempt {
        let started = Instant::now();
        // A candidate must not be able to abort the whole evaluation.
        let outcome = catch_unwind(AssertUnwindSafe(|| self.solver.solve(input)));
        let duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        match outcome {
            Ok(Ok(submission)) => TaskAttempt::succeeded(submission, duration_ms),
            Ok(Err(error)) => TaskAttempt::failed(
                TaskFailure::new(TaskFailureKind::SolverError, error.to_string()),
                duration_ms,
            ),
            Err(_) => TaskAttempt::failed(
                TaskFailure::new(TaskFailureKind::SolverError, "solver panicked"),
                duration_ms,
            ),
        }
    }
}
