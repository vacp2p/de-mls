use std::{cell::RefCell, collections::HashMap, rc::Rc, str};

use ds::{ds::*, keystore::PublicKeyStorage, AuthToken, UserKeyPackages};
use openmls::{group::*, prelude::*};
// use waku_bindings::*;

use crate::conversation::*;
use crate::identity::Identity;
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
    auth_token: Option<AuthToken>,
    // pub(crate) contacts: HashMap<Vec<u8>, WakuPeers>,
}

impl User {
    /// Create a new user with the given name and a fresh set of credentials.
    pub fn new(username: &[u8]) -> Result<User, String> {
        let crypto = CryptoProvider::default();
        Ok(User {
            groups: RefCell::new(HashMap::new()),
            identity: RefCell::new(Identity::new(CIPHERSUITE, &crypto, username)),
            provider: crypto,
            auth_token: None,
            // contacts: HashMap::new(),
        })
    }

    pub(crate) fn username(&self) -> String {
        self.identity.borrow().identity_as_string()
    }

    pub(crate) fn id(&self) -> Vec<u8> {
        self.identity.borrow().identity()
    }

    pub(super) fn set_auth_token(&mut self, token: AuthToken) {
        self.auth_token = Some(token);
    }

    pub(super) fn auth_token(&self) -> Option<&AuthToken> {
        self.auth_token.as_ref()
    }

    pub fn create_group(&mut self, group_name: String) -> Result<(), String> {
        let group_id = group_name.as_bytes();

        if self.groups.borrow().contains_key(&group_name) {
            return Err(format!("Error creating user: {:?}", group_name));
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
        )
        .expect("Failed to create MlsGroup");

        let ds = DSClient::new_with_subscriber(self.id());
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

    pub fn register(&mut self, pks: &PublicKeyStorage) -> Result<(), String> {
        match pks.add_user(self.key_packages()) {
            Ok(token) => {
                self.set_auth_token(token);
                Ok(())
            }
            Err(e) => Err(format!("Error creating user: {:?}", e)),
        }
    }

    /// Get the key packages fo this user.
    pub fn key_packages(&self) -> UserKeyPackages {
        // clone first !
        let mut kpgs = self.identity.borrow().kp.clone();
        UserKeyPackages(kpgs.drain().collect::<Vec<(Vec<u8>, KeyPackage)>>())
    }

    pub fn invite(
        &self,
        username: String,
        group_name: String,
        pks: &PublicKeyStorage,
    ) -> Result<MlsMessageIn, String> {
        // First we need to get the key package for {id} from the DS.
        pks.is_client_exist(username.clone().into_bytes())?;

        // Reclaim a key package from the server
        let joiner_key_package = pks.get_user_kp(username.into_bytes());

        // Build a proposal with this key package and do the MLS bits.
        let mut groups = self.groups.borrow_mut();
        let group = match groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(format!("No group with name {group_name} known.")),
        };

        let (out_messages, welcome, _group_info) = group
            .mls_group
            .borrow_mut()
            .add_members(
                &self.provider,
                &self.identity.borrow().signer,
                &[joiner_key_package.unwrap()],
            )
            .map_err(|e| format!("Failed to add member to group - {e}"))?;

        let msg = out_messages.into();
        group.ds_node.as_ref().borrow_mut().msg_send(msg)?;
        // Second, process the invitation on our end.
        group
            .mls_group
            .borrow_mut()
            .merge_pending_commit(&self.provider)
            .expect("error merging pending commit");

        // Put sending welcome by p2p here
        // group.ds_node.as_ref().borrow_mut().msg_send(welcome.into())?;

        drop(groups);

        Ok(welcome.into())
    }

    pub fn recieve_msg(&self, group_name: String, pks: &PublicKeyStorage) -> Result<(), String> {
        let msg = {
            let groups = self.groups.borrow();
            let group = match groups.get(&group_name) {
                Some(g) => g,
                None => return Err("Unknown group".to_string()),
            };
            let msg = group.ds_node.as_ref().borrow_mut().msg_recv(
                self.id(),
                self.auth_token().unwrap().clone(),
                pks,
            );

            msg.unwrap()
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
            _ => return Err("Unsupported message type".to_string()),
        }
        Ok(())
    }

    fn process_protocol_msg(&self, message: ProtocolMessage) -> Result<(), String> {
        let mut groups = self.groups.borrow_mut();
        let group = match groups.get_mut(str::from_utf8(message.group_id().as_slice()).unwrap()) {
            Some(g) => g,
            None => {
                return Err(format!(
                    "Error getting group {:?} for a message",
                    message.group_id()
                ))
            }
        };
        let mut mls_group = group.mls_group.borrow_mut();

        let processed_message = match mls_group.process_message(&self.provider, message) {
            Ok(msg) => msg,
            Err(e) => {
                return Err(format!("Error processing unverified message: {:?}", e));
            }
        };

        let processed_message_credential: Credential = processed_message.credential().clone();

        match processed_message.into_content() {
            ProcessedMessageContent::ApplicationMessage(application_message) => {
                let processed_message_credential = processed_message_credential.clone();

                let sender_name = {
                    let user_id = mls_group.members().find_map(|m| {
                        let m_credential = m.credential.clone();
                        if m_credential.identity()
                            == processed_message_credential.identity()
                            && (self
                                .identity
                                .borrow()
                                .credential_with_key
                                .signature_key
                                .as_slice()
                                != m.signature_key.as_slice())
                        {
                            println!("update::Processing ApplicationMessage read sender name from credential identity for group {} ", group.group_name);
                            Some(
                                str::from_utf8(m_credential.identity()).unwrap().to_owned(),
                            )
                        } else {
                            None
                        }
                    });
                    user_id.unwrap_or("".to_owned()).as_bytes().to_vec()
                };

                let conversation_message = ConversationMessage::new(
                    String::from_utf8(application_message.into_bytes())
                        .unwrap()
                        .clone(),
                    String::from_utf8(sender_name).unwrap(),
                );
                group.conversation.add(conversation_message.clone());
            }
            ProcessedMessageContent::ProposalMessage(_proposal_ptr) => (),
            ProcessedMessageContent::ExternalJoinProposalMessage(_external_proposal_ptr) => (),
            ProcessedMessageContent::StagedCommitMessage(commit_ptr) => {
                let mut remove_proposal: bool = false;
                if commit_ptr.self_removed() {
                    remove_proposal = true;
                }
                match mls_group.merge_staged_commit(&self.provider, *commit_ptr) {
                    Ok(()) => {
                        if remove_proposal {
                            println!(
                                "update::Processing StagedCommitMessage removing {} from group {} ",
                                self.username(),
                                group.group_name
                            );
                            return Ok(());
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        };
        Ok(())
    }

    pub fn send_msg(&mut self, msg: &str, group_name: String) -> Result<(), String> {
        let groups = self.groups.borrow();
        let group = match groups.get(&group_name) {
            Some(g) => g,
            None => return Err("Unknown group".to_string()),
        };

        let message_out = group
            .mls_group
            .borrow_mut()
            .create_message(
                &self.provider,
                &self.identity.borrow().signer,
                msg.as_bytes(),
            )
            .map_err(|e| format!("{e}"))?;

        group
            .ds_node
            .as_ref()
            .borrow_mut()
            .msg_send(message_out.into())?;

        Ok(())
    }

    pub fn join_group(&self, welcome: Welcome, ds: Rc<RefCell<DSClient>>) -> Result<(), String> {
        println!("{} joining group ...", self.username());

        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group = MlsGroup::new_from_welcome(&self.provider, &group_config, welcome, None)
            .expect("Failed to create staged join");

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id.clone()).unwrap();

        ds.clone()
            .borrow_mut()
            .add_subscriber(self.identity.borrow().identity())?;

        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            ds_node: ds,
        };

        match self.groups.borrow_mut().insert(group_name, group) {
            Some(old) => Err(format!("Overrode the group {:?}", old.group_name)),
            None => Ok(()),
        }
    }

    /// Get a member
    fn find_member_index(&self, name: String, group: &Group) -> Result<LeafNodeIndex, String> {
        let mls_group = group.mls_group.borrow();
        for Member {
            index,
            encryption_key: _,
            signature_key: _,
            credential,
        } in mls_group.members()
        {
            if credential.identity() == name.as_bytes() {
                return Ok(index);
            }
        }
        Err("Unknown member".to_string())
    }

    fn recipients(&self, group: &Group, pks: &PublicKeyStorage) -> Result<Vec<Vec<u8>>, String> {
        let mut recipients = Vec::new();

        let mls_group = group.mls_group.borrow();

        for Member {
            index: _,
            encryption_key: _,
            signature_key,
            credential,
        } in mls_group.members()
        {
            if self
                .identity
                .borrow()
                .credential_with_key
                .signature_key
                .as_slice()
                != signature_key.as_slice()
            {
                match pks.is_client_exist(credential.identity().to_vec()) {
                    Ok(()) => recipients.push(credential.identity().to_vec()),
                    Err(e) => {
                        return Err(format!("There's a member in the group we don't know: {e}"))
                    }
                };
            }
        }
        Ok(recipients)
    }

    pub fn remove(&mut self, name: String, group_name: String) -> Result<(), String> {
        // Get the group ID
        let mut groups = self.groups.borrow_mut();
        let group = match groups.get_mut(&group_name) {
            Some(g) => g,
            None => return Err(format!("No group with name {group_name} known.")),
        };

        // Get the client leaf index
        let leaf_index = match self.find_member_index(name, group) {
            Ok(l) => l,
            Err(e) => return Err(e),
        };

        // Remove operation on the mls group
        let (remove_message, _welcome, _group_info) = group
            .mls_group
            .borrow_mut()
            .remove_members(
                &self.provider,
                &self.identity.borrow().signer,
                &[leaf_index],
            )
            .map_err(|e| format!("Failed to remove member from group - {e}"))?;

        let msg = remove_message.into();
        group.ds_node.as_ref().borrow_mut().msg_send(msg)?;

        // Second, process the removal on our end.
        group
            .mls_group
            .borrow_mut()
            .merge_pending_commit(&self.provider)
            .expect("error merging pending commit");

        drop(groups);

        Ok(())
    }

    /// Return the last 100 messages sent to the group.
    pub fn read_msgs(
        &self,
        group_name: String,
    ) -> Result<Option<Vec<ConversationMessage>>, String> {
        let groups = self.groups.borrow();
        groups.get(&group_name).map_or_else(
            || Err("Unknown group".to_string()),
            |g| {
                Ok(g.conversation
                    .get(100)
                    .map(|messages: &[crate::conversation::ConversationMessage]| messages.to_vec()))
            },
        )
    }
}
