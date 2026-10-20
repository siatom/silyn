//! Arm runner and scoring.
//!
//! A [`Solver`] is the only surface a candidate implements. The runner owns the
//! oracle, so a solver cannot see ground truth and cannot fake a success: its
//! JSON submission is compared field by field against generator metadata.
//!
//! Attempts go through a [`RunnerBackend`](crate::backend::RunnerBackend). The
//! CLI always uses subprocess isolation; the in-process backend exists for unit
//! tests and still turns solver errors and panics into task failures.

use std::time::Instant;

use silyn_core::SilynError;

use crate::arm::{ArmConfig, ArmId, Arms, ArmsConfig};
use crate::backend::{InProcessBackend, RunnerBackend, TaskFailure, TaskFailureKind};
use crate::generator::generate_split;
use crate::sealed::SealedTestSet;
use crate::task::{Split, Submission, Task, TaskInput};

/// Candidate or baseline implementation under evaluation.
pub trait Solver: Send + Sync {
    /// Answers one task. Failures are counted as tool errors, never as success.
    fn solve(&self, input: &TaskInput) -> Result<Submission, SilynError>;

    /// Metered spend per attempt. The mock provider reports zero.
    fn cost_usd_per_call(&self) -> f64 {
        0.0
    }
}

/// Per-arm solvers for a matrix run. Every field is a full [`Solver`]; they may
/// share an implementation (baseline) or differ only by context (W6).
#[derive(Clone, Copy)]
pub struct ArmSolvers<'a> {
    pub a: &'a dyn Solver,
    pub b: &'a dyn Solver,
    pub c: &'a dyn Solver,
}

impl<'a> ArmSolvers<'a> {
    #[must_use]
    pub fn get(&self, id: ArmId) -> &'a dyn Solver {
        match id {
            ArmId::A => self.a,
            ArmId::B => self.b,
            ArmId::C => self.c,
        }
    }
}

/// Per-task scoring, kept for diagnostics and paired statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub task_id: String,
    pub successes: u32,
    pub attempts: u32,
}

impl TaskResult {
    /// Success rate of this task in percentage points.
    #[must_use]
    pub fn success_rate_pp(&self) -> f64 {
        if self.attempts == 0 {
            0.0
        } else {
            f64::from(self.successes) * 100.0 / f64::from(self.attempts)
        }
    }
}

/// One swallowed failure, retained so a report can explain a low score.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureRecord {
    pub task_id: String,
    pub kind: TaskFailureKind,
    pub detail: String,
}

/// Outcome of running one arm over one split.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmOutcome {
    successes: u32,
    total: u32,
    cost_usd: f64,
    duration_ms: f64,
    latency_p50_ms: f64,
    latency_p95_ms: f64,
    tool_errors: u32,
    unauthorized_actions: u32,
    task_results: Vec<TaskResult>,
    failures: Vec<FailureRecord>,
}

impl ArmOutcome {
    #[must_use]
    pub fn successes(&self) -> u32 {
        self.successes
    }

    /// Attempts, i.e. `tasks * repeats`.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.total
    }

    #[must_use]
    pub fn cost_usd(&self) -> f64 {
        self.cost_usd
    }

    /// Wall-clock duration of every attempt in this arm.
    #[must_use]
    pub fn duration_ms(&self) -> f64 {
        self.duration_ms
    }

    #[must_use]
    pub fn latency_p50_ms(&self) -> f64 {
        self.latency_p50_ms
    }

    #[must_use]
    pub fn latency_p95_ms(&self) -> f64 {
        self.latency_p95_ms
    }

    #[must_use]
    pub fn tool_errors(&self) -> u32 {
        self.tool_errors
    }

    /// Actions the policy gate refused. Expected to stay zero.
    #[must_use]
    pub fn unauthorized_actions(&self) -> u32 {
        self.unauthorized_actions
    }

    /// Per-task success/failure detail, in split order.
    #[must_use]
    pub fn task_results(&self) -> &[TaskResult] {
        &self.task_results
    }

    /// Failures in attempt order, for diagnostics.
    #[must_use]
    pub fn failures(&self) -> &[FailureRecord] {
        &self.failures
    }

    /// Success rate in percentage points.
    #[must_use]
    pub fn success_rate_pp(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            f64::from(self.successes) * 100.0 / f64::from(self.total)
        }
    }

    /// Replaces the metered cost with the spend measured by the solver.
    ///
    /// The evaluator's own model is `cost_per_call * attempts`, which is exact for
    /// a fixed-price solver. A metered provider (W6) reports actual token usage,
    /// so the CLI overrides the number after the arm has run.
    #[must_use]
    pub fn with_cost_usd(mut self, cost_usd: f64) -> Self {
        if cost_usd.is_finite() && cost_usd >= 0.0 {
            self.cost_usd = cost_usd;
        }
        self
    }
}

/// Default seed recorded when the operator does not pass one.
///
/// The task stream itself is deterministic (the generator is seeded per split and
/// index), so the run seed is provenance: it is written to the statistics
/// document and, for a stochastic solver or provider, it is the value that would
/// seed the draw. The bootstrap seed is independent and fixed, so the interval of
/// a given outcome set cannot be re-rolled.
pub const DEFAULT_RUN_SEED: u64 = 0x5eed_0000_0000_2026;

/// Upper bound for `--repeats` (ADR 0004 and the suite manifest schema).
pub const MAX_REPEATS: u32 = 5;

/// Arm runner bound to a repeat count and a recorded seed.
#[derive(Debug, Clone, Copy)]
pub struct Evaluator {
    repeats: u32,
    seed: u64,
}

impl Evaluator {
    /// `repeats` must stay within the protocol range 1..=5.
    pub fn new(repeats: u32) -> Result<Self, SilynError> {
        Self::with_seed(repeats, DEFAULT_RUN_SEED)
    }

    /// Builds a runner with an explicit, recorded seed.
    pub fn with_seed(repeats: u32, seed: u64) -> Result<Self, SilynError> {
        if !(1..=MAX_REPEATS).contains(&repeats) {
            return Err(SilynError::Permanent(format!(
                "repeats must be 1..={MAX_REPEATS}, got {repeats}"
            )));
        }
        Ok(Self { repeats, seed })
    }

    #[must_use]
    pub fn repeats(&self) -> u32 {
        self.repeats
    }

    #[must_use]
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Runs train or dev in-process. The test split is denied here by design;
    /// it is only reachable through [`SealedTestSet`].
    pub fn run_arm(&self, solver: &dyn Solver, split: Split) -> Result<ArmOutcome, SilynError> {
        let config = ArmConfig::baseline(ArmId::A, "mock", Default::default());
        let backend = InProcessBackend::new(solver);
        self.run_split(&backend, &config, split)
    }

    /// Runs A, B and C in-process with the same solver and budget. Useful for
    /// offline smoke checks; the verdict still follows the paired statistics.
    pub fn run_matrix(&self, solver: &dyn Solver, split: Split) -> Result<Arms, SilynError> {
        let configs = ArmsConfig::baseline("mock", Default::default());
        let solvers = ArmSolvers {
            a: solver,
            b: solver,
            c: solver,
        };
        self.run_matrix_with(&configs, &solvers, split)
    }

    /// Runs A, B and C in-process, each with its own solver and the declared
    /// configuration.
    ///
    /// This is the W6 entry point: one solver per arm (arm A has no context, B
    /// memory only, C memory and policy), while every arm shares the solver id,
    /// model, budget and price through [`ArmsConfig::validate_equal_budget`]. The
    /// sealed split is refused here; use [`Evaluator::run_sealed_matrix_with`].
    pub fn run_matrix_with(
        &self,
        configs: &ArmsConfig,
        solvers: &ArmSolvers<'_>,
        split: Split,
    ) -> Result<Arms, SilynError> {
        if split.is_sealed() {
            return Err(SilynError::PolicyDenied(
                "the test split is sealed; use SealedTestSet::load_for_manifest".to_owned(),
            ));
        }
        configs.validate_equal_budget()?;
        let tasks = generate_split(split)?;
        Ok(self.run_matrix_tasks(&tasks, configs, solvers))
    }

    /// Runs the sealed test set in-process, one solver per arm.
    pub fn run_sealed_matrix_with(
        &self,
        configs: &ArmsConfig,
        solvers: &ArmSolvers<'_>,
        set: &SealedTestSet,
    ) -> Result<Arms, SilynError> {
        configs.validate_equal_budget()?;
        Ok(self.run_matrix_tasks(set.tasks(), configs, solvers))
    }

    fn run_matrix_tasks(
        &self,
        tasks: &[Task],
        configs: &ArmsConfig,
        solvers: &ArmSolvers<'_>,
    ) -> Arms {
        Arms {
            a: self.run_tasks(tasks, &InProcessBackend::new(solvers.a), &configs.a),
            b: self.run_tasks(tasks, &InProcessBackend::new(solvers.b), &configs.b),
            c: self.run_tasks(tasks, &InProcessBackend::new(solvers.c), &configs.c),
        }
    }

    /// Evaluates one arm over a train or dev split through a backend.
    ///
    /// A sealed split is refused before any task is generated, so a candidate
    /// path can never hold test tasks or oracles.
    pub fn run_split(
        &self,
        backend: &dyn RunnerBackend,
        config: &ArmConfig,
        split: Split,
    ) -> Result<ArmOutcome, SilynError> {
        if split.is_sealed() {
            return Err(SilynError::PolicyDenied(
                "the test split is sealed; use SealedTestSet::load_for_manifest".to_owned(),
            ));
        }
        config.validate()?;
        let tasks = generate_split(split)?;
        Ok(self.run_tasks(&tasks, backend, config))
    }

    /// Evaluates one arm over the sealed test set. This is the only test-split
    /// path in the crate and it never returns tasks or oracles to the caller.
    #[must_use]
    pub fn run_sealed(
        &self,
        backend: &dyn RunnerBackend,
        config: &ArmConfig,
        set: &SealedTestSet,
    ) -> ArmOutcome {
        self.run_tasks(set.tasks(), backend, config)
    }

    /// Evaluates the sealed test set in-process through a solver.
    #[must_use]
    pub fn evaluate_sealed(&self, solver: &dyn Solver, set: &SealedTestSet) -> ArmOutcome {
        let config = ArmConfig::baseline(ArmId::A, "mock", Default::default());
        self.run_sealed(&InProcessBackend::new(solver), &config, set)
    }

    /// Executes `tasks * repeats` attempts for one arm and scores each
    /// submission against the task oracle. Public so tests can drive a
    /// hand-built task list through a [`RunnerBackend`].
    #[must_use]
    pub fn run_tasks(
        &self,
        tasks: &[Task],
        backend: &dyn RunnerBackend,
        config: &ArmConfig,
    ) -> ArmOutcome {
        let repeats = self.repeats.clamp(1, MAX_REPEATS);
        let mut successes = 0_u32;
        let mut total = 0_u32;
        let mut tool_errors = 0_u32;
        let mut unauthorized_actions = 0_u32;
        let mut duration_ms = 0.0_f64;
        let mut latencies: Vec<f64> = Vec::new();
        let mut task_results: Vec<TaskResult> = Vec::with_capacity(tasks.len());
        let mut failures: Vec<FailureRecord> = Vec::new();

        for task in tasks {
            let mut task_successes = 0_u32;
            for _ in 0..repeats {
                total += 1;
                let started = Instant::now();
                let attempt = backend.run(&task.input, &config.budget);
                let elapsed = started.elapsed().as_secs_f64() * 1000.0;
                // Prefer the backend's own measurement; process spawn overhead
                // is included by the wall clock either way.
                let attempt_ms = if attempt.duration_ms() > 0.0 {
                    attempt.duration_ms()
                } else {
                    elapsed
                };
                duration_ms += attempt_ms;
                latencies.push(attempt_ms);
                tool_errors += attempt.tool_errors();
                unauthorized_actions += attempt.unauthorized_actions();

                match attempt.submission() {
                    Some(submission) if task.metadata.oracle.matches(submission) => {
                        successes += 1;
                        task_successes += 1;
                    }
                    Some(_) => {}
                    None => {
                        let failure = attempt.failure().cloned().unwrap_or_else(|| {
                            TaskFailure::new(
                                TaskFailureKind::SolverError,
                                "backend returned no submission and no failure",
                            )
                        });
                        failures.push(FailureRecord {
                            task_id: task.metadata.task_id.clone(),
                            kind: failure.kind(),
                            detail: failure.detail().to_owned(),
                        });
                    }
                }
            }
            task_results.push(TaskResult {
                task_id: task.metadata.task_id.clone(),
                successes: task_successes,
                attempts: repeats,
            });
        }

        latencies.sort_by(f64::total_cmp);

        ArmOutcome {
            successes,
            total,
            cost_usd: config.cost_usd_per_call * f64::from(total),
            duration_ms,
            latency_p50_ms: percentile(&latencies, 0.50),
            latency_p95_ms: percentile(&latencies, 0.95),
            tool_errors,
            unauthorized_actions,
            task_results,
            failures,
        }
    }
}

fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let last = sorted.len() - 1;
    let index = ((fraction * last as f64).ceil() as usize).min(last);
    sorted[index]
}
