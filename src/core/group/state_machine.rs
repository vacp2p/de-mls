//! Per-group state machine.
//!
//! Holds the [`GroupState`] enum and exposes named transition methods.
//! The app layer wraps this with a timer-driven controller — see
//! [`crate::app::PhaseTimer`].

use std::fmt::Display;

/// The lifecycle state of a per-group session. Transitions are driven
/// by the app layer through the named methods on [`GroupStateMachine`];
/// timing rules live in [`crate::app::PhaseTimer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupState {
    /// Joiner waiting for a welcome.
    PendingJoin,
    /// Normal operation: members vote, the steward batches and commits.
    Working,
    /// Members have stopped accepting new proposals; commit candidates
    /// are buffered for deterministic selection.
    Freezing,
    /// Selection phase: the freeze-round candidate has been picked and
    /// is being merged.
    Selection,
    /// Recovery: a steward election is in flight after a missed commit.
    Reelection,
}

/// Authorization mode for a group, orthogonal to [`GroupState`].
///
/// `Normal` is the default: only steward-list members may produce
/// commits. `Recovery` is set when an accepted Layer-3 Deadlock ECP
/// relaxes the steward gate so any member may produce the next commit
/// (RFC §Anti-Deadlock); cleared when a fresh election lands. Lives
/// alongside `GroupState` because it gates *who can act*, not *what
/// phase the round is in*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OperatingMode {
    #[default]
    Normal,
    Recovery,
}

impl Display for GroupState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupState::PendingJoin => write!(f, "PendingJoin"),
            GroupState::Working => write!(f, "Working"),
            GroupState::Freezing => write!(f, "Freezing"),
            GroupState::Selection => write!(f, "Selection"),
            GroupState::Reelection => write!(f, "Reelection"),
        }
    }
}

/// State enum + named transitions. The app layer wraps this with
/// timer-driven behaviour through [`crate::app::PhaseTimer`].
#[derive(Debug, Clone)]
pub struct GroupStateMachine {
    state: GroupState,
}

impl Default for GroupStateMachine {
    fn default() -> Self {
        Self::new_as_member()
    }
}

impl GroupStateMachine {
    /// Member starts in `Working` (creator path, or post-join).
    pub fn new_as_member() -> Self {
        Self {
            state: GroupState::Working,
        }
    }

    /// Joiner starts in `PendingJoin` until the welcome arrives.
    pub fn new_as_pending_join() -> Self {
        Self {
            state: GroupState::PendingJoin,
        }
    }

    pub fn current_state(&self) -> GroupState {
        self.state
    }

    pub fn start_working(&mut self) {
        self.state = GroupState::Working;
    }

    pub fn start_freezing(&mut self) {
        self.state = GroupState::Freezing;
    }

    /// Transition to `Freezing` only from `Working` or `Reelection`
    /// (RFC: bypass the inactivity timer for ECP-driven freezes).
    /// Returns `true` on actual transition; `false` is a no-op.
    pub fn force_freezing(&mut self) -> bool {
        match self.state {
            GroupState::Working | GroupState::Reelection => {
                self.state = GroupState::Freezing;
                true
            }
            _ => false,
        }
    }

    pub fn start_selection(&mut self) {
        self.state = GroupState::Selection;
    }

    pub fn start_reelection(&mut self) {
        self.state = GroupState::Reelection;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_as_member_starts_working() {
        let sm = GroupStateMachine::new_as_member();
        assert_eq!(sm.current_state(), GroupState::Working);
    }

    #[test]
    fn new_as_pending_join_starts_pending() {
        let sm = GroupStateMachine::new_as_pending_join();
        assert_eq!(sm.current_state(), GroupState::PendingJoin);
    }

    #[test]
    fn named_transitions_set_state() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.start_freezing();
        assert_eq!(sm.current_state(), GroupState::Freezing);
        sm.start_selection();
        assert_eq!(sm.current_state(), GroupState::Selection);
        sm.start_reelection();
        assert_eq!(sm.current_state(), GroupState::Reelection);
        sm.start_working();
        assert_eq!(sm.current_state(), GroupState::Working);
    }

    #[test]
    fn force_freezing_from_working_transitions() {
        let mut sm = GroupStateMachine::new_as_member();
        assert!(sm.force_freezing());
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    #[test]
    fn force_freezing_from_reelection_transitions() {
        let mut sm = GroupStateMachine::new_as_member();
        sm.start_reelection();
        assert!(sm.force_freezing());
        assert_eq!(sm.current_state(), GroupState::Freezing);
    }

    /// `force_freezing` is a no-op outside `Working`/`Reelection`.
    #[test]
    fn force_freezing_noop_outside_working_reelection() {
        for setup in [
            |sm: &mut GroupStateMachine| sm.start_freezing(),
            |sm: &mut GroupStateMachine| sm.start_selection(),
        ] {
            let mut sm = GroupStateMachine::new_as_member();
            setup(&mut sm);
            let before = sm.current_state();
            assert!(!sm.force_freezing());
            assert_eq!(sm.current_state(), before);
        }
    }
}
