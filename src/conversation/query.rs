//! Read-only queries over a conversation's state.

use crate::{
    ConsensusPlugin, Conversation, ConversationError, ConversationState, MemberRole,
    PeerScoreStorage, StewardListPlugin, mls_crypto::MlsService,
    protos::de_mls::messages::v1::ConversationUpdateRequest,
};

impl<C, Sc, St> Conversation<C, Sc, St>
where
    C: ConsensusPlugin,
    Sc: PeerScoreStorage,
    St: StewardListPlugin,
{
    /// Current state of the conversation's state machine.
    pub fn state(&self) -> ConversationState {
        self.current_state()
    }

    /// Name of this conversation. Identifies it in the integrator's
    /// registry and on every [`crate::Outbound`] it produces.
    pub fn id(&self) -> &str {
        &self.conversation_id
    }

    /// Identity bytes of the local member in this conversation.
    pub fn member_id_bytes(&self) -> &[u8] {
        &self.self_member_id
    }

    /// App id this conversation tags on outbound packets and uses for self-echo
    /// filtering in [`Conversation::process_inbound`].
    pub fn app_id(&self) -> &[u8] {
        &self.app_id
    }

    /// Current MLS epoch + reelection retry round. Intended for UI status
    /// display.
    pub fn epoch_and_retry(&self) -> Result<(u64, u32), ConversationError> {
        let epoch = self.mls().current_epoch()?;
        Ok((epoch, self.services.steward_list.next_retry_round()))
    }

    /// Count of buffered pending membership updates. Used by tests and the UI
    /// to verify buffer hygiene (e.g., that a joiner's buffer is empty right
    /// after they receive the welcome).
    pub fn pending_update_count(&self) -> usize {
        self.queues.pending_update_count()
    }

    /// Freeze round progress: `(received, expected)`. Returns `(0, 0)` if not
    /// in freeze or no steward list is known.
    pub fn freeze_candidate_count(&self) -> (usize, usize) {
        let received = self.queues.freeze_candidate_count();
        let expected = self
            .services
            .steward_list
            .current_list()
            .map(|l| l.len())
            .unwrap_or(0);
        (received, expected)
    }

    pub fn is_steward(&self) -> bool {
        self.services.steward_list.is_steward(&self.self_member_id)
    }

    /// `true` if the local member is the **primary** steward designated for the
    /// current epoch — the one that should commit and sponsor joiners first.
    /// Unlike [`Self::is_steward`] (true for any member on the list, backups
    /// included), this is true for exactly one member per epoch, so it gates the
    /// single-actor paths: backups defer to the primary and only step in after
    /// the recovery window. Eligibility is the same live rotation
    /// [`Self::member_roles`] uses, so all members agree on who it is.
    pub fn is_epoch_steward(&self) -> Result<bool, ConversationError> {
        let mls = self.mls();
        let epoch = mls.current_epoch()?;
        let members = mls.members()?;
        let eligible = self.queues.steward_eligibility(&members);
        let (epoch_steward, _backup) = self
            .services
            .steward_list
            .epoch_and_backup(epoch, &eligible);
        Ok(epoch_steward == Some(self.self_member_id.as_ref()))
    }

    /// Identity bytes of every current member of this conversation, as
    /// reported by MLS.
    pub fn members(&self) -> Result<Vec<Vec<u8>>, ConversationError> {
        Ok(self.mls().members()?)
    }

    pub fn member_scores(&self) -> Result<Vec<(Vec<u8>, i64)>, ConversationError> {
        self.services.scoring.all_members_with_scores()
    }

    pub fn member_score(&self, member_id: &[u8]) -> Result<Option<i64>, ConversationError> {
        self.services.scoring.score_for(member_id)
    }

    /// Identities that have an in-flight self-leave request. Used by the UI
    /// to render a "pending leave" indicator.
    pub fn pending_leave_member_ids(&self) -> Result<Vec<Vec<u8>>, ConversationError> {
        let members = self.mls().members()?;
        Ok(members
            .into_iter()
            .filter(|id| self.queues.is_pending_self_leave(id))
            .collect())
    }

    /// Steward role for each member. Uses live rotation so removed or
    /// pending-leave stewards are skipped in role display.
    pub fn member_roles(&self) -> Result<Vec<(Vec<u8>, MemberRole)>, ConversationError> {
        let mls = self.mls();
        let epoch = mls.current_epoch()?;
        let members = mls.members()?;

        let eligible = self.queues.steward_eligibility(&members);
        let (live_epoch, live_backup) = self
            .services
            .steward_list
            .epoch_and_backup(epoch, &eligible);
        let live_epoch = live_epoch.map(|s| s.to_vec());
        let live_backup = live_backup.map(|s| s.to_vec());
        let exhausted = self.services.steward_list.is_exhausted(epoch);
        let has_list = self.services.steward_list.current_list().is_some();
        let roles = members
            .iter()
            .cloned()
            .map(|id| {
                let role = if has_list && !exhausted {
                    if live_epoch.as_deref().is_some_and(|es| es == id) {
                        MemberRole::EpochSteward
                    } else if live_backup.as_deref().is_some_and(|bs| bs == id) {
                        MemberRole::BackupSteward
                    } else if self.services.steward_list.is_steward(&id) {
                        MemberRole::Steward
                    } else {
                        MemberRole::Member
                    }
                } else if has_list && self.services.steward_list.is_steward(&id) {
                    MemberRole::Steward
                } else {
                    MemberRole::Member
                };
                (id, role)
            })
            .collect();
        Ok(roles)
    }

    pub fn approved_proposals_for_current_epoch(&self) -> Vec<ConversationUpdateRequest> {
        self.queues.approved_proposals().values().cloned().collect()
    }
}
