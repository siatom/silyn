//! Verifier acceptance tests: evidence composition, artifact loading, JSON
//! subset matching and the path sandbox.
//!
//! No test touches the network, a model or a shell. File artifacts live in a
//! `tempfile` directory that is removed with the test.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use silyn_core::SilynError;
use silyn_core::contracts::{OutcomeVerifier, RunSnapshot};
use silyn_core::event::VerifiedOutcome;
use silyn_core::ids::RunId;
use silyn_growth::{
    ArtifactSet, CheckResult, Checker, CompositeVerifier, ExpectedAnswerChecker,
    FileContainsChecker, VerificationPolicy,
};

/// A checker with a fixed verdict, used to test composition only.
struct Fixed(CheckResult);

#[async_trait]
impl Checker for Fixed {
    async fn check(
        &self,
        _run: &RunSnapshot,
        _artifacts: &ArtifactSet,
    ) -> Result<CheckResult, SilynError> {
        Ok(self.0.clone())
    }
}

fn success(evidence: &[&str]) -> CheckResult {
    CheckResult::Success {
        evidence: evidence.iter().map(|item| (*item).to_owned()).collect(),
    }
}

fn failed(code: &str) -> CheckResult {
    CheckResult::Failed {
        code: code.to_owned(),
        reason: format!("{code} was detected"),
        evidence: vec![format!("evidence:{code}")],
    }
}

fn unknown(reason: &str) -> CheckResult {
    CheckResult::Unknown {
        reason: reason.to_owned(),
    }
}

fn snapshot(refs: &[&str]) -> RunSnapshot {
    RunSnapshot {
        run_id: RunId::new(),
        objective: "summarise the quarterly csv".to_owned(),
        artifact_refs: refs.iter().map(|item| (*item).to_owned()).collect(),
    }
}

fn checkers(results: Vec<CheckResult>) -> Vec<Arc<dyn Checker>> {
    results
        .into_iter()
        .map(|result| Arc::new(Fixed(result)) as Arc<dyn Checker>)
        .collect()
}

#[tokio::test]
async fn success_requires_independent_evidence() {
    let verifier = CompositeVerifier::new(checkers(vec![
        success(&["row_count:42"]),
        success(&["unit:usd"]),
    ]));

    let evidence = verifier
        .verify(snapshot(&[]))
        .await
        .expect("verify succeeds");

    assert_eq!(evidence.outcome, VerifiedOutcome::Success);
    assert_eq!(
        evidence.evidence_refs,
        vec!["row_count:42".to_owned(), "unit:usd".to_owned()],
        "success must carry the evidence of every agreeing checker"
    );
    assert!(evidence.confidence > 0.0 && evidence.confidence <= 1.0);
}

#[tokio::test]
async fn failed_dominates_unknown() {
    let results = vec![
        success(&["row_count:42"]),
        unknown("the report artifact was unavailable"),
        failed("wrong_count"),
    ];
    let verifier = CompositeVerifier::new(checkers(results.clone()));

    let combined = verifier.combine(&results);
    match &combined {
        CheckResult::Failed { code, evidence, .. } => {
            assert_eq!(code, "wrong_count", "the first failure supplies the code");
            assert!(
                evidence.contains(&"row_count:42".to_owned()),
                "evidence from passing checkers is merged into the failure"
            );
        }
        other => panic!("a failure must dominate an unknown result, got {other:?}"),
    }

    let evidence = verifier
        .verify(snapshot(&[]))
        .await
        .expect("verify succeeds");
    assert_eq!(evidence.outcome, VerifiedOutcome::Failed);
}

#[tokio::test]
async fn no_checker_is_unknown() {
    let empty = CompositeVerifier::new(Vec::new());
    assert_eq!(empty.checker_count(), 0);

    let evidence = empty
        .verify(snapshot(&[]))
        .await
        .expect("verification without checkers is not an error");
    assert_eq!(
        evidence.outcome,
        VerifiedOutcome::Unknown,
        "no checker means no evidence, never success"
    );
    assert!(evidence.evidence_refs.is_empty());
    assert!(matches!(empty.combine(&[]), CheckResult::Unknown { .. }));

    let undecided = CompositeVerifier::new(checkers(vec![unknown("no artifacts")]));
    assert_eq!(
        undecided
            .verify(snapshot(&[]))
            .await
            .expect("verify succeeds")
            .outcome,
        VerifiedOutcome::Unknown
    );
}

#[tokio::test]
async fn expected_answer_json_subset() {
    let checker = ExpectedAnswerChecker::new(json!({
        "rows": 42,
        "total": { "usd": 10 },
        "tags": ["a", "b"],
    }));

    let matching = snapshot(&[
        r#"inline:{"rows": 42, "total": {"usd": 10, "eur": 9}, "tags": ["a", "b"], "extra": true}"#,
    ]);
    let artifacts = ArtifactSet::new(None, &matching.artifact_refs);
    assert_eq!(
        checker
            .check(&matching, &artifacts)
            .await
            .expect("check runs"),
        CheckResult::Success {
            evidence: vec!["expected_answer:0:3 fields".to_owned()],
        },
        "extra keys and nested extra keys are allowed"
    );

    let wrong_scalar =
        snapshot(&[r#"inline:{"rows": 41, "total": {"usd": 10}, "tags": ["a", "b"]}"#]);
    let artifacts = ArtifactSet::new(None, &wrong_scalar.artifact_refs);
    match checker
        .check(&wrong_scalar, &artifacts)
        .await
        .expect("check runs")
    {
        CheckResult::Failed { code, reason, .. } => {
            assert_eq!(code, "wrong_answer");
            assert!(reason.contains("$.rows"), "reason names the mismatch path");
        }
        other => panic!("a different scalar must fail, got {other:?}"),
    }

    let wrong_array =
        snapshot(&[r#"inline:{"rows": 42, "total": {"usd": 10}, "tags": ["a", "b", "c"]}"#]);
    let artifacts = ArtifactSet::new(None, &wrong_array.artifact_refs);
    match checker
        .check(&wrong_array, &artifacts)
        .await
        .expect("check runs")
    {
        CheckResult::Failed { code, .. } => assert_eq!(code, "wrong_answer"),
        other => panic!("arrays must match exactly, got {other:?}"),
    }

    let no_json = snapshot(&["inline:rows,total\n42,10"]);
    let artifacts = ArtifactSet::new(None, &no_json.artifact_refs);
    assert!(matches!(
        checker
            .check(&no_json, &artifacts)
            .await
            .expect("check runs"),
        CheckResult::Unknown { .. }
    ));
}

#[tokio::test]
async fn malformed_artifact_is_failed() {
    let run = snapshot(&[r#"inline:{"rows": 42,"#]);
    let artifacts = ArtifactSet::new(None, &run.artifact_refs);
    let result = ExpectedAnswerChecker::new(json!({"rows": 42}))
        .check(&run, &artifacts)
        .await
        .expect("check runs");

    match result {
        CheckResult::Failed { code, .. } => assert_eq!(
            code, "malformed_artifact",
            "broken json is a failure, not a guess"
        ),
        other => panic!("malformed json must fail, got {other:?}"),
    }

    // A malformed artifact never reaches the composite as success.
    let verifier = CompositeVerifier::new(vec![Arc::new(ExpectedAnswerChecker::new(
        json!({"rows": 42}),
    ))]);
    let evidence = verifier.verify(run).await.expect("verify succeeds");
    assert_eq!(evidence.outcome, VerifiedOutcome::Failed);
}

#[tokio::test]
async fn artifact_refs_load_inline_file_and_plain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("create root");
    std::fs::write(root.join("report.json"), r#"{"rows": 42}"#).expect("write artifact");

    let refs = vec![
        "inline:hello".to_owned(),
        "file:report.json".to_owned(),
        "report.json".to_owned(),
        r#"{"rows": 42}"#.to_owned(),
    ];
    let artifacts = ArtifactSet::new(Some(root.clone()), &refs);

    assert_eq!(artifacts.len(), 4, "every readable reference is loaded");
    assert_eq!(artifacts.get("inline:hello"), Some("hello"));
    assert_eq!(
        artifacts.get("file:report.json"),
        Some(r#"{"rows": 42}"#),
        "file: resolves inside the root"
    );
    assert_eq!(
        artifacts.get("report.json"),
        Some(r#"{"rows": 42}"#),
        "a plain path stays a file when it exists (W3 compatibility)"
    );
    assert_eq!(
        artifacts.get(r#"{"rows": 42}"#),
        Some(r#"{"rows": 42}"#),
        "a plain string without a file is inline content"
    );
    assert_eq!(artifacts.all().len(), 4);

    // The verifier can be pointed at the same root for file references.
    let verifier = CompositeVerifier::new(vec![Arc::new(ExpectedAnswerChecker::new(
        json!({"rows": 42}),
    ))])
    .with_artifact_root(root);
    assert_eq!(
        verifier
            .verify(snapshot(&["file:report.json"]))
            .await
            .expect("verify succeeds")
            .outcome,
        VerifiedOutcome::Success
    );
}

#[tokio::test]
async fn artifact_sandbox_rejects_escape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("root");
    std::fs::create_dir_all(&root).expect("create root");
    std::fs::write(root.join("report.json"), "inside").expect("write artifact");
    std::fs::write(dir.path().join("secret.txt"), "outside the sandbox").expect("write secret");

    let escaping = ArtifactSet::new(
        Some(root.clone()),
        &[
            "file:../secret.txt".to_owned(),
            "file:./report.json".to_owned(),
        ],
    );
    assert!(
        escaping.get("file:../secret.txt").is_none(),
        "parent traversal must not be readable"
    );
    assert_eq!(
        escaping.get("file:./report.json"),
        Some("inside"),
        "a safe relative path is still readable"
    );

    let without_root = ArtifactSet::new(None, &["file:report.json".to_owned()]);
    assert!(
        without_root.get("file:report.json").is_none(),
        "file: needs a sandbox root; a missing root is not a licence to read anywhere"
    );
}

#[tokio::test]
async fn file_contains_checker_reports_missing_text() {
    let checker = FileContainsChecker::new("total_usd");

    let present = snapshot(&["inline:report_id,total_usd\n1,10"]);
    let artifacts = ArtifactSet::new(None, &present.artifact_refs);
    assert_eq!(
        checker
            .check(&present, &artifacts)
            .await
            .expect("check runs"),
        CheckResult::Success {
            evidence: vec!["artifact_contains:0".to_owned()],
        }
    );

    let absent = snapshot(&["inline:report_id,amount\n1,10"]);
    let artifacts = ArtifactSet::new(None, &absent.artifact_refs);
    match checker
        .check(&absent, &artifacts)
        .await
        .expect("check runs")
    {
        CheckResult::Failed { code, .. } => assert_eq!(code, "missing_text"),
        other => panic!("absent text must fail, got {other:?}"),
    }

    let none = snapshot(&["file:missing-report.csv"]);
    let artifacts = ArtifactSet::new(None, &none.artifact_refs);
    assert!(
        artifacts.is_empty(),
        "an unreadable reference must not be guessed"
    );
    assert!(matches!(
        checker.check(&none, &artifacts).await.expect("check runs"),
        CheckResult::Unknown { .. }
    ));
}

#[tokio::test]
async fn all_must_succeed_policy_is_strict() {
    let results = vec![
        success(&["row_count:42"]),
        unknown("report artifact missing"),
    ];

    let lenient = CompositeVerifier::new(checkers(results.clone()));
    assert!(matches!(
        lenient.combine(&results),
        CheckResult::Unknown { .. }
    ));

    let strict = CompositeVerifier::new(checkers(results.clone()))
        .with_policy(VerificationPolicy::AllMustSucceed);
    match strict.combine(&results) {
        CheckResult::Failed { code, evidence, .. } => {
            assert_eq!(code, "unverified");
            assert!(evidence.contains(&"row_count:42".to_owned()));
        }
        other => panic!("the strict policy must reject missing evidence, got {other:?}"),
    }

    let all_success = CompositeVerifier::new(checkers(vec![success(&["a"]), success(&["b"])]))
        .with_policy(VerificationPolicy::AllMustSucceed);
    assert!(matches!(
        all_success.combine(&[success(&["a"]), success(&["b"])]),
        CheckResult::Success { .. }
    ));
    assert!(
        matches!(all_success.combine(&[]), CheckResult::Unknown { .. }),
        "zero checkers stays unknown under every policy"
    );
}
