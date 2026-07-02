//! Commit-round public surface, per-round setup, and priority selection.
//!
//! Per-candidate apply lives in the sibling [`crate::commit_round::apply`] module.

use std::error::Error as StdError;

use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::{
    CommitBufferOutcome, ConversationError, ConversationQueues, NoopReason, ProcessResult, ScoreOp,
    StewardListService,
    commit_round::{apply::apply_in_priority_order, context::RoundContext},
    conversation::BufferedCommitCandidate,
    mls_crypto::{MlsMessageKind, MlsService},
    protos::de_mls::messages::v1::{CommitCandidate, ConversationUpdateRequest, MemberWelcome},
};

// ═════════════════════════════════════════════════════════════════════════════
// PUBLIC API
// ═════════════════════════════════════════════════════════════════════════════

/// What [`finalize_commit_round`] hands back to the caller.
#[derive(Debug, Clone, Default)]
pub struct CommitRoundResult {
    pub outcome: CommitRoundOutcome,
    pub score_ops: Vec<ScoreOp>,
    /// Just-committed approvals in FIFO order. Empty on no-op or when
    /// an urgent-target commit drops only its entry.
    pub committed_batch: Vec<ConversationUpdateRequest>,
}

#[derive(Debug, Clone, Default)]
pub enum CommitRoundOutcome {
    Applied {
        result: ProcessResult,
        /// Welcome artifact when our own commit added members.
        /// Surfaced to integrators via
        /// [`crate::ConversationEvent::WelcomeReady`].
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

/// Canonical commit hash used for dedup of buffered/committed candidates.
pub fn compute_commit_hash(commit_message: &[u8]) -> CommitHash {
    let mut hasher = Sha256::new();
    hasher.update(commit_message);
    CommitHash(hasher.finalize().into())
}

/// Buffer a remote commit candidate. Enforces non-empty proposals/commit,
/// valid MLS wire kinds, non-empty `steward_member_id`, and non-duplicate
/// hash. No MLS state is mutated.
pub fn buffer_commit_candidate(
    conversation: &mut ConversationQueues,
    mls: &MlsService,
    candidate_msg: CommitCandidate,
) -> Result<ProcessResult, ConversationError> {
    let conversation_id = conversation.name().to_owned();

    // Validate fully before touching commit-round state: a dup/malformed
    // candidate must not open a round only to return Noop.
    let commit_hash = compute_commit_hash(&candidate_msg.commit_message);
    if conversation.has_committed_hash(&commit_hash) {
        tracing::debug!(conversation = %conversation_id, "candidate ignored: already committed");
        return Ok(ProcessResult::Noop(NoopReason::AlreadyCommitted));
    }

    if candidate_msg.mls_proposals.is_empty() || candidate_msg.commit_message.is_empty() {
        tracing::debug!(conversation = %conversation_id, "candidate ignored: empty proposals or commit");
        return Ok(ProcessResult::Noop(NoopReason::EmptyCandidatePayload));
    }

    if candidate_msg.steward_member_id.is_empty() {
        tracing::debug!(conversation = %conversation_id, "candidate ignored: empty steward_member_id");
        return Ok(ProcessResult::Noop(NoopReason::EmptyStewardMemberId));
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

    let epoch = mls.current_epoch()?;
    let max_candidates = mls.members()?.len();

    // Valid candidate — auto-start a round if there are approved proposals.
    if !conversation.has_commit_round() {
        if conversation.approved_proposals_count() == 0 {
            // The proposal isn't locally approved yet — the consensus outcome
            // is still in flight (a peer steward can reach consensus and
            // broadcast this commit before we apply our own vote). Stash for
            // replay once approval lands rather than dropping; otherwise we'd
            // never apply the commit and would fall an epoch behind.
            tracing::debug!(conversation = %conversation_id, "candidate stashed: proposal not yet approved");
            conversation.stash_early_candidate(epoch, candidate_msg, max_candidates);
            return Ok(ProcessResult::Noop(NoopReason::CandidateStashedEarly));
        }
        conversation.start_commit_round(epoch);
    }

    let steward = candidate_msg.steward_member_id.clone();
    let outcome = conversation.add_commit_candidate(
        BufferedCommitCandidate {
            candidate_msg,
            commit_hash,
            is_local_candidate: false,
            welcome_bytes: None,
            joiner_identities: Vec::new(),
        },
        epoch,
        max_candidates,
    );
    match outcome {
        CommitBufferOutcome::Buffered => {
            info!(
                conversation = %conversation_id,
                epoch,
                total_candidates = conversation.commit_candidate_count(),
                "remote candidate buffered"
            );
            Ok(ProcessResult::CommitCandidateReceived {
                steward_id: steward,
            })
        }
        // Legitimate runtime states, not errors — drop quietly.
        CommitBufferOutcome::DuplicateHash => {
            Ok(ProcessResult::Noop(NoopReason::DuplicateBufferedHash))
        }
        CommitBufferOutcome::CapReached => {
            tracing::debug!(conversation = %conversation_id, "candidate ignored: buffer full");
            Ok(ProcessResult::Noop(NoopReason::CandidateBufferFull))
        }
    }
}

/// Re-buffer any commit candidates stashed before their proposal was locally
/// approved. Call after a consensus outcome populates the approved queue: a
/// candidate that lost the race against its own approval is now buffered into
/// the commit round and applied normally.
pub fn replay_early_candidates(
    conversation: &mut ConversationQueues,
    mls: &MlsService,
) -> Result<(), ConversationError> {
    let epoch = mls.current_epoch()?;
    for candidate in conversation.take_early_candidates(epoch) {
        buffer_commit_candidate(conversation, mls, candidate)?;
    }
    Ok(())
}

/// Snapshot round state, rank the buffered candidates by RFC priority, and
/// apply best-first — falling back to the next when MLS staging rejects one.
pub fn finalize_commit_round<Pr>(
    provider: &Pr,
    conversation: &mut ConversationQueues,
    mls: &mut MlsService,
    steward_list: &StewardListService,
    in_recovery: bool,
    allow_subset_candidates: bool,
    self_member_id: &[u8],
) -> Result<CommitRoundResult, ConversationError>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    let current_epoch = mls.current_epoch()?;
    let candidates = conversation
        .take_round_candidates(current_epoch)
        .unwrap_or_default();

    let ctx = RoundContext::snapshot(
        conversation,
        mls,
        steward_list,
        current_epoch,
        in_recovery,
        self_member_id,
    )?;
    let sorted = rank_applicable_candidates(candidates, &ctx, allow_subset_candidates);

    // Nothing applicable — empty round, stale epoch, or every candidate filtered
    // out. Drop any local pending commit and report a no-op.
    if sorted.is_empty() {
        mls.discard_own_commit(provider)?;
        return Ok(CommitRoundResult::default());
    }

    apply_in_priority_order(
        provider,
        conversation,
        mls,
        steward_list,
        sorted,
        &ctx,
        self_member_id,
    )
}

// ═════════════════════════════════════════════════════════════════════════════
// SELECTION
// ═════════════════════════════════════════════════════════════════════════════

/// Filter candidates whose action count matches the voted set, then sort
/// survivors by the RFC priority comparator (best-first).
///
/// Tiering uses the *live* epoch steward (eligibility-filtered in
/// `RoundContext::snapshot`), so a draining or already-removed nominal
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
    sorted.sort_by(|a, b| compare_candidate_priority(a, b, ctx.epoch_steward_id.as_deref()));
    sorted
}

/// RFC §"Commit validation service" selection priority, in order:
///   1. largest proposal count,
///   2. epoch steward before anyone else,
///   3. lexicographically smallest `steward_member_id`,
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
            Some(es) if c.candidate_msg.steward_member_id == es => 0,
            _ => 1,
        }
    };
    let tier_cmp = tier(a).cmp(&tier(b));
    if tier_cmp != std::cmp::Ordering::Equal {
        return tier_cmp;
    }

    let id_cmp = a
        .candidate_msg
        .steward_member_id
        .cmp(&b.candidate_msg.steward_member_id);
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
        steward_member_id: Vec<u8>,
        actions_count: usize,
        commit_hash: CommitHash,
    ) -> BufferedCommitCandidate {
        BufferedCommitCandidate {
            candidate_msg: CommitCandidate {
                conversation_id: b"test-conversation".to_vec(),
                mls_proposals: vec![vec![0xFF; 10]; actions_count],
                // commit_message is irrelevant to priority ordering (which
                // keys off mls_proposals/steward_member_id/commit_hash).
                commit_message: vec![0u8; 32],
                steward_member_id,
            },
            commit_hash,
            is_local_candidate: false,
            welcome_bytes: None,
            joiner_identities: Vec::new(),
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

        assert_eq!(candidates[0].candidate_msg.steward_member_id, other_id);
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

        assert_eq!(candidates[0].candidate_msg.steward_member_id, epoch_id);
    }

    /// Third criterion: equal tier → lexicographically smallest member_id wins.
    #[test]
    fn lexicographic_member_id_tiebreak_when_tier_equal() {
        let epoch_id = vec![0x01];
        let other_a = vec![0x05];
        let other_b = vec![0x03];

        let mut candidates = vec![
            make_candidate(other_a.clone(), 3, hash(0xAA)),
            make_candidate(other_b.clone(), 3, hash(0xBB)),
        ];

        sort_by_priority(&mut candidates, Some(&epoch_id));

        assert_eq!(candidates[0].candidate_msg.steward_member_id, other_b);
    }

    /// Final tiebreak: same member_id, same action count → lowest commit hash wins.
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
    /// we fall through to member_id comparison.
    #[test]
    fn no_steward_list_flattens_tier_and_falls_through_to_member_id() {
        let id_a = vec![0x05];
        let id_b = vec![0x03];

        let mut candidates = vec![
            make_candidate(id_a.clone(), 3, hash(0xAA)),
            make_candidate(id_b.clone(), 3, hash(0xBB)),
        ];

        sort_by_priority(&mut candidates, None);

        assert_eq!(candidates[0].candidate_msg.steward_member_id, id_b);
    }

    /// Integration: three candidates mixing all criteria in one ordering.
    #[test]
    fn full_priority_order_actions_first_then_tier_then_member_id() {
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

        assert_eq!(candidates[0].candidate_msg.steward_member_id, other_b);
        assert_eq!(candidates[1].candidate_msg.steward_member_id, epoch_id);
        assert_eq!(candidates[2].candidate_msg.steward_member_id, other_a);
    }
}
