//! Runner, isolation and arm-fairness tests.
//!
//! The worker tests spawn the real `silyn-eval` binary with `env_clear()` and a
//! fresh temporary directory, exactly like the CLI does. No network is used.

mod common;

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use chrono::{DateTime, Utc};
use common::{fast_budget, manifest, oracle, repo_root, task, worker_exe};
use silyn_eval::arm::{ArmId, ArmsConfig};
use silyn_eval::backend::{TaskBudget, TaskFailureKind};
use silyn_eval::evaluator::Evaluator;
use silyn_eval::isolation::{SubprocessBackend, WorkerFault, allowed_env_vars};
use silyn_eval::report::EvaluationReport;
use silyn_eval::sealed::SealedTestSet;
use silyn_eval::statistics::{Bootstrap, EvaluationStatistics};
use silyn_eval::{BaselineSolver, InProcessBackend, Solver, Submission, TaskInput};
use uuid::Uuid;

fn project_task() -> TaskInput {
    TaskInput {
        objective: "count the rows".to_owned(),
        csv: "id,mass_kg,quantity,timestamp\n1,72.4,1200,2026-03-01T09:30:00Z\n".to_owned(),
    }
}

/// Variables the platform adds to a child process after `env_clear()`.
///
/// macOS CoreFoundation injects `__CF_USER_TEXT_ENCODING` at process start, so
/// the worker cannot avoid inheriting it. It carries the user id and text
/// encoding only; no secret, path or caller state reaches the worker through it.
fn platform_injected_env_vars() -> &'static [&'static str] {
    if cfg!(target_os = "macos") {
        &["__CF_USER_TEXT_ENCODING"]
    } else {
        &[]
    }
}

#[test]
fn three_arms_share_the_same_tasks_and_budget() {
    let (configs, arms) = common::scripted_arms(4, 6, 9);

    // The fairness contract is enforced, not merely documented.
    configs.validate_equal_budget().expect("equal arms");
    for id in ArmId::ALL {
        let config = configs.get(id);
        assert_eq!(config.budget, configs.a.budget, "{id} budget differs");
        assert_eq!(config.model_id, configs.a.model_id, "{id} model differs");
        assert_eq!(config.solver, configs.a.solver, "{id} solver differs");
        assert_eq!(
            config.cost_usd_per_call, configs.a.cost_usd_per_call,
            "{id} price differs"
        );
    }
    assert_eq!(
        (configs.a.memory, configs.b.memory, configs.c.memory),
        (false, true, true)
    );
    assert_eq!(
        (
            configs.a.evolution,
            configs.b.evolution,
            configs.c.evolution
        ),
        (false, false, true)
    );

    // Same task count and the same task order in every arm.
    assert_eq!(arms.a.total(), arms.b.total());
    assert_eq!(arms.b.total(), arms.c.total());
    let order = |outcome: &silyn_eval::ArmOutcome| {
        outcome
            .task_results()
            .iter()
            .map(|result| result.task_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&arms.a), order(&arms.b));
    assert_eq!(order(&arms.b), order(&arms.c));
    assert_eq!(arms.c.successes(), 9);
    assert_eq!(arms.a.successes(), 4);

    // The report records all three arms with the same attempt count.
    let manifest = manifest();
    let report = EvaluationReport::build_at(
        Uuid::from_u128(1),
        &manifest,
        &arms,
        "mock",
        Some("mock"),
        &configs.baseline_id(),
        DateTime::parse_from_rfc3339("2026-10-21T00:00:00Z")
            .expect("timestamp")
            .with_timezone(&Utc),
    );
    assert_eq!(report.arms.a.total, 10);
    assert_eq!(report.arms.b.total, 10);
    assert_eq!(report.arms.c.total, 10);
    assert_eq!(report.repeats, 1);

    // An arm that buys extra attempts is refused.
    let mut rigged = configs.clone();
    rigged.c.budget = TaskBudget::new(2_000, 8, 2.0);
    assert!(rigged.validate_equal_budget().is_err());
}

#[test]
fn sealed_split_is_the_only_test_path() {
    let manifest = manifest();
    assert!(manifest.sealed, "suite must be sealed from csv-stats-v1.1");

    let set = SealedTestSet::load_for_manifest(&manifest).expect("sealed set");
    assert_eq!(set.len(), 10);
    assert_eq!(set.digest(), manifest.splits.test.sha256);

    // A manifest without the sealed flag cannot unlock the test split.
    let mut unsealed = manifest.clone();
    unsealed.sealed = false;
    let error = SealedTestSet::load_for_manifest(&unsealed).expect_err("must be denied");
    assert!(
        matches!(error, silyn_core::SilynError::PolicyDenied(_)),
        "unexpected error: {error}"
    );

    // A stale test digest cannot unlock it either.
    let mut stale = manifest.clone();
    stale.splits.test.sha256 = "0".repeat(64);
    assert!(SealedTestSet::load_for_manifest(&stale).is_err());

    // The arm runner refuses the sealed split even through a backend.
    let evaluator = Evaluator::new(1).expect("evaluator");
    let backend = InProcessBackend::new(&BaselineSolver);
    let config = ArmsConfig::baseline("mock", fast_budget()).a;
    let error = evaluator
        .run_split(&backend, &config, silyn_eval::Split::Test)
        .expect_err("sealed split must be denied");
    assert!(
        matches!(error, silyn_core::SilynError::PolicyDenied(_)),
        "unexpected error: {error}"
    );

    // A wrong submission never scores, so the sealed set cannot be gamed.
    struct Wrong;
    impl Solver for Wrong {
        fn solve(&self, _input: &TaskInput) -> Result<Submission, silyn_core::SilynError> {
            Ok(common::wrong_submission())
        }
    }
    let outcome = set.evaluate(&Wrong, 1);
    assert_eq!(outcome.successes(), 0);
    assert_eq!(outcome.total(), 10);
}

#[test]
fn worker_scrubs_environment_and_sets_fresh_cwd() {
    let backend = SubprocessBackend::new(worker_exe()).with_diagnostics(true);
    let run = backend.run_detailed(&project_task(), &fast_budget());
    let diagnostics = run.diagnostics.expect("worker diagnostics");
    assert!(
        run.attempt.submission().is_some(),
        "worker failed: {} / {}",
        run.stderr,
        run.attempt
            .failure()
            .map(|failure| failure.detail().to_owned())
            .unwrap_or_default()
    );

    let allowed: BTreeSet<String> = allowed_env_vars()
        .iter()
        .map(|key| (*key).to_owned())
        .collect();
    let broadcast: BTreeSet<String> = diagnostics.env_keys.iter().cloned().collect();
    let mut permitted = allowed.clone();
    permitted.extend(
        platform_injected_env_vars()
            .iter()
            .map(|key| (*key).to_owned()),
    );
    assert!(
        broadcast.is_subset(&permitted),
        "worker inherited variables outside the allowlist: {:?}",
        broadcast.difference(&permitted).collect::<Vec<_>>()
    );
    assert_eq!(
        diagnostics.allowed_env.into_iter().collect::<BTreeSet<_>>(),
        allowed
    );
    assert!(broadcast.contains("PATH"), "PATH must reach the worker");
    // The parent process has a much larger environment; the child must not.
    assert!(
        std::env::vars().count() > broadcast.len(),
        "environment was not cleared"
    );

    // A fresh directory per attempt, removed afterwards, and a per-task database
    // derived from it so no two attempts share state.
    assert!(
        diagnostics.cwd.contains("silyn-eval-task-"),
        "unexpected cwd: {}",
        diagnostics.cwd
    );
    assert_eq!(
        Path::new(&diagnostics.database_path)
            .parent()
            .expect("database parent"),
        Path::new(&diagnostics.cwd)
    );
    assert!(
        !Path::new(&diagnostics.cwd).exists(),
        "task directory was not cleaned up"
    );
}

#[test]
fn timeout_is_a_task_failure_not_a_crash() {
    let backend = SubprocessBackend::new(worker_exe()).with_fault(WorkerFault::Sleep);
    let evaluator = Evaluator::new(1).expect("evaluator");
    let configs = ArmsConfig::baseline("mock", TaskBudget::new(250, 8, 2.0));
    let tasks = vec![
        task("task-000", oracle(4, 2, 1)),
        task("task-001", oracle(5, 1, 2)),
    ];

    let outcome = evaluator.run_tasks(&tasks, &backend, &configs.a);
    assert_eq!(outcome.total(), 2, "the run must continue after a timeout");
    assert_eq!(outcome.successes(), 0);
    assert_eq!(outcome.tool_errors(), 2);
    assert_eq!(outcome.failures().len(), 2);
    assert!(
        outcome
            .failures()
            .iter()
            .all(|failure| failure.kind == TaskFailureKind::Timeout)
    );
}

#[test]
fn invalid_stdout_is_a_task_failure() {
    let backend = SubprocessBackend::new(worker_exe()).with_fault(WorkerFault::Garbage);
    let evaluator = Evaluator::new(1).expect("evaluator");
    let configs = ArmsConfig::baseline("mock", fast_budget());
    let tasks = vec![task("task-000", oracle(4, 2, 1))];

    let outcome = evaluator.run_tasks(&tasks, &backend, &configs.a);
    assert_eq!(outcome.total(), 1);
    assert_eq!(outcome.successes(), 0);
    assert_eq!(outcome.failures()[0].kind, TaskFailureKind::InvalidOutput);
}

#[test]
fn panicking_worker_does_not_abort_the_run() {
    for fault in [WorkerFault::Panic, WorkerFault::NonZero] {
        let backend = SubprocessBackend::new(worker_exe()).with_fault(fault);
        let evaluator = Evaluator::new(1).expect("evaluator");
        let configs = ArmsConfig::baseline("mock", fast_budget());
        let tasks = vec![
            task("task-000", oracle(4, 2, 1)),
            task("task-001", oracle(5, 1, 2)),
        ];
        let outcome = evaluator.run_tasks(&tasks, &backend, &configs.a);
        assert_eq!(outcome.total(), 2, "{fault:?} stopped the run");
        assert_eq!(outcome.successes(), 0);
        assert_eq!(outcome.tool_errors(), 2);
        assert!(
            outcome
                .failures()
                .iter()
                .all(|failure| failure.kind == TaskFailureKind::NonZeroExit)
        );
    }

    // A solver that panics inside the harness is a task failure too.
    struct Panicking;
    impl Solver for Panicking {
        fn solve(&self, _input: &TaskInput) -> Result<Submission, silyn_core::SilynError> {
            panic!("solver panic");
        }
    }
    let evaluator = Evaluator::new(1).expect("evaluator");
    let configs = ArmsConfig::baseline("mock", fast_budget());
    let backend = InProcessBackend::new(&Panicking);
    let outcome = evaluator.run_tasks(&[task("task-000", oracle(4, 2, 1))], &backend, &configs.a);
    assert_eq!(outcome.total(), 1);
    assert_eq!(outcome.successes(), 0);
    assert_eq!(outcome.failures()[0].kind, TaskFailureKind::SolverError);
}

#[test]
fn sealed_run_requires_the_operator_flag() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("report.json");

    let run = |extra: &[&str]| {
        Command::new(worker_exe())
            .current_dir(repo_root())
            .args(["run", "--split", "test"])
            .args(extra)
            .arg("--out")
            .arg(&out)
            .output()
            .expect("run silyn-eval")
    };

    // Without the operator flag the sealed split is refused and nothing is written.
    let denied = run(&[]);
    assert!(!denied.status.success(), "sealed run must be refused");
    let stderr = String::from_utf8_lossy(&denied.stderr);
    assert!(
        stderr.contains("--allow-sealed"),
        "unexpected stderr: {stderr}"
    );
    assert!(!out.exists(), "a refused run must not write a report");

    // With the flag the sealed split runs through isolated workers.
    let allowed = run(&["--allow-sealed", "--repeats", "1"]);
    assert!(
        allowed.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    let text = std::fs::read_to_string(&out).expect("report written");
    let report: serde_json::Value = serde_json::from_str(&text).expect("report is JSON");
    assert_eq!(report["suite_version"], "csv-stats-v1.1");
    assert_eq!(report["arms"]["A"]["total"], 10);
    assert_eq!(report["arms"]["B"]["total"], 10);
    assert_eq!(report["arms"]["C"]["total"], 10);
    assert!(report["verdict"].is_string());

    let stats_path = out.with_extension("stats.json");
    let stats: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stats_path).expect("statistics written"))
            .expect("statistics are JSON");
    assert_eq!(stats["sealed"], true);
    assert_eq!(stats["split"], "test");
    assert_eq!(stats["backend"], "subprocess");
    assert_eq!(stats["unauthorized_actions"], 0);
    assert_eq!(stats["bootstrap"]["resamples"], 1000);
    assert_eq!(stats["comparisons"]["c_minus_a"]["paired_tasks"], 10);

    // The statistics document round-trips through the typed contract.
    let typed: EvaluationStatistics =
        serde_json::from_value(stats).expect("statistics document deserializes");
    assert_eq!(typed.verdict, silyn_eval::VerdictDetail::Reject);
    assert_eq!(typed.bootstrap, Bootstrap::default());
}
