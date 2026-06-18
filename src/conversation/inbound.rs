//! Inbound conversation-traffic processing.
//!
//! Two layers co-located:
//!
//! - The free function [`decode_inbound_payload`] decodes an app-subtopic payload
//!   into a [`ProcessResult`]. Welcome-subtopic packets are handled at the
//!   integrator layer (because the invitation path constructs a new
//!   `MlsService` via the user-supplied factory) and are not routed through
//!   this function.
//! - [`Conversation::process_inbound`] is the single entry point for all
//!   conversation traffic. It owns echo-dedup, `PendingJoin`-drop, and the
//!   internal `dispatch_inbound_result` chain. `LeaveConversation` is terminal:
//!   `prepare_self_leave` emits `Leaving`, cancels timers, and deletes local
//!   MLS state; the integrator then removes the registry entry and cleans up
//!   the consensus scope.

use openmls_traits::signatures::Signer;
use std::sync::Arc;

use hashgraph_like_consensus::protos::consensus::v1::Proposal;
use prost::Message;
use tracing::{error, info, warn};

use crate::{
    ConsensusPlugin, ConversationEvent, ConversationPluginsFactory, PeerScoringPlugin,
    ProcessResult, ProposalKind, ScoreSnapshot, StewardList, StewardListConfig, StewardListPlugin,
    conversation::{ConversationQueues, member_set},
    freeze::{buffer_commit_candidate, compute_commit_hash},
    mls_crypto::{DecryptResult, MlsService},
    process_result::NoopReason,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationSync, ConversationUpdateRequest, EventMembershipChange,
        MemberInvite, TimingConfig, TypeMembershipChange, app_message, conversation_update_request,
    },
};

use crate::{
    Conversation, ConversationError, ConversationState, consensus::bridge::forward_incoming_vote,
};

/// Fast-path proposals (`expected_voters_count == 1`) bypass peer voting, so
/// we restrict them to self-removal. Enforcing that the MLS-authenticated
/// sender matches the `RemoveMember` target closes the unilateral-removal
/// vector that an otherwise-free `expected_voters == 1` opens.
///
/// Returns `true` when the proposal is allowed. A mismatch (different
/// target, wrong payload variant, or undecodable) produces `false`.
fn authorize_fast_path_proposal(proposal: &Proposal, mls_sender: &[u8]) -> bool {
    if proposal.expected_voters_count != 1 {
        return true;
    }
    if proposal.proposal_owner != mls_sender {
        return false;
    }
    let Ok(request) = ConversationUpdateRequest::decode(proposal.payload.as_slice()) else {
        return false;
    };
    matches!(
        request.payload,
        Some(conversation_update_request::Payload::RemoveMember(ref r)) if r.member_id == mls_sender
    )
}

/// Process an inbound packet on the app subtopic and decide what action is
/// needed. Welcome-subtopic packets are handled at the integrator layer.
pub fn decode_inbound_payload<M: MlsService>(
    conversation: &mut ConversationQueues,
    mls: &mut M,
    payload: &[u8],
) -> Result<ProcessResult, ConversationError> {
    // 1. Try the plaintext envelopes (sent as plaintext AppMessage):
    //    CommitCandidate and MemberWelcome both have to be readable before
    //    the receiver merges the commit they belong to.
    if let Ok(app_message) = AppMessage::decode(payload) {
        match app_message.payload {
            Some(app_message::Payload::CommitCandidate(candidate)) => {
                return buffer_commit_candidate(conversation, mls, candidate);
            }
            Some(app_message::Payload::MemberWelcome(welcome)) => {
                if welcome.welcome_bytes.is_empty() {
                    return Ok(ProcessResult::Noop(NoopReason::EmptyWelcomePayload));
                }
                if !conversation
                    .record_welcome_broadcast(compute_commit_hash(&welcome.welcome_bytes))
                {
                    return Ok(ProcessResult::Noop(NoopReason::DuplicateWelcomeBroadcast));
                }
                return Ok(ProcessResult::WelcomeBroadcastReceived(Box::new(welcome)));
            }
            _ => {}
        }
    }

    // 2. MLS-encrypted app messages only — use decrypt_application_only.
    //    This NEVER stores proposals or processes commits, preventing
    //    rogue MLS proposals on the app subtopic from polluting state.
    let res = mls.decrypt_application_only(payload)?;

    match res {
        DecryptResult::Application(app_bytes, sender) => {
            let app_msg = AppMessage::decode(app_bytes.as_ref())?;
            if let Some(app_message::Payload::Proposal(proposal)) = &app_msg.payload
                && !authorize_fast_path_proposal(proposal, &sender)
            {
                warn!(
                    conversation = conversation.name(),
                    proposal_id = proposal.proposal_id,
                    sender = ?sender,
                    owner = ?proposal.proposal_owner,
                    "fast-path proposal rejected: sender is not the self-removal target"
                );
                return Ok(ProcessResult::Noop(NoopReason::FastPathRejected));
            }
            // Drop BanRequests whose target isn't in the conversation — saves a
            // useless consensus round.
            if let Some(app_message::Payload::BanRequest(ban)) = &app_msg.payload
                && !mls.is_member(&ban.user_to_ban)
            {
                info!(
                    conversation = conversation.name(),
                    target = ?ban.user_to_ban,
                    "ban request skipped: target not a member"
                );
                return Ok(ProcessResult::Noop(NoopReason::BanTargetNotMember));
            }
            app_msg.try_into()
        }
        DecryptResult::Removed(_) => Ok(ProcessResult::LeaveConversation),
        DecryptResult::Ignored => {
            tracing::debug!(
                conversation = conversation.name(),
                "app message ignored (wrong epoch/conversation)"
            );
            Ok(ProcessResult::Noop(NoopReason::DecryptIgnored))
        }
        _ => {
            warn!(
                conversation = conversation.name(),
                "unexpected MLS message type on app subtopic"
            );
            Ok(ProcessResult::Noop(NoopReason::UnexpectedMlsType))
        }
    }
}

/// What [`Conversation::process_inbound`] hands back to the integrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// No further integrator action required.
    Done,
    /// Packet was self-echoed or dropped (no MLS attached yet). No action.
    Dropped,
    /// The conversation has completed its protocol-side teardown (emitted
    /// `Leaving`, deleted MLS state). The integrator must remove the
    /// registry entry and clean up the consensus scope.
    LeaveRequested,
}

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> Conversation<P, CP> {
    /// Ingest a joiner's key-package announcement (a [`MemberInvite`]).
    /// Drops self-echoes (`sender == self.app_id`). If the holder isn't
    /// already a member, promotes the invite to a membership-change proposal
    /// the epoch steward can act on.
    pub fn receive_key_package(
        &mut self,
        sender: &[u8],
        payload: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        if sender == self.app_id.as_ref() {
            return Ok(());
        }
        let invite = MemberInvite::decode(payload)?;
        let already_member = self
            .mls()
            .map(|m| m.is_member(&invite.member_id))
            .unwrap_or(false);
        if already_member {
            info!(
                conversation = %self.conversation_id,
                member = ?invite.member_id,
                "key package skipped: already a member"
            );
            return Ok(());
        }
        info!(
            conversation = %self.conversation_id,
            member = ?invite.member_id,
            "key package received"
        );
        self.handle_incoming_update_request(
            ConversationUpdateRequest {
                payload: Some(conversation_update_request::Payload::MemberInvite(invite)),
            },
            signer,
        )
    }

    /// Decrypt and dispatch an inbound conversation payload. Drops self-echoes
    /// and packets arriving before MLS is attached (`PendingJoin`). Runs the
    /// full dispatch chain internally. Returns [`DispatchOutcome::LeaveRequested`]
    /// when the conversation has completed its protocol-side teardown; the integrator
    /// must then remove the registry entry and clean up the consensus scope.
    pub fn process_inbound(
        &mut self,
        sender: &[u8],
        payload: &[u8],
        signer: &impl Signer,
    ) -> Result<DispatchOutcome, ConversationError> {
        if sender == self.app_id.as_ref() {
            return Ok(DispatchOutcome::Dropped);
        }
        if !self.has_mls() {
            tracing::debug!(
                conversation = %self.conversation_id,
                "inbound dropped: MLS not attached (still PendingJoin)"
            );
            return Ok(DispatchOutcome::Dropped);
        }
        let result = self.decode_inbound(payload)?;
        self.dispatch_inbound_result(result, signer)
    }

    /// Whether this conversation's MLS service is attached. `false` for a
    /// joiner still in `PendingJoin` (no welcome yet).
    pub fn has_mls(&self) -> bool {
        self.mls().is_some()
    }

    /// Complete a join after receiving a welcome. Idempotent: returns
    /// immediately if the conversation is no longer in `PendingJoin`. Attaches
    /// the MLS service if absent, then dispatches `JoinedConversation`
    /// internally (broadcasts the join message, seeds scoring, transitions to
    /// `Working`).
    pub fn complete_join(
        &mut self,
        mls: CP::Mls,
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        if self.current_state() != ConversationState::PendingJoin {
            return Ok(());
        }
        if !self.has_mls() {
            self.attach_mls(mls);
        }
        self.dispatch_inbound_result(ProcessResult::JoinedConversation(), signer)?;
        Ok(())
    }

    pub fn apply_welcome_sync(
        &mut self,
        sync_bytes: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        if sync_bytes.is_empty() {
            return Ok(());
        }
        let result = self.decode_inbound(sync_bytes)?;
        self.dispatch_inbound_result(result, signer)?;
        Ok(())
    }

    pub(crate) fn dispatch_inbound_result(
        &mut self,
        result: ProcessResult,
        signer: &impl Signer,
    ) -> Result<DispatchOutcome, ConversationError> {
        match result {
            ProcessResult::AppMessage(msg) => {
                self.emit_event(ConversationEvent::AppMessage(*msg));
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Proposal(proposal) => {
                self.on_incoming_proposal(*proposal)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Vote(vote) => {
                let outcome_applied = self.queues.is_consensus_outcome_applied(vote.proposal_id);
                forward_incoming_vote::<P>(
                    &self.conversation_id,
                    *vote,
                    &self.consensus,
                    outcome_applied,
                )?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::MembershipChangeReceived(request) => {
                self.handle_incoming_update_request(*request, signer)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::JoinedConversation() => {
                self.on_joined_conversation(signer)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::ConversationUpdated => {
                self.on_conversation_updated(signer)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::LeaveConversation => {
                self.prepare_self_leave()?;
                Ok(DispatchOutcome::LeaveRequested)
            }
            ProcessResult::CommitCandidateReceived {
                steward_id: steward,
            } => {
                self.on_commit_candidate_received(&steward, signer)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::ConversationSyncReceived(sync) => {
                self.on_conversation_sync(*sync)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::WelcomeBroadcastReceived(welcome) => {
                self.emit_event(ConversationEvent::WelcomeReady {
                    welcome: *welcome,
                    minted_locally: false,
                });
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Noop(reason) => {
                tracing::debug!(
                    conversation = %self.conversation_id,
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
    fn on_incoming_proposal(&mut self, proposal: Proposal) -> Result<(), ConversationError> {
        let decoded = match ConversationUpdateRequest::decode(proposal.payload.as_slice()) {
            Ok(req) => Some(req),
            Err(e) => {
                tracing::debug!(
                    proposal_id = proposal.proposal_id,
                    error = %e,
                    "incoming proposal payload failed to decode; treated as opaque commit"
                );
                None
            }
        };
        if let Some(req) = decoded.as_ref() {
            let current_epoch = match self.mls() {
                Some(mls) => mls.current_epoch()?,
                None => 0,
            };
            match &req.payload {
                Some(conversation_update_request::Payload::EmergencyCriteria(_)) => {
                    self.queues.insert_emergency(proposal.proposal_id);
                }
                Some(conversation_update_request::Payload::MemberInvite(_))
                | Some(conversation_update_request::Payload::RemoveMember(_)) => {
                    self.queues
                        .insert_pending_update(req.clone(), current_epoch);
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

        let scope = P::Scope::from(self.conversation_id.clone());
        self.consensus.process_incoming_proposal(&scope, proposal)?;
        // Skip the vote request + auto-vote for fast-path proposals: the
        // creator's bundled YES already resolved the consensus session, so peers have
        // nothing to vote on.
        if expected_voters > 1 {
            // A votable peer proposal always decodes as a
            // `ConversationUpdateRequest`; an opaque payload can't be
            // surfaced for a vote, so only the auto-vote drives it.
            if let Some(request) = decoded {
                self.emit_event(ConversationEvent::VoteRequested {
                    proposal_id,
                    request,
                });
            }
            let delay = self.config.voting_delay_for(kind);
            let vote = self.config.liveness_criteria_yes;
            self.register_auto_vote(proposal_id, delay, vote);
        }
        Ok(())
    }

    /// We just joined via welcome. Broadcast a system "joined" chat
    /// message, seed scoring with the current member set, and
    /// transition to Working.
    fn on_joined_conversation(&mut self, signer: &impl Signer) -> Result<(), ConversationError> {
        let msg: AppMessage = EventMembershipChange {
            conversation_id: self.conversation_id.clone(),
            member: self.member_id_display().to_string(),
            change_type: TypeMembershipChange::Add as i32,
        }
        .into();
        let conversation_id = self.conversation_id.clone();
        let mls = self.expect_mls_mut()?;
        let mls_members = mls.members().unwrap_or_default();
        let payload = mls.build_message(signer, &msg)?;
        self.broadcast(payload);
        self.sync_scoring_members(&mls_members);

        let event = self.start_working();
        self.emit_event(ConversationEvent::PhaseChange(event));
        info!(conversation = %conversation_id, "joined conversation");
        Ok(())
    }

    /// A commit merged. Sync scoring + pending-update buffers, transition to
    /// Working, and run steward housekeeping (list reconcile, election
    /// kick-off, buffered-update drain). The commit author's `SuccessfulCommit`
    /// reward is emitted by `finalize_freeze_round`, not here.
    fn on_conversation_updated(&mut self, signer: &impl Signer) -> Result<(), ConversationError> {
        let mls_members = match self.mls() {
            Some(mls) => mls.members().unwrap_or_default(),
            None => Vec::new(),
        };
        self.sync_scoring_members(&mls_members);
        self.prune_pending_updates_after_commit()?;

        // Transition to Working BEFORE steward checks (election needs Working
        // state). Reset reelection_round: this commit advanced the epoch,
        // so whatever retry cycle we were in belongs to the previous epoch.
        self.steward_list.reset_retry();
        let state = self.current_state();
        let working_event = if matches!(
            state,
            ConversationState::Working
                | ConversationState::Freezing
                | ConversationState::Selection
                | ConversationState::Reelection
        ) {
            Some(self.start_working())
        } else {
            None
        };

        self.steward_list_housekeeping(signer)?;
        self.process_buffered_updates(signer)?;
        self.maybe_close_recovery_window(signer);

        if let Some(event) = working_event {
            self.emit_event(ConversationEvent::PhaseChange(event));
        }
        Ok(())
    }

    /// Fire a steward election while `recovery_mode` is set so the next
    /// list installs and closes the window.
    fn maybe_close_recovery_window(&mut self, signer: &impl Signer) {
        if !self.is_in_recovery_mode() {
            return;
        }
        if let Err(e) = self.initiate_steward_election(true, signer) {
            info!(
                conversation = %self.conversation_id,
                error = %e,
                "post-recovery election deferred"
            );
        }
    }

    /// Protocol-side teardown for `LeaveConversation`: emit `Leaving`, cancel
    /// pending timers, and delete the local MLS state. The integrator removes
    /// the registry entry and cleans up the consensus scope.
    fn prepare_self_leave(&mut self) -> Result<(), ConversationError> {
        self.emit_event(ConversationEvent::Leaving);
        self.cancel_all_auto_votes();
        let taken_mls = self.take_mls();
        if let Some(mut mls) = taken_mls {
            // The leave is already committed (`Leaving` emitted, MLS
            // detached); a delete failure must log and continue, else the
            // caller never reaches `LeaveRequested` and the entry leaks.
            if let Err(e) = mls.delete() {
                error!(error = %e, "self-leave: MLS storage delete failed; leaving anyway");
            }
        }
        Ok(())
    }

    /// Peer broadcast a commit candidate. If we were in Working, enter
    /// Freezing and — if we're a steward — build our own candidate too.
    fn on_commit_candidate_received(
        &mut self,
        steward: &[u8],
        signer: &impl Signer,
    ) -> Result<(), ConversationError> {
        tracing::debug!(
            conversation = %self.conversation_id,
            steward = ?steward,
            "candidate received from peer steward"
        );
        let state = self.current_state();
        if state != ConversationState::Working && state != ConversationState::Reelection {
            return Ok(());
        }

        let Some(event) = self.start_freezing() else {
            return Ok(());
        };
        let epoch = self.expect_mls()?.current_epoch()?;
        self.queues.start_freeze_round(epoch);

        let self_member_id = Arc::clone(&self.self_member_id);
        let outbound = if self.steward_list.is_steward(&self_member_id) {
            match self.create_commit_candidate(signer, &self_member_id) {
                Ok(payload) => payload,
                Err(e) => {
                    error!(
                        conversation = %self.conversation_id,
                        error = %e,
                        "own commit candidate build failed"
                    );
                    None
                }
            }
        } else {
            None
        };

        self.emit_event(ConversationEvent::PhaseChange(event));
        if let Some(payload) = outbound {
            self.broadcast(payload);
        }
        Ok(())
    }

    /// Apply a steward's `ConversationSync` when we're a joiner without a steward
    /// list. Validates the proposed list against the members it carries
    /// (not the full MLS set — the list may have been generated before we
    /// existed), then applies list + protocol flags + timing + peer scores.
    fn on_conversation_sync(&mut self, sync: ConversationSync) -> Result<(), ConversationError> {
        if self.steward_list.current_list().is_some() {
            return Ok(());
        }
        let conversation_id = self.conversation_id.clone();
        let (members, current_epoch) = {
            let mls = self.expect_mls()?;
            (mls.members()?, mls.current_epoch()?)
        };
        let local_default_peer_score = self.scoring.default_score();
        if !validate_conversation_sync(
            &conversation_id,
            &sync,
            current_epoch,
            &members,
            local_default_peer_score,
        )? {
            return Ok(());
        }

        let sn = sync.steward_members.len();
        self.apply_conversation_sync_to_entry(&sync)?;

        info!(
            conversation = %conversation_id,
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
    ) -> Result<(), ConversationError> {
        let mut protocol_config =
            StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?;
        protocol_config.allow_subset_candidates = sync.allow_subset_candidates;

        let sn = sync.steward_members.len();
        self.steward_list.set_config(protocol_config);
        self.steward_list.install_list(
            sync.election_epoch,
            &sync.steward_members,
            sn,
            sync.retry_round,
        )?;
        self.steward_list
            .set_max_retries(sync.max_reelection_attempts);
        self.scoring.set_threshold(sync.threshold_peer_score);
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
        // result to avoid duplicate proposals from joiners.
        let _ = self.scoring.apply_snapshot(&snapshot);
        self.config.liveness_criteria_yes = sync.liveness_criteria_yes;
        self.config.pending_update_max_epochs = sync.pending_update_max_epochs;
        if let Some(timing) = &sync.timing {
            self.config.apply_timing(timing);
        }
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
    conversation_id: &str,
    sync: &ConversationSync,
    current_epoch: u64,
    members: &[Vec<u8>],
    local_default_peer_score: i64,
) -> Result<bool, ConversationError> {
    if sync.election_epoch > current_epoch {
        info!(
            conversation = conversation_id,
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
        conversation_id.as_bytes(),
        &sync.steward_members,
        &StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?,
        sync.retry_round,
    )?;
    if !(any_present && ordering_valid) {
        info!(
            conversation = conversation_id,
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
            conversation = conversation_id,
            field = zero_field,
            "conversation sync rejected: zero-valued timing field"
        );
        return Ok(false);
    }

    if local_default_peer_score <= sync.threshold_peer_score {
        info!(
            conversation = conversation_id,
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
mod decode_inbound_payload_tests {
    use super::*;
    use crate::conversation::self_leave_proposal_id;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn remove_payload(member_id: &[u8]) -> Vec<u8> {
        ConversationUpdateRequest::remove_member(member_id.to_vec()).encode_to_vec()
    }

    fn proposal_for_self_remove(sender: &[u8], expected_voters: u32) -> Proposal {
        Proposal {
            name: "test".into(),
            payload: remove_payload(sender),
            proposal_id: self_leave_proposal_id(sender),
            proposal_owner: sender.to_vec(),
            votes: Vec::new(),
            expected_voters_count: expected_voters,
            round: 1,
            timestamp: 0,
            expiration_timestamp: u64::MAX,
            liveness_criteria_yes: true,
        }
    }

    #[test]
    fn fast_path_allows_self_removal_matching_sender() {
        let sender = member(1);
        let proposal = proposal_for_self_remove(&sender, 1);
        assert!(authorize_fast_path_proposal(&proposal, &sender));
    }

    #[test]
    fn fast_path_rejects_target_other_than_sender() {
        let sender = member(1);
        let victim = member(2);
        let mut proposal = proposal_for_self_remove(&victim, 1);
        proposal.proposal_owner = sender.clone();
        assert!(!authorize_fast_path_proposal(&proposal, &sender));
    }

    #[test]
    fn fast_path_rejects_owner_mismatch() {
        let sender = member(1);
        let imposter = member(3);
        let mut proposal = proposal_for_self_remove(&sender, 1);
        proposal.proposal_owner = imposter;
        assert!(!authorize_fast_path_proposal(&proposal, &sender));
    }

    #[test]
    fn fast_path_rejects_non_remove_payload() {
        let sender = member(1);
        let mut proposal = proposal_for_self_remove(&sender, 1);
        proposal.payload = vec![0xff; 8]; // garbage
        assert!(!authorize_fast_path_proposal(&proposal, &sender));
    }

    #[test]
    fn expected_voters_gt_one_bypasses_authz() {
        let sender = member(1);
        let victim = member(2);
        let mut proposal = proposal_for_self_remove(&victim, 5);
        proposal.proposal_owner = sender.clone();
        // Regular proposals (voters > 1) aren't gated by this check — peer
        // voting provides the authorization instead.
        assert!(authorize_fast_path_proposal(&proposal, &sender));
    }
}

#[cfg(test)]
mod conversation_sync_tests {
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
