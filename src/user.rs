use alloy::{network::EthereumWallet, signers::local::PrivateKeySigner};
use kameo::Actor;
use log::info;
use openmls::{
    group::MlsGroupJoinConfig,
    key_packages::KeyPackage,
    prelude::{DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, StagedWelcome, Welcome},
};
use prost::Message;
use std::{
    collections::HashMap,
    str::{from_utf8, FromStr},
};
use waku_bindings::WakuMessage;

use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::{
    identity::Identity,
    openmls_provider::{MlsProvider, CIPHERSUITE},
};

use crate::{
    group::{Group, GroupAction},
    message::{
        wrap_conversation_message_into_application_msg, wrap_user_kp_into_welcome_msg,
        wrap_vote_start_message_into_application_msg,
    },
    protos::messages::v1::{welcome_message, AppMessage, WelcomeMessage},
};
use crate::{steward::GroupUpdateRequest, UserError};

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
    eth_signer: PrivateKeySigner,
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
            eth_signer: signer,
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

                        let (wmts, _) = group
                            .create_proposal_to_add_member(
                                &self.provider,
                                self.identity.signer(),
                                key_package,
                            )
                            .await?;
                        Ok(UserAction::SendToWaku(wmts))
                    } else {
                        Ok(UserAction::DoNothing)
                    }
                }
                welcome_message::Payload::InvitationToJoin(invitation_to_join) => {
                    if group.is_steward() {
                        Ok(UserAction::DoNothing)
                    } else {
                        println!(
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
        println!(
            "[process_app_subtopic]: User {:?} received app message for group {:?}",
            self.identity.identity_string(),
            group_name
        );

        println!("[process_app_subtopic] message: {:?}", msg.payload());
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(msg.payload())
            .map_err(|e| UserError::MlsMessageInDeserializeError(e.to_string()))?;
        let mls_message = mls_message_in
            .try_into_protocol_message()
            .map_err(|e| UserError::TryIntoProtocolMessageError(e.to_string()))?;

        let group_name = from_utf8(mls_message.group_id().as_slice())?.to_string();
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        if !group.is_mls_group_initialized() {
            return Ok(UserAction::DoNothing);
        }
        println!("[process_app_subtopic] mls_message: {:?}", mls_message);
        let res = group
            .process_protocol_msg(mls_message, &self.provider, self.identity.signature_key())
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
        info!("Processing waku message: {:?}", msg);
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
        info!("Processing waku message: {:?}", ct_name);
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
        println!("Building group message: {:?}", msg);
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
        Ok(msg_to_send)
    }

    pub async fn remove_group_users(
        &mut self,
        users: Vec<String>,
        group_name: String,
    ) -> Result<WakuMessageToSend, UserError> {
        if !self.if_group_exists(group_name.clone()) {
            return Err(UserError::GroupNotFoundError(group_name));
        }
        let group = self.groups.get_mut(&group_name).unwrap();
        let msg = group
            .remove_members(users, &self.provider, self.identity.signer())
            .await?;

        Ok(msg)
    }

    pub fn wallet(&self) -> EthereumWallet {
        EthereumWallet::from(self.eth_signer.clone())
    }

    pub async fn create_invite_proposal(
        &mut self,
        users_kp: KeyPackage,
        group_name: String,
    ) -> Result<(), UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        group.store_invite_proposal(users_kp)?;
        Ok(())
    }

    pub async fn group_drain_pending_proposals(
        &mut self,
        group_name: String,
    ) -> Result<Vec<GroupUpdateRequest>, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        Ok(group.drain_proposals()?)
    }

    pub async fn process_proposals(
        &mut self,
        group_name: String,
        proposals: Vec<GroupUpdateRequest>,
    ) -> Result<WakuMessageToSend, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        let format_msg = format!(
            "Vote start for group {} with {} proposals",
            group_name,
            proposals.len()
        );
        let app_msg = wrap_vote_start_message_into_application_msg(format_msg.clone());
        let msg = WakuMessageToSend::new(
            app_msg.encode_to_vec(),
            APP_MSG_SUBTOPIC,
            group_name.clone(),
            group.app_id().clone(),
        );

        Ok(msg)
    }

    pub async fn apply_proposals(
        &mut self,
        group_name: String,
    ) -> Result<Vec<WakuMessageToSend>, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let msg = group
            .apply_proposals(&self.provider, &self.identity.signer())
            .await?;
        Ok(msg)
    }
}
