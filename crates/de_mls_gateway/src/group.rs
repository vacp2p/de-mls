use ds::DeliveryService;
use std::time::Duration;
use tracing::info;

use crate::{forwarder::handle_user_action, Gateway};
use de_mls::{core::GroupUpdateRequest, protos::de_mls::messages::v1::BanRequest};

pub const STEWARD_EPOCH: u64 = 20;

impl<DS: DeliveryService> Gateway<DS> {
    pub async fn create_group(&self, group_name: String) -> anyhow::Result<()> {
        let core = self.core();
        let user_ref = self.user()?;
        user_ref
            .write()
            .await
            .create_group(&group_name, true)
            .await?;
        core.topics.add_many(&group_name).await;
        core.groups.insert(group_name.clone()).await;
        info!("User start sending steward message for group {group_name:?}");

        let user_clone = user_ref.clone();
        let group_name_clone = group_name.clone();
        let evt_tx_clone = self.evt_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(STEWARD_EPOCH));
            loop {
                interval.tick().await;
                // Step 1: Start steward epoch - check for proposals and start epoch if needed
                let proposals_count = match user_clone
                    .write()
                    .await
                    .start_steward_epoch(&group_name)
                    .await
                {
                    Ok(count) => count,
                    Err(e) => {
                        tracing::warn!(
                            "start steward epoch request failed for group {group_name:?}: {e}"
                        );
                        continue;
                    }
                };

                if proposals_count == 0 {
                    info!(
                        "No proposals to vote on for group: {group_name}, completing epoch without voting"
                    );
                } else {
                    info!("Found {proposals_count} proposals to vote on for group: {group_name}");

                    // Step 2: Start voting process - steward gets proposals for voting
                    let (_, user_action) = match user_clone
                        .write()
                        .await
                        .get_proposals_for_steward_voting(&group_name)
                        .await
                    {
                        Ok((proposal_id, user_action)) => (proposal_id, user_action),
                        Err(e) => {
                            tracing::warn!(
                                    "get proposals for steward voting failed for group {group_name:?}: {e}"
                                );
                            continue;
                        }
                    };

                    // Step 3: Send proposals to ws to steward to vote or do nothing if no proposals
                    // After voting, steward sends vote and proposal to waku node and start consensus process
                    handle_user_action(user_action, &core, &evt_tx_clone).await;
                }
            }
        });
        tracing::debug!("User started sending steward message for group {group_name_clone:?}");
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
        core.groups.insert(group_name.clone()).await;
        tracing::debug!("User joined group {group_name}");
        tracing::debug!(
            "User have topic for group {:?}",
            core.topics.snapshot().await
        );
        let packet = user_ref
            .write()
            .await
            .build_key_package_message(&group_name)
            .await?;
        core.app_state.delivery.send(packet).await?;
        tracing::debug!("User sent key package message for group {group_name}");
        Ok(())
    }

    pub async fn send_message(&self, group_name: String, message: String) -> anyhow::Result<()> {
        let core = self.core();
        let user_ref = self.user()?;
        let packet = user_ref
            .read()
            .await
            .send_message(&group_name, message.into_bytes())
            .await?;
        core.app_state.delivery.send(packet).await?;
        tracing::info!("sent message to group: {:?}", &group_name);
        Ok(())
    }

    pub async fn send_ban_request(
        &self,
        group_name: String,
        user_to_ban: String,
    ) -> anyhow::Result<()> {
        let core = self.core();
        let user_ref = self.user()?;

        let ban_request = BanRequest {
            user_to_ban: user_to_ban.clone(),
            requester: String::new(),
            group_name: group_name.clone(),
        };

        let action = user_ref
            .write()
            .await
            .process_ban_request(ban_request, &group_name)
            .await?;

        handle_user_action(action, &core, &self.evt_tx).await;

        Ok(())
    }

    pub async fn process_user_vote(
        &self,
        group_name: String,
        proposal_id: u32,
        vote: bool,
    ) -> anyhow::Result<()> {
        let core = self.core();
        let user_ref = self.user()?;

        let action = user_ref
            .write()
            .await
            .process_user_vote(&group_name, proposal_id, vote)
            .await?;

        handle_user_action(action, &core, &self.evt_tx).await;

        Ok(())
    }

    pub async fn group_list(&self) -> Vec<String> {
        let core = self.core();
        core.groups.all().await
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

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        group_name: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let user_ref = self.user()?;
        let proposals = user_ref
            .read()
            .await
            .get_current_epoch_proposals(&group_name)
            .await?;
        let display_proposals: Vec<(String, String)> = proposals
            .iter()
            .map(|proposal| match proposal {
                GroupUpdateRequest::AddMember(kp) => {
                    let address = kp.address_hex();
                    ("Add Member".to_string(), address)
                }
                GroupUpdateRequest::RemoveMember(id) => ("Remove Member".to_string(), id.clone()),
            })
            .collect();
        Ok(display_proposals)
    }

    pub async fn get_group_members(&self, group_name: String) -> anyhow::Result<Vec<String>> {
        let user_ref = self.user()?;
        let members = user_ref.read().await.get_group_members(&group_name).await?;
        Ok(members)
    }
}
