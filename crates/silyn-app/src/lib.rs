//! Application service layer.
//!
//! The CLI (W2) and the HTTP API (W7) are thin adapters over this crate. Business
//! rules live here so both frontends share identical behavior.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Human-readable version line used by the CLI smoke test.
#[must_use]
pub fn version_line() -> String {
    format!("silyn {VERSION}")
}
