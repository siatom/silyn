//! Paired bootstrap statistics and the pre-registered verdict.
//!
//! Every arm runs the same tasks in the same order, so the comparison between
//! arms is paired at the task level. For each task the harness computes the
//! success-rate difference in percentage points, then resamples the tasks with
//! replacement ([`Bootstrap`]) to obtain a percentile confidence interval.
//!
//! The resampler uses the same SplitMix64 generator as the task generator and a
//! recorded seed, so a fixed seed always yields the same interval. Degenerate
//! samples (all successes, all failures, or no paired tasks) return a zero-width
//! interval instead of panicking.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use silyn_core::SilynError;

use crate::arm::{ArmConfig, ArmId, Arms, ArmsConfig};
use crate::evaluator::{ArmOutcome, Evaluator};
use crate::manifest::{SuiteManifest, Thresholds};
use crate::rng::SplitMix64;
use crate::task::Split;

/// Resamples per comparison, fixed by ADR 0004.
pub const DEFAULT_RESAMPLES: u32 = 1_000;

/// Confidence level, in percent.
pub const DEFAULT_CONFIDENCE_PCT: f64 = 95.0;

/// Bootstrap seed. Deliberately independent of the run seed so the interval of a
/// fixed set of outcomes cannot be re-rolled to fit a preferred conclusion.
pub const DEFAULT_BOOTSTRAP_SEED: u64 = 0x5eed_b007_0000_0001;

/// Minimum paired task count for a meaningful interval.
pub const MIN_PAIRED_TASKS: u32 = 2;

/// Promotion outcome as carried by the frozen report schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Promote,
    Reject,
    AwaitApproval,
    Invalid,
}

impl Verdict {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Reject => "reject",
            Self::AwaitApproval => "await_approval",
            Self::Invalid => "invalid",
        }
    }
}

/// Verdict with the extra `inconclusive` detail the schema cannot express.
///
/// `schemas/eval_report.schema.json` only allows `promote`, `reject`,
/// `await_approval` and `invalid`, so an inconclusive run is written to the
/// report as `reject` and kept as `inconclusive` in the statistics document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictDetail {
    Promote,
    Reject,
    Inconclusive,
    Invalid,
}

impl VerdictDetail {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Reject => "reject",
            Self::Inconclusive => "inconclusive",
            Self::Invalid => "invalid",
        }
    }

    /// Maps to the frozen report enum. `inconclusive` becomes `reject` because
    /// an unproven promotion is not a promotion.
    #[must_use]
    pub const fn verdict(self) -> Verdict {
        match self {
            Self::Promote => Verdict::Promote,
            Self::Reject | Self::Inconclusive => Verdict::Reject,
            Self::Invalid => Verdict::Invalid,
        }
    }
}

impl From<VerdictDetail> for Verdict {
    fn from(value: VerdictDetail) -> Self {
        value.verdict()
    }
}

/// Bootstrap configuration, recorded in every statistics document.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bootstrap {
    pub resamples: u32,
    pub confidence_pct: f64,
    pub seed: u64,
}

impl Default for Bootstrap {
    fn default() -> Self {
        Self {
            resamples: DEFAULT_RESAMPLES,
            confidence_pct: DEFAULT_CONFIDENCE_PCT,
            seed: DEFAULT_BOOTSTRAP_SEED,
        }
    }
}

impl Bootstrap {
    pub fn validate(&self) -> Result<(), SilynError> {
        if !(1..=100_000).contains(&self.resamples) {
            return Err(SilynError::Schema(format!(
                "bootstrap resamples must be 1..=100000, got {}",
                self.resamples
            )));
        }
        if !(self.confidence_pct > 0.0 && self.confidence_pct < 100.0) {
            return Err(SilynError::Schema(format!(
                "bootstrap confidence must be in (0, 100), got {}",
                self.confidence_pct
            )));
        }
        Ok(())
    }
}

/// Paired gain between two arms in percentage points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairedGain {
    /// `B-A`, `C-B` or `C-A`.
    pub comparison: String,
    pub point_pp: f64,
    pub ci_low_pp: f64,
    pub ci_high_pp: f64,
    /// Tasks present in both arms.
    pub paired_tasks: u32,
    pub resamples: u32,
    pub confidence_pct: f64,
    pub seed: u64,
}

/// Summary of one arm, as recorded in the statistics document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmStatistics {
    pub label: String,
    pub solver: String,
    pub model_id: String,
    pub memory: bool,
    pub evolution: bool,
    pub cost_usd_per_call: f64,
    pub budget: crate::backend::TaskBudget,
    pub successes: u32,
    pub total: u32,
    pub success_rate_pp: f64,
    pub cost_usd: f64,
    pub duration_ms: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub tool_errors: u32,
    pub unauthorized_actions: u32,
}

impl ArmStatistics {
    #[must_use]
    pub fn new(config: &ArmConfig, outcome: &ArmOutcome) -> Self {
        Self {
            label: config.label.clone(),
            solver: config.solver.clone(),
            model_id: config.model_id.clone(),
            memory: config.memory,
            evolution: config.evolution,
            cost_usd_per_call: config.cost_usd_per_call,
            budget: config.budget,
            successes: outcome.successes(),
            total: outcome.total(),
            success_rate_pp: outcome.success_rate_pp(),
            cost_usd: outcome.cost_usd(),
            duration_ms: outcome.duration_ms(),
            latency_p50_ms: outcome.latency_p50_ms(),
            latency_p95_ms: outcome.latency_p95_ms(),
            tool_errors: outcome.tool_errors(),
            unauthorized_actions: outcome.unauthorized_actions(),
        }
    }
}

/// Arms keyed exactly like the report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsArms {
    #[serde(rename = "A")]
    pub a: ArmStatistics,
    #[serde(rename = "B")]
    pub b: ArmStatistics,
    #[serde(rename = "C")]
    pub c: ArmStatistics,
}

impl StatsArms {
    #[must_use]
    pub fn get(&self, id: ArmId) -> &ArmStatistics {
        match id {
            ArmId::A => &self.a,
            ArmId::B => &self.b,
            ArmId::C => &self.c,
        }
    }
}

/// Paired comparisons in the order the protocol prescribes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Comparisons {
    pub b_minus_a: PairedGain,
    pub c_minus_b: PairedGain,
    pub c_minus_a: PairedGain,
}

/// Suite digests copied from the manifest so a report can be re-checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteDigests {
    pub train: String,
    pub dev: String,
    pub test: String,
}

/// Document next to the schema report: seeds, budgets, durations and the paired
/// confidence intervals that `EvaluationReport` v1 has no fields for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationStatistics {
    pub schema_version: u32,
    pub suite_version: String,
    pub split: String,
    pub sealed: bool,
    pub repeats: u32,
    pub seed: u64,
    pub model_id: String,
    pub baseline_id: String,
    pub backend: String,
    pub bootstrap: Bootstrap,
    pub thresholds: Thresholds,
    pub suite_digests: SuiteDigests,
    pub arms: StatsArms,
    pub comparisons: Comparisons,
    pub verdict: VerdictDetail,
    pub verdict_reason: String,
    pub unauthorized_actions: u32,
}

/// Statistics document schema version.
pub const STATISTICS_SCHEMA_VERSION: u32 = 1;

/// Everything the statistics document needs from one run.
pub struct StatsInput<'a> {
    pub manifest: &'a SuiteManifest,
    pub evaluator: Evaluator,
    pub split: Split,
    pub sealed: bool,
    pub backend: &'a str,
    pub configs: &'a ArmsConfig,
    pub arms: &'a Arms,
    pub bootstrap: Bootstrap,
}

impl EvaluationStatistics {
    /// Builds the statistics document. The arms and their configuration must
    /// come from the same run that produced the outcomes.
    #[must_use]
    pub fn build(input: StatsInput<'_>) -> Self {
        let StatsInput {
            manifest,
            evaluator,
            split,
            sealed,
            backend,
            configs,
            arms,
            bootstrap,
        } = input;
        let decision = decide(arms, &manifest.thresholds, bootstrap);
        let unauthorized_actions = arms
            .iter()
            .map(|(_, outcome)| outcome.unauthorized_actions())
            .sum();
        Self {
            schema_version: STATISTICS_SCHEMA_VERSION,
            suite_version: manifest.version.clone(),
            split: split.as_str().to_owned(),
            sealed,
            repeats: evaluator.repeats(),
            seed: evaluator.seed(),
            model_id: configs.a.model_id.clone(),
            baseline_id: configs.baseline_id(),
            backend: backend.to_owned(),
            bootstrap,
            thresholds: manifest.thresholds,
            suite_digests: SuiteDigests {
                train: manifest.splits.train.sha256.clone(),
                dev: manifest.splits.dev.sha256.clone(),
                test: manifest.splits.test.sha256.clone(),
            },
            arms: StatsArms {
                a: ArmStatistics::new(&configs.a, &arms.a),
                b: ArmStatistics::new(&configs.b, &arms.b),
                c: ArmStatistics::new(&configs.c, &arms.c),
            },
            comparisons: Comparisons {
                b_minus_a: decision.b_minus_a,
                c_minus_b: decision.c_minus_b,
                c_minus_a: decision.c_minus_a,
            },
            verdict: decision.detail,
            verdict_reason: decision.reason,
            unauthorized_actions,
        }
    }

    /// Serializes the statistics document for `--stats-out`.
    pub fn to_json_pretty(&self) -> Result<String, SilynError> {
        serde_json::to_string_pretty(self).map_err(SilynError::from)
    }
}

/// Result of applying the pre-registered thresholds.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub detail: VerdictDetail,
    pub reason: String,
    pub b_minus_a: PairedGain,
    pub c_minus_b: PairedGain,
    pub c_minus_a: PairedGain,
}

impl Decision {
    /// Report-facing verdict (the schema has no `inconclusive`).
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        self.detail.verdict()
    }
}

/// Applies the thresholds registered in the suite manifest.
///
/// Order matters: an unusable run is `invalid`, a safety failure is `reject`, an
/// underpowered run is `inconclusive`, and only a measured gain with a positive
/// lower confidence bound can be `promote`.
#[must_use]
pub fn decide(arms: &Arms, thresholds: &Thresholds, bootstrap: Bootstrap) -> Decision {
    let b_minus_a = paired_gain(&arms.a, &arms.b, "B-A", bootstrap);
    let c_minus_b = paired_gain(&arms.b, &arms.c, "C-B", bootstrap);
    let c_minus_a = paired_gain(&arms.a, &arms.c, "C-A", bootstrap);

    let invalid = |reason: &str| Decision {
        detail: VerdictDetail::Invalid,
        reason: reason.to_owned(),
        b_minus_a: b_minus_a.clone(),
        c_minus_b: c_minus_b.clone(),
        c_minus_a: c_minus_a.clone(),
    };
    let reject = |reason: &str| Decision {
        detail: VerdictDetail::Reject,
        reason: reason.to_owned(),
        b_minus_a: b_minus_a.clone(),
        c_minus_b: c_minus_b.clone(),
        c_minus_a: c_minus_a.clone(),
    };

    if arms.a.total() == 0 || arms.b.total() == 0 || arms.c.total() == 0 {
        return invalid("an arm recorded no attempts");
    }
    if arms.a.total() != arms.b.total() || arms.b.total() != arms.c.total() {
        return invalid("the arms did not run the same number of attempts");
    }
    if c_minus_a.paired_tasks < MIN_PAIRED_TASKS {
        return Decision {
            detail: VerdictDetail::Inconclusive,
            reason: "not enough paired tasks to estimate a gain".to_owned(),
            b_minus_a,
            c_minus_b,
            c_minus_a,
        };
    }

    let unauthorized: u32 = arms
        .iter()
        .map(|(_, outcome)| outcome.unauthorized_actions())
        .sum();
    if unauthorized != 0 {
        return reject("unauthorized actions were recorded");
    }
    if arms.a.cost_usd() > thresholds.max_cost_usd
        || arms.b.cost_usd() > thresholds.max_cost_usd
        || arms.c.cost_usd() > thresholds.max_cost_usd
    {
        return reject("the absolute cost cap was exceeded");
    }
    if cost_ratio(arms.b.cost_usd(), arms.c.cost_usd()) > thresholds.max_cost_ratio {
        return reject("the policy arm costs more than the pre-registered ratio");
    }
    // A safety regression: the policy arm must not lose to the memory arm by
    // more than the registered allowance.
    let regression_pp = arms.b.success_rate_pp() - arms.c.success_rate_pp();
    if regression_pp > thresholds.max_regression_pp {
        return reject("the policy arm regresses against the memory arm");
    }
    if c_minus_a.point_pp < thresholds.min_gain_pp {
        return reject("the measured gain is below the pre-registered minimum");
    }
    if c_minus_a.ci_low_pp <= 0.0 {
        return reject("the lower confidence bound of the gain is not above zero");
    }

    Decision {
        detail: VerdictDetail::Promote,
        reason: "every pre-registered threshold is met".to_owned(),
        b_minus_a,
        c_minus_b,
        c_minus_a,
    }
}

fn cost_ratio(baseline: f64, candidate: f64) -> f64 {
    if baseline <= f64::EPSILON {
        if candidate <= f64::EPSILON {
            1.0
        } else {
            f64::INFINITY
        }
    } else {
        candidate / baseline
    }
}

/// Paired task-level gain of `candidate` over `baseline` with a percentile
/// bootstrap interval. Tasks are paired by id, so a mismatch is reported as
/// zero paired tasks rather than silently comparing different work.
#[must_use]
pub fn paired_gain(
    baseline: &ArmOutcome,
    candidate: &ArmOutcome,
    comparison: &str,
    bootstrap: Bootstrap,
) -> PairedGain {
    let candidate_rates: BTreeMap<&str, f64> = candidate
        .task_results()
        .iter()
        .map(|result| (result.task_id.as_str(), result.success_rate_pp()))
        .collect();

    let deltas: Vec<f64> = baseline
        .task_results()
        .iter()
        .filter_map(|result| {
            candidate_rates
                .get(result.task_id.as_str())
                .map(|candidate_rate| candidate_rate - result.success_rate_pp())
        })
        .collect();

    let (point_pp, ci_low_pp, ci_high_pp) = bootstrap_ci(&deltas, bootstrap);
    PairedGain {
        comparison: comparison.to_owned(),
        point_pp,
        ci_low_pp,
        ci_high_pp,
        paired_tasks: u32::try_from(deltas.len()).unwrap_or(u32::MAX),
        resamples: bootstrap.resamples,
        confidence_pct: bootstrap.confidence_pct,
        seed: bootstrap.seed,
    }
}

/// Mean of the paired deltas plus the percentile interval of the resampled
/// means. Empty and constant samples yield a zero-width interval.
#[must_use]
pub fn bootstrap_ci(deltas: &[f64], bootstrap: Bootstrap) -> (f64, f64, f64) {
    let count = deltas.len();
    if count == 0 {
        return (0.0, 0.0, 0.0);
    }
    let point = deltas.iter().sum::<f64>() / count as f64;

    // A constant sample cannot produce a different interval, so skip the work and
    // keep the degenerate cases exact.
    let constant = deltas
        .iter()
        .all(|value| value.total_cmp(&deltas[0]).is_eq());
    if constant {
        return (point, point, point);
    }

    let resamples = bootstrap.resamples.max(1) as usize;
    let mut rng = SplitMix64::new(bootstrap.seed);
    let mut means = Vec::with_capacity(resamples);
    for _ in 0..resamples {
        let mut sum = 0.0;
        for _ in 0..count {
            sum += deltas[rng.below(count as u64) as usize];
        }
        means.push(sum / count as f64);
    }
    means.sort_by(f64::total_cmp);

    let tail = (100.0 - bootstrap.confidence_pct) / 2.0;
    let low = percentile(&means, tail);
    let high = percentile(&means, 100.0 - tail);
    (point, low, high)
}

fn percentile(sorted: &[f64], percent: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let last = sorted.len() - 1;
    let position = (percent / 100.0) * last as f64;
    let index = (position.round() as usize).min(last);
    sorted[index]
}
