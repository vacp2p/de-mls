//! Per-conversation state machine.

use std::fmt::Display;

/// Lifecycle state for a conversation session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationState {
    /// Joiner waiting for a welcome.
    PendingJoin,
    /// Normal operation.
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

/// Authorization mode for a conversation.
///
/// - `Normal` is the default: only steward-list members may produce
///   commits.
/// - `Recovery` is set when an accepted Layer-3 Deadlock ECP relaxes the
///   steward gate so any member may produce the next commit
///   (RFC §Anti-Deadlock); cleared when a fresh election lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OperatingMode {
    #[default]
    Normal,
    Recovery,
}

impl Display for ConversationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConversationState::PendingJoin => write!(f, "PendingJoin"),
            ConversationState::Working => write!(f, "Working"),
            ConversationState::Freezing => write!(f, "Freezing"),
            ConversationState::Selection => write!(f, "Selection"),
            ConversationState::Reelection => write!(f, "Reelection"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConversationStateMachine {
    state: ConversationState,
}

impl Default for ConversationStateMachine {
    fn default() -> Self {
        Self::new_as_member()
    }
}

impl ConversationStateMachine {
    /// Member starts in `Working` (creator path, or post-join).
    pub fn new_as_member() -> Self {
        Self {
            state: ConversationState::Working,
        }
    }

    /// Joiner starts in `PendingJoin` until the welcome arrives.
    pub fn new_as_pending_join() -> Self {
        Self {
            state: ConversationState::PendingJoin,
        }
    }

    pub fn current_state(&self) -> ConversationState {
        self.state
    }

    pub fn start_working(&mut self) {
        self.state = ConversationState::Working;
    }

    /// Transition to `Freezing`, allowed only from `Working` or
    /// `Reelection`. Returns `true` on transition; `false` is a no-op.
    pub fn start_freezing(&mut self) -> bool {
        match self.state {
            ConversationState::Working | ConversationState::Reelection => {
                self.state = ConversationState::Freezing;
                true
            }
            _ => false,
        }
    }

    pub fn start_selection(&mut self) {
        self.state = ConversationState::Selection;
    }

    pub fn start_reelection(&mut self) {
        self.state = ConversationState::Reelection;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_as_member_starts_working() {
        let sm = ConversationStateMachine::new_as_member();
        assert_eq!(sm.current_state(), ConversationState::Working);
    }

    #[test]
    fn new_as_pending_join_starts_pending() {
        let sm = ConversationStateMachine::new_as_pending_join();
        assert_eq!(sm.current_state(), ConversationState::PendingJoin);
    }

    #[test]
    fn named_transitions_set_state() {
        let mut sm = ConversationStateMachine::new_as_member();
        assert!(sm.start_freezing());
        assert_eq!(sm.current_state(), ConversationState::Freezing);
        sm.start_selection();
        assert_eq!(sm.current_state(), ConversationState::Selection);
        sm.start_reelection();
        assert_eq!(sm.current_state(), ConversationState::Reelection);
        sm.start_working();
        assert_eq!(sm.current_state(), ConversationState::Working);
    }

    #[test]
    fn start_freezing_from_working_transitions() {
        let mut sm = ConversationStateMachine::new_as_member();
        assert!(sm.start_freezing());
        assert_eq!(sm.current_state(), ConversationState::Freezing);
    }

    #[test]
    fn start_freezing_from_reelection_transitions() {
        let mut sm = ConversationStateMachine::new_as_member();
        sm.start_reelection();
        assert!(sm.start_freezing());
        assert_eq!(sm.current_state(), ConversationState::Freezing);
    }

    /// `start_freezing` is a no-op outside `Working`/`Reelection`.
    #[test]
    fn start_freezing_noop_outside_working_reelection() {
        for setup in [
            |sm: &mut ConversationStateMachine| {
                sm.start_freezing();
            },
            |sm: &mut ConversationStateMachine| sm.start_selection(),
        ] {
            let mut sm = ConversationStateMachine::new_as_member();
            setup(&mut sm);
            let before = sm.current_state();
            assert!(!sm.start_freezing());
            assert_eq!(sm.current_state(), before);
        }
    }
}
