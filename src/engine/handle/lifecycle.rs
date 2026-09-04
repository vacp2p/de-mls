//! State-machine transitions and their phase-timer bookkeeping: entering
//! `Working`, `Freezing`, and `Selection`, and the steward-inactivity check
//! that drives the `Working` → `Freezing` transition.

use std::time::Duration;

use tracing::info;

use crate::engine::{handle::Engine, store::EngineStore, types::Phase};

impl<St: EngineStore> Engine<St> {
    pub(crate) fn start_working(&mut self) -> Phase {
        self.state_machine.start_working();
        self.timing.phase_timer.clear();
        info!(state = "Working", "state transition");
        Phase::Working
    }

    /// Enter `Freezing` from `Working`, anchoring the
    /// freeze timer. `None` from any other state.
    pub(crate) fn start_freezing(&mut self) -> Option<Phase> {
        if self.state_machine.start_freezing() {
            self.timing.phase_timer.start(self.now);
            info!(state = "Freezing", "state transition");
            Some(Phase::Freezing)
        } else {
            None
        }
    }

    pub(crate) fn start_selection(&mut self) -> Phase {
        self.state_machine.start_selection();
        info!(state = "Selection", "state transition");
        Phase::Selection
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub(crate) fn is_freeze_window_elapsed(&self) -> bool {
        self.current_state() == Phase::Freezing
            && self
                .timing
                .phase_timer
                .elapsed_since_anchor(self.now, self.config.freeze_duration)
    }

    /// The "steward waited too long" transition into `Freezing`. `Some`
    /// exactly on the call that transitions. Anchors itself on the first
    /// call with approved work.
    pub(crate) fn check_steward_inactivity(
        &mut self,
        approved_proposals_count: usize,
        inactivity_duration: Duration,
    ) -> Option<Phase> {
        if self.current_state() != Phase::Working || approved_proposals_count == 0 {
            return None;
        }
        if self.timing.phase_timer.started_at().is_none() {
            self.timing.phase_timer.start(self.now);
            info!(
                approved = approved_proposals_count,
                inactivity_ms = inactivity_duration.as_millis() as u64,
                "inactivity timer started"
            );
            return None;
        }
        if !self
            .timing
            .phase_timer
            .elapsed_since_anchor(self.now, inactivity_duration)
        {
            return None;
        }
        info!(
            inactivity_ms = inactivity_duration.as_millis() as u64,
            approved = approved_proposals_count,
            "inactivity window elapsed, entering freeze"
        );
        self.start_freezing()
    }
}
