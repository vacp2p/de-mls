//! Steward-list housekeeping on the engine: list reconciliation and
//! elections, and the unstick and score-removal emergency proposals.
//!
//! Everything here reads the facts the router last reported — [`Engine::epoch`]
//! and the member set — and writes to the wire through
//! [`Engine::send_control`].

use tracing::info;

use crate::{
    ConversationError, ElectionDecision, ElectionSkip,
    engine::{handle::Engine, store::EngineStore},
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, StewardElectionProposal, ViolationEvidence,
    },
    scoring_member_diff,
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
    // ── membership bookkeeping ─────────────────────────────────────────

    /// Add every reported member not yet tracked in scoring, and drop scored
    /// entries for members that departed. Diffing is delegated to
    /// [`scoring_member_diff`]; this method only applies the diff.
    pub(crate) fn sync_scoring_members(&mut self) -> Result<(), ConversationError> {
        let scored: Vec<Vec<u8>> = self
            .scoring
            .all_members_with_scores()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let diff = scoring_member_diff(&scored, &self.members);
        for member_id in &diff.to_add {
            self.scoring.add_member(member_id);
        }
        for member_id in &diff.to_remove {
            self.scoring.remove_member(member_id);
        }
        // store: scores (WP3g)
        Ok(())
    }

    // ── steward list ───────────────────────────────────────────────────

    /// Reconcile the list after an epoch advance, opening a voted election
    /// when one is needed. Election-init failures are logged, not surfaced —
    /// the conversation may legitimately reject a new proposal right now.
    pub(crate) fn steward_list_housekeeping(&mut self) -> Result<(), ConversationError> {
        let reconcile = self.reconcile_steward_list()?;
        if reconcile == StewardListReconcile::NeedsElection
            && let Err(e) = self.initiate_steward_election()
        {
            info!(conversation = %self.conversation_id, error = %e, "election initiation deferred");
        }
        Ok(())
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
        // store: steward_list (WP3g)
        Ok(StewardListReconcile::Settled)
    }

    /// Checks that a proposed steward-election list matches what we would
    /// build from our own settled members at `election_epoch`, rejecting any
    /// list that is biased or tampered with. The local view is safe to use:
    /// membership doesn't change during an election.
    pub(crate) fn validate_election_list(
        &self,
        election: &StewardElectionProposal,
    ) -> Result<bool, ConversationError> {
        let epoch = election.election_epoch;
        let pool: Vec<Vec<u8>> = self
            .members
            .iter()
            .filter(|m| self.queues.is_settled(m, epoch))
            .cloned()
            .collect();
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
        let candidate_pool: Vec<Vec<u8>> = self
            .members
            .iter()
            .filter(|m| self.queues.is_settled(m, epoch))
            .cloned()
            .collect();

        let (proposed_stewards, election_epoch, retry_round) = {
            let queues = &self.queues;
            let eligible = |c: &[u8]| !queues.has_approved_removal(c);
            let decision =
                self.steward_list
                    .propose_election(epoch, &candidate_pool, &self.own, eligible)?;
            match decision {
                ElectionDecision::Skip(skip) => {
                    match skip {
                        ElectionSkip::NotResponsibleProposer => {
                            let proposer = self.steward_list.responsible_proposer(
                                0,
                                &candidate_pool,
                                eligible,
                            );
                            info!(
                                conversation = %self.conversation_id,
                                proposer = ?proposer,
                                "skipping election: {skip}"
                            );
                        }
                        ElectionSkip::NoEligibleCandidates => {
                            info!(conversation = %self.conversation_id, "skipping election: {skip}");
                        }
                    }
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
