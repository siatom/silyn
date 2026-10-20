//! Stable identifiers.
//!
//! All identifiers are UUIDv7 so they sort by creation time while staying opaque
//! to callers. Wire format is the canonical hyphenated string.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Creates a new time-ordered identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            /// Wraps an existing UUID, for example when reading from storage.
            #[must_use]
            pub fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            /// Returns the underlying UUID.
            #[must_use]
            pub fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(value)?))
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

define_id!(
    /// A persistent agent identity.
    AgentId
);
define_id!(
    /// A conversation thread owned by an agent.
    SessionId
);
define_id!(
    /// A user objective with constraints.
    TaskId
);
define_id!(
    /// One execution attempt of a task.
    RunId
);
define_id!(
    /// One model or tool operation inside a run.
    StepId
);
define_id!(
    /// A long-term memory record.
    MemoryId
);
define_id!(
    /// A learned experience extracted from one or more runs.
    ExperienceId
);
define_id!(
    /// A proposed, versioned change to a mutable policy artifact.
    CandidateId
);
define_id!(
    /// An immutable policy artifact version.
    PolicyVersionId
);
define_id!(
    /// An audited promotion of a candidate.
    PromotionId
);
define_id!(
    /// An append-only journal event.
    EventId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_time_ordered_and_round_trip() {
        let first = RunId::new();
        let second = RunId::new();
        assert!(first < second, "uuid v7 must sort by creation order");

        let text = second.to_string();
        let parsed: RunId = text.parse().expect("valid uuid");
        assert_eq!(parsed, second);
    }

    #[test]
    fn ids_serialize_transparently() {
        let id = AgentId::new();
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(json, format!("\"{id}\""));
        let back: AgentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }
}
