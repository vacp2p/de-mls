use std::str::FromStr;

use alloy::primitives::Address;

use de_mls_ds::WakuDeliveryService;
use de_mls_ui_protocol::v1::MemberInfo;

use crate::{
    Gateway, UserRef,
    forwarder::{display_batch, load_member_info, lookup_conversation},
    user::UserError,
};

impl Gateway<WakuDeliveryService> {
    pub async fn create_conversation(&self, conversation_id: String) -> anyhow::Result<()> {
        tracing::info!(group = %conversation_id, "creating group as steward");
        let core = self.core();
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .start_conversation(&conversation_id)?;
        core.topics.add_many(&conversation_id)?;
        tracing::info!(group = %conversation_id, "group ready, subtopics subscribed");

        // Unified polling loop — stewards create commit candidates
        // automatically inside `poll_conversation` when the inactivity timer fires.
        let user_clone = user_ref.clone();
        tokio::spawn(Self::group_polling_loop(user_clone, conversation_id));
        Ok(())
    }

    pub async fn join_group(&self, conversation_id: String) -> anyhow::Result<()> {
        tracing::info!(group = %conversation_id, "joining group");
        let core = self.core();
        let user_ref = self.user()?;
        // A joiner holds no conversation until a welcome arrives: it subscribes
        // to the topics and announces its key package, then waits. The welcome
        // is ingested by the delivery forwarder via `User::accept_welcome`,
        // which builds and registers the conversation in `Working`.
        core.topics.add_many(&conversation_id)?;
        let key_package = user_ref.read().await.generate_key_package()?;
        user_ref
            .read()
            .await
            .send_key_package(&conversation_id, key_package)?;
        tracing::info!(group = %conversation_id, "key package sent");

        let user_clone = user_ref.clone();
        let group_name_clone = conversation_id.clone();
        tokio::spawn(async move {
            // Phase 1: wait for the welcome to register the conversation in
            // `Working`. Until then `conversation_state` returns
            // `ConversationNotFound` — treat that as "still waiting", not
            // failure. Bound the wait by a fixed number of polling rounds.
            const MAX_JOIN_ROUNDS: usize = 24;
            let mut joined = false;
            for _ in 0..MAX_JOIN_ROUNDS {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                match user_clone
                    .read()
                    .await
                    .conversation_state(&group_name_clone)
                {
                    // Conversation registered and live — joined.
                    Ok(_) => {
                        joined = true;
                        break;
                    }
                    // Welcome not in yet — keep waiting.
                    Err(UserError::ConversationNotFound) => continue,
                    Err(e) => {
                        tracing::warn!(group = %group_name_clone, error = %e, "join wait exiting");
                        break;
                    }
                }
            }

            if !joined {
                tracing::debug!(group = %group_name_clone, "join timed out waiting for welcome");
                return;
            }

            tracing::info!(group = %group_name_clone, "member joined group");

            // Phase 2: same unified polling loop as creator.
            Self::group_polling_loop(user_clone, group_name_clone).await;
        });

        Ok(())
    }

    /// Unified polling loop for any group member (creator or joiner). All
    /// time-based conversation paths are driven by a single `poll_conversation` call.
    async fn group_polling_loop(user: UserRef, conversation_id: String) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let outcome = match user.read().await.poll_conversation(&conversation_id) {
                Ok(o) => o,
                Err(e) if e.is_fatal() => {
                    tracing::warn!(group = %conversation_id, error = %e, "polling loop exiting");
                    break;
                }
                Err(e) => {
                    tracing::warn!(group = %conversation_id, error = %e, "poll_conversation error");
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
        // Route through `User`, which threads the user's signer into the
        // conversation's `send_message` and flushes the outbound.
        user_ref
            .read()
            .await
            .send_message(&conversation_id, message.into_bytes())?;
        tracing::debug!(group = %conversation_id, "app message sent");
        Ok(())
    }

    pub async fn send_ban_request(
        &self,
        conversation_id: String,
        user_to_ban: String,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;

        let target = Address::from_str(user_to_ban.trim())
            .map_err(|e| anyhow::anyhow!("invalid ban target address {user_to_ban:?}: {e}"))?;

        // Route through `User`, which threads the signer into the
        // conversation's `remove_member` and flushes the outbound.
        user_ref
            .read()
            .await
            .remove_member(&conversation_id, target.as_slice())?;

        Ok(())
    }

    pub async fn vote(
        &self,
        conversation_id: String,
        proposal_id: u32,
        vote: bool,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        // Route through `User`, which threads the signer into the
        // conversation's `vote` and flushes the outbound.
        user_ref
            .read()
            .await
            .vote(&conversation_id, proposal_id, vote)?;
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
        let entry = lookup_conversation(&user_ref, &conversation_id).await?;
        let is_steward = entry
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation"))?
            .is_steward();
        Ok(is_steward)
    }

    pub async fn get_group_state(&self, conversation_id: String) -> anyhow::Result<String> {
        let user_ref = self.user()?;
        let entry = lookup_conversation(&user_ref, &conversation_id).await?;
        let state = entry
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation"))?
            .state();
        Ok(state.to_string())
    }

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        conversation_id: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let user_ref = self.user()?;
        let entry = lookup_conversation(&user_ref, &conversation_id).await?;
        let proposals = entry
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation"))?
            .approved_proposals_for_current_epoch();
        Ok(display_batch(&proposals))
    }

    pub async fn members(&self, conversation_id: String) -> anyhow::Result<Vec<MemberInfo>> {
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
