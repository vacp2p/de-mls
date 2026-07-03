//! Commit-round public surface: receive candidates, replay early ones, and
//! finalize the round (rank via `validate`, then apply).
//!
//! Per-candidate apply lives in the sibling [`crate::commit_round::apply`] module.

use std::error::Error as StdError;

use crate::{
    ConversationError, ConversationQueues, NoopReason, ProcessResult, ScoreOp, StewardListService,
    commit_round::{
        apply::apply_in_priority_order, context::RoundContext, validate::rank_applicable_candidates,
    },
    mls_crypto::{MlsMessageKind, MlsService},
    protos::de_mls::messages::v1::{CommitCandidate, ConversationUpdateRequest, MemberWelcome},
};
use openmls_traits::{OpenMlsProvider, storage::StorageProvider};
use sha2::{Digest, Sha256};

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

/// Ingest a commit candidate received from a peer: validate it (non-empty
/// proposals/commit, valid MLS wire kinds, non-empty `steward_member_id`,
/// non-duplicate hash) and buffer it into the commit round. No MLS state is
/// mutated. The local-candidate counterpart is `Conversation::build_local_candidate`.
pub fn receive_commit_candidate(
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

    // The proposal may not be locally approved yet — the consensus outcome can
    // still be in flight (a peer steward can reach consensus and broadcast this
    // commit before our own vote lands). Stash for replay once approval arrives
    // rather than dropping; otherwise we'd never apply the commit and would fall
    // an epoch behind. Once approved, `add` opens the round on the first candidate.
    if conversation.approved_proposals_count() == 0 {
        tracing::debug!(conversation = %conversation_id, "candidate stashed: proposal not yet approved");
        conversation.stash_early_candidate(epoch, candidate_msg, max_candidates);
        return Ok(ProcessResult::Noop(NoopReason::CandidateStashedEarly));
    }

    let steward = candidate_msg.steward_member_id.clone();
    let outcome = conversation.commit_round.add(
        candidate_msg,
        false,
        None,
        Vec::new(),
        epoch,
        max_candidates,
    );
    Ok(outcome.into_process_result(steward))
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
        receive_commit_candidate(conversation, mls, candidate)?;
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
    let candidates = conversation.commit_round.take(current_epoch);
    // No candidates buffered for this epoch (or the round is stale) — nothing to
    // rank or apply.
    if candidates.is_empty() {
        return discard_and_finish(provider, mls, Vec::new());
    }

    let ctx = RoundContext::snapshot(
        conversation,
        mls,
        steward_list,
        current_epoch,
        in_recovery,
        self_member_id,
    )?;
    let sorted = rank_applicable_candidates(candidates, &ctx, allow_subset_candidates);
    // Every candidate was filtered out (action count didn't match the voted
    // set) — nothing applicable.
    if sorted.is_empty() {
        return discard_and_finish(provider, mls, Vec::new());
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

/// No candidate applied: drop any local pending commit (otherwise the next MLS
/// operation trips on a lingering pending commit) and report a no-op carrying
/// any penalties accrued while trying candidates.
pub fn discard_and_finish<Pr>(
    provider: &Pr,
    mls: &mut MlsService,
    score_ops: Vec<ScoreOp>,
) -> Result<CommitRoundResult, ConversationError>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    mls.discard_own_commit(provider)?;
    Ok(CommitRoundResult {
        score_ops,
        ..Default::default()
    })
}
