use std::borrow::{Borrow, BorrowMut};
use std::collections::HashSet;
use std::{cell::RefCell, collections::HashMap, str};
use tracing::{debug, trace};

use ds::keystore::PublicKeyStorage;
use ds::AuthToken;
use ds::{ds::*, UserInfo, UserKeyPackages};
use openmls::group::*;
use openmls::prelude::*;
// use waku_bindings::*;

use crate::conversation::*;
use crate::identity::Identity;
use crate::openmls_provider::{CryptoProvider, CIPHERSUITE};

pub struct Contact {
    id: Vec<u8>,
}

impl Contact {
    fn username(&self) -> String {
        String::from_utf8(self.id.clone()).unwrap()
    }
}

#[derive(Debug)]
pub struct Group {
    group_name: String,
    conversation: Conversation,
    mls_group: RefCell<MlsGroup>,
    ds_node: DSClient,
    // pubsub_topic: WakuPubSubTopic,
    // content_topics: Vec<WakuContentTopic>,
}

impl Group {}

pub struct User {
    pub(crate) identity: RefCell<Identity>,
    pub(crate) groups: RefCell<HashMap<String, Group>>,
    group_list: HashSet<String>,

    provider: CryptoProvider,
    // pub(crate) contacts: HashMap<Vec<u8>, WakuPeers>,
    auth_token: Option<AuthToken>,
}

impl User {
    /// Create a new user with the given name and a fresh set of credentials.
    pub fn new(username: &[u8]) -> Self {
        let crypto = CryptoProvider::default();
        Self {
            groups: RefCell::new(HashMap::new()),
            group_list: HashSet::new(),
            // contacts: HashMap::new(),
            identity: RefCell::new(Identity::new(CIPHERSUITE, &crypto, username)),
            provider: crypto,
            auth_token: None,
        }
    }

    pub(crate) fn username(&self) -> String {
        self.identity.borrow().identity_as_string()
    }

    pub(super) fn set_auth_token(&mut self, token: AuthToken) {
        self.auth_token = Some(token);
    }

    pub(super) fn auth_token(&self) -> Option<&AuthToken> {
        self.auth_token.as_ref()
    }

    pub fn create_group(&mut self, name: String) -> Result<(), String> {
        let group_id = name.as_bytes();

        if self.groups.borrow().contains_key(&name) {
            return Err(format!("Error creating user: {:?}", name));
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

        let group = Group {
            group_name: name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            ds_node: DSClient::new_with_subscriber(self.identity.borrow().identity().to_vec()),
            // pubsub_topic: WakuPubSubTopic::new(),
            // content_topics: Vec::new(),
        };

        self.groups.borrow_mut().insert(name, group);
        Ok(())
    }

    pub fn register(&mut self, pks: &PublicKeyStorage) -> Result<(), String> {
        match pks.add_user(self.key_packages()) {
            Ok(token) => Ok(self.set_auth_token(token)),
            Err(e) => Err(format!("Error creating user: {:?}", e)),
        }
    }

    /// Get the key packages fo this user.
    pub fn key_packages(&self) -> UserKeyPackages {
        // clone first !
        let mut kpgs = self.identity.borrow().kp.clone();
        UserKeyPackages(
            kpgs.drain()
                .map(|(e1, e2)| (e1, e2))
                .collect::<Vec<(Vec<u8>, KeyPackage)>>()
                .into(),
        )
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
                    Err(c) => return Err(format!("There's a member in the group we don't know.")),
                };
            }
        }
        Ok(recipients)
    }

    pub fn invite(
        &self,
        username: String,
        group_name: String,
        pks: &PublicKeyStorage,
    ) -> Result<(), String> {
        // First we need to get the key package for {id} from the DS.
        match pks.is_client_exist(username.clone().into_bytes()) {
            Ok(v) => v,
            Err(e) => return Err(e),
        };

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

        /* First, send the MlsMessage commit to the group.
        This must be done before the member invitation is locally committed.
        It avoids the invited member to receive the commit message (which is in the previous group epoch).*/
        let group = groups.get_mut(&group_name).unwrap(); // XXX: not cool.
        let group_recipients = self.recipients(group, pks).unwrap();

        let msg = GroupMessage::new(out_messages.into(), &group_recipients);
        group.borrow_mut().ds_node.msg_send(msg)?;
        // Second, process the invitation on our end.
        group
            .mls_group
            .borrow_mut()
            .merge_pending_commit(&self.provider)
            .expect("error merging pending commit");

        let msg = GroupMessage::new(welcome.into(), &group_recipients);
        group.ds_node.msg_send(msg)?;

        drop(groups);

        Ok(())
    }

    pub fn recieve_msg(
        &self,
        group_name: String,
        pks: &PublicKeyStorage,
    ) -> Result<Vec<u8>, String> {
        let groups = self.groups.borrow();
        let group = match groups.get(&group_name) {
            Some(g) => g,
            None => return Err("Unknown group".to_string()),
        };

        Ok(vec![])
    }

    pub fn send_msg(
        &mut self,
        msg: &str,
        group_name: String,
        pks: &PublicKeyStorage,
    ) -> Result<(), String> {
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

        let msg = GroupMessage::new(message_out.into(), &self.recipients(group, pks).unwrap());
        group.ds_node.msg_send(msg)?;

        Ok(())
    }

    fn join_group(&self, welcome: Welcome, ds: &DSClient) -> Result<(), String> {
        debug!("{} joining group ...", self.username());

        let mut ident = self.identity.borrow_mut();
        for secret in welcome.secrets().iter() {
            let key_package_hash = &secret.new_member();
            if ident.kp.contains_key(key_package_hash.as_slice()) {
                ident.kp.remove(key_package_hash.as_slice());
            }
        }

        let group_config = MlsGroupConfig::builder()
            .use_ratchet_tree_extension(true)
            .build();

        let mls_group = MlsGroup::new_from_welcome(&self.provider, &group_config, welcome, None)
            .expect("Failed to create staged join");

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id.clone()).unwrap();

        let group = Group {
            group_name: group_name.clone(),
            conversation: Conversation::default(),
            mls_group: RefCell::new(mls_group),
            ds_node: ds.clone(),
        };

        trace!("   {}", group_name);

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

    pub fn remove(
        &mut self,
        name: String,
        group_name: String,
        pks: &PublicKeyStorage,
    ) -> Result<(), String> {
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

        // First, send the MlsMessage remove commit to the group.
        trace!("Sending commit");
        let group = groups.get_mut(&group_name).unwrap(); // XXX: not cool.
        let group_recipients = self.recipients(group, pks).unwrap();

        let msg = GroupMessage::new(remove_message.into(), &group_recipients);
        group.ds_node.msg_send(msg)?;

        // Second, process the removal on our end.
        group
            .mls_group
            .borrow_mut()
            .merge_pending_commit(&self.provider)
            .expect("error merging pending commit");

        drop(groups);

        Ok(())
    }
}
