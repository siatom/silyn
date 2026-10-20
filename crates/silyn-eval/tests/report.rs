//! `EvaluationReport` v1 contract tests and Markdown/JSON consistency.
//!
//! There is no JSON-Schema validator in the dependency set, so the schema is
//! asserted field by field: required keys, exact key sets (`additionalProperties:
//! false`), value domains and the arm/threshold shapes from
//! `schemas/eval_report.schema.json`.

mod common;

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use silyn_eval::arm::ArmsConfig;
use silyn_eval::evaluator::Evaluator;
use silyn_eval::manifest::SuiteManifest;
use silyn_eval::report::{
    EvaluationReport, ReportInput, optional_report_fields, render_markdown, required_report_fields,
};
use silyn_eval::statistics::{Bootstrap, EvaluationStatistics, StatsInput};
use silyn_eval::{BaselineSolver, Split, Verdict};
use uuid::Uuid;

fn fixed_time() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-21T00:00:00Z")
        .expect("timestamp")
        .with_timezone(&Utc)
}

fn baseline_arms() -> (ArmsConfig, silyn_eval::Arms) {
    common::scripted_arms(4, 6, 9)
}

fn stats_for(
    manifest: &SuiteManifest,
    configs: &ArmsConfig,
    arms: &silyn_eval::Arms,
) -> EvaluationStatistics {
    let evaluator = Evaluator::with_seed(1, 42).expect("evaluator");
    EvaluationStatistics::build(StatsInput {
        manifest,
        evaluator,
        split: Split::Dev,
        sealed: false,
        backend: "subprocess",
        configs,
        arms,
        bootstrap: Bootstrap::default(),
    })
}

fn fixed_report(
    manifest: &SuiteManifest,
    configs: &ArmsConfig,
    arms: &silyn_eval::Arms,
) -> EvaluationReport {
    EvaluationReport::build_at(
        Uuid::from_u128(7),
        manifest,
        arms,
        "mock",
        Some("mock"),
        &configs.baseline_id(),
        fixed_time(),
    )
}

#[test]
fn report_matches_eval_report_contract() {
    let manifest = common::manifest();
    manifest.validate().expect("valid manifest");

    let evaluator = Evaluator::new(u32::from(manifest.repeats)).expect("evaluator");
    let arms = evaluator
        .run_matrix(&BaselineSolver, Split::Dev)
        .expect("run arms");
    let report = EvaluationReport::build_at(
        Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0007),
        &manifest,
        &arms,
        "mock",
        Some("mock"),
        "baseline.csv-counter.v1:mock",
        fixed_time(),
    );

    let json = serde_json::to_value(&report).expect("report serializes");
    let object = json.as_object().expect("report is an object");

    // `additionalProperties: false` and the required list.
    let expected_keys: BTreeSet<&str> = required_report_fields()
        .union(&optional_report_fields())
        .copied()
        .collect();
    let actual_keys: BTreeSet<&str> = object.keys().map(String::as_str).collect();
    assert_eq!(actual_keys, expected_keys);

    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["suite_version"], "csv-stats-v1.1");
    assert_eq!(json["model_id"], "mock");
    assert_eq!(json["model_reported"], "mock");
    assert_eq!(json["baseline_id"], "baseline.csv-counter.v1:mock");
    assert_eq!(json["repeats"], 1);
    assert_eq!(json["candidate_id"], Uuid::from_u128(7).to_string());
    assert_eq!(
        Uuid::parse_str(json["candidate_id"].as_str().expect("candidate id")).expect("uuid format"),
        Uuid::from_u128(7)
    );
    assert!(
        DateTime::parse_from_rfc3339(json["created_at"].as_str().expect("created_at string"))
            .is_ok()
    );

    let arms_json = json["arms"].as_object().expect("arms object");
    let arm_keys: BTreeSet<&str> = arms_json.keys().map(String::as_str).collect();
    let expected_arms: BTreeSet<&str> = ["A", "B", "C"].into_iter().collect();
    assert_eq!(arm_keys, expected_arms);
    for (name, arm) in arms_json {
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

    // W5 arms are identical by construction: no gain, so the honest verdict is
    // reject. A report must never claim a promotion without a measured gain.
    assert_eq!(json["verdict"], "reject");
    assert_eq!(report.verdict, Verdict::Reject);

    // Round-trips through the typed contract.
    let parsed: EvaluationReport = serde_json::from_value(json).expect("report deserializes");
    assert_eq!(parsed.candidate_id, Uuid::from_u128(7));
    assert_eq!(parsed.repeats, 1);
    assert_eq!(parsed.verdict, Verdict::Reject);

    // `--out` content is valid JSON with the same fields.
    let text = report.to_json_pretty().expect("pretty report");
    let reparsed: serde_json::Value = serde_json::from_str(&text).expect("valid JSON output");
    assert_eq!(reparsed["schema_version"], 1);
}

#[test]
fn report_schema_is_complete_and_typed() {
    let manifest = common::manifest();
    let (configs, arms) = baseline_arms();
    let report = fixed_report(&manifest, &configs, &arms);
    let stats = stats_for(&manifest, &configs, &arms);
    let json = serde_json::to_value(&report).expect("report serializes");

    // Type and domain checks mirroring the frozen schema.
    assert!(json["schema_version"].as_u64() == Some(1));
    assert!(Uuid::parse_str(json["candidate_id"].as_str().expect("uuid")).is_ok());
    assert!(!json["suite_version"].as_str().expect("version").is_empty());
    assert!(!json["model_id"].as_str().expect("model").is_empty());
    assert!(!json["baseline_id"].as_str().expect("baseline").is_empty());
    let repeats = json["repeats"].as_u64().expect("repeats");
    assert!((1..=5).contains(&repeats));
    assert!(
        matches!(
            json["verdict"].as_str().expect("verdict"),
            "promote" | "reject" | "await_approval" | "invalid"
        ),
        "verdict outside the frozen enum"
    );
    assert!(DateTime::parse_from_rfc3339(json["created_at"].as_str().expect("created_at")).is_ok());

    // `additionalProperties: false` on every object.
    let expected_root: BTreeSet<&str> = required_report_fields()
        .union(&optional_report_fields())
        .copied()
        .collect();
    let actual_root: BTreeSet<&str> = json
        .as_object()
        .expect("root")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(actual_root, expected_root);
    for (name, arm) in json["arms"].as_object().expect("arms") {
        let arm = arm.as_object().expect("arm");
        let keys: BTreeSet<&str> = arm.keys().map(String::as_str).collect();
        let expected: BTreeSet<&str> = [
            "successes",
            "total",
            "cost_usd",
            "latency_p50_ms",
            "latency_p95_ms",
            "tool_errors",
        ]
        .into_iter()
        .collect();
        assert_eq!(keys, expected, "arm {name} keys");
        assert!(
            arm["successes"].as_u64().expect("successes") <= arm["total"].as_u64().expect("total")
        );
        assert!(arm["total"].as_u64().expect("total") >= 1);
        assert!(arm["cost_usd"].as_f64().expect("cost") >= 0.0);
        assert!(arm["latency_p50_ms"].as_f64().expect("p50") >= 0.0);
        assert!(arm["latency_p95_ms"].as_f64().expect("p95") >= 0.0);
        assert!(
            arm["tool_errors"].as_u64().expect("tool_errors")
                <= arm["total"].as_u64().expect("total")
        );
    }
    for field in ["min_gain_pp", "max_cost_ratio", "max_regression_pp"] {
        assert!(json["thresholds"][field].is_number(), "threshold {field}");
    }

    // Thresholds are the pre-registered manifest values, never recomputed.
    assert_eq!(
        json["thresholds"]["min_gain_pp"],
        manifest.thresholds.min_gain_pp
    );
    assert_eq!(
        json["thresholds"]["max_cost_ratio"],
        manifest.thresholds.max_cost_ratio
    );
    assert_eq!(
        json["thresholds"]["max_regression_pp"],
        manifest.thresholds.max_regression_pp
    );

    // The statistics companion carries what the schema cannot: seeds, budgets,
    // durations, unauthorized actions and paired intervals.
    let stats_json = serde_json::to_value(&stats).expect("stats serialize");
    assert_eq!(stats_json["schema_version"], 1);
    assert_eq!(stats_json["seed"], 42);
    assert_eq!(stats_json["repeats"], 1);
    assert_eq!(stats_json["backend"], "subprocess");
    assert_eq!(stats_json["sealed"], false);
    assert_eq!(stats_json["thresholds"]["max_cost_usd"], 1.0);
    for name in ["A", "B", "C"] {
        let arm = stats_json["arms"][name].as_object().expect("arm stats");
        for field in [
            "successes",
            "total",
            "success_rate_pp",
            "cost_usd",
            "duration_ms",
            "tool_errors",
            "unauthorized_actions",
            "budget",
        ] {
            assert!(arm.contains_key(field), "arm {name} stats missing {field}");
        }
    }
    for comparison in ["b_minus_a", "c_minus_b", "c_minus_a"] {
        let gain = stats_json["comparisons"][comparison]
            .as_object()
            .expect("comparison");
        for field in [
            "point_pp",
            "ci_low_pp",
            "ci_high_pp",
            "paired_tasks",
            "resamples",
            "confidence_pct",
            "seed",
        ] {
            assert!(gain.contains_key(field), "{comparison} missing {field}");
        }
    }
    assert_eq!(stats_json["comparisons"]["c_minus_a"]["resamples"], 1000);
    assert_eq!(stats_json["verdict"], "promote");
    assert_eq!(report.verdict, Verdict::Promote);

    // Both documents round-trip through their typed contracts.
    let _: EvaluationReport = serde_json::from_value(json).expect("report round-trip");
    let typed_stats: EvaluationStatistics =
        serde_json::from_value(stats_json).expect("stats round-trip");
    assert_eq!(typed_stats.comparisons.c_minus_a.paired_tasks, 10);

    // The recorded repeat count is the one that ran, not the manifest default.
    let repeated = EvaluationReport::build(ReportInput {
        candidate_id: Uuid::from_u128(7),
        manifest: &manifest,
        arms: &arms,
        repeats: 3,
        model_id: "mock",
        model_reported: None,
        baseline_id: "baseline",
        created_at: fixed_time(),
    });
    assert_eq!(repeated.repeats, 3);
    assert!(repeated.model_reported.is_none());
}

#[test]
fn markdown_introduces_no_numbers_absent_from_json() {
    let manifest = common::manifest();
    let (configs, arms) = baseline_arms();
    let report = fixed_report(&manifest, &configs, &arms);
    let stats = stats_for(&manifest, &configs, &arms);

    let markdown = render_markdown(&report, &stats);
    let report_json = serde_json::to_value(&report).expect("report json");
    let stats_json = serde_json::to_value(&stats).expect("stats json");

    let mut allowed = BTreeSet::new();
    collect_json_numbers(&report_json, &mut allowed);
    collect_json_numbers(&stats_json, &mut allowed);
    let from_markdown = numbers_in_text(&markdown);
    assert!(!from_markdown.is_empty(), "markdown contains no figures");
    let unexpected: Vec<&String> = from_markdown.difference(&allowed).collect();
    assert!(
        unexpected.is_empty(),
        "markdown introduces numbers absent from the JSON: {unexpected:?}"
    );

    // The Markdown mirrors the decision without inventing labels.
    assert!(markdown.contains("| verdict | `promote` |"));
    assert!(markdown.contains("| detail | `promote` |"));
    assert!(markdown.contains(&report.candidate_id.to_string()));
    assert!(markdown.contains(&manifest.splits.test.sha256));
    assert!(markdown.contains("| C-A |"));
    // Every arm row is present exactly once.
    for name in ["| A |", "| B |", "| C |"] {
        assert_eq!(markdown.matches(name).count(), 1, "arm row {name}");
    }
}

/// All numbers reachable from a JSON document: numeric values and numeric
/// substrings of strings (uuids, timestamps, suite versions, digests).
fn collect_json_numbers(value: &serde_json::Value, out: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_f64() {
                out.insert(normalize(value));
            }
        }
        serde_json::Value::String(text) => out.extend(numbers_in_text(text)),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_json_numbers(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                out.extend(numbers_in_text(key));
                collect_json_numbers(item, out);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) => {}
    }
}

/// Numeric tokens in free text. A token is a digit run with at most one dot.
fn numbers_in_text(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        let mut dot = false;
        while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
            if bytes[index] == b'.' {
                if dot {
                    break;
                }
                dot = true;
            }
            index += 1;
        }
        let token = text[start..index].trim_end_matches('.');
        if let Ok(value) = token.parse::<f64>() {
            out.insert(normalize(value));
        }
    }
    out
}

fn normalize(value: f64) -> String {
    format!("{value:.6}")
}
