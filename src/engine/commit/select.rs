//! Closing a round: the epoch steward lookup, and the close that merges the
//! steward's candidate when every action it carries is approved here and
//! reports the miss otherwise.

use crate::{
    ConversationError, ScoreEvent, ScoreOp,
    engine::{
        handle::{Engine, PendingMerge},
        store::EngineStore,
        types::{CandidateRejection, Decision, Event, MemberId},
    },
};

impl<St: EngineStore> Engine<St> {
    /// The eligibility-filtered epoch steward for the current epoch — the
    /// only member whose candidate this round may accept.
    pub(crate) fn expected_steward(&self) -> Option<Vec<u8>> {
        let eligible = self.queues.steward_eligibility(&self.members);
        self.steward_list
            .epoch_steward(self.epoch, &eligible)
            .map(<[u8]>::to_vec)
    }

    /// Close the round. The epoch steward's candidate merges and scores
    /// `SuccessfulCommit` when every action it carries is approved here;
    /// otherwise it is discarded and the round ends as a miss, the same as
    /// when no candidate arrived.
    pub(crate) fn close_round(&mut self) -> Result<(), ConversationError> {
        let Some((hash, facts)) = self.round_candidate.take() else {
            return self.close_round_without_commit();
        };
        let sender = facts.sender.as_bytes().to_vec();
        let valid = facts.proposal_count as usize == facts.actions.len()
            && self.queues.actions_within_approved(&sender, &facts.actions);
        if !valid {
            self.decide(Decision::Discard { hashes: vec![hash] });
            self.emit(Event::CandidateRejected {
                sender: facts.sender.clone(),
                reason: CandidateRejection::ActionsMismatch,
            });
            return self.close_round_without_commit();
        }

        self.apply_score_ops(&[ScoreOp {
            member_id: sender,
            event: ScoreEvent::SuccessfulCommit,
        }]);
        self.pending_merge = Some(PendingMerge { hash, facts });
        self.decide(Decision::Merge { hash });
        Ok(())
    }

    /// End the round as a miss: score the silent steward, report
    /// `CommitMissing` and resume `Working` with the approved queue intact.
    /// The next inactivity window opens a new round, under the next steward
    /// once [`Engine::request_recovery`] skips this one.
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
