//! The approved and in-flight (voting) proposal queues, and the partial
//! freeze that an in-flight emergency criteria proposal drives (RFC §Partial
//! Freeze).

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::{
    engine::{
        proposal_kind::{
            ProposalKind, change_in_effect, change_of, in_flight_target, target_member_id_of,
        },
        queues::{EngineQueues, ProposalId, VotingMeta},
        types::{Action, ActionKind, MembershipDelta},
        util::single_voter_proposal_id,
    },
    protos::de_mls::messages::v1::{ApprovedProposal, ConversationUpdateRequest},
};

/// What became of an approved change offered to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Inserted; a removal first dropped its target's queued update.
    Queued,
    /// A change of the same kind for the same member is already queued: the
    /// same member removed by two paths, an invitation racing a sponsored
    /// join.
    Duplicate,
    /// Already in effect, or impossible, for `members` (an
    /// invite for a seated member, a removal of an absent one), or it is an
    /// update whose owner has a removal queued.
    Stale,
}

impl EngineQueues {
    // ─────────────────────────── Approved Proposals ───────────────────────────

    pub fn approved_proposals_count(&self) -> usize {
        self.approved_proposals.len()
    }

    pub fn approved_proposals(&self) -> &IndexMap<ProposalId, ConversationUpdateRequest> {
        &self.approved_proposals
    }

    /// The approved queue as the wire and the snapshot carry it, FIFO.
    pub fn approved_as_wire(&self) -> Vec<ApprovedProposal> {
        self.approved_proposals
            .iter()
            .map(|(&proposal_id, request)| ApprovedProposal {
                proposal_id,
                request: Some(request.clone()),
            })
            .collect()
    }

    /// Member ids targeted by an in-flight proposal — either still voting or
    /// already approved. `initiate_proposal` no-ops rather than opening a
    /// second session for a target already covered by a live proposal.
    pub fn active_proposal_targets(&self) -> HashSet<Vec<u8>> {
        let voting = self
            .voting_proposals
            .values()
            .filter_map(|m| m.target.clone());
        let approved = self
            .approved_proposals
            .values()
            .filter_map(target_member_id_of);
        voting.chain(approved).collect()
    }

    /// True if `member_id` has a self-leave waiting for the next commit —
    /// an approved `RemoveMember(member_id)` under the deterministic
    /// self-leave ID. Used by live rotation to skip the leaver.
    pub fn is_pending_self_leave(&self, member_id: &[u8]) -> bool {
        let pid = single_voter_proposal_id(member_id);
        self.approved_proposals
            .get(&pid)
            .is_some_and(|req| change_of(req) == Some((ActionKind::Remove, member_id)))
    }

    /// Remove the approved entry `proposal_id` when it is a `LeafUpdate`;
    /// `true` when one was removed.
    pub fn drop_approved_update(&mut self, proposal_id: ProposalId) -> bool {
        let is_update = self
            .approved_proposals
            .get(&proposal_id)
            .is_some_and(|req| matches!(change_of(req), Some((ActionKind::Update, _))));
        is_update && self.approved_proposals.shift_remove(&proposal_id).is_some()
    }

    /// True iff `approved_proposals` already holds a change of `kind` for
    /// `member_id`, whatever its proposal id. A queued `Remove` of a member
    /// also makes it ineligible as steward: MLS forbids a member from
    /// committing its own removal. Broader than
    /// [`Self::is_pending_self_leave`].
    pub fn has_approved_change(&self, kind: ActionKind, member_id: &[u8]) -> bool {
        self.approved_proposals
            .values()
            .any(|req| change_of(req) == Some((kind, member_id)))
    }

    /// Remove every approved `LeafUpdate` owned by `member_id`.
    pub fn drop_approved_update_for(&mut self, member_id: &[u8]) {
        self.approved_proposals
            .retain(|_pid, req| change_of(req) != Some((ActionKind::Update, member_id)));
    }

    /// A commit from `sender` is valid when every action it carries is allowed
    /// this round — the urgent target's removal alone, or any approved add,
    /// removal or update except the sender's own update, which the commit
    /// carries in its path — and it carries something: an action, or that own
    /// update. Approved work it leaves out is not a fault.
    pub fn actions_within_approved(&self, sender: &[u8], actions: &[Action]) -> bool {
        let urgent = self.urgent_commit_target();
        let allowed: Vec<(ActionKind, Vec<u8>)> = match urgent {
            Some(target) => vec![(ActionKind::Remove, target.to_vec())],
            None => self
                .approved_proposals()
                .values()
                .filter_map(change_of)
                .map(|(kind, member)| (kind, member.to_vec()))
                .filter(|(kind, member)| !(*kind == ActionKind::Update && member == sender))
                .collect(),
        };
        (!actions.is_empty() || self.commit_carries_update_of(sender))
            && actions
                .iter()
                .all(|a| allowed.contains(&(a.kind(), a.member().as_bytes().to_vec())))
    }

    /// True iff a commit `sender` builds now carries its own approved update:
    /// the group puts it in the commit's path, never in an action, and an
    /// urgent round carries the target's removal alone.
    pub fn commit_carries_update_of(&self, sender: &[u8]) -> bool {
        self.urgent_commit_target().is_none()
            && self.has_approved_change(ActionKind::Update, sender)
    }

    /// Offer an approved change to the queue: the one admission for a vote
    /// that ends here and for pending work a sync installs, so the queue
    /// never holds a pair the group refuses.
    pub(crate) fn admit_approved(
        &mut self,
        members: &[Vec<u8>],
        proposal_id: ProposalId,
        request: ConversationUpdateRequest,
    ) -> Admission {
        let Some((kind, member)) = change_of(&request) else {
            return Admission::Stale;
        };
        if self.has_approved_change(kind, member) {
            return Admission::Duplicate;
        }
        if change_in_effect(members, kind, member)
            || (kind == ActionKind::Update && self.has_approved_change(ActionKind::Remove, member))
        {
            return Admission::Stale;
        }
        if kind == ActionKind::Remove {
            let member = member.to_vec();
            self.drop_approved_update_for(&member);
        }
        self.insert_approved_proposal(proposal_id, request);
        Admission::Queued
    }

    /// Insert a proposal straight into the approved queue, staged for commit.
    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: ConversationUpdateRequest,
    ) {
        self.approved_proposals.insert(proposal_id, proposal);
    }

    /// Drop the approved work a merge landed: invites whose member is in
    /// `delta.added`, removals whose member is in `delta.removed`, and every
    /// update (epoch-bound). The rest stays approved for the next commit.
    pub fn drop_landed(&mut self, delta: &MembershipDelta) {
        self.approved_proposals
            .retain(|_pid, req| match change_of(req) {
                Some((ActionKind::Add, member)) => !delta.added.iter().any(|m| m == member),
                Some((ActionKind::Remove, member)) => !delta.removed.iter().any(|m| m == member),
                Some((ActionKind::Update, _)) | None => false,
            });
    }

    // ─────────────────────────── Voting Proposals ───────────────────────────

    /// Record `proposal_id` as in flight, whoever opened it. Idempotent: a
    /// later sighting (e.g. a peer echo of our own proposal) leaves the first
    /// entry untouched.
    pub fn track_voting_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: &ConversationUpdateRequest,
        epoch: u64,
    ) {
        self.voting_proposals
            .entry(proposal_id)
            .or_insert_with(|| VotingMeta {
                target: in_flight_target(proposal),
                kind: ProposalKind::of(proposal),
                epoch,
            });
    }

    /// The epoch the in-flight proposal `proposal_id` was sealed at.
    pub fn voting_epoch(&self, proposal_id: ProposalId) -> Option<u64> {
        self.voting_proposals.get(&proposal_id).map(|m| m.epoch)
    }

    /// True while `proposal_id` is in flight (in the voting queue).
    #[cfg(test)]
    pub fn is_voting(&self, proposal_id: ProposalId) -> bool {
        self.voting_proposals.contains_key(&proposal_id)
    }

    /// Drop a proposal from the voting queue (resolved, rejected, or deduped).
    pub fn remove_voting_proposal(&mut self, proposal_id: ProposalId) {
        self.voting_proposals.remove(&proposal_id);
    }

    /// Cheap idempotence check for auto-retry: don't submit a second election
    /// while one — own or a peer's — is still being voted on.
    pub fn has_election_in_flight(&self) -> bool {
        self.voting_proposals
            .values()
            .any(|m| m.kind.is_steward_election())
    }

    // ─────────────────────────── Emergency (RFC §Partial Freeze) ───────────────────────────

    /// An emergency criteria proposal is in flight (partial freeze).
    pub fn has_active_emergency(&self) -> bool {
        self.voting_proposals
            .values()
            .any(|m| m.kind.is_emergency())
    }

    /// RFC §Partial Freeze: while an emergency is active, proposals of
    /// strictly lower priority MUST be blocked. Returns `true` when `kind`
    /// should be rejected under the current freeze state.
    pub fn partial_freeze_blocks(&self, kind: ProposalKind) -> bool {
        self.has_active_emergency() && kind < ProposalKind::Emergency
    }
}
