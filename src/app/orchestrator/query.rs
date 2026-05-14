//! Read-only queries over a conversation's state (UI and diagnostics).
//!
//! Per-conversation getters live on `SessionRunner`. `User` retains
//! transient wrappers so existing callers (gateway, UI bridge) keep
//! compiling; Wave 8 drops the wrappers in favour of direct session
//! access. `list_conversations` is registry-wide and stays on `User`.

use crate::{
    app::{ConversationState, MemberRole, SessionRunner, User, UserError},
    core::{ConsensusPlugin, ConversationPluginsFactory, PeerScoringPlugin, StewardListPlugin},
    identity::format_wallet_address,
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

    pub fn get_conversation_members(&self) -> Result<Vec<String>, UserError> {
        if self.handle.mls().is_none() {
            return Ok(Vec::new());
        }
        let members = self.handle.conversation_members()?;
        Ok(members
            .into_iter()
            .map(|raw| format_wallet_address(raw.as_slice()).to_string())
            .collect())
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
        self.handle.expect_mls()?;
        let members = self.handle.conversation_members()?;
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
        let members = self.handle.conversation_members()?;

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

// ── Transient User wrappers ─────────────────────────────────────────────
//
// Forward to the session-level getter. Existing callers (gateway, UI
// bridge, tests) keep working without per-call lookup ceremony. Wave 8
// drops the wrappers once callers switch to session-direct access.

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    pub async fn get_conversation_state(
        &self,
        conversation_name: &str,
    ) -> Result<ConversationState, UserError> {
        self.with_entry(conversation_name, SessionRunner::get_conversation_state)
            .await
            .ok_or(UserError::ConversationNotFound)
    }

    pub async fn get_epoch_and_retry(
        &self,
        conversation_name: &str,
    ) -> Result<(u64, u32), UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        entry_arc.read().await.get_epoch_and_retry()
    }

    pub async fn list_conversations(&self) -> Vec<String> {
        self.conversations.read().await.keys().cloned().collect()
    }

    pub async fn get_pending_update_count(
        &self,
        conversation_name: &str,
    ) -> Result<usize, UserError> {
        self.with_entry(conversation_name, SessionRunner::get_pending_update_count)
            .await
            .ok_or(UserError::ConversationNotFound)
    }

    pub async fn get_freeze_candidate_count(
        &self,
        conversation_name: &str,
    ) -> Result<(usize, usize), UserError> {
        self.with_entry(conversation_name, SessionRunner::get_freeze_candidate_count)
            .await
            .ok_or(UserError::ConversationNotFound)
    }

    pub async fn is_steward_for_conversation(
        &self,
        conversation_name: &str,
    ) -> Result<bool, UserError> {
        self.with_entry(conversation_name, SessionRunner::is_steward_for_self)
            .await
            .ok_or(UserError::ConversationNotFound)
    }

    pub async fn get_conversation_members(
        &self,
        conversation_name: &str,
    ) -> Result<Vec<String>, UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        entry_arc.read().await.get_conversation_members()
    }

    pub async fn get_member_scores(&self, conversation_name: &str) -> Vec<(Vec<u8>, i64)> {
        match self.lookup_entry(conversation_name).await {
            Some(entry_arc) => entry_arc.read().await.get_member_scores(),
            None => Vec::new(),
        }
    }

    pub async fn get_member_score(&self, conversation_name: &str, member_id: &[u8]) -> Option<i64> {
        let entry_arc = self.lookup_entry(conversation_name).await?;
        entry_arc.read().await.get_member_score(member_id)
    }

    pub async fn get_pending_leave_identities(
        &self,
        conversation_name: &str,
    ) -> Result<Vec<Vec<u8>>, UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        entry_arc.read().await.get_pending_leave_identities()
    }

    pub async fn get_member_roles(
        &self,
        conversation_name: &str,
    ) -> Result<Vec<(Vec<u8>, MemberRole)>, UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        entry_arc.read().await.get_member_roles()
    }

    pub async fn get_approved_proposal_for_current_epoch(
        &self,
        conversation_name: &str,
    ) -> Result<Vec<ConversationUpdateRequest>, UserError> {
        self.with_entry(
            conversation_name,
            SessionRunner::get_approved_proposal_for_current_epoch,
        )
        .await
        .ok_or(UserError::ConversationNotFound)
    }
}
