//! Shared helpers for the silyn-eval integration tests.
//!
//! Every helper is offline and deterministic: tasks are built by hand, the
//! scripted backend returns fixed submissions, and the worker subprocess is the
//! crate's own binary.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use silyn_eval::ArmId;
use silyn_eval::arm::{Arms, ArmsConfig};
use silyn_eval::backend::{RunnerBackend, TaskAttempt, TaskBudget, TaskFailure, TaskFailureKind};
use silyn_eval::evaluator::{ArmOutcome, Evaluator};
use silyn_eval::manifest::SuiteManifest;
use silyn_eval::task::{Oracle, Split, Submission, Task, TaskInput, TaskMetadata, TrapFlags, Unit};

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repository root")
}

pub fn suite_dir() -> PathBuf {
    repo_root().join("eval/suites/csv-stats-v1")
}

pub fn manifest() -> SuiteManifest {
    let path = suite_dir().join("suite.toml");
    let manifest = SuiteManifest::load(&path).expect("load suite manifest");
    manifest.validate().expect("valid suite manifest");
    manifest
}

/// The `silyn-eval` binary built for this test run.
pub fn worker_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_silyn-eval"))
}

pub fn oracle(row_count: u32, missing_total: u32, duplicate_rows: u32) -> Oracle {
    Oracle {
        row_count,
        missing_total,
        duplicate_rows,
        unit: Unit::Kg,
    }
}

/// Builds a task with a unique CSV text so a scripted backend can key on it.
pub fn task(id: &str, oracle: Oracle) -> Task {
    Task {
        metadata: TaskMetadata {
            task_id: id.to_owned(),
            seed: 0,
            split: Split::Dev,
            unique_rows: oracle.row_count,
            emitted_rows: oracle.row_count + oracle.duplicate_rows,
            unit: oracle.unit,
            missing_total: oracle.missing_total,
            duplicate_rows: oracle.duplicate_rows,
            traps: TrapFlags::default(),
            oracle,
        },
        input: TaskInput {
            objective: "count the rows".to_owned(),
            csv: format!("csv::{id}"),
        },
    }
}

pub fn correct_submission(oracle: Oracle) -> Submission {
    Submission {
        row_count: i64::from(oracle.row_count),
        missing_total: i64::from(oracle.missing_total),
        duplicate_rows: i64::from(oracle.duplicate_rows),
        unit: oracle.unit,
    }
}

pub fn wrong_submission() -> Submission {
    Submission {
        row_count: 999,
        missing_total: 0,
        duplicate_rows: 0,
        unit: Unit::Lb,
    }
}

/// Backend that answers from a table and can report a refused action.
pub struct ScriptedBackend {
    pub answers: HashMap<String, Submission>,
    pub unauthorized: u32,
}

impl RunnerBackend for ScriptedBackend {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn run(&self, input: &TaskInput, _budget: &TaskBudget) -> TaskAttempt {
        match self.answers.get(&input.csv) {
            Some(submission) => TaskAttempt::succeeded(submission.clone(), 1.0)
                .with_unauthorized_actions(self.unauthorized),
            None => TaskAttempt::failed(
                TaskFailure::new(TaskFailureKind::SolverError, "no scripted answer"),
                1.0,
            ),
        }
    }
}

/// Builds an arm outcome with `correct` successes over `total` tasks.
pub fn scripted_arm(
    evaluator: &Evaluator,
    config: &silyn_eval::ArmConfig,
    correct: usize,
    total: usize,
    unauthorized: u32,
) -> ArmOutcome {
    let tasks: Vec<Task> = (0..total)
        .map(|index| task(&format!("task-{index:03}"), oracle(4, 2, 1)))
        .collect();
    let mut answers = HashMap::new();
    for (index, task) in tasks.iter().enumerate() {
        answers.insert(
            task.input.csv.clone(),
            if index < correct {
                correct_submission(task.metadata.oracle)
            } else {
                wrong_submission()
            },
        );
    }
    evaluator.run_tasks(
        &tasks,
        &ScriptedBackend {
            answers,
            unauthorized,
        },
        config,
    )
}

/// Builds a complete A/B/C matrix with the baseline configuration.
pub fn scripted_arms(a_correct: usize, b_correct: usize, c_correct: usize) -> (ArmsConfig, Arms) {
    scripted_arms_with(a_correct, b_correct, c_correct, 10, 0)
}

pub fn scripted_arms_with(
    a_correct: usize,
    b_correct: usize,
    c_correct: usize,
    total: usize,
    unauthorized: u32,
) -> (ArmsConfig, Arms) {
    let evaluator = Evaluator::new(1).expect("evaluator");
    let configs = ArmsConfig::baseline("mock", TaskBudget::new(1_000, 8, 2.0));
    let arms = Arms {
        a: scripted_arm(&evaluator, &configs.a, a_correct, total, unauthorized),
        b: scripted_arm(&evaluator, &configs.b, b_correct, total, unauthorized),
        c: scripted_arm(&evaluator, &configs.c, c_correct, total, unauthorized),
    };
    (configs, arms)
}

/// Budget used by the isolation tests.
pub fn fast_budget() -> TaskBudget {
    TaskBudget::new(500, 8, 2.0)
}

#[must_use]
pub fn arm_ids(ids: &[ArmId]) -> Vec<&'static str> {
    ids.iter().map(|id| id.as_str()).collect()
}
