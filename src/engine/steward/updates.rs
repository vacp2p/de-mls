//! The pending-update buffer every member records so a future epoch steward
//! can retry a stranded membership change: handling an incoming update
//! request, and draining the buffer into voting proposals.

use tracing::info;

use crate::{
    ConversationError,
    engine::{
        handle::Engine,
        store::EngineStore,
        types::Phase,
        util::{member_set, target_member_id_of},
        voting::CreatorVote,
    },
    protos::de_mls::messages::v1::{ConversationUpdateRequest, conversation_update_request},
};

impl<St: EngineStore> Engine<St> {
    // ── pending-update buffer ──────────────────────────────────────────

    /// Handle a membership update (an invite or a removal request) seen from
    /// `sender`: buffer it so every member has a durable record, then promote
    /// it to a voting proposal if this node is the current epoch steward and
    /// the conversation accepts new proposals.
    pub(crate) fn handle_incoming_update_request(
        &mut self,
        sender: &[u8],
        request: ConversationUpdateRequest,
    ) -> Result<(), ConversationError> {
        // Defensive — only membership changes reach here.
        if target_member_id_of(&request).is_none() {
            return Ok(());
        }
        let state = self.phase;
        let inserted = self
            .queues
            .insert_pending_update(request.clone(), self.epoch);
        // store: proposals (WP3g)

        // Only the epoch steward proposes immediately. The buffer survives
        // commit rounds so a later steward can retry.
        let is_epoch_steward = self.is_epoch_steward();
        let should_propose = is_epoch_steward && state == Phase::Working;
        info!(
            conversation = %self.conversation_id,
            epoch = self.epoch,
            sender = ?sender,
            inserted,
            buffer_total = self.queues.pending_update_count(),
            is_epoch_steward,
            state = %state,
            propose = should_propose,
            "update request buffered"
        );

        if should_propose {
            // Steward auto-propose: the steward forwards peer intent and still
            // holds a judgement call, so the proposal goes out unbundled and
            // the vote request drives the steward's vote like any other
            // member's. The proposal path may still refuse (active emergency
            // etc.) — the entry stays in the buffer for the next rotation.
            if let Err(e) = self.initiate_proposal(request, CreatorVote::Deferred) {
                self.report_deferred_proposal("steward_auto_propose", e);
            }
        }
        Ok(())
    }

    /// On epoch advance the new live epoch steward drains the pending-update
    /// buffer into voting proposals. A backup steward reaches the same drain
    /// from `tick` once `backup_takeover_window` passes (see
    /// [`Self::drive_buffered_proposals`]).
    pub(crate) fn process_buffered_updates(&mut self) -> Result<(), ConversationError> {
        if !self.is_epoch_steward() {
            return Ok(());
        }
        self.drain_buffered_updates()
    }

    /// Buffered membership updates still needing a proposal: not already
    /// covered by a live (voting or approved) proposal, and still valid — an
    /// invite for a non-member or a removal for a current member.
    pub(crate) fn actionable_buffered_updates(&self) -> Vec<ConversationUpdateRequest> {
        let members = member_set(&self.members);
        let active = self.queues.active_proposal_targets();
        self.queues
            .pending_updates()
            .iter()
            .filter(|(id, _)| !active.contains(id.as_slice()))
            .filter(|(id, p)| {
                // Drop an invite for an existing member and a removal for a
                // non-member.
                let is_member = members.contains(id.as_slice());
                match p.request.payload.as_ref() {
                    Some(conversation_update_request::Payload::MemberInvite(_)) => !is_member,
                    Some(conversation_update_request::Payload::RemoveMember(_)) => is_member,
                    _ => false,
                }
            })
            .map(|(_, p)| p.request.clone())
            .collect()
    }

    /// Promote every actionable buffered update to a voting proposal
    /// (`CreatorVote::Deferred` — the proposer relays peer intent, it doesn't
    /// endorse). Callers gate *who* drains: the epoch steward immediately via
    /// [`Self::process_buffered_updates`], a backup after
    /// `backup_takeover_window` via [`Self::drive_buffered_proposals`]. This
    /// just performs the drain.
    pub(crate) fn drain_buffered_updates(&mut self) -> Result<(), ConversationError> {
        let to_propose = self.actionable_buffered_updates();
        if to_propose.is_empty() {
            return Ok(());
        }
        info!(
            conversation = %self.conversation_id,
            count = to_propose.len(),
            "promoting buffered updates to proposals"
        );
        for request in to_propose {
            if let Err(e) = self.initiate_proposal(request, CreatorVote::Deferred) {
                self.report_deferred_proposal("drain_buffered_updates", e);
            }
        }
        Ok(())
    }
}
