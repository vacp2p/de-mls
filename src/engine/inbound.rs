//! The control-message side of the engine: one `ControlMessage` the router
//! opened, decoded and dispatched.
//!
//! The router hands over the MLS-authenticated sender and the epoch the
//! message was sealed at; nothing is read from the payload to decide who
//! sent it. A failure inside dispatch is reported as
//! [`Event::Error`](crate::engine::Event::Error) and the call still returns
//! its [`Output`], so one bad message never fails the driving call.

use prost::Message;
use tracing::{info, warn};

use crate::{
    ConversationError,
    engine::{
        handle::Engine,
        store::EngineStore,
        types::{MemberId, Output, Timestamp},
    },
    protos::de_mls::messages::v1::{
        ControlMessage, ConversationUpdateRequest, TypeMembershipChange, control_message,
        conversation_update_request,
    },
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
                return Ok(self.finish());
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
            Some(control_message::Payload::MembershipChange(change)) => (
                "membership_change",
                self.on_membership_change(&sender, change.member, change.change_type),
            ),
            None => {
                tracing::debug!(
                    conversation = %self.conversation_id,
                    sender = ?sender,
                    sealed_at_epoch = epoch,
                    "control message carried no payload"
                );
                return Ok(self.finish());
            }
        };
        if let (operation, Err(e)) = outcome {
            self.report_failure(operation, &e);
        }
        Ok(self.finish())
    }

    /// Adopt the `ConversationSync` a welcome carried.
    pub(crate) fn apply_bootstrap(&mut self, bootstrap: &[u8]) -> Result<(), ConversationError> {
        match ControlMessage::decode(bootstrap)?.payload {
            Some(control_message::Payload::ConversationSync(sync)) => {
                self.on_conversation_sync(sync)
            }
            _ => Err(ConversationError::InvalidConversationUpdateRequest),
        }
    }

    /// A membership change a peer put on the wire.
    ///
    /// `Remove` is a removal request against a current member: it is buffered
    /// like any other, and the responsible steward opens the vote. `Add` is
    /// the notice a member broadcasts about its own seating; it carries no key
    /// package, so it opens no invite round — the router calls
    /// [`Engine::sponsor_add`] with the key package it validated. What the
    /// notice does settle is that a buffered invite for that member is spent.
    fn on_membership_change(
        &mut self,
        sender: &[u8],
        member: Vec<u8>,
        change_type: i32,
    ) -> Result<(), ConversationError> {
        match TypeMembershipChange::try_from(change_type) {
            Ok(TypeMembershipChange::Add) => {
                if self.invite_buffered_for(&member) {
                    self.queues.remove_pending_update(&member);
                    // store: proposals (WP3g)
                }
                info!(
                    conversation = %self.conversation_id,
                    sender = ?sender,
                    member = ?member,
                    seated = self.is_member(&member),
                    "join notice received"
                );
                Ok(())
            }
            Ok(TypeMembershipChange::Remove) => {
                // A removal for someone who isn't here saves a useless
                // consensus round.
                if !self.is_member(&member) {
                    info!(
                        conversation = %self.conversation_id,
                        target = ?member,
                        "removal request skipped: target is not a member"
                    );
                    return Ok(());
                }
                self.handle_incoming_update_request(
                    sender,
                    ConversationUpdateRequest::remove_member(member),
                )
            }
            Err(_) => {
                warn!(
                    conversation = %self.conversation_id,
                    sender = ?sender,
                    change_type,
                    "membership change dropped: unknown change type"
                );
                Ok(())
            }
        }
    }

    /// Whether the pending-update buffer holds an invite for `member`. A
    /// removal buffered under the same key is not one, and stays.
    fn invite_buffered_for(&self, member: &[u8]) -> bool {
        self.queues.pending_updates().get(member).is_some_and(|p| {
            matches!(
                p.request.payload,
                Some(conversation_update_request::Payload::MemberInvite(_))
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::{
            steward::{control_bytes, test_support::*},
            types::Event,
        },
        protos::de_mls::messages::v1::{
            ConversationSync, EventMembershipChange, MemberInvite, TimingConfig,
        },
    };

    fn membership_change(member: &[u8], change: TypeMembershipChange) -> Vec<u8> {
        control_bytes(control_message::Payload::MembershipChange(
            EventMembershipChange {
                conversation_id: "conv".to_string(),
                member: member.to_vec(),
                change_type: change as i32,
            },
        ))
    }

    /// A removal request for a live member is buffered so a steward can open
    /// the vote.
    #[test]
    fn removal_request_reaches_the_pending_buffer() {
        let mut engine = joiner();
        let payload = membership_change(&member("alice"), TypeMembershipChange::Remove);
        engine
            .handle_control(Timestamp::ZERO, id("alice"), 1, &payload)
            .expect("handle control");
        assert!(engine.queues.has_pending_update(&member("alice")));
    }

    /// A removal for someone outside the group is dropped before it can open a
    /// round.
    #[test]
    fn removal_request_for_a_stranger_is_dropped() {
        let mut engine = joiner();
        let payload = membership_change(&member("ghost"), TypeMembershipChange::Remove);
        engine
            .handle_control(Timestamp::ZERO, id("alice"), 1, &payload)
            .expect("handle control");
        assert!(!engine.queues.has_pending_update(&member("ghost")));
    }

    /// The join notice retires the invite that admitted the member and leaves
    /// a removal buffered under the same key alone.
    #[test]
    fn join_notice_retires_a_spent_invite_only() {
        let mut engine = joiner();
        engine.queues.insert_pending_update(
            ConversationUpdateRequest::member_invite(MemberInvite {
                key_package_bytes: b"kp".to_vec(),
                member_id: member("carol"),
            }),
            engine.epoch,
        );
        let payload = membership_change(&member("carol"), TypeMembershipChange::Add);
        engine
            .handle_control(Timestamp::ZERO, id("carol"), 1, &payload)
            .expect("handle control");
        assert!(!engine.queues.has_pending_update(&member("carol")));

        engine.queues.insert_pending_update(
            ConversationUpdateRequest::remove_member(member("alice")),
            engine.epoch,
        );
        let payload = membership_change(&member("alice"), TypeMembershipChange::Add);
        engine
            .handle_control(Timestamp::ZERO, id("alice"), 1, &payload)
            .expect("handle control");
        assert!(
            engine.queues.has_pending_update(&member("alice")),
            "a buffered removal is not a spent invite"
        );
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
            allow_subset_candidates: false,
            peer_scores: vec![],
            timing: Some(TimingConfig::from(engine.config())),
            retry_round: 0,
            max_reelection_attempts: 1,
            liveness_criteria_yes: true,
            threshold_peer_score: 0,
            pending_update_max_epochs: 3,
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

    /// A bootstrap that isn't a `ConversationSync` is refused.
    #[test]
    fn a_non_sync_bootstrap_is_refused() {
        let mut engine = joiner();
        let payload = membership_change(&member("alice"), TypeMembershipChange::Add);
        assert!(matches!(
            engine.apply_bootstrap(&payload),
            Err(ConversationError::InvalidConversationUpdateRequest)
        ));
    }
}
