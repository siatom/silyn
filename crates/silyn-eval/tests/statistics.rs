//! Paired bootstrap and verdict tests.

mod common;

use silyn_eval::arm::ArmsConfig;
use silyn_eval::backend::TaskBudget;
use silyn_eval::evaluator::Evaluator;
use silyn_eval::manifest::Thresholds;
use silyn_eval::statistics::{
    Bootstrap, DEFAULT_BOOTSTRAP_SEED, MIN_PAIRED_TASKS, VerdictDetail, bootstrap_ci, decide,
    paired_gain,
};

fn thresholds() -> Thresholds {
    Thresholds {
        min_gain_pp: 10.0,
        max_cost_ratio: 2.0,
        max_regression_pp: 5.0,
        max_cost_usd: 1.0,
    }
}

#[test]
fn bootstrap_ci_is_deterministic_and_handles_degenerate_samples() {
    let bootstrap = Bootstrap::default();
    assert_eq!(bootstrap.resamples, 1000);
    assert_eq!(bootstrap.seed, DEFAULT_BOOTSTRAP_SEED);
    bootstrap.validate().expect("valid bootstrap");

    // Fixed sample, two calls: bit-identical.
    let deltas = [0.0, 100.0, 100.0, 0.0, 50.0, 100.0, 0.0, 100.0, 50.0, 0.0];
    let first = bootstrap_ci(&deltas, bootstrap);
    let second = bootstrap_ci(&deltas, bootstrap);
    assert_eq!(first.0.to_bits(), second.0.to_bits());
    assert_eq!(first.1.to_bits(), second.1.to_bits());
    assert_eq!(first.2.to_bits(), second.2.to_bits());
    assert!(first.1 <= first.0 && first.0 <= first.2, "{first:?}");

    // A different seed may move the bounds, but never the point estimate.
    let other = Bootstrap {
        seed: bootstrap.seed ^ 0x1234,
        ..bootstrap
    };
    let shifted = bootstrap_ci(&deltas, other);
    assert_eq!(shifted.0.to_bits(), first.0.to_bits());

    // Degenerate samples must not panic and must not invent a direction.
    let all_success = [0.0_f64; 10];
    assert_eq!(bootstrap_ci(&all_success, bootstrap), (0.0, 0.0, 0.0));
    let all_failure = [0.0_f64; 10];
    assert_eq!(bootstrap_ci(&all_failure, bootstrap), (0.0, 0.0, 0.0));
    let empty: [f64; 0] = [];
    assert_eq!(bootstrap_ci(&empty, bootstrap), (0.0, 0.0, 0.0));
    // One task: a zero-width interval, never a spurious promotion.
    assert_eq!(bootstrap_ci(&[100.0], bootstrap), (100.0, 100.0, 100.0));

    // Paired gain pairs tasks by id and ignores tasks only one arm ran.
    let evaluator = Evaluator::new(1).expect("evaluator");
    let config = ArmsConfig::baseline("mock", TaskBudget::new(1_000, 8, 2.0)).a;
    let baseline = common::scripted_arm(&evaluator, &config, 5, 10, 0);
    let candidate = common::scripted_arm(&evaluator, &config, 8, 10, 0);
    let gain = paired_gain(&baseline, &candidate, "C-A", bootstrap);
    assert_eq!(gain.comparison, "C-A");
    assert_eq!(gain.paired_tasks, 10);
    assert_eq!(gain.resamples, 1000);
    assert!((gain.point_pp - 30.0).abs() < f64::EPSILON);
    assert!(gain.ci_low_pp >= 0.0 && gain.ci_high_pp <= 100.0);
    assert_eq!(
        gain,
        paired_gain(&baseline, &candidate, "C-A", bootstrap),
        "same seed must reproduce the interval"
    );

    // Tasks are paired by id: a shorter arm pairs only the tasks it ran, and a
    // disjoint arm produces no paired sample instead of a fake comparison.
    let partial = common::scripted_arm(&evaluator, &config, 0, 3, 0);
    let shorter = paired_gain(&baseline, &partial, "B-A", bootstrap);
    assert_eq!(shorter.paired_tasks, 3);

    let foreign_tasks: Vec<silyn_eval::Task> = (0..10)
        .map(|index| common::task(&format!("foreign-{index:03}"), common::oracle(4, 2, 1)))
        .collect();
    let answers = foreign_tasks
        .iter()
        .map(|task| {
            (
                task.input.csv.clone(),
                common::correct_submission(task.metadata.oracle),
            )
        })
        .collect();
    let foreign = evaluator.run_tasks(
        &foreign_tasks,
        &common::ScriptedBackend {
            answers,
            unauthorized: 0,
        },
        &config,
    );
    let unmatched = paired_gain(&baseline, &foreign, "B-A", bootstrap);
    assert_eq!(unmatched.paired_tasks, 0);
    assert_eq!((unmatched.point_pp, unmatched.ci_low_pp), (0.0, 0.0));
}

#[test]
fn verdict_follows_preregistered_thresholds() {
    let bootstrap = Bootstrap::default();
    let thresholds = thresholds();

    // C beats A by a wide margin with a positive lower bound: promote.
    let (_, arms) = common::scripted_arms(0, 0, 10);
    let decision = decide(&arms, &thresholds, bootstrap);
    assert_eq!(
        decision.detail,
        VerdictDetail::Promote,
        "{}",
        decision.reason
    );
    assert_eq!(decision.verdict(), silyn_eval::Verdict::Promote);
    assert!(decision.c_minus_a.ci_low_pp > 0.0);

    // No measured gain: reject.
    let (_, arms) = common::scripted_arms(0, 0, 0);
    let decision = decide(&arms, &thresholds, bootstrap);
    assert_eq!(decision.detail, VerdictDetail::Reject);
    assert!(decision.reason.contains("minimum"));

    // The policy arm loses to the memory arm: safety regression.
    let (_, arms) = common::scripted_arms(0, 10, 0);
    let decision = decide(&arms, &thresholds, bootstrap);
    assert_eq!(decision.detail, VerdictDetail::Reject);
    assert!(decision.reason.contains("regresses"));

    // Unauthorized actions always reject, even with a large gain.
    let (_, arms) = common::scripted_arms_with(0, 0, 10, 10, 1);
    let decision = decide(&arms, &thresholds, bootstrap);
    assert_eq!(decision.detail, VerdictDetail::Reject);
    assert!(decision.reason.contains("unauthorized"));

    // Cost above the registered ratio rejects (both arms stay under the absolute
    // cap, so only the ratio can trigger).
    let evaluator = Evaluator::new(1).expect("evaluator");
    let mut free = ArmsConfig::baseline("mock", TaskBudget::new(1_000, 8, 2.0));
    free.b.cost_usd_per_call = 0.01;
    let mut expensive = free.clone();
    expensive.c.cost_usd_per_call = 0.05;
    let arms = silyn_eval::Arms {
        a: common::scripted_arm(&evaluator, &free.a, 0, 10, 0),
        b: common::scripted_arm(&evaluator, &free.b, 0, 10, 0),
        c: common::scripted_arm(&evaluator, &expensive.c, 10, 10, 0),
    };
    let decision = decide(&arms, &thresholds, bootstrap);
    assert_eq!(decision.detail, VerdictDetail::Reject);
    assert!(
        decision.reason.contains("costs"),
        "unexpected reason: {}",
        decision.reason
    );

    // A single paired task is inconclusive, and the schema has no such value, so
    // the report carries `reject`.
    let (_, arms) = common::scripted_arms_with(0, 0, 1, 1, 0);
    let decision = decide(&arms, &thresholds, bootstrap);
    assert_eq!(decision.detail, VerdictDetail::Inconclusive);
    assert_eq!(decision.verdict(), silyn_eval::Verdict::Reject);
    assert_eq!(decision.c_minus_a.paired_tasks, MIN_PAIRED_TASKS - 1);

    // Unequal attempt counts make the run invalid rather than comparable.
    let arms = silyn_eval::Arms {
        a: common::scripted_arm(&evaluator, &free.a, 0, 10, 0),
        b: common::scripted_arm(&evaluator, &free.b, 0, 4, 0),
        c: common::scripted_arm(&evaluator, &free.c, 0, 10, 0),
    };
    let decision = decide(&arms, &thresholds, bootstrap);
    assert_eq!(decision.detail, VerdictDetail::Invalid);
    assert_eq!(decision.verdict(), silyn_eval::Verdict::Invalid);

    // Arm A may not be empty either.
    let arms = silyn_eval::Arms {
        a: common::scripted_arm(&evaluator, &free.a, 0, 0, 0),
        b: common::scripted_arm(&evaluator, &free.b, 0, 10, 0),
        c: common::scripted_arm(&evaluator, &free.c, 10, 10, 0),
    };
    assert_eq!(
        decide(&arms, &thresholds, bootstrap).detail,
        VerdictDetail::Invalid
    );

    // Invalid bootstrap settings are schema errors, not panics.
    assert!(
        Bootstrap {
            resamples: 0,
            ..bootstrap
        }
        .validate()
        .is_err()
    );
    assert!(
        Bootstrap {
            confidence_pct: 100.0,
            ..bootstrap
        }
        .validate()
        .is_err()
    );
}
