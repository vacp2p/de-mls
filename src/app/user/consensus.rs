//! Proposal submission + voting.
//!
//! Outgoing proposals run as a background task: submit to consensus, register
//! ownership, broadcast (bundled or unbundled per `creator_vote`), resolve on
//! timeout. Since `User` is `Clone` (every field is `Arc` or cheap `Clone`),
//! each spawn just owns its own handle — no ctx struct per task kind.

use hashgraph_like_consensus::storage::ConsensusStorage;
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
    /// Freezing / Selection → always blocked. Reelection → emergency only.
    /// Otherwise the RFC partial-freeze rule applies (see
    /// `Group::partial_freeze_blocks`).
    async fn check_proposal_allowed(
        &self,
        group_name: &str,
        kind: ProposalKind,
    ) -> Result<u32, UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let state = entry.state_machine.current_state();

        match state {
            GroupState::Reelection => {
                if !kind.is_emergency() {
                    return Err(UserError::GroupBlocked(state.to_string()));
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

        let scope = P::Scope::from(group_name.clone());
        tokio::time::sleep(self.default_group_config.consensus_timeout).await;
        self.resolve_on_timeout(proposal_id, &scope).await;
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
        let (proposal_id, unbundled) = submit_proposal::<P>(
            group_name,
            &request,
            self.mls_service.wallet_hex(),
            &self.consensus_service,
            ProposalParams {
                expected_voters,
                proposal_expiration: self.default_group_config.proposal_expiration,
                consensus_timeout: self.default_group_config.consensus_timeout,
                liveness_criteria_yes: self.default_group_config.liveness_criteria_yes,
            },
        )
        .await?;

        let group = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            entry
                .group
                .store_voting_proposal(proposal_id, request.clone());
            if kind.is_emergency() {
                entry.group.observe_emergency(proposal_id);
            }
            entry.group.clone()
        };

        let outbound = match creator_vote {
            Some(vote) => {
                // Bundled path: cast the creator's vote; `cast_vote`'s owner
                // branch returns the Proposal with our vote attached.
                cast_vote::<P, _>(
                    &group,
                    proposal_id,
                    vote,
                    &self.consensus_service,
                    self.eth_signer.clone(),
                )
                .await?
            }
            None => unbundled,
        };

        let packet = build_message(&group, &self.mls_service, &outbound, &self.app_id)?;
        self.handler.on_outbound(group_name, packet).await?;

        match creator_vote {
            Some(_) => {
                // Creator already voted — populate history cache, no banner.
                self.handler
                    .on_own_proposal_submitted(group_name, proposal_id, &request)
                    .await?;
            }
            None => {
                // Creator hasn't voted — show them the banner like peers.
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
            }
        }

        Ok(proposal_id)
    }

    /// Resolve the proposal via the consensus library's timeout path if it's
    /// still in the active set.
    async fn resolve_on_timeout(&self, proposal_id: u32, scope: &P::Scope) {
        let still_active = self
            .consensus_service
            .storage()
            .get_active_proposals(scope)
            .await
            .map(|active| active.iter().any(|p| p.proposal_id == proposal_id))
            .unwrap_or(false);
        if !still_active {
            return;
        }
        if let Err(e) = self
            .consensus_service
            .handle_consensus_timeout(scope, proposal_id)
            .await
        {
            info!(proposal_id, error = %e, "timeout resolution skipped");
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
        let pending_join = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            entry.state_machine.current_state() == GroupState::PendingJoin
        };
        if pending_join {
            return Ok(());
        }

        let current_epoch = self.mls_service.current_epoch(group_name)?;

        let members_for_rotation = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            if self.mls_service.has_group(entry.group.group_name()) {
                group_members(&entry.group, &self.mls_service)?
            } else {
                Vec::new()
            }
        };

        let (inserted, is_epoch_steward, state, buffer_total, should_propose) = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

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
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let state = entry.state_machine.current_state();
        if state == GroupState::Freezing || state == GroupState::Selection {
            return Err(UserError::GroupBlocked(state.to_string()));
        }
        let group = entry.group.clone();
        let app_id = self.app_id.clone();
        drop(groups);

        let app_message = cast_vote::<P, _>(
            &group,
            proposal_id,
            vote,
            &self.consensus_service,
            self.eth_signer.clone(),
        )
        .await?;
        let packet = build_message(&group, &self.mls_service, &app_message, &app_id)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }
}
