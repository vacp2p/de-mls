//! The approved and in-flight (voting) proposal queues, and the active
//! emergency criteria set that drives RFC §Partial Freeze.

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::{
    engine::{
        proposal_kind::ProposalKind,
        queues::{EngineQueues, ProposalId, VotingMeta},
        util::{in_flight_target, self_leave_proposal_id, target_member_id_of},
    },
    protos::de_mls::messages::v1::{ConversationUpdateRequest, conversation_update_request},
};

impl EngineQueues {
    // ─────────────────────────── Approved Proposals ───────────────────────────

    pub fn approved_proposals_count(&self) -> usize {
        self.approved_proposals.len()
    }

    pub fn approved_proposals(&self) -> &IndexMap<ProposalId, ConversationUpdateRequest> {
        &self.approved_proposals
    }

    /// Member ids targeted by an in-flight proposal — either still voting or
    /// already approved. A buffered update whose target is in this set is
    /// already covered by a live proposal and must not be re-proposed.
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

    /// True iff `approved_proposals` carries any `RemoveMember(member_id)`,
    /// regardless of source. Used by `steward_eligibility` to skip a member
    /// whose removal is queued — MLS forbids them from committing it
    /// themselves. Broader than [`Self::is_pending_self_leave`].
    pub fn has_approved_removal(&self, member_id: &[u8]) -> bool {
        self.approved_proposals
            .values()
            .any(|req| removes_member(req, member_id))
    }

    /// True iff `approved_proposals` already admits `member_id`. Mirrors
    /// [`Self::has_approved_removal`]: an invitation and a sponsored join
    /// request can admit the same member under different proposal ids, and
    /// their key packages need not match — so MLS cannot collapse them into one
    /// proposal and rejects the whole batch.
    pub fn has_approved_invite(&self, member_id: &[u8]) -> bool {
        self.approved_proposals
            .values()
            .any(|req| invites_member(req, member_id))
    }

    /// True if `member_id` has a self-leave waiting for the next commit —
    /// an approved `RemoveMember(member_id)` under the deterministic
    /// self-leave ID. Used by live rotation to skip the leaver.
    pub fn is_pending_self_leave(&self, member_id: &[u8]) -> bool {
        let pid = self_leave_proposal_id(member_id);
        self.approved_proposals
            .get(&pid)
            .is_some_and(|req| removes_member(req, member_id))
    }

    /// Insert a proposal straight into the approved queue, staged for commit.
    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: ConversationUpdateRequest,
    ) {
        self.approved_proposals.insert(proposal_id, proposal);
    }

    /// Clear the approved-proposal queue and return the cleared batch in
    /// FIFO insertion order so callers can archive it for UI / diagnostic
    /// history. Returns an empty `Vec` when the queue was already empty.
    pub fn drain_approved_proposals(&mut self) -> Vec<ConversationUpdateRequest> {
        self.approved_proposals
            .drain(..)
            .map(|(_, req)| req)
            .collect()
    }

    /// Drop every `RemoveMember(target)` entry; other approvals stay
    /// queued for the next normal cycle.
    pub fn drop_approved_removals_for(&mut self, target: &[u8]) {
        self.approved_proposals
            .retain(|_pid, req| !removes_member(req, target));
    }

    // ─────────────────────────── Voting Proposals ───────────────────────────

    /// Record `proposal_id` as in flight, whoever opened it. Idempotent: a
    /// later sighting (e.g. a peer echo of our own proposal) leaves the first
    /// entry untouched.
    pub fn track_voting_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: &ConversationUpdateRequest,
    ) {
        self.voting_proposals
            .entry(proposal_id)
            .or_insert_with(|| VotingMeta {
                target: in_flight_target(proposal),
                kind: ProposalKind::of(proposal),
            });
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

    /// Record an active emergency criteria proposal.
    pub fn insert_emergency(&mut self, proposal_id: ProposalId) {
        self.active_emergency_ids.insert(proposal_id);
    }

    /// Mark an emergency criteria proposal as finalized.
    pub fn remove_emergency(&mut self, proposal_id: ProposalId) {
        self.active_emergency_ids.remove(&proposal_id);
    }

    /// Check if any emergency criteria proposal is active (partial freeze).
    pub fn has_active_emergency(&self) -> bool {
        !self.active_emergency_ids.is_empty()
    }

    /// RFC §Partial Freeze: while an emergency is active, proposals of
    /// strictly lower priority MUST be blocked. Returns `true` when `kind`
    /// should be rejected under the current freeze state.
    pub fn partial_freeze_blocks(&self, kind: ProposalKind) -> bool {
        self.has_active_emergency() && kind < ProposalKind::Emergency
    }
}

/// True iff `req` is a `RemoveMember` targeting `member_id`.
fn removes_member(req: &ConversationUpdateRequest, member_id: &[u8]) -> bool {
    matches!(
        req.payload.as_ref(),
        Some(conversation_update_request::Payload::RemoveMember(r)) if r.member_id == member_id
    )
}

/// True when `req` admits `member_id`, whatever key package it carries.
fn invites_member(req: &ConversationUpdateRequest, member_id: &[u8]) -> bool {
    matches!(
        req.payload.as_ref(),
        Some(conversation_update_request::Payload::MemberInvite(_))
    ) && target_member_id_of(req).is_some_and(|target| target == member_id)
}
