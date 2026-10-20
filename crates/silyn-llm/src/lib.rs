//! OpenAI-compatible model gateway (W2).
//!
//! One adapter, no vendor branches. Capability differences are probed and
//! recorded rather than silently downgraded.

/// Default endpoint for the W6 experiment provider.
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";

/// Default model id for the W6 experiment. Pinned per run and recorded.
pub const DEEPSEEK_MODEL: &str = "deepseek-chat";
