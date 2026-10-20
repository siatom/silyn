//! `EvaluationReport` v1 contract tests.
//!
//! There is no JSON-Schema validator in the dependency set, so the required
//! field set, value domains and arm shape from
//! `schemas/eval_report.schema.json` are asserted explicitly here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use silyn_eval::manifest::SuiteManifest;
use silyn_eval::report::EvaluationReport;
use silyn_eval::task::Split;
use silyn_eval::{BaselineSolver, Evaluator, Verdict};
use uuid::Uuid;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("resolve repository root")
}

fn manifest() -> SuiteManifest {
    let path = repo_root().join("eval/suites/csv-stats-v1/suite.toml");
    SuiteManifest::load(&path).expect("load suite manifest")
}

#[test]
fn report_matches_eval_report_contract() {
    let manifest = manifest();
    manifest.validate().expect("valid manifest");

    let evaluator = Evaluator::new(u32::from(manifest.repeats)).expect("evaluator");
    let arms = evaluator
        .run_matrix(&BaselineSolver, Split::Dev)
        .expect("run arms");
    let created_at = DateTime::parse_from_rfc3339("2026-10-20T12:00:00Z")
        .expect("timestamp")
        .with_timezone(&Utc);
    let candidate_id = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0007);
    let report = EvaluationReport::build_at(
        candidate_id,
        &manifest,
        arms,
        "mock",
        Some("mock"),
        "arm-a-only",
        created_at,
    );

    let json = serde_json::to_value(&report).expect("report serializes");
    let object = json.as_object().expect("report is an object");

    // `additionalProperties: false` and the required list.
    let expected_keys: BTreeSet<&str> = [
        "schema_version",
        "candidate_id",
        "suite_version",
        "model_id",
        "model_reported",
        "baseline_id",
        "repeats",
        "arms",
        "thresholds",
        "verdict",
        "created_at",
    ]
    .into_iter()
    .collect();
    let actual_keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(actual_keys, expected_keys);

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["suite_version"], "csv-stats-v1");
    assert_eq!(json["model_id"], "mock");
    assert_eq!(json["model_reported"], "mock");
    assert_eq!(json["baseline_id"], "arm-a-only");
    assert_eq!(json["repeats"], 1);
    assert_eq!(json["candidate_id"], candidate_id.to_string());
    assert_eq!(
        Uuid::parse_str(json["candidate_id"].as_str().expect("candidate id")).expect("uuid format"),
        candidate_id
    );
    assert!(
        DateTime::parse_from_rfc3339(json["created_at"].as_str().expect("created_at string"))
            .is_ok()
    );

    let arms = json["arms"].as_object().expect("arms object");
    let arm_keys: BTreeSet<&str> = arms.keys().map(String::as_str).collect();
    let expected_arms: BTreeSet<&str> = ["A", "B", "C"].into_iter().collect();
    assert_eq!(arm_keys, expected_arms);
    for (name, arm) in arms {
        let arm = arm.as_object().expect("arm object");
        for field in [
            "successes",
            "total",
            "cost_usd",
            "latency_p50_ms",
            "latency_p95_ms",
            "tool_errors",
        ] {
            assert!(arm.contains_key(field), "arm {name} is missing {field}");
        }
        assert_eq!(arm["total"], 10, "dev split has 10 tasks, repeats = 1");
        assert_eq!(arm["cost_usd"], 0.0);
        assert!(
            arm["successes"].as_u64().expect("successes") <= 10,
            "arm {name} cannot exceed the attempt count"
        );
    }

    let thresholds = json["thresholds"].as_object().expect("thresholds");
    let threshold_keys: BTreeSet<&str> = thresholds.keys().map(String::as_str).collect();
    let expected_thresholds: BTreeSet<&str> =
        ["min_gain_pp", "max_cost_ratio", "max_regression_pp"]
            .into_iter()
            .collect();
    assert_eq!(threshold_keys, expected_thresholds);
    assert_eq!(json["thresholds"]["min_gain_pp"], 10.0);
    assert_eq!(json["thresholds"]["max_cost_ratio"], 2.0);
    assert_eq!(json["thresholds"]["max_regression_pp"], 5.0);

    // W2 arms are identical by construction: no gain, so the honest verdict is
    // reject. A report must never claim a promotion without a measured gain.
    assert_eq!(json["verdict"], "reject");
    assert_eq!(report.verdict, Verdict::Reject);

    // Round-trips through the typed contract.
    let parsed: EvaluationReport = serde_json::from_value(json).expect("report deserializes");
    assert_eq!(parsed.candidate_id, candidate_id);
    assert_eq!(parsed.repeats, 1);
    assert_eq!(parsed.verdict, Verdict::Reject);

    // `--out` content is valid JSON with the same fields.
    let text = report.to_json_pretty().expect("pretty report");
    let reparsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON output");
    assert_eq!(reparsed["schema_version"], 1);
}
