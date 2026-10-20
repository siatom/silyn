//! `SuiteManifest` parser and validator (`schemas/suite_manifest.schema.json`).

use std::path::Path;

use serde::{Deserialize, Serialize};
use silyn_core::SilynError;

use crate::generator::{SUITE_ID, TASK_FAMILY};
use crate::task::Split;

/// Suite manifest as stored in `eval/suites/<suite-id>/suite.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteManifest {
    pub suite_id: String,
    pub version: String,
    pub task_family: String,
    pub repeats: u8,
    pub splits: SplitSet,
    pub thresholds: Thresholds,
    pub sealed: bool,
}

/// Per-split task count and digest, as required by the schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitSet {
    pub train: SplitEntry,
    pub dev: SplitEntry,
    pub test: SplitEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplitEntry {
    pub count: u32,
    pub sha256: String,
}

/// Pre-registered thresholds. Never edited after seeing results.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Thresholds {
    pub min_gain_pp: f64,
    pub max_cost_ratio: f64,
    pub max_regression_pp: f64,
    pub max_cost_usd: f64,
}

impl SuiteManifest {
    /// Reads and parses a manifest. Validation is a separate step.
    pub fn load(path: &Path) -> Result<Self, SilynError> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            SilynError::Permanent(format!("read manifest {}: {error}", path.display()))
        })?;
        toml::from_str(&text).map_err(|error| {
            SilynError::Schema(format!("parse manifest {}: {error}", path.display()))
        })
    }

    /// Checks invariants the JSON schema cannot express: suite identity, repeat
    /// budget, split counts against the generator and digest shape.
    pub fn validate(&self) -> Result<(), SilynError> {
        if self.suite_id != SUITE_ID {
            return Err(SilynError::Schema(format!(
                "suite_id `{}` does not match `{SUITE_ID}`",
                self.suite_id
            )));
        }
        if self.task_family != TASK_FAMILY {
            return Err(SilynError::Schema(format!(
                "task_family `{}` does not match `{TASK_FAMILY}`",
                self.task_family
            )));
        }
        if !(1..=5).contains(&self.repeats) {
            return Err(SilynError::Schema(format!(
                "repeats must be 1..=5, got {}",
                self.repeats
            )));
        }
        for split in Split::ALL {
            let entry = self.split_entry(split);
            if entry.count != split.count() {
                return Err(SilynError::Schema(format!(
                    "{split} split declares {} tasks but the generator emits {}",
                    entry.count,
                    split.count()
                )));
            }
            if !is_sha256(&entry.sha256) {
                return Err(SilynError::Schema(format!(
                    "{split} split sha256 is not a lowercase hex digest: {}",
                    entry.sha256
                )));
            }
        }
        Ok(())
    }

    /// Manifest entry for a split.
    #[must_use]
    pub fn split_entry(&self, split: Split) -> &SplitEntry {
        match split {
            Split::Train => &self.splits.train,
            Split::Dev => &self.splits.dev,
            Split::Test => &self.splits.test,
        }
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
