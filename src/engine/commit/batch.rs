//! Minting our own candidate: batching the approved queue into commit
//! actions and asking the router to build the commit, and opening the round
//! when a peer's candidate arrives first.

use tracing::info;

use crate::{
    ConversationError,
    engine::{
        handle::Engine,
        proposal_kind::ProposalKind,
        store::EngineStore,
        types::{Action, Decision, MemberId, Phase},
    },
    protos::de_mls::messages::v1::conversation_update_request::Payload,
};

impl<St: EngineStore> Engine<St> {
    /// A peer's candidate opens the round locally: from `Working` we enter
    /// `Freezing` and, if we are the epoch steward, ask for our own.
    pub(crate) fn open_round_on_peer_candidate(&mut self) -> Result<(), ConversationError> {
        if self.phase != Phase::Working {
            return Ok(());
        }
        let Some(event) = self.start_freezing() else {
            return Ok(());
        };
        self.on_freeze_entered(event)
    }

    /// `(received, expected)` candidates for the current round: at most the
    /// epoch steward's alone.
    pub(crate) fn commit_candidate_count(&self) -> (usize, usize) {
        (usize::from(self.round_candidate.is_some()), 1)
    }

    /// Ask the router to build our candidate from the approved batch.
    /// Only the epoch steward builds:
    /// `Ok(false)` when we aren't it or there is nothing to commit.
    pub(crate) fn request_own_candidate(&mut self) -> Result<bool, ConversationError> {
        if !self.is_epoch_steward() {
            return Ok(false);
        }
        if self.queues.approved_proposals().is_empty() {
            return Err(ConversationError::NoProposals);
        }
        // Governance proposals are consensus-only and never reach a commit.
        let governance: Vec<u32> = self
            .queues
            .approved_proposals()
            .iter()
            .filter(|(_, req)| ProposalKind::of(req).is_governance())
            .map(|(&id, _)| id)
            .collect();
        if !governance.is_empty() {
            return Err(ConversationError::UnexpectedNonMlsProposals {
                proposal_ids: governance,
            });
        }

        let actions = self.batch_actions()?;
        if actions.is_empty() && !self.queues.commit_carries_update_of(&self.own) {
            return Ok(false);
        }
        info!(
            conversation = %self.conversation_id,
            epoch = self.epoch,
            proposals = actions.len(),
            "commit candidate requested"
        );
        self.decide(Decision::BuildCommit { actions });
        Ok(true)
    }

    /// The approved queue projected to commit actions, in FIFO order. An
    /// urgent (emergency-driven) freeze narrows the batch to the target's
    /// removal alone. This member's own update is not an action: the commit
    /// carries it.
    fn batch_actions(&self) -> Result<Vec<Action>, ConversationError> {
        let urgent = self.queues.urgent_commit_target();
        let approved = self.queues.approved_proposals();
        let mut actions = Vec::with_capacity(approved.len());
        for (_id, proposal) in approved.iter() {
            match proposal.payload.as_ref() {
                Some(Payload::MemberInvite(invite)) => {
                    if urgent.is_some() {
                        continue;
                    }
                    actions.push(Action::Add {
                        member: MemberId::from(invite.member_id.as_slice()),
                        key_package: invite.key_package_bytes.clone(),
                    });
                }
                Some(Payload::RemoveMember(remove)) => {
                    if urgent.is_some_and(|target| remove.member_id != target) {
                        continue;
                    }
                    actions.push(Action::Remove {
                        member: MemberId::from(remove.member_id.as_slice()),
                    });
                }
                Some(Payload::LeafUpdate(update)) => {
                    if urgent.is_some() || update.member_id == self.own {
                        continue;
                    }
                    actions.push(Action::Update {
                        member: MemberId::from(update.member_id.as_slice()),
                    });
                }
                _ => return Err(ConversationError::InvalidConversationUpdateRequest),
            }
        }
        Ok(actions)
    }

    /// Finish entering `Freezing`: ask the router for our own candidate — a
    /// no-op unless we are the epoch steward — then announce the phase.
    pub(crate) fn on_freeze_entered(&mut self, event: Phase) -> Result<(), ConversationError> {
        if let Err(e) = self.request_own_candidate() {
            // If the build request fails, this round gets stuck and keeps retrying,
            // but just changing the phase looks like everything is fine.
            self.report_failure("commit_candidate_build", &e);
        }
        self.emit_phase(Some(event));
        Ok(())
    }
}
