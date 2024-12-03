use alloy::{
    hex::{self},
    network::{EthereumWallet, Network},
    primitives::Address,
    providers::Provider,
    signers::{local::PrivateKeySigner, SignerSync},
    transports::Transport,
};
use chrono::Utc;
use clap::builder::Str;
use fred::types::Message;
use openmls::{group::*, prelude::*};
use rand::{random, thread_rng};
use secp256k1::ecdsa::Signature as secpSignature;
use secp256k1::hashes::{sha256, Hash};
use secp256k1::{Message as secpMessage, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use serde_json::ser;
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
use tokio_util::sync::CancellationToken;
use waku_bindings::{Running, WakuContentTopic, WakuMessage, WakuNodeHandle};

use ds::ds_waku::{
    DeMlsMessage, WakuGroupClient, APP_MSG_SUBTOPIC, COMMIT_MSG_SUBTOPIC, WELCOME_SUBTOPIC,
};
use mls_crypto::openmls_provider::*;
use sc_key_store::{sc_ks::ScKeyStorage, *};

use crate::{contact::ContactsList, conversation::*};
use crate::{identity::Identity, UserError};

pub struct Group {
    group_name: String,
    conversation: Conversation,
    mls_group: Option<RefCell<MlsGroup>>,
    // rc_client: RClient,
    waku_client: WakuGroupClient,
    pub admin: Option<Admin>,
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

    fn generate_admin_message(&self) -> AdminMessage;

    // Process accumulated messages
    async fn process_messages(&self) -> Result<(), UserError>;

    // Subscribe to a Waku topic and push messages to the queue
    async fn listen_to_messages(&self, topic: &str);

    async fn push_message(&self, message: Vec<u8>);
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminMessage {
    pub_key: Vec<u8>,
    signature: Vec<u8>,
}

impl AdminMessage {
    pub fn new(pub_key: Vec<u8>, signature: Vec<u8>) -> Self {
        AdminMessage { pub_key, signature }
    }
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

    fn generate_admin_message(&self) -> AdminMessage {
        let digest = sha256::Hash::hash(&self.current_key_pair.serialize());
        let msg = secpMessage::from_digest(digest.to_byte_array());
        let sig = self.current_key_pair_private.sign_ecdsa(msg);
        AdminMessage::new(
            self.current_key_pair.serialize().to_vec(),
            sig.serialize_compact().to_vec(),
        )
    }

    async fn push_message(&self, message: Vec<u8>) {
        self.message_queue.as_ref().lock().await.push(message);
    }

    async fn process_messages(&self) -> Result<(), UserError> {
        if self.message_queue.as_ref().lock().await.is_empty() {
            println!("No messages to process");
            return Ok(());
        }
        let messages = self.message_queue.as_ref().lock().await.clone();
        for message in messages {
            let admin_msg: AdminMessage = serde_json::from_slice(&message).unwrap();

            let digest = sha256::Hash::hash(&admin_msg.pub_key);
            let original_msg = secpMessage::from_digest(digest.to_byte_array());

            let sig = secpSignature::from_compact(admin_msg.signature.as_slice()).unwrap();

            let pub_key = PublicKey::from_slice(&admin_msg.pub_key).unwrap();
            let verified = sig.verify(&original_msg, &pub_key);
            match verified {
                Ok(_) => println!("Message verified"),
                Err(e) => return Err(UserError::MessageVerificationFailed(e)),
            }
        }
        Ok(())
    }

    async fn listen_to_messages(&self, topic: &str) {
        todo!()
    }
}

pub struct User {
    pub identity: Identity,
    pub groups: HashMap<String, Group>,
    provider: MlsCryptoProvider,
    eth_signer: PrivateKeySigner,
    // we don't need on-chain connection if we don't create a group
    // sc_ks: Option<ScKeyStorage<T, P, N>>,
    pub waku_node: WakuNodeHandle<Running>,
    pub contacts: ContactsList,
}

impl User
// where
//     T: Transport + Clone,
//     P: Provider<T, N>,
//     N: Network,
{
    /// Create a new user with the given name and a fresh set of credentials.
    pub async fn new(
        user_eth_priv_key: &str,
        node: WakuNodeHandle<Running>,
        // nodes: Vec<String>,
        // chat_client: ChatClient,
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
            // sc_ks: None,
            waku_node: node,
            contacts: ContactsList::new().await?,
        };
        Ok(user)
    }

    // pub async fn connect_to_smart_contract(
    //     &mut self,
    //     sc_storage_address: &str,
    //     provider: P,
    // ) -> Result<(), UserError> {
    //     let storage_address = Address::from_str(sc_storage_address)?;
    //     self.sc_ks = Some(ScKeyStorage::new(provider, storage_address));
    //     self.sc_ks
    //         .as_mut()
    //         .unwrap()
    //         .add_user(&self.identity.to_string())
    //         .await?;
    //     Ok(())
    // }

    pub async fn process_waku_msg(
        &mut self,
        group_name: String,
        msg: WakuMessage,
    ) -> Result<Option<String>, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        let ct = msg.content_topic().content_topic_name.to_string();
        match ct.as_str() {
            WELCOME_SUBTOPIC => {
                group
                    .admin
                    .as_mut()
                    .unwrap()
                    .push_message(msg.payload().to_vec())
                    .await;
                return Ok(Some(format!("WELCOME msg: {:?}", msg.payload())));
            }
            COMMIT_MSG_SUBTOPIC => {
                return Ok(Some(format!("COMMIT_MSG msg: {:?}", msg)));
            }
            APP_MSG_SUBTOPIC => {
                let buf: DeMlsMessage = serde_json::from_slice(&msg.payload().to_vec())?;
                if buf.sender == self.identity.to_string() {
                    return Ok(None);
                }
                if buf.msg_type == ds::ds_waku::MsgType::MlsText {
                    let res = MlsMessageIn::tls_deserialize_bytes(&buf.msg)?;
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
                } else {
                    return Err(UserError::UnknownMessageType(buf.msg_type.to_string()));
                }
            }
            _ => {
                return Err(UserError::UnknownMessageType(ct));
            }
        }
    }

    pub fn create_group(
        &mut self,
        group_name: String,
        // sc_address: String,
    ) -> Result<Receiver<WakuMessage>, UserError> {
        let group_id = group_name.as_bytes();

        if self.groups.contains_key(&group_name) {
            return Err(UserError::GroupAlreadyExistsError(group_name));
        }

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

        let (sender, receiver) = channel::<WakuMessage>();
        let waku_client =
            WakuGroupClient::new(&self.waku_node, group_name.clone(), sender).unwrap();
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: Some(RefCell::new(mls_group)),
            waku_client,
            admin: Some(Admin::new()),
        };

        self.groups.insert(group_name.clone(), group);
        // self.contacts.insert_group2sc(group_name, sc_address)?;
        Ok(receiver)
    }

    // pub async fn add_user_to_acl(&mut self, user_address: &str) -> Result<(), UserError> {
    //     if self.sc_ks.is_none() {
    //         return Err(UserError::MissingSmartContractConnection);
    //     }
    //     self.sc_ks.as_mut().unwrap().add_user(user_address).await?;
    //     Ok(())
    // }

    pub async fn restore_key_package(
        &mut self,
        mut signed_kp: &[u8],
    ) -> Result<KeyPackage, UserError> {
        // if self.sc_ks.is_none() {
        //     return Err(UserError::MissingSmartContractConnection);
        // }

        let key_package_in = KeyPackageIn::tls_deserialize(&mut signed_kp)?;
        let key_package =
            key_package_in.validate(self.provider.crypto(), ProtocolVersion::Mls10)?;

        Ok(key_package)
    }

    pub async fn invite(
        &mut self,
        users: Vec<String>,
        group_name: String,
    ) -> Result<(), UserError> {
        // if self.sc_ks.is_none() {
        //     return Err(UserError::MissingSmartContractConnection);
        // }

        let users_for_invite = self
            .contacts
            .prepare_joiners(users.clone(), group_name.clone())
            .await?;

        let mut joiners_key_package: Vec<KeyPackage> = Vec::with_capacity(users_for_invite.len());
        let mut user_addrs = Vec::with_capacity(users_for_invite.len());
        for (user_addr, user_kp) in users_for_invite {
            joiners_key_package.push(self.restore_key_package(&user_kp).await?);
            user_addrs.push(user_addr);
        }

        // Build a proposal with this key package and do the MLS bits.
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        let (out_messages, welcome, _group_info) =
            group.mls_group.as_ref().unwrap().borrow_mut().add_members(
                &self.provider,
                &self.identity.signer,
                &joiners_key_package,
            )?;

        group.waku_client.send_to_waku(
            &self.waku_node,
            out_messages.tls_serialize_detached()?,
            ds::ds_waku::MsgType::UpdateGroup,
        )?;
        // group
        //     .rc_client
        //     .msg_send(
        //         out_messages.tls_serialize_detached()?,
        //         self.identity.to_string(),
        //         group_name,
        //     )
        //     .await?;
        // Second, process the invitation on our end.
        group
            .mls_group
            .as_ref()
            .unwrap()
            .borrow_mut()
            .merge_pending_commit(&self.provider)?;
        // Send welcome by p2p
        group.waku_client.send_to_waku(
            &self.waku_node,
            welcome.tls_serialize_detached()?,
            ds::ds_waku::MsgType::InviteToGroup,
        )?;

        // self.contacts
        //     .send_welcome_msg_to_users(self.identity.to_string(), user_addrs, welcome)?;

        Ok(())
    }

    // pub async fn receive_msg(
    //     &mut self,
    //     msg_bytes: Vec<u8>,
    // ) -> Result<Option<ConversationMessage>, UserError> {
    //     let buf: DeMlsMessage = serde_json::from_slice(&msg_bytes)?;
    //     if buf.sender == self.identity.to_string() {
    //         return Ok(None);
    //     }
    //     if buf.msg_type == ds::ds_waku::MsgType::MlsText {
    //         let res = MlsMessageIn::tls_deserialize_bytes(&buf.msg)?;
    //         let msg = match res.extract() {
    //             MlsMessageInBody::PrivateMessage(message) => {
    //                 self.process_protocol_msg(message.into())?
    //             }
    //             MlsMessageInBody::PublicMessage(message) => {
    //                 self.process_protocol_msg(message.into())?
    //             }
    //             _ => return Err(UserError::UnsupportedMessageType),
    //         };
    //         return Ok(msg);
    //     } else {
    //         return Err(UserError::UnknownMessageType(buf.msg_type.to_string()));
    //     }
    // }

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
        msg: AdminMessage,
        group_name: String,
    ) -> Result<String, UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        let res = group.waku_client.send_to_waku(
            &self.waku_node,
            serde_json::to_vec(&msg).unwrap(),
            ds::ds_waku::MsgType::InviteToGroup,
        )?;
        Ok(res)
    }

    pub async fn send_msg(
        &mut self,
        msg: &str,
        group_name: String,
        sender: String,
    ) -> Result<(), UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };

        if group.mls_group.is_some() {
            let message_out = group
                .mls_group
                .as_ref()
                .unwrap()
                .borrow_mut()
                .create_message(&self.provider, &self.identity.signer, msg.as_bytes())?
                .tls_serialize_detached()?;
            group.waku_client.send_to_waku(
                &self.waku_node,
                message_out,
                ds::ds_waku::MsgType::MlsText,
            )?;
        } else {
            group.waku_client.send_to_waku(
                &self.waku_node,
                msg.as_bytes().to_vec(),
                ds::ds_waku::MsgType::Text,
            )?;
        };

        // group
        //     .rc_client
        //     .msg_send(message_out.tls_serialize_detached()?, sender, group_name)
        //     .await?;
        Ok(())
    }

    pub async fn subscribe_to_group(
        &mut self,
        group_name: String,
    ) -> Result<(Receiver<WakuMessage>, Vec<String>), UserError> {
        let (sender, receiver) = channel::<WakuMessage>();
        let w_c = WakuGroupClient::new(&self.waku_node, group_name.clone(), sender).unwrap();
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: None,
            waku_client: w_c,
            admin: None,
        };

        self.groups.insert(group_name.clone(), group);
        // let group_after = self.groups.get_mut(&group_name).unwrap();
        // let res = group_after.waku_client.waku_relay_topics(&self.waku_node);
        // let res = self.waku_node.relay_topics();
        // if let Ok(topics) = res {
        // Ok((receiver, topics))
        // } else {
        // Err(UserError::DeliveryServiceError(res.err().unwrap()))
        // }
        Ok((receiver, Vec::new()))
    }

    pub async fn join_group(
        &mut self,
        welcome: String,
    ) -> Result<(Receiver<WakuMessage>, String), UserError> {
        let wbytes = hex::decode(welcome).unwrap();
        let welc = MlsMessageIn::tls_deserialize_bytes(wbytes).unwrap();
        let welcome = welc.into_welcome();
        if welcome.is_none() {
            return Err(UserError::EmptyWelcomeMessageError);
        }

        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        // TODO: After we move from openmls, we will have to delete the used key package here ourselves.
        let mls_group =
            MlsGroup::new_from_welcome(&self.provider, &group_config, welcome.unwrap(), None)?;

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id)?;

        // let (rc, br) = RClient::new_with_group(group_name.clone()).await?;
        let (sender, receiver) = channel::<WakuMessage>();
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: Some(RefCell::new(mls_group)),
            waku_client: WakuGroupClient::new(&self.waku_node, group_name.clone(), sender).unwrap(),
            admin: None,
        };

        match self.groups.insert(group_name.clone(), group) {
            Some(old) => Err(UserError::GroupAlreadyExistsError(old.group_name)),
            None => Ok((receiver, group_name)),
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

    /// Return the last 100 messages sent to the group.
    pub fn read_msgs(
        &self,
        group_name: String,
    ) -> Result<Option<Vec<ConversationMessage>>, UserError> {
        self.groups.get(&group_name).map_or_else(
            || Err(UserError::GroupNotFoundError(group_name)),
            |g| {
                Ok(g.conversation
                    .get(100)
                    .map(|messages: &[crate::conversation::ConversationMessage]| messages.to_vec()))
            },
        )
    }

    pub fn group_members(&self, group_name: String) -> Result<Vec<String>, UserError> {
        let group = match self.groups.get(&group_name) {
            Some(g) => g,
            None => return Err(UserError::GroupNotFoundError(group_name)),
        };
        Ok(group.group_members(self.identity.signature_pub_key().as_slice()))
    }

    pub fn user_groups(&self) -> Result<Vec<String>, UserError> {
        if self.groups.is_empty() {
            return Ok(Vec::default());
        }
        Ok(self.groups.keys().map(|k| k.to_owned()).collect())
    }

    pub fn wallet(&self) -> EthereumWallet {
        EthereumWallet::from(self.eth_signer.clone())
    }

    fn sign(&self, msg: String) -> Result<String, UserError> {
        let signature = self.eth_signer.sign_message_sync(msg.as_bytes())?;
        let res = serde_json::to_string(&signature)?;
        Ok(res)
    }

    // pub fn send_responce_on_request(
    //     &mut self,
    //     req: RequestMLSPayload,
    //     user_address: &str,
    // ) -> Result<(), UserError> {
    //     let self_address = self.identity.to_string();
    //     match req.msg_type {
    //         ReqMessageType::InviteToGroup => {
    //             let signature = self.sign(req.msg_to_sign())?;
    //             let key_package = self
    //                 .identity
    //                 .generate_key_package(CIPHERSUITE, &self.provider)?;
    //             let resp = ResponseMLSPayload::new(
    //                 signature,
    //                 self_address.clone(),
    //                 req.group_name(),
    //                 key_package.tls_serialize_detached()?,
    //             );
    //             self.contacts
    //                 .send_resp_msg_to_user(self_address, user_address, resp)?;

    //             Ok(())
    //         }
    //         ReqMessageType::RemoveFromGroup => Ok(()),
    //     }
    // }

    // pub async fn parce_responce(&mut self, resp: ResponseMLSPayload) -> Result<(), UserError> {
    //     // if self.sc_ks.is_none() {
    //     //     return Err(UserError::MissingSmartContractConnection);
    //     // }
    //     let group_name = resp.group_name.clone();
    //     let sc_address = self.contacts.group2sc(group_name.clone())?;
    //     let (user_wallet, kp) = resp.validate(sc_address, group_name.clone())?;

    //     self.contacts
    //         .add_key_package_to_contact(&user_wallet, kp, group_name.clone())
    //         .await?;

    //     self.contacts.handle_response(&user_wallet)?;
    //     Ok(())
    // }

    // pub fn sc_address(&self) -> Result<String, UserError> {
    //     if self.sc_ks.is_none() {
    //         return Err(UserError::MissingSmartContractConnection);
    //     }
    //     Ok(self.sc_ks.as_ref().unwrap().sc_adsress())
    // }

    // pub async fn handle_send_req(
    //     &mut self,
    //     user_wallet: &str,
    //     group_name: String,
    // ) -> Result<Option<CancellationToken>, UserError> {
    //     if !self.contacts.does_user_in_contacts(user_wallet).await {
    //         self.contacts.add_new_contact(user_wallet).await?;
    //     }
    //     self.contacts
    //         .send_msg_req(
    //             self.identity.to_string(),
    //             user_wallet.to_owned(),
    //             group_name,
    //             ReqMessageType::InviteToGroup,
    //         )
    //         .unwrap();

    //     Ok(self.contacts.future_req.get(user_wallet).cloned())
    // }
}

impl Group {
    /// Get a member
    fn find_member_index(&self, user_id: String) -> Result<LeafNodeIndex, GroupError> {
        let member = self
            .mls_group
            .as_ref()
            .unwrap()
            .borrow()
            .members()
            .find(|m| m.credential.identity().eq(user_id.as_bytes()));

        match member {
            Some(m) => Ok(m.index),
            None => Err(GroupError::UnknownGroupMemberError(user_id)),
        }
    }

    pub fn group_members(&self, user_signature: &[u8]) -> Vec<String> {
        self.mls_group
            .as_ref()
            .unwrap()
            .borrow()
            .members()
            .filter(|m| m.signature_key == user_signature)
            .map(|m| hex::encode(m.credential.identity()))
            .collect::<Vec<String>>()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("Unknown group member : {0}")]
    UnknownGroupMemberError(String),
}
