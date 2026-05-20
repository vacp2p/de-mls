use de_mls::{
    app::{DispatchOutcome, FreezeTimeoutStatus, PendingJoinTick, SessionRunner, UserError},
    ds::WakuDeliveryService,
    protos::de_mls::messages::v1::BanRequest,
};
use de_mls_ui_protocol::v1::{AppEvent, MemberInfo};

use crate::{
    Gateway, UserRef,
    forwarder::{display_batch, load_member_info, lookup_session},
};

/// True when a [`UserError`] surfaced during the polling loop should end
/// the loop. Exhaustive match per variant so a newly-added [`UserError`]
/// variant forces an explicit decision at compile time. Fatal variants
/// mean "the conversation is gone, stop polling"; non-fatal variants are
/// transient and the loop continues.
fn is_polling_fatal(err: &UserError) -> bool {
    match err {
        // Conversation-level terminal states.
        UserError::ConversationNotFound | UserError::AlreadyLeaving => true,
        // Lock poisoning means the session is corrupted — no recovery.
        // Background task join failure means a spawned task panicked or
        // was cancelled; conservative response is to exit the loop.
        UserError::LockPoisoned(_) | UserError::TaskJoin(_) => true,
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
        core.topics.add_many(&conversation_name)?;
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
        core.topics.add_many(&conversation_name)?;
        let key_package = user_ref.read().await.generate_key_package()?;
        let session = lookup_session(&user_ref, &conversation_name).await?;
        SessionRunner::send_kp_message(&session, key_package).await?;
        tracing::info!(group = %conversation_name, "key package sent");

        // Phase 1 (PendingJoin): Poll every 5s until joined or timed out
        // Phase 2 (Working): Wait until epoch boundaries and check for Waiting transition
        let user_clone = user_ref.clone();
        let group_name_clone = conversation_name.clone();
        let evt_tx_clone = self.evt_tx.clone();
        tokio::spawn(async move {
            // Phase 1: Wait for join
            let joined = loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let Ok(session) = lookup_session(&user_clone, &group_name_clone).await else {
                    break false;
                };
                let tick = match SessionRunner::check_pending_join(&session) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!(group = %group_name_clone, error = %e, "check_pending_join failed");
                        break false;
                    }
                };
                match tick {
                    PendingJoinTick::StillPending => continue,
                    PendingJoinTick::NotPending => {
                        // Transitioned out of PendingJoin (typically Working
                        // after welcome merge). Confirm via state read.
                        let state = match session.read() {
                            Ok(s) => s.get_conversation_state(),
                            Err(_) => {
                                tracing::warn!(
                                    group = %group_name_clone,
                                    "state read skipped: session lock poisoned"
                                );
                                break false;
                            }
                        };
                        break state == de_mls::app::ConversationState::Working;
                    }
                    PendingJoinTick::Expired => {
                        if let Err(e) = user_clone
                            .read()
                            .await
                            .finalize_self_leave(&group_name_clone)
                            .await
                        {
                            tracing::error!(group = %group_name_clone, error = %e, "pending-join cleanup failed");
                        }
                        break false;
                    }
                }
            };

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
            let Ok(session) = lookup_session(&user, &conversation_name).await else {
                tracing::warn!(group = %conversation_name, "polling loop: session gone");
                break;
            };
            if let Err(e) = SessionRunner::tick_deadlines(&session).await {
                if is_polling_fatal(&e) {
                    tracing::warn!(group = %conversation_name, error = %e, "polling loop exiting (tick_deadlines)");
                    break;
                }
                tracing::warn!(group = %conversation_name, error = %e, "tick_deadlines failed");
            }
            let freeze_outcome = match SessionRunner::poll_freeze_status(&session).await {
                Ok(o) => o,
                Err(e) => {
                    if is_polling_fatal(&e) {
                        tracing::warn!(group = %conversation_name, error = %e, "polling loop exiting");
                        break;
                    }
                    continue;
                }
            };
            let (freeze_status, dispatch) = freeze_outcome;
            if matches!(dispatch, DispatchOutcome::LeaveRequested) {
                if let Err(e) = user
                    .read()
                    .await
                    .finalize_self_leave(&conversation_name)
                    .await
                {
                    tracing::error!(group = %conversation_name, error = %e, "self-leave cleanup failed");
                }
                break;
            }
            match freeze_status {
                FreezeTimeoutStatus::NotFreezing => {
                    match SessionRunner::check_member_freeze(&session).await {
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
                    let candidate_count = match session.read() {
                        Ok(s) => Some(s.get_freeze_candidate_count()),
                        Err(_) => {
                            tracing::warn!(
                                group = %conversation_name,
                                "freeze candidate count skipped: session lock poisoned"
                            );
                            None
                        }
                    };
                    if let Some((received, expected)) = candidate_count {
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
        let session = lookup_session(&user_ref, &conversation_name).await?;
        SessionRunner::send_app_message(&session, message.into_bytes()).await?;
        tracing::debug!(group = %conversation_name, "app message sent");
        Ok(())
    }

    pub async fn send_ban_request(
        &self,
        conversation_name: String,
        user_to_ban: String,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_name).await?;

        let ban_request = BanRequest {
            user_to_ban: user_to_ban.clone(),
            conversation_name: conversation_name.clone(),
        };

        SessionRunner::process_ban_request(&session, ban_request).await?;

        Ok(())
    }

    pub async fn process_user_vote(
        &self,
        conversation_name: String,
        proposal_id: u32,
        vote: bool,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_name).await?;
        SessionRunner::process_user_vote(&session, proposal_id, vote).await?;
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
            Ok(user_ref) => user_ref
                .read()
                .await
                .list_conversations()
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn get_steward_status(&self, conversation_name: String) -> anyhow::Result<bool> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_name).await?;
        let is_steward = session
            .read()
            .map_err(|_| UserError::LockPoisoned("session"))?
            .is_steward_for_self();
        Ok(is_steward)
    }

    pub async fn get_group_state(&self, conversation_name: String) -> anyhow::Result<String> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_name).await?;
        let state = session
            .read()
            .map_err(|_| UserError::LockPoisoned("session"))?
            .get_conversation_state();
        Ok(state.to_string())
    }

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        conversation_name: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let user_ref = self.user()?;
        let session = lookup_session(&user_ref, &conversation_name).await?;
        let proposals = session
            .read()
            .map_err(|_| UserError::LockPoisoned("session"))?
            .get_approved_proposal_for_current_epoch();
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
