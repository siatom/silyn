//! Error mapping from SQLx to the shared Silyn taxonomy.

use std::fmt::Display;
use std::str::FromStr;

use silyn_core::SilynError;

/// Maps a SQLx error to a Silyn error.
///
/// Integrity violations are permanent: retrying the same statement cannot fix
/// them. Connectivity and locking failures remain `Storage`, which the caller
/// may treat as retryable at a higher level.
pub fn db_error(error: sqlx::Error) -> SilynError {
    match &error {
        sqlx::Error::Database(database) if database.is_unique_violation() => {
            SilynError::Permanent(format!("unique constraint violated: {database}"))
        }
        sqlx::Error::Database(database) if database.is_foreign_key_violation() => {
            SilynError::Permanent(format!("foreign key violated: {database}"))
        }
        sqlx::Error::RowNotFound => SilynError::Permanent("row not found".to_owned()),
        _ => SilynError::Storage(error.to_string()),
    }
}

/// Parses a stored identifier.
pub fn parse_id<T>(value: &str, field: &str) -> Result<T, SilynError>
where
    T: FromStr,
    T::Err: Display,
{
    value
        .parse()
        .map_err(|error| SilynError::Permanent(format!("invalid {field}: {error}")))
}
