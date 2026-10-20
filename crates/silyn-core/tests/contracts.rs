//! Contract tests are ignored until the corresponding subsystem lands.
//! W0 requires that they exist and compile; W1–W5 make them executable.

use silyn_core::contracts::{
    MemoryHit, MemoryQuery, MemoryRepository, MemoryScope, ModelProvider, ModelRequest,
    ModelResponse, PromotionDecision, PromotionGate, PromotionPolicy,
};
use silyn_core::error::SilynError;

#[test]
#[ignore = "contract test: enable when the W2 model gateway lands"]
fn model_provider_round_trip() {
    struct Echo;

    #[async_trait::async_trait]
    impl ModelProvider for Echo {
        async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, SilynError> {
            Ok(ModelResponse {
                content: format!("{} messages", request.messages.len()),
                model: request.model,
                input_tokens: 0,
                output_tokens: 0,
            })
        }
    }

    let provider = Echo;
    let request = ModelRequest {
        model: "test".into(),
        messages: vec![],
        temperature: None,
        max_tokens: None,
    };
    let response = futures_lite_block_on(provider.complete(request));
    assert_eq!(response.expect("echo").content, "0 messages");
}

#[test]
#[ignore = "contract test: enable when the W4 memory engine lands"]
fn memory_search_returns_provenance() {
    struct Empty;

    #[async_trait::async_trait]
    impl MemoryRepository for Empty {
        async fn search(
            &self,
            _scope: MemoryScope,
            _query: MemoryQuery,
        ) -> Result<Vec<MemoryHit>, SilynError> {
            Ok(vec![])
        }
    }

    let hits = futures_lite_block_on(Empty.search(
        MemoryScope::Agent,
        MemoryQuery {
            text: "units".into(),
            limit: 5,
            types: vec![],
        },
    ))
    .expect("empty search");
    assert!(hits.is_empty());
}

#[test]
#[ignore = "contract test: enable when the W5 promotion gate lands"]
fn promotion_gate_requires_low_risk_for_auto() {
    struct RejectAll;

    #[async_trait::async_trait]
    impl PromotionGate for RejectAll {
        async fn decide(
            &self,
            _report: silyn_core::contracts::EvaluationReport,
            _policy: PromotionPolicy,
        ) -> Result<PromotionDecision, SilynError> {
            Ok(PromotionDecision::AwaitApproval)
        }
    }

    let decision = futures_lite_block_on(RejectAll.decide(
        silyn_core::contracts::EvaluationReport {
            candidate_id: silyn_core::ids::CandidateId::new(),
            suite_version: "csv-stats-v1".into(),
            baseline_id: "baseline".into(),
            metrics: serde_json::json!({}),
        },
        PromotionPolicy {
            allow_low_risk_auto: true,
            min_gain_pp: 10.0,
            max_cost_ratio: 2.0,
            max_regression_pp: 5.0,
        },
    ))
    .expect("decision");
    assert_eq!(decision, PromotionDecision::AwaitApproval);
}

/// Minimal blocking executor so contract tests stay dependency-free.
fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll};

    let mut context = Context::from_waker(std::task::Waker::noop());
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}
