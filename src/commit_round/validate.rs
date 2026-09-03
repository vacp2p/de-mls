//! Commit validation service (RFC §"Commit validation service"): validate each
//! candidate — stage it, authorize its committer against the steward list, and
//! check its MLS actions against the voted-approved set — and rank the
//! candidates by RFC priority to pick the winner. Ranking drives
//! `finalize_commit_round`; per-candidate validation drives `apply_incoming_candidate`.

use std::error::Error as StdError;

use openmls_traits::{OpenMlsProvider, storage::StorageProvider};

use crate::{
    BufferedCommitCandidate, ConversationError, ConversationQueues, StewardListService,
    commit_round::context::RoundContext,
    mls_crypto::{
        MlsProposalOutput, MlsService, StagedCandidateResult, signature_key_of_key_package,
    },
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, ViolationEvidence,
        conversation_update_request::Payload,
    },
};

#[derive(Debug)]
pub enum StagingOutcome {
    /// Staged cleanly; the wire claims match the staged commit.
    Staged {
        commit_sender: Vec<u8>,
        self_removed: bool,
        commit_actions: Vec<MlsProposalOutput>,
    },
    /// MLS rejected the commit (bad wire, protocol error).
    Abort,
    /// Staged, but a wire claim (`steward_member_id`, `proposal_count`)
    /// contradicts the staged commit.
    Violation(ViolationEvidence),
}

/// Stage the commit and cross-check the candidate's wire claims against it.
///
/// Leaves MLS in the staged state on `Staged`; the caller must clean up
/// via `discard_staged_commit` for `Abort` / `Violation`.
pub fn stage_candidate<Pr>(
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
        .stage_remote_commit(provider, &candidate.commit_message)
        .inspect_err(|e| {
            tracing::debug!(conversation = conversation_id, error = %e, "candidate failed to stage");
        });

    let (commit_sender, self_removed, commit_actions, staged_count) = match staged_result {
        Ok(StagedCandidateResult::Staged {
            commit_sender,
            self_removed,
            actions,
            proposal_count,
        }) => (commit_sender, self_removed, actions, proposal_count),
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

    // The wire-claimed count fed the pre-stage ranking; a mismatch with the
    // staged commit gamed the selection order. Attributed to the MLS signer.
    if staged_count != candidate.proposal_count as usize {
        tracing::warn!(
            conversation = conversation_id,
            claimed = candidate.proposal_count,
            staged = staged_count,
            "violation: claimed proposal count doesn't match the staged commit"
        );
        return Ok(StagingOutcome::Violation(ViolationEvidence::broken_commit(
            commit_sender,
            ctx.current_epoch,
            "commit candidate's claimed proposal count doesn't match its actions",
        )));
    }

    Ok(StagingOutcome::Staged {
        commit_sender,
        self_removed,
        commit_actions,
    })
}

/// Check that a commit's MLS actions match the voted-approved set.
/// `Some(evidence)` on mismatch.
///
/// Compares both sides as sorted `(kind, &member_id)` projections —
/// no `MlsProposalOutput` allocation, no per-element clone.
///
/// Scope is Add and Remove. A commit may also carry an Update, and a committer
/// rotates its own leaf through the commit's UpdatePath rather than a proposal,
/// so a leaf's credential or signature key can change without reaching either
/// side of this comparison. Everything keyed by leaf — peer score, join epoch,
/// steward eligibility — follows the leaf rather than the key, and so stays with
/// whoever holds that leaf afterwards.
pub fn validate_commit_candidate(
    conversation: &ConversationQueues,
    sender_id: &[u8],
    mls_actions: &[MlsProposalOutput],
    ctx: &RoundContext,
) -> Result<Option<ViolationEvidence>, ConversationError> {
    let mut expected: Vec<(ActionKind, Vec<u8>)> = conversation
        .approved_proposals()
        .values()
        .filter_map(action_projection_from_request)
        .collect();
    let mut actual: Vec<(ActionKind, Vec<u8>)> =
        mls_actions.iter().map(action_projection_from_mls).collect();
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

/// The membership change an action performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ActionKind {
    Add,
    Remove,
}

/// `(kind, id)` projection of an approved voting request — an add keys on
/// the joiner's signature key, a remove on the target's `member_id` (leaf
/// index). Returns `None` for non-MLS payloads (emergency/election).
fn action_projection_from_request(
    req: &ConversationUpdateRequest,
) -> Option<(ActionKind, Vec<u8>)> {
    match req.payload.as_ref()? {
        Payload::MemberInvite(im) => Some((
            ActionKind::Add,
            signature_key_of_key_package(&im.key_package_bytes).ok()?,
        )),
        Payload::RemoveMember(rm) => Some((ActionKind::Remove, rm.member_id.clone())),
        _ => None,
    }
}

/// `(kind, id)` projection of an MLS-staged action, matching
/// [`action_projection_from_request`]: add by the signature key, remove by leaf index.
fn action_projection_from_mls(action: &MlsProposalOutput) -> (ActionKind, Vec<u8>) {
    match action {
        MlsProposalOutput::Add(id) => (ActionKind::Add, id.clone()),
        MlsProposalOutput::Remove(id) => (ActionKind::Remove, id.clone()),
    }
}

/// RFC §"de-MLS Objects": any steward-list member may commit to preserve
/// liveness. Epoch-steward priority is about *selection*, not authorization.
///
/// `None` also covers "no list yet" (joiner pre-sync) and "Layer-3
/// recovery_mode active" (RFC §Anti-Deadlock: any member MAY commit to restore
/// liveness; mirrors the relaxed gate in `build_local_candidate`).
pub fn check_commit_sender_authorized(
    conversation: &ConversationQueues,
    steward_list: &StewardListService,
    commit_sender: &[u8],
    ctx: &RoundContext,
) -> Option<ViolationEvidence> {
    if ctx.in_recovery {
        return None;
    }
    steward_list.current_list()?;
    if steward_list.is_steward(commit_sender) {
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

// ─────────────────────────── Selection ───────────────────────────

/// Filter candidates whose action count matches the voted set, then sort
/// survivors by the RFC priority comparator (best-first).
pub fn rank_applicable_candidates(
    candidates: Vec<BufferedCommitCandidate>,
    ctx: &RoundContext,
    allow_subset: bool,
) -> Vec<BufferedCommitCandidate> {
    let mut sorted: Vec<_> = candidates
        .into_iter()
        .filter(|c| {
            let n = c.candidate_msg.proposal_count as usize;
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
fn compare_candidate_priority(
    a: &BufferedCommitCandidate,
    b: &BufferedCommitCandidate,
    epoch_steward_id: Option<&[u8]>,
) -> std::cmp::Ordering {
    // Tier: 0 for the epoch steward, 1 for everyone else.
    let tier = |c: &BufferedCommitCandidate| -> u8 {
        match epoch_steward_id {
            Some(es) if c.candidate_msg.steward_member_id == es => 0,
            _ => 1,
        }
    };

    // In priority order: largest proposal count (`b` before `a` for descending),
    // then epoch steward, then smallest `steward_member_id`, then lowest hash.
    b.candidate_msg
        .proposal_count
        .cmp(&a.candidate_msg.proposal_count)
        .then_with(|| tier(a).cmp(&tier(b)))
        .then_with(|| {
            a.candidate_msg
                .steward_member_id
                .cmp(&b.candidate_msg.steward_member_id)
        })
        .then_with(|| a.commit_hash.cmp(&b.commit_hash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitHash, StewardListConfig, compute_commit_hash};

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

    /// An exhausted list (Layer-2 re-election window) must still verify the
    /// committer against its members — it does NOT grant the Layer-3 "any member
    /// may commit" permission. A steward's superseding commit passes; a
    /// non-steward's is rejected.
    #[test]
    fn exhausted_list_still_gates_on_membership() {
        let (sl, alice, _bob) = list_over_two_epochs();
        let carol = vec![3u8; 20]; // not on the list
        let queues = ConversationQueues::new("g", 10);
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
        let queues = ConversationQueues::new("g", 10);
        let ctx = ctx_at(2, true);
        assert!(
            check_commit_sender_authorized(&queues, &sl, &[9u8; 20], &ctx).is_none(),
            "recovery mode authorizes any member"
        );
    }

    fn sort_by_priority(
        candidates: &mut [BufferedCommitCandidate],
        epoch_steward_id: Option<&[u8]>,
    ) {
        candidates.sort_by(|a, b| compare_candidate_priority(a, b, epoch_steward_id));
    }

    fn hash(byte: u8) -> CommitHash {
        compute_commit_hash(&[byte; 32])
    }

    fn make_candidate(
        steward_member_id: Vec<u8>,
        actions_count: usize,
        commit_hash: CommitHash,
    ) -> BufferedCommitCandidate {
        BufferedCommitCandidate {
            candidate_msg: CommitCandidate {
                conversation_id: b"test-conversation".to_vec(),
                proposal_count: actions_count as u32,
                // commit_message is irrelevant to priority ordering (which
                // keys off proposal_count/steward_member_id/commit_hash).
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
        assert_eq!(candidates[0].candidate_msg.proposal_count as usize, 5);
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

        let (h_aa, h_cc) = (hash(0xAA), hash(0xCC));
        let expected_winner = h_aa.min(h_cc);

        assert_eq!(candidates[0].commit_hash, expected_winner);
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

    /// A claimed count that mismatches the staged commit is a violation
    /// pinned on the MLS-verified signer; an honest claim stages cleanly.
    #[test]
    fn a_lying_proposal_count_is_a_violation_and_an_honest_one_stages() {
        use crate::mls_crypto::{MlsCommitInput, member_id_of};
        use crate::protos::de_mls::messages::v1::CommitCandidate;
        use crate::test_fixtures::{TestProvider, make_creator_mls, member_key_package};

        let (mut alice, provider_a, signer_a) = make_creator_mls(b"alice");
        let provider_b = TestProvider::default();
        let (bob_kp, bob_signer) = member_key_package(&provider_b, b"bob");
        let artifacts = alice
            .create_commit_candidate(&provider_a, &signer_a, &[MlsCommitInput::Add(bob_kp)])
            .expect("add bob");
        alice.merge_own_commit(&provider_a).unwrap();
        let mut bob = crate::mls_crypto::MlsService::new_from_welcome(
            &provider_b,
            &artifacts.welcome.unwrap(),
            openmls::group::MlsGroupJoinConfig::builder(),
        )
        .unwrap()
        .unwrap();
        let bob_id = member_id_of(bob.group().own_leaf_index());

        let candidate_from_bob = |mls: &mut crate::mls_crypto::MlsService, claim: u32| {
            let carol_kp = member_key_package(&provider_b, b"carol").0;
            let built = mls
                .create_commit_candidate(&provider_b, &bob_signer, &[MlsCommitInput::Add(carol_kp)])
                .expect("bob's candidate");
            CommitCandidate {
                conversation_id: b"test-conversation".to_vec(),
                commit_message: built.commit,
                steward_member_id: bob_id.clone(),
                proposal_count: claim,
            }
        };
        let ctx = ctx_at(alice.group().epoch().as_u64(), false);

        // Claim 2, carry 1 → violation attributed to bob.
        let lying = candidate_from_bob(&mut bob, 2);
        let outcome = stage_candidate(&provider_a, &mut alice, "test-conversation", &lying, &ctx)
            .expect("staging itself succeeds");
        match outcome {
            StagingOutcome::Violation(evidence) => assert_eq!(evidence.target_member_id, bob_id),
            other => panic!("expected a violation, got {other:?}"),
        }
        alice.discard_staged_commit();
        bob.discard_own_commit(&provider_b).unwrap();

        // Honest claim on a fresh candidate → staged.
        let honest = candidate_from_bob(&mut bob, 1);
        let outcome = stage_candidate(&provider_a, &mut alice, "test-conversation", &honest, &ctx)
            .expect("staging succeeds");
        assert!(
            matches!(outcome, StagingOutcome::Staged { .. }),
            "an honest count stages cleanly, got {outcome:?}"
        );
        alice.discard_staged_commit();
    }
}
