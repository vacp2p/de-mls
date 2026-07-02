//! Commit validation service (RFC §"Commit validation service"): stage a remote
//! candidate, authorize its committer against the steward list, and check its
//! MLS actions against the voted-approved set. Drives `apply_incoming_candidate`.

use std::error::Error as StdError;

use openmls_traits::{OpenMlsProvider, storage::StorageProvider};

use crate::{
    ConversationError, ConversationQueues, StewardListService,
    commit_round::context::RoundContext,
    mls_crypto::{MlsProposalOutput, MlsService, StagedCandidateResult},
    protos::de_mls::messages::v1::{
        CommitCandidate, ConversationUpdateRequest, ViolationEvidence,
        conversation_update_request::Payload,
    },
};

pub enum StagingOutcome {
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

/// Check that a commit's MLS actions match the voted-approved set.
/// `Some(evidence)` on mismatch.
///
/// Compares both sides as sorted `(kind_tag, &member_id)` projections —
/// no `MlsProposalOutput` allocation, no per-element clone.
pub fn validate_commit_candidate(
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

    /// An exhausted list (Layer-2 re-election window) must still verify the
    /// committer against its members — it does NOT grant the Layer-3 "any member
    /// may commit" permission. A steward's superseding commit passes; a
    /// non-steward's is rejected.
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
}
