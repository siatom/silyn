//! Silyn evaluation harness.
//!
//! This crate owns the synthetic `csv-stats-v1` task family, the oracle-based
//! scoring of a solver, the sealed test set and the evaluation report contract
//! (see `docs/architecture/adr/0004-evaluation-protocol.md`).
//!
//! Boundaries:
//! - a [`Solver`] only ever receives [`TaskInput`] (objective + CSV text);
//! - oracles live in metadata that is computed by the generator before the CSV is
//!   rendered and is never parsed back out of the CSV;
//! - the `test` split is only reachable through [`SealedTestSet`], which exposes
//!   evaluation but no task or oracle accessor;
//! - arms B and C reuse arm A in W2 because memory and evolution do not exist yet.

pub mod baseline;
pub mod evaluator;
pub mod generator;
pub mod integrity;
pub mod manifest;
pub mod report;
pub mod sealed;
pub mod task;

pub use baseline::BaselineSolver;
pub use evaluator::{ArmOutcome, Arms, Evaluator, Solver, TaskResult};
pub use manifest::SuiteManifest;
pub use report::{EvaluationReport, Verdict};
pub use sealed::SealedTestSet;
pub use task::{Oracle, Split, Submission, Task, TaskInput, TaskMetadata, TrapFlags, Unit};
