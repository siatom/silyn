//! Evolution route tests: submission, listing, evaluation, promotion, rollback
//! and the policy-denial path.
//!
//! Reports are synthetic but complete: the gate re-reads them through the same
//! projection as `silyn evolution evaluate`, so a fabricated body still has to
//! agree across the report and its statistics companion.

mod common;

use std::path::PathBuf;

use axum::http::StatusCode;
use common::{TestServer, agent_id};
use serde_json::{Value, json};
use silyn_app::PolicyPatch;
use silyn_core::ids::{CandidateId, PolicyVersionId, RunId};

/// One agent with an active policy and a terminal evidence run.
struct Fixture {
    agent: silyn_core::ids::AgentId,
    policy: PolicyVersionId,
    run: RunId,
}

async fn fixture(server: &TestServer) -> Fixture {
    let raw = server.create_agent().await;
    let agent = agent_id(&raw);
    let policy = server.app.init_policy(agent).await.expect("init policy").id;
    let session = server.create_session(&raw).await;
    let task = server.create_task(&session, "summarize the csv").await;
    let run: RunId = server.run_offline(&task).await.json()["run_id"]
        .as_str()
        .expect("run id")
        .parse()
        .expect("run id");

    Fixture { agent, policy, run }
}

fn patch(candidate: CandidateId, base: PolicyVersionId, evidence: RunId) -> Value {
    json!({
        "schema_version": 1,
        "candidate_id": candidate,
        "base_policy_version": base,
        "hypothesis": "A shorter planner fragment improves summary accuracy.",
        "evidence_run_ids": [evidence],
        "expected_gain": { "metric": "accuracy_pp", "delta_pp": 12.0 },
        "risk_class": "low",
        "operations": [{
            "op": "replace",
            "path": "/planner/prompt_fragments",
            "value": ["Summarize the table in two sentences."]
        }]
    })
}

fn evaluation(agent: silyn_core::ids::AgentId, candidate: CandidateId) -> Value {
    json!({
        "agent_id": agent,
        "report": {
            "schema_version": 1,
            "candidate_id": candidate,
            "suite_version": "csv-stats-v1",
            "baseline_id": "baseline-a",
            "verdict": "promote"
        },
        "stats": {
            "schema_version": 1,
            "suite_version": "csv-stats-v1",
            "verdict": "promote",
            "unauthorized_actions": 0,
            "comparisons": {
                "c_minus_a": { "point_pp": 14.0, "ci_low_pp": 6.0 }
            }
        }
    })
}

async fn submit(server: &TestServer, fixture: &Fixture) -> (CandidateId, Value) {
    let candidate = CandidateId::new();
    let body = json!({
        "agent_id": fixture.agent,
        "patch": patch(candidate, fixture.policy, fixture.run),
    });
    let response = server.post("/v1/evolution/candidates", Some(&body)).await;
    assert_eq!(response.status, StatusCode::CREATED, "{}", response.text());
    (candidate, response.json())
}

fn evaluation_file_count(server: &TestServer) -> usize {
    std::fs::read_dir(server.app.home().join("tmp").join("evaluation"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0)
}

#[tokio::test]
async fn candidates_can_be_submitted_listed_and_shown() {
    let server = TestServer::start().await;
    let fixture = fixture(&server).await;

    let (candidate, record) = submit(&server, &fixture).await;
    assert_eq!(record["id"], candidate.to_string());
    assert_eq!(record["state"], "static_checked");
    assert_eq!(record["risk_class"], "low");
    assert_eq!(
        record["patch"]["evidence_run_ids"][0],
        fixture.run.to_string()
    );

    let list = server
        .get(&format!(
            "/v1/evolution/candidates?agent_id={}",
            fixture.agent
        ))
        .await;
    assert_eq!(list.status, StatusCode::OK);
    let rows = list.json();
    let rows = rows.as_array().expect("candidates");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], candidate.to_string());

    let show = server
        .get(&format!(
            "/v1/evolution/candidates/{candidate}?agent_id={}",
            fixture.agent
        ))
        .await;
    assert_eq!(show.status, StatusCode::OK);
    let detail = show.json();
    assert_eq!(detail["candidate"]["id"], candidate.to_string());
    assert_eq!(
        detail["evaluations"].as_array().expect("evaluations").len(),
        0
    );

    // A missing scope is a contract error, not a cross-agent read.
    let unscoped = server.get("/v1/evolution/candidates").await;
    assert_eq!(unscoped.status, StatusCode::BAD_REQUEST);
    assert!(unscoped.error_message().contains("agent_id"));

    // An unknown candidate is a 404 scoped to the agent.
    let unknown = server
        .get(&format!(
            "/v1/evolution/candidates/{}?agent_id={}",
            CandidateId::new(),
            fixture.agent
        ))
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    // The app's static checks still run: a patch without a hypothesis is a 400.
    let mut invalid = json!({
        "agent_id": fixture.agent,
        "patch": patch(CandidateId::new(), fixture.policy, fixture.run),
    });
    invalid["patch"]
        .as_object_mut()
        .expect("patch object")
        .remove("hypothesis");
    let rejected = server
        .post("/v1/evolution/candidates", Some(&invalid))
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    assert_eq!(rejected.error_class(), "permanent");

    // Deserializing the recorded patch proves the body used the app type.
    let recorded: PolicyPatch =
        serde_json::from_value(record["patch"].clone()).expect("patch round trip");
    assert_eq!(recorded.candidate_id, candidate);
}

#[tokio::test]
async fn cross_agent_evidence_is_policy_denied() {
    let server = TestServer::start().await;
    let alice = fixture(&server).await;

    // A terminal run that belongs to a different agent.
    let bob = server.create_agent_owned("bob").await;
    let bob_session = server.create_session(&bob).await;
    let bob_task = server.create_task(&bob_session, "summarize the csv").await;
    let bob_run: RunId = server.run_offline(&bob_task).await.json()["run_id"]
        .as_str()
        .expect("run id")
        .parse()
        .expect("run id");

    let body = json!({
        "agent_id": alice.agent,
        "patch": patch(CandidateId::new(), alice.policy, bob_run),
    });
    let response = server.post("/v1/evolution/candidates", Some(&body)).await;

    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "{}",
        response.text()
    );
    assert_eq!(response.error_class(), "policy_denied");
    assert!(
        response.error_message().contains("does not belong"),
        "{}",
        response.error_message()
    );
}

#[tokio::test]
async fn evaluation_and_promotion_follow_the_gate() {
    let server = TestServer::start().await;
    let fixture = fixture(&server).await;
    let (candidate, _) = submit(&server, &fixture).await;

    let evaluated = server
        .post(
            &format!("/v1/evolution/candidates/{candidate}/evaluate"),
            Some(&evaluation(fixture.agent, candidate)),
        )
        .await;
    assert_eq!(evaluated.status, StatusCode::OK, "{}", evaluated.text());
    let attachment = evaluated.json();
    assert_eq!(attachment["candidate_id"], candidate.to_string());
    assert_eq!(attachment["state"], "evaluated");
    assert_eq!(attachment["verdict"], "promote");
    assert_eq!(attachment["gain_pp"], 14.0);
    assert_eq!(attachment["ci_low_pp"], 6.0);
    assert_eq!(attachment["evidence_run_ids"][0], fixture.run.to_string());

    // The report body is persisted under the app home so `report_path` stays
    // resolvable for audit, and it is private.
    let report_path = PathBuf::from(
        attachment["report_path"]
            .as_str()
            .expect("report path")
            .to_owned(),
    );
    assert!(
        report_path.starts_with(server.app.home()),
        "{report_path:?}"
    );
    assert!(report_path.is_file());
    assert_eq!(evaluation_file_count(&server), 2, "report plus statistics");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&report_path)
            .expect("report metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "evaluation files must be private");
    }

    // A report that names another candidate is rejected by the service and its
    // files are removed again.
    let mismatched = server
        .post(
            &format!("/v1/evolution/candidates/{candidate}/evaluate"),
            Some(&evaluation(fixture.agent, CandidateId::new())),
        )
        .await;
    assert_eq!(mismatched.status, StatusCode::BAD_REQUEST);
    assert!(mismatched.error_message().contains("names candidate"));
    assert_eq!(evaluation_file_count(&server), 2, "rejected files removed");

    // A malformed statistics companion is also a 400, never a silent default.
    let mut broken = evaluation(fixture.agent, candidate);
    broken["stats"]["comparisons"]["c_minus_a"]["point_pp"] = json!("high");
    let rejected = server
        .post(
            &format!("/v1/evolution/candidates/{candidate}/evaluate"),
            Some(&broken),
        )
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    assert_eq!(evaluation_file_count(&server), 2);

    // The gate promotes the low-risk candidate with a cleared threshold.
    let promoted = server
        .post(
            &format!("/v1/evolution/candidates/{candidate}/promote"),
            Some(&json!({ "agent_id": fixture.agent })),
        )
        .await;
    assert_eq!(promoted.status, StatusCode::OK, "{}", promoted.text());
    let promotion = promoted.json();
    assert_eq!(promotion["decision"], "promote");
    assert_eq!(promotion["state"], "promoted");
    let promotion_id = promotion["promotion"]["id"]
        .as_str()
        .expect("promotion id")
        .to_owned();
    assert_eq!(
        promotion["to_version"],
        promotion["promotion"]["to_version"].clone()
    );

    // Rollback restores the base version.
    let rolled_back = server
        .post(
            &format!("/v1/evolution/promotions/{promotion_id}/rollback"),
            Some(&json!({ "agent_id": fixture.agent })),
        )
        .await;
    assert_eq!(rolled_back.status, StatusCode::OK, "{}", rolled_back.text());
    let rollback = rolled_back.json();
    assert_eq!(rollback["promotion_id"], promotion_id);
    assert_eq!(rollback["to_version"], fixture.policy.to_string());

    let show = server
        .get(&format!(
            "/v1/evolution/candidates/{candidate}?agent_id={}",
            fixture.agent
        ))
        .await
        .json();
    assert_eq!(show["candidate"]["state"], "rolled_back");
    assert_eq!(
        show["evaluations"].as_array().expect("evaluations").len(),
        1
    );
    assert!(
        !show["promotions"]
            .as_array()
            .expect("promotions")
            .is_empty()
    );
}
