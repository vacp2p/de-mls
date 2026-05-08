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
        DeMlsProvider, GroupEventHandler, PeerScoringPlugin, ProcessResult, ProposalKind,
        ScoreSnapshot, StewardList, StewardListConfig, StewardListPlugin, member_set,
    },
    ds::{APP_MSG_SUBTOPIC, InboundPacket, WELCOME_SUBTOPIC},
    identity::{Identity, ShortId},
    mls_crypto::{MlsService, key_package_bytes_from_json},
    protos::de_mls::messages::v1::{
        AppMessage, ConversationMessage, GroupSync, GroupUpdateRequest, InviteMember,
        WelcomeMessage, group_update_request, welcome_message,
    },
};

impl<
    P: DeMlsProvider,
    M: MlsService,
    Sc: PeerScoringPlugin,
    St: StewardListPlugin,
    I: Identity,
    H: GroupEventHandler + 'static,
    SCH: StateChangeHandler + 'static,
> User<P, M, Sc, St, I, H, SCH>
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
                let entry_arc = self
                    .lookup_entry(group_name)
                    .await
                    .ok_or(UserError::GroupNotFound)?;
                let entry = entry_arc.read().await;
                forward_incoming_vote::<P>(&entry.group, vote, &*self.consensus_service).await?;
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
        let decoded = GroupUpdateRequest::decode(proposal.payload.as_slice()).ok();
        if let Some(req) = decoded.as_ref()
            && let Some(entry_arc) = self.lookup_entry(group_name).await
        {
            let mut entry = entry_arc.write().await;
            let current_epoch = match entry.mls() {
                Some(mls) => mls.current_epoch()?,
                None => 0,
            };
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
        let proposal_id = proposal.proposal_id;
        let expected_voters = proposal.expected_voters_count;
        let kind = decoded
            .as_ref()
            .map(ProposalKind::of)
            .unwrap_or(ProposalKind::Commit);
        forward_incoming_proposal::<P>(
            group_name,
            proposal,
            &*self.consensus_service,
            &*self.handler,
        )
        .await?;
        // Skip auto-vote for fast-path proposals: the creator's bundled
        // YES already resolved the session, so the timer would hit a
        // closed session.
        if expected_voters > 1 {
            let Some((delay, vote)) = self
                .with_entry(group_name, |e| {
                    (
                        e.config.voting_delay_for(kind),
                        e.config.liveness_criteria_yes,
                    )
                })
                .await
            else {
                return Ok(());
            };
            self.spawn_auto_vote(group_name, proposal_id, delay, vote);
        }
        Ok(())
    }

    /// We just joined via welcome. Broadcast a system "joined" chat message,
    /// sync scoring, and transition to Working. Pending-update pruning is
    /// defensive — PendingJoin doesn't buffer, but paths may change.
    async fn on_joined_group(&self, name: &str) -> Result<(), UserError> {
        self.prune_pending_updates_after_commit(name).await?;

        let msg: AppMessage = ConversationMessage {
            message: format!(
                "User {} joined the group",
                self.identity().identity_display()
            )
            .into_bytes(),
            sender: "SYSTEM".to_string(),
            group_name: name.to_string(),
        }
        .into();
        let Some(entry_arc) = self.lookup_entry(name).await else {
            return Ok(());
        };
        let (packet, mls_members) = {
            let entry = entry_arc.read().await;
            let mls = entry.expect_mls()?;
            let packet = mls.build_message(&msg, &self.app_id)?;
            let members = entry.group_members().unwrap_or_default();
            (packet, members)
        };
        self.handler.on_outbound(name, packet).await?;
        self.handler.on_joined_group(name).await?;
        self.sync_scoring_members(name, &mls_members).await;

        let state = {
            let mut entry = entry_arc.write().await;
            entry.start_working();
            entry.current_state()
        };
        self.state_handler.on_state_changed(name, state).await;
        Ok(())
    }

    /// A commit merged. Sync scoring + pending-update buffers, transition to
    /// Working, and run steward housekeeping (auto-fill, election kick-off,
    /// buffered-update drain). The commit author's `SuccessfulCommit`
    /// reward is emitted by `finalize_freeze_round`, not here.
    async fn on_group_updated(&self, group_name: &str) -> Result<(), UserError> {
        if let Some(entry_arc) = self.lookup_entry(group_name).await {
            let mls_members = {
                let entry = entry_arc.read().await;
                if entry.mls().is_some() {
                    entry.group_members().unwrap_or_default()
                } else {
                    Vec::new()
                }
            };
            self.sync_scoring_members(group_name, &mls_members).await;
        }
        self.prune_pending_updates_after_commit(group_name).await?;

        // Transition to Working BEFORE steward checks (election needs Working
        // state). Reset reelection_round: this commit advanced the epoch,
        // so whatever retry cycle we were in belongs to the previous epoch.
        let transitioned = match self.lookup_entry(group_name).await {
            Some(entry_arc) => {
                let mut entry = entry_arc.write().await;
                entry.steward.reset_retry();
                let state = entry.current_state();
                if matches!(
                    state,
                    GroupState::Working
                        | GroupState::Freezing
                        | GroupState::Selection
                        | GroupState::Reelection
                ) {
                    entry.start_working();
                    true
                } else {
                    false
                }
            }
            None => false,
        };

        self.steward_list_housekeeping(group_name).await?;
        self.process_buffered_updates(group_name).await?;
        self.maybe_close_recovery_window(group_name).await;

        if transitioned {
            self.state_handler
                .on_state_changed(group_name, GroupState::Working)
                .await;
        }
        Ok(())
    }

    /// Fire a steward election while `recovery_mode` is set so the next
    /// list installs and closes the window.
    async fn maybe_close_recovery_window(&self, group_name: &str) {
        let in_recovery_mode = self
            .with_entry(group_name, |entry| entry.is_in_recovery_mode())
            .await
            .unwrap_or(false);
        if !in_recovery_mode {
            return;
        }
        if let Err(e) = self
            .try_initiate_steward_election(group_name, true, None)
            .await
        {
            info!(
                group = group_name,
                error = %e,
                "post-recovery election deferred"
            );
        }
    }

    async fn on_leave_group(&self, group_name: &str) -> Result<(), UserError> {
        let entry = self.groups.write().await.remove(group_name);
        if let Some(entry) = entry {
            // Drop MLS state for the departing group: take the service out
            // of the Group so its `delete()` runs before the entry drops.
            // Per-group scoring is dropped when the entry leaves the
            // registry below.
            if let Some(mls) = entry.write().await.take_mls() {
                let _ = mls.delete();
            }
        }
        self.cleanup_consensus_scope(group_name).await?;
        self.handler.on_leave_group(group_name).await?;
        Ok(())
    }

    /// Peer broadcast a commit candidate. If we were in Working, enter
    /// Freezing and — if we're a steward — build our own candidate too.
    async fn on_commit_candidate_received(&self, group_name: &str) -> Result<(), UserError> {
        let entry_arc = match self.lookup_entry(group_name).await {
            Some(e) => e,
            None => return Ok(()),
        };
        let (transitioned, outbound) = {
            let mut entry = entry_arc.write().await;
            if entry.current_state() != GroupState::Working {
                return Ok(());
            }

            entry.start_freezing();
            let epoch = entry.expect_mls()?.current_epoch()?;
            entry.group.ensure_freeze_round(epoch);

            let self_identity = self.identity().identity_bytes().to_vec();
            let outbound = if entry.steward.is_steward(&self_identity) {
                match entry.create_commit_candidate(&self_identity, &self.app_id) {
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
        let (members, current_epoch) = {
            let entry_arc = match self.lookup_entry(group_name).await {
                Some(e) => e,
                None => return Ok(()),
            };
            let entry = entry_arc.read().await;
            if entry.steward.current_list().is_some() {
                return Ok(());
            }
            let mls = entry.expect_mls()?;
            (entry.group_members()?, mls.current_epoch()?)
        };
        let local_default_peer_score = self.default_group_config.default_peer_score;
        if !validate_group_sync(
            group_name,
            &sync,
            current_epoch,
            &members,
            local_default_peer_score,
        )? {
            return Ok(());
        }

        let sn = sync.steward_members.len();
        self.apply_group_sync_to_entry(group_name, &sync).await?;

        info!(
            group = group_name,
            election_epoch = sync.election_epoch,
            stewards = sn,
            scores = sync.peer_scores.len(),
            timing = sync.timing.is_some(),
            "group sync applied"
        );
        Ok(())
    }

    async fn apply_group_sync_to_entry(
        &self,
        group_name: &str,
        sync: &GroupSync,
    ) -> Result<(), UserError> {
        let mut protocol_config =
            StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?;
        protocol_config.allow_subset_candidates = sync.allow_subset_candidates;

        let entry_arc = match self.lookup_entry(group_name).await {
            Some(e) => e,
            None => return Ok(()),
        };
        let mut entry = entry_arc.write().await;
        let sn = sync.steward_members.len();
        entry.steward.set_config(protocol_config);
        let _events = entry.steward.install_list(
            sync.election_epoch,
            &sync.steward_members,
            sn,
            sync.retry_round,
        )?;
        entry.steward.set_max_retries(sync.max_reelection_attempts);
        entry.scoring.set_threshold(sync.threshold_peer_score);
        let snapshot = ScoreSnapshot {
            diverged: sync
                .peer_scores
                .iter()
                .map(|ps| (ps.member_id.clone(), ps.score))
                .collect(),
        };
        // The GroupSync sender (an existing steward) holds the same
        // scores and is the canonical actor for any below-threshold
        // member in this snapshot — they'll submit
        // `SCORE_BELOW_THRESHOLD` from their own event chain. Drop our
        // events to avoid duplicate proposals from joiners.
        let _events = entry.scoring.apply_snapshot(&snapshot);
        entry.config.liveness_criteria_yes = sync.liveness_criteria_yes;
        entry.config.pending_update_max_epochs = sync.pending_update_max_epochs;
        if let Some(timing) = &sync.timing {
            let pt = &mut entry.phase_timer;
            pt.set_commit_inactivity_duration(std::time::Duration::from_millis(
                timing.commit_inactivity_duration_ms,
            ));
            pt.set_freeze_duration(std::time::Duration::from_millis(timing.freeze_duration_ms));
            pt.set_recovery_inactivity_duration(std::time::Duration::from_millis(
                timing.recovery_inactivity_duration_ms,
            ));
            entry.config.proposal_expiration =
                std::time::Duration::from_millis(timing.proposal_expiration_ms);
            entry.config.consensus_timeout =
                std::time::Duration::from_millis(timing.consensus_timeout_ms);
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

        let entry_arc = self
            .lookup_entry(&group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;

        match packet.subtopic.as_str() {
            WELCOME_SUBTOPIC => {
                self.process_welcome_packet(&group_name, &packet.payload)
                    .await
            }
            APP_MSG_SUBTOPIC => {
                let result = {
                    let mut entry = entry_arc.write().await;
                    if entry.mls().is_none() {
                        // App messages on a group we haven't joined yet → ignore.
                        return Ok(());
                    }
                    entry.process_inbound(&packet.payload)?
                };
                self.dispatch_inbound_result(&group_name, result).await
            }
            other => Err(UserError::Core(crate::core::CoreError::InvalidSubtopic(
                other.to_string(),
            ))),
        }
    }

    /// Welcome-subtopic dispatch. Two payload kinds:
    /// - `UserKeyPackage` — a peer wants to join. If we already have an MLS
    ///   service for this group and the candidate isn't a member, surface
    ///   it as a membership-change request.
    /// - `InvitationToJoin` — try the welcome factory. If it returns
    ///   `Some(svc)`, attach to the entry and fire the join flow.
    async fn process_welcome_packet(
        &self,
        group_name: &str,
        payload: &[u8],
    ) -> Result<(), UserError> {
        let welcome_msg = WelcomeMessage::decode(payload)?;
        match welcome_msg.payload {
            Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;

                let entry_arc = self
                    .lookup_entry(group_name)
                    .await
                    .ok_or(UserError::GroupNotFound)?;
                let already_member = {
                    let entry = entry_arc.read().await;
                    entry.mls().map(|m| m.is_member(&identity)).unwrap_or(false)
                };
                if already_member {
                    info!(
                        group = group_name,
                        identity = %ShortId(&identity),
                        "key package skipped: already a member"
                    );
                    return Ok(());
                }

                info!(
                    group = group_name,
                    identity = %ShortId(&identity),
                    "key package received"
                );

                let gur = GroupUpdateRequest {
                    payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                        key_package_bytes,
                        identity,
                    })),
                };
                self.handle_incoming_update_request(group_name, gur).await
            }
            Some(welcome_message::Payload::InvitationToJoin(invitation)) => {
                let entry_arc = self
                    .lookup_entry(group_name)
                    .await
                    .ok_or(UserError::GroupNotFound)?;
                let self_id = self.identity().identity_bytes().to_vec();
                let already_in = {
                    let entry = entry_arc.read().await;
                    entry.steward.is_steward(&self_id) || entry.mls().is_some()
                };
                if already_in {
                    return Ok(());
                }

                let svc = (self.mls_welcome_factory)(&invitation.mls_message_out_bytes)?;
                let Some(svc) = svc else {
                    // Welcome wasn't for us.
                    return Ok(());
                };
                let joined_name = svc.group_id().to_string();
                {
                    let mut entry = entry_arc.write().await;
                    entry.attach_mls(svc);
                }
                info!(group = group_name, "joined group via welcome");
                self.dispatch_inbound_result(
                    group_name,
                    crate::core::ProcessResult::JoinedGroup(joined_name),
                )
                .await
            }
            None => Ok(()),
        }
    }
}

/// Returns `true` when the sync is acceptable for application. Logs the
/// rejection reason on `false`.
///
/// `members` is the joiner's current MLS member set; ghost stewards
/// (removed since the list was elected) are tolerated as long as at
/// least one listed steward is still present.
///
/// `local_default_peer_score` is the joiner's configured starting score
/// for new members (not synced; per-node). Rejecting when it sits at
/// or below the synced threshold prevents a misconfiguration where every
/// new member added by this joiner starts already eligible for removal.
fn validate_group_sync(
    group_name: &str,
    sync: &GroupSync,
    current_epoch: u64,
    members: &[Vec<u8>],
    local_default_peer_score: i64,
) -> Result<bool, UserError> {
    if sync.election_epoch > current_epoch {
        info!(
            group = group_name,
            election_epoch = sync.election_epoch,
            current_epoch,
            "group sync rejected: election_epoch > current_epoch"
        );
        return Ok(false);
    }

    let members_set = member_set(members);
    let any_present = sync
        .steward_members
        .iter()
        .any(|s| members_set.contains(s.as_slice()));
    let ordering_valid = StewardList::validate(
        &sync.steward_members,
        sync.election_epoch,
        group_name.as_bytes(),
        &sync.steward_members,
        &StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?,
        sync.retry_round,
    )?;
    if !(any_present && ordering_valid) {
        info!(
            group = group_name,
            any_present,
            ordering = ordering_valid,
            "group sync rejected: invalid"
        );
        return Ok(false);
    }

    if let Some(timing) = &sync.timing
        && let Some(zero_field) = first_zero_timing_field(timing)
    {
        info!(
            group = group_name,
            field = zero_field,
            "group sync rejected: zero-valued timing field"
        );
        return Ok(false);
    }

    if local_default_peer_score <= sync.threshold_peer_score {
        info!(
            group = group_name,
            local_default_peer_score,
            threshold_peer_score = sync.threshold_peer_score,
            "group sync rejected: default_peer_score at or below threshold would mark new members removable on add"
        );
        return Ok(false);
    }
    Ok(true)
}

/// Name of the first zero-valued field in `timing`, or `None` if all
/// fields are non-zero. Zero in any timing field would short-circuit the
/// timer it drives (consensus_timeout firing immediately,
/// commit_inactivity breaking the inactivity tracker, etc.).
fn first_zero_timing_field(
    timing: &crate::protos::de_mls::messages::v1::TimingConfig,
) -> Option<&'static str> {
    if timing.commit_inactivity_duration_ms == 0 {
        Some("commit_inactivity_duration_ms")
    } else if timing.freeze_duration_ms == 0 {
        Some("freeze_duration_ms")
    } else if timing.proposal_expiration_ms == 0 {
        Some("proposal_expiration_ms")
    } else if timing.consensus_timeout_ms == 0 {
        Some("consensus_timeout_ms")
    } else if timing.recovery_inactivity_duration_ms == 0 {
        Some("recovery_inactivity_duration_ms")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protos::de_mls::messages::v1::TimingConfig;

    fn nonzero_timing() -> TimingConfig {
        TimingConfig {
            commit_inactivity_duration_ms: 60_000,
            freeze_duration_ms: 30_000,
            proposal_expiration_ms: 3_600_000,
            consensus_timeout_ms: 30_000,
            recovery_inactivity_duration_ms: 5_000,
        }
    }

    #[test]
    fn nonzero_timing_passes() {
        assert!(first_zero_timing_field(&nonzero_timing()).is_none());
    }

    fn valid_sync_with(threshold: i64) -> GroupSync {
        GroupSync {
            steward_members: vec![b"alice".to_vec()],
            election_epoch: 0,
            sn_min: 1,
            sn_max: 5,
            allow_subset_candidates: false,
            peer_scores: vec![],
            timing: Some(nonzero_timing()),
            retry_round: 0,
            max_reelection_attempts: 1,
            liveness_criteria_yes: true,
            threshold_peer_score: threshold,
            pending_update_max_epochs: 3,
        }
    }

    /// Joiner's `default_peer_score` strictly above the synced threshold
    /// — new members added by this joiner start safely above the bar.
    #[test]
    fn validate_accepts_default_above_threshold() {
        let sync = valid_sync_with(0);
        assert!(validate_group_sync("g", &sync, 0, &[b"alice".to_vec()], 100).unwrap());
    }

    /// Joiner's `default_peer_score` equal to the threshold — new members
    /// would start at threshold and `score <= threshold` already qualifies
    /// them for removal.
    #[test]
    fn validate_rejects_default_equal_to_threshold() {
        let sync = valid_sync_with(50);
        assert!(!validate_group_sync("g", &sync, 0, &[b"alice".to_vec()], 50).unwrap());
    }

    /// Joiner's `default_peer_score` below the threshold — every new
    /// member added by this joiner starts removable.
    #[test]
    fn validate_rejects_default_below_threshold() {
        let sync = valid_sync_with(100);
        assert!(!validate_group_sync("g", &sync, 0, &[b"alice".to_vec()], 50).unwrap());
    }

    #[test]
    fn each_zero_field_is_detected() {
        let cases = [
            (
                "commit_inactivity_duration_ms",
                TimingConfig {
                    commit_inactivity_duration_ms: 0,
                    ..nonzero_timing()
                },
            ),
            (
                "freeze_duration_ms",
                TimingConfig {
                    freeze_duration_ms: 0,
                    ..nonzero_timing()
                },
            ),
            (
                "proposal_expiration_ms",
                TimingConfig {
                    proposal_expiration_ms: 0,
                    ..nonzero_timing()
                },
            ),
            (
                "consensus_timeout_ms",
                TimingConfig {
                    consensus_timeout_ms: 0,
                    ..nonzero_timing()
                },
            ),
            (
                "recovery_inactivity_duration_ms",
                TimingConfig {
                    recovery_inactivity_duration_ms: 0,
                    ..nonzero_timing()
                },
            ),
        ];
        for (name, timing) in cases {
            assert_eq!(
                first_zero_timing_field(&timing),
                Some(name),
                "expected field {name} to be detected as zero"
            );
        }
    }
}
