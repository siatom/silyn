//! Sealed test split contract.

use std::sync::Mutex;

use silyn_core::SilynError;
use silyn_eval::generator::{generate_split, split_sha256};
use silyn_eval::task::{Split, Submission, TaskInput, Unit};
use silyn_eval::{BaselineSolver, Evaluator, SealedTestSet, Solver};

#[derive(Debug, Default)]
struct ProbeSolver {
    /// Serialized key sets seen by the solver, one per task.
    seen_keys: Mutex<Vec<Vec<String>>>,
}

impl Solver for ProbeSolver {
    fn solve(&self, input: &TaskInput) -> Result<Submission, SilynError> {
        let value = serde_json::to_value(input).expect("TaskInput is serializable");
        let mut keys: Vec<String> = value
            .as_object()
            .expect("TaskInput is an object")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        self.seen_keys.lock().expect("probe lock").push(keys);

        // Deliberately wrong on every field: the sealed set must not award a
        // success just because a solver returned something.
        Ok(Submission {
            row_count: 0,
            missing_total: 0,
            duplicate_rows: 0,
            unit: Unit::Kg,
        })
    }
}

#[test]
fn test_split_is_only_reachable_through_the_sealed_set() {
    // The generator refuses to hand out test tasks.
    assert!(matches!(
        generate_split(Split::Test),
        Err(SilynError::PolicyDenied(_))
    ));

    // The arm runner refuses the sealed split too.
    let evaluator = Evaluator::new(1).expect("evaluator");
    assert!(matches!(
        evaluator.run_arm(&BaselineSolver, Split::Test),
        Err(SilynError::PolicyDenied(_))
    ));

    let set = SealedTestSet::load_default();
    assert_eq!(set.len(), Split::Test.count() as usize);
    assert!(!set.is_empty());
    assert_eq!(
        set.digest(),
        split_sha256(Split::Test).expect("test split digest")
    );

    // Debug formatting must not leak test tasks or oracles either.
    let rendered = format!("{set:?}");
    assert!(!rendered.contains("oracle"), "debug leaked an oracle");
    assert!(!rendered.contains("row_count"), "debug leaked task data");

    let probe = ProbeSolver::default();
    let outcome = set.evaluate(&probe, 1);
    assert_eq!(outcome.total(), Split::Test.count());
    assert_eq!(outcome.successes(), 0, "a wrong submission must not score");
    assert_eq!(outcome.tool_errors(), 0);
    assert_eq!(outcome.task_results().len(), Split::Test.count() as usize);
    assert!(
        outcome
            .task_results()
            .iter()
            .all(|result| result.successes == 0 && result.attempts == 1)
    );

    // Candidates only ever see the objective and the CSV text.
    let seen = probe.seen_keys.lock().expect("probe lock");
    assert_eq!(seen.len(), Split::Test.count() as usize);
    let expected = ["csv".to_owned(), "objective".to_owned()];
    for keys in seen.iter() {
        assert_eq!(keys.as_slice(), expected.as_slice());
    }

    // Regression guard on the public surface: only len, is_empty, digest and
    // evaluate may be reachable; no getter hands out tasks or oracles.
    let source = include_str!("../src/sealed.rs");
    for forbidden in [
        ["pub", "fn", "tasks"].join(" "),
        ["pub", "fn", "task"].join(" "),
        ["pub", "fn", "oracles"].join(" "),
        ["pub", "fn", "oracle"].join(" "),
    ] {
        assert!(!source.contains(&forbidden), "found `{forbidden}`");
    }
}
