//! Recorded provider responses for offline integration tests.
//!
//! A cassette maps a prompt fingerprint to a recorded completion. Replay never
//! calls the network and fails loudly on a missing entry.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use silyn_core::SilynError;
use silyn_core::contracts::{ModelProvider, ModelRequest, ModelResponse, Role};

#[derive(Debug, Clone, Deserialize)]
pub struct CassetteEntry {
    pub model: String,
    pub prompt_sha256: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct CassetteFile {
    entries: Vec<CassetteEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct Cassette {
    entries: HashMap<String, CassetteEntry>,
}

impl Cassette {
    /// Loads a cassette from JSON.
    pub fn from_json(input: &str) -> Result<Self, SilynError> {
        let file: CassetteFile = serde_json::from_str(input)
            .map_err(|error| SilynError::Schema(format!("invalid cassette: {error}")))?;
        let entries = file
            .entries
            .into_iter()
            .map(|entry| (entry.prompt_sha256.clone(), entry))
            .collect();
        Ok(Self { entries })
    }

    /// Returns a provider that replays this cassette.
    #[must_use]
    pub fn provider(&self) -> CassetteProvider {
        CassetteProvider {
            entries: self.entries.clone(),
        }
    }
}

pub struct CassetteProvider {
    entries: HashMap<String, CassetteEntry>,
}

#[async_trait]
impl ModelProvider for CassetteProvider {
    async fn complete(&self, request: ModelRequest) -> Result<ModelResponse, SilynError> {
        let fingerprint = prompt_fingerprint(&request);
        let entry = self.entries.get(&fingerprint).ok_or_else(|| {
            SilynError::Permanent(format!(
                "cassette has no entry for prompt {fingerprint} and model {}",
                request.model
            ))
        })?;

        if entry.model != request.model {
            return Err(SilynError::Permanent(format!(
                "cassette entry is for model {} but request used {}",
                entry.model, request.model
            )));
        }

        Ok(ModelResponse {
            content: entry.content.clone(),
            model: entry.model.clone(),
            input_tokens: entry.input_tokens,
            output_tokens: entry.output_tokens,
        })
    }
}

/// Stable fingerprint of a prompt: model plus ordered role/content pairs.
#[must_use]
pub fn prompt_fingerprint(request: &ModelRequest) -> String {
    let mut hasher = Sha256::new();
    hasher.update(request.model.as_bytes());
    for message in &request.messages {
        hasher.update([0x1f]);
        hasher.update(role_name(message.role).as_bytes());
        hasher.update([0x1f]);
        hasher.update(message.content.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}
