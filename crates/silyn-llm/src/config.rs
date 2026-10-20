//! Provider configuration.
//!
//! The adapter is configured entirely from the environment. API keys are read
//! through an indirection variable so a vendor key never appears in a config
//! file or a log line.

use std::env;
use std::time::Duration;

use silyn_core::SilynError;

use crate::DEEPSEEK_BASE_URL;
use crate::DEEPSEEK_MODEL;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input_per_million: f64,
    pub output_per_million: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
    pub max_retries: u32,
    pub price: Option<Price>,
}

impl ProviderConfig {
    /// Reads provider settings from the process environment.
    ///
    /// `SILYN_LLM_API_KEY_ENV` names the variable that holds the key; it defaults
    /// to `DEEPSEEK_API_KEY`.
    pub fn from_env() -> Result<Self, SilynError> {
        let key_env =
            env::var("SILYN_LLM_API_KEY_ENV").unwrap_or_else(|_| "DEEPSEEK_API_KEY".to_owned());
        let api_key = env::var(&key_env)
            .map_err(|_| SilynError::Permanent(format!("missing API key variable {key_env}")))?;
        if api_key.trim().is_empty() {
            return Err(SilynError::Permanent(format!(
                "API key variable {key_env} is empty"
            )));
        }

        let timeout_secs = parse_u64("SILYN_LLM_TIMEOUT_SECS", 120)?;
        let max_retries = parse_u32("SILYN_LLM_MAX_RETRIES", 3)?;

        Ok(Self {
            base_url: env::var("SILYN_LLM_BASE_URL")
                .unwrap_or_else(|_| DEEPSEEK_BASE_URL.to_owned()),
            api_key,
            model: env::var("SILYN_LLM_MODEL").unwrap_or_else(|_| DEEPSEEK_MODEL.to_owned()),
            timeout: Duration::from_secs(timeout_secs),
            max_retries,
            price: read_price()?,
        })
    }

    /// Cost of a response in USD when a price table is configured.
    #[must_use]
    pub fn cost_usd(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        self.price.map(|price| {
            (input_tokens as f64 / 1_000_000.0) * price.input_per_million
                + (output_tokens as f64 / 1_000_000.0) * price.output_per_million
        })
    }
}

fn parse_u64(name: &str, default: u64) -> Result<u64, SilynError> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| SilynError::Permanent(format!("{name} must be an integer"))),
        Err(_) => Ok(default),
    }
}

fn parse_u32(name: &str, default: u32) -> Result<u32, SilynError> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| SilynError::Permanent(format!("{name} must be an integer"))),
        Err(_) => Ok(default),
    }
}

fn read_price() -> Result<Option<Price>, SilynError> {
    let input = env::var("SILYN_LLM_INPUT_PRICE_PER_M").ok();
    let output = env::var("SILYN_LLM_OUTPUT_PRICE_PER_M").ok();
    match (input, output) {
        (Some(input), Some(output)) => Ok(Some(Price {
            input_per_million: input
                .parse()
                .map_err(|_| SilynError::Permanent("invalid input price".to_owned()))?,
            output_per_million: output
                .parse()
                .map_err(|_| SilynError::Permanent("invalid output price".to_owned()))?,
        })),
        _ => Ok(None),
    }
}
