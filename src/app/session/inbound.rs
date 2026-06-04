//! Session-side inbound dispatch.
//!
//! `dispatch_inbound_result` and every `ProcessResult` branch handler live
//! on `SessionRunner` as associated functions taking `Arc<RwLock<Self>>` so
//! they can release the runner lock across `` points without holding
//! it during proposal lifecycles.
//!
//! `LeaveConversation` is split: the session-side helper `prepare_self_leave`
//! does the protocol work (emit `Leaving`, take and delete the MLS service);
//! the User-side caller drops the entry from the registry, cleans up the
//! consensus scope, and broadcasts `ConversationLifecycle::Removed`. The
//! session method returns [`DispatchOutcome::LeaveRequested`] so the caller
//! knows to finish the lifecycle on the User side.

use std::sync::{Arc, RwLock};

use hashgraph_like_consensus::protos::consensus::v1::Proposal;
use prost::Message;
use tracing::{error, info};

use crate::{
    app::{
        ConversationState, LockExt, SessionRunner, UserError,
        session::{
            consensus::build_vote_banner_event,
            consensus_bridge::{forward_incoming_proposal, forward_incoming_vote},
            runner::send_packet,
        },
    },
    core::{
        ConsensusPlugin, ConversationPluginsFactory, PeerScoringPlugin, ProcessResult,
        ProposalKind, ScoreSnapshot, SessionEvent, StewardList, StewardListConfig,
        StewardListPlugin, member_set,
    },
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::{
        AppMessage, ConversationMessage, ConversationSync, ConversationUpdateRequest, TimingConfig,
        conversation_update_request,
    },
};

/// What `SessionRunner::dispatch_inbound_result` hands back to the
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
    pub(crate) fn dispatch_inbound_result(
        arc: &Arc<RwLock<Self>>,
        result: ProcessResult,
    ) -> Result<DispatchOutcome, UserError> {
        match result {
            ProcessResult::AppMessage(msg) => {
                arc.read_or_err("session")?
                    .emit_event(SessionEvent::AppMessage(*msg));
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Proposal(proposal) => {
                Self::on_incoming_proposal(arc, *proposal)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Vote(vote) => {
                let proposal_id = vote.proposal_id;
                let (consensus, conversation_id, outcome_applied) = {
                    let s = arc.read_or_err("session")?;
                    (
                        s.consensus.clone(),
                        s.conversation_id.clone(),
                        s.conversation
                            .conversation
                            .is_consensus_outcome_applied(proposal_id),
                    )
                };
                forward_incoming_vote::<P>(&conversation_id, *vote, &consensus, outcome_applied)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::MembershipChangeReceived(request) => {
                Self::handle_incoming_update_request(arc, *request)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::JoinedConversation(_name) => {
                // `name` is always this conversation's name — `process_inbound`
                // emits it via the local MLS service. Use the session's own
                // `conversation_id` rather than the parameter.
                Self::on_joined_conversation(arc)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::ConversationUpdated => {
                Self::on_conversation_updated(arc)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::LeaveConversation => {
                Self::prepare_self_leave(arc)?;
                Ok(DispatchOutcome::LeaveRequested)
            }
            ProcessResult::CommitCandidateReceived {
                steward_id: steward,
            } => {
                Self::on_commit_candidate_received(arc, &steward)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::ConversationSyncReceived(sync) => {
                Self::on_conversation_sync(arc, *sync)?;
                Ok(DispatchOutcome::Done)
            }
            ProcessResult::Noop(reason) => {
                let conv_name = arc.read_or_err("session")?.conversation_id.clone();
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
    fn on_incoming_proposal(arc: &Arc<RwLock<Self>>, proposal: Proposal) -> Result<(), UserError> {
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
            let mut s = arc.write_or_err("session")?;
            let current_epoch = match s.conversation.mls() {
                Some(mls) => mls.current_epoch()?,
                None => 0,
            };
            match &req.payload {
                Some(conversation_update_request::Payload::EmergencyCriteria(_)) => {
                    s.conversation
                        .conversation
                        .insert_emergency(proposal.proposal_id);
                }
                Some(conversation_update_request::Payload::MemberInvite(_))
                | Some(conversation_update_request::Payload::RemoveMember(_)) => {
                    s.conversation
                        .conversation
                        .insert_pending_update(req.clone(), current_epoch);
                }
                _ => {}
            }
        }
        let proposal_id = proposal.proposal_id;
        let expected_voters = proposal.expected_voters_count;
        let payload = proposal.payload.clone();
        let kind = decoded
            .as_ref()
            .map(ProposalKind::of)
            .unwrap_or(ProposalKind::Commit);
        let (consensus, conversation_id) = {
            let s = arc.read_or_err("session")?;
            (s.consensus.clone(), s.conversation_id.clone())
        };
        forward_incoming_proposal::<P>(&conversation_id, proposal, &consensus)?;
        // Skip the banner + auto-vote for fast-path proposals: the
        // creator's bundled YES already resolved the session, so peers have
        // nothing to vote on.
        if expected_voters > 1 {
            let banner = build_vote_banner_event(&conversation_id, proposal_id, payload);
            arc.read_or_err("session")?
                .emit_event(SessionEvent::AppMessage(banner));
            let (delay, vote) = {
                let s = arc.read_or_err("session")?;
                (
                    s.conversation.config.voting_delay_for(kind),
                    s.conversation.config.liveness_criteria_yes,
                )
            };
            arc.write_or_err("session")?
                .register_auto_vote(proposal_id, delay, vote);
        }
        Ok(())
    }

    /// We just joined via welcome. Broadcast a system "joined" chat
    /// message, seed scoring with the current member set, and
    /// transition to Working.
    fn on_joined_conversation(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let (packet, mls_members, conversation_id) = {
            let mut s = arc.write_or_err("session")?;
            let msg: AppMessage = ConversationMessage {
                message: format!("User {} joined the conversation", s.member_id_display)
                    .into_bytes(),
                sender: "SYSTEM".to_string(),
                conversation_id: s.conversation_id.clone(),
            }
            .into();
            let app_id = Arc::clone(&s.app_id);
            let conversation_id = s.conversation_id.clone();
            let mls = s.conversation.expect_mls_mut()?;
            let members = mls.members().unwrap_or_default();
            let packet = mls.build_message(&msg, &app_id)?;
            (packet, members, conversation_id)
        };
        let transport = Arc::clone(arc.read_or_err("session")?.transport());
        send_packet(&transport, packet)?;
        arc.write_or_err("session")?
            .sync_scoring_members(&mls_members);

        let event = arc.write_or_err("session")?.start_working();
        arc.read_or_err("session")?
            .emit_event(SessionEvent::PhaseChange(event));
        info!(conversation = %conversation_id, "joined conversation");
        Ok(())
    }

    /// A commit merged. Sync scoring + pending-update buffers, transition to
    /// Working, and run steward housekeeping (list reconcile, election
    /// kick-off, buffered-update drain). The commit author's `SuccessfulCommit`
    /// reward is emitted by `finalize_freeze_round`, not here.
    fn on_conversation_updated(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        let mls_members = {
            let s = arc.read_or_err("session")?;
            match s.conversation.mls() {
                Some(mls) => mls.members().unwrap_or_default(),
                None => Vec::new(),
            }
        };
        arc.write_or_err("session")?
            .sync_scoring_members(&mls_members);
        arc.write_or_err("session")?
            .prune_pending_updates_after_commit()?;

        // Transition to Working BEFORE steward checks (election needs Working
        // state). Reset reelection_round: this commit advanced the epoch,
        // so whatever retry cycle we were in belongs to the previous epoch.
        let working_event = {
            let mut s = arc.write_or_err("session")?;
            s.conversation.steward_list.reset_retry();
            let state = s.conversation.current_state();
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

        Self::steward_list_housekeeping(arc)?;
        Self::process_buffered_updates(arc)?;
        Self::maybe_close_recovery_window(arc);

        if let Some(event) = working_event {
            arc.read_or_err("session")?
                .emit_event(SessionEvent::PhaseChange(event));
        }
        Ok(())
    }

    /// Fire a steward election while `recovery_mode` is set so the next
    /// list installs and closes the window.
    fn maybe_close_recovery_window(arc: &Arc<RwLock<Self>>) {
        let in_recovery_mode = match arc.read_or_err("session") {
            Ok(s) => s.conversation.is_in_recovery_mode(),
            Err(e) => {
                tracing::warn!(error = %e, "recovery window check skipped: session lock poisoned");
                return;
            }
        };
        if !in_recovery_mode {
            return;
        }
        if let Err(e) = Self::initiate_steward_election(arc, true) {
            let conv_name = arc
                .read_or_err("session")
                .map(|s| s.conversation_id.clone())
                .unwrap_or_else(|_| "<poisoned>".to_string());
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
    fn prepare_self_leave(arc: &Arc<RwLock<Self>>) -> Result<(), UserError> {
        arc.read_or_err("session")?
            .emit_event(SessionEvent::Leaving);
        // Bind to a let so the write guard is dropped before `mls.delete()`
        // runs the storage I/O — otherwise the guard's `if let` scrutinee
        // lifetime would keep every other task on this session blocked
        // throughout the delete.
        let taken_mls = arc.write_or_err("session")?.conversation.take_mls();
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
        arc: &Arc<RwLock<Self>>,
        steward: &[u8],
    ) -> Result<(), UserError> {
        {
            let conv_name = arc.read_or_err("session")?.conversation_id.clone();
            tracing::debug!(
                conversation = %conv_name,
                steward = ?steward,
                "candidate received from peer steward"
            );
        }
        let (event, outbound) = {
            let mut s = arc.write_or_err("session")?;
            // A valid commit landing supersedes a pending reelection: a member
            // that gave up waiting (Reelection) must still apply the steward's
            // commit, not ignore it. `start_freezing` is allowed from both
            // Working and Reelection; the buffered candidate is finalized on
            // the next poll.
            let state = s.conversation.current_state();
            if state != ConversationState::Working && state != ConversationState::Reelection {
                return Ok(());
            }

            let Some(event) = s.start_freezing() else {
                return Ok(());
            };
            let epoch = s.conversation.expect_mls()?.current_epoch()?;
            s.conversation.conversation.start_freeze_round(epoch);

            let self_member_id = Arc::clone(&s.self_member_id);
            let app_id = Arc::clone(&s.app_id);
            let outbound = if s.conversation.steward_list.is_steward(&self_member_id) {
                match s
                    .conversation
                    .create_commit_candidate(&self_member_id, &app_id)
                {
                    Ok(packets) => packets,
                    Err(e) => {
                        error!(
                            conversation = %s.conversation_id,
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

        arc.read_or_err("session")?
            .emit_event(SessionEvent::PhaseChange(event));
        if let Some(message) = outbound {
            let transport = Arc::clone(arc.read_or_err("session")?.transport());
            send_packet(&transport, message)?;
        }
        Ok(())
    }

    /// Apply a steward's `ConversationSync` when we're a joiner without a steward
    /// list. Validates the proposed list against the members it carries
    /// (not the full MLS set — the list may have been generated before we
    /// existed), then applies list + protocol flags + timing + peer scores.
    fn on_conversation_sync(
        arc: &Arc<RwLock<Self>>,
        sync: ConversationSync,
    ) -> Result<(), UserError> {
        let (members, current_epoch, local_default_peer_score, conversation_id) = {
            let s = arc.read_or_err("session")?;
            if s.conversation.steward_list.current_list().is_some() {
                return Ok(());
            }
            let mls = s.conversation.expect_mls()?;
            (
                mls.members()?,
                mls.current_epoch()?,
                s.conversation.scoring.default_score(),
                s.conversation_id.clone(),
            )
        };
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
        arc.write_or_err("session")?
            .apply_conversation_sync_to_entry(&sync)?;

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
    ) -> Result<(), UserError> {
        let mut protocol_config =
            StewardListConfig::new(sync.sn_min as usize, sync.sn_max as usize)?;
        protocol_config.allow_subset_candidates = sync.allow_subset_candidates;

        let sn = sync.steward_members.len();
        self.conversation.steward_list.set_config(protocol_config);
        let _events = self.conversation.steward_list.install_list(
            sync.election_epoch,
            &sync.steward_members,
            sn,
            sync.retry_round,
        )?;
        self.conversation
            .steward_list
            .set_max_retries(sync.max_reelection_attempts);
        self.conversation
            .scoring
            .set_threshold(sync.threshold_peer_score);
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
        let _ = self.conversation.scoring.apply_snapshot(&snapshot);
        self.conversation.config.liveness_criteria_yes = sync.liveness_criteria_yes;
        self.conversation.config.pending_update_max_epochs = sync.pending_update_max_epochs;
        if let Some(timing) = &sync.timing {
            self.conversation.config.apply_timing(timing);
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
) -> Result<bool, UserError> {
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
