use alloy::{
    hex::{self, FromHexError},
    network::{EthereumWallet, Network},
    primitives::Address,
    providers::Provider,
    signers::{
        local::{LocalSignerError, PrivateKeySigner},
        SignerSync,
    },
    transports::Transport,
};
use fred::types::Message;
use openmls::{group::*, prelude::*};
use openmls_rust_crypto::MemoryKeyStoreError;
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Display,
    str::{from_utf8, FromStr, Utf8Error},
    string::FromUtf8Error,
};
use tokio::sync::broadcast::Receiver;

use ds::{
    chat_client::{ChatClient, ReqMessageType, RequestMLSPayload, ResponseMLSPayload},
    ds::*,
    ChatServiceError,
};
use mls_crypto::openmls_provider::*;
use sc_key_store::{sc_ks::ScKeyStorage, *};

use crate::{
    contact::ContactError,
    identity::{Identity, IdentityError},
};
use crate::{contact::ContactsList, conversation::*};

pub struct Group {
    group_name: String,
    conversation: Conversation,
    mls_group: RefCell<MlsGroup>,
    rc_client: RClient,
    // pubsub_topic: WakuPubSubTopic,
    // content_topics: Vec<WakuContentTopic>,
}
impl Display for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Group: {:#?}", self.group_name)
    }
}

pub struct User<T, P, N> {
    pub identity: Identity,
    pub groups: HashMap<String, Group>,
    provider: MlsCryptoProvider,
    eth_signer: PrivateKeySigner,
    // we don't need on-chain connection if we don't create a group
    sc_ks: Option<ScKeyStorage<T, P, N>>,
    pub contacts: ContactsList,
}

impl<T, P, N> User<T, P, N>
where
    T: Transport + Clone,
    P: Provider<T, N>,
    N: Network,
{
    /// Create a new user with the given name and a fresh set of credentials.
    pub async fn new(user_eth_priv_key: &str, chat_client: ChatClient) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(user_eth_priv_key)?;
        let user_address = signer.address();

        let crypto = MlsCryptoProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, user_address.as_slice())?;
        let user = User {
            groups: HashMap::new(),
            identity: id,
            eth_signer: signer,
            provider: crypto,
            sc_ks: None,
            contacts: ContactsList::new(chat_client).await?,
        };
        Ok(user)
    }

    pub async fn connect_to_smart_contract(
        &mut self,
        sc_storage_address: &str,
        provider: P,
    ) -> Result<(), UserError> {
        let storage_address = Address::from_str(sc_storage_address)?;
        self.sc_ks = Some(ScKeyStorage::new(provider, storage_address));
        self.sc_ks
            .as_mut()
            .unwrap()
            .add_user(&self.identity.to_string())
            .await?;
        Ok(())
    }

    pub async fn create_group(
        &mut self,
        group_name: String,
    ) -> Result<Receiver<Message>, UserError> {
        let group_id = group_name.as_bytes();

        if self.groups.contains_key(&group_name) {
            return Err(UserError::AlreadyExistedGroupError(group_name));
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

        let (rc, broadcaster) = RClient::new_for_group(group_name.clone()).await?;
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            rc_client: rc,
            // pubsub_topic: WakuPubSubTopic::new(),
            // content_topics: Vec::new(),
        };

        self.groups.insert(group_name, group);
        Ok(broadcaster)
    }

    pub async fn add_user_to_acl(&mut self, user_address: &str) -> Result<(), UserError> {
        if self.sc_ks.is_none() {
            return Err(UserError::EmptyScConnection);
        }
        self.sc_ks.as_mut().unwrap().add_user(user_address).await?;
        Ok(())
    }

    pub async fn restore_key_package(
        &mut self,
        mut signed_kp: &[u8],
    ) -> Result<KeyPackage, UserError> {
        if self.sc_ks.is_none() {
            return Err(UserError::EmptyScConnection);
        }

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
        if self.sc_ks.is_none() {
            return Err(UserError::EmptyScConnection);
        }

        // for user_wallet in users.iter() {
        //     if !self.contacts.does_user_in_contacts(user_wallet).await {
        //         self.contacts.add_new_contact(user_wallet).await?;
        //     }
        //     self.contacts
        //         .send_req_msg_to_user(
        //             self.identity.to_string(),
        //             user_wallet,
        //             self.sc_ks.as_mut().unwrap().get_sc_adsress(),
        //             ReqMessageType::InviteToGroup,
        //         )
        //         .await?;
        //     println!("waiting token");
        //     match self.contacts.future_req.get(user_wallet) {
        //         Some(token) => token.cancelled().await,
        //         None => return Err(UserError::UnknownUserError),
        //     };
        //     println!("cancelled token");
        //     self.contacts.future_req.remove(user_wallet);

        //     self.add_user_to_acl(user_wallet).await?;
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
            None => return Err(UserError::UnknownGroupError(group_name)),
        };

        let (out_messages, welcome, _group_info) = group.mls_group.borrow_mut().add_members(
            &self.provider,
            &self.identity.signer,
            &joiners_key_package,
        )?;

        group
            .rc_client
            .msg_send(out_messages, self.identity.to_string())
            .await?;
        // Second, process the invitation on our end.
        group
            .mls_group
            .borrow_mut()
            .merge_pending_commit(&self.provider)?;
        // Send welcome by p2p
        self.contacts
            .send_welcome_msg_to_users(self.identity.to_string(), user_addrs, welcome)?;

        Ok(())
    }

    pub async fn receive_msg(
        &mut self,
        msg_bytes: Vec<u8>,
    ) -> Result<Option<ConversationMessage>, UserError> {
        let buf: SenderStruct = serde_json::from_slice(&msg_bytes)?;
        if buf.sender == self.identity.to_string() {
            return Ok(None);
        }
        let res = MlsMessageIn::tls_deserialize_bytes(&buf.msg)?;
        let msg = match res.extract() {
            MlsMessageInBody::PrivateMessage(message) => {
                self.process_protocol_msg(message.into())?
            }
            MlsMessageInBody::PublicMessage(message) => {
                self.process_protocol_msg(message.into())?
            }
            _ => return Err(UserError::MessageTypeError),
        };
        Ok(msg)
    }

    pub fn process_protocol_msg(
        &mut self,
        message: ProtocolMessage,
    ) -> Result<Option<ConversationMessage>, UserError> {
        let group_name = from_utf8(message.group_id().as_slice())?.to_string();
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::UnknownGroupError(group_name)),
        };
        let mut mls_group = group.mls_group.borrow_mut();

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

    pub async fn send_msg(
        &mut self,
        msg: &str,
        group_name: String,
        sender: String,
    ) -> Result<(), UserError> {
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::UnknownGroupError(group_name)),
        };

        let message_out = group.mls_group.borrow_mut().create_message(
            &self.provider,
            &self.identity.signer,
            msg.as_bytes(),
        )?;

        group.rc_client.msg_send(message_out, sender).await?;
        Ok(())
    }

    pub async fn join_group(
        &mut self,
        welcome: String,
    ) -> Result<(Receiver<Message>, String), UserError> {
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

        let (rc, br) = RClient::new_for_group(group_name.clone()).await?;
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            rc_client: rc,
        };

        match self.groups.insert(group_name.clone(), group) {
            Some(old) => Err(UserError::AlreadyExistedGroupError(old.group_name)),
            None => Ok((br, group_name)),
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
            || Err(UserError::UnknownGroupError(group_name)),
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
            None => return Err(UserError::UnknownGroupError(group_name)),
        };
        Ok(group.group_members(self.identity.signature_pub_key().as_slice()))
    }

    pub fn user_groups(&self) -> Result<Vec<String>, UserError> {
        if self.groups.is_empty() {
            return Ok(Vec::default());
        }
        Ok(self.groups.keys().map(|k| k.to_owned()).collect())
    }

    pub fn get_wallet(&self) -> EthereumWallet {
        EthereumWallet::from(self.eth_signer.clone())
    }

    fn generate_signature(&self, msg: String) -> Result<String, UserError> {
        let signature = self.eth_signer.sign_message_sync(msg.as_bytes())?;
        let res = serde_json::to_string(&signature)?;
        Ok(res)
    }

    pub fn send_responce_on_request(
        &mut self,
        req: RequestMLSPayload,
        self_address: String,
        user_address: &str,
    ) -> Result<(), UserError> {
        match req.msg_type {
            ReqMessageType::InviteToGroup => {
                let signature = self.generate_signature(req.msg)?;
                let key_package = self
                    .identity
                    .generate_key_package(CIPHERSUITE, &self.provider)?;
                let resp = ResponseMLSPayload::new(
                    signature,
                    self_address.clone(),
                    key_package.tls_serialize_detached()?,
                );
                self.contacts
                    .send_resp_msg_to_user(self_address, user_address, resp)?;

                Ok(())
            }
            ReqMessageType::RemoveFromGroup => Ok(()),
        }
    }

    pub async fn parce_responce(
        &mut self,
        resp: ResponseMLSPayload,
        group_name: String,
    ) -> Result<(), UserError> {
        if self.sc_ks.is_none() {
            return Err(UserError::EmptyScConnection);
        }
        let (user_wallet, kp) = resp.validate(self.sc_ks.as_ref().unwrap().get_sc_adsress())?;

        self.contacts
            .add_key_package_to_contact(&user_wallet, kp, group_name.clone())
            .await?;

        self.contacts.handle_response(&user_wallet)?;
        Ok(())
    }

    pub fn get_sc_address(&self) -> Result<String, UserError> {
        if self.sc_ks.is_none() {
            return Err(UserError::EmptyScConnection);
        }
        Ok(self.sc_ks.as_ref().unwrap().get_sc_adsress())
    }
}

impl Group {
    /// Get a member
    fn find_member_index(&self, user_id: String) -> Result<LeafNodeIndex, GroupError> {
        let member = self
            .mls_group
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

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("User doesn't have connection to smart contract")]
    EmptyScConnection,
    #[error("Unknown group: {0}")]
    UnknownGroupError(String),
    #[error("Group already exist: {0}")]
    AlreadyExistedGroupError(String),
    #[error("Unsupported message type")]
    MessageTypeError,
    #[error("User already exist: {0}")]
    AlreadyExistedUserError(String),
    #[error("Empty welcome message")]
    EmptyWelcomeMessageError,
    #[error("Inner Message from server is invalid")]
    InvalidChatMessageError,
    #[error("Message from server is invalid")]
    InvalidServerMessageError,
    #[error("Unknown user")]
    UnknownUserError,

    #[error(transparent)]
    DeliveryServiceError(#[from] DeliveryServiceError),
    #[error(transparent)]
    KeyStoreError(#[from] KeyStoreError),
    #[error(transparent)]
    IdentityError(#[from] IdentityError),
    #[error(transparent)]
    ContactError(#[from] ContactError),
    #[error(transparent)]
    ChatServiceError(#[from] ChatServiceError),

    #[error("Something wrong while creating Mls group: {0}")]
    MlsGroupCreationError(#[from] NewGroupError<MemoryKeyStoreError>),
    #[error("Something wrong while adding member to Mls group: {0}")]
    MlsAddMemberError(#[from] AddMembersError<MemoryKeyStoreError>),
    #[error("Something wrong while merging pending commit: {0}")]
    MlsMergePendingCommitError(#[from] MergePendingCommitError<MemoryKeyStoreError>),
    #[error("Something wrong while merging commit: {0}")]
    MlsMergeCommitError(#[from] MergeCommitError<MemoryKeyStoreError>),
    #[error("Error processing unverified message: {0}")]
    MlsProcessMessageError(#[from] ProcessMessageError),
    #[error("Something wrong while creating message: {0}")]
    MlsCreateMessageError(#[from] CreateMessageError),
    #[error("Failed to create staged join: {0}")]
    MlsWelcomeError(#[from] WelcomeError<MemoryKeyStoreError>),
    #[error("Failed to remove member from group: {0}")]
    MlsRemoveMembersError(#[from] RemoveMembersError<MemoryKeyStoreError>),
    #[error("Failed to validate user key_pacakge: {0}")]
    MlsKeyPackageVerifyError(#[from] KeyPackageVerifyError),

    #[error("Parse String UTF8 error: {0}")]
    ParseUTF8Error(#[from] FromUtf8Error),
    #[error("Parse str UTF8 error: {0}")]
    ParseStrUTF8Error(#[from] Utf8Error),
    #[error("Json error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Serialization problem: {0}")]
    TlsError(#[from] tls_codec::Error),
    #[error("Unable to parce the address: {0}")]
    AlloyFromHexError(#[from] FromHexError),
    #[error("Unable to parce the signer: {0}")]
    AlloyParceSignerError(#[from] LocalSignerError),
    #[error("Unable to sign the message: {0}")]
    AlloySignersError(#[from] alloy::signers::Error),
    #[error("Write to stdout error")]
    IoError(#[from] std::io::Error),

    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
