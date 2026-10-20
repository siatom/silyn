//! Helpers shared by the CLI and the HTTP adapter.
//!
//! These live in the app crate so the transports cannot drift: the CLI's
//! `--model` default and the HTTP run body's `model` default resolve through
//! the same rule.

use std::env;

/// Environment variable that overrides the model id for a run.
pub const MODEL_ENV: &str = "SILYN_LLM_MODEL";

/// Default model id for a run: [`MODEL_ENV`], else the provider default.
///
/// A blank variable is treated as unset, so an exported empty value cannot
/// produce a run record with an empty model id.
#[must_use]
pub fn default_model() -> String {
    env::var(MODEL_ENV)
        .ok()
        .map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| silyn_llm::DEEPSEEK_MODEL.to_owned())
}
