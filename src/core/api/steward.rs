//! Steward commit candidate creation and group member queries.

use prost::Message;
use tracing::info;

use crate::{
    core::{
        api::compute_commit_hash,
        error::CoreError,
        group::{BufferedCommitCandidate, Group, member_set},
        proposal_kind::ProposalKind,
        steward_list::ProtocolConfig,
    },
    ds::{APP_MSG_SUBTOPIC, OutboundPacket},
    mls_crypto::{
        CommitCandidate as MlsCommitCandidate, IdentityProvider, KeyPackageBytes, MlsCommitInput,
        MlsService,
    },
    protos::de_mls::messages::v1::{AppMessage, CommitCandidate, group_update_request},
};

// ─────────────────────────── Steward Operations ───────────────────────────

/// Build a commit candidate and buffer it for [`crate::core::finalize_freeze_round`].
///
/// The gate is plain `is_steward()` (list membership) — intentionally **not**
/// `is_live_epoch_steward` or a list-exhaustion check, so members of the
/// *previous* list can commit recovery actions when an election fails.
/// `Group::is_in_recovery_mode()` (Layer 3) bypasses the gate entirely.
pub fn create_commit_candidate<M>(
    group: &mut Group<M>,
    app_id: &[u8],
) -> Result<Option<OutboundPacket>, CoreError>
where
    M: MlsService,
{
    if !group.is_steward() && !group.is_in_recovery_mode() {
        return Err(CoreError::NotASteward);
    }

    if group.approved_proposals().is_empty() {
        return Err(CoreError::NoProposals);
    }
    let res = group.mls();
    if res.is_none() {
        return Err(CoreError::MlsGroupNotInitialized);
    }
    let mls = res.unwrap();

    // MLS forbids committing one's own removal. If the approved batch contains
    // RemoveMember(self), skip local candidate creation — another steward will
    // commit the batch (including this node's removal) once they enter freeze.
    let self_identity = mls.identity().identity_bytes();
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
    let current_members = mls.members()?;
    let current_members_set = member_set(&current_members);
    let is_member = |id: &[u8]| current_members_set.contains(id);

    // Urgent (ECP-driven) freeze: restrict the batch to just the target's
    // RemoveMember. See `Group::urgent_commit_target`.
    let urgent_target = group.urgent_commit_target().map(|t| t.to_vec());

    // Iterate in insertion order (FIFO): library proposal IDs are
    // content-derived hashes, so sort-by-id is not temporal.
    let k_max = group.protocol_config().k_max;
    let mut updates = Vec::with_capacity(group.approved_order().len().min(k_max));
    for pid in group.approved_order() {
        if updates.len() >= k_max {
            break;
        }
        let Some(proposal) = group.approved_proposals().get(pid) else {
            continue;
        };
        match proposal.payload.as_ref() {
            Some(group_update_request::Payload::InviteMember(im)) => {
                if urgent_target.is_some() {
                    continue;
                }
                if is_member(&im.identity) {
                    continue;
                }
                updates.push(MlsCommitInput::Add(KeyPackageBytes::new(
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
                updates.push(MlsCommitInput::Remove(rm.identity.clone()));
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
    } = mls.create_commit_candidate(&updates)?;

    let candidate = CommitCandidate {
        group_name: group.group_name_bytes().to_vec(),
        mls_proposals,
        commit_message: commit,
        steward_identity: mls.identity().identity_bytes().to_vec(),
    };

    // Welcome bytes are deferred: sent from finalize_freeze_round after the
    // commit merges, so joiners can't advance epoch ahead of the steward.
    let commit_hash = compute_commit_hash(&candidate.commit_message);
    let epoch = mls.current_epoch()?;
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

/// Current members of a group, read from its MLS service. Returns an
/// empty list when the group hasn't accepted a welcome yet (e.g. a
/// pending joiner querying its own state).
pub fn group_members<M>(group: &Group<M>) -> Result<Vec<Vec<u8>>, CoreError>
where
    M: MlsService,
{
    match group.mls() {
        Some(mls) => Ok(mls.members()?),
        None => Ok(Vec::new()),
    }
}

// ─────────────────────────── Housekeeping Decisions ───────────────────────────

/// Outcome of [`evaluate_election_initiation`]. `Skip` carries a brief
/// reason for the log; `Proposed` carries the parameters the caller uses
/// to build and submit a `StewardElection` proposal.
#[derive(Debug, Clone)]
pub enum ElectionDecision {
    Skip(&'static str),
    Proposed {
        candidate_pool: Vec<Vec<u8>>,
        election_epoch: u64,
        retry_round: u32,
        protocol_config: ProtocolConfig,
    },
}

/// Decide whether this node SHOULD file a steward-election proposal.
///
/// Checks (in order): (1) gate on recovery vs list-exhaustion, (2)
/// election-already-in-flight, (3) responsible-proposer authorization,
/// (4) eligible-candidate-pool non-empty. The async submission stays in
/// the caller; this function performs no I/O.
pub fn evaluate_election_initiation<M: MlsService>(
    group: &Group<M>,
    mls_members: &[Vec<u8>],
    self_identity: &[u8],
    epoch: u64,
    recovery: bool,
    extra_exclude: Option<&[u8]>,
) -> ElectionDecision {
    if !recovery && !group.is_steward_list_exhausted(epoch) {
        return ElectionDecision::Skip("steward list not exhausted");
    }
    if group.has_election_in_flight() {
        return ElectionDecision::Skip("election already in flight");
    }

    let mls_members_set: std::collections::HashSet<&[u8]> =
        mls_members.iter().map(Vec::as_slice).collect();
    let is_authorized = group
        .steward_list()
        .and_then(|list| list.responsible_election_proposer(|c| mls_members_set.contains(c)))
        .is_some_and(|proposer| proposer == self_identity);
    if !is_authorized {
        return ElectionDecision::Skip("not the responsible proposer");
    }

    let candidate_pool: Vec<Vec<u8>> = mls_members
        .iter()
        .filter(|m| {
            if extra_exclude.is_some_and(|x| x == m.as_slice()) {
                return false;
            }
            if recovery && group.is_pending_removal(m) {
                return false;
            }
            true
        })
        .cloned()
        .collect();
    if candidate_pool.is_empty() {
        return ElectionDecision::Skip("no eligible candidates after filter");
    }

    ElectionDecision::Proposed {
        candidate_pool,
        election_epoch: epoch,
        retry_round: group.reelection_round(),
        protocol_config: group.protocol_config().clone(),
    }
}

/// `true` when this node is the responsible proposer for a Layer-3
/// `Deadlock` ECP. Requires the proposer to be MLS-present and not
/// queued for removal.
pub fn is_deadlock_ecp_proposer<M: MlsService>(
    group: &Group<M>,
    mls_members: &[Vec<u8>],
    self_identity: &[u8],
) -> bool {
    let mls_members_set: std::collections::HashSet<&[u8]> =
        mls_members.iter().map(Vec::as_slice).collect();
    group
        .steward_list()
        .and_then(|list| {
            list.responsible_election_proposer(|c| {
                mls_members_set.contains(c) && !group.is_pending_removal(c)
            })
        })
        .is_some_and(|proposer| proposer == self_identity)
}
