use std::str::Utf8Error;
use std::string::FromUtf8Error;
use std::{cell::RefCell, collections::HashMap, rc::Rc, str};

use ds::ds::*;
use openmls::{group::*, prelude::*};
use openmls_rust_crypto::MemoryKeyStoreError;
use sc_key_store::*;
use sc_key_store::{local_ks::LocalCache, pks::PublicKeyStorage};
// use waku_bindings::*;

use crate::conversation::*;
use crate::identity::{Identity, IdentityError};
use crate::openmls_provider::{CryptoProvider, CIPHERSUITE};

#[derive(Debug)]
pub struct Group {
    group_name: String,
    conversation: Conversation,
    mls_group: RefCell<MlsGroup>,
    pub ds_node: Rc<RefCell<DSClient>>,
    // pubsub_topic: WakuPubSubTopic,
    // content_topics: Vec<WakuContentTopic>,
}

pub struct User {
    pub(crate) identity: RefCell<Identity>,
    pub(crate) groups: RefCell<HashMap<String, Group>>,
    provider: CryptoProvider,
    local_ks: LocalCache,
    // pub(crate) contacts: HashMap<Vec<u8>, WakuPeers>,
}

impl User {
    /// Create a new user with the given name and a fresh set of credentials.
    pub fn new(username: &[u8]) -> Result<User, UserError> {
        let crypto = CryptoProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, username)?;
        Ok(User {
            groups: RefCell::new(HashMap::new()),
            identity: RefCell::new(id),
            provider: crypto,
            local_ks: LocalCache::empty_key_store(username),
            // contacts: HashMap::new(),
        })
    }

    pub(crate) fn username(&self) -> String {
        self.identity.borrow().to_string()
    }

    pub(crate) fn id(&self) -> Vec<u8> {
        self.identity.borrow().identity()
    }

    fn is_signature_eq(&self, sign: &Vec<u8>) -> bool {
        self.identity
            .borrow()
            .credential_with_key
            .signature_key
            .as_slice()
            == sign
    }

    pub fn create_group(&mut self, group_name: String) -> Result<(), UserError> {
        let group_id = group_name.as_bytes();

        if self.groups.borrow().contains_key(&group_name) {
            return Err(UserError::UnknownGroupError(group_name));
        }

        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group = MlsGroup::new_with_group_id(
            &self.provider,
            &self.identity.borrow().signer,
            &group_config,
            GroupId::from_slice(group_id),
            self.identity.borrow().credential_with_key.clone(),
        )?;

        let ds = DSClient::new_with_subscriber(self.id())?;
        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            ds_node: Rc::new(RefCell::new(ds)),
            // pubsub_topic: WakuPubSubTopic::new(),
            // content_topics: Vec::new(),
        };

        self.groups.borrow_mut().insert(group_name, group);
        Ok(())
    }

    pub fn register(&mut self, mut pks: &mut PublicKeyStorage) -> Result<(), UserError> {
        pks.add_user(self.key_packages(), self.identity.borrow().signer.public())?;
        self.local_ks.get_update_from_smart_contract(pks)?;
        Ok(())
    }

    /// Get the key packages fo this user.
    pub fn key_packages(&self) -> UserKeyPackages {
        let mut kpgs = self.identity.borrow().kp.clone();
        UserKeyPackages(kpgs.drain().collect::<Vec<(Vec<u8>, KeyPackage)>>())
    }

    pub fn invite(
        &mut self,
        username: String,
        group_name: String,
        mut pks: &mut PublicKeyStorage,
    ) -> Result<MlsMessageIn, UserError> {
        // First we need to get the key package for {id} from the DS.
        if !pks.does_user_exist(username.as_bytes()) {
            return Err(UserError::UnknownUserError);
        }

        // Reclaim a key package from the server
        let joiner_key_package = pks.get_avaliable_user_kp(username.as_bytes())?;

        // Build a proposal with this key package and do the MLS bits.
        let mut groups = self.groups.borrow_mut();
        let group = match groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::UnknownGroupError(group_name)),
        };

        let (out_messages, welcome, _group_info) = group.mls_group.borrow_mut().add_members(
            &self.provider,
            &self.identity.borrow().signer,
            &[joiner_key_package],
        )?;

        let msg = out_messages.into();
        group.ds_node.as_ref().borrow_mut().msg_send(msg)?;
        // Second, process the invitation on our end.
        group
            .mls_group
            .borrow_mut()
            .merge_pending_commit(&self.provider)?;

        // Put sending welcome by p2p here
        // group.ds_node.as_ref().borrow_mut().msg_send(welcome.into())?;

        drop(groups);

        Ok(welcome.into())
    }

    pub fn recieve_msg(&self, group_name: String, pks: &PublicKeyStorage) -> Result<(), UserError> {
        let msg = {
            let groups = self.groups.borrow();
            let group = match groups.get(&group_name) {
                Some(g) => g,
                None => return Err(UserError::UnknownGroupError(group_name)),
            };
            let msg = group
                .ds_node
                .as_ref()
                .borrow_mut()
                .msg_recv(self.id().as_ref(), pks)?;
            msg
        };

        match msg.extract() {
            MlsMessageInBody::Welcome(_welcome) => {
                // Now irrelevant because message are attached to group
                // self.join_group(welcome, Rc::clone(&group.ds_node))?;
            }
            MlsMessageInBody::PrivateMessage(message) => {
                self.process_protocol_msg(message.into())?;
            }
            MlsMessageInBody::PublicMessage(message) => {
                self.process_protocol_msg(message.into())?;
            }
            _ => return Err(UserError::MessageTypeError),
        }
        Ok(())
    }

    fn process_protocol_msg(&self, message: ProtocolMessage) -> Result<(), UserError> {
        let mut groups = self.groups.borrow_mut();
        let group_name = str::from_utf8(message.group_id().as_slice())?;
        let group = match groups.get_mut(group_name) {
            Some(g) => g,
            None => return Err(UserError::UnknownGroupError(group_name.to_string())),
        };
        let mut mls_group = group.mls_group.borrow_mut();

        let processed_message = mls_group.process_message(&self.provider, message)?;

        let processed_message_credential: Credential = processed_message.credential().clone();

        match processed_message.into_content() {
            ProcessedMessageContent::ApplicationMessage(application_message) => {
                let sender_name = {
                    let user_id = mls_group.members().find_map(|m| {
                        if m.credential.identity()
                            == processed_message_credential.identity()
                            && (self
                                .identity
                                .borrow()
                                .credential_with_key
                                .signature_key
                                .as_slice()
                                != m.signature_key.as_slice())
                        {
                            println!("process ApplicationMessage: read sender name from credential identity for group {} ", group.group_name);
                            Some(
                                str::from_utf8(m.credential.identity()).unwrap().to_owned(),
                            )
                        } else {
                            None
                        }
                    });
                    user_id.unwrap_or("".to_owned()).as_bytes().to_vec()
                };

                let conversation_message = ConversationMessage::new(
                    String::from_utf8(application_message.into_bytes())?,
                    String::from_utf8(sender_name)?,
                );
                group.conversation.add(conversation_message);
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
                    println!(
                        "update::Processing StagedCommitMessage removing {} from group {} ",
                        self.username(),
                        group.group_name
                    );
                    return Ok(());
                }
            }
        };
        Ok(())
    }

    pub fn send_msg(&mut self, msg: &str, group_name: String) -> Result<(), UserError> {
        let groups = self.groups.borrow();
        let group = match groups.get(&group_name) {
            Some(g) => g,
            None => return Err(UserError::UnknownGroupError(group_name)),
        };

        let message_out = group.mls_group.borrow_mut().create_message(
            &self.provider,
            &self.identity.borrow().signer,
            msg.as_bytes(),
        )?;

        group
            .ds_node
            .as_ref()
            .borrow_mut()
            .msg_send(message_out.into())?;

        Ok(())
    }

    pub fn join_group(&self, welcome: Welcome, ds: Rc<RefCell<DSClient>>) -> Result<(), UserError> {
        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group = MlsGroup::new_from_welcome(&self.provider, &group_config, welcome, None)?;

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id)?;

        ds.borrow_mut()
            .add_subscriber(self.identity.borrow().identity())?;

        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            ds_node: ds,
        };

        match self.groups.borrow_mut().insert(group_name, group) {
            Some(old) => Err(UserError::AlreadyExistedGroupError(old.group_name)),
            None => Ok(()),
        }
    }

    /// Get a member
    fn find_member_index(&self, name: String, group: &Group) -> Result<LeafNodeIndex, UserError> {
        let mls_group = group.mls_group.borrow();

        let member = mls_group
            .members()
            .find(|m| m.credential.identity().eq(name.as_bytes()));

        match member {
            Some(m) => Ok(m.index),
            None => Err(UserError::UnknownGroupMemberError(name)),
        }
    }

    fn group_members(&self, group: &Group) -> Result<Vec<Vec<u8>>, UserError> {
        let mls_group = group.mls_group.borrow();

        let members: Vec<Vec<u8>> = mls_group
            .members()
            .filter(|m| self.is_signature_eq(&m.signature_key))
            .map(|m| m.credential.identity().to_vec())
            .collect();
        Ok(members)
    }

    pub fn remove(&mut self, name: String, group_name: String) -> Result<(), UserError> {
        // Get the group ID
        let mut groups = self.groups.borrow_mut();
        let group = match groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(UserError::UnknownGroupError(group_name)),
        };

        // Get the user leaf index
        let leaf_index = self.find_member_index(name, group)?;

        // Remove operation on the mls group
        let (remove_message, _welcome, _group_info) = group.mls_group.borrow_mut().remove_members(
            &self.provider,
            &self.identity.borrow().signer,
            &[leaf_index],
        )?;

        let msg = remove_message.into();
        group.ds_node.as_ref().borrow_mut().msg_send(msg)?;

        // Second, process the removal on our end.
        group
            .mls_group
            .borrow_mut()
            .merge_pending_commit(&self.provider)?;

        drop(groups);

        Ok(())
    }

    /// Return the last 100 messages sent to the group.
    pub fn read_msgs(
        &self,
        group_name: String,
    ) -> Result<Option<Vec<ConversationMessage>>, UserError> {
        let groups = self.groups.borrow();
        groups.get(&group_name).map_or_else(
            || Err(UserError::UnknownGroupError(group_name)),
            |g| {
                Ok(g.conversation
                    .get(100)
                    .map(|messages: &[crate::conversation::ConversationMessage]| messages.to_vec()))
            },
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("Unknown group: {0}")]
    UnknownGroupError(String),
    #[error("Group already exist: {0}")]
    AlreadyExistedGroupError(String),
    #[error("Unknown group member : {0}")]
    UnknownGroupMemberError(String),
    #[error("Unsupported message type")]
    MessageTypeError,
    #[error("Unknown user")]
    UnknownUserError,
    #[error("Delivery Service error: {0}")]
    DeliveryServiceError(#[from] DeliveryServiceError),
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
    #[error("Unknown error: {0}")]
    Other(anyhow::Error),
}
