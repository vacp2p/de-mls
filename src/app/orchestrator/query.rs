//! Read-only queries over a conversation's state (UI and diagnostics).

use crate::{
    app::{ConversationPlugins, ConversationState, MemberRole, User, UserError},
    core::{ConversationEventHandler, DeMlsProvider, PeerScoringPlugin, StewardListPlugin},
    identity::format_wallet_address,
    protos::de_mls::messages::v1::ConversationUpdateRequest,
};

use crate::mls_crypto::MlsService;

impl<P: DeMlsProvider, GP: ConversationPlugins, H: ConversationEventHandler + 'static>
    User<P, GP, H>
{
    pub async fn get_conversation_state(
        &self,
        conversation_name: &str,
    ) -> Result<ConversationState, UserError> {
        self.with_entry(conversation_name, |e| e.handle.current_state())
            .await
            .ok_or(UserError::ConversationNotFound)
    }

    /// Current MLS epoch + reelection retry round. `(0, 0)` when the
    /// conversation has no MLS state yet (pending join). Intended for UI
    /// status display.
    pub async fn get_epoch_and_retry(
        &self,
        conversation_name: &str,
    ) -> Result<(u64, u32), UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let entry = entry_arc.read().await;
        let epoch = match entry.handle.mls() {
            Some(mls) => mls.current_epoch()?,
            None => 0,
        };
        Ok((epoch, entry.handle.steward_list.retry_round()))
    }

    pub async fn list_conversations(&self) -> Vec<String> {
        self.conversations.read().await.keys().cloned().collect()
    }

    /// Count of buffered pending membership updates. Used by tests and the UI
    /// to verify buffer hygiene (e.g., that a joiner's buffer is empty right
    /// after they receive the welcome).
    pub async fn get_pending_update_count(
        &self,
        conversation_name: &str,
    ) -> Result<usize, UserError> {
        self.with_entry(conversation_name, |e| {
            e.handle.conversation.pending_update_count()
        })
        .await
        .ok_or(UserError::ConversationNotFound)
    }

    /// Freeze round progress: `(received, expected)`. Returns `(0, 0)` if not
    /// in freeze or no steward list is known.
    pub async fn get_freeze_candidate_count(
        &self,
        conversation_name: &str,
    ) -> Result<(usize, usize), UserError> {
        self.with_entry(conversation_name, |e| {
            let received = e.handle.conversation.freeze_candidate_count();
            let expected = e
                .handle
                .steward_list
                .current_list()
                .map(|l| l.len())
                .unwrap_or(0);
            (received, expected)
        })
        .await
        .ok_or(UserError::ConversationNotFound)
    }

    pub async fn is_steward_for_conversation(
        &self,
        conversation_name: &str,
    ) -> Result<bool, UserError> {
        let self_id = self.self_identity().to_vec();
        self.with_entry(conversation_name, |e| {
            e.handle.steward_list.is_steward(&self_id)
        })
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
        let entry = entry_arc.read().await;
        if entry.handle.mls().is_none() {
            return Ok(Vec::new());
        }
        let members = entry.handle.conversation_members()?;
        Ok(members
            .into_iter()
            .map(|raw| format_wallet_address(raw.as_slice()).to_string())
            .collect())
    }

    pub async fn get_member_scores(&self, conversation_name: &str) -> Vec<(Vec<u8>, i64)> {
        match self.lookup_entry(conversation_name).await {
            Some(entry_arc) => entry_arc
                .read()
                .await
                .handle
                .scoring
                .all_members_with_scores(),
            None => Vec::new(),
        }
    }

    pub async fn get_member_score(&self, conversation_name: &str, member_id: &[u8]) -> Option<i64> {
        let entry_arc = self.lookup_entry(conversation_name).await?;
        let entry = entry_arc.read().await;
        entry.handle.scoring.score_for(member_id)
    }

    /// Identities that have an in-flight self-leave request. Used by the UI
    /// to render a "pending leave" indicator.
    pub async fn get_pending_leave_identities(
        &self,
        conversation_name: &str,
    ) -> Result<Vec<Vec<u8>>, UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let entry = entry_arc.read().await;
        entry.handle.expect_mls()?;
        let members = entry.handle.conversation_members()?;
        Ok(members
            .into_iter()
            .filter(|id| entry.handle.conversation.is_pending_self_leave(id))
            .collect())
    }

    /// Steward role for each member. Uses live rotation so removed or
    /// pending-leave stewards are skipped in role display.
    pub async fn get_member_roles(
        &self,
        conversation_name: &str,
    ) -> Result<Vec<(Vec<u8>, MemberRole)>, UserError> {
        let entry_arc = self
            .lookup_entry(conversation_name)
            .await
            .ok_or(UserError::ConversationNotFound)?;
        let entry = entry_arc.read().await;
        let mls = entry.handle.expect_mls()?;
        let epoch = mls.current_epoch()?;
        let members = entry.handle.conversation_members()?;

        let eligible = entry.handle.conversation.steward_eligibility(&members);
        let (live_epoch, live_backup) =
            entry.handle.steward_list.epoch_and_backup(epoch, &eligible);
        let live_epoch = live_epoch.map(|s| s.to_vec());
        let live_backup = live_backup.map(|s| s.to_vec());
        let exhausted = entry.handle.steward_list.is_exhausted(epoch);
        let has_list = entry.handle.steward_list.current_list().is_some();
        let roles = members
            .iter()
            .cloned()
            .map(|id| {
                let role = if has_list && !exhausted {
                    if live_epoch.as_deref().is_some_and(|es| es == id) {
                        MemberRole::EpochSteward
                    } else if live_backup.as_deref().is_some_and(|bs| bs == id) {
                        MemberRole::BackupSteward
                    } else if entry.handle.steward_list.is_steward(&id) {
                        MemberRole::Steward
                    } else {
                        MemberRole::Member
                    }
                } else if has_list && entry.handle.steward_list.is_steward(&id) {
                    MemberRole::Steward
                } else {
                    MemberRole::Member
                };
                (id, role)
            })
            .collect();
        Ok(roles)
    }

    pub async fn get_approved_proposal_for_current_epoch(
        &self,
        conversation_name: &str,
    ) -> Result<Vec<ConversationUpdateRequest>, UserError> {
        self.with_entry(conversation_name, |e| {
            e.handle
                .conversation
                .approved_proposals()
                .values()
                .cloned()
                .collect()
        })
        .await
        .ok_or(UserError::ConversationNotFound)
    }
}
