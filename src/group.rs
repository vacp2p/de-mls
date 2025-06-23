use alloy::hex;
use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use kameo::Actor;
// use log::info;
use openmls::{
    group::{GroupEpoch, GroupId, MlsGroup, MlsGroupCreateConfig},
    prelude::{
        hash_ref::ProposalRef, Credential, CredentialWithKey, KeyPackage, LeafNodeIndex,
        OpenMlsProvider, ProcessedMessageContent, ProtocolMessage,
    },
};
use openmls_basic_credential::SignatureKeyPair;
use prost::Message;
use std::{fmt::Display, sync::Arc};
use tokio::sync::Mutex;

use crate::{
    message::{
        wrap_conversation_message_into_application_msg, wrap_group_announcement_in_welcome_msg,
        wrap_invitation_into_welcome_msg,
    },
    protos::messages::v1::{app_message, AppMessage},
    steward::{GroupUpdateRequest, Steward},
    *,
};
use mls_crypto::openmls_provider::MlsProvider;

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
    steward: Option<Steward>,
    is_kp_shared: bool,
    app_id: Vec<u8>,
}

impl Group {
    pub fn new(
        group_name: String,
        is_creation: bool,
        provider: Option<&MlsProvider>,
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
                steward: Some(Steward::new()),
                is_kp_shared: true,
                app_id: uuid.clone(),
            })
        } else {
            Ok(Group {
                group_name,
                mls_group: None,
                steward: None,
                is_kp_shared: false,
                app_id: uuid.clone(),
            })
        }
    }

    pub async fn members_identity(&self) -> Result<Vec<String>, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
        }
        let mls_group = self.mls_group.as_ref().unwrap().lock().await;
        let x = mls_group
            .members()
            .map(|m| hex::encode(m.credential.serialized_content()))
            .collect();
        Ok(x)
    }

    pub async fn find_member_index(
        &self,
        identity: Vec<u8>,
    ) -> Result<Option<LeafNodeIndex>, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
        }
        let mls_group = self.mls_group.as_ref().unwrap().lock().await;
        let x = mls_group.members().find_map(|m| {
            if m.credential.serialized_content() == identity {
                Some(m.index)
            } else {
                None
            }
        });
        Ok(x)
    }

    pub async fn epoch(&self) -> Result<GroupEpoch, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
        }
        let mls_group = self.mls_group.as_ref().unwrap().lock().await;
        Ok(mls_group.epoch())
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

    pub fn is_steward(&self) -> bool {
        self.steward.is_some()
    }

    pub fn app_id(&self) -> Vec<u8> {
        self.app_id.clone()
    }

    pub fn decrypt_steward_msg(&self, message: Vec<u8>) -> Result<KeyPackage, GroupError> {
        if !self.is_steward() {
            return Err(GroupError::StewardNotSet);
        }
        let msg: KeyPackage = self.steward.as_ref().unwrap().decrypt_message(message)?;
        Ok(msg)
    }

    pub fn store_invite_proposal(&mut self, key_package: KeyPackage) -> Result<(), GroupError> {
        if !self.is_steward() {
            return Err(GroupError::StewardNotSet);
        }
        self.steward
            .as_mut()
            .unwrap()
            .put_invite_proposal(key_package);
        Ok(())
    }

    pub fn drain_proposals(&mut self) -> Result<Vec<GroupUpdateRequest>, GroupError> {
        if !self.is_steward() {
            return Err(GroupError::StewardNotSet);
        }
        Ok(self.steward.as_mut().unwrap().drain_proposals())
    }

    pub async fn create_proposal_to_add_member(
        &mut self,
        provider: &MlsProvider,
        signer: &SignatureKeyPair,
        key_package: KeyPackage,
    ) -> Result<(WakuMessageToSend, ProposalRef), GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
        }
        if !self.is_steward() {
            return Err(GroupError::StewardNotSet);
        }
        let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;

        let (mls_message_out, proposal_ref) =
            mls_group.propose_add_member(provider, signer, &key_package)?;

        println!(
            "nof proposals: {:?}",
            mls_group.pending_proposals().collect::<Vec<_>>().len()
        );
        let wmts = WakuMessageToSend::new(
            mls_message_out.to_bytes()?,
            APP_MSG_SUBTOPIC,
            self.group_name.clone(),
            self.app_id.clone(),
        );
        Ok((wmts, proposal_ref))
    }

    pub async fn create_proposal_to_remove_member(
        &mut self,
        provider: &MlsProvider,
        signer: &SignatureKeyPair,
        identity: Vec<u8>,
    ) -> Result<(WakuMessageToSend, ProposalRef), GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
        }
        let member_index = self.find_member_index(identity).await?;
        if member_index.is_some() {
            let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;
            let (mls_message_out, proposal_ref) =
                mls_group.propose_remove_member(provider, signer, member_index.unwrap())?;
            let wmts = WakuMessageToSend::new(
                mls_message_out.to_bytes()?,
                APP_MSG_SUBTOPIC,
                self.group_name.clone(),
                self.app_id.clone(),
            );
            Ok((wmts, proposal_ref))
        } else {
            return Err(GroupError::MemberNotFound);
        }
    }

    // pub fn get_pending_proposals(&self) -> Result<Vec<ProposalRef>, GroupError> {
    //     if !self.is_steward() {
    //         return Err(GroupError::StewardNotSet);
    //     }
    //     Ok(self.steward.as_ref().unwrap().get_pending_proposals())
    // }

    // pub fn drain_pending_proposals(&mut self) -> Result<Vec<ProposalRef>, GroupError> {
    //     if !self.is_steward() {
    //         return Err(GroupError::StewardNotSet);
    //     }
    //     Ok(self.steward.as_mut().unwrap().drain_pending_proposals())
    // }

    pub async fn apply_proposals(
        &mut self,
        provider: &MlsProvider,
        signer: &SignatureKeyPair,
    ) -> Result<Vec<WakuMessageToSend>, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
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
        } else {
            return Err(GroupError::EmptyWelcomeMessage);
        }
    }

    pub async fn remove_members(
        &mut self,
        users: Vec<String>,
        provider: &MlsProvider,
        signer: &SignatureKeyPair,
    ) -> Result<WakuMessageToSend, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
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

        // let app_msg =
        //     wrap_mls_out_message_into_application_msg(remove_message, sender, self.group_name.clone());
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
        provider: &MlsProvider,
        signature_key: Vec<u8>,
    ) -> Result<GroupAction, GroupError> {
        let group_id = message.group_id().as_slice().to_vec();
        if group_id != self.group_name.as_bytes().to_vec() {
            return Ok(GroupAction::DoNothing);
        }
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
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
                        return Err(GroupError::GroupStillActive);
                    }
                    return Ok(GroupAction::LeaveGroup);
                }
            }
        };
        Ok(GroupAction::DoNothing)
    }

    pub fn generate_steward_message(&mut self) -> Result<WakuMessageToSend, GroupError> {
        let steward = match self.steward.as_mut() {
            Some(a) => a,
            None => return Err(GroupError::StewardNotSet),
        };
        steward.refresh_key_pair();

        let msg_to_send = WakuMessageToSend::new(
            wrap_group_announcement_in_welcome_msg(steward.create_announcement()).encode_to_vec(),
            WELCOME_SUBTOPIC,
            self.group_name.clone(),
            self.app_id.clone(),
        );
        Ok(msg_to_send)
    }

    pub async fn build_message(
        &mut self,
        provider: &MlsProvider,
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

    use log::info;

    use crate::steward::GroupUpdateRequest;

    use super::Group;
    use mls_crypto::openmls_provider::MlsProvider;

    #[tokio::test]
    async fn test_create_proposal_to_add_members() {
        let group_name = "new_group".to_string();

        let crypto = MlsProvider::default();
        let id_steward = random_identity().expect("Failed to create identity");
        let mut id_user = random_identity().expect("Failed to create identity");

        let mut group_steward = Group::new(
            group_name.clone(),
            true,
            Some(&crypto),
            Some(&id_steward.signer()),
            Some(&id_steward.credential_with_key()),
        )
        .expect("Failed to create group");

        let _ = Group::new(group_name.clone(), false, None, None, None)
            .expect("Failed to create group");

        let kp_user = id_user
            .generate_key_package(&crypto)
            .expect("Failed to generate key package");

        group_steward
            .store_invite_proposal(kp_user.clone())
            .expect("Failed to create proposal");

        let pending_proposals = group_steward
            .drain_proposals()
            .expect("Failed to get pending proposals");
        assert_eq!(pending_proposals.len(), 1);
        assert_eq!(
            pending_proposals[0],
            GroupUpdateRequest::AddMember(kp_user.clone())
        );

        let out = group_steward
            .apply_proposals(&crypto, &id_steward.signer())
            .await
            .expect("Failed to apply proposals");

        info!("Out: {:?}", out);

        let waku_msg_with_welcome = out[1]
            .build_waku_message()
            .expect("Failed to build waku message");
        info!("Waku message: {:?}", waku_msg_with_welcome);

        // group_user.join_group(waku_msg_with_welcome).await.expect("Failed to join group");
    }
}
