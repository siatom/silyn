//! OpenAI-compatible provider.

use std::time::Duration;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use silyn_core::SilynError;
use silyn_core::contracts::{Message, ModelProvider, ModelRequest, ModelResponse, Role};

use crate::config::ProviderConfig;
use crate::transport::{HttpRequest, HttpResponse, Transport, TransportError};

const MAX_ERROR_BODY_CHARS: usize = 400;

#[derive(Debug, Clone, PartialEq)]
struct Completion {
    content: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
}

/// A provider that speaks the OpenAI chat-completions protocol.
pub struct OpenAiCompatibleProvider<T: Transport> {
    config: ProviderConfig,
    transport: T,
}

impl<T: Transport> OpenAiCompatibleProvider<T> {
    #[must_use]
    pub fn new(config: ProviderConfig, transport: T) -> Self {
        Self { config, transport }
    }

    #[must_use]
    pub fn config(&self) -> &ProviderConfig {
        &self.config
    }

    #[must_use]
    pub fn into_parts(self) -> (ProviderConfig, T) {
        (self.config, self.transport)
    }

    /// Requests JSON output and parses it. A malformed response triggers exactly
    /// one repair attempt with the original request plus a repair instruction.
    pub async fn complete_structured<D: DeserializeOwned>(
        &self,
        request: ModelRequest,
        repair_hint: &str,
    ) -> Result<D, SilynError> {
        let first = self.send(&request, true).await?;
        match decode_json(&first.content) {
            Ok(value) => Ok(value),
            Err(first_error) => {
                let mut repair = request;
                repair.messages.push(Message {
                    role: Role::User,
                    content: format!(
                        "{repair_hint}\nReturn only one valid JSON object.\n\
                         The previous response could not be parsed: {first_error}"
                    ),
                });
                let second = self.send(&repair, true).await?;
                decode_json(&second.content).map_err(|error| {
                    SilynError::Schema(format!(
                        "structured output failed after one repair: {error}"
                    ))
                })
            }
        }
    }

    async fn send(
        &self,
        request: &ModelRequest,
        json_mode: bool,
    ) -> Result<Completion, SilynError> {
        let payload = build_payload(request, json_mode)?;
        let body = serde_json::to_string(&payload)?;
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );

        let mut last_error = None;
        for attempt in 0..=self.config.max_retries {
            let http_request = HttpRequest {
                url: url.clone(),
                headers: vec![
                    (
                        "Authorization".to_owned(),
                        format!("Bearer {}", self.config.api_key),
                    ),
                    ("Content-Type".to_owned(), "application/json".to_owned()),
                ],
                body: body.clone(),
            };

            match self.transport.post_json(http_request).await {
                Ok(response) if (200..300).contains(&response.status) => {
                    return decode_completion(&response, &self.config.model);
                }
                Ok(response) if retryable_status(response.status) => {
                    last_error = Some(SilynError::Transient(format!(
                        "provider returned HTTP {}: {}",
                        response.status,
                        truncate(&response.body)
                    )));
                }
                Ok(response) => {
                    return Err(SilynError::Permanent(format!(
                        "provider returned HTTP {}: {}",
                        response.status,
                        truncate(&response.body)
                    )));
                }
                Err(TransportError::Timeout) => {
                    last_error = Some(SilynError::Transient(
                        "provider request timed out".to_owned(),
                    ));
                }
                Err(TransportError::Network(message)) => {
                    last_error = Some(SilynError::Transient(format!(
                        "provider network error: {message}"
                    )));
                }
            }

            if attempt < self.config.max_retries {
                tokio::time::sleep(backoff(attempt)).await;
            }
        }

        Err(last_error
            .unwrap_or_else(|| SilynError::Transient("retry budget exhausted".to_owned())))
    }
}

#[async_trait]
impl<T: Transport> ModelProvider for OpenAiCompatibleProvider<T> {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, SilynError> {
        let completion = self.send(&request, false).await?;
        Ok(ModelResponse {
            content: completion.content,
            model: completion.model,
            input_tokens: completion.input_tokens,
            output_tokens: completion.output_tokens,
        })
    }
}

fn build_payload(request: &ModelRequest, json_mode: bool) -> Result<Value, SilynError> {
    let mut payload = Map::new();
    payload.insert("model".to_owned(), json!(request.model));
    payload.insert("messages".to_owned(), json!(request.messages));
    if let Some(temperature) = request.temperature {
        payload.insert("temperature".to_owned(), json!(temperature));
    }
    if let Some(max_tokens) = request.max_tokens {
        payload.insert("max_tokens".to_owned(), json!(max_tokens));
    }
    if json_mode {
        payload.insert(
            "response_format".to_owned(),
            json!({ "type": "json_object" }),
        );
    }
    Ok(Value::Object(payload))
}

fn decode_completion(
    response: &HttpResponse,
    fallback_model: &str,
) -> Result<Completion, SilynError> {
    let value: Value = serde_json::from_str(&response.body).map_err(|error| {
        SilynError::Permanent(format!("provider returned malformed JSON: {error}"))
    })?;

    let content = value
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            SilynError::Permanent("provider response has no message content".to_owned())
        })?
        .to_owned();

    let model = value
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(fallback_model)
        .to_owned();

    let input_tokens = value
        .get("usage")
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = value
        .get("usage")
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    Ok(Completion {
        content,
        model,
        input_tokens,
        output_tokens,
    })
}

fn decode_json<D: DeserializeOwned>(content: &str) -> Result<D, SilynError> {
    serde_json::from_str(content)
        .map_err(|error| SilynError::Schema(format!("invalid JSON content: {error}")))
}

fn retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis(100 * 2_u64.pow(attempt.min(4)))
}

fn truncate(body: &str) -> String {
    let mut truncated: String = body.chars().take(MAX_ERROR_BODY_CHARS).collect();
    if body.chars().count() > MAX_ERROR_BODY_CHARS {
        truncated.push_str("...");
    }
    truncated
}
