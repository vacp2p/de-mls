//! Minting our own candidate: batching the approved queue into commit
//! actions, asking the router to build the commit, opening the round when a
//! peer's candidate arrives first, and replaying candidates stashed ahead of
//! their proposal's approval.

use tracing::info;

use crate::{
    ConversationError,
    engine::{
        commit::buffer::BufferedCandidate,
        handle::{Engine, PendingBuild},
        proposal_kind::ProposalKind,
        store::EngineStore,
        types::{Action, Decision, MemberId, Phase},
    },
    protos::de_mls::messages::v1::conversation_update_request::Payload,
};

impl<St: EngineStore> Engine<St> {
    /// A peer's candidate opens the round locally: from `Working` we enter
    /// `Freezing` and, if we are the epoch steward, mint our own.
    pub(crate) fn open_round_on_peer_candidate(&mut self) -> Result<(), ConversationError> {
        if self.current_state() != Phase::Working {
            return Ok(());
        }
        let Some(event) = self.start_freezing() else {
            return Ok(());
        };
        self.on_freeze_entered(event)
    }

    /// Re-buffer candidates stashed before their proposal was approved.
    pub(crate) fn replay_early_candidates(&mut self) -> Result<(), ConversationError> {
        let epoch = self.epoch;
        let stashed = self.queues.take_early_candidates(epoch);
        if stashed.is_empty() {
            return Ok(());
        }
        let max = self.members.len();
        for candidate in stashed {
            if self.queues.has_committed_hash(&candidate.hash) {
                continue;
            }
            self.round.insert(
                epoch,
                BufferedCandidate {
                    hash: candidate.hash,
                    envelope: candidate.envelope,
                    facts: Some(candidate.facts),
                    is_local: false,
                },
                max,
            );
        }
        Ok(())
    }

    /// `(received, expected)` candidates for the current round.
    pub(crate) fn commit_candidate_count(&self) -> (usize, usize) {
        let received = self.round.candidates(self.epoch).len();
        let expected = self
            .steward_list
            .current_list()
            .map_or(0, |list| list.len());
        (received, expected)
    }

    /// Ask the router to build our candidate from the approved batch. Only
    /// the epoch steward builds (design 11): `Ok(false)` when we aren't it,
    /// there is nothing to commit, or a build is already outstanding.
    pub(crate) fn request_own_candidate(&mut self) -> Result<bool, ConversationError> {
        if self.pending_build.is_some() {
            return Ok(false);
        }
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
        if actions.is_empty() {
            return Ok(false);
        }
        info!(
            conversation = %self.conversation_id,
            epoch = self.epoch,
            proposals = actions.len(),
            "commit candidate requested"
        );
        self.pending_build = Some(PendingBuild {
            actions: actions.clone(),
        });
        self.decide(Decision::BuildCommit { actions });
        Ok(true)
    }

    /// The approved queue projected to commit actions, in FIFO order and
    /// capped at `commit_batch_max`. An urgent (emergency-driven) freeze
    /// narrows the batch to the target's removal alone.
    fn batch_actions(&self) -> Result<Vec<Action>, ConversationError> {
        let urgent = self.queues.urgent_commit_target();
        let k_max = self.config.commit_batch_max;
        let approved = self.queues.approved_proposals();
        let mut actions = Vec::with_capacity(approved.len().min(k_max));
        for (_id, proposal) in approved.iter().take(k_max) {
            match proposal.payload.as_ref() {
                Some(Payload::MemberInvite(invite)) => {
                    if urgent.is_some() || self.is_member(&invite.member_id) {
                        continue;
                    }
                    actions.push(Action::Add {
                        member: MemberId::from(invite.member_id.as_slice()),
                        key_package: invite.key_package_bytes.clone(),
                    });
                }
                Some(Payload::RemoveMember(remove)) => {
                    if urgent.is_some_and(|target| remove.member_id != target)
                        || !self.is_member(&remove.member_id)
                    {
                        continue;
                    }
                    actions.push(Action::Remove {
                        member: MemberId::from(remove.member_id.as_slice()),
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
            // A mint failure stalls this round and repeats on every retry, and
            // the phase change alone reads as a healthy freeze.
            self.report_failure("commit_candidate_build", &e);
        }
        self.emit_phase(Some(event));
        Ok(())
    }
}
