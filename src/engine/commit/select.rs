//! Round selection: the eligibility-filtered epoch steward lookup and the
//! close that ends one round.
//!
//! Only the epoch steward's candidate ever reaches [`Engine::round_candidate`]:
//! [`Engine::handle_candidate`] discards and scores anyone
//! else's on sight. `close_round` validates that one candidate against the
//! voted set, or reports the miss when none arrived.

use crate::{
    ConversationError, ScoreEvent, ScoreOp,
    engine::{
        handle::{Engine, PendingMerge},
        queues::EngineQueues,
        store::EngineStore,
        types::{Action, Decision, Event, MemberId},
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, conversation_update_request::Payload,
    },
};

/// The membership change an action performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ActionKind {
    Add,
    Remove,
}

impl<St: EngineStore> Engine<St> {
    /// The eligibility-filtered epoch steward for the current epoch — the
    /// only member whose candidate this round may accept.
    pub(crate) fn expected_steward(&self) -> Option<Vec<u8>> {
        let eligible = self.queues.steward_eligibility(&self.members);
        self.steward_list
            .epoch_steward(self.epoch, &eligible)
            .map(<[u8]>::to_vec)
    }

    /// Close the round: validate the epoch steward's candidate against the
    /// voted set, or report the miss when none arrived this round.
    ///
    /// A valid candidate scores `SuccessfulCommit` for its sender and asks
    /// the router to merge it. An invalid one — a proposal count that
    /// contradicts the staged commit, or actions that don't match exactly
    /// what was voted — scores `BrokenMlsProposal`, asks the router to
    /// discard it, then falls through to the same miss the round reports
    /// when no candidate arrived at all.
    pub(crate) fn close_round(&mut self) -> Result<(), ConversationError> {
        let Some((hash, facts)) = self.round_candidate.take() else {
            return self.close_round_without_commit();
        };
        let sender = facts.sender.as_bytes().to_vec();
        let valid = facts.proposal_count as usize == facts.actions.len()
            && actions_match_voted(&self.queues, &facts.actions);
        if !valid {
            self.apply_score_ops(&[penalty(&sender, ScoreEvent::BrokenMlsProposal)]);
            self.decide(Decision::Discard { hashes: vec![hash] });
            return self.close_round_without_commit();
        }

        self.apply_score_ops(&[penalty(&sender, ScoreEvent::SuccessfulCommit)]);
        let is_local = sender == self.own;
        self.pending_merge = Some(PendingMerge {
            hash,
            facts: (!is_local).then_some(facts),
            is_local,
        });
        self.decide(Decision::Merge { hash });
        Ok(())
    }

    /// No candidate validated this round, or none arrived: report the miss
    /// and resume `Working` with the approved batch still queued — the
    /// router asks again on the next inactivity tick, or after
    /// [`Engine::request_recovery`] skips the silent steward.
    pub(crate) fn close_round_without_commit(&mut self) -> Result<(), ConversationError> {
        let expected = self.expected_steward();

        if let Some(steward) = expected.as_deref()
            && steward != self.own.as_slice()
        {
            self.apply_score_ops(&[ScoreOp {
                member_id: steward.to_vec(),
                event: ScoreEvent::CensorshipInactivity,
            }]);
        }

        self.emit(Event::CommitMissing {
            epoch: self.epoch,
            steward: expected.map(|s| MemberId::from(s.as_slice())),
        });

        let resumed = self.start_working();
        self.emit_phase(Some(resumed));
        Ok(())
    }
}

/// A penalty against the authenticated author of a candidate.
pub(crate) fn penalty(member_id: &[u8], event: ScoreEvent) -> ScoreOp {
    ScoreOp {
        member_id: member_id.to_vec(),
        event,
    }
}

/// Whether a commit's actions are exactly the membership changes the group
/// voted to approve.
///
/// Both sides are compared as sorted, deduplicated `(kind, member)`
/// projections: any member may open a proposal, so several approvals can name
/// the same change while the commit carries it once.
pub(crate) fn actions_match_voted(queues: &EngineQueues, actions: &[Action]) -> bool {
    let mut expected: Vec<(ActionKind, Vec<u8>)> = queues
        .approved_proposals()
        .values()
        .filter_map(voted_projection)
        .collect();
    let mut actual: Vec<(ActionKind, Vec<u8>)> = actions.iter().map(staged_projection).collect();
    expected.sort();
    expected.dedup();
    actual.sort();
    actual.dedup();
    expected == actual
}

/// `(kind, member)` projection of an approved request: an add keys on the
/// joiner's id as the proposer read it from the key package, a remove on its
/// target. `None` for governance payloads, which never reach a commit.
fn voted_projection(req: &ConversationUpdateRequest) -> Option<(ActionKind, Vec<u8>)> {
    match req.payload.as_ref()? {
        Payload::MemberInvite(invite) => Some((ActionKind::Add, invite.member_id.clone())),
        Payload::RemoveMember(remove) => Some((ActionKind::Remove, remove.member_id.clone())),
        _ => None,
    }
}

/// `(kind, member)` projection of a staged action, matching
/// [`voted_projection`]: one key space before and after a member is seated.
fn staged_projection(action: &Action) -> (ActionKind, Vec<u8>) {
    match action {
        Action::Add { member, .. } => (ActionKind::Add, member.as_bytes().to_vec()),
        Action::Remove { member } => (ActionKind::Remove, member.as_bytes().to_vec()),
    }
}
