use ds::waku::WakuDeliveryService;
use hex::ToHex;
use tracing::info;

use crate::forwarder::push_consensus_state;
use crate::Gateway;
use de_mls::{
    app::{IntervalScheduler, StewardScheduler, StewardSchedulerConfig},
    protos::de_mls::messages::v1::{group_update_request, BanRequest},
};

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
        core.groups.insert(group_name.clone()).await;
        info!("User start sending steward message for group {group_name:?}");

        let user_clone = user_ref.clone();
        let evt_tx = self.evt_tx.clone();
        let group_name_clone = group_name.clone();
        tokio::spawn(async move {
            let mut scheduler = IntervalScheduler::new(StewardSchedulerConfig::default());
            loop {
                scheduler.next_tick().await;
                if let Err(e) = user_clone
                    .write()
                    .await
                    .start_steward_epoch(&group_name)
                    .await
                {
                    tracing::warn!(
                        "start steward epoch request failed for group {group_name:?}: {e}"
                    );
                    continue;
                }
                push_consensus_state(&user_clone, &evt_tx, &group_name).await;
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
        user_ref.write().await.send_kp_message(&group_name).await?;
        tracing::debug!("User sent key package message for group {group_name}");

        // Spawn member epoch timer (same interval as steward)
        // When the timer fires, check if we have approved proposals and enter Waiting state.
        let user_clone = user_ref.clone();
        let group_name_clone = group_name.clone();
        tokio::spawn(async move {
            let mut scheduler = IntervalScheduler::new(StewardSchedulerConfig::default());
            loop {
                scheduler.next_tick().await;
                if let Err(e) = user_clone
                    .read()
                    .await
                    .start_member_epoch(&group_name_clone)
                    .await
                {
                    tracing::warn!(
                        "start member epoch check failed for group {group_name_clone:?}: {e}"
                    );
                }
            }
        });

        Ok(())
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
                None => return Err(anyhow::anyhow!("message")),
            }
        }
        Ok(display_proposals)
    }

    pub async fn get_group_members(&self, group_name: String) -> anyhow::Result<Vec<String>> {
        let user_ref = self.user()?;
        let members = user_ref.read().await.get_group_members(&group_name).await?;
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
                    None => {}
                }
            }
            result.push(display_batch);
        }
        Ok(result)
    }
}
