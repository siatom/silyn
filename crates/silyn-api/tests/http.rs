//! HTTP surface tests: health, the agent/session/task/run round trip, SSE,
//! cancellation, memory scoping, error mapping, auth and bind policy.
//!
//! All tests use `tower::ServiceExt::oneshot` with a temporary home. No port is
//! opened and no provider or network is contacted.

mod common;

use std::net::SocketAddr;
use std::path::Path;

use axum::http::{Method, StatusCode};
use common::{RESPONSE_TIMEOUT, TestServer, agent_id, pending_run};
use serde_json::json;
use silyn_api::ApiConfig;
use silyn_app::{MemoryScope, Provenance};
use silyn_core::SilynError;
use silyn_core::ids::{AgentId, RunId, SessionId, TaskId};

#[tokio::test]
async fn health_is_public_and_reports_version() {
    let server = TestServer::with_token(Some("sekret")).await;

    // No Authorization header: /health is outside the token middleware.
    let response = server
        .send_with_token(Method::GET, "/health", None, None)
        .await;

    assert_eq!(response.status, StatusCode::OK);
    let body = response.json();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["version"], silyn_api::VERSION);
    assert!(
        !response.text().contains("home"),
        "health must not leak the data directory"
    );
}

#[tokio::test]
async fn agent_session_task_run_round_trip() {
    let server = TestServer::start().await;

    let agent = server
        .post("/v1/agents", Some(&json!({ "owner_id": " local-user " })))
        .await;
    assert_eq!(agent.status, StatusCode::CREATED);
    let agent_id = agent.json()["id"].as_str().expect("agent id").to_owned();
    assert_eq!(agent.json()["owner_id"], "local-user");

    let agents = server.get("/v1/agents").await;
    assert_eq!(agents.status, StatusCode::OK);
    assert_eq!(agents.json().as_array().expect("agents").len(), 1);

    let session = server
        .post(&format!("/v1/agents/{agent_id}/sessions"), None)
        .await;
    assert_eq!(session.status, StatusCode::CREATED);
    assert_eq!(session.json()["agent_id"], agent_id);
    let session_id = session.json()["id"]
        .as_str()
        .expect("session id")
        .to_owned();

    let task = server
        .post(
            "/v1/tasks",
            Some(&json!({
                "session_id": session_id,
                "objective": " summarize the csv ",
                "budget": { "max_steps": 4 },
            })),
        )
        .await;
    assert_eq!(task.status, StatusCode::CREATED);
    assert_eq!(task.json()["status"], "created");
    assert_eq!(task.json()["objective"], "summarize the csv");
    let task_id = task.json()["id"].as_str().expect("task id").to_owned();

    let run = server.run_offline(&task_id).await;
    let body = run.json();
    assert_eq!(body["status"], "completed");
    assert_eq!(body["outcome"], "unknown");
    assert_eq!(body["final_answer"], "offline mock run");
    assert_eq!(body["steps"], 1);
}

#[tokio::test]
async fn run_can_be_fetched_by_id() {
    let server = TestServer::start().await;
    let (_agent, _session, task) = server.create_task_chain().await;

    let completed = server.run_offline(&task).await.json();
    let run_id = completed["run_id"].as_str().expect("run id").to_owned();

    let stored = server.get(&format!("/v1/runs/{run_id}")).await;
    assert_eq!(stored.status, StatusCode::OK);
    let record = stored.json();
    assert_eq!(record["id"], run_id);
    assert_eq!(record["task_id"], task);
    assert_eq!(record["status"], "completed");
    assert_eq!(record["model_id"], "mock");
    assert!(record["started_at"].is_string());
    assert!(record["finished_at"].is_string());
}

#[tokio::test]
async fn events_stream_replays_a_terminal_run_and_ends() {
    let server = TestServer::start().await;
    let (_agent, _session, task) = server.create_task_chain().await;
    let run_id = server.run_offline(&task).await.json()["run_id"]
        .as_str()
        .expect("run id")
        .to_owned();

    let response = tokio::time::timeout(
        RESPONSE_TIMEOUT,
        server.get(&format!("/v1/runs/{run_id}/events")),
    )
    .await
    .expect("the stream must end for a terminal run");

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.content_type.as_deref(),
        Some("text/event-stream"),
        "SSE content type"
    );

    let text = response.text();
    assert!(text.contains("id: 1"), "{text}");
    assert!(text.contains("event: run_created"), "{text}");
    assert!(text.contains("event: run_started"), "{text}");
    assert!(text.contains("event: run_completed"), "{text}");
    assert!(
        text.contains("\"type\":\"run_completed\""),
        "the data frame carries the envelope: {text}"
    );
    assert!(
        text.contains(&format!("\"aggregate_id\":\"{run_id}\"")),
        "{text}"
    );
    // Without keep-alive comments the body is a sequence of `\n\n` frames and
    // the response ends as soon as the run is terminal.
    assert!(text.ends_with("\n\n"), "{text}");

    // An unknown run is a 404 before any stream starts.
    let unknown = server
        .get(&format!("/v1/runs/{}/events", RunId::new()))
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cancel_unknown_is_404_and_known_writes_the_signal() {
    let server = TestServer::start().await;

    let unknown = RunId::new();
    let response = server
        .post(&format!("/v1/runs/{unknown}/cancel"), None)
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(response.error_class(), "permanent");
    assert!(response.error_message().contains("not found"));
    assert!(!server.app.cancel_request_path(unknown).exists());

    // A run row without execution: the state a killed process leaves behind.
    let (_agent, _session, task) = server.create_task_chain().await;
    let task_id: TaskId = task.parse().expect("task id");
    let pending = pending_run(&server.app, task_id).await;

    let first = server
        .post(&format!("/v1/runs/{pending}/cancel"), None)
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text());
    let body = first.json();
    assert_eq!(body["run_id"], pending.to_string());
    assert_eq!(body["already_requested"], false);
    let path = body["path"].as_str().expect("cancel path");
    assert_eq!(Path::new(path), server.app.cancel_request_path(pending));
    assert!(server.app.cancel_request_path(pending).is_file());

    // Idempotent: the second request reports the existing signal.
    let second = server
        .post(&format!("/v1/runs/{pending}/cancel"), None)
        .await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(second.json()["already_requested"], true);

    // A terminal run cannot be cancelled.
    let completed = server.run_offline(&task).await.json()["run_id"]
        .as_str()
        .expect("run id")
        .to_owned();
    let terminal = server
        .post(&format!("/v1/runs/{completed}/cancel"), None)
        .await;
    assert_eq!(terminal.status, StatusCode::BAD_REQUEST);
    assert!(terminal.error_message().contains("cannot be cancelled"));
}

#[tokio::test]
async fn memory_search_is_agent_scoped() {
    let server = TestServer::start().await;
    let alice = agent_id(&server.create_agent_owned("alice").await);
    let bob = agent_id(&server.create_agent_owned("bob").await);

    let secret = server
        .app
        .add_memory(
            alice,
            "deploy window rule",
            "Deploy window starts Monday.",
            MemoryScope::Agent,
            Provenance::UserStatement,
        )
        .await
        .expect("alice memory");
    server
        .app
        .add_memory(
            alice,
            "deploy checklist",
            "Two approvals before deploy.",
            MemoryScope::Session,
            Provenance::ExternalEvidence,
        )
        .await
        .expect("second alice memory");
    server
        .app
        .add_memory(
            bob,
            "handbook",
            "Deploy requires reading the handbook.",
            MemoryScope::Agent,
            Provenance::UserStatement,
        )
        .await
        .expect("bob memory");

    let hits = server
        .get(&format!("/v1/agents/{alice}/memories?q=deploy"))
        .await;
    assert_eq!(hits.status, StatusCode::OK);
    let hits = hits.json();
    let rows = hits.as_array().expect("memory hits");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter().any(|row| row["id"] == secret.id.to_string()),
        "{rows:?}"
    );

    let others = server
        .get(&format!("/v1/agents/{bob}/memories?q=deploy"))
        .await
        .json();
    let rows = others.as_array().expect("memory hits");
    assert_eq!(rows.len(), 1);
    assert!(
        rows.iter().all(|row| row["id"] != secret.id.to_string()),
        "agent b must never see agent a's memory: {rows:?}"
    );

    let limited = server
        .get(&format!("/v1/agents/{alice}/memories?limit=1"))
        .await;
    assert_eq!(limited.status, StatusCode::OK);
    assert_eq!(limited.json().as_array().expect("hits").len(), 1);

    let zero = server
        .get(&format!("/v1/agents/{alice}/memories?limit=0"))
        .await;
    assert_eq!(zero.status, StatusCode::BAD_REQUEST);

    let unknown = server
        .get(&format!("/v1/agents/{}/memories", AgentId::new()))
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_ids_are_404_and_malformed_input_is_400() {
    let server = TestServer::start().await;

    let unknown_run = server.get(&format!("/v1/runs/{}", RunId::new())).await;
    assert_eq!(unknown_run.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown_run.error_class(), "permanent");
    assert!(unknown_run.error_message().contains("not found"));

    let bad_id = server.get("/v1/runs/not-a-uuid").await;
    assert_eq!(bad_id.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_id.error_class(), "permanent");
    assert!(bad_id.error_message().contains("not a valid run id"));

    let bad_json = server
        .send_raw(Method::POST, "/v1/agents", "{not json")
        .await;
    assert_eq!(bad_json.status, StatusCode::BAD_REQUEST);
    assert!(bad_json.error_message().contains("invalid JSON body"));

    let empty_body = server.post("/v1/agents", None).await;
    assert_eq!(empty_body.status, StatusCode::BAD_REQUEST);

    let (_agent, session, _task) = server.create_task_chain().await;

    let empty_objective = server
        .post(
            "/v1/tasks",
            Some(&json!({ "session_id": session, "objective": "   " })),
        )
        .await;
    assert_eq!(empty_objective.status, StatusCode::BAD_REQUEST);

    let missing_session = server
        .post(
            "/v1/tasks",
            Some(&json!({ "session_id": SessionId::new(), "objective": "do it" })),
        )
        .await;
    assert_eq!(missing_session.status, StatusCode::NOT_FOUND);

    let missing_agent = server
        .post(&format!("/v1/agents/{}/sessions", AgentId::new()), None)
        .await;
    assert_eq!(missing_agent.status, StatusCode::NOT_FOUND);

    let unknown_route = server.get("/v1/does-not-exist").await;
    assert_eq!(unknown_route.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn non_loopback_bind_is_rejected_without_binding() {
    let loopback: SocketAddr = "127.0.0.1:7788".parse().expect("addr");
    let ipv6_loopback: SocketAddr = "[::1]:7788".parse().expect("addr");
    let any: SocketAddr = "0.0.0.0:7788".parse().expect("addr");
    let routable: SocketAddr = "192.0.2.10:7788".parse().expect("addr");

    for addr in [loopback, ipv6_loopback] {
        assert!(
            ApiConfig {
                addr,
                token: None,
                allow_non_loopback: false,
            }
            .validate()
            .is_ok(),
            "{addr} is loopback"
        );
    }

    for addr in [any, routable] {
        let error = ApiConfig {
            addr,
            token: None,
            allow_non_loopback: false,
        }
        .validate()
        .expect_err("non-loopback bind must be refused");
        assert!(matches!(error, SilynError::Permanent(_)));
        assert!(error.to_string().contains("local-only"), "{error}");
    }

    assert!(
        ApiConfig {
            addr: any,
            token: None,
            allow_non_loopback: true,
        }
        .validate()
        .is_ok(),
        "the explicit override permits a non-loopback bind"
    );

    // `serve_with` validates before it opens a socket, so this returns without
    // listening and without a timeout.
    let server = TestServer::start().await;
    let error = silyn_api::serve_with(server.app.clone(), any, None, false)
        .await
        .expect_err("serve must refuse a public address");
    assert!(matches!(error, SilynError::Permanent(_)));
}

#[tokio::test]
async fn token_middleware_protects_v1_only() {
    let server = TestServer::with_token(Some("sekret")).await;

    let missing = server
        .send_with_token(Method::GET, "/v1/agents", None, None)
        .await;
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED);
    assert_eq!(missing.error_class(), "unauthenticated");
    assert!(
        !missing.text().contains("sekret"),
        "the response must not echo the expected token"
    );

    let wrong = server
        .send_with_token(Method::GET, "/v1/agents", None, Some("wrong"))
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);

    let right = server
        .send_with_token(Method::GET, "/v1/agents", None, Some("sekret"))
        .await;
    assert_eq!(right.status, StatusCode::OK);

    let health = server
        .send_with_token(Method::GET, "/health", None, None)
        .await;
    assert_eq!(health.status, StatusCode::OK);

    // With no token configured the local surface trusts the loopback caller.
    let open = TestServer::start().await;
    assert_eq!(open.get("/v1/agents").await.status, StatusCode::OK);
}
