use std::time::Duration;
use tracing::info;

use de_mls::{
    protos::de_mls::messages::v1::{app_message, BanRequest},
    steward,
    user::UserAction,
    user_actor::{
        CreateGroupRequest, GetCurrentEpochProposalsRequest, GetGroupMembersRequest,
        GetProposalsForStewardVotingRequest, IsStewardStatusRequest, SendGroupMessage,
        StartStewardEpochRequest, StewardMessageRequest, UserVoteRequest,
    },
    user_app_instance::STEWARD_EPOCH,
};
use de_mls_ui_protocol::v1::AppEvent;

use crate::Gateway;

impl Gateway {
    pub async fn create_group(&self, group_name: String) -> anyhow::Result<()> {
        let core = self.core();
        let user = self.user()?;
        user.ask(CreateGroupRequest {
            group_name: group_name.clone(),
            is_creation: true,
        })
        .await?;
        core.topics.add_many(&group_name).await;
        core.groups.insert(group_name.clone()).await;
        info!("User start sending steward message for group {group_name:?}");
        let user_clone = user.clone();
        let group_name_clone = group_name.clone();
        let evt_tx_clone = self.evt_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(STEWARD_EPOCH));
            loop {
                interval.tick().await;
                // Step 1: Start steward epoch - check for proposals and start epoch if needed
                let proposals_count = match user_clone
                    .ask(StartStewardEpochRequest {
                        group_name: group_name.clone(),
                    })
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

                // Step 2: Send new steward key to the waku node for new epoch
                let msg = match user_clone
                    .ask(StewardMessageRequest {
                        group_name: group_name.clone(),
                    })
                    .await
                {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::warn!(
                            "steward message request failed for group {group_name:?}: {e}"
                        );
                        continue;
                    }
                };
                if let Err(e) = core.app_state.waku_node.send(msg).await {
                    tracing::warn!("failed to send steward message for group {group_name:?}: {e}");
                    continue;
                }

                if proposals_count == 0 {
                    info!("No proposals to vote on for group: {group_name}, completing epoch without voting");
                } else {
                    info!("Found {proposals_count} proposals to vote on for group: {group_name}");

                    // Step 3: Start voting process - steward gets proposals for voting
                    let action = match user_clone
                        .ask(GetProposalsForStewardVotingRequest {
                            group_name: group_name.clone(),
                        })
                        .await
                    {
                        Ok(action) => action,
                        Err(e) => {
                            tracing::warn!(
                                "get proposals for steward voting failed for group {group_name:?}: {e}"
                            );
                            continue;
                        }
                    };

                    // Step 4: Send proposals to ws to steward to vote or do nothing if no proposals
                    // After voting, steward sends vote and proposal to waku node and start consensus process
                    match action {
                        UserAction::SendToApp(app_msg) => {
                            if let Some(app_message::Payload::VotePayload(vp)) = &app_msg.payload {
                                if let Err(e) =
                                    evt_tx_clone.unbounded_send(AppEvent::VoteRequested(vp.clone()))
                                {
                                    tracing::warn!("failed to send vote requested event: {e}");
                                }

                                // Also clear current epoch proposals when voting starts
                                if let Err(e) = evt_tx_clone.unbounded_send(
                                    AppEvent::CurrentEpochProposalsCleared {
                                        group_id: group_name.clone(),
                                    },
                                ) {
                                    tracing::warn!("failed to send proposals cleared event: {e}");
                                }
                            }
                            if let Some(app_message::Payload::ProposalAdded(pa)) = &app_msg.payload
                            {
                                if let Err(e) =
                                    evt_tx_clone.unbounded_send(AppEvent::from(pa.clone()))
                                {
                                    tracing::warn!("failed to send proposal added event: {e}");
                                }
                            }
                        }
                        UserAction::DoNothing => {
                            info!("No action to take for group: {group_name}");
                            return Ok(());
                        }
                        _ => {
                            return Err(anyhow::anyhow!("Invalid user action: {action}"));
                        }
                    }
                }
            }
        });
        tracing::debug!("User started sending steward message for group {group_name_clone:?}");
        Ok(())
    }

    pub async fn join_group(&self, group_name: String) -> anyhow::Result<()> {
        let core = self.core();
        let user = self.user()?;
        user.ask(CreateGroupRequest {
            group_name: group_name.clone(),
            is_creation: false,
        })
        .await?;
        core.topics.add_many(&group_name).await;
        core.groups.insert(group_name.clone()).await;
        tracing::debug!("User joined group {group_name}");
        tracing::debug!(
            "User have topic for group {:?}",
            core.topics.snapshot().await
        );
        Ok(())
    }

    pub async fn send_message(&self, group_name: String, message: String) -> anyhow::Result<()> {
        let core = self.core();
        let user = self.user()?;
        let pmt = user
            .ask(SendGroupMessage {
                message: message.clone().into_bytes(),
                group_name: group_name.clone(),
            })
            .await?;
        core.app_state.waku_node.send(pmt).await?;
        Ok(())
    }

    pub async fn send_ban_request(
        &self,
        group_name: String,
        user_to_ban: String,
    ) -> anyhow::Result<()> {
        let core = self.core();
        let user = self.user()?;

        let ban_request = BanRequest {
            user_to_ban: user_to_ban.clone(),
            requester: String::new(),
            group_name: group_name.clone(),
        };

        let msg = user
            .ask(de_mls::user_actor::BuildBanMessage {
                ban_request,
                group_name: group_name.clone(),
            })
            .await?;
        match msg {
            UserAction::SendToWaku(msg) => {
                core.app_state.waku_node.send(msg).await?;
            }
            UserAction::SendToApp(app_msg) => {
                let event = match app_msg.payload {
                    Some(app_message::Payload::ProposalAdded(ref proposal)) => {
                        AppEvent::from(proposal.clone())
                    }
                    Some(app_message::Payload::BanRequest(ref ban_request)) => {
                        AppEvent::from(ban_request.clone())
                    }
                    _ => return Err(anyhow::anyhow!("Invalid user action")),
                };
                self.push_event(event);
            }
            _ => return Err(anyhow::anyhow!("Invalid user action")),
        }

        Ok(())
    }

    pub async fn process_user_vote(
        &self,
        group_name: String,
        proposal_id: u32,
        vote: bool,
    ) -> anyhow::Result<()> {
        let user = self.user()?;

        let user_vote_result = user
            .ask(UserVoteRequest {
                group_name: group_name.clone(),
                proposal_id,
                vote,
            })
            .await?;
        if let Some(waku_msg) = user_vote_result {
            self.core().app_state.waku_node.send(waku_msg).await?;
        }
        Ok(())
    }

    pub async fn group_list(&self) -> Vec<String> {
        let core = self.core();
        core.groups.all().await
    }

    pub async fn get_steward_status(&self, group_name: String) -> anyhow::Result<bool> {
        let user = self.user()?;
        let is_steward = user.ask(IsStewardStatusRequest { group_name }).await?;
        Ok(is_steward)
    }

    /// Get current epoch proposals for the given group
    pub async fn get_current_epoch_proposals(
        &self,
        group_name: String,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let user = self.user()?;

        let proposals = user
            .ask(GetCurrentEpochProposalsRequest { group_name })
            .await?;
        let display_proposals: Vec<(String, String)> = proposals
            .iter()
            .map(|proposal| match proposal {
                steward::GroupUpdateRequest::AddMember(kp) => {
                    let address = format!(
                        "0x{}",
                        hex::encode(kp.leaf_node().credential().serialized_content())
                    );
                    ("Add Member".to_string(), address)
                }
                steward::GroupUpdateRequest::RemoveMember(id) => {
                    ("Remove Member".to_string(), id.clone())
                }
            })
            .collect();
        Ok(display_proposals)
    }

    pub async fn get_group_members(&self, group_name: String) -> anyhow::Result<Vec<String>> {
        let user = self.user()?;
        let members = user
            .ask(GetGroupMembersRequest {
                group_name: group_name.clone(),
            })
            .await?;
        Ok(members)
    }
}
