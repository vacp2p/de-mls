use de_mls::{
    app::{FreezeTimeoutStatus, UserError},
    ds::WakuDeliveryService,
    protos::de_mls::messages::v1::BanRequest,
};
use de_mls_ui_protocol::v1::{AppEvent, MemberInfo};

use crate::{
    Gateway, UserRef,
    forwarder::{display_batch, load_member_info},
};

/// True when a [`UserError`] surfaced during the polling loop should end
/// the loop. Exhaustive match per variant so a newly-added [`UserError`]
/// variant forces an explicit decision at compile time. Fatal variants
/// mean "the conversation is gone, stop polling"; non-fatal variants are
/// transient and the loop continues.
fn is_polling_fatal(err: &UserError) -> bool {
    match err {
        UserError::ConversationNotFound | UserError::AlreadyLeaving => true,
        UserError::ConversationAlreadyExists
        | UserError::ConversationBlocked(_)
        | UserError::PartialFreeze
        | UserError::Transport(_)
        | UserError::Core(_)
        | UserError::Consensus(_)
        | UserError::Message(_)
        | UserError::SystemTime(_)
        | UserError::Signer(_)
        | UserError::Mls(_)
        | UserError::Identity(_) => false,
    }
}

impl Gateway<WakuDeliveryService> {
    pub async fn create_conversation(&self, conversation_name: String) -> anyhow::Result<()> {
        tracing::info!(group = %conversation_name, "creating group as steward");
        let core = self.core();
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .start_conversation(&conversation_name, true)
            .await?;
        core.topics.add_many(&conversation_name).await;
        tracing::info!(group = %conversation_name, "group ready, subtopics subscribed");

        // Unified polling loop — stewards create commit candidates
        // automatically via check_member_freeze when the inactivity timer fires.
        let user_clone = user_ref.clone();
        let evt_tx = self.evt_tx.clone();
        tokio::spawn(Self::group_polling_loop(
            user_clone,
            evt_tx,
            conversation_name,
        ));
        Ok(())
    }

    pub async fn join_group(&self, conversation_name: String) -> anyhow::Result<()> {
        tracing::info!(group = %conversation_name, "joining group");
        let core = self.core();
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .start_conversation(&conversation_name, false)
            .await?;
        core.topics.add_many(&conversation_name).await;
        user_ref
            .write()
            .await
            .send_kp_message(&conversation_name)
            .await?;
        tracing::info!(group = %conversation_name, "key package sent");

        // Phase 1 (PendingJoin): Poll every 5s until joined or timed out
        // Phase 2 (Working): Wait until epoch boundaries and check for Waiting transition
        let user_clone = user_ref.clone();
        let group_name_clone = conversation_name.clone();
        let evt_tx_clone = self.evt_tx.clone();
        tokio::spawn(async move {
            // Phase 1: Wait for join
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                match user_clone
                    .read()
                    .await
                    .check_pending_join(&group_name_clone)
                    .await
                {
                    Ok(true) => continue, // still waiting
                    Ok(false) => break,   // joined or timed out
                    Err(e) => {
                        tracing::error!(group = %group_name_clone, error = %e, "check_pending_join failed");
                        break;
                    }
                }
            }

            // Check if we actually joined
            let joined = user_clone
                .read()
                .await
                .get_conversation_state(&group_name_clone)
                .await
                .map(|s| s == de_mls::app::ConversationState::Working)
                .unwrap_or(false);

            if !joined {
                tracing::debug!(group = %group_name_clone, "join failed");
                return;
            }

            tracing::info!(group = %group_name_clone, "member joined group");

            // Phase 2: same unified polling loop as creator
            Self::group_polling_loop(user_clone, evt_tx_clone, group_name_clone).await;
        });

        Ok(())
    }

    /// Unified polling loop for any group member (creator or joiner).
    ///
    /// Handles freeze status polling, inactivity detection, and commit candidate
    /// creation for stewards. All members run the same loop — steward-specific
    /// behavior is triggered inside `check_member_freeze` when `is_steward()` is true.
    async fn group_polling_loop(
        user: UserRef,
        evt_tx: futures::channel::mpsc::UnboundedSender<AppEvent>,
        conversation_name: String,
    ) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let freeze_status = match user
                .read()
                .await
                .poll_freeze_status(&conversation_name)
                .await
            {
                Ok(status) => status,
                Err(e) => {
                    if is_polling_fatal(&e) {
                        tracing::warn!(group = %conversation_name, error = %e, "polling loop exiting");
                        break;
                    }
                    continue;
                }
            };
            match freeze_status {
                FreezeTimeoutStatus::NotFreezing => {
                    match user
                        .read()
                        .await
                        .check_member_freeze(&conversation_name)
                        .await
                    {
                        Ok(true) => { /* entered Freezing (+ created candidate if steward) */ }
                        Ok(false) => {}
                        Err(e) => {
                            if is_polling_fatal(&e) {
                                tracing::warn!(group = %conversation_name, error = %e, "polling loop exiting");
                                break;
                            }
                            tracing::error!(group = %conversation_name, error = %e, "check_member_freeze failed");
                        }
                    }
                }
                FreezeTimeoutStatus::StillFreezing => {
                    if let Ok((received, expected)) = user
                        .read()
                        .await
                        .get_freeze_candidate_count(&conversation_name)
                        .await
                    {
                        let _ = evt_tx.unbounded_send(AppEvent::FreezeCandidates {
                            conversation_id: conversation_name.clone(),
                            received,
                            expected,
                        });
                    }
                    continue;
                }
                FreezeTimeoutStatus::Applied => {}
                FreezeTimeoutStatus::TimedOut { has_proposals } => {
                    if has_proposals {
                        tracing::warn!(
                            group = %conversation_name,
                            "commit timeout with pending proposals, emergency vote initiated"
                        );
                    }
                }
            }
        }
    }

    pub async fn send_message(
        &self,
        conversation_name: String,
        message: String,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        user_ref
            .read()
            .await
            .send_app_message(&conversation_name, message.into_bytes())
            .await?;
        tracing::debug!(group = %conversation_name, "app message sent");
        Ok(())
    }

    pub async fn send_ban_request(
        &self,
        conversation_name: String,
        user_to_ban: String,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;

        let ban_request = BanRequest {
            user_to_ban: user_to_ban.clone(),
            conversation_name: conversation_name.clone(),
        };

        user_ref
            .write()
            .await
            .process_ban_request(ban_request, &conversation_name)
            .await?;

        Ok(())
    }

    pub async fn process_user_vote(
        &self,
        conversation_name: String,
        proposal_id: u32,
        vote: bool,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;

        // process_user_vote now sends via handler internally
        user_ref
            .write()
            .await
            .process_user_vote(&conversation_name, proposal_id, vote)
            .await?;

        Ok(())
    }

    pub async fn leave_conversation(&self, conversation_name: String) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .leave_conversation(&conversation_name)
            .await?;
        Ok(())
    }

    pub async fn group_list(&self) -> Vec<String> {
        match self.user() {
            Ok(user_ref) => user_ref.read().await.list_conversations().await,
            Err(_) => Vec::new(),
        }
    }

    pub async fn get_steward_status(&self, conversation_name: String) -> anyhow::Result<bool> {
        let user_ref = self.user()?;
        let is_steward = user_ref
            .read()
            .await
            .is_steward_for_conversation(&conversation_name)
            .await?;
        Ok(is_steward)
    }

    pub async fn get_group_state(&self, conversation_name: String) -> anyhow::Result<String> {
        let user_ref = self.user()?;
        let state = user_ref
            .read()
            .await
            .get_conversation_state(&conversation_name)
            .await?;
        Ok(state.to_string())
    }

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        conversation_name: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let user_ref = self.user()?;
        let proposals = user_ref
            .read()
            .await
            .get_approved_proposal_for_current_epoch(&conversation_name)
            .await?;
        Ok(display_batch(&proposals))
    }

    pub async fn get_conversation_members(
        &self,
        conversation_name: String,
    ) -> anyhow::Result<Vec<MemberInfo>> {
        load_member_info(&self.user()?, &conversation_name).await
    }

    /// Get epoch history for a group (past batches of approved proposals).
    ///
    /// Returns up to the last 10 epochs, each as a list of `(action, identity)` pairs.
    pub async fn get_epoch_history(
        &self,
        conversation_name: String,
    ) -> anyhow::Result<Vec<Vec<(String, String)>>> {
        let store = self.epoch_history.lock();
        Ok(store
            .get(&conversation_name)
            .map(|history| history.iter().map(|b| display_batch(b)).collect())
            .unwrap_or_default())
    }
}
