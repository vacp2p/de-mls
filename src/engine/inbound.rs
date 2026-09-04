//! The control-message side of the engine: one `ControlMessage` the router
//! opened, decoded and dispatched.
//!
//! The router hands over the MLS-authenticated sender and the epoch the
//! message was sealed at; nothing is read from the payload to decide who
//! sent it. A failure inside dispatch is reported as
//! [`Event::Error`](crate::engine::Event::Error) and the call still returns
//! its [`Output`], so one bad message never fails the driving call.

use prost::Message;

use crate::{
    ConversationError,
    engine::{
        handle::Engine,
        store::EngineStore,
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
                self.on_incoming_proposal(&sender, proposal),
            ),
            Some(control_message::Payload::Vote(vote)) => {
                ("incoming_vote", self.forward_incoming_vote(&sender, vote))
            }
            Some(control_message::Payload::ConversationSync(sync)) => {
                ("conversation_sync", self.on_conversation_sync(sync))
            }
            Some(control_message::Payload::ConversationSyncRequest(_)) => (
                "conversation_sync_request",
                self.on_conversation_sync_request(&sender),
            ),
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
        if let (operation, Err(e)) = outcome {
            self.report_failure(operation, &e);
        }
        self.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::{steward::control_bytes, test_support::*, types::Event},
        protos::de_mls::messages::v1::{ConversationSync, TimingConfig},
    };

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
            unsettled_members: vec![],
            default_peer_score: 100,
        };
        let payload = control_bytes(control_message::Payload::ConversationSync(sync));
        let out = engine
            .handle_control(Timestamp::ZERO, id("alice"), 1, &payload)
            .expect("handle control");
        assert!(out.events.is_empty());
        assert!(!engine.is_synced());
    }
}
