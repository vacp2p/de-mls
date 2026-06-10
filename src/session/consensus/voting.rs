//! Opening proposals and casting votes.
//!
//! Everything runs inline on the caller's thread: opening a proposal
//! starts the consensus session immediately; the time-based follow-ups
//! (auto-votes, consensus timeouts) are deadlines that `tick_deadlines`
//! fires on a later poll.

use hashgraph_like_consensus::{error::ConsensusError, storage::ConsensusStorage};
use tracing::info;

use crate::{
    core::{
        ConsensusPlugin, ConversationPluginsFactory, ProposalKind, SessionEvent,
        SyncConsensusReceiver, self_leave_proposal_id,
    },
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest},
    session::{
        ConversationState, SessionError, SessionRunner,
        consensus::bridge::{
            ProposalParams, cast_vote, submit_proposal, submit_self_leave_proposal,
        },
    },
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

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
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
    pub fn initiate_proposal(
        &mut self,
        request: ConversationUpdateRequest,
        creator_vote: CreatorVote,
    ) -> Result<(), SessionError> {
        let kind = ProposalKind::of(&request);
        let expected_voters = self.check_proposal_allowed(kind)?;

        let liveness_criteria_yes = self.conversation.config.liveness_criteria_yes;
        let consensus_timeout = self.conversation.config.consensus_timeout;
        let voting_delay = self.conversation.config.voting_delay_for(kind);

        let (proposal_id, unbundled) = submit_proposal::<P>(
            &self.conversation_id,
            &request,
            &self.self_member_id,
            &self.consensus,
            ProposalParams {
                expected_voters,
                proposal_expiration: self.conversation.config.proposal_expiration,
                consensus_timeout,
                liveness_criteria_yes,
            },
        )?;

        self.conversation
            .queues
            .insert_voting_proposal(proposal_id, request.clone());
        if kind.is_emergency() {
            self.conversation.queues.insert_emergency(proposal_id);
        }
        // Removed again by `apply_consensus_outcome` if an outcome lands
        // before the deadline fires.
        self.register_consensus_timeout(proposal_id, consensus_timeout);

        match creator_vote {
            CreatorVote::Yes => {
                // Owner-bundling API, not the `cast_vote` helper: peers don't
                // have the proposal yet, so a Vote-only message would be
                // undeliverable.
                let scope = P::Scope::from(self.conversation_id.clone());
                let proposal =
                    self.consensus
                        .cast_vote_and_get_proposal(&scope, proposal_id, true)?;
                info!(
                    conversation = %self.conversation_id,
                    proposal_id,
                    actor = "owner",
                    "YES vote cast (bundled at submit)"
                );
                let outbound: AppMessage = proposal.into();
                let payload = self
                    .conversation
                    .expect_mls_mut()?
                    .build_message(&outbound)?;
                self.broadcast(payload);
                self.emit_event(SessionEvent::OwnProposalSubmitted {
                    proposal_id,
                    request,
                });
            }
            CreatorVote::Deferred => {
                let payload = self
                    .conversation
                    .expect_mls_mut()?
                    .build_message(&unbundled)?;
                self.broadcast(payload);
                self.emit_event(SessionEvent::VoteRequested {
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
    pub fn vote(&mut self, proposal_id: u32, vote: bool) -> Result<(), SessionError> {
        let state = self.conversation.current_state();
        if state == ConversationState::Freezing || state == ConversationState::Selection {
            return Err(SessionError::ConversationBlocked(state.to_string()));
        }
        self.cancel_auto_vote(proposal_id);
        self.broadcast_vote(proposal_id, vote)
    }

    /// Fire elapsed auto-votes and consensus timeouts, then drain the
    /// event bus into `apply_consensus_outcome`. Per-proposal errors are
    /// logged and skipped so one stuck proposal can't block the rest.
    pub(crate) fn tick_deadlines(&mut self) {
        let now = std::time::Instant::now();
        let auto_votes_due: Vec<(u32, bool)> = self
            .pending_auto_votes
            .iter()
            .filter(|(_, e)| e.fire_at <= now)
            .map(|(id, e)| (*id, e.vote))
            .collect();
        for (id, _) in &auto_votes_due {
            self.pending_auto_votes.remove(id);
        }
        let timeouts_due: Vec<u32> = self
            .pending_consensus_timeouts
            .iter()
            .filter(|(_, fire_at)| **fire_at <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in &timeouts_due {
            self.pending_consensus_timeouts.remove(id);
        }

        for (proposal_id, vote) in auto_votes_due {
            if let Err(e) = self.broadcast_vote(proposal_id, vote) {
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

        loop {
            // The bus is private to this conversation's service, so the
            // scope on each event is always ours — drained, not matched.
            let Some((_scope, event)) =
                <_ as SyncConsensusReceiver<_>>::try_recv(&mut self.consensus_rx)
            else {
                break;
            };
            if let Err(e) = self.apply_consensus_outcome(event) {
                tracing::warn!(
                    conversation = %self.conversation_id,
                    error = %e,
                    "apply_consensus_outcome failed"
                );
            }
        }
    }

    // ── Crate-internal ───────────────────────────────────────────────

    /// Open a self-leave round: `RemoveMember(self)` with one expected
    /// voter and the leaver's YES bundled, so it resolves synchronously and
    /// lands in `approved_proposals` for the next steward commit.
    ///
    /// Safe to repeat: the pending-leave check catches local duplicates,
    /// and the deterministic [`self_leave_proposal_id`] dedupes
    /// retransmits inside the consensus library.
    pub(crate) fn initiate_self_leave(&mut self) -> Result<(), SessionError> {
        if self
            .conversation
            .queues
            .is_pending_self_leave(&self.self_member_id)
        {
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
        self.conversation
            .queues
            .insert_voting_proposal(proposal_id, request.clone());

        let submitted = submit_self_leave_proposal::<P>(
            &self.conversation_id,
            &self.self_member_id,
            &self.consensus,
            ProposalParams {
                expected_voters: 1,
                proposal_expiration: self.conversation.config.proposal_expiration,
                consensus_timeout: self.conversation.config.consensus_timeout,
                liveness_criteria_yes: true,
            },
        )?;

        // `None`: an earlier submit is already driving this proposal_id;
        // our voting entry resolves on that session.
        let Some((_proposal_id, app_msg)) = submitted else {
            return Ok(());
        };

        let payload = self
            .conversation
            .expect_mls_mut()?
            .build_message(&app_msg)?;
        self.broadcast(payload);
        Ok(())
    }

    // ── Private ──────────────────────────────────────────────────────

    /// Gate a new proposal on the current state and return the expected
    /// voter count (the full member set). During `Reelection` only
    /// emergency and election proposals pass; an active emergency
    /// partial-freezes everything below its priority.
    fn check_proposal_allowed(&self, kind: ProposalKind) -> Result<u32, SessionError> {
        let state = self.conversation.current_state();

        match state {
            ConversationState::Reelection => {
                if !kind.is_emergency() && !kind.is_steward_election() {
                    return Err(SessionError::ConversationBlocked(state.to_string()));
                }
                if self.conversation.queues.partial_freeze_blocks(kind) {
                    return Err(SessionError::PartialFreeze);
                }
            }
            ConversationState::Freezing | ConversationState::Selection => {
                return Err(SessionError::ConversationBlocked(state.to_string()));
            }
            _ => {
                if self.conversation.queues.partial_freeze_blocks(kind) {
                    return Err(SessionError::PartialFreeze);
                }
            }
        }

        let members = self.conversation.expect_mls()?.members()?;
        Ok(members.len() as u32)
    }

    /// Push a proposal whose deadline elapsed into the consensus library's
    /// timeout resolution. `SessionNotFound`/`SessionNotActive` despite the
    /// `still_active` guard means the session resolved in between — benign
    /// when the proposal is in the resolved cache, a logic bug worth a
    /// warning when it isn't.
    fn resolve_on_timeout(&self, proposal_id: u32) {
        let scope = P::Scope::from(self.conversation_id.clone());
        let still_active = self
            .consensus
            .storage()
            .get_active_proposals(&scope)
            .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
            .unwrap_or(false);
        if !still_active {
            return;
        }
        match self.consensus.handle_consensus_timeout(&scope, proposal_id) {
            Ok(_) => {}
            Err(ConsensusError::SessionNotFound) | Err(ConsensusError::SessionNotActive) => {
                let resolved_locally = self
                    .conversation
                    .queues
                    .is_consensus_outcome_applied(proposal_id);
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
    fn broadcast_vote(&mut self, proposal_id: u32, vote: bool) -> Result<(), SessionError> {
        let app_message =
            cast_vote::<P>(&self.conversation_id, proposal_id, vote, &self.consensus)?;
        let payload = self
            .conversation
            .expect_mls_mut()?
            .build_message(&app_message)?;
        self.broadcast(payload);
        Ok(())
    }
}
