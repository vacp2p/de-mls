//! Phase-3 loop: apply each candidate in priority order until one wins.
//! Per-candidate apply paths (local merge / remote stage-validate-merge),
//! MLS staging, validation, and post-commit bookkeeping live here.

use std::error::Error as StdError;

use openmls_traits::{OpenMlsProvider, storage::StorageProvider};

use crate::{
    CommitHash, ConversationError, ConversationQueues, FreezeFinalizeResult, FreezeOutcome,
    ProcessResult,
    ScoreEvent::{self, MisbehavingCommit},
    ScoreOp, StewardListService,
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

/// Walk `sorted` best-first; the first candidate that applies wins.
/// Rejected candidates score a local penalty
/// (RFC §Peer Scoring allows direct local scoring for observable violations).
pub(super) fn apply_in_priority_order<Pr>(
    provider: &Pr,
    conversation: &mut ConversationQueues,
    mls: &mut MlsService,
    steward: &StewardListService,
    sorted: Vec<BufferedCommitCandidate>,
    ctx: &RoundContext,
    self_member_id: &[u8],
) -> Result<FreezeFinalizeResult, ConversationError>
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
            apply_incoming_candidate(provider, conversation, mls, steward, chosen, ctx)?
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

    // Defensive cleanup — normally a no-op. A local candidate always wins once
    // reached, so reaching here means none was in the round: either we built no
    // commit, or we built one that wasn't buffered (per-round cap / duplicate
    // hash). In that last case a stray pending commit lingers in MLS and would
    // block the next operation (only one pending commit allowed), so clear it.
    mls.discard_own_commit(provider)?;
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
fn record_winner_scores(
    score_ops: &mut Vec<ScoreOp>,
    committer: &[u8],
    self_member_id: &[u8],
    ctx: &RoundContext,
    losers: impl Iterator<Item = BufferedCommitCandidate>,
    sl_service: &StewardListService,
) {
    score_ops.push(ScoreOp {
        member_id: committer.to_vec(),
        event: ScoreEvent::SuccessfulCommit,
    });

    // `epoch_steward_id` was resolved through `steward_eligibility`
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
        if sl_service.is_steward(&claimed) {
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

// ─────────────────────────── Application Paths ───────────────────────────

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
    mls.merge_own_commit(provider)?;

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
/// Staging coexists with our own pending commit — OpenMLS only needs the single
/// pending slot free at *merge* time, so this discards our own commit right
/// before merging the winning remote.
fn apply_incoming_candidate<Pr>(
    provider: &Pr,
    conversation: &mut ConversationQueues,
    mls: &mut MlsService,
    sl_service: &StewardListService,
    chosen: BufferedCommitCandidate,
    ctx: &RoundContext,
) -> Result<CandidateOutcome, ConversationError>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    let conversation_id = conversation.name().to_owned();

    let (commit_sender, self_removed, commit_actions) =
        match stage_candidate(provider, mls, &conversation_id, &chosen.candidate_msg, ctx)? {
            StagingOutcome::Staged {
                commit_sender,
                self_removed,
                commit_actions,
            } => (commit_sender, self_removed, commit_actions),
            StagingOutcome::Abort => {
                // The commit never staged (stale epoch, wrong group, malformed),
                // so its sender was never MLS-authenticated. The only id we hold
                // is the plaintext wire `steward_member_id`, which is forgeable —
                // scoring it would let anyone grief a victim with junk
                // candidates. Drop without penalty and try the next candidate.
                mls.discard_staged_commit(provider)?;
                return Ok(CandidateOutcome::Drop(None));
            }
            StagingOutcome::Violation(v) => {
                mls.discard_staged_commit(provider)?;
                return Ok(CandidateOutcome::Drop(v.target_score_op()));
            }
        };

    // Commit sender must be on the steward list (RFC §"Commit validation service").
    if let Some(violation) =
        check_commit_sender_authorized(conversation, sl_service, &commit_sender, ctx)
    {
        mls.discard_staged_commit(provider)?;
        return Ok(CandidateOutcome::Drop(violation.target_score_op()));
    }

    // MLS actions must match the set we voted to approve.
    if let Some(violation) =
        validate_commit_candidate(conversation, &commit_sender, &commit_actions, ctx)?
    {
        mls.discard_staged_commit(provider)?;
        return Ok(CandidateOutcome::Drop(violation.target_score_op()));
    }

    // The remote wins. Clear our own pending commit (if any) before applying it:
    // de-mls's split staging flow doesn't auto-clear it, and a leftover would
    // block the next MLS operation. Safe — the staged remote is held separately
    // and survives this discard (see `discard_own_then_merge_remote_*` test).
    mls.discard_own_commit(provider)?;
    mls.merge_staged_commit(provider)?;
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
fn stage_candidate<Pr>(
    provider: &Pr,
    mls: &mut MlsService,
    conversation_id: &str,
    candidate: &CommitCandidate,
    ctx: &RoundContext,
) -> Result<StagingOutcome, ConversationError>
where
    Pr: OpenMlsProvider,
    <Pr::StorageProvider as StorageProvider<1>>::Error: StdError + Send + Sync + 'static,
{
    let staged_result = mls
        .stage_remote_commit(provider, &candidate.mls_proposals, &candidate.commit_message)
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
) -> Result<Option<ViolationEvidence>, ConversationError> {
    let mut expected: Vec<(u8, &[u8])> = conversation
        .approved_proposals()
        .values()
        .filter_map(action_projection_from_request)
        .collect();
    let mut actual: Vec<(u8, &[u8])> = mls_actions.iter().map(action_projection_from_mls).collect();
    // Dedup by (kind, member): RFC §Consensus Types lets any member create a
    // Commit proposal, so several finalized proposals may name the same
    // membership change. The steward commits that change once, so the MLS
    // actions carry one entry where the voted set carries duplicates — not a
    // violation. Both sides are compared as deduplicated sets.
    expected.sort();
    expected.dedup();
    actual.sort();
    actual.dedup();

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
/// `None` also covers "no list yet" (joiner pre-sync) and "Layer-3
/// recovery_mode active" (RFC §Anti-Deadlock: any member MAY commit to restore
/// liveness; mirrors the relaxed gate in `create_commit_candidate`).
fn check_commit_sender_authorized(
    conversation: &ConversationQueues,
    sl_service: &StewardListService,
    commit_sender: &[u8],
    ctx: &RoundContext,
) -> Option<ViolationEvidence> {
    if ctx.in_recovery {
        return None;
    }
    sl_service.current_list()?;
    if sl_service.is_steward(commit_sender) {
        return None;
    }
    tracing::warn!(
        conversation = conversation.name(),
        "violation: commit from unauthorized sender"
    );
    Some(ViolationEvidence::misbehaving_commit(
        commit_sender.to_vec(),
        ctx.current_epoch,
        "commit from unauthorized sender (not on the steward list)",
    ))
}

// ─────────────────────────── State Utilities ───────────────────────────

/// Apply post-commit bookkeeping and return the cleared batch (empty when
/// the commit was urgent-target-only and only the targeted entry was
/// dropped). Caller surfaces the batch through `FreezeFinalizeResult` so
/// it can be archived for UI history.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StewardListConfig;

    fn ctx_at(epoch: u64, in_recovery: bool) -> RoundContext {
        RoundContext {
            mls_count: 1,
            self_remove_pending: false,
            current_epoch: epoch,
            in_recovery,
            epoch_steward_id: None,
        }
    }

    fn list_over_two_epochs() -> (StewardListService, Vec<u8>, Vec<u8>) {
        let mut sl = StewardListService::empty(StewardListConfig::new(2, 2).unwrap());
        let alice = vec![1u8; 20];
        let bob = vec![2u8; 20];
        // Installed for epochs [0, 2) → exhausted at epoch 2, members still known.
        sl.install_list(0, &[alice.clone(), bob.clone()], 2, 0)
            .unwrap();
        assert!(sl.is_exhausted(2), "list [0,2) is exhausted at epoch 2");
        (sl, alice, bob)
    }

    /// Regression guard: an exhausted list (Layer-2 re-election window) must still
    /// verify the committer against its members — it does NOT grant the Layer-3
    /// "any member may commit" permission. A steward's superseding commit passes;
    /// a non-steward's is rejected.
    #[test]
    fn exhausted_list_still_gates_on_membership() {
        let (sl, alice, _bob) = list_over_two_epochs();
        let carol = vec![3u8; 20]; // not on the list
        let queues = ConversationQueues::new("g");
        let ctx = ctx_at(2, false);

        assert!(
            check_commit_sender_authorized(&queues, &sl, &alice, &ctx).is_none(),
            "on-list steward stays authorized when the list is exhausted"
        );
        assert!(
            check_commit_sender_authorized(&queues, &sl, &carol, &ctx).is_some(),
            "non-steward commit must be rejected even when the list is exhausted"
        );
    }

    /// Layer 3: recovery mode is the one gate that opens for any sender.
    #[test]
    fn recovery_mode_authorizes_any_sender() {
        let (sl, _alice, _bob) = list_over_two_epochs();
        let queues = ConversationQueues::new("g");
        let ctx = ctx_at(2, true);
        assert!(
            check_commit_sender_authorized(&queues, &sl, &[9u8; 20], &ctx).is_none(),
            "recovery mode authorizes any member"
        );
    }

    /// High-level apply-loop behaviour and the regression guard for the
    /// lost-own-commit lag: when a higher-priority remote candidate fails, our
    /// own lower-priority local candidate still applies — it is no longer
    /// discarded out from under us. Under the old pre-discard this returned
    /// `NoCandidate`.
    #[test]
    fn local_candidate_applies_after_a_higher_priority_remote_fails() {
        use crate::freeze::round::compute_commit_hash;
        use crate::mls_crypto::MlsCommitInput;
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
                mls_proposals: artifacts.proposals.clone(),
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
                mls_proposals: vec![],
                commit_message: vec![0xFFu8; 64],
                steward_member_id: alice.clone(),
            },
            commit_hash: compute_commit_hash(&[0xFFu8; 64]),
            is_local_candidate: false,
            welcome_bytes: None,
            joiner_identities: vec![],
        };

        let epoch_before = mls.current_epoch().unwrap();
        let mut conversation = ConversationQueues::new("test-conversation");
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
            matches!(result.outcome, FreezeOutcome::Applied { .. }),
            "own local commit applied after the higher-priority remote failed"
        );
        assert_eq!(mls.current_epoch().unwrap(), epoch_before + 1);
        assert!(
            mls.is_member(b"bob"),
            "our own commit (add bob) was applied"
        );
    }
}
