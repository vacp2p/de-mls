//! Steward commit candidate creation and group member queries.

use super::freeze::compute_commit_hash;
use super::*;

// ─────────────────────────── Steward Operations ───────────────────────────

/// Create and broadcast a commit candidate for the current epoch.
///
/// This does not merge the commit immediately. The candidate is buffered and
/// later applied via [`finalize_freeze_round`].
pub fn create_commit_candidate<S>(
    group: &mut Group,
    mls: &MlsService<S>,
    app_id: &[u8],
) -> Result<Vec<OutboundPacket>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !group.is_steward() {
        return Err(CoreError::StewardNotSet);
    }

    let proposals = group.approved_proposals();
    if proposals.is_empty() {
        return Err(CoreError::NoProposals);
    }

    if !mls.has_group(group.group_name()) {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    // Emergency criteria and steward election proposals are consensus-only — they don't
    // produce MLS operations and must NOT be in the approved queue at batch creation time.
    let non_mls_ids: Vec<u32> = proposals
        .iter()
        .filter(|(_, req)| {
            matches!(
                req.payload,
                Some(group_update_request::Payload::EmergencyCriteria(_))
                    | Some(group_update_request::Payload::StewardElection(_))
            )
        })
        .map(|(&id, _)| id)
        .collect();

    if !non_mls_ids.is_empty() {
        return Err(CoreError::UnexpectedNonMlsProposals {
            proposal_ids: non_mls_ids,
        });
    }

    let mut updates = Vec::with_capacity(proposals.len());
    for (_, proposal) in proposals {
        match proposal.payload {
            Some(group_update_request::Payload::InviteMember(im)) => {
                updates.push(GroupUpdate::Add(KeyPackageBytes::new(
                    im.key_package_bytes,
                    im.identity,
                )));
            }
            Some(group_update_request::Payload::RemoveMember(identity)) => {
                updates.push(GroupUpdate::Remove(identity.identity));
            }
            _ => return Err(CoreError::InvalidGroupUpdateRequest),
        }
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

    // Store own candidate locally for deterministic selection at freeze timeout.
    // Welcome bytes are deferred — they'll be sent after commit merge in finalize_freeze_round
    // to prevent joiners from advancing epoch before the steward merges.
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

    let candidate_msg: AppMessage = candidate.into();

    let batch_packet = OutboundPacket::new(
        candidate_msg.encode_to_vec(),
        APP_MSG_SUBTOPIC,
        group.group_name(),
        app_id,
    );

    Ok(vec![batch_packet])
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
