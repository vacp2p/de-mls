//! What the application asks of the conversation: proposing a member in or
//! out, voting, leaving, and the two recovery-side requests.
//!
//! Each action begins the driving call, does its work, and returns the
//! [`Output`] it produced. A refusal is an `Err` and leaves the engine
//! exactly as it was.

use crate::{
    ConversationError,
    engine::{
        handle::Engine,
        store::EngineStore,
        types::{MemberId, Output, Phase, Timestamp},
    },
    protos::de_mls::messages::v1::{ConversationUpdateRequest, MemberInvite},
};

impl<St: EngineStore> Engine<St> {
    /// Propose adding `member`, whose key package the router validated.
    /// Proposing is this member's own YES, so no vote request comes back.
    ///
    /// A `ConversationBlocked` refusal means a commit round is open; the
    /// engine keeps no memory of the attempt, so it is the caller's to retry
    /// once it drains an `Event::PhaseChange(Phase::Working)`.
    pub fn propose_add(
        &mut self,
        now: Timestamp,
        member: MemberId,
        key_package: Vec<u8>,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        let state = self.phase;
        if state != Phase::Working {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }
        if self.is_member(member.as_bytes()) {
            return Err(ConversationError::AlreadyMember);
        }
        self.initiate_proposal(invite(&member, key_package))?;
        Ok(self.finish())
    }

    /// Propose removing `member`. The requester's intent is the removal, so
    /// its YES is bundled at submit.
    pub fn propose_remove(
        &mut self,
        now: Timestamp,
        member: MemberId,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        let state = self.phase;
        if state != Phase::Working {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }
        if !self.is_member(member.as_bytes()) {
            return Err(ConversationError::MemberGone);
        }
        self.initiate_proposal(ConversationUpdateRequest::remove_member(
            member.as_bytes().to_vec(),
        ))?;
        Ok(self.finish())
    }

    /// Ask the group to remove `member` because its peer score is too low.
    ///
    /// This is the emergency route, unlike the plain [`Self::propose_remove`]
    /// vote: on YES the proposal turns into a removal and commits right away
    /// instead of waiting for the inactivity timer. The engine never calls it
    /// itself — it reports score movement through
    /// [`Event::MemberScoreChanged`](crate::engine::Event::MemberScoreChanged)
    /// and deciding a score is too low is the application's call. Any member
    /// may propose, and it still needs ⌈2n/3⌉ consensus. The proposal carries
    /// this node's score for `member`, which peers weigh against their own.
    ///
    /// Fails when `member` has no score here.
    pub fn propose_score_removal(
        &mut self,
        now: Timestamp,
        member: MemberId,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        self.file_score_removal_ecp(member.as_bytes())?;
        Ok(self.finish())
    }

    /// Leave the conversation. The engine stays live until a commit merges
    /// the removal; that commit lands as
    /// [`Decision::Leave`](crate::engine::Decision::Leave).
    pub fn leave(&mut self, now: Timestamp) -> Result<Output, ConversationError> {
        self.begin(now);
        self.initiate_self_leave()?;
        Ok(self.finish())
    }

    /// Cast this member's vote on `proposal_id`, replacing the auto-vote that
    /// would otherwise fire. Refused mid-rotation, where a vote could arrive
    /// after the round it belongs to closed.
    pub fn vote(
        &mut self,
        now: Timestamp,
        proposal_id: u32,
        approve: bool,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        let state = self.phase;
        if matches!(state, Phase::Freezing | Phase::Selection) {
            return Err(ConversationError::ConversationBlocked(state.to_string()));
        }
        self.cancel_auto_vote(proposal_id);
        self.cast_local_vote(proposal_id, approve)?;
        Ok(self.finish())
    }

    /// Ask a steward for a `ConversationSync`. `tick` asks on its own
    /// whenever the local steward list is missing or exhausted, paced at
    /// `backup_takeover_window`; [`Engine::is_synced`] turns true once a sync
    /// is adopted.
    pub fn request_sync(&mut self, now: Timestamp) -> Result<Output, ConversationError> {
        self.begin(now);
        self.broadcast_sync_request();
        Ok(self.finish())
    }

    /// File the deadlock emergency proposal: on ⌈2n/3⌉ YES the silent epoch
    /// steward is skipped for this epoch and the next eligible steward on the
    /// list takes over. Any member may call it, any time approved work is
    /// waiting for a commit.
    pub fn request_recovery(&mut self, now: Timestamp) -> Result<Output, ConversationError> {
        self.begin(now);
        self.file_deadlock_ecp()?;
        Ok(self.finish())
    }

    /// Elect a fresh steward list now, even if the current one still
    /// nominally covers the epoch — for when `CommitMissing` keeps reporting
    /// `steward: None` because every remaining steward has been skipped.
    pub fn propose_election(&mut self, now: Timestamp) -> Result<Output, ConversationError> {
        self.begin(now);
        self.initiate_steward_election()?;
        Ok(self.finish())
    }
}

/// The invite proposal for `member`, carrying the key package the router
/// validated and the id it read from it.
fn invite(member: &MemberId, key_package: Vec<u8>) -> ConversationUpdateRequest {
    ConversationUpdateRequest::member_invite(MemberInvite {
        key_package_bytes: key_package,
        member_id: member.as_bytes().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{
        test_support::{creator, id, joiner},
        types::Outbound,
    };

    /// Adding someone already seated is refused, and nothing moves.
    #[test]
    fn propose_add_refuses_an_existing_member() {
        let mut engine = joiner();
        assert!(matches!(
            engine.propose_add(Timestamp::ZERO, id("alice"), b"kp".to_vec()),
            Err(ConversationError::AlreadyMember)
        ));
        assert!(engine.out.outbound.is_empty());
    }

    /// A round in progress blocks a fresh add: refused, and nothing about
    /// the engine moves. The engine keeps no memory of the attempt — it is
    /// the caller's to retry once the round closes.
    #[test]
    fn propose_add_refuses_while_freezing() {
        let mut engine = creator();
        engine.begin(Timestamp::ZERO);
        engine.start_freezing();
        assert!(matches!(
            engine.propose_add(Timestamp::ZERO, id("dave"), b"kp".to_vec()),
            Err(ConversationError::ConversationBlocked(_))
        ));
        assert_eq!(engine.phase(), Phase::Freezing);
        assert_eq!(engine.queues.approved_proposals_count(), 0);
        assert!(engine.out.outbound.is_empty());
    }

    /// Removing someone who isn't here is refused.
    #[test]
    fn propose_remove_refuses_a_stranger() {
        let mut engine = joiner();
        assert!(matches!(
            engine.propose_remove(Timestamp::ZERO, id("ghost")),
            Err(ConversationError::MemberGone)
        ));
    }

    /// A sync request goes out as one control message.
    #[test]
    fn request_sync_puts_one_request_on_the_wire() {
        let mut engine = joiner();
        let out = engine.request_sync(Timestamp::ZERO).expect("request sync");
        assert_eq!(out.outbound.len(), 1);
    }

    /// Scoring an unknown member has nothing to file.
    #[test]
    fn score_removal_refuses_an_unscored_member() {
        let mut engine = creator();
        assert!(matches!(
            engine.propose_score_removal(Timestamp::ZERO, id("ghost")),
            Err(ConversationError::InvalidConversationUpdateRequest)
        ));
    }

    /// `propose_election` puts a steward-election proposal on the wire.
    #[test]
    fn propose_election_puts_an_election_proposal_on_the_wire() {
        let mut engine = creator();
        let out = engine
            .propose_election(Timestamp::ZERO)
            .expect("propose election");
        assert!(
            out.outbound
                .iter()
                .any(|o| matches!(o, Outbound::Control(_))),
            "an election proposal goes out as a control message"
        );
    }
}
