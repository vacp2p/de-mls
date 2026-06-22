use std::str::FromStr;

use alloy::primitives::Address;

use de_mls_ds::WakuDeliveryService;
use de_mls_ui_protocol::v1::MemberInfo;

use crate::{
    ConversationRef, Gateway, UserRef,
    forwarder::{display_batch, load_member_info},
    user::UserError,
};

/// State string surfaced for a conversation whose welcome has not yet
/// arrived. Distinct from the library's [`de_mls::ConversationState`] values,
/// which only exist once the conversation is live.
const PENDING_JOIN_STATE: &str = "PendingJoin";

/// Where a conversation id resolves in the gateway registry.
enum Located {
    /// A live conversation handle.
    Live(ConversationRef),
    /// A registered join still waiting for its welcome.
    Pending,
    /// No slot for this id.
    Missing,
}

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
        // A joiner holds no live conversation until a welcome arrives. We first
        // register a pending-join slot so the conversation lists and reports as
        // "joining" right away, then subscribe to the topics and announce our
        // key package. The welcome is ingested by the delivery forwarder via
        // `User::accept_welcome`, which fills the slot with the `Working`
        // conversation.
        user_ref.read().await.begin_pending_join(&conversation_id)?;
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
                // Drop the pending slot so the abandoned join stops listing /
                // reporting as "joining" (no-op if the welcome raced in).
                if let Err(e) = user_clone
                    .read()
                    .await
                    .abandon_pending_join(&group_name_clone)
                {
                    tracing::warn!(group = %group_name_clone, error = %e, "abandon pending join failed");
                }
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

    /// Resolve a conversation id to a live handle, a pending join, or nothing.
    async fn locate(&self, conversation_id: &str) -> anyhow::Result<Located> {
        let user_ref = self.user()?;
        let Some(entry) = user_ref.read().await.lookup_entry(conversation_id)? else {
            return Ok(Located::Missing);
        };
        let pending = entry
            .read()
            .map_err(|_| UserError::LockPoisoned("conversation"))?
            .is_pending();
        Ok(if pending {
            Located::Pending
        } else {
            Located::Live(entry)
        })
    }

    pub async fn get_steward_status(&self, conversation_id: String) -> anyhow::Result<bool> {
        match self.locate(&conversation_id).await? {
            Located::Live(entry) => Ok(entry
                .read()
                .map_err(|_| UserError::LockPoisoned("conversation"))?
                .live_ref()?
                .is_steward()),
            // A joining member isn't a steward yet.
            Located::Pending => Ok(false),
            Located::Missing => Err(UserError::ConversationNotFound.into()),
        }
    }

    pub async fn get_group_state(&self, conversation_id: String) -> anyhow::Result<String> {
        match self.locate(&conversation_id).await? {
            Located::Live(entry) => {
                let state = entry
                    .read()
                    .map_err(|_| UserError::LockPoisoned("conversation"))?
                    .live_ref()?
                    .state();
                Ok(state.to_string())
            }
            Located::Pending => Ok(PENDING_JOIN_STATE.to_string()),
            Located::Missing => Err(UserError::ConversationNotFound.into()),
        }
    }

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        conversation_id: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        match self.locate(&conversation_id).await? {
            Located::Live(entry) => {
                let proposals = entry
                    .read()
                    .map_err(|_| UserError::LockPoisoned("conversation"))?
                    .live_ref()?
                    .approved_proposals_for_current_epoch();
                Ok(display_batch(&proposals))
            }
            // No epoch yet — the joiner has no proposals to show.
            Located::Pending => Ok(Vec::new()),
            Located::Missing => Err(UserError::ConversationNotFound.into()),
        }
    }

    pub async fn members(&self, conversation_id: String) -> anyhow::Result<Vec<MemberInfo>> {
        match self.locate(&conversation_id).await? {
            Located::Live(_) => load_member_info(&self.user()?, &conversation_id).await,
            // No roster until we hold the welcome.
            Located::Pending => Ok(Vec::new()),
            Located::Missing => Err(UserError::ConversationNotFound.into()),
        }
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
