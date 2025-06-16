use alloy::hex;
use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use kameo::Actor;
// use log::info;
use openmls::{group::*, prelude::*};
use openmls::{
    group::{GroupId, MlsGroup},
    prelude::{
        hash_ref::ProposalRef, Credential, CredentialWithKey, KeyPackage, ProcessedMessageContent,
        ProtocolMessage,
    },
};
use openmls_basic_credential::SignatureKeyPair;
use prost::Message;
use std::{fmt::Display, sync::Arc};
use tokio::sync::Mutex;

use crate::{
    admin::*,
    message::*,
    protos::messages::v1::{app_message, AppMessage},
    *,
};
use mls_crypto::openmls_provider::MlsCryptoProvider;

#[derive(Clone, Debug)]
pub enum GroupAction {
    GroupAppMsg(AppMessage),
    LeaveGroup,
    DoNothing,
}

#[derive(Clone, Debug, Actor)]
pub struct Group {
    group_name: String,
    mls_group: Option<Arc<Mutex<MlsGroup>>>,
    admin: Option<GroupAdmin>,
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
            let group_config = MlsGroupCreateConfig::builder()
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
                admin: Some(GroupAdmin::new_admin()),
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
            .map(|m| hex::encode(m.credential.serialized_content()))
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
        let msg: KeyPackage = self.admin.as_ref().unwrap().decrypt_message(message)?;
        Ok(msg)
    }

    pub fn push_income_key_package(&mut self, key_package: KeyPackage) -> Result<(), GroupError> {
        if !self.is_admin() {
            return Err(GroupError::AdminNotSetError);
        }
        self.admin
            .as_mut()
            .unwrap()
            .add_incoming_key_package(key_package);
        Ok(())
    }

    pub fn processed_key_packages(&mut self) -> Result<Vec<KeyPackage>, GroupError> {
        if !self.is_admin() {
            return Err(GroupError::AdminNotSetError);
        }
        Ok(self.admin.as_mut().unwrap().drain_processed_key_packages())
    }

    pub async fn create_proposal_to_add_members(
        &mut self,
        key_package: KeyPackage,
        provider: &MlsCryptoProvider,
        signer: &SignatureKeyPair,
    ) -> Result<(), GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotInitializedError);
        }
        if !self.is_admin() {
            return Err(GroupError::AdminNotSetError);
        }
        let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;

        let (_, href) = mls_group.propose_add_member(provider, signer, &key_package)?;

        self.admin
            .as_mut()
            .unwrap()
            .add_pending_proposal(href.clone());

        println!(
            "nof proposals: {:?}",
            mls_group.pending_proposals().collect::<Vec<_>>().len()
        );
        println!(
            "Pending proposals: {:?}",
            mls_group.pending_proposals().collect::<Vec<_>>()
        );

        Ok(())
    }

    pub fn get_pending_proposals(&self) -> Result<Vec<ProposalRef>, GroupError> {
        if !self.is_admin() {
            return Err(GroupError::AdminNotSetError);
        }
        Ok(self.admin.as_ref().unwrap().get_pending_proposals())
    }

    pub fn drain_pending_proposals(&mut self) -> Result<Vec<ProposalRef>, GroupError> {
        if !self.is_admin() {
            return Err(GroupError::AdminNotSetError);
        }
        Ok(self.admin.as_mut().unwrap().drain_pending_proposals())
    }

    pub async fn add_members(
        &mut self,
        provider: &MlsCryptoProvider,
        signer: &SignatureKeyPair,
    ) -> Result<Vec<WakuMessageToSend>, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotInitializedError);
        }
        let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;
        let (out_messages, welcome, _group_info) =
            mls_group.commit_to_pending_proposals(provider, signer)?;

        mls_group.merge_pending_commit(provider)?;

        let msg_to_send_commit = WakuMessageToSend::new(
            out_messages.to_bytes()?,
            APP_MSG_SUBTOPIC,
            self.group_name.clone(),
            self.app_id.clone(),
        );

        if let Some(welcome) = welcome {
            let msg_to_send_welcome = WakuMessageToSend::new(
                wrap_invitation_into_welcome_msg(welcome)?.encode_to_vec(),
                WELCOME_SUBTOPIC,
                self.group_name.clone(),
                self.app_id.clone(),
            );
            return Ok(vec![msg_to_send_commit, msg_to_send_welcome]);
        }

        Ok(vec![msg_to_send_commit])
    }

    pub async fn remove_members(
        &mut self,
        users: Vec<String>,
        provider: &MlsCryptoProvider,
        signer: &SignatureKeyPair,
    ) -> Result<WakuMessageToSend, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotInitializedError);
        }
        let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;
        let mut leaf_indexs = Vec::new();
        let members = mls_group.members().collect::<Vec<_>>();
        for user in users {
            for m in members.iter() {
                if hex::encode(m.credential.serialized_content()) == user {
                    leaf_indexs.push(m.index);
                }
            }
        }
        // Remove operation on the mls group
        let (remove_message, _welcome, _group_info) =
            mls_group.remove_members(provider, signer, &leaf_indexs)?;

        // Second, process the removal on our end.
        mls_group.merge_pending_commit(provider)?;

        let msg_to_send_commit = WakuMessageToSend::new(
            remove_message.to_bytes()?,
            APP_MSG_SUBTOPIC,
            self.group_name.clone(),
            self.app_id.clone(),
        );

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

        println!(
            "[process_protocol_msg] processed_message: {:?}",
            processed_message
        );
        match processed_message.into_content() {
            ProcessedMessageContent::ApplicationMessage(application_message) => {
                let sender_name = {
                    let user_id = mls_group.members().find_map(|m| {
                        if m.credential.serialized_content()
                            == processed_message_credential.serialized_content()
                            && (signature_key != m.signature_key.as_slice())
                        {
                            Some(hex::encode(m.credential.serialized_content()))
                        } else {
                            None
                        }
                    });
                    if user_id.is_none() {
                        return Ok(GroupAction::DoNothing);
                    }
                    user_id.unwrap()
                };

                let app_msg_bytes = application_message.into_bytes();
                let app_msg_bytes_slice = app_msg_bytes.as_slice();

                println!(
                    "[process_protocol_msg] application_message: {:?}",
                    app_msg_bytes.clone()
                );
                let app_msg = AppMessage::decode(app_msg_bytes_slice)
                    .map_err(|e| GroupError::AppMessageDecodeError(e.to_string()))?;

                match app_msg.payload {
                    Some(app_message::Payload::ConversationMessage(conversation_message)) => {
                        let msg_to_send = wrap_conversation_message_into_application_msg(
                            conversation_message.message,
                            sender_name,
                            self.group_name.clone(),
                        );
                        return Ok(GroupAction::GroupAppMsg(msg_to_send));
                    }
                    Some(app_message::Payload::VoteStartMessage(vote_start_message)) => {
                        let msg_to_send = wrap_conversation_message_into_application_msg(
                            vote_start_message.group_name,
                            sender_name,
                            self.group_name.clone(),
                        );
                        return Ok(GroupAction::GroupAppMsg(msg_to_send));
                    }
                    _ => return Ok(GroupAction::DoNothing),
                }
            }
            ProcessedMessageContent::ProposalMessage(proposal_ptr) => {
                let _ = mls_group
                    .store_pending_proposal(provider.storage(), proposal_ptr.as_ref().clone());
            }
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
                    return Ok(GroupAction::LeaveGroup);
                }
            }
        };
        Ok(GroupAction::DoNothing)
    }

    pub fn generate_admin_message(&mut self) -> Result<WakuMessageToSend, GroupError> {
        let admin = match self.admin.as_mut() {
            Some(a) => a,
            None => return Err(GroupError::AdminNotSetError),
        };
        admin.refresh_key_pair();

        let msg_to_send = WakuMessageToSend::new(
            wrap_group_announcement_in_welcome_msg(admin.create_admin_announcement())
                .encode_to_vec(),
            WELCOME_SUBTOPIC,
            self.group_name.clone(),
            self.app_id.clone(),
        );
        Ok(msg_to_send)
    }

    pub async fn build_message(
        &mut self,
        provider: &MlsCryptoProvider,
        signer: &SignatureKeyPair,
        msg: &AppMessage,
    ) -> Result<WakuMessageToSend, GroupError> {
        println!("Building message: {:?}", msg);
        let message_out = self
            .mls_group
            .as_mut()
            .unwrap()
            .lock()
            .await
            .create_message(provider, signer, &msg.encode_to_vec())?
            .to_bytes()?;
        println!("Message out: {:?}", message_out);
        Ok(WakuMessageToSend::new(
            message_out,
            APP_MSG_SUBTOPIC,
            self.group_name.clone(),
            self.app_id.clone(),
        ))
    }
}

impl Display for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Group: {:#?}", self.group_name)
    }
}

#[cfg(test)]
mod tests {
    use mls_crypto::identity::random_identity;

    use crate::user::User;
    use log::info;

    use super::*;

    #[tokio::test]
    async fn test_create_proposal_to_add_members() {
        let group_name = "new_group".to_string();

        let crypto = MlsCryptoProvider::default();
        let id_admin = random_identity().expect("Failed to create identity");
        let mut id_user = random_identity().expect("Failed to create identity");

        let mut group_admin = Group::new(
            group_name.clone(),
            true,
            Some(&crypto),
            Some(&id_admin.signer()),
            Some(&id_admin.credential_with_key()),
        )
        .expect("Failed to create group");

        let group_user = Group::new(group_name.clone(), false, None, None, None)
            .expect("Failed to create group");

        let kp_user = id_user
            .generate_key_package(&crypto)
            .expect("Failed to generate key package");

        group_admin
            .create_proposal_to_add_members(kp_user, &crypto, &id_admin.signer())
            .await
            .expect("Failed to create proposal");

        let pending_proposals = group_admin
            .get_pending_proposals()
            .expect("Failed to get pending proposals");
        assert_eq!(pending_proposals.len(), 1);

        let out = group_admin
            .add_members(&crypto, &id_admin.signer())
            .await
            .expect("Failed to add members");

        info!("Out: {:?}", out);

        let waku_msg_with_welcome = out[1]
            .build_waku_message()
            .expect("Failed to build waku message");
        info!("Waku message: {:?}", waku_msg_with_welcome);

        // group_user.join_group(waku_msg_with_welcome).await.expect("Failed to join group");
    }
}
