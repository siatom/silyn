//! Sealed test set.
//!
//! The test split is generated on demand inside this module and stored in a
//! private field. The public surface is deliberately limited to length, digest,
//! [`SealedTestSet::evaluate`] and [`SealedTestSet::load_for_manifest`]: there is
//! no accessor that hands out test tasks, inputs or oracles. A [`Solver`] only
//! ever receives [`TaskInput`], and the crate-private
//! [`SealedTestSet::tasks`] is reachable only from the arm runner.

use silyn_core::SilynError;

use crate::evaluator::{ArmOutcome, Solver};
use crate::generator;
use crate::manifest::SuiteManifest;
use crate::task::{Split, Task};

/// Sealed holder of the `csv-stats-v1` test split.
pub struct SealedTestSet {
    tasks: Vec<Task>,
    digest: String,
}

impl std::fmt::Debug for SealedTestSet {
    /// Never prints tasks or oracles, even through `{:?}`.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SealedTestSet")
            .field("tasks", &self.tasks.len())
            .field("digest", &self.digest)
            .finish()
    }
}

impl SealedTestSet {
    /// Builds the sealed set from the deterministic generator.
    #[must_use]
    pub fn load_default() -> Self {
        let tasks = generator::sealed_test_tasks();
        let digest = generator::digest_of(&tasks);
        Self { tasks, digest }
    }

    /// Loads the test split for an operator-approved sealed run.
    ///
    /// Fails unless the manifest declares `sealed = true` and the recorded test
    /// digest matches the generated split, so editing the suite without a new
    /// version cannot produce a report.
    pub fn load_for_manifest(manifest: &SuiteManifest) -> Result<Self, SilynError> {
        if !manifest.sealed {
            return Err(SilynError::PolicyDenied(
                "the suite is not sealed (`sealed = false`); run train or dev, or create a new \
                 sealed suite version"
                    .to_owned(),
            ));
        }
        let set = Self::load_default();
        let expected = &manifest.split_entry(Split::Test).sha256;
        if &set.digest != expected {
            return Err(SilynError::Schema(format!(
                "sealed test split digest {} does not match suite.toml ({}); regenerate the \
                 manifest for a new suite version",
                set.digest, expected
            )));
        }
        Ok(set)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Digest of the sealed split (inputs and oracles). Must equal the `test`
    /// entry in `suite.toml`.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Crate-private task access for the arm runner. Never public: a candidate
    /// path must not be able to name test tasks or oracles.
    pub(crate) fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    /// Scores a solver against the sealed tasks, in-process.
    pub fn evaluate(&self, solver: &dyn Solver, repeats: u32) -> ArmOutcome {
        let repeats = repeats.clamp(1, crate::evaluator::MAX_REPEATS);
        let evaluator =
            crate::evaluator::Evaluator::with_seed(repeats, crate::evaluator::DEFAULT_RUN_SEED)
                .expect("repeats is clamped to the protocol range");
        evaluator.evaluate_sealed(solver, self)
    }
}

impl Default for SealedTestSet {
    fn default() -> Self {
        Self::load_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_generator_and_manifest() {
        let set = SealedTestSet::load_default();
        assert_eq!(set.len(), Split::Test.count() as usize);
        assert_eq!(
            set.digest(),
            generator::split_sha256(Split::Test).expect("test split digest")
        );
    }

    #[test]
    fn no_public_task_or_oracle_accessor() {
        // Regression guard: the sealed type must not grow a getter.
        let source = include_str!("sealed.rs");
        for forbidden in [
            ["pub", "fn", "tasks"].join(" "),
            ["pub", "fn", "task"].join(" "),
            ["pub", "fn", "oracles"].join(" "),
            ["pub", "fn", "oracle"].join(" "),
        ] {
            assert!(
                !source.contains(&forbidden),
                "SealedTestSet must not expose `{forbidden}`"
            );
        }
    }
}
