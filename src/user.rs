use alloy::{network::EthereumWallet, signers::local::PrivateKeySigner};
use kameo::{
    message::{Context, Message},
    Actor,
};
use log::info;
use openmls::{group::*, prelude::*};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    str::{from_utf8, FromStr},
};
use waku_bindings::WakuMessage;

use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::{identity::Identity, openmls_provider::*};

use crate::UserError;
use crate::{
    group::{Group, GroupAction},
    GroupAnnouncement, MessageToPrint, WelcomeMessage, WelcomeMessageType,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserAction {
    SendToWaku(WakuMessageToSend),
    SendToGroup(MessageToPrint),
    RemoveGroup(String),
    DoNothing,
}

#[derive(Actor)]
pub struct User {
    identity: Identity,
    groups: HashMap<String, Group>,
    provider: MlsCryptoProvider,
    eth_signer: PrivateKeySigner,
}

impl Message<WakuMessage> for User {
    type Reply = Result<UserAction, UserError>;

    async fn handle(
        &mut self,
        msg: WakuMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_waku_message(msg).await
    }
}

pub struct CreateGroupRequest {
    pub group_name: String,
    pub is_creation: bool,
}

impl Message<CreateGroupRequest> for User {
    type Reply = Result<(), UserError>;

    async fn handle(
        &mut self,
        msg: CreateGroupRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.create_group(msg.group_name.clone(), msg.is_creation)
            .await?;
        Ok(())
    }
}

pub struct AdminMessageRequest {
    pub group_name: String,
}

impl Message<AdminMessageRequest> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: AdminMessageRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.prepare_admin_msg(msg.group_name.clone()).await
    }
}

pub struct LeaveGroupRequest {
    pub group_name: String,
}

impl Message<LeaveGroupRequest> for User {
    type Reply = Result<(), UserError>;

    async fn handle(
        &mut self,
        msg: LeaveGroupRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.leave_group(msg.group_name.clone()).await?;
        Ok(())
    }
}

pub struct RemoveUserRequest {
    pub user_to_ban: String,
    pub group_name: String,
}

impl Message<RemoveUserRequest> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: RemoveUserRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.remove_group_users(vec![msg.user_to_ban], msg.group_name.clone())
            .await
    }
}

pub struct GetIncomeKeyPackagesRequest {
    pub group_name: String,
}

impl Message<GetIncomeKeyPackagesRequest> for User {
    type Reply = Result<Vec<KeyPackage>, UserError>;

    async fn handle(
        &mut self,
        msg: GetIncomeKeyPackagesRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.get_processed_income_key_packages(msg.group_name.clone())
            .await
    }
}

pub struct InviteUsersRequest {
    pub group_name: String,
    pub users: Vec<KeyPackage>,
}

impl Message<InviteUsersRequest> for User {
    type Reply = Result<Vec<WakuMessageToSend>, UserError>;

    async fn handle(
        &mut self,
        msg: InviteUsersRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.invite_users(msg.users.clone(), msg.group_name.clone())
            .await
    }
}

pub struct SendGroupMessage {
    pub msg: String,
    pub group_name: String,
}

impl Message<SendGroupMessage> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: SendGroupMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.build_group_message(&msg.msg, msg.group_name.clone())
            .await
    }
}

impl User {
    pub fn new(user_eth_priv_key: &str) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(user_eth_priv_key)?;
        let user_address = signer.address();

        let crypto = MlsCryptoProvider::default();
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
                Some(&self.identity.signer()),
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

    pub async fn handle_welcome_subtopic(
        &mut self,
        msg: WakuMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let welcome_msg: WelcomeMessage = serde_json::from_slice(msg.payload())?;
        match welcome_msg.message_type {
            WelcomeMessageType::GroupAnnouncement => {
                let app_id = group.app_id();
                if group.is_admin() || group.is_kp_shared() {
                    info!("Its admin or key package already shared");
                    Ok(UserAction::DoNothing)
                } else {
                    info!(
                        "User {:?} received group announcement message for group {:?}",
                        self.identity.identity_string(),
                        group_name
                    );
                    let group_announcement: GroupAnnouncement =
                        serde_json::from_slice(&welcome_msg.message_payload)?;
                    if !group_announcement.verify()? {
                        return Err(UserError::MessageVerificationFailed);
                    }

                    let key_package = serde_json::to_vec(
                        &self
                            .identity
                            .generate_key_package(CIPHERSUITE, &self.provider)?,
                    )?;

                    let encrypted_key_package = group_announcement.encrypt(key_package)?;
                    let msg: Vec<u8> = serde_json::to_vec(&WelcomeMessage {
                        message_type: WelcomeMessageType::UserKeyPackage,
                        message_payload: encrypted_key_package,
                    })?;

                    group.set_kp_shared(true);

                    Ok(UserAction::SendToWaku(WakuMessageToSend::new(
                        msg,
                        WELCOME_SUBTOPIC.to_string(),
                        group_name.clone(),
                        app_id.clone(),
                    )))
                }
            }
            WelcomeMessageType::UserKeyPackage => {
                // We already shared the key package with the group admin and we don't need to do it again
                if group.is_admin() {
                    info!(
                        "Admin {:?} received key package for the group {:?}",
                        self.identity.identity_string(),
                        group_name
                    );
                    let key_package = group.decrypt_admin_msg(welcome_msg.message_payload)?;
                    group.push_income_key_package(key_package.clone())?;
                }
                Ok(UserAction::DoNothing)
            }
            WelcomeMessageType::InvitationToJoin => {
                if group.is_admin() {
                    Ok(UserAction::DoNothing)
                } else {
                    info!(
                        "User {:?} received invitation to join group {:?}",
                        self.identity.identity_string(),
                        group_name
                    );
                    let welc = MlsMessageIn::tls_deserialize_bytes(welcome_msg.message_payload)?;
                    let welcome = match welc.into_welcome() {
                        Some(w) => w,
                        None => return Err(UserError::EmptyWelcomeMessageError),
                    };
                    // find the key package in the welcome message
                    if welcome.secrets().iter().any(|egs| {
                        let hash_ref = egs.new_member().as_slice().to_vec();
                        self.provider
                            .key_store()
                            .read(&hash_ref)
                            .map(|kp: KeyPackage| (kp, hash_ref))
                            .is_some()
                    }) {
                        self.join_group(welcome)?;
                        let msg = self
                            .build_group_message("User joined to the group", group_name)
                            .await?;
                        Ok(UserAction::SendToWaku(msg))
                    } else {
                        Ok(UserAction::DoNothing)
                    }
                }
            }
        }
    }

    pub async fn handle_app_msg_subtopic(
        &mut self,
        msg: WakuMessage,
        group_name: String,
    ) -> Result<UserAction, UserError> {
        info!(
            "User {:?} received app message for group {:?}",
            self.identity.identity_string(),
            group_name
        );
        let res = MlsMessageIn::tls_deserialize_bytes(msg.payload())?;
        let action = match res.extract() {
            MlsMessageInBody::PrivateMessage(message) => {
                self.process_protocol_msg(message.into()).await?
            }
            MlsMessageInBody::PublicMessage(message) => {
                self.process_protocol_msg(message.into()).await?
            }
            _ => return Err(UserError::UnsupportedMessageType),
        };
        Ok(action)
    }

    pub async fn get_processed_income_key_packages(
        &mut self,
        group_name: String,
    ) -> Result<Vec<KeyPackage>, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        Ok(group.processed_key_packages()?)
    }

    pub async fn add_income_key_package(
        &mut self,
        kp: KeyPackage,
        group_name: String,
    ) -> Result<(), UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        Ok(group.push_income_key_package(kp)?)
    }

    pub async fn handle_waku_message(&mut self, msg: WakuMessage) -> Result<UserAction, UserError> {
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
        match ct_name.as_str() {
            WELCOME_SUBTOPIC => self.handle_welcome_subtopic(msg, group_name).await,
            APP_MSG_SUBTOPIC => self.handle_app_msg_subtopic(msg, group_name).await,
            _ => Err(UserError::UnknownContentTopicType(ct_name)),
        }
    }

    pub async fn invite_users(
        &mut self,
        users_kp: Vec<KeyPackage>,
        group_name: String,
    ) -> Result<Vec<WakuMessageToSend>, UserError> {
        // Build a proposal with this key package and do the MLS bits.
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let out_messages = group
            .add_members(users_kp, &self.provider, &self.identity.signer())
            .await?;
        info!(
            "User {:?} invited users to group {:?}",
            self.identity.identity_string(),
            group_name
        );
        Ok(out_messages)
    }

    fn join_group(&mut self, welcome: Welcome) -> Result<(), UserError> {
        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group = MlsGroup::new_from_welcome(&self.provider, &group_config, welcome, None)?;

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

    pub async fn process_protocol_msg(
        &mut self,
        message: ProtocolMessage,
    ) -> Result<UserAction, UserError> {
        let group_name = from_utf8(message.group_id().as_slice())?.to_string();
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        if !group.is_mls_group_initialized() {
            return Ok(UserAction::DoNothing);
        }
        let res = group
            .process_protocol_msg(message, &self.provider, self.identity.signature_key())
            .await?;

        match res {
            GroupAction::MessageToPrint(msg) => Ok(UserAction::SendToGroup(msg)),
            GroupAction::RemoveGroup => Ok(UserAction::RemoveGroup(group_name)),
            GroupAction::DoNothing => Ok(UserAction::DoNothing),
        }
    }

    pub async fn prepare_admin_msg(
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
            .generate_admin_message()?;
        Ok(msg_to_send)
    }

    pub async fn build_group_message(
        &mut self,
        msg: &str,
        group_name: String,
    ) -> Result<WakuMessageToSend, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        if !group.is_mls_group_initialized() {
            return Err(UserError::GroupNotFoundError(group_name));
        }
        let msg_to_send = group
            .build_message(&self.provider, &self.identity.signer(), msg)
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
            .remove_members(users, &self.provider, &self.identity.signer())
            .await?;

        Ok(msg)
    }

    pub async fn leave_group(&mut self, group_name: String) -> Result<(), UserError> {
        if !self.if_group_exists(group_name.clone()) {
            return Err(UserError::GroupNotFoundError(group_name));
        }
        self.groups.remove(&group_name);
        Ok(())
    }

    pub fn wallet(&self) -> EthereumWallet {
        EthereumWallet::from(self.eth_signer.clone())
    }
}
