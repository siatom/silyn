//! Enum encoding shared by storage modules.
//!
//! Enums are persisted as their `serde` snake_case string form so the database
//! vocabulary matches the JSON protocol and does not drift from `silyn-core`.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use silyn_core::SilynError;

pub(crate) fn encode_enum<T: Serialize>(value: &T) -> Result<String, SilynError> {
    let encoded = serde_json::to_value(value)?;
    encoded
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| SilynError::Schema("enum did not encode as a string".to_owned()))
}

pub(crate) fn decode_enum<T: DeserializeOwned>(value: &str) -> Result<T, SilynError> {
    serde_json::from_value(Value::String(value.to_owned())).map_err(Into::into)
}
