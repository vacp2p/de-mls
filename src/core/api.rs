//! Core API for MLS group operations.
//!
//! This module provides the fundamental building blocks for MLS group management.
//! All functions operate on [`GroupHandle`] instances for app-level state and
//! [`MlsService`] for MLS cryptographic operations.
//!
//! # Overview
//!
//! The API is organized into three categories:
//!
//! ## Group Lifecycle
//! - [`create_group`] - Create a new group as steward
//! - [`prepare_to_join`] - Prepare a handle for joining
//! - [`join_group_from_invite`] - Complete join with welcome message
//! - [`become_steward`] / [`resign_steward`] - Steward role management
//!
//! ## Message Operations
//! - [`build_message`] - Encrypt an application message for the group
//! - [`build_key_package_message`] - Create key package for joining
//!
//! ## Inbound Processing
//! - [`process_inbound`] - Process received packets, returns [`ProcessResult`]
//!
//! ## Steward Operations
//! - [`create_commit_candidate`] - Build/broadcast commit candidate (no immediate merge)
//! - [`finalize_freeze_round`] - Select and apply a buffered candidate
//!
//! # Typical Flow
//!
//! ```text
//! Steward creates group:
//!   mls.create_group(name) → mls state initialized
//!   create_group(name) → GroupHandle (steward=true)
//!
//! Member joins:
//!   prepare_to_join() → GroupHandle (steward=false)
//!   build_key_package_message() → send to network
//!   ... wait for welcome ...
//!   process_inbound(welcome) → ProcessResult::JoinedGroup
//!
//! Sending messages:
//!   build_message(app_msg) → OutboundPacket → send to network
//!
//! Receiving messages:
//!   process_inbound(payload) → match ProcessResult { ... }
//! ```

use openmls_rust_crypto::MemoryStorage;
use prost::Message;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use tracing::{info, warn};

use crate::core::{
    ProposalId,
    error::CoreError,
    group_handle::{BufferedCommitCandidate, GroupHandle},
    types::ProcessResult,
    types::invitation_from_bytes,
};
use crate::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use crate::mls_crypto::{
    CommitCandidate as MlsCommitCandidate, DeMlsStorage, DecryptResult, GroupUpdate,
    KeyPackageBytes, MlsMessageKind, MlsProposalAction, MlsService, StagedCommitResult,
    key_package_bytes_from_json,
};
use crate::protos::de_mls::messages::v1::{
    AppMessage, CommitCandidate, GroupUpdateRequest, InviteMember, UserKeyPackage,
    ViolationEvidence, WelcomeMessage, app_message, group_update_request, welcome_message,
};

// ─────────────────────────── Group Lifecycle ───────────────────────────

/// Create a new MLS group as the steward.
pub fn create_group<S>(name: &str, mls: &MlsService<S>) -> Result<GroupHandle, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    mls.create_group(name)?;
    Ok(GroupHandle::new_as_creator(name, mls.wallet_bytes()))
}

/// Prepare a handle for joining an existing group.
pub fn prepare_to_join(name: &str) -> GroupHandle {
    GroupHandle::new_for_join(name)
}

/// Complete joining a group using a welcome message.
pub fn join_group_from_invite<S>(
    handle: &mut GroupHandle,
    welcome_bytes: &[u8],
    mls: &MlsService<S>,
) -> Result<String, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = mls.join_group(welcome_bytes)?;
    handle.set_mls_initialized();
    Ok(group_name)
}

/// Become the steward of a group.
pub fn become_steward(handle: &mut GroupHandle) {
    handle.become_steward();
}

/// Resign as steward of a group.
pub fn resign_steward(handle: &mut GroupHandle) {
    handle.resign_steward();
}

// ─────────────────────────── Message Building ───────────────────────────

/// Build an MLS-encrypted application message.
pub fn build_message<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
    app_msg: &AppMessage,
) -> Result<OutboundPacket, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_mls_initialized() {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    let message_out = mls.encrypt(handle.group_name(), &app_msg.encode_to_vec())?;

    Ok(OutboundPacket::new(
        message_out,
        APP_MSG_SUBTOPIC,
        handle.group_name(),
        handle.app_id(),
    ))
}

/// Build a key package message for joining a group.
pub fn build_key_package_message<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
) -> Result<OutboundPacket, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let key_package = mls.generate_key_package()?;
    let welcome_msg: WelcomeMessage = UserKeyPackage {
        key_package_bytes: key_package.as_bytes().to_vec(),
    }
    .into();

    Ok(OutboundPacket::new(
        welcome_msg.encode_to_vec(),
        WELCOME_SUBTOPIC,
        handle.group_name(),
        handle.app_id(),
    ))
}

// ─────────────────────────── Inbound Processing ───────────────────────────

/// Process an inbound packet and determine what action is needed.
pub fn process_inbound<S>(
    handle: &mut GroupHandle,
    payload: &[u8],
    subtopic: &str,
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    match subtopic {
        WELCOME_SUBTOPIC => process_welcome_subtopic(handle, payload, mls),
        APP_MSG_SUBTOPIC => process_app_subtopic(handle, payload, mls),
        _ => Err(CoreError::InvalidSubtopic(subtopic.to_string())),
    }
}

fn process_welcome_subtopic<S>(
    handle: &mut GroupHandle,
    payload: &[u8],
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let welcome_msg = WelcomeMessage::decode(payload)?;
    match welcome_msg.payload {
        Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
            if handle.is_steward() {
                info!(
                    "Steward received key package for group {}",
                    handle.group_name()
                );
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;

                let gur = GroupUpdateRequest {
                    payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                        key_package_bytes,
                        identity,
                    })),
                };

                return Ok(ProcessResult::GetUpdateRequest(gur));
            }
            Ok(ProcessResult::Noop)
        }
        Some(welcome_message::Payload::InvitationToJoin(invitation)) => {
            if handle.is_steward() || handle.is_mls_initialized() {
                return Ok(ProcessResult::Noop);
            }

            if mls.is_welcome_for_us(&invitation.mls_message_out_bytes)? {
                let group_name = mls.join_group(&invitation.mls_message_out_bytes)?;
                handle.set_mls_initialized();
                info!(
                    "[process_welcome_subtopic]: Joined group {}",
                    handle.group_name()
                );
                return Ok(ProcessResult::JoinedGroup(group_name));
            }
            Ok(ProcessResult::Noop)
        }
        None => Ok(ProcessResult::Noop),
    }
}

fn process_app_subtopic<S>(
    handle: &mut GroupHandle,
    payload: &[u8],
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_mls_initialized() {
        return Ok(ProcessResult::Noop);
    }

    // 1. Try plaintext CommitCandidate (sent as plaintext AppMessage)
    if let Ok(app_message) = AppMessage::decode(payload) {
        if let Some(app_message::Payload::CommitCandidate(candidate)) = app_message.payload {
            return process_commit_candidate(handle, candidate, mls);
        }
    }

    // 2. MLS-encrypted app messages only — use decrypt_application_only.
    //    This NEVER stores proposals or processes commits, preventing
    //    rogue MLS proposals on the app subtopic from polluting state.
    let res = mls.decrypt_application_only(handle.group_name(), payload)?;

    match res {
        DecryptResult::Application(app_bytes, _sender) => {
            AppMessage::decode(app_bytes.as_ref())?.try_into()
        }
        DecryptResult::Removed(_) => Ok(ProcessResult::LeaveGroup),
        _ => {
            warn!("Unexpected MLS message type on app subtopic, ignoring");
            Ok(ProcessResult::Noop)
        }
    }
}

// ─────────────────────────── Batch Processing (internal helpers) ───────────────────────────

/// Build deferred welcome outbound packets from a chosen candidate.
///
/// Welcome messages are deferred until after commit merge so that joiners
/// don't advance to the new epoch before the steward does.
fn build_deferred_welcome(
    welcome_bytes: Option<Vec<u8>>,
    handle: &GroupHandle,
) -> Vec<OutboundPacket> {
    let Some(welcome_bytes) = welcome_bytes else {
        return vec![];
    };
    let welcome_msg: WelcomeMessage = invitation_from_bytes(welcome_bytes);
    vec![OutboundPacket::new(
        welcome_msg.encode_to_vec(),
        WELCOME_SUBTOPIC,
        handle.group_name(),
        handle.app_id(),
    )]
}

/// Compute a SHA-256 hash of the raw commit message bytes.
fn compute_commit_hash(commit_message: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(commit_message);
    hasher.finalize().to_vec()
}

/// Validate that candidate wire messages have the expected MLS kinds.
///
/// This is a cheap check only (deserialize + outer content type), with no
/// MLS state mutation.
fn has_valid_candidate_wire_kinds<S>(candidate: &CommitCandidate, mls: &MlsService<S>) -> bool
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    candidate.mls_proposals.iter().all(|proposal| {
        matches!(
            mls.inspect_message_kind(proposal),
            Ok(MlsMessageKind::Proposal)
        )
    }) && matches!(
        mls.inspect_message_kind(&candidate.commit_message),
        Ok(MlsMessageKind::Commit)
    )
}

/// Cleanup staged MLS state for a candidate being discarded.
fn discard_candidate_state<S>(
    handle: &mut GroupHandle,
    mls: &MlsService<S>,
    group_name: &str,
) -> Result<(), CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    mls.discard_staged_commit(group_name)?;
    handle.clear_freeze_round();
    Ok(())
}

/// Convert commit outcome into the corresponding process result.
fn process_result_from_commit_outcome(self_removed: bool) -> ProcessResult {
    if self_removed {
        ProcessResult::LeaveGroup
    } else {
        ProcessResult::GroupUpdated
    }
}

/// Cheap pre-checks only — no MLS state mutation.
///
/// 1. State check: only buffer during Freezing/Selection (freeze round active).
/// 2. Dedup: check commit_hash against committed + buffered sets.
/// 3. Shape check: mls_proposals and commit_message must be non-empty.
/// 4. Kind check: wire-level inspection (Proposal/Commit) with no MLS state mutation.
/// 5. Buffer as BufferedCommitCandidate.
fn process_commit_candidate<S>(
    handle: &mut GroupHandle,
    candidate_msg: CommitCandidate,
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // 1. Ensure a freeze round exists. If none is active but the member has
    //    approved proposals, auto-start one so the candidate isn't dropped
    //    due to timing mismatch between steward and member epoch boundaries.
    if handle.freeze_round().is_none() {
        if handle.approved_proposals_count() > 0 {
            handle.ensure_freeze_round();
        } else {
            tracing::debug!(
                "Ignoring candidate for group {}: no approved proposals",
                handle.group_name(),
            );
            return Ok(ProcessResult::Noop);
        }
    }

    // 2. Dedup
    let commit_hash = compute_commit_hash(&candidate_msg.commit_message);
    if handle.is_duplicate_commit_candidate(&commit_hash) {
        tracing::debug!(
            "Ignoring duplicate candidate for group {}: already processed/buffered",
            handle.group_name(),
        );
        return Ok(ProcessResult::Noop);
    }

    // 3. Shape check
    if candidate_msg.mls_proposals.is_empty() || candidate_msg.commit_message.is_empty() {
        tracing::debug!(
            "Ignoring candidate for group {}: empty proposals or commit",
            handle.group_name(),
        );
        return Ok(ProcessResult::Noop);
    }

    // 4. Kind check (wire-level only, no MLS state mutation)
    if !has_valid_candidate_wire_kinds(&candidate_msg, mls) {
        return Ok(ProcessResult::Noop);
    }

    // 5. Buffer
    let buffered = handle.buffer_freeze_candidate(BufferedCommitCandidate {
        candidate_msg,
        commit_hash,
        is_local_candidate: false,
        welcome_bytes: None,
    });
    if buffered && !handle.is_steward() {
        // Only members use CandidateBuffered as a state-transition signal.
        // Stewards manage their own epoch flow via start_steward_epoch.
        Ok(ProcessResult::CandidateBuffered)
    } else {
        Ok(ProcessResult::Noop)
    }
}

// ─────────────────────────── Freeze Round Finalization ───────────────────────────

/// Result of deterministic freeze finalization.
#[derive(Debug, Clone)]
pub enum FreezeFinalizeResult {
    /// A candidate was selected and applied.
    /// The optional outbound packets include deferred welcome messages that
    /// must be sent AFTER the commit is merged (to prevent joiners from
    /// advancing epoch before the steward).
    Applied {
        result: ProcessResult,
        outbound: Vec<OutboundPacket>,
    },
    /// No valid candidate could be selected/applied.
    NoCandidate,
}

/// Post-validation type for selection (internal to finalize_freeze_round).
struct ValidatedCandidate {
    candidate_msg: CommitCandidate,
    commit_hash: Vec<u8>,
    is_local_candidate: bool,
    actions_count: usize,
    /// Deferred welcome bytes (only present on local candidates with Add proposals).
    welcome_bytes: Option<Vec<u8>>,
}

/// Deterministically select and apply a buffered candidate for the active freeze round.
///
/// Three distinct phases:
/// 1. Pre-validation (cheap, no MLS processing)
/// 2. Selection (deterministic choice from pre-validated candidates)
/// 3. Application (MLS processing — chosen candidate only)
pub fn finalize_freeze_round<S>(
    handle: &mut GroupHandle,
    mls: &MlsService<S>,
) -> Result<FreezeFinalizeResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    handle.lock_freeze_round_selection();

    let candidates = match handle.freeze_round() {
        Some(round) if round.epoch == handle.current_epoch() => round.candidates.clone(),
        _ => return Ok(FreezeFinalizeResult::NoCandidate),
    };

    if candidates.is_empty() {
        return Ok(FreezeFinalizeResult::NoCandidate);
    }

    let local_proposals = handle.approved_proposals();
    // Count only non-emergency proposals for matching
    let expected_mls_count = local_proposals
        .values()
        .filter(|req| {
            !matches!(
                req.payload,
                Some(group_update_request::Payload::EmergencyCriteria(_))
            )
        })
        .count();

    // ── Phase 1: Pre-validation (cheap, no MLS processing) ──
    let mut pre_validated: Vec<ValidatedCandidate> = Vec::new();

    for candidate in &candidates {
        // Count match
        let allow_subset = handle.allow_subset_candidates();
        if !allow_subset && candidate.candidate_msg.mls_proposals.len() != expected_mls_count {
            continue;
        }
        if allow_subset && candidate.candidate_msg.mls_proposals.len() > expected_mls_count {
            continue;
        }

        // Shape: non-empty (already checked at buffer time, but defensive)
        if candidate.candidate_msg.mls_proposals.is_empty()
            || candidate.candidate_msg.commit_message.is_empty()
        {
            continue;
        }

        // Dedup: not already committed
        if handle.is_duplicate_commit_candidate(&candidate.commit_hash) {
            continue;
        }

        // Kind check (wire-level only)
        if !has_valid_candidate_wire_kinds(&candidate.candidate_msg, mls) {
            continue;
        }

        pre_validated.push(ValidatedCandidate {
            candidate_msg: candidate.candidate_msg.clone(),
            commit_hash: candidate.commit_hash.clone(),
            is_local_candidate: candidate.is_local_candidate,
            actions_count: candidate.candidate_msg.mls_proposals.len(),
            welcome_bytes: candidate.welcome_bytes.clone(),
        });
    }

    if pre_validated.is_empty() {
        return Ok(FreezeFinalizeResult::NoCandidate);
    }

    // ── Phase 2: Selection (deterministic choice) ──
    // Prefer local candidate (steward's own), then largest action set, then lowest commit_hash
    pre_validated.sort_by(|a, b| {
        // Local candidate first
        let local_cmp = b.is_local_candidate.cmp(&a.is_local_candidate);
        if local_cmp != std::cmp::Ordering::Equal {
            return local_cmp;
        }
        // Larger action set first
        let size_cmp = b.actions_count.cmp(&a.actions_count);
        if size_cmp != std::cmp::Ordering::Equal {
            return size_cmp;
        }
        // Lowest commit_hash as tiebreak
        a.commit_hash.cmp(&b.commit_hash)
    });

    let chosen = pre_validated.into_iter().next().unwrap();

    // ── Phase 3: Application (MLS processing — chosen candidate only) ──
    let group_name = handle.group_name().to_owned();

    // Local candidate: steward already validated when creating the commit
    if handle.is_steward() && chosen.is_local_candidate {
        let local_identity = mls.wallet_bytes();
        let self_removed = local_proposals.values().any(|req| {
            matches!(
                req.payload,
                Some(group_update_request::Payload::RemoveMember(ref remove))
                    if remove.identity == local_identity
            )
        });

        mls.merge_own_commit(&group_name)?;
        handle.set_steward_identity(local_identity);
        handle.record_committed_batch(chosen.commit_hash);
        handle.clear_approved_proposals();

        // Build deferred welcome packets now that commit is merged
        let outbound = build_deferred_welcome(chosen.welcome_bytes, handle);
        handle.clear_freeze_round();

        return Ok(FreezeFinalizeResult::Applied {
            result: process_result_from_commit_outcome(self_removed),
            outbound,
        });
    }

    // Remote candidate: discard own commit if steward
    if handle.is_steward() {
        if let Err(e) = mls.discard_own_commit(&group_name) {
            warn!("[finalize_freeze_round] failed to discard own commit: {e}");
        }
    }

    // Process each MLS proposal via process_candidate_proposal (stores in pending queue)
    let mut proposal_senders: Vec<Vec<u8>> = Vec::new();
    let mut mls_actions: Vec<MlsProposalAction> = Vec::new();

    for (i, proposal_bytes) in chosen.candidate_msg.mls_proposals.iter().enumerate() {
        match mls.process_candidate_proposal(&group_name, proposal_bytes) {
            Ok(DecryptResult::ProposalStored(sender, action)) => {
                proposal_senders.push(sender);
                mls_actions.push(action);
            }
            Ok(other) => {
                tracing::debug!(
                    "MLS proposal {} for group {} returned {:?} during application",
                    i,
                    group_name,
                    other,
                );
                discard_candidate_state(handle, mls, &group_name)?;
                return Ok(FreezeFinalizeResult::NoCandidate);
            }
            Err(e) => {
                tracing::debug!(
                    "MLS proposal {} for group {} failed during application: {}",
                    i,
                    group_name,
                    e,
                );
                discard_candidate_state(handle, mls, &group_name)?;
                return Ok(FreezeFinalizeResult::NoCandidate);
            }
        }
    }

    // Verify all proposal senders match each other
    if proposal_senders.len() > 1 {
        let first = &proposal_senders[0];
        if proposal_senders.iter().any(|s| s != first) {
            tracing::warn!(
                "Violation: proposals have different senders for group {}",
                group_name,
            );
            discard_candidate_state(handle, mls, &group_name)?;
            let steward_id = proposal_senders.into_iter().next().unwrap_or_default();
            return Ok(FreezeFinalizeResult::Applied {
                result: ProcessResult::ViolationDetected(ViolationEvidence::broken_mls_proposal(
                    steward_id,
                    handle.current_epoch(),
                    "proposals have different senders",
                )),
                outbound: vec![],
            });
        }
    }

    // Process commit
    let staged_result = match mls.process_commit(&group_name, &chosen.candidate_msg.commit_message)
    {
        Ok(result) => result,
        Err(e) => {
            tracing::debug!(
                "Commit for group {} failed to process during application: {}",
                group_name,
                e,
            );
            discard_candidate_state(handle, mls, &group_name)?;
            return Ok(FreezeFinalizeResult::NoCandidate);
        }
    };

    let (commit_sender, self_removed, commit_actions) = match staged_result {
        StagedCommitResult::Staged {
            sender_identity,
            self_removed,
            actions,
        } => (sender_identity, self_removed, actions),
        StagedCommitResult::Ignored => {
            discard_candidate_state(handle, mls, &group_name)?;
            return Ok(FreezeFinalizeResult::NoCandidate);
        }
    };

    // Verify proposal senders match commit sender
    if let Some(first_proposal_sender) = proposal_senders.first() {
        if first_proposal_sender != &commit_sender {
            tracing::warn!(
                "Violation: proposal sender != commit sender for group {}",
                group_name,
            );
            discard_candidate_state(handle, mls, &group_name)?;
            return Ok(FreezeFinalizeResult::Applied {
                result: ProcessResult::ViolationDetected(ViolationEvidence::broken_commit(
                    commit_sender,
                    handle.current_epoch(),
                    "proposal sender differs from commit sender",
                )),
                outbound: vec![],
            });
        }
    }

    // Validate MLS actions against local voted proposals
    if let Some(result) =
        validate_commit_candidate(handle, &local_proposals, &commit_sender, &commit_actions)?
    {
        discard_candidate_state(handle, mls, &group_name)?;
        return Ok(FreezeFinalizeResult::Applied {
            result,
            outbound: vec![],
        });
    }

    // All checks passed — merge
    mls.merge_staged_commit(&group_name)?;
    handle.set_steward_identity(commit_sender);
    handle.record_committed_batch(chosen.commit_hash);
    handle.clear_approved_proposals();
    handle.clear_freeze_round();

    // Remote candidates never carry welcome bytes (only local candidates do)
    Ok(FreezeFinalizeResult::Applied {
        result: process_result_from_commit_outcome(self_removed),
        outbound: vec![],
    })
}

// ─────────────────────────── Batch Validation ───────────────────────────

/// Validate MLS actions from a commit against local voted proposals.
///
/// Returns `Some(ProcessResult::ViolationDetected(...))` if a violation is found,
/// `None` if all checks pass.
pub(crate) fn validate_commit_candidate(
    handle: &GroupHandle,
    local_proposals: &HashMap<ProposalId, GroupUpdateRequest>,
    sender_id: &[u8],
    mls_actions: &[MlsProposalAction],
) -> Result<Option<ProcessResult>, CoreError> {
    let group_name = handle.group_name();

    let mut expected_actions: Vec<MlsProposalAction> = local_proposals
        .values()
        .filter_map(expected_action_for_request)
        .collect();
    let mut actual_actions = mls_actions.to_vec();

    expected_actions.sort();
    actual_actions.sort();

    if actual_actions != expected_actions {
        tracing::warn!(
            "Violation: broken MLS proposal for group {} — \
             MLS actions {:?} don't match voted {:?}",
            group_name,
            actual_actions,
            expected_actions,
        );
        return Ok(Some(ProcessResult::ViolationDetected(
            ViolationEvidence::broken_mls_proposal(
                sender_id.to_vec(),
                handle.current_epoch(),
                format!("MLS actions {actual_actions:?} != voted {expected_actions:?}"),
            ),
        )));
    }

    Ok(None)
}

/// Derive the expected [`MlsProposalAction`] from a voted [`GroupUpdateRequest`].
fn expected_action_for_request(req: &GroupUpdateRequest) -> Option<MlsProposalAction> {
    use crate::protos::de_mls::messages::v1::group_update_request::Payload;
    match &req.payload {
        Some(Payload::InviteMember(im)) => Some(MlsProposalAction::Add(im.identity.clone())),
        Some(Payload::RemoveMember(rm)) => Some(MlsProposalAction::Remove(rm.identity.clone())),
        // Emergency criteria proposals don't produce MLS proposals
        Some(Payload::EmergencyCriteria(_)) | None => None,
    }
}

// ─────────────────────────── Proposal Queries ───────────────────────────

/// Get the count of approved proposals waiting to be committed.
pub fn approved_proposals_count(handle: &GroupHandle) -> usize {
    handle.approved_proposals_count()
}

/// Get all approved proposals waiting to be committed.
pub fn approved_proposals(handle: &GroupHandle) -> HashMap<ProposalId, GroupUpdateRequest> {
    handle.approved_proposals()
}

/// Get the epoch history (past batches of approved proposals).
pub fn epoch_history(handle: &GroupHandle) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
    handle.epoch_history()
}

// ─────────────────────────── Steward Operations ───────────────────────────

/// Create and broadcast a commit candidate for the current epoch.
///
/// This does not merge the commit immediately. The candidate is buffered and
/// later applied via [`finalize_freeze_round`].
pub fn create_commit_candidate<S>(
    handle: &mut GroupHandle,
    mls: &MlsService<S>,
) -> Result<Vec<OutboundPacket>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_steward() {
        return Err(CoreError::StewardNotSet);
    }

    let proposals = handle.approved_proposals();
    if proposals.is_empty() {
        return Err(CoreError::NoProposals);
    }

    if !handle.is_mls_initialized() {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    // Emergency criteria proposals are consensus-only — they don't produce MLS operations
    // and must NOT be in the approved queue at batch creation time.
    let emergency_ids: Vec<u32> = proposals
        .iter()
        .filter(|(_, req)| {
            matches!(
                req.payload,
                Some(group_update_request::Payload::EmergencyCriteria(_))
            )
        })
        .map(|(&id, _)| id)
        .collect();

    if !emergency_ids.is_empty() {
        return Err(CoreError::UnexpectedEmergencyProposals {
            proposal_ids: emergency_ids,
        });
    }

    let mut updates = Vec::with_capacity(proposals.len());
    for (_, proposal) in proposals {
        match proposal.payload {
            Some(group_update_request::Payload::InviteMember(im)) => {
                updates.push(GroupUpdate::Add(KeyPackageBytes::new(
                    im.key_package_bytes,
                    im.identity,
                )));
            }
            Some(group_update_request::Payload::RemoveMember(identity)) => {
                updates.push(GroupUpdate::Remove(identity.identity));
            }
            _ => return Err(CoreError::InvalidGroupUpdateRequest),
        }
    }

    let MlsCommitCandidate {
        proposals: mls_proposals,
        commit,
        welcome,
    } = mls.create_commit_candidate(handle.group_name(), &updates)?;

    let candidate = CommitCandidate {
        group_name: handle.group_name_bytes().to_vec(),
        mls_proposals,
        commit_message: commit,
    };

    // Store own candidate locally for deterministic selection at freeze timeout.
    // Welcome bytes are deferred — they'll be sent after commit merge in finalize_freeze_round
    // to prevent joiners from advancing epoch before the steward merges.
    let commit_hash = compute_commit_hash(&candidate.commit_message);
    let _ = handle.buffer_freeze_candidate(BufferedCommitCandidate {
        candidate_msg: candidate.clone(),
        commit_hash,
        is_local_candidate: true,
        welcome_bytes: welcome,
    });

    let candidate_msg: AppMessage = candidate.into();

    let batch_packet = OutboundPacket::new(
        candidate_msg.encode_to_vec(),
        APP_MSG_SUBTOPIC,
        handle.group_name(),
        handle.app_id(),
    );

    Ok(vec![batch_packet])
}

// ─────────────────────────── Member Queries ───────────────────────────

/// Get the current members of a group.
pub fn group_members<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
) -> Result<Vec<Vec<u8>>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_mls_initialized() {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    let members = mls.members(handle.group_name())?;
    Ok(members)
}

// ─────────────────────────── Unit Tests ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{build_key_package_message, create_group, prepare_to_join, process_inbound};
    use crate::ds::WELCOME_SUBTOPIC;
    use crate::mls_crypto::{MemoryDeMlsStorage, MlsService, parse_wallet_address};

    fn setup_mls(wallet_hex: &str) -> MlsService<MemoryDeMlsStorage> {
        let storage = MemoryDeMlsStorage::new();
        let mls = MlsService::new(storage);
        let wallet = parse_wallet_address(wallet_hex).unwrap();
        mls.init(wallet).unwrap();
        mls
    }

    fn setup_steward(
        group_name: &str,
        wallet_hex: &str,
    ) -> (MlsService<MemoryDeMlsStorage>, GroupHandle) {
        let mls = setup_mls(wallet_hex);
        let handle = create_group(group_name, &mls).unwrap();
        (mls, handle)
    }

    fn setup_joiner(
        group_name: &str,
        wallet_hex: &str,
    ) -> (MlsService<MemoryDeMlsStorage>, GroupHandle, OutboundPacket) {
        let mls = setup_mls(wallet_hex);
        let handle = prepare_to_join(group_name);
        let kp_packet = build_key_package_message(&handle, &mls).unwrap();
        (mls, handle, kp_packet)
    }

    fn steward_add_joiner(
        steward_mls: &MlsService<MemoryDeMlsStorage>,
        steward_handle: &mut GroupHandle,
        joiner_kp_packet: &OutboundPacket,
    ) -> (OutboundPacket, OutboundPacket) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static PROPOSAL_COUNTER: AtomicU32 = AtomicU32::new(200);

        let result = process_inbound(
            steward_handle,
            &joiner_kp_packet.payload,
            WELCOME_SUBTOPIC,
            steward_mls,
        )
        .unwrap();

        let gur = match result {
            ProcessResult::GetUpdateRequest(gur) => gur,
            other => panic!("Expected GetUpdateRequest, got {:?}", other),
        };

        let proposal_id = PROPOSAL_COUNTER.fetch_add(1, Ordering::Relaxed);
        steward_handle.insert_approved_proposal(proposal_id, gur);
        let packets = create_commit_candidate(steward_handle, steward_mls).unwrap();

        let finalize = finalize_freeze_round(steward_handle, steward_mls).unwrap();
        let welcome_packet = match finalize {
            FreezeFinalizeResult::Applied { result, outbound } => {
                assert!(
                    matches!(result, ProcessResult::GroupUpdated),
                    "Expected GroupUpdated, got {:?}",
                    result
                );
                outbound
                    .into_iter()
                    .find(|p| p.subtopic == WELCOME_SUBTOPIC)
                    .expect("Expected deferred welcome packet from finalize_freeze_round")
            }
            other => panic!("Expected Applied, got {:?}", other),
        };

        let batch_packet = packets
            .iter()
            .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
            .expect("Expected batch proposals packet")
            .clone();

        (welcome_packet, batch_packet)
    }

    #[test]
    fn test_validate_batch_proposals_action_mismatch() {
        let group_name = "validate-action-mismatch";
        let (steward_mls, mut steward_handle) =
            setup_steward(group_name, "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        let (joiner_mls, mut joiner_handle, kp_packet) =
            setup_joiner(group_name, "0x70997970C51812dc3A010C7d01b50e0d17dc79C8");

        let (welcome_packet, _) = steward_add_joiner(&steward_mls, &mut steward_handle, &kp_packet);
        process_inbound(
            &mut joiner_handle,
            &welcome_packet.payload,
            WELCOME_SUBTOPIC,
            &joiner_mls,
        )
        .unwrap();

        let (_joiner2_mls, _joiner2_handle, kp2_packet) =
            setup_joiner(group_name, "0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
        let result = process_inbound(
            &mut steward_handle,
            &kp2_packet.payload,
            WELCOME_SUBTOPIC,
            &steward_mls,
        )
        .unwrap();
        let gur = match result {
            ProcessResult::GetUpdateRequest(gur) => gur,
            other => panic!("Expected GetUpdateRequest, got {:?}", other),
        };

        let proposal_id: ProposalId = 43;
        joiner_handle.insert_approved_proposal(proposal_id, gur);

        let steward_id = steward_handle.steward_identity().unwrap().to_vec();
        let local_proposals = joiner_handle.approved_proposals();

        // Pass wrong MLS actions (empty) while local has an add proposal
        let result = validate_commit_candidate(
            &joiner_handle,
            &local_proposals,
            &steward_id,
            &[], // empty actions, should mismatch
        )
        .unwrap();

        match result {
            Some(ProcessResult::ViolationDetected(evidence)) => {
                use crate::protos::de_mls::messages::v1::ViolationType;
                assert_eq!(
                    evidence.violation_type,
                    ViolationType::BrokenMlsProposal as i32
                );
            }
            other => panic!("Expected Some(ViolationDetected), got {:?}", other),
        }
    }
}
