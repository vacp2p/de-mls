//! Freeze round candidate processing, selection, and commit application.

use super::*;

// ─────────────────────────── Commit Validation ───────────────────────────

/// Validate MLS actions from a commit against local voted proposals.
///
/// Returns `Some(ProcessResult::ViolationDetected(...))` if a violation is found,
/// `None` if all checks pass.
pub(super) fn validate_commit_candidate(
    group: &Group,
    local_proposals: &HashMap<ProposalId, GroupUpdateRequest>,
    sender_id: &[u8],
    mls_actions: &[MlsProposalAction],
    epoch: u64,
) -> Result<Option<ProcessResult>, CoreError> {
    let group_name = group.group_name();

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
                epoch,
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
        // Emergency criteria and steward election proposals don't produce MLS proposals
        Some(Payload::EmergencyCriteria(_)) | Some(Payload::StewardElection(_)) | None => None,
    }
}

// ─────────────────────────── Authorization ───────────────────────────

/// Check if a commit sender is authorized for the given epoch.
///
/// Any member on the steward list is authorized to commit (RFC §de-MLS Objects:
/// "other stewards MAY also generate a commit within the same epoch to preserve
/// liveness"). The epoch steward has priority in **selection**, not authorization.
///
/// Returns `Some(ViolationEvidence)` if unauthorized (not on list),
/// `None` if authorized, no list available (joiner pre-sync), or list exhausted.
fn check_commit_sender_authorized(
    group: &Group,
    commit_sender: &[u8],
    epoch: u64,
) -> Option<ViolationEvidence> {
    let list = group.steward_list()?;
    // If the list is exhausted for this epoch, we can't determine authorization —
    // skip the check (a new election should be in progress).
    if list.is_exhausted(epoch) {
        return None;
    }
    if list.contains(commit_sender) {
        return None;
    }
    Some(ViolationEvidence::broken_commit(
        commit_sender.to_vec(),
        epoch,
        "commit from unauthorized sender (not on the steward list)",
    ))
}

// ─────────────────────────── Batch Processing (internal helpers) ───────────────────────────

/// Build deferred welcome outbound packets from a chosen candidate.
///
/// Welcome messages are deferred until after commit merge so that joiners
/// don't advance to the new epoch before the steward does.
fn build_deferred_welcome(
    welcome_bytes: Option<Vec<u8>>,
    group: &Group,
    app_id: &[u8],
) -> Vec<OutboundPacket> {
    let Some(welcome_bytes) = welcome_bytes else {
        return vec![];
    };
    let welcome_msg: WelcomeMessage = invitation_from_bytes(welcome_bytes);
    vec![OutboundPacket::new(
        welcome_msg.encode_to_vec(),
        WELCOME_SUBTOPIC,
        group.group_name(),
        app_id,
    )]
}

/// Compute a SHA-256 hash of the raw commit message bytes.
pub(super) fn compute_commit_hash(commit_message: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(commit_message);
    hasher.finalize().to_vec()
}

/// Validate that candidate wire messages have the expected MLS kinds.
///
/// This is a cheap check only (deserialize + outer content type), with no
/// MLS state mutation.
pub(super) fn has_valid_candidate_wire_kinds<S>(
    candidate: &CommitCandidate,
    mls: &MlsService<S>,
) -> bool
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
fn discard_candidate_state<S>(group: &mut Group, mls: &MlsService<S>) -> Result<(), CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = group.group_name();
    mls.discard_staged_commit(group_name)?;
    group.clear_freeze_round();
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
pub(super) fn process_commit_candidate<S>(
    group: &mut Group,
    candidate_msg: CommitCandidate,
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // 1. Ensure a freeze round exists. If none is active but approved proposals
    //    exist, auto-start one so the candidate isn't dropped.
    if group.freeze_round().is_none() {
        if group.approved_proposals_count() > 0 {
            let epoch = mls.current_epoch(group.group_name())?;
            group.ensure_freeze_round(epoch);
        } else {
            tracing::debug!(
                "Ignoring candidate for group {}: no approved proposals",
                group.group_name(),
            );
            return Ok(ProcessResult::Noop);
        }
    }

    // 2. Dedup
    let commit_hash = compute_commit_hash(&candidate_msg.commit_message);
    if group.is_duplicate_commit_candidate(&commit_hash) {
        tracing::debug!(
            "Ignoring duplicate candidate for group {}: already processed/buffered",
            group.group_name(),
        );
        return Ok(ProcessResult::Noop);
    }

    // 3. Shape check
    if candidate_msg.mls_proposals.is_empty() || candidate_msg.commit_message.is_empty() {
        tracing::debug!(
            "Ignoring candidate for group {}: empty proposals or commit",
            group.group_name(),
        );
        return Ok(ProcessResult::Noop);
    }

    // 4. Kind check (wire-level only, no MLS state mutation)
    if !has_valid_candidate_wire_kinds(&candidate_msg, mls) {
        return Ok(ProcessResult::Noop);
    }

    // 5. Buffer
    let epoch = mls.current_epoch(group.group_name())?;
    let buffered = group.add_freeze_candidate(
        BufferedCommitCandidate {
            candidate_msg,
            commit_hash,
            is_local_candidate: false,
            welcome_bytes: None,
        },
        epoch,
    );
    if buffered {
        Ok(ProcessResult::CommitCandidateReceived)
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
    /// Steward identity from the proto (used for pre-MLS selection ordering).
    steward_identity: Vec<u8>,
}

/// Sort candidates by priority for deterministic selection (RFC §"Commit validation service").
///
/// All nodes must converge on the same candidate regardless of which is "local".
/// `is_local_candidate` is intentionally excluded from the sort: it is node-local
/// knowledge and would cause divergence if multiple stewards exist.
///
/// Priority order (RFC-compliant):
///   1. Largest `actions_count` first (longest deterministic proposal sequence)
///   2. Epoch steward preferred (tier 0) over everyone else (tier 1)
///   3. Lexicographically smallest `steward_identity`
///   4. Lowest `commit_hash`
///
/// When `epoch_steward_id` is `None` (joiner pre-sync, no steward list), all
/// candidates are tier 1, so action count -> identity -> hash determines order.
fn sort_candidates_by_priority(
    candidates: &mut [ValidatedCandidate],
    epoch_steward_id: Option<&[u8]>,
) {
    candidates.sort_by(|a, b| {
        // Primary: largest action count first
        let size_cmp = b.actions_count.cmp(&a.actions_count);
        if size_cmp != std::cmp::Ordering::Equal {
            return size_cmp;
        }

        // Steward tier: 0 = epoch steward, 1 = everyone else
        let tier = |candidate: &ValidatedCandidate| -> u8 {
            if let Some(es) = epoch_steward_id {
                if candidate.steward_identity == es {
                    return 0;
                }
            }
            1
        };

        let tier_cmp = tier(a).cmp(&tier(b));
        if tier_cmp != std::cmp::Ordering::Equal {
            return tier_cmp;
        }

        // Lexicographically smallest steward_identity
        let id_cmp = a.steward_identity.cmp(&b.steward_identity);
        if id_cmp != std::cmp::Ordering::Equal {
            return id_cmp;
        }

        // Final tiebreak: lowest commit_hash
        a.commit_hash.cmp(&b.commit_hash)
    });
}

/// Deterministically select and apply a buffered candidate for the active freeze round.
///
/// Three distinct phases:
/// 1. Pre-validation (cheap, no MLS processing)
/// 2. Selection (deterministic choice from pre-validated candidates)
/// 3. Application (MLS processing — chosen candidate only)
pub fn finalize_freeze_round<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    allow_subset_candidates: bool,
    app_id: &[u8],
) -> Result<FreezeFinalizeResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let current_epoch = mls.current_epoch(group.group_name())?;
    group.lock_freeze_round_selection(current_epoch);

    let candidates = match group.freeze_round() {
        Some(round) if round.epoch == current_epoch => round.candidates.clone(),
        _ => return Ok(FreezeFinalizeResult::NoCandidate),
    };

    if candidates.is_empty() {
        return Ok(FreezeFinalizeResult::NoCandidate);
    }

    let local_proposals = group.approved_proposals();
    // Count only MLS-producing proposals (exclude emergency and election)
    let expected_mls_count = local_proposals
        .values()
        .filter(|req| {
            !matches!(
                req.payload,
                Some(group_update_request::Payload::EmergencyCriteria(_))
                    | Some(group_update_request::Payload::StewardElection(_))
            )
        })
        .count();

    // ── Phase 1: Pre-validation (cheap, no MLS processing) ──
    let mut pre_validated: Vec<ValidatedCandidate> = Vec::new();

    for candidate in &candidates {
        // Count match
        if !allow_subset_candidates
            && candidate.candidate_msg.mls_proposals.len() != expected_mls_count
        {
            continue;
        }
        if allow_subset_candidates
            && candidate.candidate_msg.mls_proposals.len() > expected_mls_count
        {
            continue;
        }

        // Shape: non-empty (already checked at buffer time, but defensive)
        if candidate.candidate_msg.mls_proposals.is_empty()
            || candidate.candidate_msg.commit_message.is_empty()
        {
            continue;
        }

        // Dedup: not already committed
        if group.is_duplicate_commit_candidate(&candidate.commit_hash) {
            continue;
        }

        // Kind check (wire-level only)
        if !has_valid_candidate_wire_kinds(&candidate.candidate_msg, mls) {
            continue;
        }

        // Identity check: skip candidates with empty steward_identity
        if candidate.candidate_msg.steward_identity.is_empty() {
            tracing::debug!(
                "Skipping candidate for group {}: empty steward_identity",
                group.group_name(),
            );
            continue;
        }

        pre_validated.push(ValidatedCandidate {
            candidate_msg: candidate.candidate_msg.clone(),
            commit_hash: candidate.commit_hash.clone(),
            is_local_candidate: candidate.is_local_candidate,
            actions_count: candidate.candidate_msg.mls_proposals.len(),
            welcome_bytes: candidate.welcome_bytes.clone(),
            steward_identity: candidate.candidate_msg.steward_identity.clone(),
        });
    }

    if pre_validated.is_empty() {
        return Ok(FreezeFinalizeResult::NoCandidate);
    }

    // ── Phase 2: Selection (deterministic choice, content-only) ──
    let epoch_steward_id = group
        .steward_list()
        .and_then(|l| l.epoch_steward(current_epoch))
        .map(|s| s.to_vec());

    sort_candidates_by_priority(&mut pre_validated, epoch_steward_id.as_deref());

    // TODO(M2): If Phase 3 application fails for the top-sorted candidate, we currently
    // return NoCandidate without attempting the next pre-validated candidate. A malicious
    // node could exploit this by broadcasting a syntactically valid but MLS-invalid commit
    // to DoS the freeze round. For M2, retry through pre_validated in sorted order until
    // one succeeds or the list is exhausted (RFC §"Commit validation service" §"Fallback").
    let chosen = pre_validated.into_iter().next().unwrap();

    // ── Phase 3: Application (MLS processing — chosen candidate only) ──
    let group_name = group.group_name().to_owned();

    // Local candidate: steward already validated when creating the commit
    if group.is_steward() && chosen.is_local_candidate {
        let local_identity = mls.wallet_bytes();
        let self_removed = local_proposals.values().any(|req| {
            matches!(
                req.payload,
                Some(group_update_request::Payload::RemoveMember(ref remove))
                    if remove.identity == local_identity
            )
        });

        mls.merge_own_commit(&group_name)?;
        group.record_committed_batch(chosen.commit_hash);
        group.clear_approved_proposals();

        // Build deferred welcome packets now that commit is merged
        let outbound = build_deferred_welcome(chosen.welcome_bytes, group, app_id);
        group.clear_freeze_round();

        return Ok(FreezeFinalizeResult::Applied {
            result: process_result_from_commit_outcome(self_removed),
            outbound,
        });
    }

    // Remote candidate: discard own commit if steward
    if group.is_steward() {
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
                discard_candidate_state(group, mls)?;
                return Ok(FreezeFinalizeResult::NoCandidate);
            }
            Err(e) => {
                tracing::debug!(
                    "MLS proposal {} for group {} failed during application: {}",
                    i,
                    group_name,
                    e,
                );
                discard_candidate_state(group, mls)?;
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
            discard_candidate_state(group, mls)?;
            let steward_id = proposal_senders.into_iter().next().unwrap_or_default();
            return Ok(FreezeFinalizeResult::Applied {
                result: ProcessResult::ViolationDetected(ViolationEvidence::broken_mls_proposal(
                    steward_id,
                    current_epoch,
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
            discard_candidate_state(group, mls)?;
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
            discard_candidate_state(group, mls)?;
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
            discard_candidate_state(group, mls)?;
            return Ok(FreezeFinalizeResult::Applied {
                result: ProcessResult::ViolationDetected(ViolationEvidence::broken_commit(
                    commit_sender,
                    current_epoch,
                    "proposal sender differs from commit sender",
                )),
                outbound: vec![],
            });
        }
    }

    // Verify commit sender is an authorized committer (RFC §"Commit validation service").
    if let Some(violation) = check_commit_sender_authorized(group, &commit_sender, current_epoch) {
        tracing::warn!(
            "Violation: commit from unauthorized sender for group {}",
            group_name,
        );
        discard_candidate_state(group, mls)?;
        return Ok(FreezeFinalizeResult::Applied {
            result: ProcessResult::ViolationDetected(violation),
            outbound: vec![],
        });
    }

    // Validate MLS actions against local voted proposals
    if let Some(result) = validate_commit_candidate(
        group,
        &local_proposals,
        &commit_sender,
        &commit_actions,
        current_epoch,
    )? {
        discard_candidate_state(group, mls)?;
        return Ok(FreezeFinalizeResult::Applied {
            result,
            outbound: vec![],
        });
    }

    // All checks passed — merge
    mls.merge_staged_commit(&group_name)?;
    group.record_committed_batch(chosen.commit_hash);
    group.clear_approved_proposals();
    group.clear_freeze_round();

    // Remote candidates never carry welcome bytes (only local candidates do)
    Ok(FreezeFinalizeResult::Applied {
        result: process_result_from_commit_outcome(self_removed),
        outbound: vec![],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_candidate(
        steward_identity: Vec<u8>,
        actions_count: usize,
        commit_hash: Vec<u8>,
    ) -> ValidatedCandidate {
        ValidatedCandidate {
            candidate_msg: CommitCandidate {
                group_name: b"test-group".to_vec(),
                mls_proposals: vec![vec![0xFF; 10]; actions_count],
                commit_message: commit_hash.clone(),
                steward_identity: steward_identity.clone(),
            },
            commit_hash,
            is_local_candidate: false,
            actions_count,
            welcome_bytes: None,
            steward_identity,
        }
    }

    #[test]
    fn epoch_steward_wins_over_other_at_same_action_count() {
        let epoch_id = vec![0x01];
        let other_id = vec![0x02];

        let mut candidates = vec![
            make_candidate(other_id.clone(), 3, vec![0xAA]),
            make_candidate(epoch_id.clone(), 3, vec![0xBB]),
        ];

        sort_candidates_by_priority(&mut candidates, Some(&epoch_id));

        // Same action count: epoch steward wins via tier
        assert_eq!(candidates[0].steward_identity, epoch_id);
        assert_eq!(candidates[1].steward_identity, other_id);
    }

    #[test]
    fn more_actions_beats_epoch_steward() {
        // RFC: longest proposal sequence is primary criterion.
        // A non-epoch-steward with more actions beats the epoch steward with fewer.
        let epoch_id = vec![0x01];
        let other_id = vec![0x03];

        let mut candidates = vec![
            make_candidate(epoch_id.clone(), 3, vec![0xAA]),
            make_candidate(other_id.clone(), 5, vec![0xBB]),
        ];

        sort_candidates_by_priority(&mut candidates, Some(&epoch_id));

        // 5 actions > 3 actions, so non-epoch-steward wins
        assert_eq!(candidates[0].steward_identity, other_id);
        assert_eq!(candidates[0].actions_count, 5);
        assert_eq!(candidates[1].steward_identity, epoch_id);
        assert_eq!(candidates[1].actions_count, 3);
    }

    #[test]
    fn lexicographic_identity_tiebreak_same_action_count() {
        let epoch_id = vec![0x01];
        let other_a = vec![0x05];
        let other_b = vec![0x03];

        let mut candidates = vec![
            make_candidate(other_a.clone(), 3, vec![0xAA]),
            make_candidate(other_b.clone(), 3, vec![0xBB]),
        ];

        sort_candidates_by_priority(&mut candidates, Some(&epoch_id));

        // Same action count, both non-epoch: 0x03 < 0x05 lexicographically
        assert_eq!(candidates[0].steward_identity, other_b);
        assert_eq!(candidates[1].steward_identity, other_a);
    }

    #[test]
    fn same_identity_breaks_tie_by_action_count() {
        let epoch_id = vec![0x01];
        let other_id = vec![0x03];

        let mut candidates = vec![
            make_candidate(other_id.clone(), 2, vec![0xAA]),
            make_candidate(other_id.clone(), 5, vec![0xBB]),
        ];

        sort_candidates_by_priority(&mut candidates, Some(&epoch_id));

        // Same identity, larger action count first
        assert_eq!(candidates[0].actions_count, 5);
        assert_eq!(candidates[1].actions_count, 2);
    }

    #[test]
    fn same_identity_same_actions_breaks_tie_by_commit_hash() {
        let id = vec![0x05];

        let mut candidates = vec![
            make_candidate(id.clone(), 3, vec![0xCC]),
            make_candidate(id.clone(), 3, vec![0xAA]),
        ];

        sort_candidates_by_priority(&mut candidates, Some(&[0x01]));

        // Same identity, same action count, lowest commit_hash first
        assert_eq!(candidates[0].commit_hash, vec![0xAA]);
        assert_eq!(candidates[1].commit_hash, vec![0xCC]);
    }

    #[test]
    fn no_steward_list_falls_back_to_action_count_then_identity() {
        let id_a = vec![0x05];
        let id_b = vec![0x03];

        let mut candidates = vec![
            make_candidate(id_a.clone(), 2, vec![0xAA]),
            make_candidate(id_b.clone(), 5, vec![0xBB]),
        ];

        // No steward list
        sort_candidates_by_priority(&mut candidates, None);

        // 5 actions > 2 actions (primary criterion)
        assert_eq!(candidates[0].steward_identity, id_b);
        assert_eq!(candidates[0].actions_count, 5);
        assert_eq!(candidates[1].steward_identity, id_a);
        assert_eq!(candidates[1].actions_count, 2);
    }

    #[test]
    fn no_steward_list_same_actions_falls_back_to_identity() {
        let id_a = vec![0x05];
        let id_b = vec![0x03];

        let mut candidates = vec![
            make_candidate(id_a.clone(), 3, vec![0xAA]),
            make_candidate(id_b.clone(), 3, vec![0xBB]),
        ];

        sort_candidates_by_priority(&mut candidates, None);

        // Same action count, no steward list: lexicographic identity 0x03 < 0x05
        assert_eq!(candidates[0].steward_identity, id_b);
        assert_eq!(candidates[1].steward_identity, id_a);
    }

    #[test]
    fn no_steward_list_same_identity_falls_back_to_action_count() {
        let id = vec![0x05];

        let mut candidates = vec![
            make_candidate(id.clone(), 2, vec![0xAA]),
            make_candidate(id.clone(), 5, vec![0xBB]),
        ];

        sort_candidates_by_priority(&mut candidates, None);

        // Same identity, larger action count first
        assert_eq!(candidates[0].actions_count, 5);
        assert_eq!(candidates[1].actions_count, 2);
    }

    #[test]
    fn full_priority_order_actions_first_then_tier_then_identity() {
        let epoch_id = vec![0x01];
        let other_a = vec![0x03];
        let other_b = vec![0x04];

        // other_b has 5 actions (most), epoch has 3 (tied with other_a), other_a has 3
        let mut candidates = vec![
            make_candidate(other_b.clone(), 5, vec![0x11]),
            make_candidate(other_a.clone(), 3, vec![0x22]),
            make_candidate(epoch_id.clone(), 3, vec![0x44]),
        ];

        sort_candidates_by_priority(&mut candidates, Some(&epoch_id));

        // 1st: other_b (5 actions — most)
        assert_eq!(candidates[0].steward_identity, other_b);
        assert_eq!(candidates[0].actions_count, 5);
        // 2nd: epoch_id (3 actions, but epoch steward tier wins over other_a)
        assert_eq!(candidates[1].steward_identity, epoch_id);
        assert_eq!(candidates[1].actions_count, 3);
        // 3rd: other_a (3 actions, non-epoch)
        assert_eq!(candidates[2].steward_identity, other_a);
        assert_eq!(candidates[2].actions_count, 3);
    }
}
