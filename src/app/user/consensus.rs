//! Proposal submission + voting.
//!
//! Outgoing proposals run as a background task: submit to consensus, register
//! ownership, broadcast (bundled or unbundled per `creator_vote`), resolve on
//! timeout. Since `User` is `Clone` (every field is `Arc` or cheap `Clone`),
//! each spawn just owns its own handle — no ctx struct per task kind.

use std::time::Duration;

use hashgraph_like_consensus::{error::ConsensusError, storage::ConsensusStorage};
use prost::Message;
use tracing::{error, info};

use crate::{
    app::{
        GroupState, ProposalParams, StateChangeHandler, User, UserError, cast_vote, submit_proposal,
    },
    core::{
        DeMlsProvider, GroupEventHandler, ProposalKind, build_message, group_members,
        target_identity_of,
    },
    protos::de_mls::messages::v1::{AppMessage, GroupUpdateRequest, VotePayload},
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
    group_name: String,
    request: GroupUpdateRequest,
    expected_voters: u32,
    kind: ProposalKind,
    creator_vote: Option<bool>,
}

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Check that the group state allows creating a proposal of this kind and
    /// return the expected voter count.
    ///
    /// `Reelection` allows only ECP + StewardElection (the recovery itself),
    /// further filtered by `partial_freeze_blocks`. `Freezing` / `Selection`
    /// block everything. Other states defer to `partial_freeze_blocks`.
    async fn check_proposal_allowed(
        &self,
        group_name: &str,
        kind: ProposalKind,
    ) -> Result<u32, UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let entry = entry_arc.read().await;
        let state = entry.state_machine.current_state();

        match state {
            GroupState::Reelection => {
                if !kind.is_emergency() && !kind.is_steward_election() {
                    return Err(UserError::GroupBlocked(state.to_string()));
                }
                if entry.group.partial_freeze_blocks(kind) {
                    return Err(UserError::PartialFreeze);
                }
            }
            GroupState::Freezing | GroupState::Selection => {
                return Err(UserError::GroupBlocked(state.to_string()));
            }
            _ => {
                if entry.group.partial_freeze_blocks(kind) {
                    return Err(UserError::PartialFreeze);
                }
            }
        }

        let members = group_members(&entry.group, &self.mls_service)?;
        Ok(members.len() as u32)
    }

    /// Start a consensus vote for a group update request.
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
    ///
    /// Validates the group state synchronously, then spawns a task that
    /// drives the proposal through its lifecycle.
    pub async fn initiate_proposal(
        &self,
        group_name: String,
        request: GroupUpdateRequest,
        creator_vote: Option<bool>,
    ) -> Result<(), UserError> {
        let kind = ProposalKind::of(&request);
        let expected_voters = self.check_proposal_allowed(&group_name, kind).await?;
        self.spawn_proposal_submission(NewProposal {
            group_name,
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
    /// `GroupEventHandler::on_error` and everything after that is best-effort.
    async fn run_proposal_lifecycle(self, np: NewProposal) {
        let NewProposal {
            group_name,
            request,
            expected_voters,
            kind,
            creator_vote,
        } = np;

        let proposal_id = match self
            .register_new_proposal(&group_name, request, expected_voters, kind, creator_vote)
            .await
        {
            Ok(pid) => pid,
            Err(err) => {
                error!(group = %group_name, error = %err, "proposal submission failed");
                self.handler
                    .on_error(&group_name, "Start voting", &err.to_string())
                    .await;
                return;
            }
        };

        let Some(consensus_timeout) = self
            .with_entry(&group_name, |e| e.state_machine.consensus_timeout())
            .await
        else {
            return;
        };
        tokio::time::sleep(consensus_timeout).await;
        self.resolve_on_timeout(&group_name, proposal_id).await;
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
        group_name: &str,
        request: GroupUpdateRequest,
        expected_voters: u32,
        kind: ProposalKind,
        creator_vote: Option<bool>,
    ) -> Result<u32, UserError> {
        let (proposal_expiration, consensus_timeout, liveness_criteria_yes, voting_delay) = self
            .with_entry(group_name, |e| {
                (
                    e.state_machine.proposal_expiration(),
                    e.state_machine.consensus_timeout(),
                    e.group.liveness_criteria_yes(),
                    e.state_machine.voting_delay_for(kind),
                )
            })
            .await
            .ok_or(UserError::GroupNotFound)?;

        let (proposal_id, unbundled) = submit_proposal::<P>(
            group_name,
            &request,
            self.mls_service.wallet_hex(),
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
                .lookup_entry(group_name)
                .await
                .ok_or(UserError::GroupNotFound)?;
            let mut entry = entry_arc.write().await;
            entry
                .group
                .store_voting_proposal(proposal_id, request.clone());
            if kind.is_emergency() {
                entry.group.observe_emergency(proposal_id);
            }
        }

        let outbound = match creator_vote {
            Some(vote) => {
                // Bundled path: the creator's vote goes on the wire with the
                // proposal as one atomic broadcast. Use the consensus
                // library's owner-bundling API directly — the normal
                // `cast_vote` helper sends Vote-only messages, which would
                // leave peers without the proposal.
                let scope = P::Scope::from(group_name.to_string());
                let proposal = self
                    .consensus_service
                    .cast_vote_and_get_proposal(&scope, proposal_id, vote, self.eth_signer.clone())
                    .await?;
                info!(
                    group = group_name,
                    proposal_id,
                    choice = if vote { "YES" } else { "NO" },
                    actor = "owner",
                    "vote cast (bundled at submit)"
                );
                proposal.into()
            }
            None => unbundled,
        };

        let packet = build_message(group_name, &self.mls_service, &outbound, &self.app_id)?;
        self.handler.on_outbound(group_name, packet).await?;

        match creator_vote {
            Some(_) => {
                // Creator already voted — populate history cache, no banner.
                self.handler
                    .on_own_proposal_submitted(group_name, proposal_id, &request)
                    .await?;
            }
            None => {
                // Creator hasn't voted — show them the banner like peers
                // and start their own auto-vote timer. Peers receive the
                // proposal via wire and run their own timers locally.
                let payload = request.encode_to_vec();
                let vote_notification: AppMessage = VotePayload {
                    group_id: group_name.to_string(),
                    proposal_id,
                    payload,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                }
                .into();
                self.handler
                    .on_app_message(group_name, vote_notification)
                    .await?;
                self.spawn_auto_vote(
                    group_name.to_string(),
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
    async fn resolve_on_timeout(&self, group_name: &str, proposal_id: u32) {
        let scope = P::Scope::from(group_name.to_string());
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
                    .with_entry(group_name, |entry| {
                        entry.group.is_consensus_outcome_applied(proposal_id)
                    })
                    .await
                    .unwrap_or(false);
                if resolved_locally {
                    tracing::debug!(
                        group = group_name,
                        proposal_id,
                        "timeout fired for already-resolved proposal: ignoring"
                    );
                } else {
                    tracing::warn!(
                        group = group_name,
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
    /// steward and the group accepts new proposals.
    pub async fn handle_incoming_update_request(
        &self,
        group_name: &str,
        request: GroupUpdateRequest,
    ) -> Result<(), UserError> {
        // Joiners in PendingJoin aren't active participants: buffering KPs
        // would just fill their queue with entries already covered by the
        // welcome they're waiting on. Checked *before* the MLS epoch lookup
        // because a PendingJoin member has no MLS group yet —
        // `current_epoch()` would fail with `GroupNotFound` and surface as
        // a spurious error in the gateway's inbound forwarder.
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;

        let (pending_join, members_for_rotation) = {
            let entry = entry_arc.read().await;
            let pending = entry.state_machine.current_state() == GroupState::PendingJoin;
            let members = if !pending && self.mls_service.has_group(entry.group.group_name()) {
                group_members(&entry.group, &self.mls_service)?
            } else {
                Vec::new()
            };
            (pending, members)
        };
        if pending_join {
            return Ok(());
        }

        let current_epoch = self.mls_service.current_epoch(group_name)?;

        let (inserted, is_epoch_steward, state, buffer_total, should_propose) = {
            let mut entry = entry_arc.write().await;

            // Defensive — core only emits membership changes here.
            if target_identity_of(&request).is_none() {
                return Ok(());
            }

            let inserted = entry
                .group
                .buffer_pending_update(request.clone(), current_epoch);

            // Only the epoch steward proposes immediately. The buffer
            // survives freeze rounds so a later steward can retry.
            let is_es = entry
                .group
                .is_live_epoch_steward(current_epoch, &members_for_rotation);
            let state = entry.state_machine.current_state();
            let total = entry.group.pending_update_count();
            let should = is_es && state == GroupState::Working;
            (inserted, is_es, state, total, should)
        };

        info!(
            group = group_name,
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
                .initiate_proposal(group_name.to_string(), request, None)
                .await
            {
                info!(group = group_name, error = %e, "proposal deferred");
            }
        }
        Ok(())
    }

    pub async fn process_user_vote(
        &mut self,
        group_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        {
            let entry = entry_arc.read().await;
            let state = entry.state_machine.current_state();
            if state == GroupState::Freezing || state == GroupState::Selection {
                return Err(UserError::GroupBlocked(state.to_string()));
            }
        }
        let app_id = self.app_id.clone();

        // Manual vote takes precedence over the pending auto-vote timer.
        self.cancel_auto_vote(group_name, proposal_id);

        let app_message = cast_vote::<P, _>(
            group_name,
            proposal_id,
            vote,
            &self.consensus_service,
            self.eth_signer.clone(),
        )
        .await?;
        let packet = build_message(group_name, &self.mls_service, &app_message, &app_id)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Spawn an auto-vote timer for `(group_name, proposal_id)`. Idempotent
    /// — an existing handle for the same key is aborted and replaced.
    /// `vote` is captured before the sleep so a `GroupSync` during the
    /// delay can't change it.
    pub(crate) fn spawn_auto_vote(
        &self,
        group_name: String,
        proposal_id: u32,
        delay: Duration,
        vote: bool,
    ) {
        self.cancel_auto_vote(&group_name, proposal_id);

        let user = self.clone();
        let key = (group_name.clone(), proposal_id);

        let handle = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(e) = user.cast_auto_vote(&group_name, proposal_id, vote).await {
                tracing::debug!(
                    group = %group_name,
                    proposal_id,
                    error = %e,
                    "auto-vote skipped (already voted or session resolved)"
                );
            } else {
                info!(
                    group = %group_name,
                    proposal_id,
                    vote,
                    "auto-vote cast on timer"
                );
            }
            // Remove the handle when the task completes so repeated manual
            // votes after the timer fires don't try to abort a dead handle.
            if let Ok(mut timers) = user.auto_vote_timers.lock() {
                timers.remove(&(group_name, proposal_id));
            }
        });

        if let Ok(mut timers) = self.auto_vote_timers.lock() {
            timers.insert(key, handle);
        }
    }

    /// Abort the auto-vote timer for `(group_name, proposal_id)` if one is
    /// registered. No-op otherwise.
    pub(crate) fn cancel_auto_vote(&self, group_name: &str, proposal_id: u32) {
        if let Ok(mut timers) = self.auto_vote_timers.lock()
            && let Some(handle) = timers.remove(&(group_name.to_string(), proposal_id))
        {
            handle.abort();
        }
    }

    /// Abort every auto-vote timer belonging to `group_name`. Called on
    /// group leave so no stale timers fire against a group we've left.
    pub(crate) fn cancel_group_auto_votes(&self, group_name: &str) {
        if let Ok(mut timers) = self.auto_vote_timers.lock() {
            timers.retain(|(g, _), handle| {
                if g == group_name {
                    handle.abort();
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Cast the auto-vote on behalf of the local member. Same broadcast
    /// path as a manual vote — the library sees the two identically.
    async fn cast_auto_vote(
        &self,
        group_name: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<(), UserError> {
        if self.lookup_entry(group_name).await.is_none() {
            return Err(UserError::GroupNotFound);
        }
        let app_message = cast_vote::<P, _>(
            group_name,
            proposal_id,
            vote,
            &self.consensus_service,
            self.eth_signer.clone(),
        )
        .await?;
        let packet = build_message(group_name, &self.mls_service, &app_message, &self.app_id)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }
}
