//! HTTP projection of the shared error taxonomy.
//!
//! The `class` field deliberately matches the CLI (`permanent`, `policy_denied`,
//! `transient`, `indeterminate`) so scripts can branch on one vocabulary. The
//! status code carries the HTTP-specific distinction the class cannot
//! (`400` for a bad request, `404` for an unknown id, `500` for storage or
//! provider failures).
//!
//! Messages are the bounded `SilynError` display strings. They never contain
//! request headers, request bodies, prompts or API keys, so they are safe to
//! return; stack traces are never constructed.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use silyn_core::{ErrorClass, SilynError};

/// One error response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    status: StatusCode,
    class: &'static str,
    message: String,
}

impl ApiError {
    /// Builds an error from an explicit status, class and message.
    #[must_use]
    pub fn new(status: StatusCode, class: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            class,
            message: message.into(),
        }
    }

    /// A request body or path parameter that does not match the contract.
    #[must_use]
    pub fn schema(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "permanent", message)
    }

    /// A missing or wrong bearer token.
    #[must_use]
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "missing or invalid bearer token",
        )
    }

    /// Projects a service error onto the HTTP status contract.
    #[must_use]
    pub fn from_silyn(error: &SilynError) -> Self {
        let (status, class) = project(error);
        Self::new(status, class, error.to_string())
    }

    /// HTTP status of this error.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// Wire class of this error.
    #[must_use]
    pub fn class(&self) -> &'static str {
        self.class
    }

    /// Operator-facing message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<SilynError> for ApiError {
    fn from(error: SilynError) -> Self {
        Self::from_silyn(&error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let payload = json!({
            "error": {
                "class": self.class,
                "status": self.status.as_u16(),
                "message": self.message,
            },
        });
        (self.status, Json(payload)).into_response()
    }
}

/// Status and wire class for one service error.
///
/// `Permanent` is split by message because the app layer uses it for both
/// validation (`400`) and missing rows (`404`); every other variant has one
/// meaning.
fn project(error: &SilynError) -> (StatusCode, &'static str) {
    match error {
        SilynError::Permanent(message) if is_not_found(message) => {
            (StatusCode::NOT_FOUND, class_name(error.class()))
        }
        SilynError::Permanent(_) => (StatusCode::BAD_REQUEST, class_name(error.class())),
        SilynError::Schema(_) => (StatusCode::BAD_REQUEST, class_name(error.class())),
        SilynError::PolicyDenied(_) | SilynError::InvalidTransition { .. } => {
            (StatusCode::FORBIDDEN, class_name(error.class()))
        }
        SilynError::Transient(_) => (StatusCode::SERVICE_UNAVAILABLE, class_name(error.class())),
        SilynError::Indeterminate(_) => (StatusCode::CONFLICT, class_name(error.class())),
        // A storage failure is a local, non-retryable server fault; a model
        // failure is a provider fault. Neither is a client error and neither
        // may look retryable without a human decision.
        SilynError::Storage(_) | SilynError::Model(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, class_name(error.class()))
        }
    }
}

/// True when the app layer reported a missing row.
///
/// The app formats these as `<entity> <id> not found` (and
/// `base policy version <id> not found for agent <id>`), so the marker is the
/// shared phrase rather than an exhaustive list of entities.
fn is_not_found(message: &str) -> bool {
    message.contains("not found")
}

/// Wire class, identical to the CLI's `--json` error class.
fn class_name(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Transient => "transient",
        ErrorClass::Permanent => "permanent",
        ErrorClass::PolicyDenied => "policy_denied",
        ErrorClass::Indeterminate => "indeterminate",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_covers_every_error_class() {
        let cases = [
            (
                SilynError::Permanent("run 0000 not found".to_owned()),
                StatusCode::NOT_FOUND,
                "permanent",
            ),
            (
                SilynError::Permanent("agent owner must not be empty".to_owned()),
                StatusCode::BAD_REQUEST,
                "permanent",
            ),
            (
                SilynError::Schema("invalid evaluation report".to_owned()),
                StatusCode::BAD_REQUEST,
                "permanent",
            ),
            (
                SilynError::PolicyDenied("capability not granted".to_owned()),
                StatusCode::FORBIDDEN,
                "policy_denied",
            ),
            (
                SilynError::InvalidTransition {
                    from: "evaluated".to_owned(),
                    to: "approved".to_owned(),
                },
                StatusCode::FORBIDDEN,
                "policy_denied",
            ),
            (
                SilynError::Transient("database is busy".to_owned()),
                StatusCode::SERVICE_UNAVAILABLE,
                "transient",
            ),
            (
                SilynError::Indeterminate("write may have happened".to_owned()),
                StatusCode::CONFLICT,
                "indeterminate",
            ),
            (
                SilynError::Storage("cannot open the database".to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "permanent",
            ),
            (
                SilynError::Model("provider returned 500".to_owned()),
                StatusCode::INTERNAL_SERVER_ERROR,
                "permanent",
            ),
        ];

        for (error, status, class) in cases {
            let api = ApiError::from_silyn(&error);
            assert_eq!(api.status(), status, "{error}");
            assert_eq!(api.class(), class, "{error}");
        }
    }

    #[test]
    fn invalid_transition_is_not_reported_as_success_or_retryable() {
        let api = ApiError::from(SilynError::InvalidTransition {
            from: "promoted".to_owned(),
            to: "rolled_back".to_owned(),
        });
        assert_ne!(api.status(), StatusCode::OK);
        assert_ne!(api.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn unauthorized_never_echoes_the_presented_token() {
        let api = ApiError::unauthorized();
        assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
        assert!(!api.message().contains("secret"));
    }
}
