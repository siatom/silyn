//! Deterministic provider for tests and offline development.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use silyn_core::SilynError;
use silyn_core::contracts::{ModelProvider, ModelRequest, ModelResponse};

#[derive(Default)]
pub struct MockProvider {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl MockProvider {
    #[must_use]
    pub fn new(responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Requests observed so far, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .map(|requests| requests.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl ModelProvider for MockProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, SilynError> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request);
        }
        self.responses
            .lock()
            .ok()
            .and_then(|mut responses| responses.pop_front())
            .ok_or_else(|| SilynError::Permanent("mock provider exhausted".to_owned()))
    }
}
