use alloy::{
    hex::{self},
    network::EthereumWallet,
    signers::local::PrivateKeySigner,
};
use chrono::Utc;
use libsecp256k1::{PublicKey, SecretKey};
use log::info;
use openmls::{group::*, prelude::*};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt::Display,
    str::{from_utf8, FromStr},
    sync::{
        mpsc::{channel, Receiver},
        Arc,
    },
};
use tokio::sync::Mutex;
use waku_bindings::WakuMessage;

use ds::ds_waku::{
    MessageToSend, WakuGroupClient, APP_MSG_SUBTOPIC, COMMIT_MSG_SUBTOPIC, WELCOME_SUBTOPIC,
};
use mls_crypto::openmls_provider::*;

use crate::{
    conversation::*, decrypt_message, encrypt_message, generate_keypair, sign_message,
    verify_message,
};
use crate::{identity::Identity, UserError};

pub struct Group {
    group_name: String,
    conversation: Conversation,
    mls_group: Option<Arc<Mutex<MlsGroup>>>,
    // pub waku_client: WakuGroupClient,
    pub admin: Option<Admin>,
    is_kp_shared: bool,
}

impl Group {
    pub fn is_mls_group_initialized(&self) -> bool {
        self.mls_group.is_some()
    }
}

impl Display for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Group: {:#?}", self.group_name)
    }
}

pub struct Admin {
    is_admin: bool,
    message_queue: Arc<Mutex<Vec<Vec<u8>>>>,
    current_key_pair: PublicKey,
    current_key_pair_private: SecretKey,
    key_pair_timestamp: u64,
}

pub trait AdminTrait {
    fn new() -> Self;
    fn is_admin(&self) -> bool;
    fn set_admin(&mut self, is_admin: bool);
    fn generate_new_key_pair(&mut self) -> Result<(), UserError>;

    fn generate_admin_message(&self) -> GroupAnnouncement;

    fn decrypt_msg(&mut self, message: Vec<u8>) -> Result<KeyPackage, UserError>;
    // Process accumulated messages
    // async fn process_messages(&self) -> impl std::future::Future<Output = Result<(), UserError>> + Send;

    fn push_message(&self, message: Vec<u8>) -> impl std::future::Future<Output = ()> + Send;
}

impl AdminTrait for Admin {
    fn new() -> Self {
        let (public_key, secret_key) = generate_keypair();
        Admin {
            is_admin: true,
            message_queue: Arc::new(Mutex::new(Vec::new())),
            current_key_pair: public_key,
            current_key_pair_private: secret_key,
            key_pair_timestamp: Utc::now().timestamp() as u64,
        }
    }

    fn is_admin(&self) -> bool {
        self.is_admin
    }

    fn set_admin(&mut self, is_admin: bool) {
        self.is_admin = is_admin;
    }

    fn generate_new_key_pair(&mut self) -> Result<(), UserError> {
        let (public_key, secret_key) = generate_keypair();
        self.current_key_pair = public_key;
        self.current_key_pair_private = secret_key;
        self.key_pair_timestamp = Utc::now().timestamp() as u64;
        Ok(())
    }

    fn generate_admin_message(&self) -> GroupAnnouncement {
        let signature = sign_message(
            &self.current_key_pair.serialize_compressed(),
            &self.current_key_pair_private,
        );
        GroupAnnouncement::new(
            self.current_key_pair.serialize_compressed().to_vec(),
            signature,
        )
    }

    async fn push_message(&self, message: Vec<u8>) {
        self.message_queue.as_ref().lock().await.push(message);
    }

    fn decrypt_msg(&mut self, message: Vec<u8>) -> Result<KeyPackage, UserError> {
        let msg: Vec<u8> = decrypt_message(&message, self.current_key_pair_private)?;
        let key_package: KeyPackage = serde_json::from_slice(&msg)?;
        Ok(key_package)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WelcomeMessageType {
    GroupAnnouncement,
    KeyPackageShare,
    WelcomeShare,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeMessage {
    pub message_type: WelcomeMessageType,
    pub message_payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupAnnouncement {
    pub_key: Vec<u8>,
    signature: Vec<u8>,
}

impl GroupAnnouncement {
    pub fn new(pub_key: Vec<u8>, signature: Vec<u8>) -> Self {
        GroupAnnouncement { pub_key, signature }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppMessage {
    pub sender: Vec<u8>,
    pub message: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UserAction {
    SendToWaku(MessageToSend),
    SendToGroup(String),
    DoNothing,
}

pub struct User {
    pub identity: Identity,
    pub groups: HashMap<String, Group>,
    provider: MlsCryptoProvider,
    eth_signer: PrivateKeySigner,
    pub waku_clients: HashMap<String, WakuGroupClient>,
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
            waku_clients: HashMap::new(),
        };
        Ok(user)
    }

    pub fn create_group(&mut self, group_name: String) -> Result<Receiver<WakuMessage>, UserError> {
        let group_id = group_name.as_bytes();
        if self.groups.contains_key(&group_name) {
            return Err(UserError::GroupAlreadyExistsError(group_name));
        }

        // Create a new MLS group instance
        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();
        let mls_group = MlsGroup::new_with_group_id(
            &self.provider,
            &self.identity.signer,
            &group_config,
            GroupId::from_slice(group_id),
            self.identity.credential_with_key.clone(),
        )?;

        // Create a channel to send and receive messages to the group
        let (sender, receiver) = channel::<WakuMessage>();
        let waku_client = WakuGroupClient::new(group_name.clone(), sender)?;
        self.waku_clients.insert(group_name.clone(), waku_client);

        // If you create a group, you are an admin.
        // You don't need to share the key package with the group because you are the admin
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: Some(Arc::new(Mutex::new(mls_group))),
            admin: Some(Admin::new()),
            is_kp_shared: true,
        };

        self.groups.insert(group_name.clone(), group);
        Ok(receiver)
    }

    /// Subscribe to a group without creating a new MLS group instance
    pub fn subscribe_to_group(
        &mut self,
        group_name: String,
    ) -> Result<Receiver<WakuMessage>, UserError> {
        let (sender, receiver) = channel::<WakuMessage>();
        let w_c = WakuGroupClient::new(group_name.clone(), sender)?;
        self.waku_clients.insert(group_name.clone(), w_c);
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: None,
            admin: None,
            is_kp_shared: false, // you've just subscribed to the group, so you share the key package after getting the admin key
        };

        self.groups.insert(group_name.clone(), group);
        Ok(receiver)
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
                if group.admin.is_some() || group.is_kp_shared {
                    Ok(vec![UserAction::DoNothing])
                } else {
                    info!(
                        "User {:?} received group announcement message for group {:?}",
                        self.identity.identity_string(),
                        group_name
                    );
                    let group_announcement =
                        verify_group_announcement(&welcome_msg.message_payload)?;

                    let key_package = serde_json::to_vec(
                        &self
                            .identity
                            .generate_key_package(CIPHERSUITE, &self.provider)?,
                    )?;

                    let encrypted_key_package =
                        encrypt_message(&key_package, &group_announcement.pub_key)?;
                    let msg: Vec<u8> = serde_json::to_vec(&WelcomeMessage {
                        message_type: WelcomeMessageType::KeyPackageShare,
                        message_payload: encrypted_key_package,
                    })?;

                    group.is_kp_shared = true;

                    Ok(vec![UserAction::SendToWaku(MessageToSend {
                        msg,
                        subtopic: WELCOME_SUBTOPIC.to_string(),
                    })])
                }
            }
            WelcomeMessageType::KeyPackageShare => {
                // We already shared the key package with the group admin and we don't need to do it again
                if group.admin.is_none() {
                    Ok(vec![UserAction::DoNothing])
                } else {
                    info!(
                        "User {:?} received key package share message for group {:?}",
                        self.identity.identity_string(),
                        group_name
                    );
                    let key_package = group
                        .admin
                        .as_mut()
                        .unwrap()
                        .decrypt_msg(welcome_msg.message_payload)?;
                    let msgs = self.invite(vec![key_package], group_name).await?;
                    Ok(msgs
                        .iter()
                        .map(|msg| UserAction::SendToWaku(msg.clone()))
                        .collect())
                }
            }
            WelcomeMessageType::WelcomeShare => {
                if group.admin.is_some() {
                    Ok(vec![UserAction::DoNothing])
                } else {
                    info!(
                        "User {:?} received welcome share message for group {:?}",
                        self.identity.identity_string(),
                        group_name
                    );
                    let welc =
                        MlsMessageIn::tls_deserialize_bytes(welcome_msg.message_payload).unwrap();
                    let welcome = welc.into_welcome();
                    if welcome.is_none() {
                        return Err(UserError::EmptyWelcomeMessageError);
                    }
                    let welcome_copy = welcome.unwrap();
                    // find the key package in the welcome message
                    if welcome_copy.secrets().iter().any(|egs| {
                        let hash_ref = egs.new_member().as_slice().to_vec();
                        self.provider
                            .key_store()
                            .read(&hash_ref)
                            .map(|kp: KeyPackage| (kp, hash_ref))
                            .is_some()
                    }) {
                        self.join_group(welcome_copy)?;
                        let msg = self.send_msg("User joined the group", group_name).await?;
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
                    let msg = match res.extract() {
                        MlsMessageInBody::PrivateMessage(message) => {
                            self.process_protocol_msg(message.into()).await?
                        }
                        MlsMessageInBody::PublicMessage(message) => {
                            self.process_protocol_msg(message.into()).await?
                        }
                        _ => return Err(UserError::UnsupportedMessageType),
                    };
                    if msg.is_some() {
                        Ok(vec![UserAction::SendToGroup(msg.unwrap().to_string())])
                    } else {
                        Ok(vec![UserAction::DoNothing])
                    }
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
                let msg = match res.extract() {
                    MlsMessageInBody::PrivateMessage(message) => {
                        self.process_protocol_msg(message.into()).await?
                    }
                    MlsMessageInBody::PublicMessage(message) => {
                        self.process_protocol_msg(message.into()).await?
                    }
                    _ => return Err(UserError::UnsupportedMessageType),
                };
                Ok(vec![UserAction::SendToGroup(msg.unwrap().to_string())])
            }
            _ => Err(UserError::UnknownContentTopicType(ct)),
        }
    }

    async fn invite(
        &mut self,
        users_kp: Vec<KeyPackage>,
        group_name: String,
    ) -> Result<Vec<MessageToSend>, UserError> {
        // Build a proposal with this key package and do the MLS bits.
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let mut mls_group = group.mls_group.as_mut().unwrap().lock().await;
        let (out_messages, welcome, _group_info) =
            mls_group.add_members(&self.provider, &self.identity.signer, &users_kp)?;

        mls_group.merge_pending_commit(&self.provider)?;

        let msg_to_send_commit = MessageToSend {
            msg: out_messages.tls_serialize_detached()?,
            subtopic: COMMIT_MSG_SUBTOPIC.to_string(),
        };

        let welcome_serialized = welcome.tls_serialize_detached()?;
        let welcome_msg: Vec<u8> = serde_json::to_vec(&WelcomeMessage {
            message_type: WelcomeMessageType::WelcomeShare,
            message_payload: welcome_serialized,
        })?;

        let msg_to_send_welcome = MessageToSend {
            msg: welcome_msg,
            subtopic: WELCOME_SUBTOPIC.to_string(),
        };
        info!(
            "User {:?} invited users to group {:?}",
            self.identity.identity(),
            group_name
        );
        Ok(vec![msg_to_send_commit, msg_to_send_welcome])
    }

    fn join_group(&mut self, welcome: Welcome) -> Result<(), UserError> {
        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group = MlsGroup::new_from_welcome(&self.provider, &group_config, welcome, None)?;

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id)?;

        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        group.is_kp_shared = true;

        group.mls_group = Some(Arc::new(Mutex::new(mls_group)));
        info!(
            "User {:?} joined group {:?}",
            self.identity.identity(),
            group_name
        );
        Ok(())
    }

    pub async fn process_protocol_msg(
        &mut self,
        message: ProtocolMessage,
    ) -> Result<Option<ConversationMessage>, UserError> {
        let group_name = from_utf8(message.group_id().as_slice())?.to_string();
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let mut mls_group = group.mls_group.as_mut().unwrap().lock().await;

        // If the message is from a previous epoch, we don't need to process it and it's a commit for welcome message
        if message.epoch() < mls_group.epoch() && message.epoch() == 0.into() {
            return Ok(None);
        }

        let processed_message = mls_group.process_message(&self.provider, message)?;
        let processed_message_credential: Credential = processed_message.credential().clone();

        match processed_message.into_content() {
            ProcessedMessageContent::ApplicationMessage(application_message) => {
                let sender_name = {
                    let user_id = mls_group.members().find_map(|m| {
                        if m.credential.identity() == processed_message_credential.identity()
                            && (self.identity.credential_with_key.signature_key.as_slice()
                                != m.signature_key.as_slice())
                        {
                            Some(hex::encode(m.credential.identity()))
                        } else {
                            None
                        }
                    });
                    user_id.unwrap_or("".to_owned())
                };

                let conversation_message = ConversationMessage::new(
                    group_name,
                    sender_name,
                    String::from_utf8(application_message.into_bytes())?,
                );
                group.conversation.add(conversation_message.clone());
                return Ok(Some(conversation_message));
            }
            ProcessedMessageContent::ProposalMessage(_proposal_ptr) => (),
            ProcessedMessageContent::ExternalJoinProposalMessage(_external_proposal_ptr) => (),
            ProcessedMessageContent::StagedCommitMessage(commit_ptr) => {
                let mut remove_proposal: bool = false;
                if commit_ptr.self_removed() {
                    remove_proposal = true;
                }
                mls_group.merge_staged_commit(&self.provider, *commit_ptr)?;
                if remove_proposal {
                    // here we need to remove group instance locally and
                    // also remove correspond key package from local storage ans sc storage
                    return Ok(None);
                }
            }
        };
        Ok(None)
    }

    pub async fn prepare_admin_msg(
        &mut self,
        group_name: String,
    ) -> Result<MessageToSend, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        group.admin.as_mut().unwrap().generate_new_key_pair()?;
        let admin_msg = group.admin.as_mut().unwrap().generate_admin_message();

        let wm = WelcomeMessage {
            message_type: WelcomeMessageType::GroupAnnouncement,
            message_payload: serde_json::to_vec(&admin_msg)?,
        };
        let msg_to_send = MessageToSend {
            msg: serde_json::to_vec(&wm)?,
            subtopic: WELCOME_SUBTOPIC.to_string(),
        };
        Ok(msg_to_send)
    }

    pub async fn send_msg(
        &mut self,
        msg: &str,
        group_name: String,
    ) -> Result<MessageToSend, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        if group.mls_group.is_none() {
            Err(UserError::GroupNotFoundError(group_name))
        } else {
            let message_out = group
                .mls_group
                .as_mut()
                .unwrap()
                .lock()
                .await
                .create_message(&self.provider, &self.identity.signer, msg.as_bytes())?
                .tls_serialize_detached()?;
            let app_msg = serde_json::to_vec(&AppMessage {
                sender: self.identity.identity().to_vec(),
                message: message_out,
            })?;
            Ok(MessageToSend {
                msg: app_msg,
                subtopic: APP_MSG_SUBTOPIC.to_string(),
            })
            // group
            //     .waku_client
            //     .send_to_waku(&self.waku_node, app_msg, APP_MSG_SUBTOPIC)?;
        }
    }

    // pub async fn remove(&mut self, name: String, group_name: String) -> Result<(), UserError> {
    //     // Get the group ID
    //     let group = match self.groups.get_mut(&group_name) {
    //         Some(g) => g,
    //         None => return Err(UserError::UnknownGroupError(group_name)),
    //     };

    //     // Get the user leaf index
    //     let leaf_index = group.find_member_index(name)?;

    //     // Remove operation on the mls group
    //     let (remove_message, _welcome, _group_info) = group.mls_group.borrow_mut().remove_members(
    //         &self.provider,
    //         &self.identity.signer,
    //         &[leaf_index],
    //     )?;

    //     group.rc_client.msg_send(remove_message).await?;

    //     // Second, process the removal on our end.
    //     group
    //         .mls_group
    //         .borrow_mut()
    //         .merge_pending_commit(&self.provider)?;

    //     Ok(())
    // }

    pub fn wallet(&self) -> EthereumWallet {
        EthereumWallet::from(self.eth_signer.clone())
    }
}

fn verify_group_announcement(msg: &[u8]) -> Result<GroupAnnouncement, UserError> {
    let admin_msg: GroupAnnouncement = serde_json::from_slice(msg)?;
    let verified = verify_message(&admin_msg.pub_key, &admin_msg.signature, &admin_msg.pub_key)?;
    if verified {
        Ok(admin_msg)
    } else {
        Err(UserError::MessageVerificationFailed)
    }
}
