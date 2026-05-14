//! Inbound packet dispatch and consensus-event handling.
//!
//! Layout:
//!
//! - `process_inbound_packet` and `process_welcome_packet` stay on `User`.
//!   The packet entry point does echo-dedup + name routing; the welcome
//!   handler reaches the per-conv plugin factory (`welcome_mls`), which
//!   isn't on the session.
//! - `dispatch_inbound_result` and every `ProcessResult` branch handler
//!   live on `SessionRunner`. They're associated functions taking
//!   `Arc<RwLock<Self>>` so they can release the runner lock across
//!   `.await` points without holding it during proposal lifecycles.
//! - `LeaveConversation` is split: the session-side helper
//!   `prepare_self_leave` does the protocol work (emit `Leaving`, take and
//!   delete the MLS service); the User-side wrapper drops the entry from
//!   the registry, cleans up the consensus scope, and broadcasts
//!   `ConversationLifecycle::Removed`. The session method returns
//!   [`DispatchOutcome::LeaveRequested`] so the caller knows to finish
//!   the lifecycle on the User side.

use std::sync::Arc;

use hashgraph_like_consensus::protos::consensus::v1::Proposal;
use prost::Message;
use tokio::sync::RwLock;
use tracing::{error, info};

use crate::{
    app::{
        ConversationState, SessionRunner, User, UserError, forward_incoming_vote,
        relay_incoming_proposal,
    },
    core::{
        ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory, CoreError,
        PeerScoringPlugin, ProcessResult, ProposalKind, ScoreSnapshot, SessionEvent, StewardList,
        StewardListConfig, StewardListPlugin, member_set,
    },
    ds::{APP_MSG_SUBTOPIC, InboundPacket, WELCOME_SUBTOPIC},
    identity::ShortId,
    mls_crypto::{MlsService, key_package_bytes_from_json},
    protos::de_mls::messages::v1::{
        AppMessage, ConversationMessage, ConversationSync, ConversationUpdateRequest, InviteMember,
        TimingConfig, WelcomeMessage, conversation_update_request, welcome_message,
    },
};

/// What [`SessionRunner::dispatch_inbound_result`] hands back to the
/// caller. `LeaveRequested` signals that the session has done its
/// protocol-side teardown (emitted `Leaving`, deleted MLS state) and the
/// caller — which holds the User-side handles — must drop the entry
/// from the registry and broadcast the lifecycle removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// No further User-side action required.
    Done,
    /// The session has prepared itself for removal; the caller should
    /// remove the registry entry and clean up the consensus scope.
    LeaveRequested,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    /// Dispatch a single [`ProcessResult`] to its branch handler. Called
    /// by `User::process_inbound_packet` after the MLS-side
    /// `process_inbound` returns a result, and by other call sites that
    /// produce a `ProcessResult` (e.g. the welcome-side
    /// `JoinedConversation`).
    pub async fn dispatch_inbound_result(
        arc: &Arc<RwLock<Self>>,
        result: ProcessResult,
    ) -> Result<DispatchOutcome, UserError> {
        match result {
            ProcessResult::AppMessage(msg) => {
                arc.read().await.emit_event(SessionEvent::AppMessage(*msg));
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Proposal(proposal) => {
                Self::on_incoming_proposal(arc, *proposal).await?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Vote(vote) => {
                let s = arc.read().await;
                forward_incoming_vote::<P>(&s.handle.conversation, *vote, &s.consensus).await?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::MembershipChangeReceived(request) => {
                Self::handle_incoming_update_request(arc, *request).await?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::JoinedConversation(_name) => {
                // `name` is always this conversation's name — `process_inbound`
                // emits it via the local MLS service. Use the session's own
                // `conversation_name` rather than the parameter.
                Self::on_joined_conversation(arc).await?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::ConversationUpdated => {
                Self::on_conversation_updated(arc).await?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::LeaveConversation => {
                Self::prepare_self_leave(arc).await?;
                Ok(DispatchOutcome::LeaveRequested)
            }
            ProcessResult::CommitCandidateReceived { steward } => {
                Self::on_commit_candidate_received(arc, &steward).await?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::ConversationSyncReceived(sync) => {
                Self::on_conversation_sync(arc, *sync).await?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Noop(reason) => {
                let conv_name = arc.read().await.conversation_name.clone();
                tracing::debug!(
                    conversation = %conv_name,
                    ?reason,
                    "inbound dispatched as noop"
                );
                Ok(DispatchOutcome::Done)
            }
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
        arc: &Arc<RwLock<Self>>,
        proposal: Proposal,
    ) -> Result<(), UserError> {
        let decoded = ConversationUpdateRequest::decode(proposal.payload.as_slice()).ok();
        if let Some(req) = decoded.as_ref() {
            let mut s = arc.write().await;
            let current_epoch = match s.handle.mls() {
                Some(mls) => mls.current_epoch()?,
                None => 0,
            };
            match &req.payload {
                Some(conversation_update_request::Payload::EmergencyCriteria(_)) => {
                    s.handle
                        .conversation
                        .observe_emergency(proposal.proposal_id);
                }
                Some(conversation_update_request::Payload::InviteMember(_))
                | Some(conversation_update_request::Payload::RemoveMember(_)) => {
                    s.handle
                        .conversation
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
        let (consensus, conversation_name) = {
            let s = arc.read().await;
            (s.consensus.clone(), s.conversation_name.clone())
        };
        if let Some(vote_notification) =
            relay_incoming_proposal::<P>(&conversation_name, proposal, &consensus).await?
        {
            arc.read()
                .await
                .emit_event(SessionEvent::AppMessage(vote_notification));
        }
        // Skip auto-vote for fast-path proposals: the creator's bundled
        // YES already resolved the session, so the timer would hit a
        // closed session.
        if expected_voters > 1 {
            let (delay, vote) = {
                let s = arc.read().await;
                (
                    s.handle.config.voting_delay_for(kind),
                    s.handle.config.liveness_criteria_yes,
                )
            };
            Self::spawn_auto_vote(arc, proposal_id, delay, vote).await;
        }
        Ok(())
    }

    /// We just joined via welcome. Broadcast a system "joined" chat message,
    /// sync scoring, and transition to Working. Pending-update pruning is
    /// defensive — PendingJoin doesn't buffer, but paths may change.
    async fn on_joined_conversation(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        arc.write()
            .await
            .prune_pending_updates_after_commit()
            .await?;

        let (packet, mls_members, conversation_name) = {
            let s = arc.read().await;
            let msg: AppMessage = ConversationMessage {
                message: format!("User {} joined the conversation", s.identity_display)
                    .into_bytes(),
                sender: "SYSTEM".to_string(),
                conversation_name: s.conversation_name.clone(),
            }
            .into();
            let mls = s.handle.expect_mls()?;
            let packet = mls.build_message(&msg, &s.app_id)?;
            let members = s.handle.conversation_members().unwrap_or_default();
            (packet, members, s.conversation_name.clone())
        };
        arc.read().await.send_outbound(packet).await?;
        arc.read().await.emit_event(SessionEvent::Joined);
        arc.write().await.sync_scoring_members(&mls_members).await;

        let event = arc.write().await.start_working();
        arc.read()
            .await
            .emit_event(SessionEvent::PhaseChange(event));
        info!(conversation = %conversation_name, "joined conversation");
        Ok(())
    }

    /// A commit merged. Sync scoring + pending-update buffers, transition to
    /// Working, and run steward housekeeping (auto-fill, election kick-off,
    /// buffered-update drain). The commit author's `SuccessfulCommit`
    /// reward is emitted by `finalize_freeze_round`, not here.
    async fn on_conversation_updated(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let mls_members = {
            let s = arc.read().await;
            if s.handle.mls().is_some() {
                s.handle.conversation_members().unwrap_or_default()
            } else {
                Vec::new()
            }
        };
        arc.write().await.sync_scoring_members(&mls_members).await;
        arc.write()
            .await
            .prune_pending_updates_after_commit()
            .await?;

        // Transition to Working BEFORE steward checks (election needs Working
        // state). Reset reelection_round: this commit advanced the epoch,
        // so whatever retry cycle we were in belongs to the previous epoch.
        let working_event = {
            let mut s = arc.write().await;
            s.handle.steward_list.reset_retry();
            let state = s.handle.current_state();
            if matches!(
                state,
                ConversationState::Working
                    | ConversationState::Freezing
                    | ConversationState::Selection
                    | ConversationState::Reelection
            ) {
                Some(s.start_working())
            } else {
                None
            }
        };

        Self::steward_list_housekeeping(arc).await?;
        Self::process_buffered_updates(arc).await?;
        Self::maybe_close_recovery_window(arc).await;

        if let Some(event) = working_event {
            arc.read()
                .await
                .emit_event(SessionEvent::PhaseChange(event));
        }
        Ok(())
    }

    /// Fire a steward election while `recovery_mode` is set so the next
    /// list installs and closes the window.
    async fn maybe_close_recovery_window(arc: &Arc<RwLock<Self>>) {
        let in_recovery_mode = arc.read().await.handle.is_in_recovery_mode();
        if !in_recovery_mode {
            return;
        }
        if let Err(e) = Self::try_initiate_steward_election(arc, true, None).await {
            let conv_name = arc.read().await.conversation_name.clone();
            info!(
                conversation = %conv_name,
                error = %e,
                "post-recovery election deferred"
            );
        }
    }

    /// Protocol-side teardown for `LeaveConversation`: emit `Leaving` on
    /// the session's bus and delete the local MLS state. The User-side
    /// caller drops the entry from the registry and broadcasts
    /// `ConversationLifecycle::Removed`.
    async fn prepare_self_leave(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        arc.read().await.emit_event(SessionEvent::Leaving);
        if let Some(mls) = arc.write().await.handle.take_mls() {
            mls.delete()?;
        }
        Ok(())
    }

    /// Peer broadcast a commit candidate. If we were in Working, enter
    /// Freezing and — if we're a steward — build our own candidate too.
    async fn on_commit_candidate_received(
        arc: &Arc<RwLock<Self>>,
        steward: &[u8],
    ) -> Result<(), UserError> {
        {
            let conv_name = arc.read().await.conversation_name.clone();
            tracing::debug!(
                conversation = %conv_name,
                steward = %ShortId::new(steward),
                "candidate received from peer steward"
            );
        }
        let (event, outbound) = {
            let mut s = arc.write().await;
            if s.handle.current_state() != ConversationState::Working {
                return Ok(());
            }

            let event = s.start_freezing();
            let epoch = s.handle.expect_mls()?.current_epoch()?;
            s.handle.conversation.ensure_freeze_round(epoch);

            let self_identity = Arc::clone(&s.self_identity);
            let app_id = Arc::clone(&s.app_id);
            let outbound = if s.handle.steward_list.is_steward(&self_identity) {
                match s.handle.create_commit_candidate(&self_identity, &app_id) {
                    Ok(packets) => packets,
                    Err(e) => {
                        error!(
                            conversation = %s.conversation_name,
                            error = %e,
                            "own commit candidate build failed"
                        );
                        None
                    }
                }
            } else {
                None
            };
            (event, outbound)
        };

        arc.read()
            .await
            .emit_event(SessionEvent::PhaseChange(event));
        if let Some(message) = outbound {
            arc.read().await.send_outbound(message).await?;
        }
        Ok(())
    }

    /// Apply a steward's `ConversationSync` when we're a joiner without a steward
    /// list. Validates the proposed list against the members it carries
    /// (not the full MLS set — the list may have been generated before we
    /// existed), then applies list + protocol flags + timing + peer scores.
    async fn on_conversation_sync(
        arc: &Arc<RwLock<Self>>,
        sync: ConversationSync,
    ) -> Result<(), UserError> {
        let (members, current_epoch, local_default_peer_score, conversation_name) = {
            let s = arc.read().await;
            if s.handle.steward_list.current_list().is_some() {
                return Ok(());
            }
            let mls = s.handle.expect_mls()?;
            (
                s.handle.conversation_members()?,
                mls.current_epoch()?,
                s.handle.scoring.default_score(),
                s.conversation_name.clone(),
            )
        };
        if !validate_conversation_sync(
            &conversation_name,
            &sync,
            current_epoch,
            &members,
            local_default_peer_score,
        )? {
            return Ok(());
        }

        let sn = sync.steward_members.len();
        arc.write().await.apply_conversation_sync_to_entry(&sync)?;

        info!(
            conversation = %conversation_name,
            election_epoch = sync.election_epoch,
            stewards = sn,
            scores = sync.peer_scores.len(),
            timing = sync.timing.is_some(),
            "conversation sync applied"
        );
        Ok(())
    }

    fn apply_conversation_sync_to_entry(
        &mut self,
        sync: &ConversationSync,
    ) -> Result<(), UserError> {
        let mut protocol_config =
            StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?;
        protocol_config.allow_subset_candidates = sync.allow_subset_candidates;

        let sn = sync.steward_members.len();
        self.handle.steward_list.set_config(protocol_config);
        let _events = self.handle.steward_list.install_list(
            sync.election_epoch,
            &sync.steward_members,
            sn,
            sync.retry_round,
        )?;
        self.handle
            .steward_list
            .set_max_retries(sync.max_reelection_attempts);
        self.handle.scoring.set_threshold(sync.threshold_peer_score);
        let snapshot = ScoreSnapshot {
            diverged: sync
                .peer_scores
                .iter()
                .map(|ps| (ps.member_id.clone(), ps.score))
                .collect(),
        };
        // The ConversationSync sender (an existing steward) holds the same
        // scores and is the canonical actor for any below-threshold
        // member in this snapshot — they'll submit
        // `SCORE_BELOW_THRESHOLD` from their own event chain. Drop our
        // events to avoid duplicate proposals from joiners.
        let _events = self.handle.scoring.apply_snapshot(&snapshot);
        self.handle.config.liveness_criteria_yes = sync.liveness_criteria_yes;
        self.handle.config.pending_update_max_epochs = sync.pending_update_max_epochs;
        if let Some(timing) = &sync.timing {
            self.handle.config.apply_timing(timing);
        }
        Ok(())
    }
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Process an inbound packet. The User-level entry point owns echo
    /// dedup, name-based routing, and the welcome subtopic's plug-in-
    /// factory access. App-message packets are handed off to the session
    /// for MLS processing and dispatch.
    pub async fn process_inbound_packet(&self, packet: InboundPacket) -> Result<(), UserError> {
        let conversation_name = packet.conversation_id.clone();

        // Echo dedup: drop our own messages received back from pub/sub.
        if packet.app_id.as_slice() == &*self.app_id {
            return Ok(());
        }

        let entry_arc = self
            .lookup_entry(&conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;

        match packet.subtopic.as_str() {
            WELCOME_SUBTOPIC => {
                self.process_welcome_packet(&conversation_name, &packet.payload, &entry_arc)
                    .await
            }
            APP_MSG_SUBTOPIC => {
                let result = {
                    let mut entry = entry_arc.write().await;
                    if entry.handle.mls().is_none() {
                        return Ok(());
                    }
                    entry.handle.process_inbound(&packet.payload)?
                };
                self.finish_dispatch(&conversation_name, &entry_arc, result)
                    .await
            }
            other => Err(UserError::Core(CoreError::InvalidSubtopic(
                other.to_string(),
            ))),
        }
    }

    /// Welcome-subtopic dispatch. Two payload kinds:
    /// - `UserKeyPackage` — a peer wants to join. If we already have an MLS
    ///   service for this conversation and the candidate isn't a member, surface
    ///   it as a membership-change request.
    /// - `InvitationToJoin` — try the welcome factory. If it returns
    ///   `Some(svc)`, attach to the runner and fire the join flow.
    async fn process_welcome_packet(
        &self,
        conversation_name: &str,
        payload: &[u8],
        entry_arc: &Arc<RwLock<SessionRunner<P, CP>>>,
    ) -> Result<(), UserError> {
        let welcome_msg = WelcomeMessage::decode(payload)?;
        match welcome_msg.payload {
            Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;

                let already_member = {
                    let entry = entry_arc.read().await;
                    entry
                        .handle
                        .mls()
                        .map(|m| m.is_member(&identity))
                        .unwrap_or(false)
                };
                if already_member {
                    info!(
                        conversation = conversation_name,
                        identity = %ShortId::new(&identity),
                        "key package skipped: already a member"
                    );
                    return Ok(());
                }

                info!(
                    conversation = conversation_name,
                    identity = %ShortId::new(&identity),
                    "key package received"
                );

                let gur = ConversationUpdateRequest {
                    payload: Some(conversation_update_request::Payload::InviteMember(
                        InviteMember {
                            key_package_bytes,
                            identity,
                        },
                    )),
                };
                SessionRunner::handle_incoming_update_request(entry_arc, gur).await
            }
            Some(welcome_message::Payload::InvitationToJoin(invitation)) => {
                let self_id = self.self_identity();
                let already_in = {
                    let entry = entry_arc.read().await;
                    entry.handle.steward_list.is_steward(self_id) || entry.handle.mls().is_some()
                };
                if already_in {
                    return Ok(());
                }

                let svc = self
                    .plugins
                    .conversation_plugins
                    .welcome_mls(&invitation.mls_message_out_bytes)?;
                let Some(svc) = svc else {
                    // Welcome wasn't for us.
                    return Ok(());
                };
                let joined_name = svc.conversation_id().to_string();
                {
                    let mut entry = entry_arc.write().await;
                    entry.handle.attach_mls(svc);
                }
                info!(
                    conversation = conversation_name,
                    "joined conversation via welcome"
                );
                self.finish_dispatch(
                    conversation_name,
                    entry_arc,
                    ProcessResult::JoinedConversation(joined_name),
                )
                .await
            }
            None => Ok(()),
        }
    }

    /// Drive the session-side dispatcher and finish lifecycle work on the
    /// User side when the session signals `LeaveRequested`.
    async fn finish_dispatch(
        &self,
        conversation_name: &str,
        entry_arc: &Arc<RwLock<SessionRunner<P, CP>>>,
        result: ProcessResult,
    ) -> Result<(), UserError> {
        let outcome = SessionRunner::dispatch_inbound_result(entry_arc, result).await?;
        if matches!(outcome, DispatchOutcome::LeaveRequested) {
            self.finalize_self_leave(conversation_name).await?;
        }
        Ok(())
    }

    /// User-side completion of `LeaveConversation`: drop the entry from
    /// the registry, clean up the consensus scope, and broadcast removal.
    /// The session-side teardown (emit `Leaving`, delete MLS state) runs
    /// inside [`SessionRunner::dispatch_inbound_result`] /
    /// [`SessionRunner::poll_freeze_status`] /
    /// [`SessionRunner::check_pending_join`]; this method is the cleanup
    /// callers run when those signal "registry should be removed"
    /// (`DispatchOutcome::LeaveRequested` or `PendingJoinTick::Expired`).
    pub async fn finalize_self_leave(&self, conversation_name: &str) -> Result<(), UserError> {
        self.conversations.write().await.remove(conversation_name);
        self.cleanup_consensus_scope(conversation_name).await?;
        let _ = self.lifecycle.send(ConversationLifecycle::Removed(
            conversation_name.to_string(),
        ));
        Ok(())
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
fn validate_conversation_sync(
    conversation_name: &str,
    sync: &ConversationSync,
    current_epoch: u64,
    members: &[Vec<u8>],
    local_default_peer_score: i64,
) -> Result<bool, UserError> {
    if sync.election_epoch > current_epoch {
        info!(
            conversation = conversation_name,
            election_epoch = sync.election_epoch,
            current_epoch,
            "conversation sync rejected: election_epoch > current_epoch"
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
        conversation_name.as_bytes(),
        &sync.steward_members,
        &StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?,
        sync.retry_round,
    )?;
    if !(any_present && ordering_valid) {
        info!(
            conversation = conversation_name,
            any_present,
            ordering = ordering_valid,
            "conversation sync rejected: invalid"
        );
        return Ok(false);
    }

    if let Some(timing) = &sync.timing
        && let Some(zero_field) = first_zero_timing_field(timing)
    {
        info!(
            conversation = conversation_name,
            field = zero_field,
            "conversation sync rejected: zero-valued timing field"
        );
        return Ok(false);
    }

    if local_default_peer_score <= sync.threshold_peer_score {
        info!(
            conversation = conversation_name,
            local_default_peer_score,
            threshold_peer_score = sync.threshold_peer_score,
            "conversation sync rejected: default_peer_score at or below threshold would mark new members removable on add"
        );
        return Ok(false);
    }
    Ok(true)
}

/// Name of the first zero-valued field in `timing`, or `None` if all
/// fields are non-zero. Zero in any timing field would short-circuit the
/// timer it drives (consensus_timeout firing immediately,
/// commit_inactivity breaking the inactivity tracker, etc.).
fn first_zero_timing_field(timing: &TimingConfig) -> Option<&'static str> {
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

    fn valid_sync_with(threshold: i64) -> ConversationSync {
        ConversationSync {
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
        assert!(validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()], 100).unwrap());
    }

    /// Joiner's `default_peer_score` equal to the threshold — new members
    /// would start at threshold and `score <= threshold` already qualifies
    /// them for removal.
    #[test]
    fn validate_rejects_default_equal_to_threshold() {
        let sync = valid_sync_with(50);
        assert!(!validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()], 50).unwrap());
    }

    /// Joiner's `default_peer_score` below the threshold — every new
    /// member added by this joiner starts removable.
    #[test]
    fn validate_rejects_default_below_threshold() {
        let sync = valid_sync_with(100);
        assert!(!validate_conversation_sync("g", &sync, 0, &[b"alice".to_vec()], 50).unwrap());
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
