//! Inbound packet dispatch and consensus event handling.

use hashgraph_like_consensus::protos::consensus::v1::Proposal;
use prost::Message;
use tracing::{error, info};

use crate::{
    app::{
        GroupState, StateChangeHandler, User, UserError, forward_incoming_proposal,
        forward_incoming_vote,
    },
    core::{
        DeMlsProvider, GroupEventHandler, ProcessResult, ProtocolConfig, StewardList,
        build_message, create_commit_candidate, group_members, process_inbound,
    },
    ds::InboundPacket,
    mls_crypto::ShortId,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationMessage, GroupSync, GroupUpdateRequest, LeaveRequest,
        group_update_request,
    },
};

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Dispatches a single ProcessResult to the appropriate handler/consensus/state-machine action.
    pub async fn dispatch_inbound_result(
        &self,
        group_name: &str,
        result: ProcessResult,
    ) -> Result<(), UserError> {
        match result {
            ProcessResult::AppMessage(msg) => {
                self.handler.on_app_message(group_name, msg).await?;
                Ok(())
            }
            ProcessResult::Proposal(proposal) => {
                self.on_incoming_proposal(group_name, proposal).await
            }
            ProcessResult::Vote(vote) => {
                forward_incoming_vote::<P>(group_name, vote, &*self.consensus_service).await?;
                Ok(())
            }
            ProcessResult::MembershipChangeReceived(request) => {
                self.handle_incoming_update_request(group_name, request)
                    .await
            }
            ProcessResult::JoinedGroup(name) => self.on_joined_group(&name).await,
            ProcessResult::GroupUpdated => self.on_group_updated(group_name).await,
            ProcessResult::LeaveGroup => self.on_leave_group(group_name).await,
            ProcessResult::CommitCandidateReceived => {
                self.on_commit_candidate_received(group_name).await
            }
            ProcessResult::GroupSyncReceived(sync) => self.on_group_sync(group_name, sync).await,
            ProcessResult::LeaveRequestReceived(leave) => {
                self.on_leave_request(group_name, leave).await
            }
            ProcessResult::Noop => Ok(()),
        }
    }

    /// Before forwarding to consensus, mirror intent into local buffers:
    /// emergency proposals set the partial-freeze flag and resolve any
    /// locally-buffered ECP for the same violation; membership-change
    /// proposals get mirrored into the pending-update buffer so a future
    /// epoch steward can retry if this round fails.
    ///
    /// RFC §"Partial Freeze Semantics" asks that lower-priority proposals
    /// from peers be DROPPED during an active emergency, not merely locally
    /// blocked. We don't drop today — the RFC's Δ-synchrony assumption keeps
    /// divergence windows small. Consensus-service-level priority gating is
    /// tracked as a backlog item in `docs/ROADMAP.md`.
    async fn on_incoming_proposal(
        &self,
        group_name: &str,
        proposal: Proposal,
    ) -> Result<(), UserError> {
        if let Ok(req) = GroupUpdateRequest::decode(proposal.payload.as_slice()) {
            let current_epoch = self.mls_service.current_epoch(group_name)?;
            let mut groups = self.groups.write().await;
            if let Some(entry) = groups.get_mut(group_name) {
                match &req.payload {
                    Some(group_update_request::Payload::EmergencyCriteria(_)) => {
                        entry.group.observe_emergency(proposal.proposal_id);
                    }
                    Some(group_update_request::Payload::InviteMember(_))
                    | Some(group_update_request::Payload::RemoveMember(_)) => {
                        entry
                            .group
                            .buffer_pending_update(req.clone(), current_epoch);
                    }
                    _ => {}
                }
            }
        }
        let proposal_id = proposal.proposal_id;
        forward_incoming_proposal::<P>(
            group_name,
            proposal,
            &*self.consensus_service,
            &*self.handler,
        )
        .await?;
        // Start this member's auto-vote timer. Fires `voting_delay` after
        // the proposal becomes visible; cancelled on manual vote or
        // ProposalDecided.
        self.spawn_auto_vote(group_name.to_string(), proposal_id);
        Ok(())
    }

    /// We just joined via welcome. Broadcast a system "joined" chat message,
    /// sync scoring, and transition to Working. Pending-update pruning is
    /// defensive — PendingJoin no longer buffers, but paths may change.
    async fn on_joined_group(&self, name: &str) -> Result<(), UserError> {
        self.prune_pending_updates_after_commit(name).await?;

        let msg: AppMessage = ConversationMessage {
            message: format!("User {} joined the group", self.mls_service.wallet_hex())
                .into_bytes(),
            sender: "SYSTEM".to_string(),
            group_name: name.to_string(),
        }
        .into();
        let packet = {
            let groups = self.groups.read().await;
            match groups.get(name) {
                Some(entry) => build_message(&entry.group, &self.mls_service, &msg, &self.app_id)?,
                None => return Ok(()),
            }
        };
        self.handler.on_outbound(name, packet).await?;
        self.handler.on_joined_group(name).await?;

        {
            let groups = self.groups.read().await;
            if let Some(entry) = groups.get(name) {
                self.sync_scoring_members(name, &entry.group);
            }
        }

        let state = {
            let mut groups = self.groups.write().await;
            groups.get_mut(name).map(|entry| {
                entry.state_machine.start_working();
                entry.state_machine.current_state()
            })
        };
        if let Some(state) = state {
            self.state_handler.on_state_changed(name, state).await;
        }
        Ok(())
    }

    /// A commit merged. Sync scoring + pending-update buffers, transition to
    /// Working, and run steward housekeeping (auto-fill, election kick-off,
    /// buffered-update drain). The commit author's `SuccessfulCommit`
    /// reward is emitted by `finalize_freeze_round`, not here.
    async fn on_group_updated(&self, group_name: &str) -> Result<(), UserError> {
        {
            let groups = self.groups.read().await;
            if let Some(entry) = groups.get(group_name) {
                self.sync_scoring_members(group_name, &entry.group);
            }
        }
        self.prune_pending_updates_after_commit(group_name).await?;

        // Transition to Working BEFORE steward checks (election needs Working
        // state). Reset reelection_round: this commit advanced the epoch,
        // so whatever retry cycle we were in belongs to the previous epoch.
        let transitioned = {
            let mut groups = self.groups.write().await;
            match groups.get_mut(group_name) {
                Some(entry) => {
                    entry.group.reset_reelection_round();
                    let state = entry.state_machine.current_state();
                    if matches!(
                        state,
                        GroupState::Working
                            | GroupState::Freezing
                            | GroupState::Selection
                            | GroupState::Reelection
                    ) {
                        entry.state_machine.start_working();
                        true
                    } else {
                        false
                    }
                }
                None => false,
            }
        };

        self.steward_list_housekeeping(group_name).await?;
        self.process_buffered_updates(group_name).await?;

        if transitioned {
            self.state_handler
                .on_state_changed(group_name, GroupState::Working)
                .await;
        }
        Ok(())
    }

    async fn on_leave_group(&self, group_name: &str) -> Result<(), UserError> {
        self.groups.write().await.remove(group_name);
        self.cleanup_consensus_scope(group_name).await?;
        self.handler.on_leave_group(group_name).await?;
        Ok(())
    }

    /// Peer broadcast a commit candidate. If we were in Working, enter
    /// Freezing and — if we're a steward — build our own candidate too.
    async fn on_commit_candidate_received(&self, group_name: &str) -> Result<(), UserError> {
        let (transitioned, outbound) = {
            let mut groups = self.groups.write().await;
            let Some(entry) = groups.get_mut(group_name) else {
                return Ok(());
            };
            if entry.state_machine.current_state() != GroupState::Working {
                return Ok(());
            }

            entry.state_machine.start_freezing();
            let epoch = self.mls_service.current_epoch(group_name)?;
            entry.group.ensure_freeze_round(epoch);

            let outbound = if entry.group.is_steward() {
                match create_commit_candidate(&mut entry.group, &self.mls_service, &self.app_id) {
                    Ok(packets) => packets,
                    Err(e) => {
                        error!(
                            group = group_name,
                            error = %e,
                            "own commit candidate build failed"
                        );
                        None
                    }
                }
            } else {
                None
            };
            (true, outbound)
        };

        if transitioned {
            self.state_handler
                .on_state_changed(group_name, GroupState::Freezing)
                .await;
            if let Some(message) = outbound {
                self.handler.on_outbound(group_name, message).await?;
            }
        }
        Ok(())
    }

    /// Apply a steward's `GroupSync` when we're a joiner without a steward
    /// list. Validates the proposed list against the members it carries
    /// (not the full MLS set — the list may have been generated before we
    /// existed), then applies list + protocol flags + timing + peer scores.
    async fn on_group_sync(&self, group_name: &str, sync: GroupSync) -> Result<(), UserError> {
        let members = {
            let groups = self.groups.read().await;
            let Some(entry) = groups.get(group_name) else {
                return Ok(());
            };
            if entry.group.steward_list().is_some() {
                return Ok(());
            }
            group_members(&entry.group, &self.mls_service)?
        };

        let current_epoch = self.mls_service.current_epoch(group_name)?;
        if sync.start_epoch > current_epoch {
            info!(
                group = group_name,
                start_epoch = sync.start_epoch,
                current_epoch,
                "group sync rejected: start_epoch > current_epoch"
            );
            return Ok(());
        }

        // Ghost stewards (removed since the list was elected) are skipped at
        // query time by the `live_*` rotation helpers, so we tolerate them
        // here rather than rejecting the whole sync. A sync with zero live
        // stewards is useless and still rejected. Full list pruning on
        // member removal is tracked in the roadmap.
        let any_present = sync.steward_members.iter().any(|s| members.contains(s));
        let ordering_valid = StewardList::validate(
            &sync.steward_members,
            sync.start_epoch,
            group_name.as_bytes(),
            &sync.steward_members,
            &ProtocolConfig::new(sync.sn_min as usize, sync.sn_max as usize)?,
            sync.retry_round,
        )?;
        if !(any_present && ordering_valid) {
            info!(
                group = group_name,
                any_present,
                ordering = ordering_valid,
                "group sync rejected: invalid"
            );
            return Ok(());
        }

        let sn = sync.steward_members.len();
        {
            let mut groups = self.groups.write().await;
            if let Some(entry) = groups.get_mut(group_name) {
                entry.group.generate_and_set_steward_list(
                    sync.start_epoch,
                    &sync.steward_members,
                    sn,
                )?;
                entry
                    .group
                    .set_allow_subset_candidates(sync.allow_subset_candidates);
                if let Some(timing) = &sync.timing {
                    let epoch_dur = std::time::Duration::from_millis(timing.epoch_duration_ms);
                    let freeze_dur = std::time::Duration::from_millis(timing.freeze_duration_ms);
                    entry.state_machine.update_timing(epoch_dur, freeze_dur);
                }
            }
        }

        if !sync.peer_scores.is_empty() {
            let mut scoring = self.scoring();
            for ps in &sync.peer_scores {
                scoring.set_score(group_name, &ps.member_id, ps.score);
            }
        }

        info!(
            group = group_name,
            start_epoch = sync.start_epoch,
            stewards = sn,
            scores = sync.peer_scores.len(),
            timing = sync.timing.is_some(),
            "group sync applied"
        );
        Ok(())
    }

    /// Signer/identity match has already been verified at the MLS decrypt
    /// layer (see `process_app_subtopic`). We just need to confirm the
    /// identity is a current group member before accepting the auto-removal.
    async fn on_leave_request(
        &self,
        group_name: &str,
        leave: LeaveRequest,
    ) -> Result<(), UserError> {
        let accepted = {
            let mut groups = self.groups.write().await;
            let Some(entry) = groups.get_mut(group_name) else {
                return Ok(());
            };
            let members = group_members(&entry.group, &self.mls_service)?;
            if !members.iter().any(|m| m == &leave.identity) {
                tracing::warn!(
                    group = group_name,
                    sender = %ShortId(&leave.identity),
                    "leave request ignored: sender not a current member"
                );
                false
            } else {
                entry
                    .group
                    .accept_auto_approved_leave(leave.identity.clone())
            }
        };
        if accepted {
            info!(
                group = group_name,
                identity = %ShortId(&leave.identity),
                "leave accepted (auto-approved)"
            );
        }
        Ok(())
    }

    /// Process an inbound packet.
    pub async fn process_inbound_packet(&self, packet: InboundPacket) -> Result<(), UserError> {
        let group_name = packet.group_id.clone();

        // Echo dedup: drop our own messages received back from pub/sub
        if packet.app_id == self.app_id {
            return Ok(());
        }

        // Verify group exists
        {
            let groups = self.groups.read().await;
            if !groups.contains_key(&group_name) {
                return Err(UserError::GroupNotFound);
            }
        }

        // Process the packet
        let result = {
            let mut groups = self.groups.write().await;
            let entry = groups
                .get_mut(&group_name)
                .ok_or(UserError::GroupNotFound)?;

            process_inbound(
                &mut entry.group,
                &packet.payload,
                &packet.subtopic,
                &self.mls_service,
            )?
        };

        self.dispatch_inbound_result(&group_name, result).await
    }
}
