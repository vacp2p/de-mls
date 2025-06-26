use alloy::signers::local::PrivateKeySigner;
use kameo::Actor;
use log::info;
use openmls::{
    group::MlsGroupJoinConfig,
    prelude::{DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, StagedWelcome, Welcome},
};
use prost::Message;
use std::{collections::HashMap, str::FromStr};
use waku_bindings::WakuMessage;

use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::{
    identity::Identity,
    openmls_provider::{MlsProvider, CIPHERSUITE},
};

use crate::{
    error::UserError,
    group::{Group, GroupAction},
    message::{wrap_conversation_message_into_application_msg, wrap_user_kp_into_welcome_msg},
    protos::messages::v1::{
        app_message, welcome_message, AppMessage, BatchProposalsMessage, WelcomeMessage,
    },
};

#[derive(Debug, Clone, PartialEq)]
pub enum UserAction {
    SendToWaku(WakuMessageToSend),
    SendToApp(AppMessage),
    LeaveGroup(String),
    DoNothing,
}

#[derive(Actor)]
pub struct User {
    identity: Identity,
    groups: HashMap<String, Group>,
    provider: MlsProvider,
    _eth_signer: PrivateKeySigner,
}

impl User {
    pub fn new(user_eth_priv_key: &str) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(user_eth_priv_key)?;
        let user_address = signer.address();

        let crypto = MlsProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, user_address.as_slice())?;

        let user = User {
            groups: HashMap::new(),
            identity: id,
            _eth_signer: signer,
            provider: crypto,
        };
        Ok(user)
    }

    pub async fn create_group(
        &mut self,
        group_name: String,
        is_creation: bool,
    ) -> Result<(), UserError> {
        if self.if_group_exists(group_name.clone()) {
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

        self.groups.insert(group_name.clone(), group);
        Ok(())
    }

    pub fn get_group(&self, group_name: String) -> Result<Group, UserError> {
        match self.groups.get(&group_name) {
            Some(g) => Ok(g.clone()),
            None => Err(UserError::GroupNotFoundError(group_name)),
        }
    }

    pub fn if_group_exists(&self, group_name: String) -> bool {
        self.groups.contains_key(&group_name)
    }

    pub async fn process_welcome_subtopic(
        &mut self,
        msg: WakuMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        let received_msg = WelcomeMessage::decode(msg.payload())?;
        if let Some(payload) = &received_msg.payload {
            match payload {
                welcome_message::Payload::GroupAnnouncement(group_announcement) => {
                    let app_id = group.app_id();
                    if group.is_steward() || group.is_kp_shared() {
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
                        group.set_kp_shared(true);

                        Ok(UserAction::SendToWaku(WakuMessageToSend::new(
                            wrap_user_kp_into_welcome_msg(encrypted_key_package)?.encode_to_vec(),
                            WELCOME_SUBTOPIC,
                            group_name.clone(),
                            app_id.clone(),
                        )))
                    }
                }
                welcome_message::Payload::UserKeyPackage(user_key_package) => {
                    if group.is_steward() {
                        info!(
                            "Steward {:?} received key package for the group {:?}",
                            self.identity.identity_string(),
                            group_name
                        );
                        let key_package =
                            group.decrypt_steward_msg(user_key_package.encrypt_kp.clone())?;

                        group.store_invite_proposal(Box::new(key_package)).await?;
                        Ok(UserAction::DoNothing)
                    } else {
                        Ok(UserAction::DoNothing)
                    }
                }
                welcome_message::Payload::InvitationToJoin(invitation_to_join) => {
                    if group.is_steward() {
                        Ok(UserAction::DoNothing)
                    } else {
                        info!(
                            "User {:?} received invitation to join group {:?}",
                            self.identity.identity_string(),
                            group_name
                        );

                        let (mls_in, _) = MlsMessageIn::tls_deserialize_bytes(
                            &invitation_to_join.mls_message_out_bytes,
                        )
                        .map_err(|e| UserError::MlsMessageInDeserializeError(e.to_string()))?;

                        let welcome = match mls_in.extract() {
                            MlsMessageBodyIn::Welcome(welcome) => welcome,
                            _ => return Err(UserError::EmptyWelcomeMessageError),
                        };

                        if welcome.secrets().iter().any(|egs| {
                            let hash_ref = egs.new_member().as_slice().to_vec();
                            self.identity.is_key_package_exists(&hash_ref)
                        }) {
                            self.join_group(welcome)?;
                            let msg = self
                                .build_group_message("User joined to the group", group_name)
                                .await?;
                            Ok(UserAction::SendToWaku(msg))
                        } else {
                            info!(
                                "User {:?} received invitation to join group {:?}, but key package is not shared",
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
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        if !group.is_mls_group_initialized() {
            return Ok(UserAction::DoNothing);
        }
        info!(
            "[process_app_subtopic]: User {:?} received app message for group {:?}",
            self.identity.identity_string(),
            group_name
        );

        // Try to parse as AppMessage first
        if let Ok(app_message) = AppMessage::decode(msg.payload()) {
            return self.process_app_message(app_message, group_name).await;
        }

        // Fall back to MLS protocol message
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(msg.payload())
            .map_err(|e| UserError::MlsMessageInDeserializeError(e.to_string()))?;
        let mls_message = mls_message_in
            .try_into_protocol_message()
            .map_err(|e| UserError::TryIntoProtocolMessageError(e.to_string()))?;

        let res = group
            .process_protocol_msg(mls_message, &self.provider, self.identity.signature_key())
            .await?;

        match res {
            GroupAction::GroupAppMsg(msg) => Ok(UserAction::SendToApp(msg)),
            GroupAction::LeaveGroup => Ok(UserAction::LeaveGroup(group_name)),
            GroupAction::DoNothing => Ok(UserAction::DoNothing),
        }
    }

    /// Process AppMessage payload
    async fn process_app_message(
        &mut self,
        app_message: AppMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        match app_message.payload {
            Some(app_message::Payload::BatchProposalsMessage(batch_msg)) => {
                self.process_batch_proposals_message(batch_msg, group_name)
                    .await
            }
            Some(app_message::Payload::ConversationMessage(conv_msg)) => {
                Ok(UserAction::SendToApp(AppMessage {
                    payload: Some(app_message::Payload::ConversationMessage(conv_msg)),
                }))
            }
            Some(app_message::Payload::VoteStartMessage(vote_msg)) => {
                Ok(UserAction::SendToApp(AppMessage {
                    payload: Some(app_message::Payload::VoteStartMessage(vote_msg)),
                }))
            }
            _ => Ok(UserAction::DoNothing),
        }
    }

    /// Process batch proposals message
    async fn process_batch_proposals_message(
        &mut self,
        batch_msg: BatchProposalsMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        // First, process all proposals before the commit
        for proposal_bytes in batch_msg.mls_proposals {
            let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&proposal_bytes)
                .map_err(|e| UserError::MlsMessageInDeserializeError(e.to_string()))?;

            let protocol_message = mls_message_in
                .try_into_protocol_message()
                .map_err(|e| UserError::TryIntoProtocolMessageError(e.to_string()))?;

            // Process the proposal
            let _res = group
                .process_protocol_msg(
                    protocol_message,
                    &self.provider,
                    self.identity.signature_key(),
                )
                .await?;
        }

        // Then process the commit message
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&batch_msg.commit_message)
            .map_err(|e| UserError::MlsMessageInDeserializeError(e.to_string()))?;

        let protocol_message = mls_message_in
            .try_into_protocol_message()
            .map_err(|e| UserError::TryIntoProtocolMessageError(e.to_string()))?;

        let res = group
            .process_protocol_msg(
                protocol_message,
                &self.provider,
                self.identity.signature_key(),
            )
            .await?;

        match res {
            GroupAction::GroupAppMsg(msg) => Ok(UserAction::SendToApp(msg)),
            GroupAction::LeaveGroup => Ok(UserAction::LeaveGroup(group_name)),
            GroupAction::DoNothing => Ok(UserAction::DoNothing),
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
        let group = match self.groups.get(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let app_id = group.app_id();
        if msg.meta == app_id {
            info!("Message is from the same app, skipping");
            return Ok(UserAction::DoNothing);
        }
        let ct_name = msg.content_topic.content_topic_name.to_string();
        info!("Processing waku message from content topic: {:?}", ct_name);
        match ct_name.as_str() {
            WELCOME_SUBTOPIC => self.process_welcome_subtopic(msg, group_name).await,
            APP_MSG_SUBTOPIC => self.process_app_subtopic(msg, group_name).await,
            _ => Err(UserError::UnknownContentTopicType(ct_name)),
        }
    }

    /// This function is used to join a group after receiving a welcome message
    fn join_group(&mut self, welcome: Welcome) -> Result<(), UserError> {
        let group_config = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group =
            StagedWelcome::new_from_welcome(&self.provider, &group_config, welcome, None)?
                .into_group(&self.provider)?;

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id)?;

        if !self.if_group_exists(group_name.clone()) {
            return Err(UserError::GroupNotFoundError(group_name));
        }

        self.groups
            .get_mut(&group_name)
            .unwrap()
            .set_mls_group(mls_group)?;

        info!(
            "User {:?} joined group {:?}",
            self.identity.identity_string(),
            group_name
        );
        Ok(())
    }

    /// This function is used to leave a group after receiving a commit message
    pub async fn leave_group(&mut self, group_name: String) -> Result<(), UserError> {
        if !self.if_group_exists(group_name.clone()) {
            return Err(UserError::GroupNotFoundError(group_name));
        }
        self.groups.remove(&group_name);
        Ok(())
    }

    pub async fn prepare_steward_msg(
        &mut self,
        group_name: String,
    ) -> Result<WakuMessageToSend, UserError> {
        if !self.if_group_exists(group_name.clone()) {
            return Err(UserError::GroupNotFoundError(group_name));
        }
        let msg_to_send = self
            .groups
            .get_mut(&group_name)
            .unwrap()
            .generate_steward_message()?;
        Ok(msg_to_send)
    }

    pub async fn build_group_message(
        &mut self,
        msg: &str,
        group_name: String,
    ) -> Result<WakuMessageToSend, UserError> {
        info!("Start building group message");
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        if !group.is_mls_group_initialized() {
            return Err(UserError::GroupNotFoundError(group_name));
        }
        let app_msg = wrap_conversation_message_into_application_msg(
            msg.as_bytes().to_vec(),
            self.identity.identity_string(),
            group_name.clone(),
        );
        let msg_to_send = group
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
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        let messages = group
            .create_batch_proposals_message(&self.provider, self.identity.signer())
            .await?;
        Ok(messages)
    }

    /// Remove proposals and complete the steward epoch for the given group.
    pub async fn remove_proposals_and_complete(
        &mut self,
        group_name: String,
    ) -> Result<(), UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        group.remove_proposals_and_complete().await?;
        Ok(())
    }

    /// Start a new steward epoch for the given group.
    /// Returns the number of proposals that will be voted on.
    /// If there are no proposals, returns 0 and doesn't change state.
    pub async fn start_steward_epoch(&mut self, group_name: String) -> Result<usize, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        // Check if there are proposals in the current epoch
        let proposal_count = group.get_pending_proposals_count().await;

        if proposal_count == 0 {
            // No proposals to vote on, return 0 and don't change state
            info!("No proposals to vote on, skipping steward epoch");
            return Ok(0);
        }

        // There are proposals, start the steward epoch
        info!("Starting steward epoch with {} proposals", proposal_count);
        group.start_steward_epoch().await?;
        Ok(proposal_count)
    }

    /// Start voting for the given group, returning the vote ID.
    pub async fn start_voting(&mut self, group_name: String) -> Result<Vec<u8>, UserError> {
        let current_identity = self.identity.identity_string();
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        group.start_voting()?;
        let members = group.members_identity().await?;
        let participant_ids: Vec<Vec<u8>> = members.into_iter().collect();
        println!(
            "User: start_voting - participants: {:?}",
            participant_ids
                .iter()
                .map(alloy::hex::encode)
                .collect::<Vec<_>>()
        );
        println!(
            "User: start_voting - current user identity: {}",
            current_identity
        );
        let vote_id = uuid::Uuid::new_v4().as_bytes().to_vec();

        Ok(vote_id)
    }

    /// Complete voting for the given group and vote ID, returning the result.
    pub async fn complete_voting(
        &mut self,
        group_name: String,
        _vote_id: Vec<u8>,
    ) -> Result<bool, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        // Get vote result from HashGraph consensus
        let vote_result = true;

        // Update group state based on vote result
        group.complete_voting(vote_result)?;
        Ok(vote_result)
    }

    /// Submit a vote for the given vote ID.
    pub async fn submit_vote(&mut self, vote_id: Vec<u8>, vote: bool) -> Result<(), UserError> {
        let current_identity = self.identity.identity_string();
        info!("User: submit_vote - vote_id: {:?}, vote: {}", vote_id, vote);
        info!(
            "User: submit_vote - current user identity: {}",
            current_identity
        );
        Ok(())
    }

    /// Add a remove proposal to the steward for the given group.
    pub async fn add_remove_proposal(
        &mut self,
        group_name: String,
        identity: String,
    ) -> Result<(), UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let identity_bytes = alloy::hex::decode(&identity)
            .map_err(|e| UserError::ApplyProposalsError(format!("Invalid hex string: {}", e)))?;
        group.store_remove_proposal(identity_bytes).await?;
        Ok(())
    }

    /// Get the number of pending proposals for the given group.
    pub async fn get_pending_proposals_count(
        &self,
        group_name: String,
    ) -> Result<usize, UserError> {
        let group = self
            .groups
            .get(&group_name)
            .ok_or(UserError::GroupNotFoundError(group_name))?;
        Ok(group.get_pending_proposals_count().await)
    }
}
