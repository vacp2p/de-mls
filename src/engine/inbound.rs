//! The control-message side of the engine: one `ControlMessage` the router
//! opened, decoded and dispatched.
//!
//! The router hands over the MLS-authenticated sender and the epoch the
//! message was sealed at; nothing is read from the payload to decide who
//! sent it. A failure inside dispatch is reported as
//! [`Event::Error`](crate::engine::Event::Error) and the call still returns
//! its [`Output`], so one bad message never fails the driving call. A refusal
//! is returned as `Err` and leaves nothing behind.

use prost::Message;

use crate::{
    ConversationError,
    engine::{
        EngineStore,
        handle::Engine,
        types::{MemberId, Output, Timestamp},
    },
    protos::de_mls::messages::v1::{ControlMessage, control_message},
};

impl<St: EngineStore> Engine<St> {
    /// A `ControlMessage` the router opened. `sender` and `epoch` come from
    /// the MLS layer, never from the payload.
    pub fn handle_control(
        &mut self,
        now: Timestamp,
        sender: MemberId,
        epoch: u64,
        payload: &[u8],
    ) -> Result<Output, ConversationError> {
        self.begin(now);
        let message = match ControlMessage::decode(payload) {
            Ok(message) => message,
            Err(e) => {
                self.report_failure("control_decode", &e.into());
                return self.finish();
            }
        };
        let sender = sender.as_bytes().to_vec();
        let outcome = match message.payload {
            Some(control_message::Payload::Proposal(proposal)) => (
                "incoming_proposal",
                self.on_incoming_proposal(&sender, proposal, epoch),
            ),
            Some(control_message::Payload::Vote(vote)) => {
                ("incoming_vote", self.forward_incoming_vote(&sender, vote))
            }
            Some(control_message::Payload::ConversationSync(sync)) => {
                ("conversation_sync", self.on_conversation_sync(sync))
            }
            Some(control_message::Payload::ConversationSyncRequest(_)) => {
                self.on_conversation_sync_request(&sender);
                ("conversation_sync_request", Ok(()))
            }
            None => {
                tracing::debug!(
                    conversation = %self.conversation_id,
                    sender = ?sender,
                    sealed_at_epoch = epoch,
                    "control message carried no payload"
                );
                return self.finish();
            }
        };
        match outcome {
            (_, Err(ConversationError::Refused(refusal))) => {
                Err(ConversationError::Refused(refusal))
            }
            (operation, Err(e)) => {
                self.report_failure(operation, &e);
                self.finish()
            }
            (_, Ok(())) => self.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Refusal,
        engine::{
            InMemoryStore,
            consensus::{MemberSigner, wire::control_bytes},
            store::keys,
            test_support::*,
            types::Event,
        },
        protos::de_mls::messages::v1::{ConversationSync, ConversationUpdateRequest, TimingConfig},
    };
    use hashgraph_like_consensus::{
        protos::consensus::v1::{Proposal, Vote},
        utils::build_vote,
    };

    /// Every key the engine writes, with its bytes.
    fn store_contents(engine: &Engine<InMemoryStore>) -> Vec<(&'static str, Option<Vec<u8>>)> {
        [
            keys::STEWARD_LIST,
            keys::PROPOSALS,
            keys::JOIN_EPOCHS,
            keys::SKIPPED_STEWARDS,
            keys::SCORES,
            keys::CONSENSUS,
            keys::META,
        ]
        .into_iter()
        .map(|key| (key, engine.store().get(key).expect("in-memory store")))
        .collect()
    }

    fn vote_bytes(proposal_id: u32, owner: Vec<u8>) -> Vec<u8> {
        control_bytes(control_message::Payload::Vote(Vote {
            proposal_id,
            vote: true,
            vote_owner: owner,
            ..Default::default()
        }))
    }

    /// A vote for a session this node does not hold is refused and the store
    /// is untouched.
    #[test]
    fn a_vote_for_an_unknown_session_is_refused_without_trace() {
        let mut engine = founder();
        let before = store_contents(&engine);
        let result = engine.handle_control(
            Timestamp::ZERO,
            id("bob"),
            1,
            &vote_bytes(99, member("bob")),
        );
        assert!(matches!(
            result,
            Err(ConversationError::Refused(Refusal::UnknownSession {
                proposal_id: 99
            }))
        ));
        assert_eq!(store_contents(&engine), before);
    }

    /// The same vote fed twice to a node holding the session: the second
    /// is dropped and neither call reports an error.
    #[test]
    fn a_duplicate_vote_is_dropped_without_an_error() {
        let mut engine = founder();
        let proposal = Proposal {
            proposal_id: 7,
            proposal_owner: member("bob"),
            payload: ConversationUpdateRequest::remove_member(member("bob")).encode_to_vec(),
            expected_voters_count: 2,
            round: 1,
            expiration_timestamp: 600,
            liveness_criteria_yes: true,
            ..Default::default()
        };
        let vote = build_vote(&proposal, true, &MemberSigner::new(member("bob")), 0).unwrap();
        let bytes = control_bytes(control_message::Payload::Proposal(proposal));
        engine
            .handle_control(Timestamp::ZERO, id("bob"), 1, &bytes)
            .expect("proposal");
        let vote = control_bytes(control_message::Payload::Vote(vote));
        for _ in 0..2 {
            let out = engine
                .handle_control(Timestamp::ZERO, id("bob"), 1, &vote)
                .expect("an Ok both times");
            assert!(
                !out.events.iter().any(|e| matches!(e, Event::Error { .. })),
                "{:?}",
                out.events
            );
        }
    }

    /// Undecodable bytes are reported, not returned: the call still hands back
    /// an `Output`.
    #[test]
    fn undecodable_control_is_reported() {
        let mut engine = joiner();
        let out = engine
            .handle_control(Timestamp::ZERO, id("alice"), 1, &[0xff; 8])
            .expect("handle control");
        assert!(matches!(out.events.as_slice(), [Event::Error { .. }]));
    }

    /// A sync that fails validation is refused without an error and without
    /// installing a list.
    #[test]
    fn an_invalid_sync_is_refused_quietly() {
        let mut engine = joiner();
        let sync = ConversationSync {
            steward_members: vec![member("ghost")],
            election_epoch: 1,
            sn_min: 1,
            sn_max: 5,
            peer_scores: vec![],
            timing: Some(TimingConfig::from(engine.config())),
            retry_round: 0,
            liveness_criteria_yes: true,
            join_epochs: vec![],
            default_peer_score: 100,
            approved_proposals: vec![],
            voting_sessions: vec![],
        };
        let payload = control_bytes(control_message::Payload::ConversationSync(sync));
        let out = engine
            .handle_control(Timestamp::ZERO, id("alice"), 1, &payload)
            .expect("handle control");
        assert!(out.events.is_empty());
        assert!(!engine.is_synced());
    }
}
