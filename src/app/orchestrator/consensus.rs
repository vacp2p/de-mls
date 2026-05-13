//! Proposal submission + voting.
//!
//! Outgoing proposals run as a background task: submit to consensus, register
//! ownership, broadcast (bundled or unbundled per `creator_vote`), resolve on
//! timeout. Since `User` is `Clone` (every field is `Arc` or cheap `Clone`),
//! each spawn just owns its own handle — no ctx struct per task kind.

use std::{sync::Arc, time::Duration};

use hashgraph_like_consensus::{error::ConsensusError, storage::ConsensusStorage};
use prost::Message;
use tracing::{error, info};

use crate::{
    app::{
        ConversationPlugins, ConversationState, ProposalParams, User, UserError, cast_vote,
        submit_proposal,
    },
    core::{
        ConversationEventHandler, DeMlsProvider, ProposalKind, StewardListPlugin,
        target_identity_of,
    },
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest, VotePayload},
};

/// Per-call arguments for a new outgoing proposal. Bundled so the spawned
/// task has one typed payload instead of five captures.
///
/// `creator_vote` is `Some(v)` when the creator's vote is known up front
/// (user-initiated path where the user picked, or self-executing protocol
/// action) and should be bundled into the outbound proposal. `None` means
/// "broadcast the unbundled proposal and let the creator vote via the
/// normal banner like any other member" — used for steward auto-propose
/// paths where the steward still holds a judgement call.
struct NewProposal {
    conversation_name: String,
    request: ConversationUpdateRequest,
    expected_voters: u32,
    kind: ProposalKind,
    creator_vote: Option<bool>,
}

use crate::mls_crypto::MlsService;

impl<P: DeMlsProvider, GP: ConversationPlugins, H: ConversationEventHandler + 'static>
    User<P, GP, H>
{
    /// Check that the conversation state allows creating a proposal of this
    /// kind and return the expected voter count.
    async fn check_proposal_allowed(
        &self,
        conversation_name: &str,
        kind: ProposalKind,
    ) -> Result<u32, UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let entry = entry_arc.read().await;
        let state = entry.handle.current_state();

        match state {
            ConversationState::Reelection => {
                if !kind.is_emergency() && !kind.is_steward_election() {
                    return Err(UserError::ConversationBlocked(state.to_string()));
                }
                if entry.handle.conversation.partial_freeze_blocks(kind) {
                    return Err(UserError::PartialFreeze);
                }
            }
            ConversationState::Freezing | ConversationState::Selection => {
                return Err(UserError::ConversationBlocked(state.to_string()));
            }
            _ => {
                if entry.handle.conversation.partial_freeze_blocks(kind) {
                    return Err(UserError::PartialFreeze);
                }
            }
        }

        entry.handle.expect_mls()?;
        let members = entry.handle.conversation_members()?;
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
        &self,
        conversation_name: String,
        request: ConversationUpdateRequest,
        creator_vote: Option<bool>,
    ) -> Result<(), UserError> {
        let kind = ProposalKind::of(&request);
        let expected_voters = self
            .check_proposal_allowed(&conversation_name, kind)
            .await?;
        self.spawn_proposal_submission(NewProposal {
            conversation_name,
            request,
            expected_voters,
            kind,
            creator_vote,
        });
        Ok(())
    }

    fn spawn_proposal_submission(&self, np: NewProposal) {
        let user = self.clone();
        tokio::spawn(async move { user.run_proposal_lifecycle(np).await });
    }

    /// Proposal background task: register (which broadcasts proposal +
    /// creator's vote in one bundle) → sleep `consensus_timeout` → resolve.
    /// Never returns an error — submission failures are surfaced through
    /// `ConversationEventHandler::on_error` and everything after that is best-effort.
    async fn run_proposal_lifecycle(self, np: NewProposal) {
        let NewProposal {
            conversation_name,
            request,
            expected_voters,
            kind,
            creator_vote,
        } = np;

        let proposal_id = match self
            .register_new_proposal(
                &conversation_name,
                request,
                expected_voters,
                kind,
                creator_vote,
            )
            .await
        {
            Ok(pid) => pid,
            Err(err) => {
                error!(conversation = %conversation_name, error = %err, "proposal submission failed");
                self.handler
                    .on_error(&conversation_name, "Start voting", &err.to_string())
                    .await;
                return;
            }
        };

        let Some(consensus_timeout) = self
            .with_entry(&conversation_name, |e| e.handle.config.consensus_timeout)
            .await
        else {
            return;
        };
        tokio::time::sleep(consensus_timeout).await;
        self.resolve_on_timeout(&conversation_name, proposal_id)
            .await;
    }

    /// Open the consensus session, record ownership, then either bundle
    /// the creator's vote or broadcast unbundled depending on
    /// `creator_vote`. Always notifies our own UI — via
    /// `on_own_proposal_submitted` when bundled (no banner, history
    /// cache only) or via `on_app_message(VotePayload)` when unbundled
    /// (banner shows, same path peers use).
    ///
    /// Ownership is stored *before* the vote is cast, so a single-voter
    /// consensus transition can't race `is_owner=false` when the event
    /// forwarder picks it up.
    async fn register_new_proposal(
        &self,
        conversation_name: &str,
        request: ConversationUpdateRequest,
        expected_voters: u32,
        kind: ProposalKind,
        creator_vote: Option<bool>,
    ) -> Result<u32, UserError> {
        let (proposal_expiration, consensus_timeout, liveness_criteria_yes, voting_delay) = self
            .with_entry(conversation_name, |e| {
                (
                    e.handle.config.proposal_expiration,
                    e.handle.config.consensus_timeout,
                    e.handle.config.liveness_criteria_yes,
                    e.handle.config.voting_delay_for(kind),
                )
            })
            .await
            .ok_or(UserError::ConversationNotFound)?;

        let (proposal_id, unbundled) = submit_proposal::<P>(
            conversation_name,
            &request,
            self.self_identity(),
            &self.consensus_service,
            ProposalParams {
                expected_voters,
                proposal_expiration,
                consensus_timeout,
                liveness_criteria_yes,
            },
        )
        .await?;

        {
            let entry_arc = self
                .lookup_entry(conversation_name)
                .await
                .ok_or(UserError::ConversationNotFound)?;
            let mut entry = entry_arc.write().await;
            entry
                .handle
                .conversation
                .store_voting_proposal(proposal_id, request.clone());
            if kind.is_emergency() {
                entry.handle.conversation.observe_emergency(proposal_id);
            }
        }

        let outbound = match creator_vote {
            Some(vote) => {
                // Bundled path: the creator's vote goes on the wire with the
                // proposal as one atomic broadcast. Use the consensus
                // library's owner-bundling API directly — the normal
                // `cast_vote` helper sends Vote-only messages, which would
                // leave peers without the proposal.
                let scope = P::Scope::from(conversation_name.to_string());
                let proposal = self
                    .consensus_service
                    .cast_vote_and_get_proposal(&scope, proposal_id, vote, self.eth_signer.clone())
                    .await?;
                info!(
                    conversation = conversation_name,
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
            let entry_arc = self
                .lookup_entry(conversation_name)
                .await
                .ok_or(UserError::ConversationNotFound)?;
            let entry = entry_arc.read().await;
            entry
                .handle
                .expect_mls()?
                .build_message(&outbound, &self.app_id)?
        };
        self.handler.on_outbound(conversation_name, packet).await?;

        match creator_vote {
            Some(_) => {
                // Creator already voted — populate history cache, no banner.
                self.handler
                    .on_own_proposal_submitted(conversation_name, proposal_id, &request)
                    .await?;
            }
            None => {
                // Creator hasn't voted — show them the banner like peers
                // and start their own auto-vote timer. Peers receive the
                // proposal via wire and run their own timers locally.
                let payload = request.encode_to_vec();
                let vote_notification: AppMessage = VotePayload {
                    conversation_id: conversation_name.to_string(),
                    proposal_id,
                    payload,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                }
                .into();
                self.handler
                    .on_app_message(conversation_name, vote_notification)
                    .await?;
                self.spawn_auto_vote(
                    conversation_name,
                    proposal_id,
                    voting_delay,
                    liveness_criteria_yes,
                )
                .await;
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
    async fn resolve_on_timeout(&self, conversation_name: &str, proposal_id: u32) {
        let scope = P::Scope::from(conversation_name.to_string());
        let still_active = self
            .consensus_service
            .storage()
            .get_active_proposals(&scope)
            .await
            .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
            .unwrap_or(false);
        if !still_active {
            return;
        }
        match self
            .consensus_service
            .handle_consensus_timeout(&scope, proposal_id)
            .await
        {
            Ok(_) => {}
            Err(ConsensusError::SessionNotFound) | Err(ConsensusError::SessionNotActive) => {
                let resolved_locally = self
                    .with_entry(conversation_name, |entry| {
                        entry
                            .handle
                            .conversation
                            .is_consensus_outcome_applied(proposal_id)
                    })
                    .await
                    .unwrap_or(false);
                if resolved_locally {
                    tracing::debug!(
                        conversation = conversation_name,
                        proposal_id,
                        "timeout fired for already-resolved proposal: ignoring"
                    );
                } else {
                    tracing::warn!(
                        conversation = conversation_name,
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
        &self,
        conversation_name: &str,
        request: ConversationUpdateRequest,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;

        let (pending_join, members_for_rotation, current_epoch) = {
            let entry = entry_arc.read().await;
            let pending = entry.handle.current_state() == ConversationState::PendingJoin;
            match (pending, entry.handle.mls()) {
                (true, _) | (false, None) => (pending, Vec::new(), 0u64),
                (false, Some(mls)) => (
                    false,
                    entry.handle.conversation_members()?,
                    mls.current_epoch()?,
                ),
            }
        };
        if pending_join {
            return Ok(());
        }

        let (inserted, is_epoch_steward, state, buffer_total, should_propose) = {
            let mut entry = entry_arc.write().await;

            // Defensive — core only emits membership changes here.
            if target_identity_of(&request).is_none() {
                return Ok(());
            }

            let inserted = entry
                .handle
                .conversation
                .buffer_pending_update(request.clone(), current_epoch);

            // Only the epoch steward proposes immediately. The buffer
            // survives freeze rounds so a later steward can retry.
            let self_identity = self.self_identity();
            let eligible = entry
                .handle
                .conversation
                .steward_eligibility(&members_for_rotation);
            let is_es = entry
                .handle
                .steward_list
                .epoch_steward(current_epoch, &eligible)
                .is_some_and(|es| es == self_identity);
            let state = entry.handle.current_state();
            let total = entry.handle.conversation.pending_update_count();
            let should = is_es && state == ConversationState::Working;
            (inserted, is_es, state, total, should)
        };

        info!(
            conversation = conversation_name,
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
            if let Err(e) = self
                .initiate_proposal(conversation_name.to_string(), request, None)
                .await
            {
                info!(conversation = conversation_name, error = %e, "proposal deferred");
            }
        }
        Ok(())
    }

    pub async fn process_user_vote(
        &mut self,
        conversation_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        {
            let entry = entry_arc.read().await;
            let state = entry.handle.current_state();
            if state == ConversationState::Freezing || state == ConversationState::Selection {
                return Err(UserError::ConversationBlocked(state.to_string()));
            }
        }
        let app_id = self.app_id.clone();

        // Manual vote takes precedence over the pending auto-vote timer.
        self.cancel_auto_vote(conversation_name, proposal_id).await;

        let app_message = cast_vote::<P, _>(
            conversation_name,
            proposal_id,
            vote,
            &self.consensus_service,
            self.eth_signer.clone(),
        )
        .await?;
        let packet = {
            let entry = entry_arc.read().await;
            entry
                .handle
                .expect_mls()?
                .build_message(&app_message, &app_id)?
        };
        self.handler.on_outbound(conversation_name, packet).await?;
        Ok(())
    }

    /// Spawn an auto-vote timer for `(conversation_name, proposal_id)`. Idempotent
    /// — an existing handle for the same key is aborted and replaced.
    /// `vote` is captured before the sleep so a `ConversationSync` during the
    /// delay can't change it.
    pub(crate) async fn spawn_auto_vote(
        &self,
        conversation_name: &str,
        proposal_id: u32,
        delay: Duration,
        vote: bool,
    ) {
        let timers = match self.lookup_entry(conversation_name).await {
            Some(entry_arc) => entry_arc.read().await.auto_vote_timers.clone(),
            None => return,
        };

        // Idempotent: abort any existing handle for this proposal.
        if let Ok(mut t) = timers.lock()
            && let Some(old) = t.remove(&proposal_id)
        {
            old.abort();
        }

        let user = self.clone();
        let cname_owned = conversation_name.to_string();
        let timers_for_task = Arc::clone(&timers);
        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(e) = user.cast_auto_vote(&cname_owned, proposal_id, vote).await {
                tracing::debug!(
                    conversation = %cname_owned,
                    proposal_id,
                    error = %e,
                    "auto-vote skipped (already voted or session resolved)"
                );
            } else {
                info!(
                    conversation = %cname_owned,
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

    /// Abort the auto-vote timer for `(conversation_name, proposal_id)` if one is
    /// registered. No-op otherwise.
    pub(crate) async fn cancel_auto_vote(&self, conversation_name: &str, proposal_id: u32) {
        if let Some(entry_arc) = self.lookup_entry(conversation_name).await {
            entry_arc.read().await.cancel_auto_vote(proposal_id);
        }
    }

    /// Abort every auto-vote timer belonging to `conversation_name`. Called on
    /// conversation leave so no stale timers fire against a conversation we've left.
    pub(crate) async fn cancel_conversation_auto_votes(&self, conversation_name: &str) {
        if let Some(entry_arc) = self.lookup_entry(conversation_name).await {
            entry_arc.read().await.cancel_all_auto_votes();
        }
    }

    /// Cast the auto-vote on behalf of the local member. Same broadcast
    /// path as a manual vote — the library sees the two identically.
    async fn cast_auto_vote(
        &self,
        conversation_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let app_message = cast_vote::<P, _>(
            conversation_name,
            proposal_id,
            vote,
            &self.consensus_service,
            self.eth_signer.clone(),
        )
        .await?;
        let packet = {
            let entry = entry_arc.read().await;
            entry
                .handle
                .expect_mls()?
                .build_message(&app_message, &self.app_id)?
        };
        self.handler.on_outbound(conversation_name, packet).await?;
        Ok(())
    }
}
