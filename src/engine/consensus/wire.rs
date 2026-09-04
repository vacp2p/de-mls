//! Wire-level proposal construction: consensus-session parameters, the
//! control-message encoders, opening a session (regular and self-leave), and
//! the fast-path authorization check for single-voter proposals.

use hashgraph_like_consensus::{
    error::ConsensusError,
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    types::CreateProposalRequest,
    utils::build_vote,
};
use prost::Message;
use tracing::info;

use std::time::Duration;

use crate::{
    ConversationError,
    engine::{handle::Engine, store::EngineStore, util::self_leave_proposal_id},
    protos::de_mls::messages::v1::{
        ControlMessage, ConversationUpdateRequest, control_message, conversation_update_request,
    },
};

/// Per-proposal consensus-session parameters.
pub(crate) struct ProposalParams {
    pub(crate) expected_voters: u32,
    pub(crate) proposal_expiration: Duration,
    pub(crate) consensus_timeout: Duration,
    pub(crate) liveness_criteria_yes: bool,
}

/// Encode `proposal` as the control message peers receive.
pub(crate) fn control_proposal(proposal: Proposal) -> Vec<u8> {
    ControlMessage {
        payload: Some(control_message::Payload::Proposal(proposal)),
    }
    .encode_to_vec()
}

/// Encode `vote` as the control message peers receive.
pub(crate) fn control_vote(vote: Vote) -> Vec<u8> {
    ControlMessage {
        payload: Some(control_message::Payload::Vote(vote)),
    }
    .encode_to_vec()
}

/// Fast-path proposals (`expected_voters_count == 1`) bypass peer voting, so
/// they are restricted to self-removal. Requiring the MLS-authenticated
/// `sender` to be both the owner and the `RemoveMember` target closes the
/// unilateral-removal vector an otherwise-free `expected_voters == 1` opens.
///
/// `true` when the proposal is allowed. A mismatch — different target, wrong
/// payload variant, or undecodable — is `false`.
pub(crate) fn authorize_fast_path_proposal(proposal: &Proposal, sender: &[u8]) -> bool {
    if proposal.expected_voters_count != 1 {
        return true;
    }
    if proposal.proposal_owner != sender {
        return false;
    }
    let Ok(request) = ConversationUpdateRequest::decode(proposal.payload.as_slice()) else {
        return false;
    };
    matches!(
        request.payload,
        Some(conversation_update_request::Payload::RemoveMember(ref r)) if r.member_id == sender
    )
}

impl<St: EngineStore> Engine<St> {
    /// Open a self-leave round: `RemoveMember(self)` with one expected voter
    /// and the leaver's YES bundled, so it resolves synchronously and lands
    /// in the approved queue for the next steward commit.
    ///
    /// Safe to repeat: the pending-leave check catches local duplicates, and
    /// the deterministic [`self_leave_proposal_id`] dedupes retransmits
    /// inside the consensus library.
    pub(crate) fn initiate_self_leave(&mut self) -> Result<(), ConversationError> {
        if self.queues.is_pending_self_leave(&self.own) {
            info!(
                conversation = %self.conversation_id,
                "self-leave already in flight, ignoring duplicate"
            );
            return Ok(());
        }

        let request = ConversationUpdateRequest::remove_member(self.own.clone());
        let proposal_id = self_leave_proposal_id(&self.own);

        // Track before the session opens: the bundled YES fires the outcome
        // synchronously.
        self.queues.track_voting_proposal(proposal_id, &request);

        let submitted = self.submit_self_leave_proposal(ProposalParams {
            expected_voters: 1,
            proposal_expiration: self.config.proposal_expiration,
            consensus_timeout: self.config.consensus_timeout,
            liveness_criteria_yes: true,
        })?;

        // `None`: an earlier submit is already driving this proposal id; our
        // voting entry resolves on that session.
        let Some(proposal) = submitted else {
            return Ok(());
        };

        self.send_control(control_proposal(proposal));
        // store: consensus sessions (WP3g)
        Ok(())
    }

    /// Open a consensus session for `request`; returns the new proposal id
    /// and the unbundled `Proposal` wire message.
    pub(crate) fn submit_proposal(
        &self,
        request: &ConversationUpdateRequest,
        params: ProposalParams,
    ) -> Result<(u32, Proposal), ConversationError> {
        let create_request = CreateProposalRequest::new(
            uuid::Uuid::new_v4().to_string(),
            request.encode_to_vec(),
            self.own.clone(),
            params.expected_voters,
            params.proposal_expiration.as_secs(),
            params.liveness_criteria_yes,
        )?;

        let scope = self.conversation_id.clone();
        let proposal = self.consensus.create_proposal_with_config(
            &scope,
            create_request,
            Some(ConsensusConfig::gossipsub().with_timeout(params.consensus_timeout)?),
            self.now.as_secs(),
        )?;

        info!(
            conversation = %self.conversation_id,
            proposal_id = proposal.proposal_id,
            voters = params.expected_voters,
            "proposal opened"
        );

        let proposal_id = proposal.proposal_id;
        Ok((proposal_id, proposal))
    }

    /// Open a self-leave session: a hand-crafted `Proposal` carrying the
    /// deterministic [`self_leave_proposal_id`] and the leaver's YES, so the
    /// single-voter session resolves on arrival.
    ///
    /// The fixed id makes retransmits collide as `ProposalAlreadyExist`,
    /// returned as `Ok(None)` — nothing to broadcast.
    fn submit_self_leave_proposal(
        &self,
        params: ProposalParams,
    ) -> Result<Option<Proposal>, ConversationError> {
        let request = ConversationUpdateRequest::remove_member(self.own.clone());
        let payload = request.encode_to_vec();

        let now = self.now.as_secs();
        let expiration = now.saturating_add(params.proposal_expiration.as_secs());

        let proposal_id = self_leave_proposal_id(&self.own);
        let mut proposal = Proposal {
            name: format!("self-leave:{proposal_id}"),
            payload,
            proposal_id,
            proposal_owner: self.own.clone(),
            votes: Vec::new(),
            expected_voters_count: params.expected_voters,
            round: 1,
            timestamp: now,
            expiration_timestamp: expiration,
            liveness_criteria_yes: params.liveness_criteria_yes,
        };

        let yes_vote = build_vote(&proposal, true, self.consensus.signer(), now)?;
        proposal.votes.push(yes_vote);

        let scope = self.conversation_id.clone();
        match self
            .consensus
            .process_incoming_proposal(&scope, proposal.clone(), now)
        {
            Ok(()) => {
                info!(
                    conversation = %self.conversation_id,
                    proposal_id, "self-leave proposal opened (expected_voters=1, bundled YES)"
                );
                Ok(Some(proposal))
            }
            Err(ConsensusError::ProposalAlreadyExist) => {
                info!(
                    conversation = %self.conversation_id,
                    proposal_id, "self-leave already in flight, skipping retransmit"
                );
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}
