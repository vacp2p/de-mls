//! Freeze round candidate processing, selection, and commit application.

use openmls_rust_crypto::MemoryStorage;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::{
    core::{
        CoreError, Group, ProcessResult, api::lifecycle::build_invitation_packet,
        group::BufferedCommitCandidate,
    },
    ds::OutboundPacket,
    mls_crypto::{
        DeMlsStorage, DecryptResult, MlsMessageKind, MlsProposalAction, MlsService,
        StagedCommitResult,
    },
    protos::de_mls::messages::v1::{
        CommitCandidate, GroupUpdateRequest, ViolationEvidence, group_update_request::Payload,
    },
};

// ═════════════════════════════════════════════════════════════════════════════
// PUBLIC API
// ═════════════════════════════════════════════════════════════════════════════

/// What [`finalize_freeze_round`] hands back to the caller.
///
/// `result` is boxed because [`ProcessResult`] is a large enum (~240 bytes);
/// keeping it inline would bloat every `NoCandidate` return too.
#[derive(Debug, Clone)]
pub enum FreezeFinalizeResult {
    /// A dispatchable result — successful apply, self-leave, or violation.
    /// `outbound` carries deferred welcomes when our own candidate won.
    Outcome {
        result: Box<ProcessResult>,
        outbound: Option<OutboundPacket>,
    },
    NoCandidate,
}

/// Canonical commit hash used for dedup of buffered/committed candidates.
pub fn compute_commit_hash(commit_message: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(commit_message);
    hasher.finalize().to_vec()
}

/// Buffer a remote commit candidate for the active freeze round.
///
/// Enforces the invariants [`finalize_freeze_round`] assumes: non-empty
/// proposals/commit, valid MLS wire kinds, non-empty `steward_identity`,
/// not already committed. No MLS state is mutated here.
pub fn process_commit_candidate<S>(
    group: &mut Group,
    candidate_msg: CommitCandidate,
    mls: &MlsService<S>,
) -> Result<ProcessResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = group.group_name().to_owned();

    // Auto-start a freeze round if we already have approved proposals —
    // otherwise the candidate would be silently dropped.
    if group.freeze_round().is_none() {
        if group.approved_proposals_count() == 0 {
            tracing::debug!("Ignoring candidate for group {group_name}: no approved proposals");
            return Ok(ProcessResult::Noop);
        }
        let epoch = mls.current_epoch(&group_name)?;
        group.ensure_freeze_round(epoch);
    }

    let commit_hash = compute_commit_hash(&candidate_msg.commit_message);
    if group.is_duplicate_commit_candidate(&commit_hash) {
        tracing::debug!("Ignoring duplicate candidate for group {group_name}: already committed");
        return Ok(ProcessResult::Noop);
    }

    if candidate_msg.mls_proposals.is_empty() || candidate_msg.commit_message.is_empty() {
        tracing::debug!("Ignoring candidate for group {group_name}: empty proposals or commit");
        return Ok(ProcessResult::Noop);
    }

    if candidate_msg.steward_identity.is_empty() {
        tracing::debug!("Ignoring candidate for group {group_name}: empty steward_identity");
        return Ok(ProcessResult::Noop);
    }

    // Wire-level kind check — no MLS staging.
    let proposals_ok = candidate_msg
        .mls_proposals
        .iter()
        .all(|p| matches!(mls.inspect_message_kind(p), Ok(MlsMessageKind::Proposal)));
    let commit_ok = matches!(
        mls.inspect_message_kind(&candidate_msg.commit_message),
        Ok(MlsMessageKind::Commit)
    );
    if !proposals_ok || !commit_ok {
        return Ok(ProcessResult::Noop);
    }

    let epoch = mls.current_epoch(&group_name)?;
    let buffered = group.add_freeze_candidate(
        BufferedCommitCandidate {
            candidate_msg,
            commit_hash,
            is_local_candidate: false,
            welcome_bytes: None,
        },
        epoch,
    );
    if !buffered {
        return Ok(ProcessResult::Noop);
    }

    info!(
        "[process_commit_candidate] Buffered remote candidate for group {group_name} \
         (epoch={epoch}, total_candidates={})",
        group.freeze_candidate_count(),
    );
    Ok(ProcessResult::CommitCandidateReceived)
}

/// Pick and apply a buffered candidate for the active freeze round.
///
/// 1. filter by proposal count,
/// 2. select via the RFC priority comparator,
/// 3. apply via the local-candidate or incoming-candidate path.
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

    // Pre-compute everything we need from the approved queue so we don't keep
    // a borrow alive across the `&mut group` calls below. `expected_actions`
    // doubles as the MLS-producing filter (emergency/election map to `None`).
    let self_identity = mls.wallet_bytes();
    let mut expected_actions: Vec<MlsProposalAction> = Vec::new();
    let mut self_remove_pending = false;
    for req in group.approved_proposals().values() {
        if let Some(action) = expected_action_for_request(req) {
            expected_actions.push(action);
        }
        if matches!(&req.payload, Some(Payload::RemoveMember(rm)) if rm.identity == self_identity) {
            self_remove_pending = true;
        }
    }
    let expected_mls_count = expected_actions.len();

    // Phase 1 — filter by proposal count. Other invariants were set at buffer time.
    let applicable = candidates.into_iter().filter(|c| {
        let n = c.candidate_msg.mls_proposals.len();
        if allow_subset_candidates {
            n <= expected_mls_count
        } else {
            n == expected_mls_count
        }
    });

    // Phase 2 — pick the winner. `min_by` is O(n); we never read past the top.
    // If the winner fails to apply we return NoCandidate; ROADMAP Track A
    // folds MLS staging into validation so only applyable candidates survive.
    let epoch_steward_id = group
        .steward_list()
        .and_then(|l| l.epoch_steward(current_epoch))
        .map(|s| s.to_vec());
    let chosen = match applicable
        .min_by(|a, b| compare_candidate_priority(a, b, epoch_steward_id.as_deref()))
    {
        Some(c) => c,
        None => return Ok(FreezeFinalizeResult::NoCandidate),
    };

    // Phase 3 — dispatch. `is_local_candidate` is only set by a steward's own
    // `create_commit_candidate`, so no extra `is_steward()` guard needed.
    if chosen.is_local_candidate {
        apply_local_candidate(group, mls, chosen, self_remove_pending, app_id)
    } else {
        apply_incoming_candidate(group, mls, chosen, &expected_actions, current_epoch)
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// PRIVATE HELPERS
// ═════════════════════════════════════════════════════════════════════════════

// ─────────────────────────── Selection ───────────────────────────

/// RFC §"Commit validation service" selection priority, in order:
///   1. largest proposal count,
///   2. epoch steward before anyone else,
///   3. lexicographically smallest `steward_identity`,
///   4. lowest `commit_hash`.
///
/// `is_local_candidate` is deliberately ignored — it's node-local and would
/// break convergence. With no steward list (e.g. joiner pre-sync), tier
/// collapses and the remaining criteria still decide.
fn compare_candidate_priority(
    a: &BufferedCommitCandidate,
    b: &BufferedCommitCandidate,
    epoch_steward_id: Option<&[u8]>,
) -> std::cmp::Ordering {
    // Action count — `b` first to get descending order.
    let size_cmp = b
        .candidate_msg
        .mls_proposals
        .len()
        .cmp(&a.candidate_msg.mls_proposals.len());
    if size_cmp != std::cmp::Ordering::Equal {
        return size_cmp;
    }

    // Tier: 0 for epoch steward, 1 for everyone else.
    let tier = |c: &BufferedCommitCandidate| -> u8 {
        match epoch_steward_id {
            Some(es) if c.candidate_msg.steward_identity == es => 0,
            _ => 1,
        }
    };
    let tier_cmp = tier(a).cmp(&tier(b));
    if tier_cmp != std::cmp::Ordering::Equal {
        return tier_cmp;
    }

    let id_cmp = a
        .candidate_msg
        .steward_identity
        .cmp(&b.candidate_msg.steward_identity);
    if id_cmp != std::cmp::Ordering::Equal {
        return id_cmp;
    }

    a.commit_hash.cmp(&b.commit_hash)
}

// ─────────────────────────── Application Paths ───────────────────────────

/// Merge our own commit and send the deferred welcomes we held back.
/// We already validated at commit-creation time — no re-staging needed.
fn apply_local_candidate<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    chosen: BufferedCommitCandidate,
    self_removed: bool,
    app_id: &[u8],
) -> Result<FreezeFinalizeResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    mls.merge_own_commit(group.group_name())?;

    // Welcomes go out only after our merge — joiners must not race ahead of the steward's epoch.
    let outbound = chosen
        .welcome_bytes
        .map(|bytes| build_invitation_packet(bytes, group, app_id));

    record_applied_commit(group, chosen.commit_hash);

    let result = if self_removed {
        ProcessResult::LeaveGroup
    } else {
        ProcessResult::GroupUpdated
    };
    Ok(FreezeFinalizeResult::Outcome {
        result: Box::new(result),
        outbound,
    })
}

/// Stage, validate, and merge a candidate authored by another steward.
fn apply_incoming_candidate<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    chosen: BufferedCommitCandidate,
    expected_actions: &[MlsProposalAction],
    current_epoch: u64,
) -> Result<FreezeFinalizeResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = group.group_name().to_owned();

    // Drop any own staged commit before applying someone else's. Absence of a
    // staged commit is a benign no-op, hence log-and-continue on failure.
    if group.is_steward() {
        if let Err(e) = mls.discard_own_commit(&group_name) {
            warn!("[apply_incoming_candidate] discard_own_commit failed: {e}");
        }
    }

    let (commit_sender, self_removed, commit_actions) =
        match stage_candidate(mls, &group_name, &chosen.candidate_msg, current_epoch)? {
            StagingOutcome::Staged {
                commit_sender,
                self_removed,
                commit_actions,
            } => (commit_sender, self_removed, commit_actions),
            StagingOutcome::Abort => return discard_and_abort(group, mls),
            StagingOutcome::Violation(v) => return discard_and_report_violation(group, mls, v),
        };

    // Commit sender must be on the steward list (RFC §"Commit validation service").
    if let Some(violation) = check_commit_sender_authorized(group, &commit_sender, current_epoch) {
        tracing::warn!("Violation: commit from unauthorized sender for group {group_name:?}");
        return discard_and_report_violation(group, mls, violation);
    }

    // MLS actions must match the set we voted to approve.
    if let Some(result) = validate_commit_candidate(
        group,
        expected_actions,
        &commit_sender,
        &commit_actions,
        current_epoch,
    )? {
        discard_candidate_state(group, mls)?;
        return Ok(FreezeFinalizeResult::Outcome {
            result: Box::new(result),
            outbound: None,
        });
    }

    mls.merge_staged_commit(&group_name)?;
    record_applied_commit(group, chosen.commit_hash);

    // Remote candidates never carry welcome bytes — only the author sends those.
    let result = if self_removed {
        ProcessResult::LeaveGroup
    } else {
        ProcessResult::GroupUpdated
    };
    Ok(FreezeFinalizeResult::Outcome {
        result: Box::new(result),
        outbound: None,
    })
}

// ─────────────────────────── MLS Staging ───────────────────────────

enum StagingOutcome {
    /// Staged cleanly; senders are internally consistent.
    Staged {
        commit_sender: Vec<u8>,
        self_removed: bool,
        commit_actions: Vec<MlsProposalAction>,
    },
    /// MLS rejected a piece (bad wire, protocol error).
    Abort,
    /// Staged, but a sender-consistency invariant failed.
    Violation(ViolationEvidence),
}

/// Stage proposals and commit, cross-checking sender consistency along the way.
///
/// Leaves MLS in the staged state on `Staged`; the caller must clean up via
/// `discard_and_*` for `Abort` / `Violation`.
fn stage_candidate<S>(
    mls: &MlsService<S>,
    group_name: &str,
    candidate: &CommitCandidate,
    current_epoch: u64,
) -> Result<StagingOutcome, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // Any non-`ProposalStored` outcome from MLS aborts staging.
    let mut proposal_senders: Vec<Vec<u8>> = Vec::new();
    for (i, proposal_bytes) in candidate.mls_proposals.iter().enumerate() {
        match mls.process_candidate_proposal(group_name, proposal_bytes) {
            Ok(DecryptResult::ProposalStored(sender, _action)) => proposal_senders.push(sender),
            outcome => {
                tracing::debug!(
                    "MLS proposal {i} for group {group_name:?} rejected during application: {outcome:?}",
                );
                return Ok(StagingOutcome::Abort);
            }
        }
    }

    // All proposals must come from the same sender.
    if let Some(first) = proposal_senders.first() {
        if proposal_senders.iter().any(|s| s != first) {
            tracing::warn!("Violation: proposals have different senders for group {group_name:?}");
            return Ok(StagingOutcome::Violation(
                ViolationEvidence::broken_mls_proposal(
                    first.clone(),
                    current_epoch,
                    "proposals have different senders",
                ),
            ));
        }
    }

    let staged_result = match mls.process_commit(group_name, &candidate.commit_message) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(
                "Commit for group {group_name:?} failed to process during application: {e}",
            );
            return Ok(StagingOutcome::Abort);
        }
    };
    let (commit_sender, self_removed, commit_actions) = match staged_result {
        StagedCommitResult::Staged {
            sender_identity,
            self_removed,
            actions,
        } => (sender_identity, self_removed, actions),
        StagedCommitResult::Ignored => return Ok(StagingOutcome::Abort),
    };

    // Commit sender must match the proposals' sender.
    if let Some(first) = proposal_senders.first() {
        if first != &commit_sender {
            tracing::warn!("Violation: proposal sender != commit sender for group {group_name:?}");
            return Ok(StagingOutcome::Violation(ViolationEvidence::broken_commit(
                commit_sender,
                current_epoch,
                "proposal sender differs from commit sender",
            )));
        }
    }

    Ok(StagingOutcome::Staged {
        commit_sender,
        self_removed,
        commit_actions,
    })
}

// ─────────────────────────── Validation ───────────────────────────

/// Check that a commit's MLS actions equal the set we voted to approve.
fn validate_commit_candidate(
    group: &Group,
    expected_actions: &[MlsProposalAction],
    sender_id: &[u8],
    mls_actions: &[MlsProposalAction],
    epoch: u64,
) -> Result<Option<ProcessResult>, CoreError> {
    let group_name = group.group_name();

    let mut expected_actions = expected_actions.to_vec();
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

/// The MLS action a voted request should map to, or `None` for non-MLS
/// payloads (emergency/election) — doubles as the "MLS-producing" filter.
fn expected_action_for_request(req: &GroupUpdateRequest) -> Option<MlsProposalAction> {
    match &req.payload {
        Some(Payload::InviteMember(im)) => Some(MlsProposalAction::Add(im.identity.clone())),
        Some(Payload::RemoveMember(rm)) => Some(MlsProposalAction::Remove(rm.identity.clone())),
        Some(Payload::EmergencyCriteria(_)) | Some(Payload::StewardElection(_)) | None => None,
    }
}

/// RFC §"de-MLS Objects": any steward-list member may commit to preserve
/// liveness. Epoch-steward priority is about *selection*, not authorization.
///
/// `None` also covers "no list yet" (joiner pre-sync) and "list exhausted"
/// (re-election in progress).
fn check_commit_sender_authorized(
    group: &Group,
    commit_sender: &[u8],
    epoch: u64,
) -> Option<ViolationEvidence> {
    let list = group.steward_list()?;
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

// ─────────────────────────── State Utilities ───────────────────────────

fn discard_candidate_state<S>(group: &mut Group, mls: &MlsService<S>) -> Result<(), CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = group.group_name();
    mls.discard_staged_commit(group_name)?;
    group.clear_freeze_round();
    Ok(())
}

fn record_applied_commit(group: &mut Group, commit_hash: Vec<u8>) {
    group.record_committed_batch(commit_hash);
    group.clear_approved_proposals();
    group.clear_freeze_round();
}

fn discard_and_abort<S>(
    group: &mut Group,
    mls: &MlsService<S>,
) -> Result<FreezeFinalizeResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    discard_candidate_state(group, mls)?;
    Ok(FreezeFinalizeResult::NoCandidate)
}

fn discard_and_report_violation<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    violation: ViolationEvidence,
) -> Result<FreezeFinalizeResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    discard_candidate_state(group, mls)?;
    Ok(FreezeFinalizeResult::Outcome {
        result: Box::new(ProcessResult::ViolationDetected(violation)),
        outbound: None,
    })
}

// ═════════════════════════════════════════════════════════════════════════════
// TESTS
// ═════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only: sort candidates by the RFC priority comparator so tests can
    /// assert the full ranked order. Production uses `min_by` instead.
    fn sort_by_priority(
        candidates: &mut [BufferedCommitCandidate],
        epoch_steward_id: Option<&[u8]>,
    ) {
        candidates.sort_by(|a, b| compare_candidate_priority(a, b, epoch_steward_id));
    }

    fn make_candidate(
        steward_identity: Vec<u8>,
        actions_count: usize,
        commit_hash: Vec<u8>,
    ) -> BufferedCommitCandidate {
        BufferedCommitCandidate {
            candidate_msg: CommitCandidate {
                group_name: b"test-group".to_vec(),
                mls_proposals: vec![vec![0xFF; 10]; actions_count],
                commit_message: commit_hash.clone(),
                steward_identity,
            },
            commit_hash,
            is_local_candidate: false,
            welcome_bytes: None,
        }
    }

    /// Primary criterion: longer proposal sequence wins, even over the epoch steward.
    #[test]
    fn more_actions_beats_epoch_steward() {
        let epoch_id = vec![0x01];
        let other_id = vec![0x03];

        let mut candidates = vec![
            make_candidate(epoch_id.clone(), 3, vec![0xAA]),
            make_candidate(other_id.clone(), 5, vec![0xBB]),
        ];

        sort_by_priority(&mut candidates, Some(&epoch_id));

        assert_eq!(candidates[0].candidate_msg.steward_identity, other_id);
        assert_eq!(candidates[0].candidate_msg.mls_proposals.len(), 5);
    }

    /// Second criterion: equal action count → epoch steward wins on tier.
    #[test]
    fn epoch_steward_wins_tier_on_equal_action_count() {
        let epoch_id = vec![0x01];
        let other_id = vec![0x02];

        let mut candidates = vec![
            make_candidate(other_id.clone(), 3, vec![0xAA]),
            make_candidate(epoch_id.clone(), 3, vec![0xBB]),
        ];

        sort_by_priority(&mut candidates, Some(&epoch_id));

        assert_eq!(candidates[0].candidate_msg.steward_identity, epoch_id);
    }

    /// Third criterion: equal tier → lexicographically smallest identity wins.
    #[test]
    fn lexicographic_identity_tiebreak_when_tier_equal() {
        let epoch_id = vec![0x01];
        let other_a = vec![0x05];
        let other_b = vec![0x03];

        let mut candidates = vec![
            make_candidate(other_a.clone(), 3, vec![0xAA]),
            make_candidate(other_b.clone(), 3, vec![0xBB]),
        ];

        sort_by_priority(&mut candidates, Some(&epoch_id));

        assert_eq!(candidates[0].candidate_msg.steward_identity, other_b);
    }

    /// Final tiebreak: same identity, same action count → lowest commit hash wins.
    #[test]
    fn commit_hash_as_final_tiebreak() {
        let id = vec![0x05];

        let mut candidates = vec![
            make_candidate(id.clone(), 3, vec![0xCC]),
            make_candidate(id.clone(), 3, vec![0xAA]),
        ];

        sort_by_priority(&mut candidates, Some(&[0x01]));

        assert_eq!(candidates[0].commit_hash, vec![0xAA]);
    }

    /// No steward list → tier is always 1, so the tier check is a no-op and
    /// we fall through to identity comparison.
    #[test]
    fn no_steward_list_flattens_tier_and_falls_through_to_identity() {
        let id_a = vec![0x05];
        let id_b = vec![0x03];

        let mut candidates = vec![
            make_candidate(id_a.clone(), 3, vec![0xAA]),
            make_candidate(id_b.clone(), 3, vec![0xBB]),
        ];

        sort_by_priority(&mut candidates, None);

        assert_eq!(candidates[0].candidate_msg.steward_identity, id_b);
    }

    /// Integration: three candidates mixing all criteria in one ordering.
    #[test]
    fn full_priority_order_actions_first_then_tier_then_identity() {
        // other_b: 5 actions (wins on primary)
        // epoch:   3 actions, epoch-steward tier
        // other_a: 3 actions, non-epoch
        let epoch_id = vec![0x01];
        let other_a = vec![0x03];
        let other_b = vec![0x04];

        let mut candidates = vec![
            make_candidate(other_b.clone(), 5, vec![0x11]),
            make_candidate(other_a.clone(), 3, vec![0x22]),
            make_candidate(epoch_id.clone(), 3, vec![0x44]),
        ];

        sort_by_priority(&mut candidates, Some(&epoch_id));

        assert_eq!(candidates[0].candidate_msg.steward_identity, other_b);
        assert_eq!(candidates[1].candidate_msg.steward_identity, epoch_id);
        assert_eq!(candidates[2].candidate_msg.steward_identity, other_a);
    }
}
