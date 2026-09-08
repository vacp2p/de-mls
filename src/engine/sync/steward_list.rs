//! Steward-list housekeeping on the engine: list reconciliation and
//! elections, and the unstick and score-removal emergency proposals.
//!
//! Everything here reads the facts the router last reported — [`Engine::epoch`]
//! and the member set — and writes to the wire through
//! [`Engine::send_control`].

use tracing::info;

use crate::{
    ConversationError, ElectionDecision,
    engine::{handle::Engine, store::EngineStore, types::Phase},
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, ViolationEvidence,
    },
};

/// Outcome of reconciling the steward list to the current epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StewardListReconcile {
    /// The list covers the epoch, or the settled members were installed
    /// locally and deterministically.
    Settled,
    /// The settled members exceed `sn_max`; the caller runs the election.
    NeedsElection,
}

impl<St: EngineStore> Engine<St> {
    /// Reconcile the list to the current epoch and set the phase from the
    /// result: `Working` when a list covers the epoch, `Syncing` when the
    /// settled members outgrew it and an election is required — filed here
    /// when this node is the responsible proposer. Runs after every merge
    /// and at a restart. A reconcile failure is reported and leaves the
    /// list as it was; an election-init failure is logged, since the
    /// proposal may legitimately be refused right now.
    pub(crate) fn reconcile_list_and_phase(&mut self) {
        let reconcile = match self.reconcile_steward_list() {
            Ok(reconcile) => reconcile,
            Err(e) => {
                self.report_failure("reconcile_steward_list", &e);
                StewardListReconcile::Settled
            }
        };
        let phase = match reconcile {
            StewardListReconcile::Settled => self.start_working(),
            StewardListReconcile::NeedsElection => self.start_syncing(),
        };
        self.emit_phase(Some(phase));
        if phase == Phase::Syncing
            && let Err(e) = self.initiate_steward_election()
        {
            info!(conversation = %self.conversation_id, error = %e, "election initiation deferred");
        }
    }

    /// Reconcile the steward list to the current epoch. No-op while the list
    /// still covers it. Otherwise the settled-member count decides: `<= sn_max`
    /// installs them locally (deterministic, no vote); `> sn_max` returns
    /// [`StewardListReconcile::NeedsElection`]. Every node computes the same
    /// local list, so they agree without a round.
    pub(crate) fn reconcile_steward_list(
        &mut self,
    ) -> Result<StewardListReconcile, ConversationError> {
        let current_epoch = self.epoch;
        if !self.steward_list.is_exhausted(current_epoch) {
            return Ok(StewardListReconcile::Settled);
        }
        // Stewards are settled members only — a just-joined member can't commit
        // or vote yet, so it's excluded until the next epoch.
        let settled = self.queues.settled_members(&self.members, current_epoch);
        if self.steward_list.election_required(settled.len()) {
            return Ok(StewardListReconcile::NeedsElection);
        }
        let sn = self.steward_list.config().compute_list_size(settled.len());
        self.steward_list
            .install_list(current_epoch, &settled, sn, 0)?;
        self.dirty.steward_list = true;
        Ok(StewardListReconcile::Settled)
    }

    /// The members an election at `epoch` may seat: settled, and not already
    /// voted out. Excluding the departing member is what lets an election
    /// replace a list whose only steward is leaving — the same epoch over the
    /// same pool would regenerate the same list.
    fn election_pool(&self, epoch: u64) -> Vec<Vec<u8>> {
        self.members
            .iter()
            .filter(|m| self.queues.is_settled(m, epoch) && !self.queues.has_approved_removal(m))
            .cloned()
            .collect()
    }

    /// Checks that a proposed steward-election list matches what we would
    /// build from our own election pool at `election_epoch`, rejecting any
    /// list that is biased or tampered with. The local view is safe to use:
    /// membership doesn't change during an election.
    pub(crate) fn validate_election_list(
        &self,
        election: &StewardElectionProposal,
    ) -> Result<bool, ConversationError> {
        let epoch = election.election_epoch;
        let pool = self.election_pool(epoch);
        self.steward_list.validate_proposed(
            &election.proposed_stewards,
            epoch,
            &pool,
            election.retry_round,
        )
    }

    /// Submit a steward-election proposal. Only the deterministic responsible
    /// proposer actually submits; others no-op, so this is safe to call on
    /// every tick without double-proposing. Runs whether the list is
    /// exhausted or not: the natural-exhaustion caller already checked that,
    /// and an application-requested election
    /// ([`Engine::propose_election`]) needs to run when every remaining
    /// steward has been skipped rather than the list being exhausted.
    pub(crate) fn initiate_steward_election(&mut self) -> Result<(), ConversationError> {
        // `has_election_in_flight` is a proposal-queue check, not a
        // steward-list one — gated here, before the list call.
        if self.queues.has_election_in_flight() {
            return Ok(());
        }
        let epoch = self.epoch;
        let candidate_pool = self.election_pool(epoch);

        let (proposed_stewards, election_epoch, retry_round) = {
            let queues = &self.queues;
            let eligible = |c: &[u8]| !queues.has_approved_removal(c);
            let decision =
                self.steward_list
                    .propose_election(epoch, &candidate_pool, &self.own, eligible)?;
            match decision {
                ElectionDecision::Skip(skip) => {
                    info!(conversation = %self.conversation_id, "skipping election: {skip}");
                    return Ok(());
                }
                ElectionDecision::Proposed {
                    proposed_stewards,
                    election_epoch,
                    retry_round,
                } => (proposed_stewards, election_epoch, retry_round),
            }
        };

        let stewards = proposed_stewards.len();
        let request = ConversationUpdateRequest::steward_election(StewardElectionProposal {
            proposed_stewards,
            election_epoch,
            retry_round,
        });
        info!(
            conversation = %self.conversation_id,
            epoch = election_epoch,
            retry_round,
            stewards,
            "initiating steward election"
        );
        self.initiate_proposal(request)
    }

    // ── unstick and score-removal proposals ─────────────────────────────

    /// File the `Deadlock` emergency proposal: on ⌈2n/3⌉ YES the current
    /// epoch steward is skipped for this epoch and the next eligible steward
    /// on the list becomes ES. Any member may file it, any time approved
    /// work is waiting for a commit — it only *proposes*, so one member can't
    /// force the outcome.
    pub(crate) fn file_deadlock_ecp(&mut self) -> Result<(), ConversationError> {
        if self.queues.approved_proposals_count() == 0 {
            return Err(ConversationError::NoProposals);
        }
        let request = ViolationEvidence::deadlock(self.epoch)
            .with_creator(self.own.clone())
            .into_update_request()?;
        info!(conversation = %self.conversation_id, epoch = self.epoch, "initiating Deadlock ECP");
        // Bundled YES: filing it is this member's own vote that the deadlock
        // is real.
        self.initiate_proposal(request)
    }

    /// File the emergency proposal that removes `target` on the grounds of
    /// its peer score. Fails when `target` has no score here.
    pub(crate) fn file_score_removal_ecp(
        &mut self,
        target: &[u8],
    ) -> Result<(), ConversationError> {
        let Some(score) = self.scoring.score_for(target) else {
            return Err(ConversationError::InvalidConversationUpdateRequest);
        };
        let request = ViolationEvidence::score_below_threshold(target.to_vec(), self.epoch, score)
            .with_creator(self.own.clone())
            .into_update_request()?;
        info!(
            conversation = %self.conversation_id,
            target = ?target,
            score,
            epoch = self.epoch,
            "initiating SCORE_BELOW_THRESHOLD ECP"
        );
        // Bundled YES: proposing it is this member's own vote for the removal.
        self.initiate_proposal(request)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        protos::de_mls::messages::v1::StewardElectionProposal,
        test_support::{creator, member},
    };

    /// The honest list recomputes from the local members
    #[test]
    fn validate_election_list_recomputes_from_local_members() {
        let engine = creator();
        let honest = StewardElectionProposal {
            proposed_stewards: vec![engine.own.clone()],
            election_epoch: engine.epoch,
            retry_round: 0,
        };
        assert!(
            engine.validate_election_list(&honest).unwrap(),
            "the honest list matches the local members"
        );
        let forged = StewardElectionProposal {
            proposed_stewards: vec![member("nobody")],
            election_epoch: engine.epoch,
            retry_round: 0,
        };
        assert!(
            !engine.validate_election_list(&forged).unwrap(),
            "a list off the local members is rejected"
        );
    }
}
