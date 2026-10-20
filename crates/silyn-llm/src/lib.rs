//! OpenAI-compatible model gateway.
//!
//! One adapter, no vendor branches. Capability differences are probed by the
//! runtime and recorded; this crate only guarantees protocol behavior:
//! request shaping, retry classification, structured output repair and usage
//! accounting.

pub mod cassette;
pub mod config;
pub mod mock;
pub mod provider;
pub mod transport;

pub use cassette::{Cassette, CassetteEntry, CassetteProvider, prompt_fingerprint};
pub use config::{Price, ProviderConfig};
pub use mock::MockProvider;
pub use provider::OpenAiCompatibleProvider;
pub use transport::{
    HttpRequest, HttpResponse, MockTransport, ReqwestTransport, Transport, TransportError,
};

/// Default endpoint for the W6 experiment provider.
pub const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";

/// Default model id for the W6 experiment. Pinned per run and recorded.
pub const DEEPSEEK_MODEL: &str = "deepseek-chat";
