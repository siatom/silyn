//! `EvaluationReport` v1 (`schemas/eval_report.schema.json`).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use silyn_core::SilynError;
use uuid::Uuid;

use crate::evaluator::{ArmOutcome, Arms};
use crate::manifest::{SuiteManifest, Thresholds};

/// Report schema version. Bump only with a new schema.
pub const SCHEMA_VERSION: u32 = 1;

/// Promotion outcome. W2 never auto-promotes: the harness can only recommend
/// `await_approval` or `reject`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Promote,
    Reject,
    AwaitApproval,
    Invalid,
}

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

impl From<ArmOutcome> for ArmReport {
    fn from(value: ArmOutcome) -> Self {
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

impl From<Arms> for ArmsReport {
    fn from(value: Arms) -> Self {
        Self {
            a: value.a.into(),
            b: value.b.into(),
            c: value.c.into(),
        }
    }
}

impl EvaluationReport {
    /// Builds a report at the current time.
    #[must_use]
    pub fn build(
        candidate_id: Uuid,
        manifest: &SuiteManifest,
        arms: Arms,
        model_id: &str,
        model_reported: Option<&str>,
        baseline_id: &str,
    ) -> Self {
        Self::build_at(
            candidate_id,
            manifest,
            arms,
            model_id,
            model_reported,
            baseline_id,
            Utc::now(),
        )
    }

    /// Builds a report with an explicit timestamp (used by tests).
    #[must_use]
    pub fn build_at(
        candidate_id: Uuid,
        manifest: &SuiteManifest,
        arms: Arms,
        model_id: &str,
        model_reported: Option<&str>,
        baseline_id: &str,
        created_at: DateTime<Utc>,
    ) -> Self {
        let verdict = decide(&arms, &manifest.thresholds);
        Self {
            schema_version: SCHEMA_VERSION,
            candidate_id,
            suite_version: manifest.version.clone(),
            model_id: model_id.to_owned(),
            model_reported: model_reported.map(ToOwned::to_owned),
            baseline_id: baseline_id.to_owned(),
            repeats: u32::from(manifest.repeats),
            arms: ArmsReport::from(arms),
            thresholds: ReportThresholds::from(&manifest.thresholds),
            verdict,
            created_at,
        }
    }

    /// Serializes the report for `--out`.
    pub fn to_json_pretty(&self) -> Result<String, SilynError> {
        serde_json::to_string_pretty(self).map_err(SilynError::from)
    }
}

/// Applies the pre-registered thresholds to the observed arms.
///
/// - any empty or mismatched arm makes the run `invalid`;
/// - `C` must beat `B` by at least `min_gain_pp`;
/// - `C` must not regress against `A` by more than `max_regression_pp`;
/// - `C` must not cost more than `max_cost_ratio` times `B`;
/// - a passing run still needs human approval (`await_approval`).
#[must_use]
pub fn decide(arms: &Arms, thresholds: &Thresholds) -> Verdict {
    if arms.a.total() == 0
        || arms.b.total() == 0
        || arms.c.total() == 0
        || arms.a.total() != arms.b.total()
        || arms.b.total() != arms.c.total()
    {
        return Verdict::Invalid;
    }

    let gain_pp = arms.c.success_rate_pp() - arms.b.success_rate_pp();
    let regression_pp = arms.a.success_rate_pp() - arms.c.success_rate_pp();
    let cost_ratio = cost_ratio(arms.b.cost_usd(), arms.c.cost_usd());

    if gain_pp >= thresholds.min_gain_pp
        && regression_pp <= thresholds.max_regression_pp
        && cost_ratio <= thresholds.max_cost_ratio
    {
        Verdict::AwaitApproval
    } else {
        Verdict::Reject
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
