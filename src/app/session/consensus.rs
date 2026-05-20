//! Proposal submission + voting on `SessionRunner`.
//!
//! Outgoing proposals run as a background task: submit to consensus, register
//! ownership, broadcast (bundled or unbundled per `creator_vote`), resolve on
//! timeout. The spawn helpers take `Arc<RwLock<SessionRunner>>` so the task
//! body can release the runner lock across `.await` points without holding
//! it during the consensus timeout sleep.

use std::sync::{Arc, RwLock};

use hashgraph_like_consensus::{error::ConsensusError, storage::ConsensusStorage};
use prost::Message;
use tracing::{error, info};

use crate::{
    app::{
        ConversationState, LockExt, SessionRunner, UserError,
        session::{
            consensus_bridge::{
                ProposalParams, cast_vote, submit_proposal, submit_self_leave_proposal,
            },
            runner::send_packet,
        },
    },
    core::{
        ConsensusPlugin, ConversationPluginsFactory, ProposalKind, SessionEvent, StewardListPlugin,
        self_leave_proposal_id, target_identity_of,
    },
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationUpdateRequest, RemoveMember, VotePayload,
        conversation_update_request,
    },
};

/// The creator's intent at proposal submit time. Controls both the wire
/// shape and the local UI flow — see [`SessionRunner::initiate_proposal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatorVote {
    /// Creator commits YES at submit. Vote is bundled with the proposal
    /// in one atomic wire message; local UI gets `OwnProposalSubmitted`
    /// (no banner). Used for unambiguous actions: ban-button click,
    /// self-executing protocol moves (`SCORE_BELOW_THRESHOLD`, `Deadlock`).
    Yes,
    /// Creator hasn't decided. Broadcast unbundled; local UI banners
    /// alongside the auto-vote timer (`liveness_criteria_yes` after
    /// `voting_delay_for`). Used for steward auto-propose paths where
    /// the steward still exercises judgement.
    Deferred,
}

/// Typed payload for the spawned proposal lifecycle.
struct NewProposal {
    request: ConversationUpdateRequest,
    expected_voters: u32,
    kind: ProposalKind,
    creator_vote: CreatorVote,
}

/// Build the `AppMessage` carrying a `VotePayload` for banner display in
/// the local UI. Used both by the creator's `Deferred` submit path
/// (own proposal) and by peers receiving an unbundled `Proposal` over the
/// wire — both render the same banner.
pub(crate) fn build_vote_banner_event(
    conversation_name: &str,
    proposal_id: u32,
    payload: Vec<u8>,
) -> AppMessage {
    VotePayload {
        conversation_id: conversation_name.to_string(),
        proposal_id,
        payload,
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    }
    .into()
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
    pub async fn initiate_proposal(
        arc: &Arc<RwLock<Self>>,
        request: ConversationUpdateRequest,
        creator_vote: CreatorVote,
    ) -> Result<(), UserError> {
        let kind = ProposalKind::of(&request);
        let expected_voters = arc.read_or_err("session")?.check_proposal_allowed(kind)?;
        Self::register_new_proposal(
            arc,
            NewProposal {
                request,
                expected_voters,
                kind,
                creator_vote,
            },
        )
        .await?;
        Ok(())
    }

    /// Handle an incoming membership update (KP-derived `InviteMember` or
    /// `RemoveMember`): buffer it so every member has a durable record, then
    /// promote it to a voting proposal if this node is the current epoch
    /// steward and the conversation accepts new proposals.
    pub async fn handle_incoming_update_request(
        arc: &Arc<RwLock<Self>>,
        request: ConversationUpdateRequest,
    ) -> Result<(), UserError> {
        let (pending_join, members_for_rotation, current_epoch) = {
            let s = arc.read_or_err("session")?;
            let pending = s.handle.current_state() == ConversationState::PendingJoin;
            match (pending, s.handle.mls()) {
                (true, _) | (false, None) => (pending, Vec::new(), 0u64),
                (false, Some(mls)) => (
                    false,
                    s.handle.conversation_members()?,
                    mls.current_epoch()?,
                ),
            }
        };
        if pending_join {
            return Ok(());
        }

        let (inserted, is_epoch_steward, state, buffer_total, should_propose, conversation_name) = {
            let mut s = arc.write_or_err("session")?;

            // Defensive — core only emits membership changes here.
            if target_identity_of(&request).is_none() {
                return Ok(());
            }

            let inserted = s
                .handle
                .conversation
                .buffer_pending_update(request.clone(), current_epoch);

            // Only the epoch steward proposes immediately. The buffer
            // survives freeze rounds so a later steward can retry.
            let self_identity = Arc::clone(&s.self_identity);
            let eligible = s
                .handle
                .conversation
                .steward_eligibility(&members_for_rotation);
            let is_es = s
                .handle
                .steward_list
                .epoch_steward(current_epoch, &eligible)
                .is_some_and(|es| es == &*self_identity);
            let state = s.handle.current_state();
            let total = s.handle.conversation.pending_update_count();
            let should = is_es && state == ConversationState::Working;
            let name = s.conversation_name.clone();
            (inserted, is_es, state, total, should, name)
        };

        info!(
            conversation = %conversation_name,
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
            // let the banner drive the steward's vote like any other member.
            // `check_proposal_allowed` may still reject (active emergency
            // etc.) — leave the entry in the buffer for next rotation.
            if let Err(e) = Self::initiate_proposal(arc, request, CreatorVote::Deferred).await {
                info!(conversation = %conversation_name, error = %e, "proposal deferred");
            }
        }
        Ok(())
    }

    /// Cast a manual vote on behalf of the local member. Blocked in
    /// `Freezing` and `Selection`; cancels any pending auto-vote so the
    /// manual choice wins.
    pub async fn process_user_vote(
        arc: &Arc<RwLock<Self>>,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let (consensus, conversation_name) = {
            let s = arc.read_or_err("session")?;
            let state = s.handle.current_state();
            if state == ConversationState::Freezing || state == ConversationState::Selection {
                return Err(UserError::ConversationBlocked(state.to_string()));
            }
            (s.consensus.clone(), s.conversation_name.clone())
        };

        // Manual vote takes precedence over the pending auto-vote timer.
        arc.write_or_err("session")?.cancel_auto_vote(proposal_id);

        let app_message = cast_vote::<P>(&conversation_name, proposal_id, vote, &consensus).await?;
        let packet = {
            let mut s = arc.write_or_err("session")?;
            let app_id = Arc::clone(&s.app_id);
            s.handle
                .expect_mls_mut()?
                .build_message(&app_message, &app_id)?
        };
        let transport = Arc::clone(arc.read_or_err("session")?.transport());
        send_packet(&transport, packet)?;
        Ok(())
    }

    /// Walk pending deadlines and fire any whose `fire_at` has elapsed.
    /// Call from the caller's polling loop. Drains entries synchronously
    /// under a brief write guard, then awaits the async fire (consensus
    /// call or vote cast + publish) without holding the lock.
    pub async fn tick_deadlines(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let now = std::time::Instant::now();
        let (auto_votes_due, timeouts_due) = {
            let mut s = arc.write_or_err("session")?;
            let auto_votes: Vec<(u32, bool)> = s
                .pending_auto_votes
                .iter()
                .filter(|(_, e)| e.fire_at <= now)
                .map(|(id, e)| (*id, e.vote))
                .collect();
            for (id, _) in &auto_votes {
                s.pending_auto_votes.remove(id);
            }
            let timeouts: Vec<u32> = s
                .pending_consensus_timeouts
                .iter()
                .filter(|(_, fire_at)| **fire_at <= now)
                .map(|(id, _)| *id)
                .collect();
            for id in &timeouts {
                s.pending_consensus_timeouts.remove(id);
            }
            (auto_votes, timeouts)
        };

        for (proposal_id, vote) in auto_votes_due {
            if let Err(e) = Self::cast_auto_vote(arc, proposal_id, vote).await {
                tracing::debug!(
                    proposal_id,
                    error = %e,
                    "auto-vote skipped (already voted or session resolved)"
                );
            }
        }
        for proposal_id in timeouts_due {
            Self::resolve_on_timeout(arc, proposal_id).await;
        }

        Ok(())
    }

    // ── Crate-internal ───────────────────────────────────────────────

    /// Open a self-leave consensus session: `RemoveMember(self)` with
    /// `expected_voters = 1` and the leaver's YES bundled. Resolves
    /// synchronously, so the normal `apply_consensus_outcome` path commits
    /// the removal on the next steward commit. Idempotent — a second call
    /// after a successful submit short-circuits on the local pending-leave
    /// check, and a retransmit dedupes inside the consensus library via
    /// the deterministic [`self_leave_proposal_id`].
    pub(crate) async fn initiate_self_leave(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let self_identity = Arc::clone(&arc.read_or_err("session")?.self_identity);

        let (already_pending, conversation_name) = {
            let s = arc.read_or_err("session")?;
            (
                s.handle.conversation.is_pending_self_leave(&self_identity),
                s.conversation_name.clone(),
            )
        };
        if already_pending {
            info!(
                conversation = %conversation_name,
                "self-leave already in flight, ignoring duplicate"
            );
            return Ok(());
        }

        let request = ConversationUpdateRequest {
            payload: Some(conversation_update_request::Payload::RemoveMember(
                RemoveMember {
                    identity: self_identity.to_vec(),
                },
            )),
        };
        let proposal_id = self_leave_proposal_id(&self_identity);

        // Register ownership BEFORE the session opens — the bundled YES
        // fires `ConsensusReached` synchronously, and
        // `apply_consensus_outcome` needs `is_owner_of_proposal` to be true
        // by then.
        let (consensus, proposal_expiration, consensus_timeout) = {
            let mut s = arc.write_or_err("session")?;
            s.handle
                .conversation
                .store_voting_proposal(proposal_id, request);
            (
                s.consensus.clone(),
                s.handle.config.proposal_expiration,
                s.handle.config.consensus_timeout,
            )
        };

        let submitted = submit_self_leave_proposal::<P>(
            &conversation_name,
            &self_identity,
            &consensus,
            ProposalParams {
                expected_voters: 1,
                proposal_expiration,
                consensus_timeout,
                liveness_criteria_yes: true,
            },
        )
        .await?;

        // Dedup (`ProposalAlreadyExist`) — another submit is already driving
        // this proposal_id. Our voting entry resolves on that session.
        let Some((_proposal_id, app_msg)) = submitted else {
            return Ok(());
        };

        let packet = {
            let mut s = arc.write_or_err("session")?;
            let app_id = Arc::clone(&s.app_id);
            s.handle
                .expect_mls_mut()?
                .build_message(&app_msg, &app_id)?
        };
        let transport = Arc::clone(arc.read_or_err("session")?.transport());
        send_packet(&transport, packet)?;
        Ok(())
    }

    // ── Private ──────────────────────────────────────────────────────

    /// Check that the conversation state allows creating a proposal of this
    /// kind and return the expected voter count.
    fn check_proposal_allowed(&self, kind: ProposalKind) -> Result<u32, UserError> {
        let state = self.handle.current_state();

        match state {
            ConversationState::Reelection => {
                if !kind.is_emergency() && !kind.is_steward_election() {
                    return Err(UserError::ConversationBlocked(state.to_string()));
                }
                if self.handle.conversation.partial_freeze_blocks(kind) {
                    return Err(UserError::PartialFreeze);
                }
            }
            ConversationState::Freezing | ConversationState::Selection => {
                return Err(UserError::ConversationBlocked(state.to_string()));
            }
            _ => {
                if self.handle.conversation.partial_freeze_blocks(kind) {
                    return Err(UserError::PartialFreeze);
                }
            }
        }

        self.handle.expect_mls()?;
        let members = self.handle.conversation_members()?;
        Ok(members.len() as u32)
    }

    /// Open the consensus session, record ownership, then either bundle
    /// the creator's vote or broadcast unbundled depending on
    /// `creator_vote`. Always notifies our own UI — via
    /// `OwnProposalSubmitted` when bundled (no banner, history cache
    /// only) or via `AppMessage(VotePayload)` when unbundled (banner
    /// shows, same path peers use).
    ///
    /// Ownership is stored *before* the vote is cast, so a single-voter
    /// consensus transition can't race `is_owner=false` when the event
    /// forwarder picks it up.
    async fn register_new_proposal(
        arc: &Arc<RwLock<Self>>,
        np: NewProposal,
    ) -> Result<u32, UserError> {
        let NewProposal {
            request,
            expected_voters,
            kind,
            creator_vote,
        } = np;

        let (
            proposal_expiration,
            consensus_timeout,
            liveness_criteria_yes,
            voting_delay,
            consensus,
            conversation_name,
            self_identity,
        ) = {
            let s = arc.read_or_err("session")?;
            (
                s.handle.config.proposal_expiration,
                s.handle.config.consensus_timeout,
                s.handle.config.liveness_criteria_yes,
                s.handle.config.voting_delay_for(kind),
                s.consensus.clone(),
                s.conversation_name.clone(),
                Arc::clone(&s.self_identity),
            )
        };

        let (proposal_id, unbundled) = submit_proposal::<P>(
            &conversation_name,
            &request,
            &self_identity,
            &consensus,
            ProposalParams {
                expected_voters,
                proposal_expiration,
                consensus_timeout,
                liveness_criteria_yes,
            },
        )
        .await?;

        {
            let mut s = arc.write_or_err("session")?;
            s.handle
                .conversation
                .store_voting_proposal(proposal_id, request.clone());
            if kind.is_emergency() {
                s.handle.conversation.observe_emergency(proposal_id);
            }
            // Register the consensus timeout deadline. The caller's polling
            // loop fires `resolve_on_timeout` via `tick_deadlines` once the
            // deadline elapses; the entry is removed naturally on
            // `apply_consensus_outcome` if consensus resolves first.
            s.register_consensus_timeout(proposal_id, consensus_timeout);
        }

        match creator_vote {
            CreatorVote::Yes => {
                // Bundled path: the creator's vote goes on the wire with the
                // proposal as one atomic broadcast. Use the consensus
                // library's owner-bundling API directly — the normal
                // `cast_vote` helper sends Vote-only messages, which would
                // leave peers without the proposal.
                let scope = P::Scope::from(conversation_name.clone());
                let proposal = consensus
                    .cast_vote_and_get_proposal(&scope, proposal_id, true)
                    .await?;
                info!(
                    conversation = %conversation_name,
                    proposal_id,
                    actor = "owner",
                    "YES vote cast (bundled at submit)"
                );
                let outbound: AppMessage = proposal.into();
                let packet = {
                    let mut s = arc.write_or_err("session")?;
                    let app_id = Arc::clone(&s.app_id);
                    s.handle
                        .expect_mls_mut()?
                        .build_message(&outbound, &app_id)?
                };
                let transport = Arc::clone(arc.read_or_err("session")?.transport());
                send_packet(&transport, packet)?;
                // Creator already voted — populate history cache, no banner.
                arc.read_or_err("session")?
                    .emit_event(SessionEvent::OwnProposalSubmitted {
                        proposal_id,
                        request,
                    });
            }
            CreatorVote::Deferred => {
                // Unbundled path: broadcast the proposal alone. Show the
                // creator the banner like peers and start their own
                // auto-vote timer; peers run their own timers locally.
                let packet = {
                    let mut s = arc.write_or_err("session")?;
                    let app_id = Arc::clone(&s.app_id);
                    s.handle
                        .expect_mls_mut()?
                        .build_message(&unbundled, &app_id)?
                };
                let transport = Arc::clone(arc.read_or_err("session")?.transport());
                send_packet(&transport, packet)?;
                let banner = build_vote_banner_event(
                    &conversation_name,
                    proposal_id,
                    request.encode_to_vec(),
                );
                arc.read_or_err("session")?
                    .emit_event(SessionEvent::AppMessage(banner));
                arc.write_or_err("session")?.register_auto_vote(
                    proposal_id,
                    voting_delay,
                    liveness_criteria_yes,
                );
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
    async fn resolve_on_timeout(arc: &Arc<RwLock<Self>>, proposal_id: u32) {
        let (consensus, conversation_name) = match arc.read_or_err("session") {
            Ok(s) => (s.consensus.clone(), s.conversation_name.clone()),
            Err(e) => {
                error!(proposal_id, error = %e, "timeout resolution aborted: session lock poisoned");
                return;
            }
        };
        let scope = P::Scope::from(conversation_name.clone());
        let still_active = consensus
            .storage()
            .get_active_proposals(&scope)
            .await
            .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
            .unwrap_or(false);
        if !still_active {
            return;
        }
        match consensus
            .handle_consensus_timeout(&scope, proposal_id)
            .await
        {
            Ok(_) => {}
            Err(ConsensusError::SessionNotFound) | Err(ConsensusError::SessionNotActive) => {
                let resolved_locally = arc
                    .read_or_err("session")
                    .map(|s| {
                        s.handle
                            .conversation
                            .is_consensus_outcome_applied(proposal_id)
                    })
                    .unwrap_or(false);
                if resolved_locally {
                    tracing::debug!(
                        conversation = %conversation_name,
                        proposal_id,
                        "timeout fired for already-resolved proposal: ignoring"
                    );
                } else {
                    tracing::warn!(
                        conversation = %conversation_name,
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
    async fn cast_auto_vote(
        arc: &Arc<RwLock<Self>>,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let (consensus, conversation_name) = {
            let s = arc.read_or_err("session")?;
            (s.consensus.clone(), s.conversation_name.clone())
        };
        let app_message = cast_vote::<P>(&conversation_name, proposal_id, vote, &consensus).await?;
        let packet = {
            let mut s = arc.write_or_err("session")?;
            let app_id = Arc::clone(&s.app_id);
            s.handle
                .expect_mls_mut()?
                .build_message(&app_message, &app_id)?
        };
        let transport = Arc::clone(arc.read_or_err("session")?.transport());
        send_packet(&transport, packet)?;
        Ok(())
    }
}
