//! Phase-3 loop: apply each candidate in priority order until one wins.
//! Per-candidate apply paths (local merge / remote stage-validate-merge),
//! MLS staging, validation, and post-commit bookkeeping live here.

use crate::{
    core::{
        CommitHash, Conversation, CoreError, FreezeFinalizeResult, FreezeOutcome, ProcessResult,
        ScoreEvent, ScoreOp, StewardListPlugin, conversation::BufferedCommitCandidate,
        freeze::round::RoundContext,
    },
    mls_crypto::{MlsProposalOutput, MlsService, StagedCandidateResult},
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, MemberWelcome, ViolationEvidence,
        conversation_update_request::Payload,
    },
};

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
        committed_batch: Vec<ConversationUpdateRequest>,
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
pub(super) fn apply_in_priority_order<M: MlsService, St: StewardListPlugin>(
    conversation: &mut Conversation,
    mls: &mut M,
    steward: &St,
    sorted: Vec<BufferedCommitCandidate>,
    ctx: &RoundContext,
    self_identity: &[u8],
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
            if !own_commit_discarded && steward.is_steward(self_identity) {
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
                    self_identity,
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
            CandidateOutcome::Drop(op) => score_ops.push(op),
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

/// Append the scoring side-effects of a winning commit:
/// `SuccessfulCommit` on the committer, `CensorshipInactivity` on the
/// absent named steward (if a backup won and we aren't that steward),
/// and `HonestCommitAttempt` on each unpicked competitor whose
/// wire-claimed identity sits on the steward list.
///
/// RFC §Commit Validation: unpicked competitors are honest under
/// Δ-synchrony (same approved set, different MLS entropy) and MUST NOT
/// be classified as misbehaviour. The steward-list check rejects forged
/// credit claims from non-stewards.
fn record_winner_scores<St: StewardListPlugin>(
    score_ops: &mut Vec<ScoreOp>,
    committer: &[u8],
    self_identity: &[u8],
    ctx: &RoundContext,
    losers: impl Iterator<Item = BufferedCommitCandidate>,
    steward: &St,
    conversation_id: &str,
) {
    score_ops.push(ScoreOp {
        member_id: committer.to_vec(),
        event: ScoreEvent::SuccessfulCommit,
    });

    // The walk in `live_epoch_steward_id` already filters out
    // queued-removal targets, so a draining named steward never appears
    // here as `expected`.
    if let Some(expected) = ctx.live_epoch_steward_id.as_deref()
        && expected != committer
        && expected != self_identity
    {
        score_ops.push(ScoreOp {
            member_id: expected.to_vec(),
            event: ScoreEvent::CensorshipInactivity,
        });
    }

    for loser in losers {
        let claimed = loser.candidate_msg.steward_identity;
        if steward.is_steward(&claimed) {
            score_ops.push(ScoreOp {
                member_id: claimed,
                event: ScoreEvent::HonestCommitAttempt,
            });
        } else {
            tracing::debug!(
                conversation = %conversation_id,
                "dropping HonestCommitAttempt: claimed identity not on steward list"
            );
        }
    }
}

// ─────────────────────────── Application Paths ───────────────────────────

/// Merge our own commit and send the deferred welcomes we held back.
/// Validation happened at commit-creation time, so no re-staging is needed.
/// Always returns `Terminal(Applied)` on a clean merge.
fn apply_local_candidate<M: MlsService>(
    conversation: &mut Conversation,
    mls: &mut M,
    chosen: BufferedCommitCandidate,
    ctx: &RoundContext,
) -> Result<CandidateOutcome, CoreError> {
    // Local candidate: we wrote the message, so the wire-claimed identity
    // is our own and is trusted by definition.
    let committer = chosen.candidate_msg.steward_identity.clone();

    mls.merge_own_commit()?;

    let committed_batch = record_applied_commit(conversation, chosen.commit_hash);

    // Build the welcome artifact only after merge.
    // `conversation_sync_bytes` is populated at the app layer (which
    // owns scoring + steward-list snapshot state) before the
    // [`crate::core::SessionEvent::WelcomeReady`] event is emitted.
    let welcome = chosen.welcome_bytes.map(|welcome_bytes| MemberWelcome {
        welcome_bytes,
        conversation_sync_bytes: Vec::new(),
    });

    let result = if ctx.self_remove_pending {
        ProcessResult::LeaveConversation
    } else {
        ProcessResult::ConversationUpdated
    };
    Ok(CandidateOutcome::Terminal {
        outcome: FreezeOutcome::Applied { result, welcome },
        committer,
        committed_batch,
    })
}

/// Stage, validate, and merge a candidate authored by another steward.
/// Returns `Terminal(Applied)` on a clean merge, or `Drop` with a score
/// penalty if any check fails (MLS staging, sender mismatch,
/// unauthorized sender, action-set mismatch).
///
/// Caller must have discarded any own pending commit first — MLS allows
/// only one per conversation at a time.
fn apply_incoming_candidate<M: MlsService, St: StewardListPlugin>(
    conversation: &mut Conversation,
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
                return Ok(CandidateOutcome::Drop(ScoreOp {
                    member_id: chosen.candidate_msg.steward_identity,
                    event: ScoreEvent::MisbehavingCommit,
                }));
            }
            StagingOutcome::Violation(v) => {
                mls.discard_staged_commit()?;
                return Ok(CandidateOutcome::Drop(
                    v.target_score_op()
                        .expect("staged-violation always has a target-side score"),
                ));
            }
        };

    // Commit sender must be on the steward list (RFC §"Commit validation service").
    if let Some(violation) =
        check_commit_sender_authorized(conversation, steward, &commit_sender, ctx)
    {
        mls.discard_staged_commit()?;
        return Ok(CandidateOutcome::Drop(
            violation
                .target_score_op()
                .expect("locally-built violation always has a target-side score"),
        ));
    }

    // MLS actions must match the set we voted to approve.
    if let Some(violation) =
        validate_commit_candidate(conversation, &commit_sender, &commit_actions, ctx)?
    {
        mls.discard_staged_commit()?;
        return Ok(CandidateOutcome::Drop(
            violation
                .target_score_op()
                .expect("locally-built violation always has a target-side score"),
        ));
    }

    mls.merge_staged_commit()?;
    let committed_batch = record_applied_commit(conversation, chosen.commit_hash);

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
    let Ok(StagedCandidateResult::Staged {
        commit_sender,
        proposal_senders,
        self_removed,
        actions: commit_actions,
    }) = mls
        .stage_remote_commit(&candidate.mls_proposals, &candidate.commit_message)
        .inspect_err(|e| {
            tracing::debug!(conversation = conversation_id, error = %e, "candidate failed to stage");
        })
    else {
        return Ok(StagingOutcome::Abort);
    };

    // Wire-claimed `steward_identity` must match the MLS-verified
    // `commit_sender`; mismatch is a broken commit (RFC §Steward
    // violation list) and is attributed to the actual signer.
    if candidate.steward_identity != commit_sender {
        tracing::warn!(
            conversation = conversation_id,
            "violation: wire steward_identity doesn't match MLS commit_sender"
        );
        return Ok(StagingOutcome::Violation(ViolationEvidence::broken_commit(
            commit_sender,
            ctx.current_epoch,
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
            conversation = conversation_id,
            "violation: bundled proposals don't match the commit sender"
        );
        return Ok(StagingOutcome::Violation(ViolationEvidence::broken_commit(
            commit_sender,
            ctx.current_epoch,
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
///
/// Compares both sides as sorted `(kind_tag, &identity)` projections —
/// no `MlsProposalOutput` allocation, no per-element clone.
fn validate_commit_candidate(
    conversation: &Conversation,
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

/// `(kind_tag, identity)` projection of an approved voting request.
/// Returns `None` for non-MLS payloads (emergency/election).
fn action_projection_from_request(req: &ConversationUpdateRequest) -> Option<(u8, &[u8])> {
    match req.payload.as_ref()? {
        Payload::MemberInvite(im) => Some((0, &im.identity)),
        Payload::RemoveMember(rm) => Some((1, &rm.identity)),
        _ => None,
    }
}

/// `(kind_tag, identity)` projection of an MLS-staged action. `Other`
/// gets a distinct tag so it never matches voted Add/Remove.
fn action_projection_from_mls(action: &MlsProposalOutput) -> (u8, &[u8]) {
    match action {
        MlsProposalOutput::Add(id) => (0, id),
        MlsProposalOutput::Remove(id) => (1, id),
        MlsProposalOutput::Other(_) => (2, b""),
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
    conversation: &Conversation,
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
fn record_applied_commit(
    conversation: &mut Conversation,
    commit_hash: CommitHash,
) -> Vec<ConversationUpdateRequest> {
    conversation.record_committed_batch(commit_hash);
    let snapshot = if let Some(target) = conversation.take_urgent_commit_target() {
        // Urgent commit: leave the rest of the queue for the next cycle.
        conversation.drop_approved_removals_for(&target);
        Vec::new()
    } else {
        conversation.clear_approved_proposals()
    };
    conversation.clear_freeze_round();
    snapshot
}
