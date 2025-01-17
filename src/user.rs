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

use ds::{
    ds_waku::{APP_MSG_SUBTOPIC, COMMIT_MSG_SUBTOPIC, WELCOME_SUBTOPIC},
    waku_actor::ProcessMessageToSend,
};
use mls_crypto::openmls_provider::*;

use crate::{
    group_actor::{Group, GroupAction},
    AppMessage, GroupAnnouncement, MessageToPrint, WelcomeMessage, WelcomeMessageType,
};
use crate::{identity::Identity, UserError};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserAction {
    SendToWaku(ProcessMessageToSend),
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
    type Reply = Result<Vec<UserAction>, UserError>;

    async fn handle(
        &mut self,
        msg: WakuMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let actions = self.process_waku_msg(msg).await?;
        Ok(actions)
    }
}

pub struct ProcessCreateGroup {
    pub group_name: String,
    pub is_creation: bool,
}

impl Message<ProcessCreateGroup> for User {
    type Reply = Result<(), UserError>;

    async fn handle(
        &mut self,
        msg: ProcessCreateGroup,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.create_group(msg.group_name.clone(), msg.is_creation)
            .await?;
        Ok(())
    }
}

pub struct ProcessAdminMessage {
    pub group_name: String,
}

impl Message<ProcessAdminMessage> for User {
    type Reply = Result<ProcessMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: ProcessAdminMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.prepare_admin_msg(msg.group_name.clone()).await
    }
}

pub struct ProcessLeaveGroup {
    pub group_name: String,
}

impl Message<ProcessLeaveGroup> for User {
    type Reply = Result<(), UserError>;

    async fn handle(
        &mut self,
        msg: ProcessLeaveGroup,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.leave_group(msg.group_name.clone()).await?;
        Ok(())
    }
}

pub struct ProcessSendMessage {
    pub msg: String,
    pub group_name: String,
}

impl Message<ProcessSendMessage> for User {
    type Reply = Result<ProcessMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: ProcessSendMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.prepare_msg_to_send(&msg.msg, msg.group_name.clone())
            .await
    }
}

pub struct ProcessRemoveUser {
    pub user_to_ban: String,
    pub group_name: String,
}

impl Message<ProcessRemoveUser> for User {
    type Reply = Result<ProcessMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: ProcessRemoveUser,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.remove_users_from_group(vec![msg.user_to_ban], msg.group_name.clone())
            .await
    }
}

impl User {
    /// Create a new user with the given name and a fresh set of credentials.
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
                Some(&self.identity.signer),
                Some(&self.identity.credential_with_key),
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
    ) -> Result<Vec<UserAction>, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let welcome_msg: WelcomeMessage = serde_json::from_slice(msg.payload())?;
        match welcome_msg.message_type {
            WelcomeMessageType::GroupAnnouncement => {
                let app_id = group.app_id();
                if group.is_admin() || group.is_kp_shared() {
                    Ok(vec![UserAction::DoNothing])
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
                        message_type: WelcomeMessageType::KeyPackageShare,
                        message_payload: encrypted_key_package,
                    })?;

                    group.set_kp_shared(true);

                    Ok(vec![UserAction::SendToWaku(ProcessMessageToSend {
                        msg,
                        subtopic: WELCOME_SUBTOPIC.to_string(),
                        group_id: group_name.clone(),
                        app_id: app_id.clone(),
                    })])
                }
            }
            WelcomeMessageType::KeyPackageShare => {
                // We already shared the key package with the group admin and we don't need to do it again
                if !group.is_admin() {
                    Ok(vec![UserAction::DoNothing])
                } else {
                    info!(
                        "User {:?} received key package share message for group {:?}",
                        self.identity.identity_string(),
                        group_name
                    );
                    let key_package = group.decrypt_admin_msg(welcome_msg.message_payload)?;
                    let msgs = self.invite_users(vec![key_package], group_name).await?;
                    Ok(msgs
                        .iter()
                        .map(|msg| UserAction::SendToWaku(msg.clone()))
                        .collect())
                }
            }
            WelcomeMessageType::WelcomeShare => {
                if group.is_admin() {
                    Ok(vec![UserAction::DoNothing])
                } else {
                    info!(
                        "User {:?} received welcome share message for group {:?}",
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
                            .prepare_msg_to_send("User joined to the group", group_name)
                            .await?;
                        Ok(vec![UserAction::SendToWaku(msg)])
                    } else {
                        Ok(vec![UserAction::DoNothing])
                    }
                }
            }
        }
    }

    pub async fn process_waku_msg(
        &mut self,
        msg: WakuMessage,
    ) -> Result<Vec<UserAction>, UserError> {
        let ct = msg.content_topic();
        let group_name = ct.application_name.to_string();
        let group = match self.groups.get(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let app_id = group.app_id();
        if msg.meta() == app_id {
            return Ok(vec![UserAction::DoNothing]);
        }
        let ct = ct.content_topic_name.to_string();
        match ct.as_str() {
            WELCOME_SUBTOPIC => self.handle_welcome_subtopic(msg, group_name).await,
            COMMIT_MSG_SUBTOPIC => {
                if group.is_mls_group_initialized() {
                    info!(
                        "User {:?} received commit message for group {:?}",
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
                    Ok(vec![action])
                } else {
                    Ok(vec![UserAction::DoNothing])
                }
            }
            APP_MSG_SUBTOPIC => {
                info!(
                    "User {:?} received app message for group {:?}",
                    self.identity.identity_string(),
                    group_name
                );
                let buf: AppMessage = serde_json::from_slice(msg.payload())?;
                if buf.sender == self.identity.identity() {
                    return Ok(vec![UserAction::DoNothing]);
                }
                let res = MlsMessageIn::tls_deserialize_bytes(&buf.message)?;
                let action = match res.extract() {
                    MlsMessageInBody::PrivateMessage(message) => {
                        self.process_protocol_msg(message.into()).await?
                    }
                    MlsMessageInBody::PublicMessage(message) => {
                        self.process_protocol_msg(message.into()).await?
                    }
                    _ => return Err(UserError::UnsupportedMessageType),
                };
                Ok(vec![action])
            }
            _ => Err(UserError::UnknownContentTopicType(ct)),
        }
    }

    pub async fn invite_users(
        &mut self,
        users_kp: Vec<KeyPackage>,
        group_name: String,
    ) -> Result<Vec<ProcessMessageToSend>, UserError> {
        // Build a proposal with this key package and do the MLS bits.
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let out_messages = group
            .add_members(users_kp, &self.provider, &self.identity.signer)
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
            .process_protocol_msg(
                message,
                &self.provider,
                self.identity
                    .credential_with_key
                    .signature_key
                    .as_slice()
                    .to_vec(),
            )
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
    ) -> Result<ProcessMessageToSend, UserError> {
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

    pub async fn prepare_msg_to_send(
        &mut self,
        msg: &str,
        group_name: String,
    ) -> Result<ProcessMessageToSend, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        if !group.is_mls_group_initialized() {
            Err(UserError::GroupNotFoundError(group_name))
        } else {
            let msg_to_send = group
                .create_message(
                    &self.provider,
                    &self.identity.signer,
                    msg,
                    self.identity.identity().to_vec(),
                )
                .await?;
            Ok(msg_to_send)
        }
    }

    pub async fn remove_users_from_group(
        &mut self,
        users: Vec<String>,
        group_name: String,
    ) -> Result<ProcessMessageToSend, UserError> {
        if !self.if_group_exists(group_name.clone()) {
            return Err(UserError::GroupNotFoundError(group_name));
        }
        let group = self.groups.get_mut(&group_name).unwrap();
        let msg = group
            .remove_members(users, &self.provider, &self.identity.signer)
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
