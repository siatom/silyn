use silyn_core::state::{CandidateState, LifecycleState, RunStatus, TaskStatus};

#[test]
fn task_happy_path_is_reachable() {
    let path = [
        TaskStatus::Created,
        TaskStatus::Ready,
        TaskStatus::Running,
        TaskStatus::WaitingTool,
        TaskStatus::Running,
        TaskStatus::Verifying,
        TaskStatus::Completed,
    ];
    for pair in path.windows(2) {
        assert!(
            pair[0].can_transition_to(pair[1]),
            "{:?} -> {:?} must be legal",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn terminal_states_are_final() {
    for state in [
        TaskStatus::Completed,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ] {
        assert!(state.is_terminal());
        assert!(!state.can_transition_to(TaskStatus::Running));
    }
    for state in [
        RunStatus::Completed,
        RunStatus::Failed,
        RunStatus::Cancelled,
    ] {
        assert!(state.is_terminal());
        assert!(!state.can_transition_to(RunStatus::Running));
    }
}

#[test]
fn run_cannot_skip_verification() {
    assert!(!RunStatus::Running.can_transition_to(RunStatus::Completed));
    assert!(RunStatus::Running.can_transition_to(RunStatus::Verifying));
}

#[test]
fn candidate_cannot_promote_without_evaluation() {
    assert!(!CandidateState::Proposed.can_transition_to(CandidateState::Promoted));
    assert!(!CandidateState::Experimenting.can_transition_to(CandidateState::Promoted));
    assert!(CandidateState::Evaluated.can_transition_to(CandidateState::Approved));
}
