//! The apply loop: try the ranked candidates best-first until one merges, and
//! its two apply paths — local merge, and remote merge once the
//! [`crate::commit_round::validate`] service clears the candidate. Post-commit
//! bookkeeping and winner/loser scoring live here too.

use std::error::Error as StdError;

use openmls_traits::{OpenMlsProvider, storage::StorageProvider};

use crate::{
    BufferedCommitCandidate, CommitHash, CommitRoundOutcome, CommitRoundResult, ConversationError,
    ConversationQueues, ProcessResult,
    ScoreEvent::{self, MisbehavingCommit},
    ScoreOp, StewardListService,
    commit_round::{
        context::RoundContext,
        round::discard_and_finish,
        validate::{
            StagingOutcome, check_commit_sender_authorized, stage_candidate,
            validate_commit_candidate,
        },
    },
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::MemberWelcome,
};

/// Outcome of applying one candidate. `Terminal` ends the round; `Drop`
/// skips to the next, recording its penalty when the violation carries one.
///
/// On the remote path `committer` is the MLS-verified sender, not the forgeable
/// wire `steward_member_id`, so a forged id can't redirect the `SuccessfulCommit`
/// reward; our own candidate uses our own id.
enum CandidateOutcome {
    Terminal {
        outcome: CommitRoundOutcome,
        committer: Vec<u8>,
    },
    Drop(Option<ScoreOp>),
}

/// Walk `sorted` best-first; the first candidate that applies wins.
/// Rejected candidates score a local penalty
/// (RFC §Peer Scoring allows direct local scoring for observable violations).
pub fn apply_in_priority_order<Pr>(
    provider: &Pr,
    conversation: &mut ConversationQueues,
    mls: &mut MlsService,
    steward_list: &StewardListService,
    sorted: Vec<BufferedCommitCandidate>,
    ctx: &RoundContext,
    self_member_id: &[u8],
) -> Result<CommitRoundResult, ConversationError>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    let mut score_ops: Vec<ScoreOp> = Vec::new();

    let mut remaining = sorted.into_iter();
    while let Some(chosen) = remaining.next() {
        let apply_result = if chosen.is_local_candidate {
            apply_local_candidate(provider, conversation, mls, chosen, ctx)?
        } else {
            apply_incoming_candidate(provider, conversation, mls, steward_list, chosen, ctx)?
        };

        match apply_result {
            CandidateOutcome::Terminal { outcome, committer } => {
                record_winner_scores(
                    &mut score_ops,
                    &committer,
                    self_member_id,
                    ctx,
                    remaining,
                    steward_list,
                );
                return Ok(CommitRoundResult { outcome, score_ops });
            }
            CandidateOutcome::Drop(op) => score_ops.extend(op),
        }
    }

    // Defensive cleanup — normally a no-op. A local candidate always wins once
    // reached, so reaching here means none was in the round: either we built no
    // commit, or we built one that wasn't buffered (per-round cap / duplicate
    // hash). In that last case a stray pending commit lingers in MLS and would
    // block the next operation (only one pending commit allowed), so clear it.
    discard_and_finish(provider, mls, score_ops)
}

/// Append the scoring side-effects of a winning commit.
///
/// RFC §Commit Validation: unpicked competitors are honest under
/// Δ-synchrony (same approved set, different MLS entropy) and MUST NOT
/// be classified as misbehaviour.
fn record_winner_scores(
    score_ops: &mut Vec<ScoreOp>,
    committer: &[u8],
    self_member_id: &[u8],
    ctx: &RoundContext,
    losers: impl Iterator<Item = BufferedCommitCandidate>,
    steward_list: &StewardListService,
) {
    score_ops.push(ScoreOp {
        member_id: committer.to_vec(),
        event: ScoreEvent::SuccessfulCommit,
    });

    // `epoch_steward_id` is eligibility-filtered in `RoundContext::snapshot`, so
    // an absent expected steward is genuinely at fault. Penalise it — but never
    // ourselves, and not the member who actually committed.
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
        if steward_list.is_steward(&claimed) {
            score_ops.push(ScoreOp {
                member_id: claimed,
                event: ScoreEvent::HonestCommitAttempt,
            });
        } else {
            score_ops.push(ScoreOp {
                member_id: claimed,
                event: MisbehavingCommit,
            });
        }
    }
}

/// Merge our own commit and surface the welcome we held back until merge.
/// Validation happened at commit-creation time, so no re-staging is needed.
fn apply_local_candidate<Pr>(
    provider: &Pr,
    conversation: &mut ConversationQueues,
    mls: &mut MlsService,
    chosen: BufferedCommitCandidate,
    ctx: &RoundContext,
) -> Result<CandidateOutcome, ConversationError>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    let delta = mls.merge_own_commit(provider)?;
    conversation.store_membership_delta(delta);

    // The merge is the commit point (epoch advanced by one); derive it rather
    // than reading MLS again, so an already-applied commit can't become an error.
    finalize_committed_batch(conversation, chosen.commit_hash, ctx.current_epoch + 1);

    // Build the welcome artifact only after merge.
    let joiner_identities = chosen.joiner_identities;
    let welcome = chosen.welcome_bytes.map(|welcome_bytes| MemberWelcome {
        welcome_bytes,
        conversation_sync_bytes: Vec::new(),
        joiner_identities,
    });

    Ok(CandidateOutcome::Terminal {
        outcome: CommitRoundOutcome::Applied {
            result: leave_or_update(ctx.self_remove_pending),
            welcome,
        },
        committer: chosen.candidate_msg.steward_member_id,
    })
}

/// A merged commit either ejected the local member (its removal was in the
/// batch) or advanced the group; map that to the inbound result the caller
/// dispatches.
fn leave_or_update(self_removed: bool) -> ProcessResult {
    if self_removed {
        ProcessResult::LeaveConversation
    } else {
        ProcessResult::ConversationUpdated
    }
}

/// Stage, validate, and merge a candidate authored by another steward.
/// `Drop` (with penalty) on any failed check; `Terminal(Applied)` on merge.
///
/// Staging coexists with our own pending commit — OpenMLS only needs the single
/// pending slot free at *merge* time, so this discards our own commit right
/// before merging the winning remote.
fn apply_incoming_candidate<Pr>(
    provider: &Pr,
    conversation: &mut ConversationQueues,
    mls: &mut MlsService,
    steward_list: &StewardListService,
    chosen: BufferedCommitCandidate,
    ctx: &RoundContext,
) -> Result<CandidateOutcome, ConversationError>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    // Validation ladder: stage → authorize sender → match actions. The first
    // failing gate rolls back the staging and drops; the caller's loop then
    // tries the next candidate.
    let (commit_sender, self_removed, commit_actions) = match stage_candidate(
        provider,
        mls,
        conversation.name(),
        &chosen.candidate_msg,
        ctx,
    )? {
        StagingOutcome::Staged {
            commit_sender,
            self_removed,
            commit_actions,
        } => (commit_sender, self_removed, commit_actions),
        StagingOutcome::Abort => return Ok(reject_staged(mls, None)),
        StagingOutcome::Violation(v) => {
            return Ok(reject_staged(mls, v.target_score_op()));
        }
    };

    // Commit sender must be on the steward list (RFC §"Commit validation service").
    if let Some(violation) =
        check_commit_sender_authorized(conversation, steward_list, &commit_sender, ctx)
    {
        return Ok(reject_staged(mls, violation.target_score_op()));
    }

    // MLS actions must match the set we voted to approve.
    if let Some(violation) =
        validate_commit_candidate(conversation, &commit_sender, &commit_actions, ctx)?
    {
        return Ok(reject_staged(mls, violation.target_score_op()));
    }

    // The remote wins. Clear our own pending commit (if any) before applying it.
    // Safe — the staged remote is held separately and survives this discard.
    mls.discard_own_commit(provider)?;
    let delta = mls.merge_staged_commit(provider)?;
    conversation.store_membership_delta(delta);
    // The merge is the commit point (epoch advanced by one); derive it rather
    // than reading MLS again, so an already-applied commit can't become an error.
    finalize_committed_batch(conversation, chosen.commit_hash, ctx.current_epoch + 1);

    Ok(CandidateOutcome::Terminal {
        outcome: CommitRoundOutcome::Applied {
            result: leave_or_update(self_removed),
            welcome: None,
        },
        committer: commit_sender,
    })
}

/// Roll back a staged remote candidate and drop it, optionally with a penalty,
/// so the priority loop falls through to the next candidate.
fn reject_staged(mls: &mut MlsService, penalty: Option<ScoreOp>) -> CandidateOutcome {
    mls.discard_staged_commit();
    CandidateOutcome::Drop(penalty)
}

/// Apply post-commit bookkeeping and return the cleared batch (empty when
/// the commit was urgent-target-only and only the targeted entry was
/// dropped). Caller surfaces the batch through `CommitRoundResult` so
/// it can be archived for UI history.
fn finalize_committed_batch(
    conversation: &mut ConversationQueues,
    commit_hash: CommitHash,
    current_epoch: u64,
) {
    conversation.insert_committed_hash(commit_hash);
    // Stamp join epochs and clear resolved pending/approved entries now, before
    // the steward-list reconcile reads settled membership.
    conversation.apply_membership_delta_bookkeeping(current_epoch);
    if let Some(target) = conversation.take_urgent_commit_target() {
        // Urgent commit: leave the rest of the queue for the next cycle.
        conversation.drop_approved_removals_for(&target);
    } else {
        conversation.drain_approved_proposals();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// High-level apply-loop behaviour: when a higher-priority remote candidate
    /// fails staging, our own lower-priority local candidate still applies — the
    /// loop falls through to it rather than discarding it out from under us.
    #[test]
    fn local_candidate_applies_after_a_higher_priority_remote_fails() {
        use crate::commit_round::round::compute_commit_hash;
        use crate::mls_crypto::MlsCommitInput;
        use crate::protos::de_mls::messages::v1::CommitCandidate;
        use crate::test_fixtures::{TEST_SUITE, make_creator_mls, steward_service_steward};
        use openmls::credentials::{BasicCredential, CredentialWithKey};
        use openmls::key_packages::KeyPackage;
        use openmls::prelude::tls_codec::Serialize as _;
        use openmls_basic_credential::SignatureKeyPair;

        let (mut mls, provider, signer) = make_creator_mls(b"alice");
        let alice = b"alice".to_vec();
        let steward = steward_service_steward(&alice);

        // Alice's real own commit candidate (add bob).
        let bob_kp = {
            let s = SignatureKeyPair::new(TEST_SUITE.signature_algorithm()).unwrap();
            let cred = CredentialWithKey {
                credential: BasicCredential::new(b"bob".to_vec()).into(),
                signature_key: s.to_public_vec().into(),
            };
            KeyPackage::builder()
                .build(TEST_SUITE, &provider, &s, cred)
                .unwrap()
                .key_package()
                .tls_serialize_detached()
                .unwrap()
        };
        let artifacts = mls
            .create_commit_candidate(&provider, &signer, &[MlsCommitInput::Add(bob_kp)])
            .unwrap();
        let local = BufferedCommitCandidate {
            candidate_msg: CommitCandidate {
                conversation_id: b"test-conversation".to_vec(),
                proposal_count: artifacts.proposal_count as u32,
                commit_message: artifacts.commit.clone(),
                steward_member_id: alice.clone(),
            },
            commit_hash: compute_commit_hash(&artifacts.commit),
            is_local_candidate: true,
            welcome_bytes: artifacts.welcome.clone(),
            joiner_identities: vec![b"bob".to_vec()],
        };

        // A higher-priority remote that fails staging (garbage commit).
        let failing_remote = BufferedCommitCandidate {
            candidate_msg: CommitCandidate {
                conversation_id: b"test-conversation".to_vec(),
                proposal_count: 1,
                commit_message: vec![0xFFu8; 64],
                steward_member_id: alice.clone(),
            },
            commit_hash: compute_commit_hash(&[0xFFu8; 64]),
            is_local_candidate: false,
            welcome_bytes: None,
            joiner_identities: vec![],
        };

        let epoch_before = mls.group().epoch().as_u64();
        let mut conversation = ConversationQueues::new("test-conversation", 10);
        let ctx = RoundContext {
            mls_count: 1,
            self_remove_pending: false,
            current_epoch: epoch_before,
            in_recovery: false,
            epoch_steward_id: Some(alice.clone()),
        };

        // Remote first (higher priority), our local second.
        let result = apply_in_priority_order(
            &provider,
            &mut conversation,
            &mut mls,
            &steward,
            vec![failing_remote, local],
            &ctx,
            &alice,
        )
        .unwrap();

        assert!(
            matches!(result.outcome, CommitRoundOutcome::Applied { .. }),
            "own local commit applied after the higher-priority remote failed"
        );
        assert_eq!(mls.group().epoch().as_u64(), epoch_before + 1);
        assert!(
            mls.group()
                .members()
                .any(|m| m.credential.serialized_content() == b"bob"),
            "our own commit (add bob) was applied"
        );
    }
}
