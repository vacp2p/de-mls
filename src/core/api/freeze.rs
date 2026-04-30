//! Freeze round candidate processing, selection, and commit application.

use openmls_rust_crypto::MemoryStorage;
use sha2::{Digest, Sha256};
use tracing::info;

use crate::{
    core::{
        CoreError, Group, ProcessResult, ScoreEvent, ScoreOp,
        api::lifecycle::build_invitation_packet, group::BufferedCommitCandidate,
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
/// `score_ops` accumulates during the phase-3 loop — each time a candidate
/// is dropped for MLS-staging failure the caller records a
/// [`ScoreEvent::MisbehavingCommit`] against its author. The app layer
/// feeds these directly into the peer-scoring service without an ECP
/// round-trip (RFC §Peer Scoring: locally-observed violations may be
/// scored immediately).
#[derive(Debug, Clone, Default)]
pub struct FreezeFinalizeResult {
    pub outcome: FreezeOutcome,
    pub score_ops: Vec<ScoreOp>,
}

/// Terminal outcome of a freeze round: either a dispatchable result or no
/// candidate was applyable.
///
/// `result` is boxed because [`ProcessResult`] is a large enum (~240 bytes)
/// and `NoCandidate` is the common case.
#[derive(Debug, Clone, Default)]
pub enum FreezeOutcome {
    /// A dispatchable result — successful apply or self-leave.
    /// `outbound` carries deferred welcomes when our own candidate won.
    Applied {
        result: Box<ProcessResult>,
        outbound: Option<OutboundPacket>,
    },
    #[default]
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
            tracing::debug!(group = %group_name, "candidate ignored: no approved proposals");
            return Ok(ProcessResult::Noop);
        }
        let epoch = mls.current_epoch(&group_name)?;
        group.ensure_freeze_round(epoch);
    }

    let commit_hash = compute_commit_hash(&candidate_msg.commit_message);
    if group.is_duplicate_commit_candidate(&commit_hash) {
        tracing::debug!(group = %group_name, "candidate ignored: already committed");
        return Ok(ProcessResult::Noop);
    }

    if candidate_msg.mls_proposals.is_empty() || candidate_msg.commit_message.is_empty() {
        tracing::debug!(group = %group_name, "candidate ignored: empty proposals or commit");
        return Ok(ProcessResult::Noop);
    }

    if candidate_msg.steward_identity.is_empty() {
        tracing::debug!(group = %group_name, "candidate ignored: empty steward_identity");
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
        group = %group_name,
        epoch,
        total_candidates = group.freeze_candidate_count(),
        "remote candidate buffered"
    );
    Ok(ProcessResult::CommitCandidateReceived)
}

/// Pick and apply a buffered candidate for the active freeze round.
///
/// Three phases:
/// 1. Snapshot the round context — approved-queue actions, current
///    epoch, and the pre-merge live epoch steward.
/// 2. Filter candidates by action count and rank them by RFC priority.
/// 3. Apply in priority order, falling back on the next candidate when
///    MLS staging rejects the current one.
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
        _ => {
            // Drop any local pending commit so the next MLS encrypt
            // doesn't trip on "pending proposal exists".
            let _ = mls.discard_own_commit(group.group_name());
            return Ok(FreezeFinalizeResult::default());
        }
    };

    if candidates.is_empty() {
        let _ = mls.discard_own_commit(group.group_name());
        return Ok(FreezeFinalizeResult::default());
    }

    let ctx = RoundContext::snapshot(group, mls, current_epoch)?;
    let sorted = rank_applicable_candidates(candidates, &ctx, allow_subset_candidates);

    if sorted.is_empty() {
        group.clear_freeze_round();
        let _ = mls.discard_own_commit(group.group_name());
        return Ok(FreezeFinalizeResult::default());
    }

    apply_in_priority_order(group, mls, sorted, &ctx, app_id)
}

// ═════════════════════════════════════════════════════════════════════════════
// PRIVATE HELPERS
// ═════════════════════════════════════════════════════════════════════════════

// ─────────────────────────── Round Setup ───────────────────────────

/// Precomputed round-level data: the approved-queue snapshot every
/// candidate must match, the epoch the round targets, and the identities
/// used for self-accusation skips and committer-vs-expected checks.
///
/// `mls_actions` is the expected MLS action set (non-MLS payloads
/// filtered out). `self_remove_pending` flips the local apply's terminal
/// result to `LeaveGroup` when our removal is in the batch.
/// `live_epoch_steward_id` is the pre-merge eligibility-filtered steward
/// expected to have committed at `current_epoch`; used to penalise an
/// absent steward when a backup commits in their place.
struct RoundContext {
    mls_actions: Vec<MlsProposalAction>,
    mls_count: usize,
    self_remove_pending: bool,
    current_epoch: u64,
    live_epoch_steward_id: Option<Vec<u8>>,
    self_identity: Vec<u8>,
}

impl RoundContext {
    fn snapshot<S>(
        group: &Group,
        mls: &MlsService<S>,
        current_epoch: u64,
    ) -> Result<Self, CoreError>
    where
        S: DeMlsStorage<MlsStorage = MemoryStorage>,
    {
        let self_identity = mls.wallet_bytes();

        let mut mls_actions: Vec<MlsProposalAction> = Vec::new();
        let mut self_remove_pending = false;
        for req in group.approved_proposals().values() {
            if let Some(action) = expected_action_for_request(req) {
                mls_actions.push(action);
            }
            if matches!(
                &req.payload,
                Some(Payload::RemoveMember(rm)) if rm.identity == self_identity
            ) {
                self_remove_pending = true;
            }
        }
        let mls_count = mls_actions.len();

        let members = mls.members(group.group_name())?;
        let live_epoch_steward_id = group
            .live_epoch_steward(current_epoch, &members)
            .map(|s| s.to_vec());

        Ok(Self {
            mls_actions,
            mls_count,
            self_remove_pending,
            current_epoch,
            live_epoch_steward_id,
            self_identity,
        })
    }
}

/// Filter candidates by action count (Phase 1) and sort survivors by the
/// RFC priority comparator (Phase 2). Result is ordered best-first.
///
/// Priority tiering uses the *live* epoch steward — eligibility-filtered
/// in `RoundContext::snapshot`, so a draining or already-removed nominal
/// steward never wins the tier.
fn rank_applicable_candidates(
    candidates: Vec<BufferedCommitCandidate>,
    ctx: &RoundContext,
    allow_subset: bool,
) -> Vec<BufferedCommitCandidate> {
    let mut sorted: Vec<_> = candidates
        .into_iter()
        .filter(|c| {
            let n = c.candidate_msg.mls_proposals.len();
            if allow_subset {
                n <= ctx.mls_count
            } else {
                n == ctx.mls_count
            }
        })
        .collect();
    sorted.sort_by(|a, b| compare_candidate_priority(a, b, ctx.live_epoch_steward_id.as_deref()));
    sorted
}

// ─────────────────────────── Phase-3 Loop ───────────────────────────

/// Result of trying to apply one candidate. `Terminal` ends the round;
/// `Drop` records a local score penalty and the caller tries the next.
///
/// `committer` is the MLS-verified sender (incoming) or our own identity
/// (local). Scoring keys off this so a forged wire claim cannot redirect
/// the `SuccessfulCommit` reward.
enum CandidateOutcome {
    Terminal {
        outcome: FreezeOutcome,
        committer: Vec<u8>,
    },
    Drop(ScoreOp),
}

/// Walk `sorted` in priority order. Each rejected candidate adds a local
/// score penalty; the first candidate that applies wins the round. No
/// ECP is filed — RFC §Peer Scoring allows direct local scoring for
/// observable violations.
///
/// `own_commit_discarded` enforces MLS's one-pending-commit rule: the
/// first incoming attempt wipes our own pending commit, so a
/// lower-priority local candidate afterwards has nothing to apply.
fn apply_in_priority_order<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    sorted: Vec<BufferedCommitCandidate>,
    ctx: &RoundContext,
    app_id: &[u8],
) -> Result<FreezeFinalizeResult, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let mut score_ops: Vec<ScoreOp> = Vec::new();
    let mut own_commit_discarded = false;
    let group_name = group.group_name().to_owned();

    let mut remaining = sorted.into_iter();
    while let Some(chosen) = remaining.next() {
        let apply_result = if chosen.is_local_candidate {
            if own_commit_discarded {
                tracing::debug!(
                    group = %group_name,
                    "own pending commit is discarded; skipping local candidate"
                );
                continue;
            }
            apply_local_candidate(group, mls, chosen, ctx.self_remove_pending, app_id)?
        } else {
            if !own_commit_discarded && group.is_steward() {
                // A failure here leaves an old pending commit in MLS and
                // would sabotage every subsequent staging attempt — bubble
                // the error out of the round instead of pressing on.
                mls.discard_own_commit(&group_name)?;
                own_commit_discarded = true;
            }
            apply_incoming_candidate(group, mls, chosen, &ctx.mls_actions, ctx.current_epoch)?
        };

        match apply_result {
            CandidateOutcome::Terminal { outcome, committer } => {
                score_ops.push(ScoreOp {
                    member_id: committer.clone(),
                    event: ScoreEvent::SuccessfulCommit,
                });
                // If a backup committed in place of the epoch steward,
                // penalise the absent steward (RFC §Steward violation
                // list: censorship/inactivity). Skip self-accusation —
                // we know our own liveness directly. The walk in
                // `live_epoch_steward_id` already filters out
                // queued-removal targets, so a draining named steward
                // never appears here as `expected`.
                if let Some(expected) = ctx.live_epoch_steward_id.as_deref() {
                    if expected != committer.as_slice() && expected != ctx.self_identity.as_slice()
                    {
                        score_ops.push(ScoreOp {
                            member_id: expected.to_vec(),
                            event: ScoreEvent::CensorshipInactivity,
                        });
                    }
                }
                // Unpicked candidates that passed the count filter are
                // honest competitors under Δ-synchrony (same approved set,
                // different MLS entropy). RFC §Commit Validation: "MUST
                // NOT be classified as misbehaviour." Score only when the
                // wire-claimed identity sits on the steward list — a forged
                // claim from a non-steward earns no credit.
                for loser in remaining {
                    let claimed = loser.candidate_msg.steward_identity;
                    let on_list = group.steward_list().is_some_and(|l| l.contains(&claimed));
                    if on_list {
                        score_ops.push(ScoreOp {
                            member_id: claimed,
                            event: ScoreEvent::HonestCommitAttempt,
                        });
                    } else {
                        tracing::debug!(
                            group = %group_name,
                            "dropping HonestCommitAttempt: claimed identity not on steward list"
                        );
                    }
                }
                return Ok(FreezeFinalizeResult { outcome, score_ops });
            }
            CandidateOutcome::Drop(op) => score_ops.push(op),
        }
    }

    // No candidate applied. Drop any local pending commit that wasn't
    // merged or discarded along an incoming-wins path — leaving it
    // behind would break the next MLS encrypt.
    if !own_commit_discarded {
        let _ = mls.discard_own_commit(&group_name);
    }
    group.clear_freeze_round();
    Ok(FreezeFinalizeResult {
        outcome: FreezeOutcome::NoCandidate,
        score_ops,
    })
}

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
/// Validation happened at commit-creation time, so no re-staging is needed.
/// Always returns `Terminal(Applied)` on a clean merge.
fn apply_local_candidate<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    chosen: BufferedCommitCandidate,
    self_removed: bool,
    app_id: &[u8],
) -> Result<CandidateOutcome, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    // Local candidate: we wrote the message, so the wire-claimed identity
    // is our own and is trusted by definition.
    let committer = chosen.candidate_msg.steward_identity.clone();

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
    Ok(CandidateOutcome::Terminal {
        outcome: FreezeOutcome::Applied {
            result: Box::new(result),
            outbound,
        },
        committer,
    })
}

/// Stage, validate, and merge a candidate authored by another steward.
/// Returns `Terminal(Applied)` on a clean merge, or `Drop` with a score
/// penalty if any check fails (MLS staging, sender mismatch,
/// unauthorized sender, action-set mismatch).
///
/// Caller must have discarded any own pending commit first — MLS allows
/// only one per group at a time.
fn apply_incoming_candidate<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    chosen: BufferedCommitCandidate,
    expected_actions: &[MlsProposalAction],
    current_epoch: u64,
) -> Result<CandidateOutcome, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    let group_name = group.group_name().to_owned();

    let (commit_sender, self_removed, commit_actions) =
        match stage_candidate(mls, &group_name, &chosen.candidate_msg, current_epoch)? {
            StagingOutcome::Staged {
                commit_sender,
                self_removed,
                commit_actions,
            } => (commit_sender, self_removed, commit_actions),
            StagingOutcome::Abort => {
                // Wire-valid but MLS-invalid — penalize the author; the
                // loop will try the next candidate.
                mls.discard_staged_commit(&group_name)?;
                return Ok(CandidateOutcome::Drop(ScoreOp {
                    member_id: chosen.candidate_msg.steward_identity,
                    event: ScoreEvent::MisbehavingCommit,
                }));
            }
            StagingOutcome::Violation(v) => {
                mls.discard_staged_commit(&group_name)?;
                return Ok(CandidateOutcome::Drop(
                    v.target_score_op()
                        .expect("staged-violation always has a target-side score"),
                ));
            }
        };

    // Commit sender must be on the steward list (RFC §"Commit validation service").
    if let Some(violation) = check_commit_sender_authorized(group, &commit_sender, current_epoch) {
        mls.discard_staged_commit(&group_name)?;
        return Ok(CandidateOutcome::Drop(
            violation
                .target_score_op()
                .expect("locally-built violation always has a target-side score"),
        ));
    }

    // MLS actions must match the set we voted to approve.
    if let Some(violation) = validate_commit_candidate(
        group,
        expected_actions,
        &commit_sender,
        &commit_actions,
        current_epoch,
    )? {
        mls.discard_staged_commit(&group_name)?;
        return Ok(CandidateOutcome::Drop(
            violation
                .target_score_op()
                .expect("locally-built violation always has a target-side score"),
        ));
    }

    mls.merge_staged_commit(&group_name)?;
    record_applied_commit(group, chosen.commit_hash);

    // Remote candidates never carry welcome bytes — only the author sends those.
    let result = if self_removed {
        ProcessResult::LeaveGroup
    } else {
        ProcessResult::GroupUpdated
    };
    Ok(CandidateOutcome::Terminal {
        outcome: FreezeOutcome::Applied {
            result: Box::new(result),
            outbound: None,
        },
        committer: commit_sender,
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
                    group = group_name,
                    index = i,
                    outcome = ?outcome,
                    "MLS proposal rejected during application"
                );
                return Ok(StagingOutcome::Abort);
            }
        }
    }

    let staged_result = match mls.process_commit(group_name, &candidate.commit_message) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(
                group = group_name,
                error = %e,
                "commit failed to process during application"
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

    // Wire-claimed `steward_identity` must match the MLS-verified
    // `commit_sender`; mismatch is a broken commit (RFC §Steward
    // violation list) and is attributed to the actual signer.
    if candidate.steward_identity != commit_sender {
        tracing::warn!(
            group = group_name,
            "violation: wire steward_identity doesn't match MLS commit_sender"
        );
        return Ok(StagingOutcome::Violation(ViolationEvidence::broken_commit(
            commit_sender,
            current_epoch,
            "commit candidate's steward_identity doesn't match MLS commit sender",
        )));
    }

    // Every bundled proposal must come from the committer — catches both
    // "proposals signed by a third party" and "proposals don't all agree
    // on a sender". Attribution always lands on the committer (RFC
    // §Steward violation list: only the member who released the commit is
    // accused).
    if proposal_senders.iter().any(|s| s != &commit_sender) {
        tracing::warn!(
            group = group_name,
            "violation: bundled proposals don't match the commit sender"
        );
        return Ok(StagingOutcome::Violation(ViolationEvidence::broken_commit(
            commit_sender,
            current_epoch,
            "commit bundles proposals not signed by the committer",
        )));
    }

    Ok(StagingOutcome::Staged {
        commit_sender,
        self_removed,
        commit_actions,
    })
}

// ─────────────────────────── Validation ───────────────────────────

/// Check that a commit's MLS actions match the voted-approved set.
/// `Some(evidence)` on mismatch.
fn validate_commit_candidate(
    group: &Group,
    expected_actions: &[MlsProposalAction],
    sender_id: &[u8],
    mls_actions: &[MlsProposalAction],
    epoch: u64,
) -> Result<Option<ViolationEvidence>, CoreError> {
    let group_name = group.group_name();

    let mut expected_actions = expected_actions.to_vec();
    let mut actual_actions = mls_actions.to_vec();

    expected_actions.sort();
    actual_actions.sort();

    if actual_actions != expected_actions {
        tracing::warn!(
            group = group_name,
            actual = ?actual_actions,
            expected = ?expected_actions,
            "violation: MLS actions don't match voted proposals"
        );
        return Ok(Some(ViolationEvidence::broken_mls_proposal(
            sender_id.to_vec(),
            epoch,
            format!("MLS actions {actual_actions:?} != voted {expected_actions:?}"),
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
    tracing::warn!(
        group = group.group_name(),
        "violation: commit from unauthorized sender"
    );
    Some(ViolationEvidence::broken_commit(
        commit_sender.to_vec(),
        epoch,
        "commit from unauthorized sender (not on the steward list)",
    ))
}

// ─────────────────────────── State Utilities ───────────────────────────

fn record_applied_commit(group: &mut Group, commit_hash: Vec<u8>) {
    group.record_committed_batch(commit_hash);
    if let Some(target) = group.take_urgent_commit_target() {
        // Urgent commit: leave the rest of the queue for the next cycle.
        group.drop_approved_removals_for(&target);
    } else {
        group.clear_approved_proposals();
    }
    group.clear_freeze_round();
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
