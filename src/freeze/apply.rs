//! Phase-3 loop: apply each candidate in priority order until one wins.
//! Per-candidate apply paths (local merge / remote stage-validate-merge),
//! MLS staging, validation, and post-commit bookkeeping live here.

use crate::{
    CommitHash, ConversationQueues, CoreError, FreezeFinalizeResult, FreezeOutcome, ProcessResult,
    ScoreEvent, ScoreOp, StewardListPlugin,
    conversation::BufferedCommitCandidate,
    freeze::round::RoundContext,
    mls_crypto::{MlsProposalOutput, MlsService, StagedCandidateResult},
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, MemberWelcome, ViolationEvidence,
        conversation_update_request::Payload,
    },
};

/// Outcome of applying one candidate. `Terminal` ends the round; `Drop`
/// skips to the next, recording its penalty when the violation carries one.
///
/// `committer` is the MLS-verified sender, never the wire claim — so a forged
/// `steward_member_id` cannot redirect the `SuccessfulCommit` reward.
enum CandidateOutcome {
    Terminal {
        outcome: FreezeOutcome,
        committer: Vec<u8>,
        committed_batch: Vec<ConversationUpdateRequest>,
    },
    Drop(Option<ScoreOp>),
}

/// Walk `sorted` best-first; the first candidate that applies wins. Rejected
/// candidates score a local penalty — no ECP (RFC §Peer Scoring allows direct
/// local scoring for observable violations).
///
/// `own_commit_discarded` enforces MLS's one-pending-commit rule: the first
/// incoming attempt wipes our own pending commit, so a later lower-priority
/// local candidate has nothing to apply.
pub(super) fn apply_in_priority_order<M: MlsService, St: StewardListPlugin>(
    conversation: &mut ConversationQueues,
    mls: &mut M,
    steward: &St,
    sorted: Vec<BufferedCommitCandidate>,
    ctx: &RoundContext,
    self_member_id: &[u8],
) -> Result<FreezeFinalizeResult, CoreError> {
    let mut score_ops: Vec<ScoreOp> = Vec::new();
    let mut own_commit_discarded = false;
    let conversation_id = conversation.name().to_owned();

    let mut remaining = sorted.into_iter();
    while let Some(chosen) = remaining.next() {
        let apply_result = if chosen.is_local_candidate {
            if own_commit_discarded {
                tracing::debug!(
                    conversation = %conversation_id,
                    "own pending commit is discarded; skipping local candidate"
                );
                continue;
            }
            apply_local_candidate(conversation, mls, chosen, ctx)?
        } else {
            if !own_commit_discarded && steward.is_steward(self_member_id) {
                // A failure here leaves an old pending commit in MLS and
                // would sabotage every subsequent staging attempt — bubble
                // the error out of the round instead of pressing on.
                mls.discard_own_commit()?;
                own_commit_discarded = true;
            }
            apply_incoming_candidate(conversation, mls, steward, chosen, ctx)?
        };

        match apply_result {
            CandidateOutcome::Terminal {
                outcome,
                committer,
                committed_batch,
            } => {
                record_winner_scores(
                    &mut score_ops,
                    &committer,
                    self_member_id,
                    ctx,
                    remaining,
                    steward,
                    &conversation_id,
                );
                return Ok(FreezeFinalizeResult {
                    outcome,
                    score_ops,
                    committed_batch,
                });
            }
            CandidateOutcome::Drop(op) => score_ops.extend(op),
        }
    }

    // No candidate applied. Drop any local pending commit that wasn't
    // merged or discarded along an incoming-wins path — leaving it
    // behind would break the next MLS encrypt.
    if !own_commit_discarded {
        mls.discard_own_commit()?;
    }
    conversation.clear_freeze_round();
    Ok(FreezeFinalizeResult {
        outcome: FreezeOutcome::NoCandidate,
        score_ops,
        committed_batch: Vec::new(),
    })
}

/// Append the scoring side-effects of a winning commit.
///
/// RFC §Commit Validation: unpicked competitors are honest under
/// Δ-synchrony (same approved set, different MLS entropy) and MUST NOT
/// be classified as misbehaviour.
fn record_winner_scores<St: StewardListPlugin>(
    score_ops: &mut Vec<ScoreOp>,
    committer: &[u8],
    self_member_id: &[u8],
    ctx: &RoundContext,
    losers: impl Iterator<Item = BufferedCommitCandidate>,
    steward: &St,
    conversation_id: &str,
) {
    score_ops.push(ScoreOp {
        member_id: committer.to_vec(),
        event: ScoreEvent::SuccessfulCommit,
    });

    // `epoch_steward_id` was resolved through `steward_eligibility`, so
    // `expected` can't be a queued-removal target — no extra guard needed.
    if let Some(expected) = ctx.epoch_steward_id.as_deref()
        && expected != committer
        && expected != self_member_id
    {
        score_ops.push(ScoreOp {
            member_id: expected.to_vec(),
            event: ScoreEvent::CensorshipInactivity,
        });
    }

    for loser in losers {
        let claimed = loser.candidate_msg.steward_member_id;
        if steward.is_steward(&claimed) {
            score_ops.push(ScoreOp {
                member_id: claimed,
                event: ScoreEvent::HonestCommitAttempt,
            });
        } else {
            tracing::debug!(
                conversation = %conversation_id,
                "dropping HonestCommitAttempt: claimed user not on steward list"
            );
        }
    }
}

// ─────────────────────────── Application Paths ───────────────────────────

/// Merge our own commit and surface the welcome we held back until merge.
/// Validation happened at commit-creation time, so no re-staging is needed.
fn apply_local_candidate<M: MlsService>(
    conversation: &mut ConversationQueues,
    mls: &mut M,
    chosen: BufferedCommitCandidate,
    ctx: &RoundContext,
) -> Result<CandidateOutcome, CoreError> {
    mls.merge_own_commit()?;

    let committed_batch =
        finalize_committed_batch(conversation, chosen.commit_hash, mls.current_epoch()?);

    // Build the welcome artifact only after merge.
    let joiner_identities = chosen.joiner_identities;
    let welcome = chosen.welcome_bytes.map(|welcome_bytes| MemberWelcome {
        welcome_bytes,
        conversation_sync_bytes: Vec::new(),
        joiner_identities,
    });

    let result = if ctx.self_remove_pending {
        ProcessResult::LeaveConversation
    } else {
        ProcessResult::ConversationUpdated
    };
    Ok(CandidateOutcome::Terminal {
        outcome: FreezeOutcome::Applied { result, welcome },
        committer: chosen.candidate_msg.steward_member_id.clone(),
        committed_batch,
    })
}

/// Stage, validate, and merge a candidate authored by another steward.
/// `Drop` (with penalty) on any failed check; `Terminal(Applied)` on merge.
///
/// Caller must have discarded any own pending commit first — MLS allows
/// only one per conversation.
fn apply_incoming_candidate<M: MlsService, St: StewardListPlugin>(
    conversation: &mut ConversationQueues,
    mls: &mut M,
    steward: &St,
    chosen: BufferedCommitCandidate,
    ctx: &RoundContext,
) -> Result<CandidateOutcome, CoreError> {
    let conversation_id = conversation.name().to_owned();

    let (commit_sender, self_removed, commit_actions) =
        match stage_candidate(mls, &conversation_id, &chosen.candidate_msg, ctx)? {
            StagingOutcome::Staged {
                commit_sender,
                self_removed,
                commit_actions,
            } => (commit_sender, self_removed, commit_actions),
            StagingOutcome::Abort => {
                // Wire-valid but MLS-invalid — penalize the author; the
                // loop will try the next candidate.
                mls.discard_staged_commit()?;
                return Ok(CandidateOutcome::Drop(Some(ScoreOp {
                    member_id: chosen.candidate_msg.steward_member_id,
                    event: ScoreEvent::MisbehavingCommit,
                })));
            }
            StagingOutcome::Violation(v) => {
                mls.discard_staged_commit()?;
                return Ok(CandidateOutcome::Drop(v.target_score_op()));
            }
        };

    // Commit sender must be on the steward list (RFC §"Commit validation service").
    if let Some(violation) =
        check_commit_sender_authorized(conversation, steward, &commit_sender, ctx)
    {
        mls.discard_staged_commit()?;
        return Ok(CandidateOutcome::Drop(violation.target_score_op()));
    }

    // MLS actions must match the set we voted to approve.
    if let Some(violation) =
        validate_commit_candidate(conversation, &commit_sender, &commit_actions, ctx)?
    {
        mls.discard_staged_commit()?;
        return Ok(CandidateOutcome::Drop(violation.target_score_op()));
    }

    mls.merge_staged_commit()?;
    let committed_batch =
        finalize_committed_batch(conversation, chosen.commit_hash, mls.current_epoch()?);

    // Remote candidates never carry welcome bytes — only the author sends those.
    let result = if self_removed {
        ProcessResult::LeaveConversation
    } else {
        ProcessResult::ConversationUpdated
    };
    Ok(CandidateOutcome::Terminal {
        outcome: FreezeOutcome::Applied {
            result,
            welcome: None,
        },
        committer: commit_sender,
        committed_batch,
    })
}

// ─────────────────────────── MLS Staging ───────────────────────────

enum StagingOutcome {
    /// Staged cleanly; senders are internally consistent.
    Staged {
        commit_sender: Vec<u8>,
        self_removed: bool,
        commit_actions: Vec<MlsProposalOutput>,
    },
    /// MLS rejected a piece (bad wire, protocol error).
    Abort,
    /// Staged, but a sender-consistency invariant failed.
    Violation(ViolationEvidence),
}

/// Stage proposals and commit, cross-checking sender consistency along the way.
///
/// Leaves MLS in the staged state on `Staged`; the caller must clean up
/// via `discard_staged_commit` for `Abort` / `Violation`.
fn stage_candidate<M>(
    mls: &mut M,
    conversation_id: &str,
    candidate: &CommitCandidate,
    ctx: &RoundContext,
) -> Result<StagingOutcome, CoreError>
where
    M: MlsService,
{
    let staged_result = mls
        .stage_remote_commit(&candidate.mls_proposals, &candidate.commit_message)
        .inspect_err(|e| {
            tracing::debug!(conversation = conversation_id, error = %e, "candidate failed to stage");
        });

    let (commit_sender, self_removed, commit_actions) = match staged_result {
        Ok(StagedCandidateResult::Staged {
            commit_sender,
            self_removed,
            actions,
        }) => (commit_sender, self_removed, actions),
        Ok(StagedCandidateResult::BundleSenderMismatch { commit_sender }) => {
            tracing::warn!(
                conversation = conversation_id,
                "violation: bundled proposals don't match the commit sender"
            );
            return Ok(StagingOutcome::Violation(ViolationEvidence::broken_commit(
                commit_sender,
                ctx.current_epoch,
                "commit bundles proposals not signed by the committer",
            )));
        }
        Ok(StagedCandidateResult::Aborted) | Err(_) => return Ok(StagingOutcome::Abort),
    };

    // Wire-claimed `steward_member_id` must match the MLS-verified
    // `commit_sender`; mismatch is a broken commit (RFC §Steward
    // violation list) and is attributed to the actual signer.
    if candidate.steward_member_id != commit_sender {
        tracing::warn!(
            conversation = conversation_id,
            "violation: wire steward_member_id doesn't match MLS commit_sender"
        );
        return Ok(StagingOutcome::Violation(ViolationEvidence::broken_commit(
            commit_sender,
            ctx.current_epoch,
            "commit candidate's steward_member_id doesn't match MLS commit sender",
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
///
/// Compares both sides as sorted `(kind_tag, &member_id)` projections —
/// no `MlsProposalOutput` allocation, no per-element clone.
fn validate_commit_candidate(
    conversation: &ConversationQueues,
    sender_id: &[u8],
    mls_actions: &[MlsProposalOutput],
    ctx: &RoundContext,
) -> Result<Option<ViolationEvidence>, CoreError> {
    let mut expected: Vec<(u8, &[u8])> = conversation
        .approved_proposals()
        .values()
        .filter_map(action_projection_from_request)
        .collect();
    let mut actual: Vec<(u8, &[u8])> = mls_actions.iter().map(action_projection_from_mls).collect();
    expected.sort();
    actual.sort();

    if expected == actual {
        return Ok(None);
    }

    tracing::warn!(
        conversation = conversation.name(),
        actual = ?mls_actions,
        expected = ?expected,
        "violation: MLS actions don't match voted proposals"
    );
    Ok(Some(ViolationEvidence::broken_mls_proposal(
        sender_id.to_vec(),
        ctx.current_epoch,
        format!("MLS actions {mls_actions:?} != voted {expected:?}"),
    )))
}

/// `(kind_tag, member_id)` projection of an approved voting request.
/// Returns `None` for non-MLS payloads (emergency/election).
fn action_projection_from_request(req: &ConversationUpdateRequest) -> Option<(u8, &[u8])> {
    match req.payload.as_ref()? {
        Payload::MemberInvite(im) => Some((0, &im.member_id)),
        Payload::RemoveMember(rm) => Some((1, &rm.member_id)),
        _ => None,
    }
}

/// `(kind_tag, member_id)` projection of an MLS-staged action.
fn action_projection_from_mls(action: &MlsProposalOutput) -> (u8, &[u8]) {
    match action {
        MlsProposalOutput::Add(id) => (0, id),
        MlsProposalOutput::Remove(id) => (1, id),
    }
}

/// RFC §"de-MLS Objects": any steward-list member may commit to preserve
/// liveness. Epoch-steward priority is about *selection*, not authorization.
///
/// `None` also covers "no list yet" (joiner pre-sync), "list exhausted"
/// (re-election in progress), and "Layer-3 recovery_mode active" (RFC
/// §Anti-Deadlock: any member MAY commit to restore liveness; mirrors
/// the relaxed gate in `create_commit_candidate`).
fn check_commit_sender_authorized<St: StewardListPlugin>(
    conversation: &ConversationQueues,
    steward: &St,
    commit_sender: &[u8],
    ctx: &RoundContext,
) -> Option<ViolationEvidence> {
    if ctx.in_recovery {
        return None;
    }
    steward.current_list()?;
    if steward.is_exhausted(ctx.current_epoch) {
        return None;
    }
    if steward.is_steward(commit_sender) {
        return None;
    }
    tracing::warn!(
        conversation = conversation.name(),
        "violation: commit from unauthorized sender"
    );
    Some(ViolationEvidence::broken_commit(
        commit_sender.to_vec(),
        ctx.current_epoch,
        "commit from unauthorized sender (not on the steward list)",
    ))
}

// ─────────────────────────── State Utilities ───────────────────────────

/// Apply post-commit bookkeeping and return the cleared batch (empty when
/// the commit was urgent-target-only and only the targeted entry was
/// dropped). Caller surfaces the batch through `FreezeFinalizeResult` so
/// the app layer can archive it for UI history.
fn finalize_committed_batch(
    conversation: &mut ConversationQueues,
    commit_hash: CommitHash,
    current_epoch: u64,
) -> Vec<ConversationUpdateRequest> {
    conversation.insert_committed_hash(commit_hash);
    let snapshot = if let Some(target) = conversation.take_urgent_commit_target() {
        // Urgent commit: leave the rest of the queue for the next cycle.
        conversation.drop_approved_removals_for(&target);
        Vec::new()
    } else {
        conversation.drain_approved_proposals()
    };
    // Stamp join epochs for members this commit added, so the next reconcile
    // doesn't count them toward an election they can't yet participate in.
    conversation.note_member_joins(&snapshot, current_epoch);
    conversation.clear_freeze_round();
    snapshot
}
