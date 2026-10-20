//! Step, token, cost and wall-clock budgets.
//!
//! Budgets are checked by the runtime before each step and after each model
//! response. Exhaustion is a business failure (`RunFailed`), not an error that
//! callers retry.

use std::fmt;
use std::time::{Duration, Instant};

use silyn_core::SilynError;
use silyn_llm::Price;

/// Static limits for one run.
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetSpec {
    pub max_steps: u32,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    /// Requires [`BudgetTracker::new`] to receive a price table.
    pub max_cost_usd: Option<f64>,
    pub max_wall_clock: Duration,
    pub step_timeout: Duration,
}

impl Default for BudgetSpec {
    fn default() -> Self {
        Self {
            max_steps: 16,
            max_input_tokens: 200_000,
            max_output_tokens: 50_000,
            max_cost_usd: None,
            max_wall_clock: Duration::from_secs(10 * 60),
            step_timeout: Duration::from_secs(120),
        }
    }
}

/// The limit that was exceeded.
#[derive(Debug, Clone, PartialEq)]
pub enum BudgetError {
    Steps { limit: u32, attempted: u32 },
    InputTokens { limit: u64, used: u64 },
    OutputTokens { limit: u64, used: u64 },
    Cost { limit: f64, used: f64 },
    WallClock { limit: Duration, elapsed: Duration },
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Steps { limit, attempted } => {
                write!(f, "step budget exceeded: step {attempted} > limit {limit}")
            }
            Self::InputTokens { limit, used } => {
                write!(f, "input token budget exceeded: {used} > {limit}")
            }
            Self::OutputTokens { limit, used } => {
                write!(f, "output token budget exceeded: {used} > {limit}")
            }
            Self::Cost { limit, used } => {
                write!(f, "cost budget exceeded: ${used:.6} > ${limit:.6}")
            }
            Self::WallClock { limit, elapsed } => write!(
                f,
                "wall-clock budget exceeded: {:.3}s > {:.3}s",
                elapsed.as_secs_f64(),
                limit.as_secs_f64()
            ),
        }
    }
}

impl std::error::Error for BudgetError {}

/// A point-in-time view of consumed budget.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
    pub elapsed: Duration,
}

/// Tracks usage against a [`BudgetSpec`].
#[derive(Debug)]
pub struct BudgetTracker {
    spec: BudgetSpec,
    price: Option<Price>,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    started_at: Instant,
}

impl BudgetTracker {
    /// Creates a tracker.
    ///
    /// A cost cap without a price table is a permanent configuration error.
    pub fn new(spec: BudgetSpec, price: Option<Price>) -> Result<Self, SilynError> {
        if spec.max_cost_usd.is_some() && price.is_none() {
            return Err(SilynError::Permanent(
                "a cost budget requires a price table".to_owned(),
            ));
        }
        if let Some(limit) = spec.max_cost_usd
            && (!limit.is_finite() || limit < 0.0)
        {
            return Err(SilynError::Permanent(
                "max_cost_usd must be a finite non-negative number".to_owned(),
            ));
        }
        Ok(Self {
            spec,
            price,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            started_at: Instant::now(),
        })
    }

    /// Checks the 1-based step number about to run plus the accumulated token
    /// and cost totals restored from a checkpoint.
    pub fn check_step(&self, step: u32) -> Result<(), BudgetError> {
        if step > self.spec.max_steps {
            return Err(BudgetError::Steps {
                limit: self.spec.max_steps,
                attempted: step,
            });
        }
        self.check_usage()
    }

    fn check_usage(&self) -> Result<(), BudgetError> {
        if self.input_tokens > self.spec.max_input_tokens {
            return Err(BudgetError::InputTokens {
                limit: self.spec.max_input_tokens,
                used: self.input_tokens,
            });
        }
        if self.output_tokens > self.spec.max_output_tokens {
            return Err(BudgetError::OutputTokens {
                limit: self.spec.max_output_tokens,
                used: self.output_tokens,
            });
        }
        if let Some(limit) = self.spec.max_cost_usd
            && self.cost_usd > limit
        {
            return Err(BudgetError::Cost {
                limit,
                used: self.cost_usd,
            });
        }
        Ok(())
    }

    /// Adds one model response to the totals and checks token and cost limits.
    pub fn record_model(&mut self, input: u64, output: u64) -> Result<(), BudgetError> {
        self.input_tokens = self.input_tokens.saturating_add(input);
        self.output_tokens = self.output_tokens.saturating_add(output);

        if let Some(price) = self.price {
            self.cost_usd += (input as f64 / 1_000_000.0) * price.input_per_million
                + (output as f64 / 1_000_000.0) * price.output_per_million;
        }

        self.check_usage()
    }

    /// Checks elapsed wall-clock time.
    pub fn check_wall(&self) -> Result<(), BudgetError> {
        let elapsed = self.started_at.elapsed();
        if elapsed >= self.spec.max_wall_clock {
            return Err(BudgetError::WallClock {
                limit: self.spec.max_wall_clock,
                elapsed,
            });
        }
        Ok(())
    }

    /// Returns the consumed totals.
    pub fn snapshot(&self) -> BudgetSnapshot {
        BudgetSnapshot {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cost_usd: self.cost_usd,
            elapsed: self.started_at.elapsed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> Price {
        Price {
            input_per_million: 1.0,
            output_per_million: 2.0,
        }
    }

    #[test]
    fn defaults_match_the_documented_limits() {
        let spec = BudgetSpec::default();
        assert_eq!(spec.max_steps, 16);
        assert_eq!(spec.max_input_tokens, 200_000);
        assert_eq!(spec.max_output_tokens, 50_000);
        assert_eq!(spec.max_cost_usd, None);
        assert_eq!(spec.max_wall_clock, Duration::from_secs(600));
        assert_eq!(spec.step_timeout, Duration::from_secs(120));
    }

    #[test]
    fn cost_cap_requires_a_price() {
        let spec = BudgetSpec {
            max_cost_usd: Some(1.0),
            ..BudgetSpec::default()
        };
        let error = BudgetTracker::new(spec, None).expect_err("price is required");
        assert!(matches!(error, SilynError::Permanent(_)));
    }

    #[test]
    fn each_limit_reports_its_own_error() {
        let mut tracker =
            BudgetTracker::new(BudgetSpec::default(), Some(price())).expect("tracker");
        assert!(matches!(
            tracker.check_step(17),
            Err(BudgetError::Steps { .. })
        ));
        assert!(matches!(
            tracker.record_model(10, 10),
            Ok(()) // default limits are generous
        ));

        let mut small = BudgetTracker::new(
            BudgetSpec {
                max_input_tokens: 5,
                max_output_tokens: 5,
                ..BudgetSpec::default()
            },
            None,
        )
        .expect("tracker");
        assert!(matches!(
            small.record_model(6, 0),
            Err(BudgetError::InputTokens { .. })
        ));

        let mut small = BudgetTracker::new(
            BudgetSpec {
                max_output_tokens: 1,
                ..BudgetSpec::default()
            },
            None,
        )
        .expect("tracker");
        assert!(matches!(
            small.record_model(0, 2),
            Err(BudgetError::OutputTokens { .. })
        ));

        let mut cost = BudgetTracker::new(
            BudgetSpec {
                max_cost_usd: Some(0.05),
                ..BudgetSpec::default()
            },
            Some(price()),
        )
        .expect("tracker");
        let error = cost.record_model(100_000, 0).expect_err("cost cap");
        assert!(matches!(error, BudgetError::Cost { .. }));
    }

    #[test]
    fn zero_wall_clock_is_immediately_exhausted() {
        let tracker = BudgetTracker::new(
            BudgetSpec {
                max_wall_clock: Duration::ZERO,
                ..BudgetSpec::default()
            },
            None,
        )
        .expect("tracker");
        assert!(matches!(
            tracker.check_wall(),
            Err(BudgetError::WallClock { .. })
        ));
    }

    #[test]
    fn snapshot_accumulates_usage() {
        let mut tracker =
            BudgetTracker::new(BudgetSpec::default(), Some(price())).expect("tracker");
        tracker.record_model(1_000, 2_000).expect("within budget");
        tracker.record_model(1_000, 2_000).expect("within budget");
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.input_tokens, 2_000);
        assert_eq!(snapshot.output_tokens, 4_000);
        assert!((snapshot.cost_usd - 0.01).abs() < 1e-12);
    }
}
