use hex::ToHex;

use de_mls::mls_crypto::format_wallet_address;
use de_mls::{
    app::FreezeTimeoutStatus,
    ds::WakuDeliveryService,
    protos::de_mls::messages::v1::{BanRequest, group_update_request},
};
use de_mls_ui_protocol::v1::{AppEvent, MemberInfo};

use crate::{Gateway, UserRef};

impl Gateway<WakuDeliveryService> {
    pub async fn create_group(&self, group_name: String) -> anyhow::Result<()> {
        let core = self.core();
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .create_group(&group_name, true)
            .await?;
        core.topics.add_many(&group_name).await;

        // Unified polling loop — stewards create commit candidates
        // automatically via check_member_freeze when the inactivity timer fires.
        let user_clone = user_ref.clone();
        let evt_tx = self.evt_tx.clone();
        tokio::spawn(Self::group_polling_loop(user_clone, evt_tx, group_name));
        Ok(())
    }

    pub async fn join_group(&self, group_name: String) -> anyhow::Result<()> {
        let core = self.core();
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .create_group(&group_name, false)
            .await?;
        core.topics.add_many(&group_name).await;
        user_ref.write().await.send_kp_message(&group_name).await?;
        tracing::debug!("User sent key package message for group {group_name}");

        // Phase 1 (PendingJoin): Poll every 5s until joined or timed out
        // Phase 2 (Working): Wait until epoch boundaries and check for Waiting transition
        let user_clone = user_ref.clone();
        let group_name_clone = group_name.clone();
        let evt_tx_clone = self.evt_tx.clone();
        tokio::spawn(async move {
            // Phase 1: Wait for join
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if !user_clone
                    .read()
                    .await
                    .check_pending_join(&group_name_clone)
                    .await
                {
                    break;
                }
            }

            // Check if we actually joined
            let joined = user_clone
                .read()
                .await
                .get_group_state(&group_name_clone)
                .await
                .map(|s| s == de_mls::app::GroupState::Working)
                .unwrap_or(false);

            if !joined {
                tracing::debug!("Join failed for group {group_name_clone:?}");
                return;
            }

            tracing::info!("Member joined group {group_name_clone:?}");

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
        group_name: String,
    ) {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let freeze_status = match user.read().await.poll_freeze_status(&group_name).await {
                Ok(status) => status,
                Err(e) => {
                    if e.is_fatal() {
                        tracing::warn!("Polling loop exiting for group {group_name:?}: {e}");
                        break;
                    }
                    continue;
                }
            };
            match freeze_status {
                FreezeTimeoutStatus::NotFreezing => {
                    match user.read().await.check_member_freeze(&group_name).await {
                        Ok(true) => { /* entered Freezing (+ created candidate if steward) */ }
                        Ok(false) => {}
                        Err(e) => {
                            if e.is_fatal() {
                                tracing::warn!(
                                    "Polling loop exiting for group {group_name:?}: {e}"
                                );
                                break;
                            }
                        }
                    }
                }
                FreezeTimeoutStatus::StillFreezing => {
                    if let Ok((received, expected)) = user
                        .read()
                        .await
                        .get_freeze_candidate_count(&group_name)
                        .await
                    {
                        let _ = evt_tx.unbounded_send(AppEvent::FreezeCandidates {
                            group_id: group_name.clone(),
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
                            "Commit timeout for group {group_name:?} \
                             with pending proposals — emergency vote initiated"
                        );
                    }
                }
            }
        }
    }

    pub async fn send_message(&self, group_name: String, message: String) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        user_ref
            .read()
            .await
            .send_app_message(&group_name, message.into_bytes())
            .await?;
        tracing::debug!("sent message to the group: {:?}", &group_name);
        Ok(())
    }

    pub async fn send_ban_request(
        &self,
        group_name: String,
        user_to_ban: String,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;

        let ban_request = BanRequest {
            user_to_ban: user_to_ban.clone(),
            group_name: group_name.clone(),
        };

        user_ref
            .write()
            .await
            .process_ban_request(ban_request, &group_name)
            .await?;

        Ok(())
    }

    pub async fn process_user_vote(
        &self,
        group_name: String,
        proposal_id: u32,
        vote: bool,
    ) -> anyhow::Result<()> {
        let user_ref = self.user()?;

        // process_user_vote now sends via handler internally
        user_ref
            .write()
            .await
            .process_user_vote(&group_name, proposal_id, vote)
            .await?;

        Ok(())
    }

    pub async fn leave_group(&self, group_name: String) -> anyhow::Result<()> {
        let user_ref = self.user()?;
        user_ref.write().await.leave_group(&group_name).await?;
        Ok(())
    }

    pub async fn group_list(&self) -> Vec<String> {
        match self.user() {
            Ok(user_ref) => user_ref.read().await.list_groups().await,
            Err(_) => Vec::new(),
        }
    }

    pub async fn get_steward_status(&self, group_name: String) -> anyhow::Result<bool> {
        let user_ref = self.user()?;
        let is_steward = user_ref
            .read()
            .await
            .is_steward_for_group(&group_name)
            .await?;
        Ok(is_steward)
    }

    pub async fn get_group_state(&self, group_name: String) -> anyhow::Result<String> {
        let user_ref = self.user()?;
        let state = user_ref.read().await.get_group_state(&group_name).await?;
        Ok(state.to_string())
    }

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        group_name: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let user_ref = self.user()?;
        let proposals = user_ref
            .read()
            .await
            .get_approved_proposal_for_current_epoch(&group_name)
            .await?;

        let mut display_proposals: Vec<(String, String)> = Vec::with_capacity(proposals.len());

        for proposal in proposals {
            match proposal.payload {
                Some(group_update_request::Payload::InviteMember(kp)) => {
                    let address = kp.identity.encode_hex();
                    display_proposals.push(("Add Member".to_string(), address))
                }
                Some(group_update_request::Payload::RemoveMember(id)) => {
                    display_proposals.push(("Remove Member".to_string(), id.identity.encode_hex()))
                }
                Some(group_update_request::Payload::EmergencyCriteria(ec)) => {
                    let (label, target) = match ec.evidence.as_ref() {
                        Some(e) => (
                            format!("Emergency: {}", e.violation_type_label()),
                            format_wallet_address(&e.target_member_id),
                        ),
                        None => (
                            "Emergency: Unknown Violation".to_string(),
                            "unknown".to_string(),
                        ),
                    };
                    display_proposals.push((label, target));
                }
                Some(group_update_request::Payload::StewardElection(se)) => {
                    let stewards: Vec<String> = se
                        .proposed_stewards
                        .iter()
                        .map(|s| format_wallet_address(s))
                        .collect();
                    display_proposals.push((
                        format!("Steward Election (epoch {})", se.election_epoch),
                        stewards.join(", "),
                    ));
                }
                None => return Err(anyhow::anyhow!("message")),
            }
        }
        Ok(display_proposals)
    }

    pub async fn get_group_members(&self, group_name: String) -> anyhow::Result<Vec<MemberInfo>> {
        let user_ref = self.user()?;
        let user = user_ref.read().await;
        let addresses = user.get_group_members(&group_name).await?;
        let scores = user.get_member_scores(&group_name);
        let roles = user.get_member_roles(&group_name).await.unwrap_or_default();

        let members = addresses
            .into_iter()
            .map(|address| {
                let score = scores
                    .iter()
                    .find(|(raw_id, _)| format_wallet_address(raw_id.as_slice()) == address)
                    .map(|(_, s)| *s)
                    .unwrap_or(100);
                let role = roles
                    .iter()
                    .find(|(raw_id, _)| format_wallet_address(raw_id.as_slice()) == address)
                    .map(|(_, r)| r.to_string())
                    .unwrap_or_else(|| "member".to_string());
                MemberInfo {
                    address,
                    score,
                    role,
                }
            })
            .collect();
        Ok(members)
    }

    /// Get epoch history for a group (past batches of approved proposals).
    ///
    /// Returns up to the last 10 epochs, each as a list of `(action, identity)` pairs.
    pub async fn get_epoch_history(
        &self,
        group_name: String,
    ) -> anyhow::Result<Vec<Vec<(String, String)>>> {
        let user_ref = self.user()?;
        let history = user_ref.read().await.get_epoch_history(&group_name).await?;

        let mut result = Vec::with_capacity(history.len());
        for batch in history {
            let mut display_batch = Vec::with_capacity(batch.len());
            for proposal in batch {
                match proposal.payload {
                    Some(group_update_request::Payload::InviteMember(kp)) => {
                        let address = kp.identity.encode_hex();
                        display_batch.push(("Add Member".to_string(), address));
                    }
                    Some(group_update_request::Payload::RemoveMember(id)) => {
                        display_batch.push(("Remove Member".to_string(), id.identity.encode_hex()));
                    }
                    Some(group_update_request::Payload::EmergencyCriteria(ec)) => {
                        let (label, target) = match ec.evidence.as_ref() {
                            Some(e) => (
                                format!("Emergency: {}", e.violation_type_label()),
                                format_wallet_address(&e.target_member_id),
                            ),
                            None => (
                                "Emergency: Unknown Violation".to_string(),
                                "unknown".to_string(),
                            ),
                        };
                        display_batch.push((label, target));
                    }
                    Some(group_update_request::Payload::StewardElection(se)) => {
                        display_batch.push((
                            "Steward Election".to_string(),
                            format!("epoch {}", se.election_epoch),
                        ));
                    }
                    None => {}
                }
            }
            result.push(display_batch);
        }
        Ok(result)
    }
}
