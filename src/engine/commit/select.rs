//! Round selection: the eligibility-filtered epoch steward lookup, the
//! validation ladder that accepts only that steward's candidate, and the
//! score-op helpers the ladder's verdicts carry.
//!
//! Only the epoch steward's candidate may win a round (design 11): every
//! other buffered candidate is rejected and scored, whatever it carries.

use tracing::warn;

use crate::{
    ConversationError, ScoreEvent, ScoreOp,
    engine::{
        commit::buffer::BufferedCandidate,
        handle::{Engine, PendingMerge},
        queues::EngineQueues,
        store::EngineStore,
        types::{Action, CommitHash, Decision, Event, MemberId, StagedFacts},
    },
    protos::de_mls::messages::v1::{
        ConversationUpdateRequest, conversation_update_request::Payload,
    },
};

/// The winner of one round and what the same decision discards.
pub(crate) struct RoundWinner {
    pub(crate) hash: CommitHash,
    pub(crate) facts: Option<StagedFacts>,
    pub(crate) is_local: bool,
    pub(crate) losers: Vec<CommitHash>,
}

/// The validation ladder's answer for one candidate.
pub(crate) enum CandidateVerdict {
    /// Merge it.
    Accept,
    /// Skip to the next, penalising its author when the failure is one.
    Reject(Option<ScoreOp>),
}

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

    /// Walk the round's candidates and accept the epoch steward's alone.
    /// Every other candidate — a different author, or an author claiming the
    /// role with a fork — is scored `MisbehavingCommit` and discarded. The
    /// epoch steward's candidate goes through the validation ladder (wire
    /// claims against the authenticated commit, actions against the voted
    /// set): valid scores `SuccessfulCommit` and wins; invalid scores its
    /// specific violation and is discarded like everyone else.
    pub(crate) fn select_epoch_steward_candidate(
        &mut self,
    ) -> Result<Option<RoundWinner>, ConversationError> {
        let expected = self.expected_steward();
        let candidates = self.round.candidates(self.epoch).to_vec();

        let mut ops: Vec<ScoreOp> = Vec::new();
        let mut losers: Vec<CommitHash> = Vec::new();
        let mut winner: Option<BufferedCandidate> = None;

        for candidate in candidates {
            let author = candidate.author(&self.own);
            if expected.as_deref() != Some(author.as_slice()) || winner.is_some() {
                ops.push(penalty(&author, ScoreEvent::MisbehavingCommit));
                losers.push(candidate.hash);
                continue;
            }
            match self.verdict(&candidate) {
                CandidateVerdict::Accept => {
                    ops.push(penalty(&author, ScoreEvent::SuccessfulCommit));
                    winner = Some(candidate);
                }
                CandidateVerdict::Reject(penalty) => {
                    ops.extend(penalty);
                    losers.push(candidate.hash);
                }
            }
        }

        if !ops.is_empty() {
            self.apply_score_ops(&ops);
        }

        Ok(winner.map(|w| RoundWinner {
            hash: w.hash,
            facts: w.facts,
            is_local: w.is_local,
            losers,
        }))
    }

    /// Queue the decisions one winner needs: discard the losers, then merge.
    pub(crate) fn decide_winner(&mut self, winner: RoundWinner) {
        self.pending_merge = Some(PendingMerge {
            hash: winner.hash,
            facts: winner.facts,
            is_local: winner.is_local,
        });
        if !winner.losers.is_empty() {
            self.decide(Decision::Discard {
                hashes: winner.losers,
            });
        }
        self.decide(Decision::Merge { hash: winner.hash });
    }

    /// The validation ladder for the epoch steward's candidate, over the
    /// facts the router staged: wire claims against the authenticated
    /// commit, the actions against the voted set.
    pub(crate) fn verdict(&self, candidate: &BufferedCandidate) -> CandidateVerdict {
        // Our own candidate is valid by construction: we built it from the
        // approved batch and the router staged nothing to contradict.
        if candidate.is_local {
            return CandidateVerdict::Accept;
        }
        let Some(facts) = candidate.facts.as_ref() else {
            return CandidateVerdict::Reject(None);
        };
        let sender = facts.sender.as_bytes();

        // The wire-claimed steward must be the member whose signature the
        // commit carries; a mismatch is attributed to the actual signer.
        if sender != candidate.envelope.steward_member_id.as_slice() {
            warn!(
                conversation = %self.conversation_id,
                "violation: wire steward_member_id doesn't match the commit sender"
            );
            return CandidateVerdict::Reject(Some(penalty(sender, ScoreEvent::BrokenCommit)));
        }
        // The wire-claimed count is checked against the staged commit.
        if facts.proposal_count != candidate.envelope.proposal_count {
            warn!(
                conversation = %self.conversation_id,
                claimed = candidate.envelope.proposal_count,
                staged = facts.proposal_count,
                "violation: claimed proposal count doesn't match the commit"
            );
            return CandidateVerdict::Reject(Some(penalty(sender, ScoreEvent::BrokenCommit)));
        }
        // More proposals than actions means the commit carries kinds the
        // engine has no vocabulary for, so the voted set can't be checked.
        if facts.proposal_count as usize != facts.actions.len() {
            warn!(
                conversation = %self.conversation_id,
                count = facts.proposal_count,
                actions = facts.actions.len(),
                "violation: commit carries proposals the round cannot read"
            );
            return CandidateVerdict::Reject(Some(penalty(sender, ScoreEvent::BrokenMlsProposal)));
        }
        if !actions_match_voted(&self.queues, &facts.actions) {
            warn!(
                conversation = %self.conversation_id,
                actions = ?facts.actions,
                "violation: commit actions don't match the voted proposals"
            );
            return CandidateVerdict::Reject(Some(penalty(sender, ScoreEvent::BrokenMlsProposal)));
        }
        CandidateVerdict::Accept
    }

    /// No candidate validated: drop what the round collected and report that
    /// the epoch steward missed its window. The approved batch stays queued
    /// — the router asks again on the next inactivity tick, or after
    /// [`Engine::request_recovery`] skips the silent steward.
    pub(crate) fn close_round_without_commit(&mut self) -> Result<(), ConversationError> {
        let expected = self.expected_steward();

        let hashes = self.round.hashes(self.epoch);
        self.round.clear();
        if !hashes.is_empty() {
            self.decide(Decision::Discard { hashes });
        }

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
fn penalty(member_id: &[u8], event: ScoreEvent) -> ScoreOp {
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
