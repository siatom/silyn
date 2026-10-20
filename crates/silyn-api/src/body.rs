//! Request parsing helpers shared by the route modules.
//!
//! Parsing is explicit rather than extractor-driven so every malformed input
//! produces the same JSON error shape (`400 permanent`) instead of a plain-text
//! axum rejection.

use axum::body::Bytes;
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// Parses a path parameter as a Silyn id.
pub(crate) fn parse_id<T>(raw: &str, entity: &str) -> Result<T, ApiError>
where
    T: std::str::FromStr,
{
    raw.parse()
        .map_err(|_| ApiError::schema(format!("`{raw}` is not a valid {entity} id")))
}

/// Parses a required JSON body.
pub(crate) fn parse_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    if body.is_empty() {
        return Err(ApiError::schema("request body must be a JSON object"));
    }
    serde_json::from_slice(body)
        .map_err(|error| ApiError::schema(format!("invalid JSON body: {error}")))
}

/// Parses an optional JSON body; an absent body means `T::default()`.
pub(crate) fn parse_optional_body<T: DeserializeOwned + Default>(
    body: &Bytes,
) -> Result<T, ApiError> {
    if body.is_empty() {
        return Ok(T::default());
    }
    parse_body(body)
}
