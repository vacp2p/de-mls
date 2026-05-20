//! Read-only queries over a conversation's state (UI and diagnostics).
//! Callers reach these via `User::lookup_entry`; the registry-wide
//! `list_conversations` getter lives on `User`.

use crate::{
    app::{ConversationState, MemberRole, SessionRunner, UserError},
    core::{ConsensusPlugin, ConversationPluginsFactory, PeerScoringPlugin, StewardListPlugin},
    mls_crypto::MlsService,
    protos::de_mls::messages::v1::ConversationUpdateRequest,
};

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> SessionRunner<P, CP> {
    pub fn get_conversation_state(&self) -> ConversationState {
        self.handle.current_state()
    }

    /// Current MLS epoch + reelection retry round. `(0, 0)` when the
    /// conversation has no MLS state yet (pending join). Intended for UI
    /// status display.
    pub fn get_epoch_and_retry(&self) -> Result<(u64, u32), UserError> {
        let epoch = match self.handle.mls() {
            Some(mls) => mls.current_epoch()?,
            None => 0,
        };
        Ok((epoch, self.handle.steward_list.retry_round()))
    }

    /// Count of buffered pending membership updates. Used by tests and the UI
    /// to verify buffer hygiene (e.g., that a joiner's buffer is empty right
    /// after they receive the welcome).
    pub fn get_pending_update_count(&self) -> usize {
        self.handle.conversation.pending_update_count()
    }

    /// Freeze round progress: `(received, expected)`. Returns `(0, 0)` if not
    /// in freeze or no steward list is known.
    pub fn get_freeze_candidate_count(&self) -> (usize, usize) {
        let received = self.handle.conversation.freeze_candidate_count();
        let expected = self
            .handle
            .steward_list
            .current_list()
            .map(|l| l.len())
            .unwrap_or(0);
        (received, expected)
    }

    pub fn is_steward_for_self(&self) -> bool {
        self.handle.steward_list.is_steward(&self.self_identity)
    }

    /// Identity bytes of every current member of this conversation, as
    /// reported by MLS. Returns an empty vec when the local user has no
    /// MLS state yet (pending join).
    pub fn get_conversation_members(&self) -> Result<Vec<Vec<u8>>, UserError> {
        match self.handle.mls() {
            Some(mls) => Ok(mls.members()?),
            None => Ok(Vec::new()),
        }
    }

    pub fn get_member_scores(&self) -> Vec<(Vec<u8>, i64)> {
        self.handle.scoring.all_members_with_scores()
    }

    pub fn get_member_score(&self, member_id: &[u8]) -> Option<i64> {
        self.handle.scoring.score_for(member_id)
    }

    /// Identities that have an in-flight self-leave request. Used by the UI
    /// to render a "pending leave" indicator.
    pub fn get_pending_leave_identities(&self) -> Result<Vec<Vec<u8>>, UserError> {
        let members = self.handle.expect_mls()?.members()?;
        Ok(members
            .into_iter()
            .filter(|id| self.handle.conversation.is_pending_self_leave(id))
            .collect())
    }

    /// Steward role for each member. Uses live rotation so removed or
    /// pending-leave stewards are skipped in role display.
    pub fn get_member_roles(&self) -> Result<Vec<(Vec<u8>, MemberRole)>, UserError> {
        let mls = self.handle.expect_mls()?;
        let epoch = mls.current_epoch()?;
        let members = mls.members()?;

        let eligible = self.handle.conversation.steward_eligibility(&members);
        let (live_epoch, live_backup) = self.handle.steward_list.epoch_and_backup(epoch, &eligible);
        let live_epoch = live_epoch.map(|s| s.to_vec());
        let live_backup = live_backup.map(|s| s.to_vec());
        let exhausted = self.handle.steward_list.is_exhausted(epoch);
        let has_list = self.handle.steward_list.current_list().is_some();
        let roles = members
            .iter()
            .cloned()
            .map(|id| {
                let role = if has_list && !exhausted {
                    if live_epoch.as_deref().is_some_and(|es| es == id) {
                        MemberRole::EpochSteward
                    } else if live_backup.as_deref().is_some_and(|bs| bs == id) {
                        MemberRole::BackupSteward
                    } else if self.handle.steward_list.is_steward(&id) {
                        MemberRole::Steward
                    } else {
                        MemberRole::Member
                    }
                } else if has_list && self.handle.steward_list.is_steward(&id) {
                    MemberRole::Steward
                } else {
                    MemberRole::Member
                };
                (id, role)
            })
            .collect();
        Ok(roles)
    }

    pub fn get_approved_proposal_for_current_epoch(&self) -> Vec<ConversationUpdateRequest> {
        self.handle
            .conversation
            .approved_proposals()
            .values()
            .cloned()
            .collect()
    }
}
