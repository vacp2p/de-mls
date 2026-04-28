//! Steward commit candidate creation and group member queries.

use openmls_rust_crypto::MemoryStorage;
use prost::Message;
use tracing::info;

use crate::core::api::compute_commit_hash;
use crate::core::proposal_kind::ProposalKind;
use crate::core::{
    error::CoreError,
    group::{BufferedCommitCandidate, Group},
};
use crate::ds::{APP_MSG_SUBTOPIC, OutboundPacket};
use crate::mls_crypto::{
    CommitCandidate as MlsCommitCandidate, DeMlsStorage, GroupUpdate, KeyPackageBytes, MlsService,
};
use crate::protos::de_mls::messages::v1::{AppMessage, CommitCandidate, group_update_request};

// ─────────────────────────── Steward Operations ───────────────────────────

/// Build a commit candidate and buffer it for [`crate::core::finalize_freeze_round`].
///
/// The gate is plain `is_steward()` (list membership) — intentionally **not**
/// `is_live_epoch_steward` or a list-exhaustion check. If an election fails
/// its retries and the list exhausts, members of the *previous* list can
/// still commit recovery actions (e.g. a leave) to advance the epoch and
/// trigger a fresh election.
///
/// Layer 3 exception: when `Group::is_in_recovery_mode()` is true (a
/// `Deadlock` ECP just opened the recovery window), the steward gate is
/// bypassed entirely so any member can produce the recovery commit.
pub fn create_commit_candidate<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    app_id: &[u8],
) -> Result<Option<OutboundPacket>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !group.is_steward() && !group.is_in_recovery_mode() {
        return Err(CoreError::NotASteward);
    }

    if group.approved_proposals().is_empty() {
        return Err(CoreError::NoProposals);
    }

    if !mls.has_group(group.group_name()) {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    // MLS forbids committing one's own removal. If the approved batch contains
    // RemoveMember(self), skip local candidate creation — another steward will
    // commit the batch (including this node's removal) once they enter freeze.
    let self_identity = mls.wallet_bytes();
    let self_removal_pending = group.approved_proposals().values().any(|req| {
        matches!(
            req.payload.as_ref(),
            Some(group_update_request::Payload::RemoveMember(r))
                if r.identity == self_identity
        )
    });
    if self_removal_pending {
        info!(
            group = group.group_name(),
            "commit candidate skipped: approved batch contains self-remove"
        );
        return Ok(None);
    }

    // Governance proposals (emergency, election) are consensus-only and must
    // not be in the approved queue at batch creation time.
    let non_mls_ids: Vec<u32> = group
        .approved_proposals()
        .iter()
        .filter(|(_, req)| ProposalKind::of(req).is_governance())
        .map(|(&id, _)| id)
        .collect();

    if !non_mls_ids.is_empty() {
        return Err(CoreError::UnexpectedNonMlsProposals {
            proposal_ids: non_mls_ids,
        });
    }

    // Drop approved entries already reflected in group state (stale
    // rebroadcast KPs, duplicate removes) — without this MLS would reject
    // the whole batch with "Duplicate signature key in proposals and group".
    let current_members = mls.members(group.group_name())?;
    let is_member = |id: &[u8]| current_members.iter().any(|m| m == id);

    // Urgent (ECP-driven) freeze: restrict the batch to just the target's
    // RemoveMember. Other approved proposals stay queued for the next
    // normal cycle — see `Group::urgent_commit_target`.
    let urgent_target = group.urgent_commit_target().map(|t| t.to_vec());

    let proposals = group.approved_proposals();
    let mut updates = Vec::with_capacity(proposals.len());
    for proposal in proposals.values() {
        match proposal.payload.as_ref() {
            Some(group_update_request::Payload::InviteMember(im)) => {
                if urgent_target.is_some() {
                    continue;
                }
                if is_member(&im.identity) {
                    continue;
                }
                updates.push(GroupUpdate::Add(KeyPackageBytes::new(
                    im.key_package_bytes.clone(),
                    im.identity.clone(),
                )));
            }
            Some(group_update_request::Payload::RemoveMember(rm)) => {
                if let Some(target) = urgent_target.as_deref()
                    && rm.identity != target
                {
                    continue;
                }
                if !is_member(&rm.identity) {
                    continue;
                }
                updates.push(GroupUpdate::Remove(rm.identity.clone()));
            }
            _ => return Err(CoreError::InvalidGroupUpdateRequest),
        }
    }

    if updates.is_empty() {
        return Ok(None);
    }

    let MlsCommitCandidate {
        proposals: mls_proposals,
        commit,
        welcome,
    } = mls.create_commit_candidate(group.group_name(), &updates)?;

    let candidate = CommitCandidate {
        group_name: group.group_name_bytes().to_vec(),
        mls_proposals,
        commit_message: commit,
        steward_identity: mls.wallet_bytes(),
    };

    // Welcome bytes are deferred: sent from finalize_freeze_round after the
    // commit merges, so joiners can't advance epoch ahead of the steward.
    let commit_hash = compute_commit_hash(&candidate.commit_message);
    let epoch = mls.current_epoch(group.group_name())?;
    let _ = group.add_freeze_candidate(
        BufferedCommitCandidate {
            candidate_msg: candidate.clone(),
            commit_hash,
            is_local_candidate: true,
            welcome_bytes: welcome,
        },
        epoch,
    );

    info!(
        group = group.group_name(),
        epoch,
        proposals = updates.len(),
        "commit candidate created"
    );

    let candidate_msg: AppMessage = candidate.into();
    Ok(Some(OutboundPacket::new(
        candidate_msg.encode_to_vec(),
        APP_MSG_SUBTOPIC,
        group.group_name(),
        app_id,
    )))
}

// ─────────────────────────── Member Queries ───────────────────────────

/// Get the current members of a group.
pub fn group_members<S>(group: &Group, mls: &MlsService<S>) -> Result<Vec<Vec<u8>>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !mls.has_group(group.group_name()) {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    let members = mls.members(group.group_name())?;
    Ok(members)
}
