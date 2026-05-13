//! Phase-3 loop: apply each candidate in priority order until one wins.
//! Per-candidate apply paths (local merge / remote stage-validate-merge),
//! MLS staging, validation, and post-commit bookkeeping live here.

use std::collections::HashMap;

use super::round::{FreezeFinalizeResult, FreezeOutcome, RoundContext};
use crate::{
    core::{
        Conversation, CoreError, ProcessResult, ProposalId, ScoreEvent, ScoreOp, StewardListPlugin,
        conversation::BufferedCommitCandidate, proposal_framing::build_invitation_packet,
    },
    mls_crypto::{MlsProposalOutput, MlsService, StagedCandidateResult},
    protos::de_mls::messages::v1::{CommitCandidate, ConversationUpdateRequest, ViolationEvidence},
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
        committed_batch: HashMap<ProposalId, ConversationUpdateRequest>,
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
pub(super) fn apply_in_priority_order<M: MlsService>(
    conversation: &mut Conversation,
    mls: &M,
    steward: &dyn StewardListPlugin,
    in_recovery: bool,
    sorted: Vec<BufferedCommitCandidate>,
    ctx: &RoundContext,
    app_id: &[u8],
) -> Result<FreezeFinalizeResult, CoreError> {
    let mut score_ops: Vec<ScoreOp> = Vec::new();
    let mut own_commit_discarded = false;
    let conversation_name = conversation.name().to_owned();

    let mut remaining = sorted.into_iter();
    while let Some(chosen) = remaining.next() {
        let apply_result = if chosen.is_local_candidate {
            if own_commit_discarded {
                tracing::debug!(
                    conversation = %conversation_name,
                    "own pending commit is discarded; skipping local candidate"
                );
                continue;
            }
            apply_local_candidate(conversation, mls, chosen, ctx.self_remove_pending, app_id)?
        } else {
            if !own_commit_discarded && steward.is_steward(&ctx.self_identity) {
                // A failure here leaves an old pending commit in MLS and
                // would sabotage every subsequent staging attempt — bubble
                // the error out of the round instead of pressing on.
                mls.discard_own_commit()?;
                own_commit_discarded = true;
            }
            apply_incoming_candidate(
                conversation,
                mls,
                steward,
                in_recovery,
                chosen,
                &ctx.mls_actions,
                ctx.current_epoch,
            )?
        };

        match apply_result {
            CandidateOutcome::Terminal {
                outcome,
                committer,
                committed_batch,
            } => {
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
                    let on_list = steward.is_steward(&claimed);
                    if on_list {
                        score_ops.push(ScoreOp {
                            member_id: claimed,
                            event: ScoreEvent::HonestCommitAttempt,
                        });
                    } else {
                        tracing::debug!(
                            conversation = %conversation_name,
                            "dropping HonestCommitAttempt: claimed identity not on steward list"
                        );
                    }
                }
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
        committed_batch: HashMap::new(),
    })
}

// ─────────────────────────── Application Paths ───────────────────────────

/// Merge our own commit and send the deferred welcomes we held back.
/// Validation happened at commit-creation time, so no re-staging is needed.
/// Always returns `Terminal(Applied)` on a clean merge.
fn apply_local_candidate<M: MlsService>(
    conversation: &mut Conversation,
    mls: &M,
    chosen: BufferedCommitCandidate,
    self_removed: bool,
    app_id: &[u8],
) -> Result<CandidateOutcome, CoreError> {
    // Local candidate: we wrote the message, so the wire-claimed identity
    // is our own and is trusted by definition.
    let committer = chosen.candidate_msg.steward_identity.clone();

    mls.merge_own_commit()?;

    // Welcomes go out only after our merge — joiners must not race ahead of the steward's epoch.
    let outbound = chosen
        .welcome_bytes
        .map(|bytes| build_invitation_packet(bytes, conversation.name(), app_id));

    let committed_batch = record_applied_commit(conversation, chosen.commit_hash);

    let result = if self_removed {
        ProcessResult::LeaveConversation
    } else {
        ProcessResult::ConversationUpdated
    };
    Ok(CandidateOutcome::Terminal {
        outcome: FreezeOutcome::Applied {
            result: Box::new(result),
            outbound,
        },
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
fn apply_incoming_candidate<M: MlsService>(
    conversation: &mut Conversation,
    mls: &M,
    steward: &dyn StewardListPlugin,
    in_recovery: bool,
    chosen: BufferedCommitCandidate,
    expected_actions: &[MlsProposalOutput],
    current_epoch: u64,
) -> Result<CandidateOutcome, CoreError> {
    let conversation_name = conversation.name().to_owned();

    let (commit_sender, self_removed, commit_actions) = match stage_candidate(
        mls,
        &conversation_name,
        &chosen.candidate_msg,
        current_epoch,
    )? {
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
    if let Some(violation) = check_commit_sender_authorized(
        conversation,
        steward,
        in_recovery,
        &commit_sender,
        current_epoch,
    ) {
        mls.discard_staged_commit()?;
        return Ok(CandidateOutcome::Drop(
            violation
                .target_score_op()
                .expect("locally-built violation always has a target-side score"),
        ));
    }

    // MLS actions must match the set we voted to approve.
    if let Some(violation) = validate_commit_candidate(
        conversation,
        expected_actions,
        &commit_sender,
        &commit_actions,
        current_epoch,
    )? {
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
            result: Box::new(result),
            outbound: None,
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
/// Leaves MLS in the staged state on `Staged`; the caller must clean up via
/// `discard_and_*` for `Abort` / `Violation`.
fn stage_candidate<M>(
    mls: &M,
    conversation_name: &str,
    candidate: &CommitCandidate,
    current_epoch: u64,
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
            tracing::debug!(conversation = conversation_name, error = %e, "candidate failed to stage");
        })
    else {
        return Ok(StagingOutcome::Abort);
    };

    // Wire-claimed `steward_identity` must match the MLS-verified
    // `commit_sender`; mismatch is a broken commit (RFC §Steward
    // violation list) and is attributed to the actual signer.
    if candidate.steward_identity != commit_sender {
        tracing::warn!(
            conversation = conversation_name,
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
            conversation = conversation_name,
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
    conversation: &Conversation,
    expected_actions: &[MlsProposalOutput],
    sender_id: &[u8],
    mls_actions: &[MlsProposalOutput],
    epoch: u64,
) -> Result<Option<ViolationEvidence>, CoreError> {
    let conversation_name = conversation.name();

    let mut expected_actions = expected_actions.to_vec();
    let mut actual_actions = mls_actions.to_vec();

    expected_actions.sort();
    actual_actions.sort();

    if actual_actions != expected_actions {
        tracing::warn!(
            conversation = conversation_name,
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

/// RFC §"de-MLS Objects": any steward-list member may commit to preserve
/// liveness. Epoch-steward priority is about *selection*, not authorization.
///
/// `None` also covers "no list yet" (joiner pre-sync), "list exhausted"
/// (re-election in progress), and "Layer-3 recovery_mode active" (RFC
/// §Anti-Deadlock: any member MAY commit to restore liveness; mirrors
/// the relaxed gate in `create_commit_candidate`).
fn check_commit_sender_authorized(
    conversation: &Conversation,
    steward: &dyn StewardListPlugin,
    in_recovery: bool,
    commit_sender: &[u8],
    epoch: u64,
) -> Option<ViolationEvidence> {
    if in_recovery {
        return None;
    }
    steward.current_list()?;
    if steward.is_exhausted(epoch) {
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
        epoch,
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
    commit_hash: Vec<u8>,
) -> HashMap<ProposalId, ConversationUpdateRequest> {
    conversation.record_committed_batch(commit_hash);
    let snapshot = if let Some(target) = conversation.take_urgent_commit_target() {
        // Urgent commit: leave the rest of the queue for the next cycle.
        conversation.drop_approved_removals_for(&target);
        HashMap::new()
    } else {
        conversation.clear_approved_proposals()
    };
    conversation.clear_freeze_round();
    snapshot
}
