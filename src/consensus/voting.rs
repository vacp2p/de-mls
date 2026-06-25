//! Opening proposals, casting votes, and the small adapters that talk to the
//! consensus service.
//!
//! Everything runs inline on the caller's thread: opening a proposal starts the
//! consensus session right away, while the time-based follow-ups (auto-votes,
//! consensus timeouts) wait as deadlines that `tick_deadlines` fires on a later
//! poll. The submit/cast/forward methods at the bottom call the consensus
//! service and return a wire message for the caller to broadcast.

use std::error::Error as StdError;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hashgraph_like_consensus::{
    error::ConsensusError,
    protos::consensus::v1::{Proposal, Vote},
    session::ConsensusConfig,
    storage::ConsensusStorage,
    types::CreateProposalRequest,
    utils::build_vote,
};
use openmls_traits::{OpenMlsProvider, signatures::Signer, storage::StorageProvider};
use prost::Message;
use tracing::info;

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationEvent, ConversationState,
    PeerScoreStorage, ProposalKind,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest},
    self_leave_proposal_id,
};

/// The creator's intent at proposal submit time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatorVote {
    /// Bundle a YES vote with the proposal in one atomic wire message; the
    /// integrator gets `OwnProposalSubmitted`, no vote request. For actions
    /// where submitting already expresses the vote (member add, ban,
    /// self-executing protocol moves).
    Yes,
    /// Broadcast the proposal unbundled and treat the creator like any
    /// other voter: `VoteRequested` plus the auto-vote timer. For steward
    /// auto-propose paths, where the steward forwards peer intent without
    /// endorsing it.
    Deferred,
}

/// Per-proposal consensus-session parameters.
struct ProposalParams {
    expected_voters: u32,
    proposal_expiration: Duration,
    consensus_timeout: Duration,
    liveness_criteria_yes: bool,
}

impl<C, Sc> Conversation<C, Sc>
where
    C: ConsensusPlugin,
    Sc: PeerScoreStorage,
{
    // ── Public API ───────────────────────────────────────────────────

    /// Open a consensus vote for `request`; [`CreatorVote`] picks the wire
    /// shape and the integrator event.
    ///
    /// Errors when the state machine forbids new proposals (freeze phases,
    /// partial freeze during an active emergency). On success the proposal
    /// is on the wire and a consensus-timeout deadline is armed for
    /// `tick_deadlines` to fire.
    ///
    /// Local ownership is recorded before any vote is cast: with a single
    /// expected voter the bundled YES resolves the session synchronously,
    /// and the outcome handler must already see us as the owner by then.
    pub fn initiate_proposal<Pr>(
        &mut self,
        provider: &Pr,
        request: ConversationUpdateRequest,
        creator_vote: CreatorVote,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let kind = ProposalKind::of(&request);
        let expected_voters = self.check_proposal_allowed(kind)?;

        let liveness_criteria_yes = self.config.liveness_criteria_yes;
        let consensus_timeout = self.config.consensus_timeout;
        let voting_delay = self.config.voting_delay_for(kind);

        let (proposal_id, unbundled) = self.submit_proposal(
            &request,
            ProposalParams {
                expected_voters,
                proposal_expiration: self.config.proposal_expiration,
                consensus_timeout,
                liveness_criteria_yes,
            },
        )?;

        self.queues
            .insert_voting_proposal(proposal_id, request.clone());
        if kind.is_emergency() {
            self.queues.insert_emergency(proposal_id);
        }
        // Removed again by `handle_consensus_outcome` if an outcome lands
        // before the deadline fires.
        self.register_consensus_timeout(proposal_id, consensus_timeout);

        match creator_vote {
            CreatorVote::Yes => {
                // Owner-bundling API, not the `cast_vote` helper: peers don't
                // have the proposal yet, so a Vote-only message would be
                // undeliverable.
                let scope = self.conversation_id.clone();
                let proposal = self.services.consensus.cast_vote_and_get_proposal(
                    &scope,
                    proposal_id,
                    true,
                )?;
                info!(
                    conversation = %self.conversation_id,
                    proposal_id,
                    actor = "owner",
                    "YES vote cast (bundled at submit)"
                );
                let payload = self
                    .mls_mut()
                    .build_message(provider, signer, &proposal.into())?;
                self.broadcast(payload);
                self.emit_event(ConversationEvent::OwnProposalSubmitted {
                    proposal_id,
                    request,
                });
            }
            CreatorVote::Deferred => {
                let payload = self.mls_mut().build_message(provider, signer, &unbundled)?;
                self.broadcast(payload);
                self.emit_event(ConversationEvent::VoteRequested {
                    proposal_id,
                    request,
                });
                self.register_auto_vote(proposal_id, voting_delay, liveness_criteria_yes);
            }
        }

        Ok(())
    }

    /// Cast the local member's vote. Cancels the pending auto-vote so the
    /// manual choice wins. Blocked while an epoch rotation is in flight
    /// (`Freezing`/`Selection`) — the encrypted vote might not decrypt on
    /// peers that already merged the next commit.
    pub fn vote<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let state = self.current_state();
        if state == ConversationState::Freezing || state == ConversationState::Selection {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }
        self.cancel_auto_vote(proposal_id);
        self.broadcast_vote(provider, proposal_id, vote, signer)
    }

    // ── Crate-internal ───────────────────────────────────────────────

    /// Fire elapsed auto-votes and consensus timeouts, then drain the
    /// outcome bus into `handle_consensus_outcome`. Per-proposal errors are
    /// logged and skipped so one stuck proposal can't block the rest.
    pub(crate) fn tick_deadlines<Pr>(&mut self, provider: &Pr, signer: &impl Signer)
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let now = std::time::Instant::now();
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
            if let Err(e) = self.broadcast_vote(provider, proposal_id, vote, signer) {
                tracing::debug!(
                    proposal_id,
                    error = %e,
                    "auto-vote skipped (already voted or session resolved)"
                );
            }
        }
        for proposal_id in timeouts_due {
            self.resolve_on_timeout(proposal_id);
        }

        while let Some((_scope, event)) = self.services.consensus_rx.try_recv() {
            if let Err(e) = self.handle_consensus_outcome(provider, event, signer) {
                tracing::warn!(
                    conversation = %self.conversation_id,
                    error = %e,
                    "handle_consensus_outcome failed"
                );
            }
        }
    }

    /// Open a self-leave round: `RemoveMember(self)` with one expected
    /// voter and the leaver's YES bundled, so it resolves synchronously and
    /// lands in `approved_proposals` for the next steward commit.
    ///
    /// Safe to repeat: the pending-leave check catches local duplicates,
    /// and the deterministic [`self_leave_proposal_id`] dedupes
    /// retransmits inside the consensus library.
    pub(crate) fn initiate_self_leave<Pr>(
        &mut self,
        provider: &Pr,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        if self.queues.is_pending_self_leave(&self.self_member_id) {
            info!(
                conversation = %self.conversation_id,
                "self-leave already in flight, ignoring duplicate"
            );
            return Ok(());
        }

        let request = ConversationUpdateRequest::remove_member(self.self_member_id.to_vec());
        let proposal_id = self_leave_proposal_id(&self.self_member_id);

        // Ownership must be recorded before the session opens: the bundled
        // YES fires `ConsensusReached` synchronously, and the outcome
        // handler needs `is_owner_of_proposal` to already be true.
        self.queues
            .insert_voting_proposal(proposal_id, request.clone());

        let submitted = self.submit_self_leave_proposal(ProposalParams {
            expected_voters: 1,
            proposal_expiration: self.config.proposal_expiration,
            consensus_timeout: self.config.consensus_timeout,
            liveness_criteria_yes: true,
        })?;

        // `None`: an earlier submit is already driving this proposal_id;
        // our voting entry resolves on that session.
        let Some((_, app_msg)) = submitted else {
            return Ok(());
        };

        let payload = self.mls_mut().build_message(provider, signer, &app_msg)?;
        self.broadcast(payload);
        Ok(())
    }

    /// Feed a peer's vote into the local consensus session.
    ///
    /// Late arrivals are swallowed (logged, `Ok`) so inbound dispatch keeps
    /// draining.
    pub(crate) fn forward_incoming_vote(&self, vote: Vote) -> Result<(), ConversationError> {
        let proposal_id = vote.proposal_id;
        let outcome_applied_locally = self.queues.is_consensus_outcome_applied(proposal_id);
        let scope = self.conversation_id.clone();
        match self.services.consensus.process_incoming_vote(&scope, vote) {
            Ok(()) => Ok(()),
            Err(ConsensusError::SessionNotActive) => {
                tracing::debug!(
                    conversation = %self.conversation_id,
                    proposal_id,
                    "late vote dropped: consensus session already resolved"
                );
                Ok(())
            }
            Err(ConsensusError::SessionNotFound) => {
                if outcome_applied_locally {
                    tracing::debug!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "late vote dropped: session trimmed after local resolution"
                    );
                } else {
                    tracing::warn!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "vote for unknown proposal id dropped: no local session and not in resolved cache"
                    );
                }
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    // ── Private ──────────────────────────────────────────────────────

    /// Gate a new proposal on the current state and return the expected
    /// voter count (the full member set). During `Reelection` only
    /// emergency and election proposals pass; an active emergency
    /// partial-freezes everything below its priority.
    fn check_proposal_allowed(&self, kind: ProposalKind) -> Result<u32, ConversationError> {
        let state = self.current_state();

        match state {
            ConversationState::Reelection => {
                if !kind.is_emergency() && !kind.is_steward_election() {
                    return Err(ConversationError::ConversationBlocked(state.to_string()));
                }
                if self.queues.partial_freeze_blocks(kind) {
                    return Err(ConversationError::PartialFreeze);
                }
            }
            ConversationState::Freezing | ConversationState::Selection => {
                return Err(ConversationError::ConversationBlocked(state.to_string()));
            }
            _ => {
                if self.queues.partial_freeze_blocks(kind) {
                    return Err(ConversationError::PartialFreeze);
                }
            }
        }

        let members = self.mls().members()?;
        Ok(members.len() as u32)
    }

    /// Push a proposal whose deadline elapsed into the consensus library's
    /// timeout resolution. `SessionNotFound`/`SessionNotActive` despite the
    /// `still_active` guard means the session resolved in between — benign
    /// when the proposal is in the resolved cache, a logic bug worth a
    /// warning when it isn't.
    fn resolve_on_timeout(&self, proposal_id: u32) {
        let scope = self.conversation_id.clone();
        let still_active = self
            .services
            .consensus
            .storage()
            .get_active_proposals(&scope)
            .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
            .unwrap_or(false);
        if !still_active {
            return;
        }
        match self
            .services
            .consensus
            .handle_consensus_timeout(&scope, proposal_id)
        {
            Ok(_) => {}
            Err(ConsensusError::SessionNotFound) | Err(ConsensusError::SessionNotActive) => {
                let resolved_locally = self.queues.is_consensus_outcome_applied(proposal_id);
                if resolved_locally {
                    tracing::debug!(
                        conversation = %self.conversation_id,
                        proposal_id,
                        "timeout fired for already-resolved proposal: ignoring"
                    );
                } else {
                    tracing::warn!(
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

    /// Cast a vote in the local session, encrypt the Vote-only wire
    /// message, and buffer it for broadcast. Manual votes and the auto-vote
    /// timer share this path; the consensus library can't tell them apart.
    fn broadcast_vote<Pr>(
        &mut self,
        provider: &Pr,
        proposal_id: u32,
        vote: bool,
        signer: &impl Signer,
    ) -> Result<(), ConversationError>
    where
        Pr: OpenMlsProvider,
        <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
    {
        let scope = self.conversation_id.clone();
        let choice = if vote { "YES" } else { "NO" };
        info!(
            conversation = %self.conversation_id,
            proposal_id, choice, "vote cast"
        );
        let vote_msg = self
            .services
            .consensus
            .cast_vote(&scope, proposal_id, vote)?;

        let payload = self
            .mls_mut()
            .build_message(provider, signer, &vote_msg.into())?;
        self.broadcast(payload);
        Ok(())
    }

    /// Open a consensus session for `request`; returns the new `proposal_id`
    /// and the unbundled `Proposal` wire message.
    fn submit_proposal(
        &self,
        request: &ConversationUpdateRequest,
        params: ProposalParams,
    ) -> Result<(u32, AppMessage), ConversationError> {
        let create_request = CreateProposalRequest::new(
            uuid::Uuid::new_v4().to_string(),
            request.encode_to_vec(),
            self.self_member_id.to_vec(),
            params.expected_voters,
            params.proposal_expiration.as_secs(),
            params.liveness_criteria_yes,
        )?;

        let scope = self.conversation_id.clone();
        let proposal = self.services.consensus.create_proposal_with_config(
            &scope,
            create_request,
            Some(ConsensusConfig::gossipsub().with_timeout(params.consensus_timeout)?),
        )?;

        info!(
            conversation = %self.conversation_id,
            proposal_id = proposal.proposal_id,
            voters = params.expected_voters,
            "proposal opened"
        );

        let proposal_id = proposal.proposal_id;
        Ok((proposal_id, proposal.into()))
    }

    /// Open a self-leave session: a hand-crafted `Proposal` carrying the
    /// deterministic [`self_leave_proposal_id`] and the leaver's signed YES,
    /// so the single-voter session resolves on arrival.
    ///
    /// The fixed id makes retransmits collide as `ProposalAlreadyExist`,
    /// returned as `Ok(None)` — nothing to broadcast.
    fn submit_self_leave_proposal(
        &self,
        params: ProposalParams,
    ) -> Result<Option<(u32, AppMessage)>, ConversationError> {
        let self_member_id = &self.self_member_id;
        let request = ConversationUpdateRequest::remove_member(self_member_id.to_vec());
        let payload = request.encode_to_vec();

        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let expiration = now.saturating_add(params.proposal_expiration.as_secs());

        let proposal_id = self_leave_proposal_id(self_member_id);
        let mut proposal = Proposal {
            name: format!("self-leave:{proposal_id}"),
            payload,
            proposal_id,
            proposal_owner: self_member_id.to_vec(),
            votes: Vec::new(),
            expected_voters_count: params.expected_voters,
            round: 1,
            timestamp: now,
            expiration_timestamp: expiration,
            liveness_criteria_yes: params.liveness_criteria_yes,
        };

        let yes_vote = build_vote(&proposal, true, self.services.consensus.signer())?;
        proposal.votes.push(yes_vote);

        let scope = self.conversation_id.clone();
        match self
            .services
            .consensus
            .process_incoming_proposal(&scope, proposal.clone())
        {
            Ok(()) => {
                info!(
                    conversation = %self.conversation_id,
                    proposal_id, "self-leave proposal opened (expected_voters=1, bundled YES)"
                );
                Ok(Some((proposal_id, proposal.into())))
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
