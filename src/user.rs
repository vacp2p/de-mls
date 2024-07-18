use alloy::{
    hex::{self, FromHexError, ToHexExt},
    network::Network,
    primitives::Address,
    providers::Provider,
    transports::Transport,
};
use fred::types::Message;
use openmls::{group::*, prelude::*};
use openmls_rust_crypto::MemoryKeyStoreError;
use std::{
    borrow::BorrowMut,
    cell::RefCell,
    collections::HashMap,
    fmt::Display,
    fs::File,
    io::Write,
    str::{from_utf8, FromStr, Utf8Error},
    string::FromUtf8Error,
};
use tokio::sync::broadcast::Receiver;

use ds::ds::*;
use mls_crypto::openmls_provider::*;
use sc_key_store::{local_ks::LocalCache, sc_ks::ScKeyStorage, *};
// use waku_bindings::*;
//

use crate::conversation::*;
use crate::identity::{Identity, IdentityError};

pub struct Group {
    group_name: String,
    conversation: Conversation,
    mls_group: RefCell<MlsGroup>,
    epoch: GroupEpoch,
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
    sc_ks: ScKeyStorage<T, P, N>,
    local_ks: LocalCache,
    // pub(crate) contacts: HashMap<Vec<u8>, WakuPeers>,
}

impl<T, P, N> User<T, P, N>
where
    T: Transport + Clone,
    P: Provider<T, N>,
    N: Network,
{
    /// Create a new user with the given name and a fresh set of credentials.
    pub async fn new(
        user_wallet_address: &[u8],
        provider: P,
        sc_storage_address: Address,
    ) -> Result<Self, UserError> {
        let crypto = MlsCryptoProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, user_wallet_address)?;
        let mut user = User {
            groups: HashMap::new(),
            identity: id,
            provider: crypto,
            local_ks: LocalCache::empty_key_store(user_wallet_address),
            sc_ks: ScKeyStorage::new(provider, sc_storage_address),
            // contacts: HashMap::new(),
        };
        user.register().await?;
        Ok(user)
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
        let epoch = mls_group.epoch();
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            epoch,
            rc_client: rc,
            // pubsub_topic: WakuPubSubTopic::new(),
            // content_topics: Vec::new(),
        };

        self.groups.insert(group_name, group);
        Ok(broadcaster)
    }

    async fn register(&mut self) -> Result<(), UserError> {
        let kp = self.key_packages();
        self.sc_ks
            .borrow_mut()
            .add_user(kp, self.identity.signer.public())
            .await?;
        self.local_ks
            .get_update_from_smart_contract(self.sc_ks.borrow_mut(), &self.provider)
            .await?;
        Ok(())
    }

    /// Get the key packages fo this user.
    pub fn key_packages(&self) -> UserKeyPackages {
        let mut kpgs = self.identity.kp.clone();
        UserKeyPackages(kpgs.drain().collect::<Vec<(Vec<u8>, KeyPackage)>>())
    }

    pub async fn invite(
        &mut self,
        user_wallet: String,
        group_name: String,
    ) -> Result<(), UserError> {
        let user_address = Address::from_str(&user_wallet)?;
        let user_wallet_address = user_address.as_slice();
        // First we need to get the key package for {id} from the DS.
        if !self
            .sc_ks
            .borrow_mut()
            .does_user_exist(user_wallet_address)
            .await?
        {
            return Err(UserError::UnknownUserError);
        }

        // Reclaim a key package from the server
        let joiner_key_package = self
            .sc_ks
            .borrow_mut()
            .get_avaliable_user_kp(user_wallet_address, &self.provider)
            .await?;

        // Build a proposal with this key package and do the MLS bits.
        let group = match self.groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::UnknownGroupError(group_name)),
        };

        let (out_messages, welcome, _group_info) = group.mls_group.borrow_mut().add_members(
            &self.provider,
            &self.identity.signer,
            &[joiner_key_package],
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
        group.epoch = group.mls_group.borrow_mut().epoch();
        // Put sending welcome by p2p here
        let bytes = welcome.tls_serialize_detached()?;
        let string = bytes.encode_hex();

        let mut file = File::create(format!("invite_{group_name}.txt"))?;
        file.write_all(string.as_bytes())?;

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
                group.epoch = group.mls_group.borrow_mut().epoch();
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
        welcome: Welcome,
    ) -> Result<(Receiver<Message>, String), UserError> {
        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group = MlsGroup::new_from_welcome(&self.provider, &group_config, welcome, None)?;

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id)?;

        let (rc, br) = RClient::new_for_group(group_name.clone()).await?;
        let epoch = mls_group.epoch();
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            epoch,
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
    #[error("Unknown group: {0}")]
    UnknownGroupError(String),
    #[error("Group already exist: {0}")]
    AlreadyExistedGroupError(String),
    #[error("Unsupported message type")]
    MessageTypeError,
    #[error("Unknown user")]
    UnknownUserError,
    #[error("Empty welcome message")]
    EmptyWelcomeMessageError,

    #[error("Delivery Service error: {0}")]
    DeliveryServiceError(#[from] DeliveryServiceError),
    #[error(transparent)]
    GroupError(#[from] GroupError),
    #[error("Key Store error: {0}")]
    KeyStoreError(#[from] KeyStoreError),
    #[error("Identity error: {0}")]
    IdentityError(#[from] IdentityError),

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
    #[error("Write to stdout error")]
    IoError(#[from] std::io::Error),

    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
