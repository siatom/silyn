use std::time::Duration;

use silyn_core::contracts::{Message, ModelProvider, ModelRequest, ModelResponse, Role};
use silyn_llm::{
    Cassette, HttpRequest, HttpResponse, MockProvider, MockTransport, OpenAiCompatibleProvider,
    ProviderConfig, prompt_fingerprint,
};

fn config(max_retries: u32) -> ProviderConfig {
    ProviderConfig {
        base_url: "https://example.test/v1".to_owned(),
        api_key: "test-key".to_owned(),
        model: "test-model".to_owned(),
        timeout: Duration::from_secs(1),
        max_retries,
        price: None,
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "test-model".to_owned(),
        messages: vec![
            Message {
                role: Role::System,
                content: "be precise".to_owned(),
            },
            Message {
                role: Role::User,
                content: "summarize".to_owned(),
            },
        ],
        temperature: Some(0.0),
        max_tokens: Some(64),
    }
}

fn completion_body(content: &str) -> String {
    serde_json::json!({
        "model": "test-model-2026",
        "choices": [{ "message": { "role": "assistant", "content": content } }],
        "usage": { "prompt_tokens": 11, "completion_tokens": 7 }
    })
    .to_string()
}

#[tokio::test]
async fn request_shape_matches_openai_protocol() {
    let transport = MockTransport::new(vec![HttpResponse {
        status: 200,
        body: completion_body("done"),
    }]);
    let provider = OpenAiCompatibleProvider::new(config(0), transport);

    let response = provider.complete(request()).await.expect("complete");
    assert_eq!(response.content, "done");
    assert_eq!(response.model, "test-model-2026");
    assert_eq!(response.input_tokens, 11);
    assert_eq!(response.output_tokens, 7);

    let requests = provider.into_parts().1.requests();
    assert_eq!(requests.len(), 1);
    let HttpRequest { url, headers, body } = &requests[0];
    assert_eq!(url, "https://example.test/v1/chat/completions");
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "Authorization" && value == "Bearer test-key")
    );

    let payload: serde_json::Value = serde_json::from_str(body).expect("json body");
    assert_eq!(payload["model"], "test-model");
    assert_eq!(payload["messages"][1]["content"], "summarize");
    assert_eq!(payload["temperature"], 0.0);
    assert!(payload.get("response_format").is_none());
}

#[tokio::test]
async fn rate_limit_is_retried() {
    let transport = MockTransport::new(vec![
        HttpResponse {
            status: 429,
            body: "slow down".to_owned(),
        },
        HttpResponse {
            status: 200,
            body: completion_body("after retry"),
        },
    ]);
    let provider = OpenAiCompatibleProvider::new(config(1), transport);

    let response = provider.complete(request()).await.expect("retry");
    assert_eq!(response.content, "after retry");
    assert_eq!(provider.into_parts().1.requests().len(), 2);
}

#[tokio::test]
async fn bad_request_is_not_retried() {
    let transport = MockTransport::new(vec![HttpResponse {
        status: 400,
        body: "invalid request".to_owned(),
    }]);
    let provider = OpenAiCompatibleProvider::new(config(3), transport);

    let error = provider
        .complete(request())
        .await
        .expect_err("400 must fail");
    assert!(
        matches!(error, silyn_core::SilynError::Permanent(_)),
        "{error:?}"
    );
    assert_eq!(provider.into_parts().1.requests().len(), 1);
}

#[tokio::test]
async fn structured_output_repairs_once() {
    let transport = MockTransport::new(vec![
        HttpResponse {
            status: 200,
            body: completion_body("this is not json"),
        },
        HttpResponse {
            status: 200,
            body: completion_body("{\"ok\":true}"),
        },
    ]);
    let provider = OpenAiCompatibleProvider::new(config(0), transport);

    let value: serde_json::Value = provider
        .complete_structured(request(), "Return a summary object.")
        .await
        .expect("repair");
    assert_eq!(value["ok"], true);

    let (_, transport) = provider.into_parts();
    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    let repair: serde_json::Value = serde_json::from_str(&requests[1].body).expect("repair body");
    let last = repair["messages"]
        .as_array()
        .and_then(|messages| messages.last())
        .expect("repair message");
    assert!(
        last["content"]
            .as_str()
            .expect("content")
            .contains("valid JSON")
    );
    assert_eq!(repair["response_format"]["type"], "json_object");
}

#[tokio::test]
async fn mock_provider_is_offline() {
    let provider = MockProvider::new(vec![ModelResponse {
        content: "scripted".to_owned(),
        model: "test-model".to_owned(),
        input_tokens: 1,
        output_tokens: 2,
    }]);

    let response = provider.complete(request()).await.expect("mock");
    assert_eq!(response.content, "scripted");
    assert_eq!(provider.requests().len(), 1);
}

#[tokio::test]
async fn cassette_replays_by_prompt() {
    let prompt = request();
    let fingerprint = prompt_fingerprint(&prompt);
    let json = serde_json::json!({
        "entries": [{
            "model": "test-model",
            "prompt_sha256": fingerprint,
            "content": "recorded",
            "input_tokens": 3,
            "output_tokens": 4
        }]
    })
    .to_string();

    let provider = Cassette::from_json(&json).expect("cassette").provider();
    let response = provider.complete(prompt).await.expect("replay");
    assert_eq!(response.content, "recorded");
    assert_eq!(response.output_tokens, 4);

    let mut other = request();
    other.messages[1].content = "different".to_owned();
    let error = provider
        .complete(other)
        .await
        .expect_err("missing entry must fail");
    assert!(matches!(error, silyn_core::SilynError::Permanent(_)));
}
