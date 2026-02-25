//! Core API for MLS group operations.
//!
//! This module provides the fundamental building blocks for MLS group management.
//! All functions operate on [`GroupHandle`] instances for app-level state and
//! [`MlsService`] for MLS cryptographic operations.
//!
//! # Overview
//!
//! The API is organized into four categories:
//!
//! ## Group Lifecycle
//! - [`create_group`] / [`create_group_with_policy`] - Create a new group as steward
//! - [`prepare_to_join`] / [`prepare_to_join_with_policy`] - Prepare a handle for joining
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
//! - [`create_batch_proposals`] - Batch approved proposals into MLS commit
//!
//! ## Quarantine
//! - [`retry_quarantined`] - Retry quarantined batches after proposals arrive
//! - [`quarantine_len`] - Number of quarantined batches
//! - [`has_quarantined`] - Whether any batches are quarantined
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
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use tracing::info;

use crate::core::{
    ProposalId,
    error::CoreError,
    group_handle::{GroupHandle, QuarantinePolicy, QuarantinedBatch},
    types::ProcessResult,
    types::invitation_from_bytes,
};
use crate::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use crate::mls_crypto::{
    CommitResult, DeMlsStorage, DecryptResult, GroupUpdate, KeyPackageBytes, MlsProposalAction,
    MlsService, StagedCommitResult, key_package_bytes_from_json,
};
use crate::protos::de_mls::messages::v1::{
    AppMessage, BatchProposalsMessage, GroupUpdateRequest, InviteMember, UserKeyPackage,
    ViolationEvidence, WelcomeMessage, app_message, group_update_request, welcome_message,
};

// ─────────────────────────── Group Lifecycle ───────────────────────────

/// Create a new MLS group as the steward with default quarantine policy.
pub fn create_group<S>(name: &str, mls: &MlsService<S>) -> Result<GroupHandle, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    create_group_with_policy(name, mls, QuarantinePolicy::default())
}

/// Create a new MLS group as the steward with custom quarantine policy.
pub fn create_group_with_policy<S>(
    name: &str,
    mls: &MlsService<S>,
    policy: QuarantinePolicy,
) -> Result<GroupHandle, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    mls.create_group(name)?;
    Ok(GroupHandle::new_as_creator_with_policy(
        name,
        mls.wallet_bytes(),
        policy,
    ))
}

/// Prepare a handle for joining an existing group with default quarantine policy.
pub fn prepare_to_join(name: &str) -> GroupHandle {
    GroupHandle::new_for_join(name)
}

/// Prepare a handle for joining an existing group with custom quarantine policy.
pub fn prepare_to_join_with_policy(name: &str, policy: QuarantinePolicy) -> GroupHandle {
    GroupHandle::new_for_join_with_policy(name, policy)
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

    // Try to parse as AppMessage first (for batch proposals)
    if let Ok(app_message) = AppMessage::decode(payload)
        && let Some(app_message::Payload::BatchProposalsMessage(batch_msg)) = app_message.payload
    {
        return process_batch_proposals(handle, batch_msg, mls);
    }

    // Fall back to MLS protocol message
    let res = mls.decrypt(handle.group_name(), payload)?;

    match res {
        DecryptResult::Application(app_bytes, _sender) => {
            AppMessage::decode(app_bytes.as_ref())?.try_into()
        }
        DecryptResult::Removed(_) => Ok(ProcessResult::LeaveGroup),
        DecryptResult::ProposalStored(..)
        | DecryptResult::CommitProcessed(_)
        | DecryptResult::Ignored => Ok(ProcessResult::Noop),
    }
}

// ─────────────────────────── Batch Processing (internal helpers) ───────────────────────────

/// Compute a canonical SHA-256 digest over a set of approved proposals.
fn compute_proposals_digest(proposals: &HashMap<ProposalId, GroupUpdateRequest>) -> Vec<u8> {
    let sorted: BTreeMap<_, _> = proposals.iter().collect();
    let mut hasher = Sha256::new();
    for (&id, req) in &sorted {
        hasher.update(id.to_le_bytes());
        hasher.update(req.encode_to_vec());
    }
    hasher.finalize().to_vec()
}

/// Compute a SHA-256 hash of the raw commit message bytes.
fn compute_commit_hash(commit_message: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(commit_message);
    hasher.finalize().to_vec()
}

/// Compute a SHA-256 fingerprint from proposal IDs and digest.
/// Proposal IDs are sorted before hashing for deterministic output.
fn compute_batch_fingerprint(proposal_ids: &[u32], proposals_digest: &[u8]) -> Vec<u8> {
    let mut sorted_ids = proposal_ids.to_vec();
    sorted_ids.sort();
    let mut hasher = Sha256::new();
    for id in &sorted_ids {
        hasher.update(id.to_le_bytes());
    }
    hasher.update(proposals_digest);
    hasher.finalize().to_vec()
}

/// Pre-check: empty set check, dedup, quarantine decision.
/// Returns `Some(ProcessResult)` if the batch should not be processed further.
fn precheck_batch<S>(
    handle: &mut GroupHandle,
    batch_msg: &BatchProposalsMessage,
    local_ids: &BTreeSet<ProposalId>,
    batch_ids: &BTreeSet<ProposalId>,
    _mls: &MlsService<S>,
) -> Option<ProcessResult>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = handle.group_name().to_owned();

    match (local_ids.is_empty(), batch_ids.is_empty()) {
        (true, true) => {
            tracing::debug!(
                "Ignoring batch for group {}: no proposals on either side",
                group_name
            );
            return Some(ProcessResult::Noop);
        }
        (true, false) => {
            let commit_hash = compute_commit_hash(&batch_msg.commit_message);
            let fingerprint =
                compute_batch_fingerprint(&batch_msg.proposal_ids, &batch_msg.proposals_digest);

            if handle.is_duplicate_batch(&commit_hash, &fingerprint) {
                tracing::debug!(
                    "Ignoring duplicate batch for group {}: already processed or quarantined",
                    group_name,
                );
                return Some(ProcessResult::Noop);
            }

            let epoch = handle.current_epoch();
            let batch_proposal_ids: Vec<u32> = batch_ids.iter().copied().collect();
            let local_proposal_ids: Vec<u32> = local_ids.iter().copied().collect();

            tracing::debug!(
                "Quarantining batch for group {}: no local proposals yet, batch contains {:?}",
                group_name,
                batch_ids,
            );
            handle.quarantine_batch(QuarantinedBatch {
                batch_msg: batch_msg.clone(),
                commit_hash,
                batch_fingerprint: fingerprint,
                quarantined_at_epoch: epoch,
            });
            return Some(ProcessResult::BatchQuarantined {
                batch_proposal_ids,
                local_proposal_ids,
                epoch,
            });
        }
        (false, true) => {
            tracing::debug!(
                "Ignoring empty batch for group {}: expected {:?} but batch is empty",
                group_name,
                local_ids,
            );
            return Some(ProcessResult::Noop);
        }
        (false, false) => {}
    }

    None
}

/// Stage MLS proposals (decrypt and store). Returns true if any were stored.
/// On failure, cleans up and returns None (caller should return Noop).
fn stage_mls_proposals<S>(
    handle: &GroupHandle,
    batch_msg: &BatchProposalsMessage,
    mls: &MlsService<S>,
) -> Result<Option<bool>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = handle.group_name();
    let mut proposals_stored = false;

    for (i, proposal_bytes) in batch_msg.mls_proposals.iter().enumerate() {
        let decrypt_result = match mls.decrypt(group_name, proposal_bytes) {
            Ok(result) => result,
            Err(e) => {
                tracing::debug!(
                    "MLS proposal {} for group {} failed to decrypt (pre-auth): {}",
                    i,
                    group_name,
                    e,
                );
                if proposals_stored {
                    mls.discard_staged_commit(group_name)?;
                }
                return Ok(None);
            }
        };
        match decrypt_result {
            DecryptResult::ProposalStored(_sender, _action) => {
                proposals_stored = true;
            }
            DecryptResult::Ignored => {
                tracing::debug!(
                    "MLS proposal {} for group {} returned Ignored (stale epoch/wrong group)",
                    i,
                    group_name,
                );
                if proposals_stored {
                    mls.discard_staged_commit(group_name)?;
                }
                return Ok(None);
            }
            other => {
                tracing::debug!(
                    "MLS proposal {} for group {} returned {:?}, expected ProposalStored (pre-auth)",
                    i,
                    group_name,
                    other,
                );
                if proposals_stored {
                    mls.discard_staged_commit(group_name)?;
                }
                return Ok(None);
            }
        }
    }

    Ok(Some(proposals_stored))
}

/// Process commit message and return staged result.
/// On failure, discards and returns None.
#[allow(clippy::type_complexity)]
fn stage_commit<S>(
    handle: &GroupHandle,
    batch_msg: &BatchProposalsMessage,
    mls: &MlsService<S>,
) -> Result<Option<(Vec<u8>, bool, Vec<MlsProposalAction>)>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = handle.group_name();

    let staged_result = match mls.process_commit(group_name, &batch_msg.commit_message) {
        Ok(result) => result,
        Err(e) => {
            tracing::debug!(
                "Commit for group {} failed to process (pre-auth): {}",
                group_name,
                e,
            );
            mls.discard_staged_commit(group_name)?;
            return Ok(None);
        }
    };

    match staged_result {
        StagedCommitResult::Staged {
            sender_identity,
            self_removed,
            actions,
        } => Ok(Some((sender_identity, self_removed, actions))),
        StagedCommitResult::Ignored => {
            tracing::debug!(
                "Commit for group {} returned Ignored (stale/benign)",
                group_name,
            );
            mls.discard_staged_commit(group_name)?;
            Ok(None)
        }
    }
}

/// Merge staged commit, record dedup, clear proposals, expire quarantine.
fn finalize_batch<S>(
    handle: &mut GroupHandle,
    batch_msg: &BatchProposalsMessage,
    mls: &MlsService<S>,
    self_removed: bool,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = handle.group_name().to_owned();
    mls.merge_staged_commit(&group_name)?;

    let commit_hash = compute_commit_hash(&batch_msg.commit_message);
    let fingerprint =
        compute_batch_fingerprint(&batch_msg.proposal_ids, &batch_msg.proposals_digest);
    handle.record_committed_batch(commit_hash, fingerprint);
    handle.clear_approved_proposals();
    handle.expire_quarantine();

    if self_removed {
        Ok(ProcessResult::LeaveGroup)
    } else {
        Ok(ProcessResult::GroupUpdated)
    }
}

fn process_batch_proposals<S>(
    handle: &mut GroupHandle,
    batch_msg: BatchProposalsMessage,
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let local_proposals = handle.approved_proposals();
    let batch_ids: BTreeSet<ProposalId> = batch_msg.proposal_ids.iter().copied().collect();
    let local_ids: BTreeSet<ProposalId> = local_proposals.keys().copied().collect();

    // ── 0. Pre-check ──────────────
    if let Some(result) = precheck_batch(handle, &batch_msg, &local_ids, &batch_ids, mls) {
        return Ok(result);
    }

    // ── 1. Stage MLS proposals ──────────────
    if stage_mls_proposals(handle, &batch_msg, mls)?.is_none() {
        return Ok(ProcessResult::Noop);
    }

    // ── 2. Stage commit ──────────────
    let (steward_id, self_removed, mls_actions) = match stage_commit(handle, &batch_msg, mls)? {
        Some(result) => result,
        None => return Ok(ProcessResult::Noop),
    };
    handle.set_steward_identity(steward_id.clone());

    // ── 3. Validate proposals against authenticated sender ───
    if let Some(violation) = validate_batch_proposals(
        handle,
        &batch_msg,
        &local_proposals,
        &steward_id,
        &mls_actions,
    )? {
        let group_name = handle.group_name().to_owned();
        mls.discard_staged_commit(&group_name)?;
        handle.clear_approved_proposals();
        return Ok(violation);
    }

    // ── 4. Finalize ────────────
    finalize_batch(handle, &batch_msg, mls, self_removed)
}

// ─────────────────────────── Quarantine API ───────────────────────────

/// Retry quarantined batches that may now match local approved proposals.
///
/// Returns only actionable results. Batches that still can't be resolved
/// are silently kept in quarantine. Processes oldest-first (by epoch, then
/// insertion order). Empty `Vec` means nothing was ready.
pub fn retry_quarantined<S>(
    handle: &mut GroupHandle,
    mls: &MlsService<S>,
) -> Result<Vec<ProcessResult>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // Enforce age policy before retrying matches.
    handle.expire_quarantine();

    let mut results = Vec::new();

    while let Some(batch) = handle.take_matching_quarantine() {
        let result = process_batch_proposals(handle, batch.batch_msg, mls)?;
        match result {
            ProcessResult::Noop | ProcessResult::BatchQuarantined { .. } => {
                // Stale/invalid or re-quarantined — continue to try remaining.
            }
            actionable => {
                results.push(actionable);
            }
        }
    }

    Ok(results)
}

/// Number of quarantined batches for this group.
pub fn quarantine_len(handle: &GroupHandle) -> usize {
    handle.quarantine_len()
}

/// Whether any batches are quarantined for this group.
pub fn has_quarantined(handle: &GroupHandle) -> bool {
    handle.has_quarantined()
}

// ─────────────────────────── Batch Validation ───────────────────────────

/// Validate batch proposals against local state using the authenticated steward identity.
///
/// Returns `Some(ProcessResult::ViolationDetected(...))` if a violation is found,
/// `None` if all checks pass.
pub(crate) fn validate_batch_proposals(
    handle: &GroupHandle,
    batch_msg: &BatchProposalsMessage,
    local_proposals: &HashMap<ProposalId, GroupUpdateRequest>,
    steward_id: &[u8],
    mls_actions: &[MlsProposalAction],
) -> Result<Option<ProcessResult>, CoreError> {
    let group_name = handle.group_name();
    let local_ids: BTreeSet<ProposalId> = local_proposals.keys().copied().collect();
    let batch_ids: BTreeSet<ProposalId> = batch_msg.proposal_ids.iter().copied().collect();

    // ── Verify proposal-set IDs match ────────────────────────
    if local_ids != batch_ids {
        tracing::warn!(
            "Violation: broken commit for group {} — proposal ID set mismatch \
             (local={:?}, batch={:?})",
            group_name,
            local_ids,
            batch_ids,
        );
        return Ok(Some(ProcessResult::ViolationDetected(
            ViolationEvidence::broken_commit(
                steward_id.to_vec(),
                handle.current_epoch(),
                format!("proposal ID mismatch: local={local_ids:?}, batch={batch_ids:?}"),
            ),
        )));
    }

    // ── Verify content digest (cryptographic binding) ────────
    let local_digest = compute_proposals_digest(local_proposals);
    if batch_msg.proposals_digest != local_digest {
        tracing::warn!(
            "Violation: broken commit for group {} — IDs match but \
             digest mismatch (steward altered proposal content)",
            group_name,
        );
        return Ok(Some(ProcessResult::ViolationDetected(
            ViolationEvidence::broken_commit(
                steward_id.to_vec(),
                handle.current_epoch(),
                batch_msg.proposals_digest.clone(),
            ),
        )));
    }

    // ── Proposal count must match MLS payloads ───────────────
    if batch_msg.mls_proposals.len() != local_ids.len() {
        tracing::warn!(
            "Violation: broken MLS proposal for group {} — proposal count ({}) \
             does not match MLS payload count ({})",
            group_name,
            local_ids.len(),
            batch_msg.mls_proposals.len(),
        );
        return Ok(Some(ProcessResult::ViolationDetected(
            ViolationEvidence::broken_mls_proposal(
                steward_id.to_vec(),
                handle.current_epoch(),
                format!(
                    "expected {} MLS proposals, got {}",
                    local_ids.len(),
                    batch_msg.mls_proposals.len()
                ),
            ),
        )));
    }

    // ── Verify MLS proposal payloads match voted proposals ───
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
                steward_id.to_vec(),
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

/// Create and send batch proposals for the current epoch.
pub fn create_batch_proposals<S>(
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

    let proposal_ids: Vec<u32> = proposals.keys().copied().collect();
    let proposals_digest = compute_proposals_digest(&proposals);
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
            Some(group_update_request::Payload::EmergencyCriteria(_)) => {
                return Err(CoreError::InvalidGroupUpdateRequest);
            }
            None => return Err(CoreError::InvalidGroupUpdateRequest),
        }
    }

    let CommitResult {
        proposals: mls_proposals,
        commit,
        welcome,
    } = mls.commit(handle.group_name(), &updates)?;

    let batch_msg: AppMessage = BatchProposalsMessage {
        group_name: handle.group_name_bytes().to_vec(),
        mls_proposals,
        commit_message: commit,
        proposal_ids,
        proposals_digest,
    }
    .into();

    let batch_packet = OutboundPacket::new(
        batch_msg.encode_to_vec(),
        APP_MSG_SUBTOPIC,
        handle.group_name(),
        handle.app_id(),
    );

    let mut messages = vec![batch_packet];

    if let Some(welcome_bytes) = welcome {
        let welcome_msg: WelcomeMessage = invitation_from_bytes(welcome_bytes);
        let welcome_packet = OutboundPacket::new(
            welcome_msg.encode_to_vec(),
            WELCOME_SUBTOPIC,
            handle.group_name(),
            handle.app_id(),
        );
        messages.push(welcome_packet);
    }

    handle.clear_approved_proposals();

    Ok(messages)
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
        let packets = create_batch_proposals(steward_handle, steward_mls).unwrap();

        let batch_packet = packets
            .iter()
            .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
            .expect("Expected batch proposals packet")
            .clone();

        let welcome_packet = packets
            .into_iter()
            .find(|p| p.subtopic == WELCOME_SUBTOPIC)
            .expect("Expected welcome packet");

        (welcome_packet, batch_packet)
    }

    #[test]
    fn test_validate_batch_proposals_id_mismatch() {
        let group_name = "validate-id-mismatch";
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

        let steward_id = steward_handle.steward_identity().unwrap().to_vec();
        let batch_msg = BatchProposalsMessage {
            group_name: group_name.as_bytes().to_vec(),
            mls_proposals: vec![],
            commit_message: vec![],
            proposal_ids: vec![999],
            proposals_digest: vec![],
        };

        let local_proposals = joiner_handle.approved_proposals();
        let result = validate_batch_proposals(
            &joiner_handle,
            &batch_msg,
            &local_proposals,
            &steward_id,
            &[],
        )
        .unwrap();

        assert!(
            matches!(result, Some(ProcessResult::ViolationDetected(_))),
            "Expected Some(ViolationDetected) for mismatched proposals, got {:?}",
            result
        );
    }

    #[test]
    fn test_validate_batch_proposals_count_mismatch() {
        let group_name = "validate-count-mismatch";
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
        joiner_handle.insert_approved_proposal(proposal_id, gur.clone());

        let mut proposals_map = HashMap::new();
        proposals_map.insert(proposal_id, gur);
        let correct_digest = compute_proposals_digest(&proposals_map);

        let batch_msg = BatchProposalsMessage {
            group_name: group_name.as_bytes().to_vec(),
            mls_proposals: vec![],
            commit_message: vec![],
            proposal_ids: vec![proposal_id],
            proposals_digest: correct_digest,
        };

        let steward_id = steward_handle.steward_identity().unwrap().to_vec();
        let local_proposals = joiner_handle.approved_proposals();
        let result = validate_batch_proposals(
            &joiner_handle,
            &batch_msg,
            &local_proposals,
            &steward_id,
            &[],
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
