//! Lifecycle state machines.
//!
//! Transitions are centralized here so runtime, storage and evaluation cannot
//! invent their own orderings. Any state not reachable through these tables is
//! rejected by the runtime.

use serde::{Deserialize, Serialize};

/// A state that can only move along an explicit transition table.
pub trait LifecycleState: Copy + Eq {
    /// Returns true when `self -> next` is a legal transition.
    fn can_transition_to(self, next: Self) -> bool;

    /// Returns true when no further transition is allowed.
    fn is_terminal(self) -> bool;
}

/// Task lifecycle, owned by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Ready,
    Running,
    WaitingApproval,
    WaitingTool,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

impl LifecycleState for TaskStatus {
    fn can_transition_to(self, next: Self) -> bool {
        use TaskStatus::{
            Cancelled, Completed, Created, Failed, Ready, Running, Verifying, WaitingApproval,
            WaitingTool,
        };
        match self {
            Created => matches!(next, Ready | Cancelled),
            Ready => matches!(next, Running | Cancelled),
            Running => matches!(
                next,
                WaitingApproval | WaitingTool | Verifying | Failed | Cancelled
            ),
            WaitingApproval | WaitingTool => matches!(next, Running | Failed | Cancelled),
            Verifying => matches!(next, Completed | Failed | Cancelled),
            Completed | Failed | Cancelled => false,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Run lifecycle. A run is one attempt of a task and may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Created,
    Running,
    WaitingApproval,
    WaitingTool,
    Verifying,
    Completed,
    Failed,
    Cancelled,
}

impl LifecycleState for RunStatus {
    fn can_transition_to(self, next: Self) -> bool {
        use RunStatus::{
            Cancelled, Completed, Created, Failed, Running, Verifying, WaitingApproval, WaitingTool,
        };
        match self {
            Created => matches!(next, Running | Cancelled),
            Running => matches!(
                next,
                WaitingApproval | WaitingTool | Verifying | Failed | Cancelled
            ),
            WaitingApproval | WaitingTool => matches!(next, Running | Failed | Cancelled),
            Verifying => matches!(next, Completed | Failed | Cancelled),
            Completed | Failed | Cancelled => false,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Evolution candidate lifecycle (ADR 0003 / ADR 0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    Proposed,
    StaticChecked,
    Experimenting,
    Evaluated,
    Rejected,
    AwaitingApproval,
    Approved,
    Canary,
    Promoted,
    RolledBack,
    Retired,
}

impl LifecycleState for CandidateState {
    fn can_transition_to(self, next: Self) -> bool {
        use CandidateState::{
            Approved, AwaitingApproval, Canary, Evaluated, Experimenting, Promoted, Proposed,
            Rejected, Retired, RolledBack, StaticChecked,
        };
        match self {
            Proposed => matches!(next, StaticChecked | Rejected),
            StaticChecked => matches!(next, Experimenting | Rejected),
            Experimenting => matches!(next, Evaluated | Rejected),
            Evaluated => matches!(next, AwaitingApproval | Approved | Rejected),
            AwaitingApproval => matches!(next, Approved | Rejected),
            Approved => matches!(next, Canary | Promoted),
            Canary => matches!(next, Promoted | RolledBack),
            Promoted => matches!(next, RolledBack | Retired),
            RolledBack => matches!(next, Retired),
            Rejected | Retired => false,
        }
    }

    fn is_terminal(self) -> bool {
        matches!(self, Self::Rejected | Self::Retired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_final() {
        for state in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            assert!(state.is_terminal());
            for next in [
                TaskStatus::Ready,
                TaskStatus::Running,
                TaskStatus::Verifying,
                TaskStatus::Completed,
            ] {
                assert!(!state.can_transition_to(next));
            }
        }
    }

    #[test]
    fn candidate_cannot_skip_static_check() {
        assert!(!CandidateState::Proposed.can_transition_to(CandidateState::Experimenting));
        assert!(CandidateState::Proposed.can_transition_to(CandidateState::StaticChecked));
    }
}
