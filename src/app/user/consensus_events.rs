//! Consensus-event dispatch — the other side of the User's inbound edge,
//! triggered when the hashgraph-like-consensus service reaches or fails
//! consensus on a proposal. Compare with `inbound.rs` which handles
//! transport-delivered packets.

use hashgraph_like_consensus::{storage::ConsensusStorage, types::ConsensusEvent};
use prost::Message;
use tracing::{error, info};

use crate::{
    app::user::emergency::emergency_score_ops,
    app::{GroupState, StateChangeHandler, User, UserError},
    core::{
        DeMlsProvider, GroupEventHandler, ProposalKind, ScoreOp, apply_consensus_result,
        group_members, target_identity_of,
    },
    protos::de_mls::messages::v1::{
        GroupUpdateRequest, StewardElectionProposal, group_update_request,
    },
};

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Entry point from the consensus service: decode the proposal, apply the
    /// result to the group, and dispatch to the correct follow-up handler
    /// (election-accepted / election-rejected / emergency-scored).
    pub async fn apply_consensus_outcome(
        &mut self,
        group_name: &str,
        event: ConsensusEvent,
    ) -> Result<(), UserError> {
        let (proposal_id, approved) = match &event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
                ..
            } => (*proposal_id, *result),
            ConsensusEvent::ConsensusFailed { proposal_id, .. } => (*proposal_id, false),
        };

        // Proposal resolved — any pending auto-vote timer for it is moot.
        self.cancel_auto_vote(group_name, proposal_id);

        // Drop re-emissions from the consensus library (timeout-path race)
        // so we don't re-apply state or double-fire UI events.
        {
            let groups = self.groups.read().await;
            if let Some(entry) = groups.get(group_name)
                && entry.group.is_consensus_outcome_applied(proposal_id)
            {
                tracing::debug!(
                    group = group_name,
                    proposal_id,
                    "duplicate consensus outcome dropped"
                );
                return Ok(());
            }
        }

        // Fetch payload from consensus service (no group lock held).
        let scope = P::Scope::from(group_name.to_string());
        let proposal = self
            .consensus_service
            .storage()
            .get_proposal(&scope, proposal_id)
            .await?;
        let payload = proposal.payload;

        // The inactivity timer is self-started by `check_steward_inactivity`
        // on the next poll — no explicit notification needed here.
        let consensus_apply = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            info!(
                group = group_name,
                proposal_id, approved, "consensus reached"
            );
            entry.group.mark_consensus_outcome_applied(proposal_id);
            apply_consensus_result(&mut entry.group, proposal_id, approved, &payload)?
        };

        if let Some(election) = consensus_apply.election {
            return self.handle_election_accepted(group_name, election).await;
        }

        if !approved && let Ok(req) = GroupUpdateRequest::decode(payload.as_slice()) {
            if ProposalKind::of(&req).is_steward_election() {
                self.handle_election_rejected(group_name).await;
            } else if let Some(target) = target_identity_of(&req) {
                // A rejected membership change is the group's decision, not a
                // transient liveness failure: the request author must resend
                // if circumstances change. Drop the buffered entry so the
                // next epoch steward doesn't auto-repromote it.
                let target = target.to_vec();
                let mut groups = self.groups.write().await;
                if let Some(entry) = groups.get_mut(group_name) {
                    entry.group.remove_pending_update(&target);
                }
            }
        }

        let score_ops = emergency_score_ops(&payload, approved);
        if !score_ops.is_empty() {
            self.handle_emergency_scored(group_name, proposal_id, &payload, &score_ops)
                .await?;
        }

        Ok(())
    }

    /// Accepted election: validate the proposed list, apply it, reset the
    /// retry counter, exit Reelection if we were in it, and drain buffered
    /// updates so the fresh epoch steward picks them up.
    async fn handle_election_accepted(
        &self,
        group_name: &str,
        election: StewardElectionProposal,
    ) -> Result<(), UserError> {
        let is_valid = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let members = group_members(&entry.group, &self.mls_service)?;
            entry.group.validate_steward_list_proposal(
                &election.proposed_stewards,
                election.election_epoch,
                &members,
                election.retry_round,
            )?
        };
        if !is_valid {
            info!(
                group = group_name,
                "steward election rejected: invalid list"
            );
            return Ok(());
        }

        let resumed_from_reelection = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
            entry.group.generate_and_set_steward_list(
                election.election_epoch,
                &election.proposed_stewards,
                election.proposed_stewards.len(),
            )?;
            entry.group.reset_reelection_round();
            if entry.state_machine.current_state() == GroupState::Reelection {
                entry.state_machine.start_working();
                true
            } else {
                false
            }
        };
        if resumed_from_reelection {
            self.state_handler
                .on_state_changed(group_name, GroupState::Working)
                .await;
        }
        info!(
            group = group_name,
            epoch = election.election_epoch,
            stewards = election.proposed_stewards.len(),
            retry_round = election.retry_round,
            "steward election applied"
        );

        self.process_buffered_updates(group_name).await
    }

    /// Rejected election: bump the retry round and, under the max, retry
    /// immediately (idempotent — only the responsible proposer actually
    /// submits). Over the max, surface the stuck state to the UI.
    async fn handle_election_rejected(&self, group_name: &str) {
        let (round, max) = {
            let mut groups = self.groups.write().await;
            let Some(entry) = groups.get_mut(group_name) else {
                return;
            };
            entry.group.bump_reelection_round();
            (
                entry.group.reelection_round(),
                self.default_group_config.max_reelection_retries,
            )
        };
        if round > max {
            let msg = format!(
                "Steward election stuck after {max} retry(ies) — group {group_name} \
                 needs manual intervention"
            );
            error!(group = group_name, round, max, "steward election stuck");
            self.handler
                .on_error(group_name, "Reelection stuck", &msg)
                .await;
            return;
        }
        info!(
            group = group_name,
            round, max, "steward election rejected, retrying"
        );
        if let Err(e) = self.try_initiate_steward_election(group_name).await {
            info!(group = group_name, error = %e, "election retry deferred");
        }
    }

    /// Emergency proposal resolved: apply score ops, clear the
    /// pending-removal / pending-ECP buffers, lift the partial freeze (and
    /// exit Reelection if we landed there), then check for new
    /// below-threshold removals.
    async fn handle_emergency_scored(
        &self,
        group_name: &str,
        proposal_id: u32,
        payload: &[u8],
        score_ops: &[ScoreOp],
    ) -> Result<(), UserError> {
        self.scoring().apply_ops(group_name, score_ops);

        if let Ok(req) = GroupUpdateRequest::decode(payload)
            && let Some(group_update_request::Payload::EmergencyCriteria(ec)) = &req.payload
            && let Some(ev) = &ec.evidence
        {
            let mut groups = self.groups.write().await;
            if let Some(entry) = groups.get_mut(group_name) {
                entry.group.resolve_pending_removal(&ev.target_member_id);
            }
        }

        let resumed_from_reelection = {
            let mut groups = self.groups.write().await;
            match groups.get_mut(group_name) {
                Some(entry) => {
                    entry.group.resolve_emergency(proposal_id);
                    if entry.state_machine.current_state() == GroupState::Reelection {
                        entry.state_machine.start_working();
                        true
                    } else {
                        false
                    }
                }
                None => false,
            }
        };
        if resumed_from_reelection {
            self.state_handler
                .on_state_changed(group_name, GroupState::Working)
                .await;
        }

        if let Err(e) = self.check_and_initiate_score_removals(group_name).await {
            error!(group = group_name, error = %e, "score-removal check failed");
        }
        Ok(())
    }
}
