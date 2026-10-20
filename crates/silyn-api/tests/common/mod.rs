//! Shared fixtures for the HTTP tests.
//!
//! Every test gets its own temporary Silyn home and a router built with
//! `tower::ServiceExt::oneshot`; no test opens a port or contacts a provider.

#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use silyn_api::{ApiState, router};
use silyn_app::{AnswerVerifier, App, RunOptions};
use silyn_core::ids::{AgentId, RunId, SessionId, TaskId};
use silyn_llm::MockProvider;
use silyn_runtime::{MemoryAuditSink, Runtime, ToolBroker};
use tempfile::TempDir;
use tower::ServiceExt;

/// A temporary app plus the router bound to it.
pub struct TestServer {
    pub dir: TempDir,
    pub app: Arc<App>,
    pub router: Router,
    pub token: Option<String>,
}

impl TestServer {
    /// Server without authentication.
    pub async fn start() -> Self {
        Self::with_token(None).await
    }

    /// Server with an optional bearer token.
    pub async fn with_token(token: Option<&str>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let app = Arc::new(App::open(dir.path().join("home")).await.expect("open app"));
        let router = router(ApiState::new(Arc::clone(&app), token.map(str::to_owned)));
        Self {
            dir,
            app,
            router,
            token: token.map(str::to_owned),
        }
    }

    /// Sends a request with the server's configured token.
    pub async fn send(&self, method: Method, uri: &str, body: Option<&Value>) -> TestResponse {
        self.send_with_token(method, uri, body, self.token.as_deref())
            .await
    }

    /// Sends a request with an explicit token (`None` sends no header).
    pub async fn send_with_token(
        &self,
        method: Method,
        uri: &str,
        body: Option<&Value>,
        token: Option<&str>,
    ) -> TestResponse {
        let bytes = match body {
            Some(value) => serde_json::to_vec(value).expect("serialize body"),
            None => Vec::new(),
        };
        self.send_bytes_with_token(method, uri, bytes, token).await
    }

    /// Sends a request with a raw body.
    pub async fn send_raw(&self, method: Method, uri: &str, body: &str) -> TestResponse {
        self.send_bytes_with_token(method, uri, body.as_bytes().to_vec(), self.token.as_deref())
            .await
    }

    async fn send_bytes_with_token(
        &self,
        method: Method,
        uri: &str,
        bytes: Vec<u8>,
        token: Option<&str>,
    ) -> TestResponse {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = if bytes.is_empty() {
            Body::empty()
        } else {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(bytes)
        };

        let response = self
            .router
            .clone()
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("router response");
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body");

        TestResponse {
            status,
            content_type,
            body,
        }
    }

    pub async fn get(&self, uri: &str) -> TestResponse {
        self.send(Method::GET, uri, None).await
    }

    pub async fn post(&self, uri: &str, body: Option<&Value>) -> TestResponse {
        self.send(Method::POST, uri, body).await
    }

    /// Creates an agent through the API and returns its id.
    pub async fn create_agent(&self) -> String {
        self.create_agent_owned("local-user").await
    }

    /// Creates an agent with an explicit owner label.
    pub async fn create_agent_owned(&self, owner: &str) -> String {
        let response = self
            .post("/v1/agents", Some(&json!({ "owner_id": owner })))
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text());
        response.json()["id"].as_str().expect("agent id").to_owned()
    }

    /// Creates a session for `agent_id` through the API.
    pub async fn create_session(&self, agent_id: &str) -> String {
        let response = self
            .post(&format!("/v1/agents/{agent_id}/sessions"), None)
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text());
        response.json()["id"]
            .as_str()
            .expect("session id")
            .to_owned()
    }

    /// Creates a task through the API.
    pub async fn create_task(&self, session_id: &str, objective: &str) -> String {
        let response = self
            .post(
                "/v1/tasks",
                Some(&json!({ "session_id": session_id, "objective": objective })),
            )
            .await;
        assert_eq!(response.status, StatusCode::CREATED, "{}", response.text());
        response.json()["id"].as_str().expect("task id").to_owned()
    }

    /// Agent -> session -> task through the API.
    pub async fn create_task_chain(&self) -> (String, String, String) {
        let agent = self.create_agent().await;
        let session = self.create_session(&agent).await;
        let task = self.create_task(&session, "summarize the csv").await;
        (agent, session, task)
    }

    /// Runs `task_id` offline and returns the `RunOutcome` JSON.
    pub async fn run_offline(&self, task_id: &str) -> TestResponse {
        let response = self
            .post(
                &format!("/v1/tasks/{task_id}/run"),
                Some(&json!({ "offline": true, "model": "mock" })),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK, "{}", response.text());
        response
    }
}

/// Response with the status, content type and body already collected.
pub struct TestResponse {
    pub status: StatusCode,
    pub content_type: Option<String>,
    pub body: Bytes,
}

impl TestResponse {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("json body")
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn error_class(&self) -> String {
        self.json()["error"]["class"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    pub fn error_message(&self) -> String {
        self.json()["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
}

/// Creates a run row without executing it, the state a crashed process leaves.
///
/// Uses the runtime's public `create_run` so the row is written by the same code
/// the app uses; the API is only asked to cancel it afterwards.
pub async fn pending_run(app: &App, task_id: TaskId) -> RunId {
    let workspace = app.workspace_dir();
    std::fs::create_dir_all(&workspace).expect("workspace");
    let tools = Arc::new(
        ToolBroker::new(&workspace, Arc::new(MemoryAuditSink::new())).expect("tool broker"),
    );
    let runtime = Runtime::new(
        app.storage().clone(),
        Arc::new(MockProvider::new(Vec::new())),
        tools,
        Arc::new(AnswerVerifier),
    );
    let options = RunOptions {
        model: "mock".to_owned(),
        ..RunOptions::default()
    };
    runtime
        .create_run(task_id, &options)
        .await
        .expect("create run")
        .id
}

/// Parses an agent id produced by the API.
pub fn agent_id(raw: &str) -> AgentId {
    raw.parse().expect("valid agent id")
}

/// Parses a session id produced by the API.
pub fn session_id(raw: &str) -> SessionId {
    raw.parse().expect("valid session id")
}

/// Parses a run id produced by the API.
pub fn run_id(raw: &str) -> RunId {
    raw.parse().expect("valid run id")
}

/// Upper bound for a request that must terminate on its own (SSE).
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
