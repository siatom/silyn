//! Sealed test set.
//!
//! The test split is generated on demand inside this module and stored in a
//! private field. The public surface is deliberately limited to length, digest
//! and [`SealedTestSet::evaluate`]: there is no accessor that hands out test
//! tasks, inputs or oracles. A [`Solver`] only ever receives [`TaskInput`].

use crate::evaluator::{ArmOutcome, Solver, run_tasks};
use crate::generator;
use crate::task::Task;

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

    /// Scores a solver against the sealed tasks. `repeats` is clamped to 1..=5.
    #[must_use]
    pub fn evaluate(&self, solver: &dyn Solver, repeats: u32) -> ArmOutcome {
        run_tasks(&self.tasks, solver, repeats)
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
    use crate::task::Split;

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
