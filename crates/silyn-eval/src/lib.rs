//! Silyn evaluation harness.
//!
//! This crate owns the synthetic `csv-stats-v1` task family, the oracle-based
//! scoring of a solver, the sealed test set, isolated per-task workers, paired
//! bootstrap statistics and the evaluation report contract (see
//! `docs/architecture/adr/0004-evaluation-protocol.md`).
//!
//! Boundaries:
//! - a [`Solver`] only ever receives [`TaskInput`] (objective + CSV text);
//! - oracles live in metadata that is computed by the generator before the CSV is
//!   rendered and is never parsed back out of the CSV;
//! - the `test` split is only reachable through [`SealedTestSet`], which exposes
//!   evaluation but no public task or oracle accessor;
//! - the CLI runs every attempt in a child process with a scrubbed environment,
//!   a fresh working directory and a wall-clock timeout;
//! - the W6 model solver runs in-process (it needs the provider key) and receives
//!   only declared, untrusted context; the subprocess worker keeps the baseline;
//! - arms A, B and C share solver, model, budget and price; only the recorded
//!   capability flags and the declared context differ.

pub mod arm;
pub mod backend;
pub mod baseline;
pub mod context;
pub mod evaluator;
pub mod generator;
pub mod integrity;
pub mod isolation;
pub mod manifest;
pub mod model_solver;
pub mod report;
pub mod sealed;
pub mod statistics;
pub mod task;

mod rng;

pub use arm::{ArmConfig, ArmId, Arms, ArmsConfig};
pub use backend::{InProcessBackend, RunnerBackend, TaskAttempt, TaskBudget, TaskFailureKind};
pub use baseline::BaselineSolver;
pub use context::{ArmContext, ArmContextRecord, ContextProvenance};
pub use evaluator::{ArmOutcome, ArmSolvers, Evaluator, FailureRecord, Solver, TaskResult};
pub use isolation::{SubprocessBackend, WorkerFault};
pub use manifest::SuiteManifest;
pub use model_solver::{MODEL_SOLVER_ID, ModelSolver};
pub use report::{EvaluationReport, render_markdown};
pub use sealed::SealedTestSet;
pub use statistics::{
    ArmStatistics, Bootstrap, Decision, EvaluationStatistics, PairedGain, Verdict, VerdictDetail,
};
pub use task::{Oracle, Split, Submission, Task, TaskInput, TaskMetadata, TrapFlags, Unit};
