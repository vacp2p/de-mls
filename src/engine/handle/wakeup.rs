//! The next-wakeup computation and the auto-vote / consensus-timeout
//! deadline registry `tick_deadlines` (in [`crate::engine::voting`]) fires
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
            .chain(self.sync_request_deadline())
            .chain(self.takeover_deadlines())
            .min()?;
        Some(earliest.saturating_duration_since(self.now))
    }

    fn sync_request_deadline(&self) -> Option<Timestamp> {
        self.timing
            .sync_request_anchor
            .map(|anchor| anchor + self.config.backup_takeover_window)
    }

    /// The backup-takeover anchors, so a backup's turn wakes the router.
    fn takeover_deadlines(&self) -> impl Iterator<Item = Timestamp> {
        let window = self.config.backup_takeover_window;
        [
            self.timing.buffered_propose_anchor.map(|a| a + window),
            self.timing.sync_resend_anchor.map(|a| a + window),
        ]
        .into_iter()
        .flatten()
    }

    fn phase_deadline(&self) -> Option<Timestamp> {
        let anchor = self.timing.phase_timer.started_at()?;
        match self.phase {
            Phase::Freezing => Some(anchor + self.config.freeze_duration),
            Phase::Working if self.queues.approved_proposals_count() > 0 => {
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
