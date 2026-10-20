//! Arm runner and scoring.
//!
//! A [`Solver`] is the only surface a candidate implements. The runner owns the
//! oracle, so a solver cannot see ground truth and cannot fake a success: its
//! JSON submission is compared field by field against generator metadata.

use std::time::Instant;

use silyn_core::SilynError;

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

/// Per-task scoring, kept for diagnostics and W5 analysis (not in the report).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskResult {
    pub task_id: String,
    pub successes: u32,
    pub attempts: u32,
}

/// Outcome of running one arm over one split.
#[derive(Debug, Clone, PartialEq)]
pub struct ArmOutcome {
    successes: u32,
    total: u32,
    cost_usd: f64,
    latency_p50_ms: f64,
    latency_p95_ms: f64,
    tool_errors: u32,
    task_results: Vec<TaskResult>,
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

    /// Per-task success/failure detail, in split order.
    #[must_use]
    pub fn task_results(&self) -> &[TaskResult] {
        &self.task_results
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
}

/// Arms A, B and C. In W2 all three run the same solver because memory (B) and
/// the promoted policy (C) do not exist yet; the report records this honestly.
#[derive(Debug, Clone, PartialEq)]
pub struct Arms {
    pub a: ArmOutcome,
    pub b: ArmOutcome,
    pub c: ArmOutcome,
}

/// Single-arm runner for the W2 skeleton.
#[derive(Debug, Clone, Copy)]
pub struct Evaluator {
    repeats: u32,
}

impl Evaluator {
    /// `repeats` must stay within the protocol range 1..=5.
    pub fn new(repeats: u32) -> Result<Self, SilynError> {
        if !(1..=5).contains(&repeats) {
            return Err(SilynError::Permanent(format!(
                "repeats must be 1..=5, got {repeats}"
            )));
        }
        Ok(Self { repeats })
    }

    #[must_use]
    pub fn repeats(&self) -> u32 {
        self.repeats
    }

    /// Runs train or dev. The test split is denied here by design.
    pub fn run_arm(&self, solver: &dyn Solver, split: Split) -> Result<ArmOutcome, SilynError> {
        if split.is_sealed() {
            return Err(SilynError::PolicyDenied(
                "the test split is sealed; evaluate it through SealedTestSet".to_owned(),
            ));
        }
        let tasks = generate_split(split)?;
        Ok(run_tasks(&tasks, solver, self.repeats))
    }

    /// Runs A, B and C with the same solver. W2 has no memory or evolution, so
    /// any difference between the arms would be noise; the verdict must not
    /// claim a gain. This is replaced by real arms in W5.
    pub fn run_matrix(&self, solver: &dyn Solver, split: Split) -> Result<Arms, SilynError> {
        Ok(Arms {
            a: self.run_arm(solver, split)?,
            b: self.run_arm(solver, split)?,
            c: self.run_arm(solver, split)?,
        })
    }

    /// Evaluates the sealed test set. Only this method path can touch test tasks.
    #[must_use]
    pub fn evaluate_sealed(&self, solver: &dyn Solver, set: &SealedTestSet) -> ArmOutcome {
        set.evaluate(solver, self.repeats)
    }
}

/// Executes `tasks * repeats` attempts and scores each submission exactly.
pub(crate) fn run_tasks(tasks: &[Task], solver: &dyn Solver, repeats: u32) -> ArmOutcome {
    let repeats = repeats.clamp(1, 5);
    let mut successes = 0_u32;
    let mut total = 0_u32;
    let mut tool_errors = 0_u32;
    let mut latencies: Vec<f64> = Vec::new();
    let mut task_results: Vec<TaskResult> = Vec::with_capacity(tasks.len());

    for task in tasks {
        let mut task_successes = 0_u32;
        for _ in 0..repeats {
            total += 1;
            let started = Instant::now();
            let result = solver.solve(&task.input);
            latencies.push(started.elapsed().as_secs_f64() * 1000.0);
            match result {
                Ok(submission) => {
                    if task.metadata.oracle.matches(&submission) {
                        successes += 1;
                        task_successes += 1;
                    }
                }
                Err(_) => tool_errors += 1,
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
        cost_usd: solver.cost_usd_per_call() * f64::from(total),
        latency_p50_ms: percentile(&latencies, 0.50),
        latency_p95_ms: percentile(&latencies, 0.95),
        tool_errors,
        task_results,
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
