//! Proposal submission + voting on `SessionRunner`.
//!
//! Outgoing proposals run as a background task: submit to consensus, register
//! ownership, broadcast (bundled or unbundled per `creator_vote`), resolve on
//! timeout. The spawn helpers take `Arc<RwLock<SessionRunner>>` so the task
//! body can release the runner lock across `.await` points without holding
//! it during the consensus timeout sleep.
//!
//! Public entries are also exposed as transient `User` wrappers that look up
//! the session and delegate. Wave 8 drops the wrappers once external callers
//! switch to using sessions directly.

use std::{sync::Arc, time::Duration};

use hashgraph_like_consensus::{error::ConsensusError, storage::ConsensusStorage};
use prost::Message;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::{
    app::{
        ConversationState, ProposalParams, SessionRunner, User, UserError, cast_vote,
        submit_proposal,
    },
    core::{
        ConsensusPlugin, ConversationPluginsFactory, ProposalKind, SessionEvent, StewardListPlugin,
        target_identity_of,
    },
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest, VotePayload},
};

/// Per-call arguments for a new outgoing proposal. Bundled so the spawned
/// task has one typed payload instead of four captures.
///
/// `creator_vote` is `Some(v)` when the creator's vote is known up front
/// (user-initiated path where the user picked, or self-executing protocol
/// action) and should be bundled into the outbound proposal. `None` means
/// "broadcast the unbundled proposal and let the creator vote via the
/// normal banner like any other member" — used for steward auto-propose
/// paths where the steward still holds a judgement call.
struct NewProposal {
    request: ConversationUpdateRequest,
    expected_voters: u32,
    kind: ProposalKind,
    creator_vote: Option<bool>,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Check that the conversation state allows creating a proposal of this
    /// kind and return the expected voter count.
    async fn check_proposal_allowed(&self, kind: ProposalKind) -> Result<u32, UserError> {
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

    /// Start a consensus vote for a conversation update request.
    ///
    /// `creator_vote` captures the creator's intent:
    /// - `Some(v)` — bundle `v` into the outbound proposal at submit time.
    ///   Used when the intent is unambiguous: user clicked a button that
    ///   means YES (ban request), or the action is self-executing
    ///   (`SCORE_BELOW_THRESHOLD` removal).
    /// - `None` — broadcast the proposal unbundled; the creator's UI
    ///   shows the usual vote banner and the creator votes like any
    ///   other member. Silence is tallied per `liveness_criteria_yes`
    ///   at consensus timeout, same as for peer silence. Used for
    ///   steward auto-propose paths (election, incoming KP, buffered
    ///   update) where the creator still exercises judgement.
    pub async fn initiate_proposal(
        arc: &Arc<RwLock<Self>>,
        request: ConversationUpdateRequest,
        creator_vote: Option<bool>,
    ) -> Result<(), UserError> {
        let kind = ProposalKind::of(&request);
        let expected_voters = arc.read().await.check_proposal_allowed(kind).await?;
        Self::spawn_proposal_submission(
            arc,
            NewProposal {
                request,
                expected_voters,
                kind,
                creator_vote,
            },
        );
        Ok(())
    }

    fn spawn_proposal_submission(arc: &Arc<RwLock<Self>>, np: NewProposal) {
        let arc = Arc::clone(arc);
        tokio::spawn(async move { Self::run_proposal_lifecycle(arc, np).await });
    }

    /// Proposal background task: register (which broadcasts proposal +
    /// creator's vote in one bundle) → sleep `consensus_timeout` → resolve.
    /// Never returns an error — submission failures are surfaced through
    /// `SessionEvent::Error` and everything after that is best-effort.
    async fn run_proposal_lifecycle(arc: Arc<RwLock<Self>>, np: NewProposal) {
        let proposal_id = match Self::register_new_proposal(&arc, np).await {
            Ok(pid) => pid,
            Err(err) => {
                let s = arc.read().await;
                error!(conversation = %s.conversation_name, error = %err, "proposal submission failed");
                s.emit_event(SessionEvent::Error {
                    operation: "Start voting".to_string(),
                    message: err.to_string(),
                });
                return;
            }
        };

        let consensus_timeout = arc.read().await.handle.config.consensus_timeout;
        tokio::time::sleep(consensus_timeout).await;
        Self::resolve_on_timeout(&arc, proposal_id).await;
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
            let s = arc.read().await;
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
            let mut s = arc.write().await;
            s.handle
                .conversation
                .store_voting_proposal(proposal_id, request.clone());
            if kind.is_emergency() {
                s.handle.conversation.observe_emergency(proposal_id);
            }
        }

        let outbound = match creator_vote {
            Some(vote) => {
                // Bundled path: the creator's vote goes on the wire with the
                // proposal as one atomic broadcast. Use the consensus
                // library's owner-bundling API directly — the normal
                // `cast_vote` helper sends Vote-only messages, which would
                // leave peers without the proposal.
                let scope = P::Scope::from(conversation_name.clone());
                let proposal = consensus
                    .cast_vote_and_get_proposal(&scope, proposal_id, vote)
                    .await?;
                info!(
                    conversation = %conversation_name,
                    proposal_id,
                    choice = if vote { "YES" } else { "NO" },
                    actor = "owner",
                    "vote cast (bundled at submit)"
                );
                proposal.into()
            }
            None => unbundled,
        };

        let packet = {
            let s = arc.read().await;
            s.handle.expect_mls()?.build_message(&outbound, &s.app_id)?
        };
        arc.read().await.send_outbound(packet).await?;

        match creator_vote {
            Some(_) => {
                // Creator already voted — populate history cache, no banner.
                arc.read()
                    .await
                    .emit_event(SessionEvent::OwnProposalSubmitted {
                        proposal_id,
                        request: request.clone(),
                    });
            }
            None => {
                // Creator hasn't voted — show them the banner like peers
                // and start their own auto-vote timer. Peers receive the
                // proposal via wire and run their own timers locally.
                let payload = request.encode_to_vec();
                let vote_notification: AppMessage = VotePayload {
                    conversation_id: conversation_name.clone(),
                    proposal_id,
                    payload,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                }
                .into();
                arc.read()
                    .await
                    .emit_event(SessionEvent::AppMessage(vote_notification));
                Self::spawn_auto_vote(arc, proposal_id, voting_delay, liveness_criteria_yes).await;
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
        let (consensus, conversation_name) = {
            let s = arc.read().await;
            (s.consensus.clone(), s.conversation_name.clone())
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
                    .read()
                    .await
                    .handle
                    .conversation
                    .is_consensus_outcome_applied(proposal_id);
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

    /// Handle an incoming membership update (KP-derived `InviteMember` or
    /// `RemoveMember`): buffer it so every member has a durable record, then
    /// promote it to a voting proposal if this node is the current epoch
    /// steward and the conversation accepts new proposals.
    pub async fn handle_incoming_update_request(
        arc: &Arc<RwLock<Self>>,
        request: ConversationUpdateRequest,
    ) -> Result<(), UserError> {
        let (pending_join, members_for_rotation, current_epoch) = {
            let s = arc.read().await;
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
            let mut s = arc.write().await;

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
            if let Err(e) = Self::initiate_proposal(arc, request, None).await {
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
            let s = arc.read().await;
            let state = s.handle.current_state();
            if state == ConversationState::Freezing || state == ConversationState::Selection {
                return Err(UserError::ConversationBlocked(state.to_string()));
            }
            (s.consensus.clone(), s.conversation_name.clone())
        };

        // Manual vote takes precedence over the pending auto-vote timer.
        arc.read().await.cancel_auto_vote(proposal_id);

        let app_message = cast_vote::<P>(&conversation_name, proposal_id, vote, &consensus).await?;
        let packet = {
            let s = arc.read().await;
            s.handle
                .expect_mls()?
                .build_message(&app_message, &s.app_id)?
        };
        arc.read().await.send_outbound(packet).await?;
        Ok(())
    }

    /// Spawn an auto-vote timer for `proposal_id`. Idempotent — an existing
    /// handle for the same key is aborted and replaced. `vote` is captured
    /// before the sleep so a `ConversationSync` during the delay can't
    /// change it.
    pub(crate) async fn spawn_auto_vote(
        arc: &Arc<RwLock<Self>>,
        proposal_id: u32,
        delay: Duration,
        vote: bool,
    ) {
        let (timers, conversation_name) = {
            let s = arc.read().await;
            (s.auto_vote_timers.clone(), s.conversation_name.clone())
        };

        // Idempotent: abort any existing handle for this proposal.
        if let Ok(mut t) = timers.lock()
            && let Some(old) = t.remove(&proposal_id)
        {
            old.abort();
        }

        let arc_for_task = Arc::clone(arc);
        let conv_name_for_log = conversation_name.clone();
        let timers_for_task = Arc::clone(&timers);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(e) = Self::cast_auto_vote(&arc_for_task, proposal_id, vote).await {
                tracing::debug!(
                    conversation = %conv_name_for_log,
                    proposal_id,
                    error = %e,
                    "auto-vote skipped (already voted or session resolved)"
                );
            } else {
                info!(
                    conversation = %conv_name_for_log,
                    proposal_id,
                    vote,
                    "auto-vote cast on timer"
                );
            }
            // Self-clean so repeated manual votes after the timer fires
            // don't try to abort a dead handle.
            if let Ok(mut t) = timers_for_task.lock() {
                t.remove(&proposal_id);
            }
        });

        if let Ok(mut t) = timers.lock() {
            t.insert(proposal_id, handle);
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
            let s = arc.read().await;
            (s.consensus.clone(), s.conversation_name.clone())
        };
        let app_message = cast_vote::<P>(&conversation_name, proposal_id, vote, &consensus).await?;
        let packet = {
            let s = arc.read().await;
            s.handle
                .expect_mls()?
                .build_message(&app_message, &s.app_id)?
        };
        arc.read().await.send_outbound(packet).await?;
        Ok(())
    }
}

// ── Transient User wrappers ─────────────────────────────────────────────
//
// Each wrapper looks up the session and forwards to the session method.
// They exist so existing User-level callers (steward.rs, inbound.rs,
// messaging.rs::process_ban_request, gateway, ui_bridge) keep compiling
// during the wave-by-wave migration. Wave 8 drops the wrappers and
// updates external callers to use sessions directly.

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    pub async fn initiate_proposal(
        &self,
        conversation_name: String,
        request: ConversationUpdateRequest,
        creator_vote: Option<bool>,
    ) -> Result<(), UserError> {
        let session = self
            .lookup_entry(&conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::initiate_proposal(&session, request, creator_vote).await
    }

    pub async fn handle_incoming_update_request(
        &self,
        conversation_name: &str,
        request: ConversationUpdateRequest,
    ) -> Result<(), UserError> {
        let session = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::handle_incoming_update_request(&session, request).await
    }

    pub async fn process_user_vote(
        &mut self,
        conversation_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let session = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        SessionRunner::process_user_vote(&session, proposal_id, vote).await
    }

    /// Abort every auto-vote timer belonging to `conversation_name`. Called
    /// on conversation leave so no stale timers fire against a conversation
    /// we've left.
    pub(crate) async fn cancel_conversation_auto_votes(&self, conversation_name: &str) {
        if let Some(entry_arc) = self.lookup_entry(conversation_name).await {
            entry_arc.read().await.cancel_all_auto_votes();
        }
    }
}
