//! Post-run experience extraction.
//!
//! The app layer is the only place that knows both the agent and the terminal
//! [`RunOutcome`], so it maps the outcome onto the deterministic
//! [`ExperienceStore`] contract:
//!
//! - `completed` + `success` writes one provisional positive lesson;
//! - `completed` + `failed` and `failed` write one remediation lesson keyed by
//!   the failure code;
//! - `unknown`, missing outcomes and `cancelled` write nothing.
//!
//! Missing evidence never becomes a remembered success. A store error is
//! returned to the caller: a run that cannot record its lesson must not look
//! fully successful to an operator.

use silyn_core::SilynError;
use silyn_core::event::VerifiedOutcome;
use silyn_core::ids::AgentId;
use silyn_core::state::RunStatus;
use silyn_growth::{CheckResult, ExperienceRecord, ExperienceStore};
use silyn_runtime::RunOutcome;

use crate::app::App;

impl App {
    /// Records at most one experience for a terminal run.
    ///
    /// Returns `Ok(None)` for `Unknown`, `Cancelled` and non-terminal outcomes.
    /// Any storage failure is propagated, never swallowed.
    pub async fn record_experience(
        &self,
        agent_id: AgentId,
        outcome: &RunOutcome,
    ) -> Result<Option<ExperienceRecord>, SilynError> {
        let Some(result) = result_for(outcome) else {
            return Ok(None);
        };
        ExperienceStore::new(self.storage().pool())
            .extract(agent_id, outcome.run_id, &result)
            .await
    }
}

/// Maps one terminal outcome onto a checker result, or `None` when no lesson is
/// justified.
fn result_for(outcome: &RunOutcome) -> Option<CheckResult> {
    match outcome.status {
        RunStatus::Completed => match outcome.outcome {
            Some(VerifiedOutcome::Success) => Some(CheckResult::Success {
                evidence: Vec::new(),
            }),
            Some(VerifiedOutcome::Failed) => Some(failure_result(outcome)),
            // A completed run with no evidence is still `Unknown`, and
            // `Unknown` must never be remembered as success.
            Some(VerifiedOutcome::Unknown) | None => None,
        },
        RunStatus::Failed => Some(failure_result(outcome)),
        RunStatus::Cancelled => None,
        // Non-terminal states are not expected here; treating them as
        // "nothing to learn yet" is the conservative choice.
        _ => None,
    }
}

fn failure_result(outcome: &RunOutcome) -> CheckResult {
    let reason = outcome
        .reason
        .clone()
        .unwrap_or_else(|| "verification failed without a reason".to_owned());
    let code = failure_code(&reason);
    CheckResult::Failed {
        code: code.to_owned(),
        reason,
        evidence: vec![format!("run:{}", outcome.run_id)],
    }
}

/// Maps a free-form failure reason onto the fixed lesson table.
///
/// The runtime records human-readable reasons, while experiences are keyed by
/// stable codes. Anything unrecognized becomes `other`, which the growth crate
/// renders as the generic lesson.
#[must_use]
pub fn failure_code(reason: &str) -> &'static str {
    let lower = reason.to_ascii_lowercase();
    if lower.contains("unit") {
        "unit_mismatch"
    } else if lower.contains("missing") {
        "missing_values"
    } else if lower.contains("duplicate") {
        "duplicate_rows"
    } else if lower.contains("count") {
        "wrong_count"
    } else if lower.contains("tool") {
        "tool_error"
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use silyn_core::ids::RunId;

    use super::*;

    fn outcome(
        status: RunStatus,
        outcome: Option<VerifiedOutcome>,
        reason: Option<&str>,
    ) -> RunOutcome {
        RunOutcome {
            run_id: RunId::new(),
            status,
            outcome,
            final_answer: None,
            reason: reason.map(str::to_owned),
            steps: 1,
            input_tokens: 0,
            output_tokens: 0,
            cost_usd: 0.0,
            memory_refs: Vec::new(),
        }
    }

    #[test]
    fn unknown_has_no_lesson() {
        assert!(
            result_for(&outcome(
                RunStatus::Completed,
                Some(VerifiedOutcome::Unknown),
                None
            ))
            .is_none()
        );
        assert!(result_for(&outcome(RunStatus::Completed, None, None)).is_none());
    }

    #[test]
    fn cancelled_has_no_lesson() {
        assert!(result_for(&outcome(RunStatus::Cancelled, None, Some("cancelled"))).is_none());
    }

    #[test]
    fn success_and_failure_map_to_results() {
        assert!(matches!(
            result_for(&outcome(
                RunStatus::Completed,
                Some(VerifiedOutcome::Success),
                None
            )),
            Some(CheckResult::Success { .. })
        ));
        assert!(matches!(
            result_for(&outcome(
                RunStatus::Completed,
                Some(VerifiedOutcome::Failed),
                None
            )),
            Some(CheckResult::Failed { .. })
        ));
        assert!(matches!(
            result_for(&outcome(
                RunStatus::Failed,
                None,
                Some("tool call is indeterminate")
            )),
            Some(CheckResult::Failed { .. })
        ));
    }

    #[test]
    fn failure_codes_are_stable() {
        assert_eq!(
            failure_code("unit mismatch: expected cents"),
            "unit_mismatch"
        );
        assert_eq!(failure_code("missing values in column b"), "missing_values");
        assert_eq!(failure_code("duplicate rows detected"), "duplicate_rows");
        assert_eq!(failure_code("wrong count: 3 != 4"), "wrong_count");
        assert_eq!(failure_code("tool `fs.read` failed"), "tool_error");
        assert_eq!(failure_code("budget exhausted"), "other");
        assert_eq!(failure_code("artifact differs at $.answer"), "other");
    }
}
