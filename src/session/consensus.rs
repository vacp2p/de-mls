//! Proposal submission + voting on `SessionRunner`.
//!
//! Outgoing proposals submit to consensus, register ownership, broadcast
//! (bundled or unbundled per `creator_vote`), and resolve on timeout.

use std::sync::Arc;

use hashgraph_like_consensus::{error::ConsensusError, storage::ConsensusStorage};
use tracing::info;

use crate::{
    core::{
        ConsensusPlugin, ConversationPluginsFactory, ProposalKind, SessionEvent, StewardListPlugin,
        SyncConsensusReceiver, self_leave_proposal_id, target_member_id_of,
    },
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationUpdateRequest, RemoveMember, conversation_update_request,
    },
    session::{
        ConversationState, SessionRunner, SessionTick, SessionError,
        consensus_bridge::{
            ProposalParams, cast_vote, submit_proposal, submit_self_leave_proposal,
        },
    },
};

/// The creator's intent at proposal submit time. Controls both the wire
/// shape and the local UI flow — see [`SessionRunner::initiate_proposal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatorVote {
    /// Creator commits YES at submit. Vote is bundled with the proposal
    /// in one atomic wire message; local UI gets `OwnProposalSubmitted`
    /// (no vote request). Used for unambiguous actions: ban-button click,
    /// self-executing protocol moves (`SCORE_BELOW_THRESHOLD`, `Deadlock`).
    Yes,
    /// Creator hasn't decided. Broadcast unbundled; local UI vote requests
    /// alongside the auto-vote timer (`liveness_criteria_yes` after
    /// `voting_delay_for`). Used for steward auto-propose paths where
    /// the steward still exercises judgement.
    Deferred,
}

/// Typed payload for the proposal lifecycle.
struct NewProposal {
    request: ConversationUpdateRequest,
    expected_voters: u32,
    kind: ProposalKind,
    creator_vote: CreatorVote,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    // ── Public API ───────────────────────────────────────────────────

    /// Start a consensus vote for `request`.
    ///
    /// Gates against the session's state machine (no proposals during
    /// `Freezing`/`Selection`, partial-freeze rules during `Reelection`,
    /// MLS must be attached), opens the consensus session inline, casts
    /// the creator's vote (bundled YES) or registers an auto-vote
    /// (Deferred), and records a `consensus_timeout` deadline. The
    /// caller's polling loop fires `handle_consensus_timeout` via
    /// [`Self::tick_deadlines`] once the deadline elapses.
    ///
    /// `creator_vote` — see [`CreatorVote`] for wire shape and local UI behavior.
    pub fn initiate_proposal(
        &mut self,
        request: ConversationUpdateRequest,
        creator_vote: CreatorVote,
    ) -> Result<(), SessionError> {
        let kind = ProposalKind::of(&request);
        let expected_voters = self.check_proposal_allowed(kind)?;
        self.register_new_proposal(NewProposal {
            request,
            expected_voters,
            kind,
            creator_vote,
        })?;
        Ok(())
    }

    /// Handle an incoming membership update (KP-derived `MemberInvite` or
    /// `RemoveMember`): buffer it so every member has a durable record, then
    /// promote it to a voting proposal if this node is the current epoch
    /// steward and the conversation accepts new proposals.
    pub fn handle_incoming_update_request(
        &mut self,
        request: ConversationUpdateRequest,
    ) -> Result<(), SessionError> {
        let (pending_join, members_for_rotation, current_epoch) = {
            let pending = self.conversation.current_state() == ConversationState::PendingJoin;
            match (pending, self.conversation.mls()) {
                (true, _) | (false, None) => (pending, Vec::new(), 0u64),
                (false, Some(mls)) => (false, mls.members()?, mls.current_epoch()?),
            }
        };
        if pending_join {
            return Ok(());
        }

        // Defensive — core only emits membership changes here.
        if target_member_id_of(&request).is_none() {
            return Ok(());
        }

        let inserted = self
            .conversation
            .queues
            .insert_pending_update(request.clone(), current_epoch);

        // Only the epoch steward proposes immediately. The buffer
        // survives freeze rounds so a later steward can retry.
        let self_member_id = Arc::clone(&self.self_member_id);
        let is_epoch_steward = {
            let eligible = self
                .conversation
                .queues
                .steward_eligibility(&members_for_rotation);
            self.conversation
                .steward_list
                .epoch_steward(current_epoch, &eligible)
                .is_some_and(|es| es == &*self_member_id)
        };
        let state = self.conversation.current_state();
        let buffer_total = self.conversation.queues.pending_update_count();
        let should_propose = is_epoch_steward && state == ConversationState::Working;
        let conversation_id = self.conversation_id.clone();

        info!(
            conversation = %conversation_id,
            epoch = current_epoch,
            inserted,
            buffer_total,
            is_epoch_steward,
            state = %state,
            propose = should_propose,
            "update request buffered"
        );

        if should_propose {
            // Steward auto-propose: the steward forwards peer intent and
            // still holds a judgement call, so we broadcast unbundled and
            // let the vote request drive the steward's vote like any other member.
            // `check_proposal_allowed` may still reject (active emergency
            // etc.) — leave the entry in the buffer for next rotation.
            if let Err(e) = self.initiate_proposal(request, CreatorVote::Deferred) {
                info!(conversation = %conversation_id, error = %e, "proposal deferred");
            }
        }
        Ok(())
    }

    /// Cast a manual vote on behalf of the local member. Blocked in
    /// `Freezing` and `Selection`; cancels any pending auto-vote so the
    /// manual choice wins.
    pub fn process_user_vote(&mut self, proposal_id: u32, vote: bool) -> Result<(), SessionError> {
        let state = self.conversation.current_state();
        if state == ConversationState::Freezing || state == ConversationState::Selection {
            return Err(SessionError::ConversationBlocked(state.to_string()));
        }
        let consensus = self.consensus.clone();
        let conversation_id = self.conversation_id.clone();

        // Manual vote takes precedence over the pending auto-vote timer.
        self.cancel_auto_vote(proposal_id);

        let app_message = cast_vote::<P>(&conversation_id, proposal_id, vote, &consensus)?;
        let payload = self
            .conversation
            .expect_mls_mut()?
            .build_message(&app_message)?;
        self.broadcast(payload);
        Ok(())
    }

    /// Walk pending deadlines, fire any whose `fire_at` has elapsed, then
    /// drain the consensus event bus and dispatch each event through
    /// `apply_consensus_outcome`. Call from the caller's polling loop.
    pub fn tick_deadlines(&mut self) -> Result<SessionTick, SessionError> {
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
            if let Err(e) = self.cast_auto_vote(proposal_id, vote) {
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
            // The bus is per-session and single-scope, so the scope on each
            // event is always this conversation's — drained, not matched.
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

        Ok(self.tick())
    }

    // ── Crate-internal ───────────────────────────────────────────────

    /// Open a self-leave consensus session: `RemoveMember(self)` with
    /// `expected_voters = 1` and the leaver's YES bundled. Resolves
    /// synchronously, so the normal `apply_consensus_outcome` path commits
    /// the removal on the next steward commit. Idempotent — a second call
    /// after a successful submit short-circuits on the local pending-leave
    /// check, and a retransmit dedupes inside the consensus library via
    /// the deterministic [`self_leave_proposal_id`].
    pub fn initiate_self_leave(&mut self) -> Result<(), SessionError> {
        let self_member_id = Arc::clone(&self.self_member_id);
        let conversation_id = self.conversation_id.clone();

        if self
            .conversation
            .queues
            .is_pending_self_leave(&self_member_id)
        {
            info!(
                conversation = %conversation_id,
                "self-leave already in flight, ignoring duplicate"
            );
            return Ok(());
        }

        let request = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::RemoveMember(
                RemoveMember {
                    member_id: self_member_id.to_vec(),
                },
            )),
        };
        let proposal_id = self_leave_proposal_id(&self_member_id);

        // Register ownership BEFORE the session opens — the bundled YES
        // fires `ConsensusReached` synchronously, and
        // `apply_consensus_outcome` needs `is_owner_of_proposal` to be true
        // by then.
        self.conversation
            .queues
            .insert_voting_proposal(proposal_id, request.clone());
        let consensus = self.consensus.clone();
        let proposal_expiration = self.conversation.config.proposal_expiration;
        let consensus_timeout = self.conversation.config.consensus_timeout;

        let submitted = submit_self_leave_proposal::<P>(
            &conversation_id,
            &self_member_id,
            &consensus,
            ProposalParams {
                expected_voters: 1,
                proposal_expiration,
                consensus_timeout,
                liveness_criteria_yes: true,
            },
        )?;

        // Dedup (`ProposalAlreadyExist`) — another submit is already driving
        // this proposal_id. Our voting entry resolves on that session.
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

    /// Check that the conversation state allows creating a proposal of this
    /// kind and return the expected voter count.
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

    /// Open the consensus session, record ownership, then either bundle
    /// the creator's vote or broadcast unbundled depending on
    /// `creator_vote`. Always notifies the integrator — via
    /// `OwnProposalSubmitted` when bundled (history only, no vote
    /// affordance) or via `VoteRequested` when unbundled (same event peers
    /// receive for the proposal).
    ///
    /// Ownership is stored *before* the vote is cast, so a single-voter
    /// consensus transition can't race `is_owner=false` when the drain
    /// loop in `tick_deadlines` picks it up.
    fn register_new_proposal(&mut self, np: NewProposal) -> Result<u32, SessionError> {
        let NewProposal {
            request,
            expected_voters,
            kind,
            creator_vote,
        } = np;

        let proposal_expiration = self.conversation.config.proposal_expiration;
        let consensus_timeout = self.conversation.config.consensus_timeout;
        let liveness_criteria_yes = self.conversation.config.liveness_criteria_yes;
        let voting_delay = self.conversation.config.voting_delay_for(kind);
        let consensus = self.consensus.clone();
        let conversation_id = self.conversation_id.clone();
        let self_member_id = Arc::clone(&self.self_member_id);

        let (proposal_id, unbundled) = submit_proposal::<P>(
            &conversation_id,
            &request,
            &self_member_id,
            &consensus,
            ProposalParams {
                expected_voters,
                proposal_expiration,
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
        // Register the consensus timeout deadline. The caller's polling
        // loop fires `resolve_on_timeout` via `tick_deadlines` once the
        // deadline elapses; the entry is removed naturally on
        // `apply_consensus_outcome` if consensus resolves first.
        self.register_consensus_timeout(proposal_id, consensus_timeout);

        match creator_vote {
            CreatorVote::Yes => {
                // Bundled path: the creator's vote goes on the wire with the
                // proposal as one atomic broadcast. Use the consensus
                // library's owner-bundling API directly — the normal
                // `cast_vote` helper sends Vote-only messages, which would
                // leave peers without the proposal.
                let scope = P::Scope::from(conversation_id.clone());
                let proposal = consensus.cast_vote_and_get_proposal(&scope, proposal_id, true)?;
                info!(
                    conversation = %conversation_id,
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
                // Creator already voted — populate history cache, no vote request.
                self.emit_event(SessionEvent::OwnProposalSubmitted {
                    proposal_id,
                    request,
                });
            }
            CreatorVote::Deferred => {
                // Unbundled path: broadcast the proposal alone. Surface the
                // vote request to the creator like any peer and start their
                // own auto-vote timer; peers run their own timers locally.
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

        Ok(proposal_id)
    }

    /// Resolve the proposal via the consensus library's timeout path if it's
    /// still in the active set.
    ///
    /// The `still_active` guard eliminates the normal case where the session
    /// has already resolved by the time the timer fires. A race can still slip
    /// through (session resolved between the guard and the call); a
    /// `SessionNotFound`/`SessionNotActive` in that window is benign as long
    /// as the proposal is in our resolved-proposals cache — we downgrade the
    /// log accordingly and warn only for truly unknown IDs (indicates a logic
    /// bug, not a race).
    fn resolve_on_timeout(&self, proposal_id: u32) {
        let consensus = self.consensus.clone();
        let conversation_id = self.conversation_id.clone();
        let scope = P::Scope::from(conversation_id.clone());
        let still_active = consensus
            .storage()
            .get_active_proposals(&scope)
            .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
            .unwrap_or(false);
        if !still_active {
            return;
        }
        match consensus.handle_consensus_timeout(&scope, proposal_id) {
            Ok(_) => {}
            Err(ConsensusError::SessionNotFound) | Err(ConsensusError::SessionNotActive) => {
                let resolved_locally = self
                    .conversation
                    .queues
                    .is_consensus_outcome_applied(proposal_id);
                if resolved_locally {
                    tracing::debug!(
                        conversation = %conversation_id,
                        proposal_id,
                        "timeout fired for already-resolved proposal: ignoring"
                    );
                } else {
                    tracing::warn!(
                        conversation = %conversation_id,
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

    /// Cast the auto-vote on behalf of the local member. Same broadcast
    /// path as a manual vote — the library sees the two identically.
    fn cast_auto_vote(&mut self, proposal_id: u32, vote: bool) -> Result<(), SessionError> {
        let consensus = self.consensus.clone();
        let conversation_id = self.conversation_id.clone();
        let app_message = cast_vote::<P>(&conversation_id, proposal_id, vote, &consensus)?;
        let payload = self
            .conversation
            .expect_mls_mut()?
            .build_message(&app_message)?;
        self.broadcast(payload);
        Ok(())
    }
}
