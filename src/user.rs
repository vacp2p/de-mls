use alloy::{
    hex::{self},
    network::EthereumWallet,
    signers::{local::PrivateKeySigner, SignerSync},
};
use chrono::Utc;
use ecies::{decrypt, encrypt};
use openmls::{group::*, prelude::*};
use rand::thread_rng;
use secp256k1::{
    ecdsa::Signature as secpSignature,
    hashes::{sha256, Hash},
    Message as secpMessage, PublicKey, SecretKey,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Display,
    str::{from_utf8, FromStr},
    sync::{
        mpsc::{channel, Receiver},
        Arc,
    },
};
use tokio::sync::Mutex;
use waku_bindings::{Running, WakuMessage, WakuNodeHandle};

use ds::ds_waku::{
    DeMlsMessage, WakuGroupClient, APP_MSG_SUBTOPIC, COMMIT_MSG_SUBTOPIC, WELCOME_SUBTOPIC,
};
use mls_crypto::openmls_provider::*;

use crate::{contact::ContactsList, conversation::*};
use crate::{identity::Identity, UserError};

pub struct Group {
    group_name: String,
    conversation: Conversation,
    mls_group: Option<RefCell<MlsGroup>>,
    waku_client: WakuGroupClient,
    pub admin: Option<Admin>,
    is_kp_shared: bool,
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

    async fn push_message(&self, message: Vec<u8>);
}

impl AdminTrait for Admin {
    fn new() -> Self {
        let secp = secp256k1::Secp256k1::new();
        let (secret_key, public_key) = secp.generate_keypair(&mut thread_rng());
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
        let secp = secp256k1::Secp256k1::new();
        let (secret_key, public_key) = secp.generate_keypair(&mut thread_rng());
        self.current_key_pair = public_key;
        self.current_key_pair_private = secret_key;
        self.key_pair_timestamp = Utc::now().timestamp() as u64;
        Ok(())
    }

    fn generate_admin_message(&self) -> GroupAnnouncement {
        let digest = sha256::Hash::hash(&self.current_key_pair.serialize());
        let msg = secpMessage::from_digest(digest.to_byte_array());
        let sig = self.current_key_pair_private.sign_ecdsa(msg);
        GroupAnnouncement::new(
            self.current_key_pair.serialize().to_vec(),
            sig.serialize_compact().to_vec(),
        )
    }

    async fn push_message(&self, message: Vec<u8>) {
        self.message_queue.as_ref().lock().await.push(message);
    }

    // async fn process_messages(&self) -> Result<(), UserError> {
    //     if self.message_queue.as_ref().lock().await.is_empty() {
    //         println!("No messages to process");
    //         return Ok(());
    //     }
    //     let messages = self.message_queue.as_ref().lock().await.clone();
    //     for message in messages {
    //         let admin_msg: GroupAnnouncement = serde_json::from_slice(&message).unwrap();

    //         let digest = sha256::Hash::hash(&admin_msg.pub_key);
    //         let original_msg = secpMessage::from_digest(digest.to_byte_array());

    //         let sig = secpSignature::from_compact(admin_msg.signature.as_slice()).unwrap();

    //         let pub_key = PublicKey::from_slice(&admin_msg.pub_key).unwrap();
    //         let verified = sig.verify(&original_msg, &pub_key);
    //         match verified {
    //             Ok(_) => println!("Message verified"),
    //             Err(e) => return Err(UserError::MessageVerificationFailed(e)),
    //         }
    //     }
    //     Ok(())
    // }

    fn decrypt_msg(&mut self, message: Vec<u8>) -> Result<KeyPackage, UserError> {
        let msg: Vec<u8> = decrypt(&self.current_key_pair_private.secret_bytes(), &message)
            .map_err(|e| UserError::DecryptionError(e.to_string()))?;
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

pub struct User {
    pub identity: Identity,
    pub groups: HashMap<String, Group>,
    provider: MlsCryptoProvider,
    eth_signer: PrivateKeySigner,
    pub waku_node: WakuNodeHandle<Running>,
    pub contacts: ContactsList,
}

impl User {
    /// Create a new user with the given name and a fresh set of credentials.
    pub async fn new(
        user_eth_priv_key: &str,
        node: WakuNodeHandle<Running>,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(user_eth_priv_key)?;
        let user_address = signer.address();

        let crypto = MlsCryptoProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, user_address.as_slice())?;

        let user = User {
            groups: HashMap::new(),
            identity: id,
            eth_signer: signer,
            provider: crypto,
            waku_node: node,
            contacts: ContactsList::new().await?,
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
        let waku_client =
            WakuGroupClient::new(&self.waku_node, group_name.clone(), sender).unwrap();

        // If you create a group, you are an admin.
        // You don't need to share the key package with the group because you are the admin
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: Some(RefCell::new(mls_group)),
            waku_client,
            admin: Some(Admin::new()),
            is_kp_shared: true,
        };

        self.groups.insert(group_name.clone(), group);
        Ok(receiver)
    }

    /// Subscribe to a group without creating a new MLS group instance
    pub async fn subscribe_to_group(
        &mut self,
        group_name: String,
    ) -> Result<Receiver<WakuMessage>, UserError> {
        let (sender, receiver) = channel::<WakuMessage>();
        let w_c = WakuGroupClient::new(&self.waku_node, group_name.clone(), sender).unwrap();
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: None,
            waku_client: w_c,
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
    ) -> Result<Option<String>, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let welcome_msg: WelcomeMessage = serde_json::from_slice(&msg.payload().to_vec())?;
        match welcome_msg.message_type {
            WelcomeMessageType::GroupAnnouncement => {
                if group.admin.is_some() || group.is_kp_shared {
                    return Ok(None);
                } else {
                    let group_announcement =
                        verify_group_announcement(&welcome_msg.message_payload)?;

                    let key_package = self
                        .identity
                        .generate_key_package(CIPHERSUITE, &self.provider)?
                        .tls_serialize_detached()?;

                    let encrypted_key_package = encrypt(&group_announcement.pub_key, &key_package)
                        .map_err(|e| UserError::EncryptionError(e.to_string()))?;

                    let msg: Vec<u8> = serde_json::to_vec(&WelcomeMessage {
                        message_type: WelcomeMessageType::KeyPackageShare,
                        message_payload: encrypted_key_package,
                    })?;

                    group
                        .waku_client
                        .send_to_waku(&self.waku_node, msg, WELCOME_SUBTOPIC)?;
                    group.is_kp_shared = true;
                    return Ok(Some("Successfully send key package to group admin".to_string()));
                }
            }
            WelcomeMessageType::KeyPackageShare => {
                // We already shared the key package with the group admin and we don't need to do it again
                if group.admin.is_none() {
                    return Ok(None);
                } else {
                    let key_package = group
                        .admin
                        .as_mut()
                        .unwrap()
                        .decrypt_msg(welcome_msg.message_payload)?;
                    self.invite(vec![key_package], group_name).await?;
                    return Ok(Some("Successfully invite new user to group".to_string()));
                }
            }
            WelcomeMessageType::WelcomeShare => {
                if group.admin.is_some() {
                    return Ok(None);
                } else {
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
                        self.join_group(welcome_copy).await?;
                        return Ok(Some("Successfully join group".to_string()));
                    } else {
                        return Ok(None);
                    }
                }
            }
        }
    }

    pub async fn process_waku_msg(
        &mut self,
        msg: WakuMessage,
    ) -> Result<Option<String>, UserError> {
        println!("Process waku msg: {:?}", msg);
        let ct = msg.content_topic();
        let group_name = ct.application_name.to_string();
        let ct = ct.content_topic_name.to_string();
        match ct.as_str() {
            WELCOME_SUBTOPIC => {
                return self.handle_welcome_subtopic(msg, group_name).await;
            }
            COMMIT_MSG_SUBTOPIC => {
                let res = MlsMessageIn::tls_deserialize_bytes(&msg.payload().to_vec())?;
                let msg = match res.extract() {
                    MlsMessageInBody::PrivateMessage(message) => {
                        self.process_protocol_msg(message.into())?
                    }
                    MlsMessageInBody::PublicMessage(message) => {
                        self.process_protocol_msg(message.into())?
                    }
                    _ => return Err(UserError::UnsupportedMessageType),
                };
                return Ok(Some(msg.unwrap().to_string()));
            }
            APP_MSG_SUBTOPIC => {
                let buf: AppMessage = serde_json::from_slice(&msg.payload().to_vec())?;
                if buf.sender == self.identity.identity() {
                    return Ok(None);
                }
                let res = MlsMessageIn::tls_deserialize_bytes(&buf.message)?;
                let msg = match res.extract() {
                    MlsMessageInBody::PrivateMessage(message) => {
                        self.process_protocol_msg(message.into())?
                    }
                    MlsMessageInBody::PublicMessage(message) => {
                        self.process_protocol_msg(message.into())?
                    }
                    _ => return Err(UserError::UnsupportedMessageType),
                };
                return Ok(Some(msg.unwrap().to_string()));
            }
            _ => {
                return Err(UserError::UnknownMessageType(ct));
            }
        }
    }

    async fn invite(
        &mut self,
        users_kp: Vec<KeyPackage>,
        group_name: String,
    ) -> Result<(), UserError> {
        // Build a proposal with this key package and do the MLS bits.
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        let (out_messages, welcome, _group_info) =
            group.mls_group.as_ref().unwrap().borrow_mut().add_members(
                &self.provider,
                &self.identity.signer,
                &users_kp,
            )?;

        group.waku_client.send_to_waku(
            &self.waku_node,
            out_messages.tls_serialize_detached()?,
            COMMIT_MSG_SUBTOPIC,
        )?;

        group
            .mls_group
            .as_ref()
            .unwrap()
            .borrow_mut()
            .merge_pending_commit(&self.provider)?;

        let welcome_serialized = welcome.tls_serialize_detached()?;
        let welcome_msg: Vec<u8> = serde_json::to_vec(&WelcomeMessage {
            message_type: WelcomeMessageType::WelcomeShare,
            message_payload: welcome_serialized,
        })?;

        group
            .waku_client
            .send_to_waku(&self.waku_node, welcome_msg, WELCOME_SUBTOPIC)?;

        Ok(())
    }

    async fn join_group(&mut self, welcome: Welcome) -> Result<(), UserError> {
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

        group.mls_group = Some(RefCell::new(mls_group));
        Ok(())
    }

    pub fn process_protocol_msg(
        &mut self,
        message: ProtocolMessage,
    ) -> Result<Option<ConversationMessage>, UserError> {
        let group_name = from_utf8(message.group_id().as_slice())?.to_string();
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let mut mls_group = group.mls_group.as_ref().unwrap().borrow_mut();

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

    pub async fn send_admin_msg(
        &mut self,
        msg: GroupAnnouncement,
        group_name: String,
    ) -> Result<String, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let wm = WelcomeMessage {
            message_type: WelcomeMessageType::GroupAnnouncement,
            message_payload: serde_json::to_vec(&msg)?,
        };
        let res = group.waku_client.send_to_waku(
            &self.waku_node,
            serde_json::to_vec(&wm)?,
            WELCOME_SUBTOPIC,
        )?;
        Ok(res)
    }

    pub async fn send_msg(&mut self, msg: &str, group_name: String) -> Result<(), UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        if group.mls_group.is_none() {
            let app_msg = serde_json::to_vec(&AppMessage {
                sender: self.identity.identity().to_vec(),
                message: msg.as_bytes().to_vec(),
            })?;
            group
                .waku_client
                .send_to_waku(&self.waku_node, app_msg, APP_MSG_SUBTOPIC)?;
        } else {
            let message_out = group
                .mls_group
                .as_ref()
                .unwrap()
                .borrow_mut()
                .create_message(&self.provider, &self.identity.signer, msg.as_bytes())?
                .tls_serialize_detached()?;
            let app_msg = serde_json::to_vec(&AppMessage {
                sender: self.identity.identity().to_vec(),
                message: message_out,
            })?;
            group
                .waku_client
                .send_to_waku(&self.waku_node, app_msg, APP_MSG_SUBTOPIC)?;
        };
        Ok(())
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

    fn sign(&self, msg: String) -> Result<String, UserError> {
        let signature = self.eth_signer.sign_message_sync(msg.as_bytes())?;
        let res = serde_json::to_string(&signature)?;
        Ok(res)
    }
}

fn verify_group_announcement(msg: &Vec<u8>) -> Result<GroupAnnouncement, UserError> {
    let admin_msg: GroupAnnouncement = serde_json::from_slice(&msg)?;
    let digest = sha256::Hash::hash(&admin_msg.pub_key);
    let original_msg = secpMessage::from_digest(digest.to_byte_array());

    let sig = secpSignature::from_compact(admin_msg.signature.as_slice()).unwrap();

    let pub_key = PublicKey::from_slice(&admin_msg.pub_key).unwrap();
    let verified = sig.verify(&original_msg, &pub_key);
    match verified {
        Ok(_) => return Ok(admin_msg),
        Err(e) => return Err(UserError::MessageVerificationFailed(e)),
    }
}
