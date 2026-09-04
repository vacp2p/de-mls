//! Time-driven consensus follow-up: firing due auto-votes and pushing
//! expired proposals into the consensus library's timeout resolution.

use hashgraph_like_consensus::{error::ConsensusError, storage::ConsensusStorage};
use tracing::{debug, info, warn};

use crate::{
    ConversationError,
    engine::{handle::Engine, store::EngineStore},
};

impl<St: EngineStore> Engine<St> {
    /// Fire the auto-votes and consensus timeouts that `now` has reached.
    /// Per-proposal errors are logged and skipped so one stuck proposal
    /// can't block the rest.
    pub(crate) fn tick_deadlines(&mut self) {
        let now = self.now;
        let auto_votes_due: Vec<(u32, bool)> = self
            .timing
            .pending_auto_votes
            .iter()
            .filter(|(_, e)| e.fire_at <= now)
            .map(|(id, e)| (*id, e.vote))
            .collect();
        for (id, _) in &auto_votes_due {
            self.timing.pending_auto_votes.remove(id);
        }
        let timeouts_due: Vec<u32> = self
            .timing
            .pending_consensus_timeouts
            .iter()
            .filter(|(_, fire_at)| **fire_at <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in &timeouts_due {
            self.timing.pending_consensus_timeouts.remove(id);
        }

        for (proposal_id, vote) in auto_votes_due {
            if let Err(e) = self.cast_local_vote(proposal_id, vote) {
                match e {
                    ConversationError::Consensus(
                        ConsensusError::UserAlreadyVoted
                        | ConsensusError::SessionNotActive
                        | ConsensusError::SessionNotFound,
                    ) => debug!(
                        proposal_id,
                        error = %e,
                        "auto-vote skipped: already voted or session resolved"
                    ),
                    ConversationError::Consensus(ConsensusError::ProposalExpired) => {
                        warn!(
                            proposal_id,
                            error = %e,
                            "auto-vote dropped: proposal expired before the vote fired; \
                             possible clock skew"
                        );
                        self.report_failure("auto_vote", &e);
                    }
                    _ => {
                        warn!(proposal_id, error = %e, "auto-vote failed");
                        self.report_failure("auto_vote", &e);
                    }
                }
            }
        }
        for proposal_id in timeouts_due {
            self.resolve_on_timeout(proposal_id);
        }
    }

    /// Push a proposal whose deadline elapsed into the consensus library's
    /// timeout resolution. `SessionNotFound`/`SessionNotActive` despite the
    /// `still_active` guard means the session resolved in between — benign
    /// when the proposal is in the resolved cache, a logic bug worth a
    /// warning when it isn't.
    fn resolve_on_timeout(&self, proposal_id: u32) {
        let scope = self.conversation_id.clone();
        let still_active = self
            .consensus
            .storage()
            .get_active_proposals(&scope)
            .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
            .unwrap_or(false);
        if !still_active {
            return;
        }
        match self
            .consensus
            .handle_consensus_timeout(&scope, proposal_id, self.now.as_secs())
        {
            Ok(_) => {}
            Err(ConsensusError::SessionNotFound) | Err(ConsensusError::SessionNotActive) => {
                if self.queues.is_consensus_outcome_applied(proposal_id) {
                    debug!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "timeout fired for already-resolved proposal: ignoring"
                    );
                } else {
                    warn!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "timeout fired for unknown proposal id: no session and not in resolved cache"
                    );
                }
            }
            Err(e) => {
                info!(proposal_id, error = %e, "timeout resolution skipped");
            }
        }
    }
}
