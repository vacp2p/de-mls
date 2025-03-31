use alloy::hex;
use ds::{
    ds_waku::{APP_MSG_SUBTOPIC, COMMIT_MSG_SUBTOPIC, WELCOME_SUBTOPIC},
    waku_actor::ProcessMessageToSend,
};
use kameo::Actor;
use openmls::{group::*, prelude::*};
use openmls_basic_credential::SignatureKeyPair;
use std::{fmt::Display, sync::Arc};
use tokio::sync::Mutex;

use crate::{admin::*, *};
use mls_crypto::openmls_provider::*;

#[derive(Clone, Debug)]
pub enum GroupAction {
    MessageToPrint(MessageToPrint),
    RemoveGroup,
    DoNothing,
}

pub enum GroupState {
    Initialized,
    KeyPackageShared,
}

#[derive(Clone, Debug, Actor)]
pub struct Group {
    group_name: String,
    mls_group: Option<Arc<Mutex<MlsGroup>>>,
    admin: Option<Admin>,
    is_kp_shared: bool,
    app_id: Vec<u8>,
}

impl Group {
    pub fn new(
        group_name: String,
        is_creation: bool,
        provider: Option<&MlsCryptoProvider>,
        signer: Option<&SignatureKeyPair>,
        credential_with_key: Option<&CredentialWithKey>,
    ) -> Result<Self, GroupError> {
        let uuid = uuid::Uuid::new_v4().as_bytes().to_vec();
        if is_creation {
            let group_id = group_name.as_bytes();
            // Create a new MLS group instance
            let group_config = MlsGroupConfig::builder()
                .use_ratchet_tree_extension(true)
                .build();
            let mls_group = MlsGroup::new_with_group_id(
                provider.unwrap(),
                signer.unwrap(),
                &group_config,
                GroupId::from_slice(group_id),
                credential_with_key.unwrap().clone(),
            )?;
            Ok(Group {
                group_name,
                mls_group: Some(Arc::new(Mutex::new(mls_group))),
                admin: Some(Admin::new()),
                is_kp_shared: true,
                app_id: uuid.clone(),
            })
        } else {
            Ok(Group {
                group_name,
                mls_group: None,
                admin: None,
                is_kp_shared: false,
                app_id: uuid.clone(),
            })
        }
    }

    pub async fn members_identity(&self) -> Vec<String> {
        let mls_group = self.mls_group.as_ref().unwrap().lock().await;
        mls_group
            .members()
            .map(|m| hex::encode(m.credential.identity()))
            .collect()
    }

    pub fn set_mls_group(&mut self, mls_group: MlsGroup) -> Result<(), GroupError> {
        self.is_kp_shared = true;
        self.mls_group = Some(Arc::new(Mutex::new(mls_group)));
        Ok(())
    }

    pub fn is_mls_group_initialized(&self) -> bool {
        self.mls_group.is_some()
    }

    pub fn is_kp_shared(&self) -> bool {
        self.is_kp_shared
    }

    pub fn set_kp_shared(&mut self, is_kp_shared: bool) {
        self.is_kp_shared = is_kp_shared;
    }

    pub fn is_admin(&self) -> bool {
        self.admin.is_some()
    }

    pub fn app_id(&self) -> Vec<u8> {
        self.app_id.clone()
    }

    pub fn decrypt_admin_msg(&self, message: Vec<u8>) -> Result<KeyPackage, GroupError> {
        if !self.is_admin() {
            return Err(GroupError::AdminNotSetError);
        }
        let msg: KeyPackage = self.admin.as_ref().unwrap().decrypt_msg(message)?;
        Ok(msg)
    }

    pub fn push_income_key_package(&mut self, key_package: KeyPackage) {
        self.admin
            .as_mut()
            .unwrap()
            .add_income_key_package(key_package);
    }

    pub fn move_income_key_package_to_processed(&mut self) {
        self.admin
            .as_mut()
            .unwrap()
            .move_income_key_package_to_processed();
    }

    pub fn processed_key_packages(&mut self) -> Vec<KeyPackage> {
        self.admin.as_mut().unwrap().processed_key_packages()
    }

    pub async fn add_members(
        &mut self,
        users_kp: Vec<KeyPackage>,
        provider: &MlsCryptoProvider,
        signer: &SignatureKeyPair,
    ) -> Result<Vec<ProcessMessageToSend>, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotInitializedError);
        }
        let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;
        let (out_messages, welcome, _group_info) =
            mls_group.add_members(provider, signer, &users_kp)?;

        mls_group.merge_pending_commit(provider)?;
        let msg_to_send_commit = ProcessMessageToSend {
            msg: out_messages.tls_serialize_detached()?,
            subtopic: COMMIT_MSG_SUBTOPIC.to_string(),
            group_id: self.group_name.clone(),
            app_id: self.app_id.clone(),
        };

        let welcome_serialized = welcome.tls_serialize_detached()?;
        let welcome_msg: Vec<u8> = serde_json::to_vec(&WelcomeMessage {
            message_type: WelcomeMessageType::WelcomeShare,
            message_payload: welcome_serialized,
        })?;

        let msg_to_send_welcome = ProcessMessageToSend {
            msg: welcome_msg,
            subtopic: WELCOME_SUBTOPIC.to_string(),
            group_id: self.group_name.clone(),
            app_id: self.app_id.clone(),
        };

        Ok(vec![msg_to_send_commit, msg_to_send_welcome])
    }

    pub async fn remove_members(
        &mut self,
        users: Vec<String>,
        provider: &MlsCryptoProvider,
        signer: &SignatureKeyPair,
    ) -> Result<ProcessMessageToSend, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotInitializedError);
        }
        let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;
        let mut leaf_indexs = Vec::new();
        let members = mls_group.members().collect::<Vec<_>>();
        for user in users {
            for m in members.iter() {
                if hex::encode(m.credential.identity()) == user {
                    leaf_indexs.push(m.index);
                }
            }
        }
        // Remove operation on the mls group
        let (remove_message, _welcome, _group_info) =
            mls_group.remove_members(provider, signer, &leaf_indexs)?;

        // Second, process the removal on our end.
        mls_group.merge_pending_commit(provider)?;

        let msg_to_send_commit = ProcessMessageToSend {
            msg: remove_message.tls_serialize_detached()?,
            subtopic: COMMIT_MSG_SUBTOPIC.to_string(),
            group_id: self.group_name.clone(),
            app_id: self.app_id.clone(),
        };

        Ok(msg_to_send_commit)
    }

    pub async fn process_protocol_msg(
        &mut self,
        message: ProtocolMessage,
        provider: &MlsCryptoProvider,
        signature_key: Vec<u8>,
    ) -> Result<GroupAction, GroupError> {
        let group_id = message.group_id().as_slice().to_vec();
        if group_id != self.group_name.as_bytes().to_vec() {
            return Ok(GroupAction::DoNothing);
        }
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotInitializedError);
        }
        let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;

        // If the message is from a previous epoch, we don't need to process it and it's a commit for welcome message
        if message.epoch() < mls_group.epoch() && message.epoch() == 0.into() {
            return Ok(GroupAction::DoNothing);
        }

        let processed_message = mls_group.process_message(provider, message)?;
        let processed_message_credential: Credential = processed_message.credential().clone();

        match processed_message.into_content() {
            ProcessedMessageContent::ApplicationMessage(application_message) => {
                let sender_name = {
                    let user_id = mls_group.members().find_map(|m| {
                        if m.credential.identity() == processed_message_credential.identity()
                            && (signature_key != m.signature_key.as_slice())
                        {
                            Some(hex::encode(m.credential.identity()))
                        } else {
                            None
                        }
                    });
                    if user_id.is_none() {
                        return Ok(GroupAction::DoNothing);
                    }
                    user_id.unwrap()
                };

                let conversation_message = MessageToPrint::new(
                    sender_name,
                    String::from_utf8(application_message.into_bytes())?,
                    self.group_name.clone(),
                );
                return Ok(GroupAction::MessageToPrint(conversation_message));
            }
            ProcessedMessageContent::ProposalMessage(_proposal_ptr) => (),
            ProcessedMessageContent::ExternalJoinProposalMessage(_external_proposal_ptr) => (),
            ProcessedMessageContent::StagedCommitMessage(commit_ptr) => {
                let mut remove_proposal: bool = false;
                if commit_ptr.self_removed() {
                    remove_proposal = true;
                }
                mls_group.merge_staged_commit(provider, *commit_ptr)?;
                if remove_proposal {
                    // here we need to remove group instance locally and
                    // also remove correspond key package from local storage ans sc storage
                    if mls_group.is_active() {
                        return Err(GroupError::GroupStillActiveError);
                    }
                    return Ok(GroupAction::RemoveGroup);
                }
            }
        };
        Ok(GroupAction::DoNothing)
    }

    pub fn generate_admin_message(&mut self) -> Result<ProcessMessageToSend, GroupError> {
        let admin = match self.admin.as_mut() {
            Some(a) => a,
            None => return Err(GroupError::AdminNotSetError),
        };
        admin.generate_new_key_pair();
        let admin_msg = admin.generate_admin_message();

        let wm = WelcomeMessage {
            message_type: WelcomeMessageType::GroupAnnouncement,
            message_payload: serde_json::to_vec(&admin_msg)?,
        };
        let msg_to_send = ProcessMessageToSend {
            msg: serde_json::to_vec(&wm)?,
            subtopic: WELCOME_SUBTOPIC.to_string(),
            group_id: self.group_name.clone(),
            app_id: self.app_id.clone(),
        };
        Ok(msg_to_send)
    }

    pub async fn create_message(
        &mut self,
        provider: &MlsCryptoProvider,
        signer: &SignatureKeyPair,
        msg: &str,
        identity: Vec<u8>,
    ) -> Result<ProcessMessageToSend, GroupError> {
        let message_out = self
            .mls_group
            .as_mut()
            .unwrap()
            .lock()
            .await
            .create_message(provider, signer, msg.as_bytes())?
            .tls_serialize_detached()?;
        let app_msg = serde_json::to_vec(&AppMessage {
            sender: identity,
            message: message_out,
        })?;
        Ok(ProcessMessageToSend {
            msg: app_msg,
            subtopic: APP_MSG_SUBTOPIC.to_string(),
            group_id: self.group_name.clone(),
            app_id: self.app_id.clone(),
        })
    }
}

impl Display for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Group: {:#?}", self.group_name)
    }
}
