//! Phase transitions and their phase-timer bookkeeping: entering
//! `Working`, `Freezing`, `Selection` and `Syncing`, leaving `Syncing`, and
//! the steward-inactivity check that drives the `Working` → `Freezing`
//! transition. Every transition sets the phase here and returns it; the
//! caller reports it through `emit_phase` at once, so the router sees one
//! `PhaseChange` per transition, in order.

use std::time::Duration;

use tracing::info;

use crate::engine::{handle::Engine, store::EngineStore, types::Phase};

impl<St: EngineStore> Engine<St> {
    pub(crate) fn start_working(&mut self) -> Phase {
        self.phase = Phase::Working;
        self.timing.phase_timer.clear();
        info!(state = "Working", "state transition");
        Phase::Working
    }

    /// Enter `Freezing` from `Working`, anchoring the freeze timer. `None`
    /// from any other phase.
    pub(crate) fn start_freezing(&mut self) -> Option<Phase> {
        if self.phase == Phase::Working {
            self.phase = Phase::Freezing;
            self.timing.phase_timer.start(self.now);
            info!(state = "Freezing", "state transition");
            Some(Phase::Freezing)
        } else {
            None
        }
    }

    pub(crate) fn start_selection(&mut self) -> Phase {
        self.phase = Phase::Selection;
        info!(state = "Selection", "state transition");
        Phase::Selection
    }

    /// Enter `Syncing`: no list covers this epoch. The inactivity clock
    /// stops; the router is told through the returned phase.
    pub(crate) fn start_syncing(&mut self) -> Phase {
        self.phase = Phase::Syncing;
        self.timing.phase_timer.clear();
        info!(state = "Syncing", "state transition");
        Phase::Syncing
    }

    /// Leave `Syncing` for `Working`: a sync was adopted, or an election
    /// resolved it. `None` from any other phase.
    pub(crate) fn leave_syncing(&mut self) -> Option<Phase> {
        if self.phase == Phase::Syncing {
            self.timing.sync_deadline = None;
            Some(self.start_working())
        } else {
            None
        }
    }

    /// `true` once the freeze window elapsed while in `Freezing`.
    pub(crate) fn is_freeze_window_elapsed(&self) -> bool {
        self.phase == Phase::Freezing
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
        if self.phase != Phase::Working || approved_proposals_count == 0 {
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
