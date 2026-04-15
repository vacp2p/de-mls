//! Inbound packet dispatch and consensus event handling.

use super::*;

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Dispatches a single ProcessResult to the appropriate handler/consensus/state-machine action.
    /// Used by process_inbound_packet() and freeze finalization.
    pub async fn dispatch_inbound_result(
        &self,
        group_name: &str,
        result: ProcessResult,
    ) -> Result<(), UserError> {
        match result {
            ProcessResult::AppMessage(msg) => {
                self.handler.on_app_message(group_name, msg).await?;
            }
            ProcessResult::Proposal(proposal) => {
                // RFC SS"Partial Freeze Semantics": track any incoming emergency proposal
                // so the partial freeze applies to all peers, not just the creator.
                if let Ok(req) = GroupUpdateRequest::decode(proposal.payload.as_slice()) {
                    if matches!(
                        req.payload,
                        Some(group_update_request::Payload::EmergencyCriteria(_))
                    ) {
                        let mut groups = self.groups.write().await;
                        if let Some(entry) = groups.get_mut(group_name) {
                            entry.group.observe_emergency(proposal.proposal_id);
                        }
                    }
                }
                // TODO(M3c): RFC SS"Partial Freeze Semantics" also requires that
                // lower-priority proposals received from peers are DROPPED when an
                // emergency is active -- not just blocked locally. `forward_incoming_proposal`
                // feeds the local consensus service and cannot be filtered here without
                // risk of consensus inconsistency (other nodes may already have accepted
                // the proposal). Full enforcement requires consensus-service-level
                // priority gating (see ROADMAP.md M3c SS3c.1).
                forward_incoming_proposal::<P>(
                    group_name,
                    proposal,
                    &*self.consensus_service,
                    &*self.handler,
                )
                .await?;
            }
            ProcessResult::Vote(vote) => {
                forward_incoming_vote::<P>(group_name, vote, &*self.consensus_service).await?;
            }
            ProcessResult::MembershipChangeReceived(request) => {
                self.initiate_proposal(group_name.to_string(), request)
                    .await?;
            }
            ProcessResult::JoinedGroup(name) => {
                // Build "User joined" system message (moved from core)
                let msg: AppMessage = ConversationMessage {
                    message: format!("User {} joined the group", self.mls_service.wallet_hex())
                        .into_bytes(),
                    sender: "SYSTEM".to_string(),
                    group_name: name.clone(),
                }
                .into();

                // Build and send outbound packet
                let packet = {
                    let groups = self.groups.read().await;
                    if let Some(entry) = groups.get(&name) {
                        core::build_message(&entry.group, &self.mls_service, &msg, &self.app_id)?
                    } else {
                        return Ok(());
                    }
                };
                self.handler.on_outbound(&name, packet).await?;
                self.handler.on_joined_group(&name).await?;

                // Sync MLS members into peer scoring
                {
                    let groups = self.groups.read().await;
                    if let Some(entry) = groups.get(&name) {
                        self.sync_scoring_members(&name, &entry.group);
                    }
                }

                // Sync epoch boundary and transition to Working
                let state = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(&name) {
                        entry.state_machine.start_working();
                        Some(entry.state_machine.current_state())
                    } else {
                        None
                    }
                };
                if let Some(state) = state {
                    self.state_handler.on_state_changed(&name, state).await;
                }
            }
            ProcessResult::GroupUpdated => {
                // TODO(M2): Reward the commit author with SuccessfulCommit once
                // ProcessResult/FreezeFinalizeResult carries the commit sender identity.
                // In multi-steward mode the winner may differ from the epoch steward.

                // Sync member list in peer scoring (commit may add/remove members)
                {
                    let groups = self.groups.read().await;
                    if let Some(entry) = groups.get(group_name) {
                        self.sync_scoring_members(group_name, &entry.group);
                    }
                }

                // Transition to Working BEFORE steward checks (election needs Working state)
                let transitioned = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(group_name) {
                        let state = entry.state_machine.current_state();
                        if state == GroupState::Working
                            || state == GroupState::Freezing
                            || state == GroupState::Selection
                            || state == GroupState::Reelection
                        {
                            entry.state_machine.start_working();
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                // Now run steward checks (auto-fill, flag sync, election) in Working state
                self.steward_list_housekeeping(group_name).await?;

                if transitioned {
                    self.state_handler
                        .on_state_changed(group_name, GroupState::Working)
                        .await;
                }
            }
            ProcessResult::LeaveGroup => {
                self.groups.write().await.remove(group_name);
                self.handler.on_leave_group(group_name).await?;
            }
            ProcessResult::ViolationDetected(evidence) => {
                info!(
                    "Violation detected: type={}, target={:?}",
                    evidence.violation_type, evidence.target_member_id
                );
                let evidence = evidence.with_creator(self.mls_service.wallet_bytes());
                self.initiate_proposal(group_name.to_string(), evidence.into_update_request()?)
                    .await?;
            }
            ProcessResult::CommitCandidateReceived => {
                let transitioned = {
                    let mut groups = self.groups.write().await;
                    if let Some(entry) = groups.get_mut(group_name) {
                        // Enter Freezing on candidate receipt if in Working state.
                        // The state guard prevents double-entry if already freezing.
                        if entry.state_machine.current_state() == GroupState::Working {
                            entry.state_machine.start_freezing();
                            let epoch = self.mls_service.current_epoch(group_name).unwrap_or(0);
                            entry.group.ensure_freeze_round(epoch);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if transitioned {
                    self.state_handler
                        .on_state_changed(group_name, GroupState::Freezing)
                        .await;
                }
            }
            ProcessResult::StewardListSyncReceived(sync) => {
                // Phase 1: read lock -- check if list needed, fetch members.
                let members = {
                    let groups = self.groups.read().await;
                    let Some(entry) = groups.get(group_name) else {
                        return Ok(());
                    };
                    if entry.group.steward_list().is_some() {
                        None // Already has list, skip
                    } else {
                        Some(core::group_members(&entry.group, &self.mls_service)?)
                    }
                };

                let Some(members) = members else {
                    return Ok(());
                };

                // Validate: (1) all proposed stewards are current group members,
                // (2) the list ordering is self-consistent (matches deterministic
                // generation with the steward members as the candidate pool).
                // We validate against steward_members, NOT full group, because
                // the list may have been generated before the joiner existed.
                let all_present = StewardList::validate_members(&sync.steward_members, &members);
                let ordering_valid = StewardList::validate(
                    &sync.steward_members,
                    sync.start_epoch,
                    group_name.as_bytes(),
                    &sync.steward_members,
                    &ProtocolConfig::new(sync.sn_min as usize, sync.sn_max as usize)
                        .unwrap_or_default(),
                )?;

                if all_present && ordering_valid {
                    // Phase 2: write lock -- apply validated list.
                    let sn = sync.steward_members.len();
                    {
                        let mut groups = self.groups.write().await;
                        if let Some(entry) = groups.get_mut(group_name) {
                            entry.group.generate_and_set_steward_list(
                                sync.start_epoch,
                                &sync.steward_members,
                                sn,
                            )?;
                        }
                    }
                    info!(
                        "[StewardListSyncReceived] Applied steward list for group \
                         {group_name} (start_epoch={}, len={sn})",
                        sync.start_epoch,
                    );
                } else {
                    info!(
                        "[StewardListSyncReceived] Ignoring invalid sync for group \
                         {group_name} (present={all_present}, ordering={ordering_valid})"
                    );
                }
            }
            ProcessResult::Noop => {}
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

            core::process_inbound(
                &mut entry.group,
                &packet.payload,
                &packet.subtopic,
                &self.mls_service,
            )?
        };

        self.dispatch_inbound_result(&group_name, result).await
    }

    /// Handle a consensus event.
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

        // Fetch payload from consensus service (no lock held).
        let scope = P::Scope::from(group_name.to_string());
        let payload = self
            .consensus_service
            .get_proposal_payload(&scope, proposal_id)
            .await?;

        // Apply consensus result (write lock)
        let consensus_apply = {
            let mut groups = self.groups.write().await;
            let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;

            let approved_before = entry.group.approved_proposals_count();

            info!("Consensus reached for proposal {proposal_id}: approved={approved}");
            let result =
                core::apply_consensus_result(&mut entry.group, proposal_id, approved, &payload)?;

            let approved_after = entry.group.approved_proposals_count();
            entry
                .state_machine
                .notify_proposal_approved(approved_before, approved_after);

            result
        };

        // -- Handle election outcome --
        if let Some(election) = consensus_apply.election {
            // Validate the proposed list against the current member set.
            let (is_valid, _) = {
                let groups = self.groups.read().await;
                let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
                let members = core::group_members(&entry.group, &self.mls_service)?;
                let valid = core::validate_election_proposal(
                    &entry.group,
                    &election.proposed_stewards,
                    election.election_epoch,
                    &members,
                )?;
                (valid, members)
            };

            if is_valid {
                {
                    let mut groups = self.groups.write().await;
                    let entry = groups.get_mut(group_name).ok_or(UserError::GroupNotFound)?;
                    core::apply_election_result(
                        &mut entry.group,
                        &election.proposed_stewards,
                        election.election_epoch,
                    )?;
                }
                // Sync steward flag from the new list.
                info!(
                    "Steward election applied for group {group_name}: \
                     epoch={}, stewards={}",
                    election.election_epoch,
                    election.proposed_stewards.len()
                );
            } else {
                info!("Steward election proposal rejected (invalid list) for group {group_name}");
            }
            return Ok(());
        }

        // -- Handle emergency score ops --
        if !consensus_apply.score_ops.is_empty() {
            // Emergency proposal was resolved -- apply scores and lift partial freeze.
            {
                let mut scoring = self.scoring();
                for op in &consensus_apply.score_ops {
                    scoring.apply_event(group_name, &op.member_id, op.event);
                }
            }

            // Resolve removal target tracking for SCORE_BELOW_THRESHOLD ECPs.
            // Extract the target_member_id from the payload evidence.
            if let Ok(req) = GroupUpdateRequest::decode(payload.as_slice()) {
                if let Some(group_update_request::Payload::EmergencyCriteria(ec)) = &req.payload {
                    if let Some(ev) = &ec.evidence {
                        let mut groups = self.groups.write().await;
                        if let Some(entry) = groups.get_mut(group_name) {
                            entry.group.resolve_pending_removal(&ev.target_member_id);
                        }
                    }
                }
            }

            // RFC SS"Partial Freeze Semantics": lift the freeze once the emergency
            // proposal is resolved (approved or rejected -- either way it's finalized).
            {
                let mut groups = self.groups.write().await;
                if let Some(entry) = groups.get_mut(group_name) {
                    entry.group.resolve_emergency(proposal_id);
                }
            }

            // After scoring changes, check if any members now fall below the threshold.
            if let Err(e) = self.check_and_initiate_score_removals(group_name).await {
                error!("[apply_consensus_outcome] check_and_initiate_score_removals failed: {e}");
            }
        }

        Ok(())
    }
}
