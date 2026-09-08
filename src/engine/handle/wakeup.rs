//! The next-wakeup computation and the auto-vote / consensus-timeout
//! deadline registry `tick_deadlines` (in [`crate::engine::consensus`]) fires
//! against.

use std::time::Duration;

use crate::engine::{
    handle::{AutoVoteEntry, Engine},
    store::EngineStore,
    types::{Phase, Timestamp},
};

impl<St: EngineStore> Engine<St> {
    /// Earliest pending deadline relative to `now`, or `None`.
    pub(crate) fn next_wakeup_in(&self) -> Option<Duration> {
        let earliest = self
            .timing
            .pending_consensus_timeouts
            .values()
            .copied()
            .chain(self.timing.pending_auto_votes.values().map(|e| e.fire_at))
            .chain(self.phase_deadline())
            .chain(self.timing.sync_deadline)
            .chain(
                self.timing
                    .sync_takeover_anchor
                    .map(|anchor| anchor + self.config.backup_takeover_window),
            )
            .min()?;
        Some(earliest.saturating_duration_since(self.now))
    }

    /// When the current phase next needs a tick. `Working` needs one to
    /// open a commit round, so it reports none while an election or an
    /// emergency keeps the round from opening.
    fn phase_deadline(&self) -> Option<Timestamp> {
        let anchor = self.timing.phase_timer.started_at()?;
        match self.phase {
            Phase::Freezing => Some(anchor + self.config.freeze_duration),
            Phase::Working
                if self.queues.approved_proposals_count() > 0
                    && !self.queues.has_election_in_flight()
                    && !self.queues.has_active_emergency() =>
            {
                Some(anchor + self.config.commit_batch_window)
            }
            _ => None,
        }
    }

    /// Register an auto-vote `delay` from now. Re-registering replaces.
    pub(crate) fn register_auto_vote(&mut self, proposal_id: u32, delay: Duration, vote: bool) {
        self.timing.pending_auto_votes.insert(
            proposal_id,
            AutoVoteEntry {
                fire_at: self.now + delay,
                vote,
            },
        );
    }

    pub(crate) fn cancel_auto_vote(&mut self, proposal_id: u32) {
        self.timing.pending_auto_votes.remove(&proposal_id);
    }

    pub(crate) fn cancel_all_auto_votes(&mut self) {
        self.timing.pending_auto_votes.clear();
    }

    /// Register a consensus-session timeout `delay` from now.
    pub(crate) fn register_consensus_timeout(&mut self, proposal_id: u32, delay: Duration) {
        self.timing
            .pending_consensus_timeouts
            .insert(proposal_id, self.now + delay);
    }

    pub(crate) fn unregister_consensus_timeout(&mut self, proposal_id: u32) {
        self.timing.pending_consensus_timeouts.remove(&proposal_id);
    }
}
