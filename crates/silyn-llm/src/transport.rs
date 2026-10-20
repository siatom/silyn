//! Transport abstraction.
//!
//! The provider talks to an HTTP-like transport, not to a vendor SDK. Tests use
//! an in-memory transport, so the whole gateway runs offline.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use silyn_core::SilynError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("request timed out")]
    Timeout,
    #[error("network failure: {0}")]
    Network(String),
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn post_json(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// Production transport backed by `reqwest` with rustls.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(timeout: Duration) -> Result<Self, SilynError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| SilynError::Permanent(format!("http client: {error}")))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn post_json(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let mut builder = self.client.post(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response = builder.body(request.body).send().await.map_err(|error| {
            if error.is_timeout() {
                TransportError::Timeout
            } else {
                TransportError::Network(error.to_string())
            }
        })?;

        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|error| TransportError::Network(format!("failed to read body: {error}")))?;
        Ok(HttpResponse { status, body })
    }
}

/// Deterministic in-memory transport for tests and offline runs.
#[derive(Default)]
pub struct MockTransport {
    responses: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl MockTransport {
    #[must_use]
    pub fn new(responses: Vec<HttpResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Requests observed so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn post_json(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request);
        }
        let response = self
            .responses
            .lock()
            .ok()
            .and_then(|mut responses| responses.pop_front());
        response.ok_or_else(|| TransportError::Network("mock transport exhausted".to_owned()))
    }
}
