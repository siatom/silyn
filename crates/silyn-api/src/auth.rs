//! Optional bearer-token authentication for `/v1/*`.
//!
//! Authentication is a transport concern: it does not change any business rule
//! and it is not an authorization decision inside the app. `/health` stays
//! public so a supervisor can probe the process without a credential.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::ApiState;
use crate::error::ApiError;

/// Environment variable holding the token. Empty means "no authentication".
pub const TOKEN_ENV: &str = "SILYN_API_TOKEN";

/// Rejects `/v1/*` requests without the configured bearer token.
///
/// When no token is configured the middleware is a pass-through: the surface is
/// already loopback-only.
pub(crate) async fn require_token(
    State(state): State<ApiState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.token() else {
        return next.run(request).await;
    };

    let presented = bearer_token(request.headers());
    if presented.is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes())) {
        return next.run(request).await;
    }

    let mut response = ApiError::unauthorized().into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

/// Extracts `Bearer <token>` from the headers, case-insensitively.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() { None } else { Some(token) }
}

/// Compares the SHA-256 digests of two tokens without early exit.
///
/// Hashing first hides the token length: the loop always runs over 32 bytes, so
/// neither a prefix nor a length difference can be observed by timing.
fn constant_time_eq(presented: &[u8], expected: &[u8]) -> bool {
    let presented = Sha256::digest(presented);
    let expected = Sha256::digest(expected);
    presented
        .iter()
        .zip(expected.iter())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn token_comparison_accepts_only_equal_values() {
        assert!(constant_time_eq(b"sekret", b"sekret"));
        assert!(!constant_time_eq(b"sekret", b"sekret "));
        assert!(!constant_time_eq(b"sekret", b"sekrey"));
        assert!(!constant_time_eq(b"sekret", b"sek"));
        assert!(!constant_time_eq(b"", b"sekret"));
    }

    #[test]
    fn bearer_header_is_parsed_case_insensitively() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("bearer   tok en  "),
        );
        assert_eq!(bearer_token(&headers), Some("tok en"));

        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer_token(&headers), None);

        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer   "));
        assert_eq!(bearer_token(&headers), None);
    }
}
