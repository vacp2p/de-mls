use alloy::{
    primitives::Address,
    signers::{local::PrivateKeySigner, Signer},
};
use kameo::Actor;
use log::info;
use openmls::{
    group::MlsGroupJoinConfig,
    prelude::{DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, StagedWelcome, Welcome},
};
use prost::Message;
use std::{collections::HashMap, fmt::Display, str::FromStr, sync::Arc};
use tokio::sync::RwLock;
use waku_bindings::WakuMessage;

use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::{
    identity::Identity,
    openmls_provider::{MlsProvider, CIPHERSUITE},
};

use crate::{
    consensus::{ConsensusConfig, ConsensusService},
    error::UserError,
    group::{Group, GroupAction},
    message::{wrap_conversation_message_into_application_msg, wrap_user_kp_into_welcome_msg},
    protos::messages::v1::{
        app_message, consensus::v1::Proposal, welcome_message, AppMessage, BanRequest,
        BatchProposalsMessage, WelcomeMessage,
    },
    LocalSigner,
};

#[derive(Debug, Clone, PartialEq)]
pub enum UserAction {
    SendToWaku(WakuMessageToSend),
    SendToApp(AppMessage),
    LeaveGroup(String),
    DoNothing,
}

impl Display for UserAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserAction::SendToWaku(_) => write!(f, "SendToWaku"),
            UserAction::SendToApp(_) => write!(f, "SendToApp"),
            UserAction::LeaveGroup(group_name) => write!(f, "LeaveGroup({group_name})"),
            UserAction::DoNothing => write!(f, "DoNothing"),
        }
    }
}

#[derive(Actor)]
pub struct User {
    identity: Identity,
    // Each group has its own lock for better concurrency
    groups: Arc<RwLock<HashMap<String, Arc<RwLock<Group>>>>>,
    provider: MlsProvider,
    consensus_service: ConsensusService,
    eth_signer: PrivateKeySigner,
}

impl User {
    pub fn new(user_eth_priv_key: &str) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(user_eth_priv_key)?;
        let user_address = signer.address();

        let crypto = MlsProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, user_address.as_slice())?;

        let user = User {
            groups: Arc::new(RwLock::new(HashMap::new())),
            identity: id,
            eth_signer: signer,
            provider: crypto,
            consensus_service: ConsensusService::new(ConsensusConfig::default()),
        };
        Ok(user)
    }

    pub async fn create_group(
        &mut self,
        group_name: String,
        is_creation: bool,
    ) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        if groups.contains_key(&group_name) {
            return Err(UserError::GroupAlreadyExistsError(group_name));
        }
        let group = if is_creation {
            Group::new(
                group_name.clone(),
                true,
                Some(&self.provider),
                Some(self.identity.signer()),
                Some(&self.identity.credential_with_key()),
            )?
        } else {
            Group::new(group_name.clone(), false, None, None, None)?
        };

        groups.insert(group_name.clone(), Arc::new(RwLock::new(group)));
        Ok(())
    }

    pub async fn get_group(&self, group_name: String) -> Result<Group, UserError> {
        let groups = self.groups.read().await;
        match groups.get(&group_name) {
            Some(g) => Ok(g.read().await.clone()),
            None => Err(UserError::GroupNotFoundError(group_name)),
        }
    }

    pub async fn if_group_exists(&self, group_name: String) -> bool {
        let groups = self.groups.read().await;
        groups.contains_key(&group_name)
    }

    pub async fn process_welcome_subtopic(
        &mut self,
        msg: WakuMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        // Get the group lock first
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        let received_msg = WelcomeMessage::decode(msg.payload())?;
        if let Some(payload) = &received_msg.payload {
            match payload {
                welcome_message::Payload::GroupAnnouncement(group_announcement) => {
                    let app_id = group_lock.read().await.app_id();
                    if group_lock.read().await.is_steward().await
                        || group_lock.read().await.is_kp_shared()
                    {
                        info!("Its steward or key package already shared");
                        Ok(UserAction::DoNothing)
                    } else {
                        info!(
                            "User {:?} received group announcement message for group {:?}",
                            self.identity.identity_string(),
                            group_name
                        );
                        if !group_announcement.verify()? {
                            return Err(UserError::MessageVerificationFailed);
                        }

                        let new_kp = self.identity.generate_key_package(&self.provider)?;
                        let encrypted_key_package = group_announcement.encrypt(new_kp)?;
                        group_lock.write().await.set_kp_shared(true);

                        Ok(UserAction::SendToWaku(WakuMessageToSend::new(
                            wrap_user_kp_into_welcome_msg(encrypted_key_package)?.encode_to_vec(),
                            WELCOME_SUBTOPIC,
                            group_name.clone(),
                            app_id.clone(),
                        )))
                    }
                }
                welcome_message::Payload::UserKeyPackage(user_key_package) => {
                    if group_lock.read().await.is_steward().await {
                        info!(
                            "Steward {:?} received key package for the group {:?}",
                            self.identity.identity_string(),
                            group_name
                        );
                        let key_package = group_lock
                            .write()
                            .await
                            .decrypt_steward_msg(user_key_package.encrypt_kp.clone())
                            .await?;

                        group_lock
                            .write()
                            .await
                            .store_invite_proposal(Box::new(key_package))
                            .await?;
                        Ok(UserAction::DoNothing)
                    } else {
                        Ok(UserAction::DoNothing)
                    }
                }
                welcome_message::Payload::InvitationToJoin(invitation_to_join) => {
                    if group_lock.read().await.is_steward().await {
                        Ok(UserAction::DoNothing)
                    } else {
                        // Release the lock before calling join_group
                        drop(group_lock);

                        // Parse the MLS message to get the welcome
                        let (mls_in, _) = MlsMessageIn::tls_deserialize_bytes(
                            &invitation_to_join.mls_message_out_bytes,
                        )?;

                        let welcome = match mls_in.extract() {
                            MlsMessageBodyIn::Welcome(welcome) => welcome,
                            _ => return Err(UserError::EmptyWelcomeMessageError),
                        };

                        if welcome.secrets().iter().any(|egs| {
                            let hash_ref = egs.new_member().as_slice().to_vec();
                            self.identity.is_key_package_exists(&hash_ref)
                        }) {
                            self.join_group(welcome).await?;
                            let msg = self
                                .build_group_message("User joined to the group", group_name)
                                .await?; // TODO: check if this is correct
                            Ok(UserAction::SendToWaku(msg))
                        } else {
                            info!(
                                "User {:?} received invitation to join group {:?}, 
                                but key package is not shared or already joined",
                                self.identity.identity_string(),
                                group_name
                            );
                            Ok(UserAction::DoNothing)
                        }
                    }
                }
            }
        } else {
            Err(UserError::EmptyWelcomeMessageError)
        }
    }

    pub async fn process_app_subtopic(
        &mut self,
        msg: WakuMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        // First check if group exists
        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name.clone()));
        }

        // Get group and check if initialized
        let is_initialized = {
            let groups = self.groups.read().await;
            if let Some(_group_lock) = groups.get(&group_name) {
                _group_lock.read().await.is_mls_group_initialized()
            } else {
                false
            }
        };

        if !is_initialized {
            return Ok(UserAction::DoNothing);
        }

        info!(
            "[user::process_app_subtopic]: User {:?} received app message for group {:?}",
            self.identity.identity_string(),
            group_name
        );

        // Try to parse as AppMessage first
        if let Ok(app_message) = AppMessage::decode(msg.payload()) {
            return self.process_app_message(app_message, group_name).await;
        }

        // Fall back to MLS protocol message
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(msg.payload())?;
        let mls_message = mls_message_in.try_into_protocol_message()?;

        // Process the message
        let res = {
            let groups = self.groups.read().await;
            if let Some(_group_lock) = groups.get(&group_name) {
                _group_lock
                    .write()
                    .await
                    .process_protocol_msg(
                        mls_message,
                        &self.provider,
                        self.identity.signature_key(),
                    )
                    .await
            } else {
                return Err(UserError::GroupNotFoundError(group_name.clone()));
            }
        }?;

        info!("[user::process_app_subtopic]: start processing group_action");

        // Handle the result outside of any lock scope
        match res {
            GroupAction::GroupAppMsg(msg) => Ok(UserAction::SendToApp(msg)),
            GroupAction::LeaveGroup => Ok(UserAction::LeaveGroup(group_name)),
            GroupAction::DoNothing => Ok(UserAction::DoNothing),
            GroupAction::GroupProposal(proposal) => {
                self.process_consensus_proposal(proposal, group_name).await
            }
            GroupAction::GroupVote(vote) => self.process_consensus_vote(vote, group_name).await,
        }
    }

    /// Process AppMessage payload
    async fn process_app_message(
        &mut self,
        app_message: AppMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        // Get the group and process the message
        let result = {
            let groups = self.groups.read().await;
            if let Some(_group_lock) = groups.get(&group_name) {
                match app_message.payload {
                    Some(app_message::Payload::BatchProposalsMessage(batch_msg)) => {
                        info!(
                            "[user::process_app_message]: Processing batch proposals message for group {group_name}"
                        );
                        // Release the lock before calling self methods
                        drop(groups);
                        self.process_batch_proposals_message(batch_msg, group_name)
                            .await
                    }
                    Some(app_message::Payload::ConversationMessage(conversation_msg)) => {
                        info!(
                            "[user::process_app_message]: Processing conversation message for group {group_name}"
                        );
                        // Release the lock before calling self methods
                        drop(groups);
                        // For now, just return the conversation message as-is
                        Ok(UserAction::SendToApp(AppMessage {
                            payload: Some(app_message::Payload::ConversationMessage(
                                conversation_msg,
                            )),
                        }))
                    }
                    Some(app_message::Payload::Proposal(proposal)) => {
                        info!(
                            "[user::process_app_message]: Processing consensus proposal for group {group_name}"
                        );
                        // Release the lock before calling self methods
                        drop(groups);
                        self.process_consensus_proposal(proposal, group_name).await
                    }
                    Some(app_message::Payload::Vote(vote)) => {
                        info!(
                            "[user::process_app_message]: Processing consensus vote for group {group_name}"
                        );
                        // Release the lock before calling self methods
                        drop(groups);
                        self.process_consensus_vote(vote, group_name).await
                    }
                    Some(app_message::Payload::BanRequest(ban_request)) => {
                        info!(
                            "[user::process_app_message]: Processing ban request for group {group_name}"
                        );
                        // Release the lock before calling self methods
                        drop(groups);
                        self.process_ban_request(ban_request, group_name).await
                    }
                    None => {
                        info!(
                            "[user::process_app_message]: No payload found in app message for group {group_name}"
                        );
                        Ok(UserAction::DoNothing)
                    }
                }
            } else {
                return Err(UserError::GroupNotFoundError(group_name));
            }
        };
        result
    }

    /// Process batch proposals message
    async fn process_batch_proposals_message(
        &mut self,
        batch_msg: BatchProposalsMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        info!(
            "[user::process_batch_proposals_message]: Processing batch proposals for group {group_name}"
        );

        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        // Process all proposals before the commit
        for proposal_bytes in batch_msg.mls_proposals {
            let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&proposal_bytes)?;
            let protocol_message = mls_message_in.try_into_protocol_message()?;

            // Process the proposal
            let _res = group_lock
                .write()
                .await
                .process_protocol_msg(
                    protocol_message,
                    &self.provider,
                    self.identity.signature_key(),
                )
                .await?;
        }

        // Then process the commit message
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&batch_msg.commit_message)?;
        let protocol_message = mls_message_in.try_into_protocol_message()?;

        let res = group_lock
            .write()
            .await
            .process_protocol_msg(
                protocol_message,
                &self.provider,
                self.identity.signature_key(),
            )
            .await?;

        // Handle the result outside of any lock scope
        match res {
            GroupAction::GroupAppMsg(msg) => Ok(UserAction::SendToApp(msg)),
            GroupAction::LeaveGroup => Ok(UserAction::LeaveGroup(group_name)),
            GroupAction::DoNothing => Ok(UserAction::DoNothing),
            GroupAction::GroupProposal(proposal) => {
                self.process_consensus_proposal(proposal, group_name).await
            }
            GroupAction::GroupVote(vote) => self.process_consensus_vote(vote, group_name).await,
        }
    }

    /// This function is used to process waku messages
    /// After processing the message, it returns an action to be performed by the user
    ///  - SendToWaku - send a message to the waku network
    ///  - SendToGroup - send a message to the group (print to the application)
    ///  - LeaveGroup - leave a group
    ///  - DoNothing - do nothing
    pub async fn process_waku_message(
        &mut self,
        msg: WakuMessage,
    ) -> Result<UserAction, UserError> {
        let group_name = msg.content_topic.application_name.to_string();

        // Check if group exists and get app_id
        let (group_exists, app_id) = {
            let groups = self.groups.read().await;
            let group = groups.get(&group_name);
            match group {
                Some(g) => (true, g.read().await.app_id()),
                None => (false, Vec::new()),
            }
        };

        if !group_exists {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        if msg.meta == app_id {
            info!("Message is from the same app, skipping");
            return Ok(UserAction::DoNothing);
        }

        let ct_name = msg.content_topic.content_topic_name.to_string();
        info!(
            "[user::process_waku_message]: Processing waku message from content topic: {ct_name}"
        );

        match ct_name.as_str() {
            WELCOME_SUBTOPIC => self.process_welcome_subtopic(msg, group_name).await,
            APP_MSG_SUBTOPIC => self.process_app_subtopic(msg, group_name).await,
            _ => Err(UserError::UnknownContentTopicType(ct_name)),
        }
    }

    /// This function is used to join a group after receiving a welcome message
    async fn join_group(&mut self, welcome: Welcome) -> Result<(), UserError> {
        let group_config = MlsGroupJoinConfig::builder().build();
        let mls_group =
            StagedWelcome::new_from_welcome(&self.provider, &group_config, welcome, None)?
                .into_group(&self.provider)?;

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id)?;

        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        // Get the group lock and set the MLS group
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        group_lock.write().await.set_mls_group(mls_group)?;

        info!(
            "User {:?} joined group {:?}",
            self.identity.identity_string(),
            group_name
        );
        Ok(())
    }

    /// This function is used to leave a group after receiving a commit message
    pub async fn leave_group(&mut self, group_name: String) -> Result<(), UserError> {
        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name));
        }
        let mut groups = self.groups.write().await;
        groups.remove(&group_name);
        Ok(())
    }

    pub async fn prepare_steward_msg(
        &mut self,
        group_name: String,
    ) -> Result<WakuMessageToSend, UserError> {
        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        let msg_to_send = group_lock.write().await.generate_steward_message().await?;
        Ok(msg_to_send)
    }

    pub async fn build_group_message(
        &mut self,
        msg: &str,
        group_name: String,
    ) -> Result<WakuMessageToSend, UserError> {
        info!("Start building group message");

        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        if !group_lock.read().await.is_mls_group_initialized() {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        let app_msg = wrap_conversation_message_into_application_msg(
            msg.as_bytes().to_vec(),
            self.identity.identity_string(),
            group_name.clone(),
        );

        let msg_to_send = group_lock
            .write()
            .await
            .build_message(&self.provider, self.identity.signer(), &app_msg)
            .await?;

        info!("End building group message");
        Ok(msg_to_send)
    }

    /// Get the identity string for debugging purposes
    pub fn identity_string(&self) -> String {
        self.identity.identity_string()
    }

    /// Apply proposals for the given group, returning the batch message(s).
    pub async fn apply_proposals(
        &mut self,
        group_name: String,
    ) -> Result<Vec<WakuMessageToSend>, UserError> {
        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        let messages = group_lock
            .write()
            .await
            .create_batch_proposals_message(&self.provider, self.identity.signer())
            .await?;
        Ok(messages)
    }

    /// Remove proposals and complete the steward epoch for the given group.
    pub async fn empty_proposals_queue_and_complete(
        &mut self,
        group_name: String,
    ) -> Result<(), UserError> {
        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        group_lock
            .write()
            .await
            .remove_proposals_and_complete()
            .await?;
        Ok(())
    }

    /// Start a new steward epoch for the given group.
    /// Returns the number of proposals that will be voted on.
    /// If there are no proposals, returns 0 and doesn't change state.
    pub async fn start_steward_epoch(&mut self, group_name: String) -> Result<usize, UserError> {
        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        // Check if there are proposals in the current epoch
        let proposal_count = group_lock.read().await.get_pending_proposals_count().await;

        if proposal_count == 0 {
            // No proposals to vote on, return 0 and don't change state
            info!("No proposals to vote on, skipping steward epoch");
            return Ok(0);
        }

        // There are proposals, start the steward epoch
        info!("Starting steward epoch with {proposal_count} proposals");
        group_lock.write().await.start_steward_epoch().await?;
        Ok(proposal_count)
    }

    /// Start voting for the given group, returning the proposal ID.
    pub async fn start_voting(
        &mut self,
        group_name: String,
    ) -> Result<(u32, UserAction), UserError> {
        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        // If this is the steward, create proposal with vote and send to group
        if group_lock.read().await.is_steward().await {
            let proposals = group_lock
                .read()
                .await
                .get_proposals_for_voting_epoch()
                .await;
            if !proposals.is_empty() {
                // Change state to Voting
                group_lock.write().await.start_voting().await?;

                // Get group members for expected voters count
                let members = group_lock.read().await.members_identity().await?;
                let participant_ids: Vec<Vec<u8>> = members.into_iter().collect();
                let expected_voters_count = participant_ids.len() as u32;

                // Create proposal with steward's vote
                let (proposal, _steward_vote) = self
                    .consensus_service
                    .create_proposal_with_vote(
                        group_name.clone(),
                        "Group Update Proposal".to_string(),
                        format!("{proposals:?}"),
                        self.eth_signer.address().to_string().as_bytes().to_vec(),
                        true, // Steward votes yes
                        expected_voters_count,
                        300,  // 5 minutes expiration
                        true, // liveness criteria yes
                        self.eth_signer.clone(),
                    )
                    .await?;

                info!(
                    "[user::start_voting] expected voters: {expected_voters_count} for proposal: {:?}", proposal.proposal_id
                );

                // Send proposal as APP message to group
                let app_message = AppMessage {
                    payload: Some(app_message::Payload::Proposal(proposal.clone())),
                };

                let waku_msg = WakuMessageToSend::new(
                    app_message.encode_to_vec(),
                    APP_MSG_SUBTOPIC,
                    group_name.clone(),
                    group_lock.read().await.app_id(),
                );

                // Send the message and return the proposal ID as vote_id
                return Ok((proposal.proposal_id, UserAction::SendToWaku(waku_msg)));
            }
        }

        Ok((0, UserAction::DoNothing))
    }

    /// Add a remove proposal to the steward for the given group.
    pub async fn add_remove_proposal(
        &mut self,
        group_name: String,
        identity: String,
    ) -> Result<(), UserError> {
        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        let identity_bytes = alloy::hex::decode(&identity)?;
        group_lock
            .write()
            .await
            .store_remove_proposal(identity_bytes)
            .await?;
        Ok(())
    }

    /// Complete voting for the given group and vote ID, returning the result.
    /// This method waits for consensus to be reached with a timeout.
    pub async fn complete_voting(
        &mut self,
        group_name: String,
        proposal_id: u32,
    ) -> Result<bool, UserError> {
        if !self.if_group_exists(group_name.clone()).await {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        // Steward behavior: wait for all votes or timeout, then check 2n/3 threshold
        info!("Steward: Waiting for consensus on proposal {proposal_id} for group {group_name}");

        // Wait for consensus to be reached (timeout after 30 seconds)
        if let Ok(vote_result) = self
            .consensus_service
            .wait_for_consensus(group_name.clone(), proposal_id, 30)
            .await
        {
            // Update group state based on vote result
            group_lock
                .write()
                .await
                .complete_voting(vote_result)
                .await?;

            info!("Steward: Consensus reached for proposal {proposal_id}: {vote_result}");

            // If vote passed, send commit message
            if vote_result {
                info!("Steward: Vote passed, sending commit message");
            }

            Ok(vote_result)
        } else {
            // Timeout reached without consensus
            info!("Steward: Unexpected timeout for proposal {proposal_id}");
            Ok(false)
        }
    }

    /// Get the number of pending proposals for the given group.
    pub async fn get_pending_proposals_count(
        &self,
        group_name: String,
    ) -> Result<usize, UserError> {
        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        let count = group_lock.read().await.get_pending_proposals_count().await;
        Ok(count)
    }

    /// Process incoming ban request
    async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        let user_to_ban = String::from_utf8_lossy(&ban_request.user_to_ban);
        info!(
            "[user::process_ban_request]: Processing ban request from {} for user {} in group {}",
            ban_request.requester, user_to_ban, group_name
        );

        // Check if this user is the steward for this group
        let is_steward = {
            let groups = self.groups.read().await;
            if let Some(group_lock) = groups.get(&group_name) {
                group_lock.read().await.is_steward().await
            } else {
                false
            }
        };

        if is_steward {
            // Steward: add the remove proposal to the queue
            info!(
                "[user::process_ban_request]: Steward adding remove proposal for user {user_to_ban}"
            );
            self.add_remove_proposal(group_name.clone(), user_to_ban.to_string())
                .await?;

            // Send system message to the group
            let system_message = format!(
                "Remove proposal for user {} added to steward queue by {}",
                user_to_ban, ban_request.requester
            );
            let app_message = wrap_conversation_message_into_application_msg(
                system_message.into_bytes(),
                "system".to_string(),
                group_name.clone(),
            );
            Ok(UserAction::SendToApp(app_message))
        } else {
            // Non-steward: just show a system message
            info!("[user::process_ban_request]: Non-steward user received ban request for user {user_to_ban}" );
            let system_message = format!(
                "User {} wants to ban user {} (only steward can execute)",
                ban_request.requester, user_to_ban
            );
            let app_message = wrap_conversation_message_into_application_msg(
                system_message.into_bytes(),
                "system".to_string(),
                group_name.clone(),
            );
            Ok(UserAction::SendToApp(app_message))
        }
    }

    /// Process incoming consensus proposal
    async fn process_consensus_proposal(
        &mut self,
        proposal: Proposal,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        // Get the group lock
        let group_lock = {
            let groups = self.groups.read().await;
            match groups.get(&group_name) {
                Some(lock) => lock.clone(),
                None => return Err(UserError::GroupNotFoundError(group_name.clone())),
            }
        };

        // Change state to Voting if not already
        if group_lock.read().await.get_state().await != crate::state_machine::GroupState::Voting {
            group_lock.write().await.start_voting().await?;
        }

        // Process the proposal in consensus service
        let vote = self
            .consensus_service
            .process_incoming_proposal(group_name.clone(), proposal, self.eth_signer.clone())
            .await?;

        // Send vote as APP message to group
        let app_message = AppMessage {
            payload: Some(app_message::Payload::Vote(vote.clone())),
        };

        let waku_msg = WakuMessageToSend::new(
            app_message.encode_to_vec(),
            APP_MSG_SUBTOPIC,
            group_name.clone(),
            group_lock.read().await.app_id(),
        );

        Ok(UserAction::SendToWaku(waku_msg))
    }

    /// Process incoming consensus vote
    async fn process_consensus_vote(
        &mut self,
        vote: crate::protos::messages::v1::consensus::v1::Vote,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        // Process the vote in consensus service
        self.consensus_service
            .process_incoming_vote(group_name.clone(), vote.clone())
            .await?;

        // Check if consensus has been reached immediately after adding this vote
        if let Some(result) = self
            .consensus_service
            .get_consensus_result(group_name.clone(), vote.vote_id)
            .await
        {
            // Consensus reached, handle result
            if result {
                // Vote passed - steward should send commit message
                info!("Consensus reached: YES - steward should send commit message");
            } else {
                // Vote failed - return to working state
                info!("Consensus reached: NO - returning to working state");
            }
        }

        Ok(UserAction::DoNothing)
    }
}

impl LocalSigner for PrivateKeySigner {
    async fn local_sign_message(&self, message: &[u8]) -> Result<Vec<u8>, anyhow::Error> {
        let signature = self.sign_message(message).await?;
        let signature_bytes = signature.as_bytes().to_vec();
        Ok(signature_bytes)
    }

    fn get_address(&self) -> Address {
        self.address()
    }
}
