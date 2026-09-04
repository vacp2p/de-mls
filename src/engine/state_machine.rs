//! Per-conversation state machine.

use crate::engine::types::Phase;

#[derive(Debug, Clone)]
pub struct ConversationStateMachine {
    state: Phase,
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
            state: Phase::Working,
        }
    }

    pub fn current_state(&self) -> Phase {
        self.state
    }

    pub fn start_working(&mut self) {
        self.state = Phase::Working;
    }

    /// Transition to `Freezing`, allowed only from `Working`. Returns `true`
    /// on transition; `false` is a no-op.
    pub fn start_freezing(&mut self) -> bool {
        match self.state {
            Phase::Working => {
                self.state = Phase::Freezing;
                true
            }
            _ => false,
        }
    }

    pub fn start_selection(&mut self) {
        self.state = Phase::Selection;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_as_member_starts_working() {
        let sm = ConversationStateMachine::new_as_member();
        assert_eq!(sm.current_state(), Phase::Working);
    }

    #[test]
    fn named_transitions_set_state() {
        let mut sm = ConversationStateMachine::new_as_member();
        assert!(sm.start_freezing());
        assert_eq!(sm.current_state(), Phase::Freezing);
        sm.start_selection();
        assert_eq!(sm.current_state(), Phase::Selection);
        sm.start_working();
        assert_eq!(sm.current_state(), Phase::Working);
    }

    #[test]
    fn start_freezing_from_working_transitions() {
        let mut sm = ConversationStateMachine::new_as_member();
        assert!(sm.start_freezing());
        assert_eq!(sm.current_state(), Phase::Freezing);
    }

    /// `start_freezing` is a no-op outside `Working`.
    #[test]
    fn start_freezing_noop_outside_working() {
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
