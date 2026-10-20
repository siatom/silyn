//! Arms A, B and C and their recorded configuration.
//!
//! Every arm is declared before the run: label, solver, model, budget, metered
//! cost and whether memory and evolution are enabled. The harness refuses a
//! matrix whose arms differ in anything except the two capability flags, so no
//! arm can buy extra tasks, time or tokens.

use serde::{Deserialize, Serialize};
use silyn_core::SilynError;

use crate::backend::TaskBudget;

/// Arm identifier. The report keys are exactly `A`, `B` and `C`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ArmId {
    A,
    B,
    C,
}

impl ArmId {
    pub const ALL: [Self; 3] = [Self::A, Self::B, Self::C];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }

    #[must_use]
    pub const fn uses_memory(self) -> bool {
        matches!(self, Self::B | Self::C)
    }

    #[must_use]
    pub const fn uses_evolution(self) -> bool {
        matches!(self, Self::C)
    }

    /// Stable label recorded in the statistics document.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::A => "no-memory-baseline",
            Self::B => "memory-only",
            Self::C => "memory-and-promoted-policy",
        }
    }
}

impl std::fmt::Display for ArmId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for ArmId {
    type Err = SilynError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "a" => Ok(Self::A),
            "b" => Ok(Self::B),
            "c" => Ok(Self::C),
            other => Err(SilynError::Permanent(format!(
                "unknown arm `{other}` (expected a, b, c or all)"
            ))),
        }
    }
}

/// Configuration for one arm. Persisted verbatim in the statistics document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmConfig {
    pub id: ArmId,
    pub label: String,
    /// Solver id, e.g. `baseline.csv-counter` or a W6 candidate id.
    pub solver: String,
    pub model_id: String,
    pub memory: bool,
    pub evolution: bool,
    pub budget: TaskBudget,
    pub cost_usd_per_call: f64,
}

impl ArmConfig {
    /// The W5 reference arm: the deterministic, memory-free baseline. B and C
    /// currently run the same solver and say so through the labels; W6 replaces
    /// them with real memory/policy arms.
    #[must_use]
    pub fn baseline(id: ArmId, model_id: &str, budget: TaskBudget) -> Self {
        Self {
            id,
            label: id.label().to_owned(),
            solver: BASELINE_SOLVER_ID.to_owned(),
            model_id: model_id.to_owned(),
            memory: id.uses_memory(),
            evolution: id.uses_evolution(),
            budget,
            cost_usd_per_call: 0.0,
        }
    }

    /// Rejects configurations that cannot be compared fairly.
    pub fn validate(&self) -> Result<(), SilynError> {
        if self.solver.trim().is_empty() {
            return Err(SilynError::Schema(format!(
                "arm {} has an empty solver id",
                self.id
            )));
        }
        if self.model_id.trim().is_empty() {
            return Err(SilynError::Schema(format!(
                "arm {} has an empty model id",
                self.id
            )));
        }
        if !(self.cost_usd_per_call.is_finite() && self.cost_usd_per_call >= 0.0) {
            return Err(SilynError::Schema(format!(
                "arm {} has an invalid cost per call",
                self.id
            )));
        }
        self.budget.validate()
    }
}

/// Solver id recorded for the W5 baseline.
pub const BASELINE_SOLVER_ID: &str = "baseline.csv-counter.v1";

/// Configuration of a complete A/B/C matrix.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmsConfig {
    pub a: ArmConfig,
    pub b: ArmConfig,
    pub c: ArmConfig,
}

impl ArmsConfig {
    /// The three W5 arms: the deterministic baseline with memory (B) and
    /// evolution (C) flags recorded. W6 supplies real arms.
    #[must_use]
    pub fn baseline(model_id: &str, budget: TaskBudget) -> Self {
        Self {
            a: ArmConfig::baseline(ArmId::A, model_id, budget),
            b: ArmConfig::baseline(ArmId::B, model_id, budget),
            c: ArmConfig::baseline(ArmId::C, model_id, budget),
        }
    }

    #[must_use]
    pub fn get(&self, id: ArmId) -> &ArmConfig {
        match id {
            ArmId::A => &self.a,
            ArmId::B => &self.b,
            ArmId::C => &self.c,
        }
    }

    /// Stable id of the baseline, recorded in the evaluation report.
    #[must_use]
    pub fn baseline_id(&self) -> String {
        format!("{}:{}", self.a.solver, self.a.model_id)
    }

    /// Verifies the fairness contract: identical solver, model, budget and price
    /// for all three arms. Only the arm identity and the capability flags may
    /// differ, so no arm can buy extra tasks, time or tokens.
    pub fn validate_equal_budget(&self) -> Result<(), SilynError> {
        for id in ArmId::ALL {
            let config = self.get(id);
            config.validate()?;
            if config.id != id {
                return Err(SilynError::Schema(format!(
                    "arm entry {id} carries id {}",
                    config.id
                )));
            }
        }
        let reference = &self.a;
        for id in [ArmId::B, ArmId::C] {
            let config = self.get(id);
            if config.solver != reference.solver
                || config.model_id != reference.model_id
                || config.budget != reference.budget
                || config.cost_usd_per_call != reference.cost_usd_per_call
            {
                return Err(SilynError::PolicyDenied(format!(
                    "arm {id} does not share the solver, model, budget and price of arm A"
                )));
            }
        }
        if !self.b.memory || !self.c.evolution || self.a.memory || self.a.evolution {
            return Err(SilynError::PolicyDenied(
                "arm capability flags must be A: none, B: memory, C: memory+evolution".to_owned(),
            ));
        }
        Ok(())
    }
}

/// The three arms of a complete matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct Arms {
    pub a: crate::evaluator::ArmOutcome,
    pub b: crate::evaluator::ArmOutcome,
    pub c: crate::evaluator::ArmOutcome,
}

impl Arms {
    #[must_use]
    pub fn get(&self, id: ArmId) -> &crate::evaluator::ArmOutcome {
        match id {
            ArmId::A => &self.a,
            ArmId::B => &self.b,
            ArmId::C => &self.c,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (ArmId, &crate::evaluator::ArmOutcome)> {
        [
            (ArmId::A, &self.a),
            (ArmId::B, &self.b),
            (ArmId::C, &self.c),
        ]
        .into_iter()
    }
}
