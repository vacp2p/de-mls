use super::freeze::compute_commit_hash;
use super::*;

// ─────────────────────────── Proposal Queries ───────────────────────────

/// Get the count of approved proposals waiting to be committed.
pub fn approved_proposals_count(handle: &GroupHandle) -> usize {
    handle.approved_proposals_count()
}

/// Get all approved proposals waiting to be committed.
pub fn approved_proposals(handle: &GroupHandle) -> HashMap<ProposalId, GroupUpdateRequest> {
    handle.approved_proposals()
}

/// Get the epoch history (past batches of approved proposals).
pub fn epoch_history(handle: &GroupHandle) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
    handle.epoch_history()
}

// ─────────────────────────── Steward Operations ───────────────────────────

/// Create and broadcast a commit candidate for the current epoch.
///
/// This does not merge the commit immediately. The candidate is buffered and
/// later applied via [`finalize_freeze_round`].
pub fn create_commit_candidate<S>(
    handle: &mut GroupHandle,
    mls: &MlsService<S>,
    app_id: &[u8],
) -> Result<Vec<OutboundPacket>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_steward() {
        return Err(CoreError::StewardNotSet);
    }

    let proposals = handle.approved_proposals();
    if proposals.is_empty() {
        return Err(CoreError::NoProposals);
    }

    if !handle.is_mls_initialized() {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    // Emergency criteria proposals are consensus-only — they don't produce MLS operations
    // and must NOT be in the approved queue at batch creation time.
    let emergency_ids: Vec<u32> = proposals
        .iter()
        .filter(|(_, req)| {
            matches!(
                req.payload,
                Some(group_update_request::Payload::EmergencyCriteria(_))
            )
        })
        .map(|(&id, _)| id)
        .collect();

    if !emergency_ids.is_empty() {
        return Err(CoreError::UnexpectedEmergencyProposals {
            proposal_ids: emergency_ids,
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
    } = mls.create_commit_candidate(handle.group_name(), &updates)?;

    let candidate = CommitCandidate {
        group_name: handle.group_name_bytes().to_vec(),
        mls_proposals,
        commit_message: commit,
    };

    // Store own candidate locally for deterministic selection at freeze timeout.
    // Welcome bytes are deferred — they'll be sent after commit merge in finalize_freeze_round
    // to prevent joiners from advancing epoch before the steward merges.
    let commit_hash = compute_commit_hash(&candidate.commit_message);
    let _ = handle.buffer_freeze_candidate(BufferedCommitCandidate {
        candidate_msg: candidate.clone(),
        commit_hash,
        is_local_candidate: true,
        welcome_bytes: welcome,
    });

    let candidate_msg: AppMessage = candidate.into();

    let batch_packet = OutboundPacket::new(
        candidate_msg.encode_to_vec(),
        APP_MSG_SUBTOPIC,
        handle.group_name(),
        app_id,
    );

    Ok(vec![batch_packet])
}

// ─────────────────────────── Member Queries ───────────────────────────

/// Get the current members of a group.
pub fn group_members<S>(
    handle: &GroupHandle,
    mls: &MlsService<S>,
) -> Result<Vec<Vec<u8>>, CoreError>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    if !handle.is_mls_initialized() {
        return Err(CoreError::MlsGroupNotInitialized);
    }

    let members = mls.members(handle.group_name())?;
    Ok(members)
}
