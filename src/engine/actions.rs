//! What the application asks of the conversation: proposing a member in or
//! out, updating its own leaf, voting, leaving, and the two recovery-side
//! requests.
//!
//! Each action begins the driving call, does its work, and returns the
//! [`Output`] it produced. A refusal is an `Err` and leaves the engine
//! exactly as it was.

use crate::{
    ConversationError,
    engine::{
        EngineStore,
        handle::Engine,
        types::{MemberId, Output, Timestamp},
    },
    protos::de_mls::messages::v1::{ConversationUpdateRequest, MemberInvite},
    traits::Publish,
};

impl<St: EngineStore> Engine<St> {
    /// Propose adding `member`, whose key package the router validated.
    /// Proposing is this member's own YES, so no vote request comes back.
    ///
    /// `PartialFreeze` means an emergency proposal is in flight; the engine
    /// keeps no memory of the attempt, and filing again is the caller's.
    pub fn propose_add(
        &mut self,
        now: Timestamp,
        member: MemberId,
        key_package: Vec<u8>,
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        if self.is_member(member.as_bytes()) {
            return Err(ConversationError::AlreadyMember);
        }
        self.initiate_proposal(invite(&member, key_package))?;
        self.finish(publisher)
    }

    /// Propose removing `member`. The requester's intent is the removal, so
    /// its YES is bundled at submit.
    pub fn propose_remove(
        &mut self,
        now: Timestamp,
        member: MemberId,
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        if !self.is_member(member.as_bytes()) {
            return Err(ConversationError::MemberGone);
        }
        self.initiate_proposal(ConversationUpdateRequest::remove_member(
            member.as_bytes().to_vec(),
        ))?;
        self.finish(publisher)
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
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        self.file_score_removal_ecp(member.as_bytes())?;
        self.finish(publisher)
    }

    /// Leave the conversation. The engine stays live until a commit merges
    /// the removal; the application sees the removal in that commit.
    pub fn leave(
        &mut self,
        now: Timestamp,
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        self.initiate_self_leave()?;
        self.finish(publisher)
    }

    /// File `update`, the MLS Update proposal this member signed for its own
    /// leaf. It is approved at once and carried by the epoch steward's next
    /// commit; a second call in the epoch is a no-op.
    pub fn propose_update(
        &mut self,
        now: Timestamp,
        update: Vec<u8>,
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        self.initiate_leaf_update(update)?;
        self.finish(publisher)
    }

    /// Cast this member's vote on `proposal_id`, replacing the auto-vote that
    /// would otherwise fire.
    pub fn vote(
        &mut self,
        now: Timestamp,
        proposal_id: u32,
        approve: bool,
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        self.cancel_auto_vote(proposal_id);
        self.cast_local_vote(proposal_id, approve)?;
        self.finish(publisher)
    }

    /// Ask the stewards for a `ConversationSync`. The engine asks once on its
    /// own, when added, at a stale restart and at an adoption; every other ask
    /// is the router's, on its own clock, in any phase: while it is still not
    /// synced, after an offline stretch, after
    /// [`Event::CandidateRejected`](crate::engine::Event::CandidateRejected).
    /// An answer as new as this node's list or newer is adopted and reported
    /// as [`Event::SyncApplied`](crate::engine::Event::SyncApplied); an older
    /// one is ignored.
    pub fn request_sync(
        &mut self,
        now: Timestamp,
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        self.broadcast_sync_request();
        self.finish(publisher)
    }

    /// File the deadlock emergency proposal: on ⌈2n/3⌉ YES the silent epoch
    /// steward is skipped for this epoch and the next eligible steward on the
    /// list takes over. Any member may call it, any time approved work is
    /// waiting for a commit.
    pub fn request_recovery(
        &mut self,
        now: Timestamp,
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        self.file_deadlock_ecp()?;
        self.finish(publisher)
    }

    /// Elect a fresh steward list now, even if the current one still
    /// nominally covers the epoch — for when `CommitMissing` keeps reporting
    /// `steward: None` because every remaining steward has been skipped.
    pub fn propose_election(
        &mut self,
        now: Timestamp,
        publisher: &mut impl Publish,
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        self.initiate_steward_election()?;
        self.finish(publisher)
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
        test_support::{creator, id},
        types::{ActionKind, Event, Phase},
    };
    use crate::traits::InMemoryPublisher;

    /// Adding someone already seated is refused, and nothing moves.
    #[test]
    fn propose_add_refuses_an_existing_member() {
        let mut publisher = InMemoryPublisher::default();
        let mut engine = creator();
        assert!(matches!(
            engine.propose_add(Timestamp::ZERO, id("alice"), b"kp".to_vec(), &mut publisher),
            Err(ConversationError::AlreadyMember)
        ));
        assert!(engine.pending_control.is_empty());
    }

    /// An add filed while a round is open lands in the approved queue with
    /// one control message out; the phase does not move.
    #[test]
    fn propose_add_lands_while_freezing() {
        let mut publisher = InMemoryPublisher::default();
        let mut engine = creator();
        engine.begin(Timestamp::ZERO);
        engine.start_freezing();
        engine
            .propose_add(Timestamp::ZERO, id("dave"), b"kp".to_vec(), &mut publisher)
            .expect("propose add");
        assert!(matches!(publisher.messages.as_slice(), [_]));
        assert_eq!(engine.phase(), Phase::Freezing);
        assert_eq!(engine.queues.approved_proposals_count(), 1);
    }

    /// A manual vote on a peer's open proposal is cast while a round is
    /// open: one control message out, the phase unchanged.
    #[test]
    fn vote_lands_while_freezing() {
        let mut publisher = InMemoryPublisher::default();
        use hashgraph_like_consensus::protos::consensus::v1::Proposal;
        use prost::Message;

        use crate::engine::test_support::{founder, member};

        let mut engine = founder();
        let request = ConversationUpdateRequest::remove_member(member("carol"));
        let proposal = Proposal {
            name: "p7".to_string(),
            payload: request.encode_to_vec(),
            proposal_id: 7,
            proposal_owner: member("bob"),
            votes: Vec::new(),
            expected_voters_count: 2,
            round: 1,
            timestamp: 0,
            expiration_timestamp: 600,
            liveness_criteria_yes: true,
        };
        engine.begin(Timestamp::ZERO);
        engine
            .on_incoming_proposal(&member("bob"), proposal, engine.epoch())
            .expect("incoming proposal");
        engine.start_freezing();
        engine.out = Output::default();

        engine
            .vote(Timestamp::ZERO, 7, true, &mut publisher)
            .expect("vote");
        assert!(matches!(publisher.messages.as_slice(), [_]));
        assert_eq!(engine.phase(), Phase::Freezing);
    }

    /// An own update lands in the approved queue at once, with one control
    /// message out; a second call in the epoch is a no-op.
    #[test]
    fn propose_update_lands_approved_and_repeats_as_a_no_op() {
        let mut publisher = InMemoryPublisher::default();
        let mut engine = creator();
        engine
            .propose_update(Timestamp::ZERO, b"upd".to_vec(), &mut publisher)
            .expect("propose update");
        assert!(matches!(publisher.messages.as_slice(), [_]));
        assert!(
            engine
                .queues
                .has_approved_change(ActionKind::Update, engine.own.as_slice())
        );

        publisher.drain();
        engine
            .propose_update(Timestamp::ZERO, b"other".to_vec(), &mut publisher)
            .expect("repeat");
        assert!(publisher.messages.is_empty());
        assert_eq!(engine.queues.approved_proposals_count(), 1);
    }

    /// Removing someone who isn't here is refused.
    #[test]
    fn propose_remove_refuses_a_stranger() {
        let mut publisher = InMemoryPublisher::default();
        let mut engine = creator();
        assert!(matches!(
            engine.propose_remove(Timestamp::ZERO, id("ghost"), &mut publisher),
            Err(ConversationError::MemberGone)
        ));
    }

    /// The router's ask goes out in any phase, `Working` included: exactly
    /// one control message.
    #[test]
    fn request_sync_asks_in_any_phase() {
        let mut publisher = InMemoryPublisher::default();
        let mut engine = creator();
        let out = engine
            .request_sync(Timestamp::ZERO, &mut publisher)
            .expect("request sync");
        assert!(matches!(publisher.messages.as_slice(), [_]));
        assert_eq!(out.wakeup, None);
    }

    /// Scoring an unknown member has nothing to file.
    #[test]
    fn score_removal_refuses_an_unscored_member() {
        let mut publisher = InMemoryPublisher::default();
        let mut engine = creator();
        assert!(matches!(
            engine.propose_score_removal(Timestamp::ZERO, id("ghost"), &mut publisher),
            Err(ConversationError::InvalidConversationUpdateRequest)
        ));
    }

    /// `propose_election` puts a steward-election proposal on the wire.
    #[test]
    fn propose_election_puts_an_election_proposal_on_the_wire() {
        let mut publisher = InMemoryPublisher::default();
        let mut engine = creator();
        engine
            .propose_election(Timestamp::ZERO, &mut publisher)
            .expect("propose election");
        assert!(
            !publisher.messages.is_empty(),
            "an election proposal goes out as a control message"
        );
    }

    /// A publisher that cannot hand the message to delivery.
    struct FailingPublisher;

    impl Publish for FailingPublisher {
        type Error = std::io::Error;

        fn publish(&mut self, _bytes: &[u8]) -> Result<(), Self::Error> {
            Err(std::io::Error::other("group closed"))
        }
    }

    /// A publish error becomes an `Event::Error`; the call still succeeds.
    #[test]
    fn a_publish_error_is_reported_and_the_call_succeeds() {
        let mut engine = creator();
        let out = engine
            .request_sync(Timestamp::ZERO, &mut FailingPublisher)
            .expect("the call succeeds");
        assert!(out.events.iter().any(|e| matches!(
            e,
            Event::Error { operation, .. } if operation == "publish"
        )));
    }

    /// While replaying, the messages a call queues are dropped.
    #[test]
    fn replaying_publishes_nothing() {
        let mut publisher = InMemoryPublisher::default();
        let mut engine = creator();
        engine.replay_until(Timestamp::from_duration_since_epoch(
            std::time::Duration::from_secs(10),
        ));
        engine
            .request_sync(Timestamp::ZERO, &mut publisher)
            .expect("request sync");
        assert!(publisher.messages.is_empty());
        assert!(engine.pending_control.is_empty());
    }
}
