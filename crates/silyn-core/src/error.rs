//! Error taxonomy shared by every subsystem.
//!
//! The class drives retry policy and recovery behavior. A caller must not retry
//! `Permanent` or `PolicyDenied`, and must never treat `Indeterminate` as success.

use serde::{Deserialize, Serialize};

/// How a failure should be handled by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Safe to retry with backoff.
    Transient,
    /// Retrying without a change will fail again.
    Permanent,
    /// Rejected by the permission or promotion gate.
    PolicyDenied,
    /// The side effect may or may not have happened; requires explicit resolution.
    Indeterminate,
}

/// Unified error type across Silyn.
#[derive(Debug, thiserror::Error)]
pub enum SilynError {
    #[error("transient failure: {0}")]
    Transient(String),

    #[error("permanent failure: {0}")]
    Permanent(String),

    #[error("policy denied: {0}")]
    PolicyDenied(String),

    #[error("indeterminate outcome: {0}")]
    Indeterminate(String),

    #[error("storage failure: {0}")]
    Storage(String),

    #[error("model failure: {0}")]
    Model(String),

    #[error("invalid state transition: {from} -> {to}")]
    InvalidTransition { from: String, to: String },

    #[error("schema validation failed: {0}")]
    Schema(String),
}

impl SilynError {
    /// Classifies the error for retry and recovery decisions.
    #[must_use]
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Transient(_) => ErrorClass::Transient,
            Self::Permanent(_) | Self::Storage(_) | Self::Model(_) | Self::Schema(_) => {
                ErrorClass::Permanent
            }
            Self::PolicyDenied(_) | Self::InvalidTransition { .. } => ErrorClass::PolicyDenied,
            Self::Indeterminate(_) => ErrorClass::Indeterminate,
        }
    }

    /// True when retrying the same operation is safe and potentially useful.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self.class(), ErrorClass::Transient)
    }
}

impl From<serde_json::Error> for SilynError {
    fn from(value: serde_json::Error) -> Self {
        Self::Schema(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_are_stable() {
        assert_eq!(
            SilynError::Transient("x".into()).class(),
            ErrorClass::Transient
        );
        assert_eq!(
            SilynError::Permanent("x".into()).class(),
            ErrorClass::Permanent
        );
        assert_eq!(
            SilynError::PolicyDenied("x".into()).class(),
            ErrorClass::PolicyDenied
        );
        assert_eq!(
            SilynError::Indeterminate("x".into()).class(),
            ErrorClass::Indeterminate
        );
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        assert!(SilynError::Transient("x".into()).is_retryable());
        assert!(!SilynError::PolicyDenied("x".into()).is_retryable());
        assert!(!SilynError::Indeterminate("x".into()).is_retryable());
    }
}
