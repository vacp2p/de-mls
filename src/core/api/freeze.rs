use super::validation::validate_commit_candidate;
use super::*;

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
pub(super) fn process_commit_candidate<S>(
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
    if buffered {
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
    allow_subset_candidates: bool,
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

    // ── Phase 2: Selection (deterministic choice, content-only) ──
    // All nodes must converge on the same candidate regardless of which is "local".
    // Prefer largest action set first, then lowest commit_hash as tiebreak.
    // is_local_candidate is intentionally excluded: it is node-local knowledge
    // and would cause divergence if multiple stewards exist (M3).
    pre_validated.sort_by(|a, b| {
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
