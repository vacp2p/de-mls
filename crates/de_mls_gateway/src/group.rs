use std::str::FromStr;

use alloy::primitives::Address;

use de_mls::{protos::de_mls::messages::v1::BanRequest, session::SessionError};
use de_mls_ds::WakuDeliveryService;
use de_mls_ui_protocol::v1::{AppEvent, MemberInfo};

use crate::{
    Gateway, UserRef,
    forwarder::{display_batch, load_member_info, lookup_session},
};

impl Gateway<WakuDeliveryService> {
    pub async fn create_conversation(&self, conversation_id: String) -> anyhow::Result<()> {
        tracing::info!(group = %conversation_id, "creating group as steward");
        let core = self.core();
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .start_conversation(&conversation_id, true)?;
        core.topics.add_many(&conversation_id)?;
        tracing::info!(group = %conversation_id, "group ready, subtopics subscribed");

        // Unified polling loop — stewards create commit candidates
        // automatically via check_member_freeze when the inactivity timer fires.
        let user_clone = user_ref.clone();
        let evt_tx = self.evt_tx.clone();
        tokio::spawn(Self::group_polling_loop(
            user_clone,
            evt_tx,
            conversation_id,
        ));
        Ok(())
    }

    pub async fn join_group(&self, conversation_id: String) -> anyhow::Result<()> {
        tracing::info!(group = %conversation_id, "joining group");
        let core = self.core();
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .start_conversation(&conversation_id, false)?;
        core.topics.add_many(&conversation_id)?;
        let key_package = user_ref.read().await.generate_key_package()?;
        user_ref
            .read()
            .await
            .send_key_package(&conversation_id, key_package)?;
        tracing::info!(group = %conversation_id, "key package sent");

        let user_clone = user_ref.clone();
        let group_name_clone = conversation_id.clone();
        let evt_tx_clone = self.evt_tx.clone();
        tokio::spawn(async move {
            // Phase 1: Poll until welcome received or timed out.
            let joined = loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let outcome = match user_clone.read().await.poll_session(&group_name_clone) {
                    Ok(o) => o,
                    Err(e) if e.is_fatal() => {
                        tracing::warn!(group = %group_name_clone, error = %e, "join poll exiting");
                        break false;
                    }
                    Err(_) => continue,
                };
                if outcome.leave_requested {
                    if let Err(e) = user_clone
                        .read()
                        .await
                        .finalize_self_leave(&group_name_clone)
                    {
                        tracing::error!(group = %group_name_clone, error = %e, "pending-join cleanup failed");
                    }
                    break false;
                }
                match user_clone
                    .read()
                    .await
                    .get_conversation_state(&group_name_clone)
                {
                    Ok(de_mls::session::ConversationState::Working) => break true,
                    Ok(de_mls::session::ConversationState::PendingJoin) => continue,
                    Ok(_) => break true,
                    Err(_) => break false,
                }
            };

            if !joined {
                tracing::debug!(group = %group_name_clone, "join failed");
                return;
            }

            tracing::info!(group = %group_name_clone, "member joined group");

            // Phase 2: same unified polling loop as creator.
            Self::group_polling_loop(user_clone, evt_tx_clone, group_name_clone).await;
        });

        Ok(())
    }

    /// Unified polling loop for any group member (creator or joiner). All
    /// time-based session paths are driven by a single `poll_session` call.
    async fn group_polling_loop(
        user: UserRef,
        _evt_tx: futures::channel::mpsc::UnboundedSender<AppEvent>,
        conversation_id: String,
    ) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let outcome = match user.read().await.poll_session(&conversation_id) {
                Ok(o) => o,
                Err(e) if e.is_fatal() => {
                    tracing::warn!(group = %conversation_id, error = %e, "polling loop exiting");
                    break;
                }
                Err(e) => {
                    tracing::warn!(group = %conversation_id, error = %e, "poll_session error");
                    continue;
                }
            };
            if outcome.leave_requested {
                if let Err(e) = user.read().await.finalize_self_leave(&conversation_id) {
                    tracing::error!(group = %conversation_id, error = %e, "self-leave cleanup failed");
                }
                break;
            }
        }
    }

    pub async fn send_message(
        &self,
        conversation_id: String,
        message: String,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_id).await?;
        session
            .write()
            .map_err(|_| SessionError::LockPoisoned("session"))?
            .push_message(message.into_bytes())?;
        tracing::debug!(group = %conversation_id, "app message sent");
        Ok(())
    }

    pub async fn send_ban_request(
        &self,
        conversation_id: String,
        user_to_ban: String,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_id).await?;

        let target = Address::from_str(user_to_ban.trim())
            .map_err(|e| anyhow::anyhow!("invalid ban target address {user_to_ban:?}: {e}"))?;
        let ban_request = BanRequest {
            user_to_ban: target.as_slice().to_vec(),
            conversation_id: conversation_id.clone(),
        };

        session
            .write()
            .map_err(|_| SessionError::LockPoisoned("session"))?
            .process_ban_request(ban_request)?;

        Ok(())
    }

    pub async fn process_user_vote(
        &self,
        conversation_id: String,
        proposal_id: u32,
        vote: bool,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_id).await?;
        session
            .write()
            .map_err(|_| SessionError::LockPoisoned("session"))?
            .process_user_vote(proposal_id, vote)?;
        Ok(())
    }

    pub async fn leave_conversation(&self, conversation_id: String) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .leave_conversation(&conversation_id)?;
        Ok(())
    }

    pub async fn group_list(&self) -> Vec<String> {
        match self.user() {
            Ok(user_ref) => user_ref
                .read()
                .await
                .list_conversations()
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn get_steward_status(&self, conversation_id: String) -> anyhow::Result<bool> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_id).await?;
        let is_steward = session
            .read()
            .map_err(|_| SessionError::LockPoisoned("session"))?
            .is_steward_for_self();
        Ok(is_steward)
    }

    pub async fn get_group_state(&self, conversation_id: String) -> anyhow::Result<String> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_id).await?;
        let state = session
            .read()
            .map_err(|_| SessionError::LockPoisoned("session"))?
            .get_conversation_state();
        Ok(state.to_string())
    }

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        conversation_id: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_id).await?;
        let proposals = session
            .read()
            .map_err(|_| SessionError::LockPoisoned("session"))?
            .get_approved_proposals_for_current_epoch();
        Ok(display_batch(&proposals))
    }

    pub async fn get_conversation_members(
        &self,
        conversation_id: String,
    ) -> anyhow::Result<Vec<MemberInfo>> {
        load_member_info(&self.user()?, &conversation_id).await
    }

    /// Get epoch history for a group (past batches of approved proposals).
    ///
    /// Returns up to the last 10 epochs, each as a list of `(action, member_id)` pairs.
    pub async fn get_epoch_history(
        &self,
        conversation_id: String,
    ) -> anyhow::Result<Vec<Vec<(String, String)>>> {
        let store = self.epoch_history.lock();
        Ok(store
            .get(&conversation_id)
            .map(|history| history.iter().map(|b| display_batch(b)).collect())
            .unwrap_or_default())
    }
}
