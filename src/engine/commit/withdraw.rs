//! Approved work the group refused at the build: dropped by the epoch
//! steward and withdrawn from every member.

use tracing::{debug, info};

use crate::{
    ConversationError,
    engine::{
        EngineStore,
        consensus::wire::control_bytes,
        handle::Engine,
        proposal_kind::change_of,
        types::{Action, Event, MemberId, Phase},
    },
    protos::de_mls::messages::v1::{ProposalsDropped, control_message},
};

impl<St: EngineStore> Engine<St> {
    /// Drop the approved add and update of each `rejected` member from the
    /// queue and report them as `Event::ActionsDropped`. Returns the dropped
    /// actions; empty when the queue held nothing for them.
    pub(crate) fn drop_rejected(&mut self, rejected: &[MemberId]) -> Vec<Action> {
        let actions: Vec<Action> = rejected
            .iter()
            .flat_map(|member| self.queues.drop_approved_for(member.as_bytes()))
            .filter_map(|req| change_of(&req).map(|change| change.to_action()))
            .collect();
        if !actions.is_empty() {
            self.dirty.proposals = true;
            info!(
                conversation = %self.conversation_id,
                members = ?rejected,
                "approved actions refused at the build dropped"
            );
            self.emit(Event::ActionsDropped {
                actions: actions.clone(),
            });
        }
        actions
    }

    /// Epoch steward side of a refused build: drop the `rejected` members'
    /// approved work, tell every member, and when a round is open ask for a
    /// new build over the rest. With nothing left the round returns to
    /// `Working`: nothing was missing.
    pub(crate) fn withdraw_rejected(&mut self, rejected: &[MemberId]) {
        let dropped = self.drop_rejected(rejected);
        if dropped.is_empty() {
            return;
        }
        let member_ids = dropped
            .iter()
            .map(|action| action.member().as_bytes().to_vec())
            .collect();
        self.send_control(control_bytes(control_message::Payload::ProposalsDropped(
            ProposalsDropped { member_ids },
        )));
        if self.phase != Phase::Freezing {
            return;
        }
        match self.request_own_candidate() {
            Ok(true) => {}
            Ok(false) | Err(ConversationError::NoProposals) => {
                let resumed = self.start_working();
                self.emit_phase(Some(resumed));
            }
            Err(e) => self.report_failure("commit_candidate_build", &e),
        }
    }

    /// A member's side of a withdrawal: drop the same approved work when the
    /// notice comes from the epoch steward. From anyone else, or while
    /// `Syncing`, it is ignored.
    pub(crate) fn on_proposals_dropped(&mut self, sender: &[u8], notice: ProposalsDropped) {
        if !self.is_synced() || self.expected_steward().as_deref() != Some(sender) {
            debug!(
                conversation = %self.conversation_id,
                "dropped notice ignored: sender is not the epoch steward"
            );
            return;
        }
        let members: Vec<MemberId> = notice
            .member_ids
            .iter()
            .map(|id| MemberId::from(id.as_slice()))
            .collect();
        self.drop_rejected(&members);
    }
}
