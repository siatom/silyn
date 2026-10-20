//! Task, submission and oracle types shared by the generator and the runner.

use serde::{Deserialize, Serialize};
use silyn_core::SilynError;

/// Unit of the mass column. The header is not authoritative; the values are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unit {
    Kg,
    Lb,
}

impl Unit {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Kg => "kg",
            Self::Lb => "lb",
        }
    }
}

/// Data split. `test` is sealed and never generated on demand for candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Train,
    Dev,
    Test,
}

impl Split {
    pub const ALL: [Self; 3] = [Self::Train, Self::Dev, Self::Test];

    /// Fixed number of tasks per split. `suite.toml` must agree with this.
    #[must_use]
    pub const fn count(self) -> u32 {
        match self {
            Self::Train => 20,
            Self::Dev => 10,
            Self::Test => 10,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Train => "train",
            Self::Dev => "dev",
            Self::Test => "test",
        }
    }

    /// True for splits that may only be evaluated through [`crate::SealedTestSet`].
    #[must_use]
    pub const fn is_sealed(self) -> bool {
        matches!(self, Self::Test)
    }
}

impl std::fmt::Display for Split {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for Split {
    type Err = SilynError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "train" => Ok(Self::Train),
            "dev" => Ok(Self::Dev),
            "test" => Ok(Self::Test),
            other => Err(SilynError::Permanent(format!(
                "unknown split `{other}` (expected train, dev or test)"
            ))),
        }
    }
}

/// What a candidate (or the baseline solver) sees. Never contains the oracle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskInput {
    pub objective: String,
    pub csv: String,
}

/// Answer format accepted from a solver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub row_count: i64,
    pub missing_total: i64,
    pub duplicate_rows: i64,
    pub unit: Unit,
}

impl Submission {
    /// Parses a JSON submission, mapping parse failures to a schema error.
    pub fn from_json(text: &str) -> Result<Self, SilynError> {
        serde_json::from_str(text)
            .map_err(|error| SilynError::Schema(format!("invalid submission: {error}")))
    }
}

/// Ground truth computed by the generator. Never parsed back out of the CSV.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Oracle {
    pub row_count: u32,
    pub missing_total: u32,
    pub duplicate_rows: u32,
    pub unit: Unit,
}

impl Oracle {
    /// Exact match on every field. Partial credit is a W5 concern.
    #[must_use]
    pub fn matches(self, submission: &Submission) -> bool {
        submission.row_count == i64::from(self.row_count)
            && submission.missing_total == i64::from(self.missing_total)
            && submission.duplicate_rows == i64::from(self.duplicate_rows)
            && submission.unit == self.unit
    }
}

/// Controlled traps injected into a task, recorded for reporting and audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrapFlags {
    /// The header claims a different unit than the values.
    pub unit_mismatch: bool,
    /// At least one cell is empty.
    pub missing_values: bool,
    /// At least one exact duplicate row was appended.
    pub duplicate_rows: bool,
    /// Values at or above 1000 use a thousands separator.
    pub thousands_separators: bool,
    /// Column names differ from the canonical schema.
    pub column_name_drift: bool,
    /// The file starts with a UTF-8 byte order mark.
    pub byte_order_mark: bool,
}

/// Generator metadata for one task. Only the runner and tests may read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskMetadata {
    pub task_id: String,
    pub seed: u64,
    pub split: Split,
    /// Distinct data rows; duplicate copies are not counted.
    pub unique_rows: u32,
    /// Data rows as written, duplicate copies included.
    pub emitted_rows: u32,
    /// Actual unit of the mass column.
    pub unit: Unit,
    /// Empty cells across all emitted rows, duplicate copies included.
    pub missing_total: u32,
    pub duplicate_rows: u32,
    pub traps: TrapFlags,
    pub oracle: Oracle,
}

/// A generated task. The `test` split never leaves the sealed module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub metadata: TaskMetadata,
    pub input: TaskInput,
}
