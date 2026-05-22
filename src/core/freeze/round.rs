//! Freeze-round public surface, per-round setup, and priority selection.
//!
//! Per-candidate apply lives in the sibling [`super::apply`] module.

use sha2::{Digest, Sha256};
use tracing::info;

use crate::{
    core::{
        Conversation, CoreError, FreezeBufferOutcome, NoopReason, ProcessResult, ScoreOp,
        StewardListPlugin, conversation::BufferedCommitCandidate,
        freeze::apply::apply_in_priority_order,
    },
    mls_crypto::{MlsMessageKind, MlsService},
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, MemberWelcome,
        conversation_update_request::Payload,
    },
};

// ═════════════════════════════════════════════════════════════════════════════
// PUBLIC API
// ═════════════════════════════════════════════════════════════════════════════

/// What [`finalize_freeze_round`] hands back to the caller.
#[derive(Debug, Clone, Default)]
pub struct FreezeFinalizeResult {
    pub outcome: FreezeOutcome,
    pub score_ops: Vec<ScoreOp>,
    /// Just-committed approvals in FIFO order. Empty on no-op or when
    /// an urgent-target commit drops only its entry.
    pub committed_batch: Vec<ConversationUpdateRequest>,
}

#[derive(Debug, Clone, Default)]
pub enum FreezeOutcome {
    Applied {
        result: ProcessResult,
        /// Welcome artifact when our own commit added members.
        /// Surfaced to integrators via
        /// [`crate::core::SessionEvent::WelcomeReady`].
        welcome: Option<MemberWelcome>,
    },
    #[default]
    NoCandidate,
}

/// SHA-256 of a commit message. Used for dedup of buffered and
/// committed candidates, and as the final tiebreak in candidate
/// ordering. Fixed-size, no allocation per hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitHash([u8; 32]);

impl CommitHash {
    /// Raw 32-byte hash.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Canonical commit hash used for dedup of buffered/committed candidates.
pub fn compute_commit_hash(commit_message: &[u8]) -> CommitHash {
    let mut hasher = Sha256::new();
    hasher.update(commit_message);
    CommitHash(hasher.finalize().into())
}

/// Buffer a remote commit candidate. Enforces non-empty proposals/commit,
/// valid MLS wire kinds, non-empty `steward_identity`, and non-duplicate
/// hash. No MLS state is mutated.
pub fn buffer_commit_candidate<M: MlsService>(
    conversation: &mut Conversation,
    mls: &mut M,
    candidate_msg: CommitCandidate,
) -> Result<ProcessResult, CoreError> {
    let conversation_id = conversation.name().to_owned();

    // Auto-start a freeze round if we already have approved proposals —
    // otherwise the candidate would be silently dropped.
    if conversation.freeze_round().is_none() {
        if conversation.approved_proposals_count() == 0 {
            tracing::debug!(conversation = %conversation_id, "candidate ignored: no approved proposals");
            return Ok(ProcessResult::Noop(NoopReason::NoApprovedProposals));
        }
        let epoch = mls.current_epoch()?;
        conversation.ensure_freeze_round(epoch);
    }

    let commit_hash = compute_commit_hash(&candidate_msg.commit_message);
    if conversation.is_duplicate_commit_candidate(&commit_hash) {
        tracing::debug!(conversation = %conversation_id, "candidate ignored: already committed");
        return Ok(ProcessResult::Noop(NoopReason::AlreadyCommitted));
    }

    if candidate_msg.mls_proposals.is_empty() || candidate_msg.commit_message.is_empty() {
        tracing::debug!(conversation = %conversation_id, "candidate ignored: empty proposals or commit");
        return Ok(ProcessResult::Noop(NoopReason::EmptyCandidatePayload));
    }

    if candidate_msg.steward_identity.is_empty() {
        tracing::debug!(conversation = %conversation_id, "candidate ignored: empty steward_identity");
        return Ok(ProcessResult::Noop(NoopReason::EmptyStewardIdentity));
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
        tracing::debug!(
            conversation = %conversation_id,
            proposals_ok,
            commit_ok,
            "candidate ignored: wire kind mismatch (not Proposal/Commit)"
        );
        return Ok(ProcessResult::Noop(NoopReason::WireKindMismatch));
    }

    let steward = candidate_msg.steward_identity.clone();
    let epoch = mls.current_epoch()?;
    let outcome = conversation.add_freeze_candidate(
        BufferedCommitCandidate {
            candidate_msg,
            commit_hash,
            is_local_candidate: false,
            welcome_bytes: None,
        },
        epoch,
    );
    match outcome {
        FreezeBufferOutcome::Buffered => {
            info!(
                conversation = %conversation_id,
                epoch,
                total_candidates = conversation.freeze_candidate_count(),
                "remote candidate buffered"
            );
            Ok(ProcessResult::CommitCandidateReceived { steward })
        }
        // Legitimate runtime states, not errors — drop quietly.
        FreezeBufferOutcome::SelectionLocked => {
            Ok(ProcessResult::Noop(NoopReason::SelectionLocked))
        }
        FreezeBufferOutcome::StaleEpoch => Ok(ProcessResult::Noop(NoopReason::StaleEpoch)),
        FreezeBufferOutcome::DuplicateHash => {
            Ok(ProcessResult::Noop(NoopReason::DuplicateBufferedHash))
        }
    }
}

/// Pick and apply a buffered candidate for the active freeze round.
///
/// Three phases:
/// 1. Snapshot the pre-merge round state (counts, epoch, recovery flag,
///    live epoch steward).
/// 2. Filter candidates by action count and rank them by RFC priority.
/// 3. Apply in priority order, falling back on the next candidate when
///    MLS staging rejects the current one.
pub fn finalize_freeze_round<M: MlsService, St: StewardListPlugin>(
    conversation: &mut Conversation,
    mls: &mut M,
    steward: &St,
    in_recovery: bool,
    allow_subset_candidates: bool,
    self_identity: &[u8],
) -> Result<FreezeFinalizeResult, CoreError> {
    let current_epoch = mls.current_epoch()?;
    conversation.lock_freeze_round_selection(current_epoch);

    let Some(candidates) = conversation.take_round_candidates(current_epoch) else {
        // Drop any local pending commit so the next MLS encrypt
        // doesn't trip on "pending proposal exists".
        mls.discard_own_commit()?;
        return Ok(FreezeFinalizeResult::default());
    };

    if candidates.is_empty() {
        mls.discard_own_commit()?;
        return Ok(FreezeFinalizeResult::default());
    }

    let ctx = RoundContext::snapshot(
        conversation,
        mls,
        steward,
        current_epoch,
        in_recovery,
        self_identity,
    )?;
    let sorted = rank_applicable_candidates(candidates, &ctx, allow_subset_candidates);

    if sorted.is_empty() {
        mls.discard_own_commit()?;
        return Ok(FreezeFinalizeResult::default());
    }

    apply_in_priority_order(conversation, mls, steward, sorted, &ctx, self_identity)
}

// ═════════════════════════════════════════════════════════════════════════════
// ROUND SETUP
// ═════════════════════════════════════════════════════════════════════════════

/// Pre-merge round state used during candidate apply.
pub(super) struct RoundContext {
    /// Number of MLS-producing approvals (Add/Remove). Used to filter
    /// candidates whose proposal count doesn't match the voted set.
    pub(super) mls_count: usize,
    /// Flips the local apply's terminal result to `LeaveConversation`
    /// when our removal is in the batch.
    pub(super) self_remove_pending: bool,
    pub(super) current_epoch: u64,
    /// RFC §Layer 3 Anti-Deadlock: any member MAY commit when set;
    /// otherwise the commit sender must be on the steward list.
    pub(super) in_recovery: bool,
    /// Pre-merge eligibility-filtered steward expected at
    /// `current_epoch`. Used to penalise an absent steward when a
    /// backup commits in their place.
    pub(super) live_epoch_steward_id: Option<Vec<u8>>,
}

impl RoundContext {
    fn snapshot<M: MlsService, St: StewardListPlugin>(
        conversation: &Conversation,
        mls: &mut M,
        steward: &St,
        current_epoch: u64,
        in_recovery: bool,
        self_identity: &[u8],
    ) -> Result<Self, CoreError> {
        let mls_count = conversation
            .approved_proposals()
            .values()
            .filter(|req| produces_mls_action(req))
            .count();
        let self_remove_pending = conversation.is_pending_removal(self_identity);

        let members = mls.members()?;
        let eligible = conversation.steward_eligibility(&members);
        let live_epoch_steward_id = steward
            .epoch_steward(current_epoch, &eligible)
            .map(|s| s.to_vec());

        Ok(Self {
            mls_count,
            self_remove_pending,
            current_epoch,
            in_recovery,
            live_epoch_steward_id,
        })
    }
}

/// True iff `req` is a membership change that produces an MLS proposal
/// (vs governance kinds like emergency criteria or steward election,
/// which are consensus-only).
pub(super) fn produces_mls_action(req: &ConversationUpdateRequest) -> bool {
    matches!(
        req.payload.as_ref(),
        Some(Payload::MemberInvite(_) | Payload::RemoveMember(_))
    )
}

// ═════════════════════════════════════════════════════════════════════════════
// SELECTION
// ═════════════════════════════════════════════════════════════════════════════

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

#[cfg(test)]
mod tests {
    use super::*;

    fn sort_by_priority(
        candidates: &mut [BufferedCommitCandidate],
        epoch_steward_id: Option<&[u8]>,
    ) {
        candidates.sort_by(|a, b| compare_candidate_priority(a, b, epoch_steward_id));
    }

    fn hash(byte: u8) -> CommitHash {
        CommitHash([byte; 32])
    }

    fn make_candidate(
        steward_identity: Vec<u8>,
        actions_count: usize,
        commit_hash: CommitHash,
    ) -> BufferedCommitCandidate {
        BufferedCommitCandidate {
            candidate_msg: CommitCandidate {
                conversation_id: b"test-conversation".to_vec(),
                mls_proposals: vec![vec![0xFF; 10]; actions_count],
                commit_message: commit_hash.as_bytes().to_vec(),
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
            make_candidate(epoch_id.clone(), 3, hash(0xAA)),
            make_candidate(other_id.clone(), 5, hash(0xBB)),
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
            make_candidate(other_id.clone(), 3, hash(0xAA)),
            make_candidate(epoch_id.clone(), 3, hash(0xBB)),
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
            make_candidate(other_a.clone(), 3, hash(0xAA)),
            make_candidate(other_b.clone(), 3, hash(0xBB)),
        ];

        sort_by_priority(&mut candidates, Some(&epoch_id));

        assert_eq!(candidates[0].candidate_msg.steward_identity, other_b);
    }

    /// Final tiebreak: same identity, same action count → lowest commit hash wins.
    #[test]
    fn commit_hash_as_final_tiebreak() {
        let id = vec![0x05];

        let mut candidates = vec![
            make_candidate(id.clone(), 3, hash(0xCC)),
            make_candidate(id.clone(), 3, hash(0xAA)),
        ];

        sort_by_priority(&mut candidates, Some(&[0x01]));

        assert_eq!(candidates[0].commit_hash, hash(0xAA));
    }

    /// No steward list → tier is always 1, so the tier check is a no-op and
    /// we fall through to identity comparison.
    #[test]
    fn no_steward_list_flattens_tier_and_falls_through_to_identity() {
        let id_a = vec![0x05];
        let id_b = vec![0x03];

        let mut candidates = vec![
            make_candidate(id_a.clone(), 3, hash(0xAA)),
            make_candidate(id_b.clone(), 3, hash(0xBB)),
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
            make_candidate(other_b.clone(), 5, hash(0x11)),
            make_candidate(other_a.clone(), 3, hash(0x22)),
            make_candidate(epoch_id.clone(), 3, hash(0x44)),
        ];

        sort_by_priority(&mut candidates, Some(&epoch_id));

        assert_eq!(candidates[0].candidate_msg.steward_identity, other_b);
        assert_eq!(candidates[1].candidate_msg.steward_identity, epoch_id);
        assert_eq!(candidates[2].candidate_msg.steward_identity, other_a);
    }
}
