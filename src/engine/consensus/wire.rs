//! Wire-level proposal construction: consensus-session parameters, the
//! control-message encoders, opening a session (regular and single-voter), and
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
    engine::{
        handle::Engine, proposal_kind::ProposalKind, store::EngineStore,
        util::single_voter_proposal_id,
    },
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

/// Encode one `ControlMessage` payload for the router to seal.
pub(crate) fn control_bytes(payload: control_message::Payload) -> Vec<u8> {
    ControlMessage {
        payload: Some(payload),
    }
    .encode_to_vec()
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
/// they are restricted to self-removal and a member's own leaf update.
/// Requiring the MLS-authenticated `sender` to be both the owner and the
/// `RemoveMember` target (or the `LeafUpdate` member) closes the
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
    match request.payload {
        Some(conversation_update_request::Payload::RemoveMember(ref r)) => r.member_id == sender,
        Some(conversation_update_request::Payload::LeafUpdate(ref u)) => u.member_id == sender,
        _ => false,
    }
}

impl<St: EngineStore> Engine<St> {
    /// Open a self-leave round: `RemoveMember(self)` with one expected voter
    /// and the leaver's YES bundled, so it resolves synchronously and lands
    /// in the approved queue for the next steward commit.
    ///
    /// Safe to repeat: the pending-leave check catches local duplicates, and
    /// the deterministic [`single_voter_proposal_id`] dedupes retransmits
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
        let proposal_id = single_voter_proposal_id(&self.own);
        self.open_single_voter(&request, proposal_id, format!("self-leave:{proposal_id}"))
    }

    /// Open a leaf-update round like a self-leave: `LeafUpdate(self)` with
    /// one expected voter and the owner's YES bundled. Gated like any
    /// membership proposal; a repeat while ours is approved is a no-op.
    pub(crate) fn initiate_leaf_update(
        &mut self,
        update: Vec<u8>,
    ) -> Result<(), ConversationError> {
        self.check_proposal_allowed(ProposalKind::Commit)?;
        if self.queues.has_approved_update(&self.own) {
            info!(
                conversation = %self.conversation_id,
                "leaf update already approved this epoch, ignoring duplicate"
            );
            return Ok(());
        }

        let proposal_id = single_voter_proposal_id(&update);
        let request = ConversationUpdateRequest::leaf_update(self.own.clone(), update);
        self.open_single_voter(&request, proposal_id, format!("leaf-update:{proposal_id}"))
    }

    /// Track `request` under `proposal_id`, open its single-voter session and
    /// broadcast the proposal.
    fn open_single_voter(
        &mut self,
        request: &ConversationUpdateRequest,
        proposal_id: u32,
        name: String,
    ) -> Result<(), ConversationError> {
        // Track before the session opens: the bundled YES fires the outcome
        // synchronously.
        self.queues.track_voting_proposal(proposal_id, request);

        let submitted = self.submit_single_voter_proposal(request, proposal_id, name)?;

        // `None`: an earlier submit is already driving this proposal id; our
        // voting entry resolves on that session.
        let Some(proposal) = submitted else {
            return Ok(());
        };

        self.send_control(control_proposal(proposal));
        self.dirty.consensus = true;
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

    /// Open a single-voter session: a hand-crafted `Proposal` carrying
    /// `proposal_id` and the owner's YES, so the session resolves on arrival.
    ///
    /// The fixed id makes retransmits collide as `ProposalAlreadyExist`,
    /// returned as `Ok(None)` — nothing to broadcast.
    fn submit_single_voter_proposal(
        &self,
        request: &ConversationUpdateRequest,
        proposal_id: u32,
        name: String,
    ) -> Result<Option<Proposal>, ConversationError> {
        let payload = request.encode_to_vec();

        let now = self.now.as_secs();
        let expiration = now.saturating_add(self.config.proposal_expiration.as_secs());

        let mut proposal = Proposal {
            name,
            payload,
            proposal_id,
            proposal_owner: self.own.clone(),
            votes: Vec::new(),
            expected_voters_count: 1,
            round: 1,
            timestamp: now,
            expiration_timestamp: expiration,
            liveness_criteria_yes: true,
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
                    proposal_id, "single-voter proposal opened (expected_voters=1, bundled YES)"
                );
                Ok(Some(proposal))
            }
            Err(ConsensusError::ProposalAlreadyExist) => {
                info!(
                    conversation = %self.conversation_id,
                    proposal_id, "single-voter proposal already in flight, skipping retransmit"
                );
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }
}
