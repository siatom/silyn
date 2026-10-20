//! `EvaluationReport` v1 (`schemas/eval_report.schema.json`) and its Markdown
//! rendering.
//!
//! The schema is frozen and closes the object (`additionalProperties: false`),
//! so durations, seeds, arm configuration and paired confidence intervals live
//! in the sibling [`EvaluationStatistics`] document. Both are written by one
//! run and the Markdown report is rendered from exactly those two documents, so
//! it can never introduce a number the JSON artifacts do not contain.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use silyn_core::SilynError;
use uuid::Uuid;

use crate::arm::Arms;
use crate::evaluator::ArmOutcome;
use crate::manifest::{SuiteManifest, Thresholds};
use crate::statistics::{Bootstrap, Decision, EvaluationStatistics};

pub use crate::statistics::Verdict;

/// Report schema version. Bump only with a new schema.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReport {
    pub schema_version: u32,
    pub candidate_id: Uuid,
    pub suite_version: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_reported: Option<String>,
    pub baseline_id: String,
    pub repeats: u32,
    pub arms: ArmsReport,
    pub thresholds: ReportThresholds,
    pub verdict: Verdict,
    pub created_at: DateTime<Utc>,
}

/// Arms keyed exactly as the schema requires: `A`, `B`, `C`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmsReport {
    #[serde(rename = "A")]
    pub a: ArmReport,
    #[serde(rename = "B")]
    pub b: ArmReport,
    #[serde(rename = "C")]
    pub c: ArmReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmReport {
    pub successes: u32,
    pub total: u32,
    pub cost_usd: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    pub tool_errors: u32,
}

/// The report carries the three decision thresholds; the absolute cost cap stays
/// in the suite manifest because the schema does not include it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportThresholds {
    pub min_gain_pp: f64,
    pub max_cost_ratio: f64,
    pub max_regression_pp: f64,
}

impl From<&Thresholds> for ReportThresholds {
    fn from(value: &Thresholds) -> Self {
        Self {
            min_gain_pp: value.min_gain_pp,
            max_cost_ratio: value.max_cost_ratio,
            max_regression_pp: value.max_regression_pp,
        }
    }
}

impl From<&ArmOutcome> for ArmReport {
    fn from(value: &ArmOutcome) -> Self {
        Self {
            successes: value.successes(),
            total: value.total(),
            cost_usd: value.cost_usd(),
            latency_p50_ms: value.latency_p50_ms(),
            latency_p95_ms: value.latency_p95_ms(),
            tool_errors: value.tool_errors(),
        }
    }
}

impl From<&Arms> for ArmsReport {
    fn from(value: &Arms) -> Self {
        Self {
            a: ArmReport::from(&value.a),
            b: ArmReport::from(&value.b),
            c: ArmReport::from(&value.c),
        }
    }
}

/// Input for [`EvaluationReport::build`].
///
/// `repeats` is explicit because the operator may run fewer or more repeats than
/// the manifest default; the report must state what actually ran.
pub struct ReportInput<'a> {
    pub candidate_id: Uuid,
    pub manifest: &'a SuiteManifest,
    pub arms: &'a Arms,
    pub repeats: u32,
    pub model_id: &'a str,
    pub model_reported: Option<&'a str>,
    pub baseline_id: &'a str,
    pub created_at: DateTime<Utc>,
}

impl EvaluationReport {
    /// Builds a report from a recorded run.
    #[must_use]
    pub fn build(input: ReportInput<'_>) -> Self {
        let ReportInput {
            candidate_id,
            manifest,
            arms,
            repeats,
            model_id,
            model_reported,
            baseline_id,
            created_at,
        } = input;
        let decision = decide_with(arms, &manifest.thresholds, Bootstrap::default());
        Self {
            schema_version: SCHEMA_VERSION,
            candidate_id,
            suite_version: manifest.version.clone(),
            model_id: model_id.to_owned(),
            model_reported: model_reported.map(ToOwned::to_owned),
            baseline_id: baseline_id.to_owned(),
            repeats,
            arms: ArmsReport::from(arms),
            thresholds: ReportThresholds::from(&manifest.thresholds),
            verdict: decision.verdict(),
            created_at,
        }
    }

    /// Builds a report with an explicit timestamp and the manifest repeat count
    /// (used by tests that do not pin the repeat count).
    #[must_use]
    pub fn build_at(
        candidate_id: Uuid,
        manifest: &SuiteManifest,
        arms: &Arms,
        model_id: &str,
        model_reported: Option<&str>,
        baseline_id: &str,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self::build(ReportInput {
            candidate_id,
            manifest,
            arms,
            repeats: u32::from(manifest.repeats),
            model_id,
            model_reported,
            baseline_id,
            created_at,
        })
    }

    /// Serializes the report for `--out`.
    pub fn to_json_pretty(&self) -> Result<String, SilynError> {
        serde_json::to_string_pretty(self).map_err(SilynError::from)
    }
}

/// Applies the pre-registered thresholds to the observed arms.
///
/// - any empty or mismatched arm makes the run `invalid`;
/// - unauthorized actions, a cost overrun or a safety regression make it `reject`;
/// - `C` must beat `A` by at least `min_gain_pp` with a positive lower bootstrap
///   bound; only then is the run `promote`.
#[must_use]
pub fn decide(arms: &Arms, thresholds: &Thresholds) -> Verdict {
    decide_with(arms, thresholds, Bootstrap::default()).verdict()
}

/// Like [`decide`], keeping the reason and the paired intervals.
#[must_use]
pub fn decide_with(arms: &Arms, thresholds: &Thresholds, bootstrap: Bootstrap) -> Decision {
    crate::statistics::decide(arms, thresholds, bootstrap)
}

/// Renders the Markdown report from the two JSON documents of one run.
///
/// Every number in the output is a field of `report` or `stats`; there are no
/// hand-written figures, derived percentages or percentages rounded differently
/// from the JSON.
#[must_use]
pub fn render_markdown(report: &EvaluationReport, stats: &EvaluationStatistics) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Evaluation report\n");
    let _ = writeln!(out, "| field | value |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| suite | `{}` |", report.suite_version);
    let _ = writeln!(out, "| split | `{}` |", stats.split);
    let _ = writeln!(out, "| sealed | {} |", stats.sealed);
    let _ = writeln!(out, "| candidate | `{}` |", report.candidate_id);
    let _ = writeln!(out, "| model | `{}` |", report.model_id);
    let _ = writeln!(
        out,
        "| model reported | {} |",
        report.model_reported.as_deref().unwrap_or("n/a")
    );
    let _ = writeln!(out, "| baseline | `{}` |", report.baseline_id);
    let _ = writeln!(out, "| repeats | {} |", report.repeats);
    let _ = writeln!(out, "| run seed | {} |", stats.seed);
    let _ = writeln!(out, "| backend | `{}` |", stats.backend);
    let _ = writeln!(out, "| created at | `{}` |", report.created_at);

    let budget = &stats.arms.a.budget;
    let _ = writeln!(out, "\n## Budget\n");
    let _ = writeln!(out, "Identical for every arm.\n");
    let _ = writeln!(out, "| field | value |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| task timeout (ms) | {} |", budget.timeout_ms);
    let _ = writeln!(out, "| max steps | {} |", budget.max_steps);
    let _ = writeln!(out, "| max cost (usd) | {} |", budget.max_cost_usd);

    let _ = writeln!(out, "\n## Arms\n");
    let _ = writeln!(
        out,
        "| arm | label | solver | memory | evolution | successes | attempts | \
         success rate (pp) | cost (usd) | duration (ms) | median latency (ms) | \
         95th percentile latency (ms) | tool errors | unauthorized actions |"
    );
    let _ = writeln!(
        out,
        "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
    );
    for (id, arm) in [
        ("A", &stats.arms.a),
        ("B", &stats.arms.b),
        ("C", &stats.arms.c),
    ] {
        let _ = writeln!(
            out,
            "| {id} | {} | `{}` | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            arm.label,
            arm.solver,
            arm.memory,
            arm.evolution,
            arm.successes,
            arm.total,
            arm.success_rate_pp,
            arm.cost_usd,
            arm.duration_ms,
            arm.latency_p50_ms,
            arm.latency_p95_ms,
            arm.tool_errors,
            arm.unauthorized_actions,
        );
    }

    let bootstrap = &stats.bootstrap;
    let _ = writeln!(out, "\n## Paired gains\n");
    let _ = writeln!(
        out,
        "Task-level paired gains in percentage points, {} resamples, {} % confidence, \
         bootstrap seed {}.\n",
        bootstrap.resamples, bootstrap.confidence_pct, bootstrap.seed
    );
    let _ = writeln!(
        out,
        "| comparison | point (pp) | CI low (pp) | CI high (pp) | paired tasks |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|");
    for gain in [
        &stats.comparisons.b_minus_a,
        &stats.comparisons.c_minus_b,
        &stats.comparisons.c_minus_a,
    ] {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            gain.comparison, gain.point_pp, gain.ci_low_pp, gain.ci_high_pp, gain.paired_tasks,
        );
    }

    let thresholds = &stats.thresholds;
    let _ = writeln!(out, "\n## Thresholds\n");
    let _ = writeln!(out, "| threshold | value |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| min_gain_pp | {} |", thresholds.min_gain_pp);
    let _ = writeln!(out, "| max_cost_ratio | {} |", thresholds.max_cost_ratio);
    let _ = writeln!(
        out,
        "| max_regression_pp | {} |",
        thresholds.max_regression_pp
    );
    let _ = writeln!(out, "| max_cost_usd | {} |", thresholds.max_cost_usd);

    let _ = writeln!(out, "\n## Verdict\n");
    let _ = writeln!(out, "| field | value |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| verdict | `{}` |", report.verdict.as_str());
    let _ = writeln!(out, "| detail | `{}` |", stats.verdict.as_str());
    let _ = writeln!(out, "| reason | {} |", stats.verdict_reason);
    let _ = writeln!(
        out,
        "| unauthorized actions | {} |",
        stats.unauthorized_actions
    );

    let _ = writeln!(out, "\n## Suite digests\n");
    let _ = writeln!(out, "| split | digest |");
    let _ = writeln!(out, "|---|---|");
    let _ = writeln!(out, "| train | `{}` |", stats.suite_digests.train);
    let _ = writeln!(out, "| dev | `{}` |", stats.suite_digests.dev);
    let _ = writeln!(out, "| test | `{}` |", stats.suite_digests.test);

    let _ = writeln!(
        out,
        "\nGenerated by `silyn-eval run`; every figure above is a field of the JSON \
         report or its statistics companion."
    );
    out
}

/// Every key the frozen schema requires, in schema order.
#[must_use]
pub fn required_report_fields() -> BTreeSet<&'static str> {
    [
        "schema_version",
        "candidate_id",
        "suite_version",
        "model_id",
        "baseline_id",
        "repeats",
        "arms",
        "thresholds",
        "verdict",
        "created_at",
    ]
    .into_iter()
    .collect()
}

/// Optional report fields allowed by the schema.
#[must_use]
pub fn optional_report_fields() -> BTreeSet<&'static str> {
    ["model_reported"].into_iter().collect()
}
