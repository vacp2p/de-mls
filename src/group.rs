use alloy::hex;
use kameo::Actor;
use log::info;
use openmls::{
    group::{GroupEpoch, GroupId, MlsGroup, MlsGroupCreateConfig},
    prelude::{
        Credential, CredentialWithKey, KeyPackage, LeafNodeIndex, OpenMlsProvider,
        ProcessedMessageContent, ProtocolMessage,
    },
};
use openmls_basic_credential::SignatureKeyPair;
use prost::Message;
use std::{fmt::Display, sync::Arc};
use tokio::sync::Mutex;
use uuid;

use crate::{
    error::GroupError,
    message::{
        wrap_batch_proposals_into_application_msg, wrap_conversation_message_into_application_msg,
        wrap_group_announcement_in_welcome_msg, wrap_invitation_into_welcome_msg,
    },
    protos::messages::v1::{app_message, AppMessage},
    state_machine::{GroupState, GroupStateMachine},
    steward::GroupUpdateRequest,
};
use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
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
    is_kp_shared: bool,
    app_id: Vec<u8>,
    state_machine: GroupStateMachine,
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
        let mut group = Group {
            group_name: group_name.clone(),
            mls_group: None,
            is_kp_shared: false,
            app_id: uuid.clone(),
            state_machine: if is_creation {
                GroupStateMachine::new_with_steward()
            } else {
                GroupStateMachine::new()
            },
        };

        if is_creation {
            if let (Some(provider), Some(signer), Some(credential_with_key)) =
                (provider, signer, credential_with_key)
            {
                // Create a new MLS group instance
                let group_config = MlsGroupCreateConfig::builder()
                    .use_ratchet_tree_extension(true)
                    .build();
                let mls_group = MlsGroup::new_with_group_id(
                    provider,
                    signer,
                    &group_config,
                    GroupId::from_slice(group_name.as_bytes()),
                    credential_with_key.clone(),
                )?;
                group.mls_group = Some(Arc::new(Mutex::new(mls_group)));
                group.is_kp_shared = true;
            }
        }

        Ok(group)
    }

    pub async fn members_identity(&self) -> Result<Vec<Vec<u8>>, GroupError> {
        if !self.is_mls_group_initialized() {
            return Err(GroupError::MlsGroupNotSet);
        }
        let mls_group = self.mls_group.as_ref().unwrap().lock().await;
        let x = mls_group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
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
        self.state_machine.has_steward()
    }

    pub fn app_id(&self) -> Vec<u8> {
        self.app_id.clone()
    }

    pub fn decrypt_steward_msg(&self, message: Vec<u8>) -> Result<KeyPackage, GroupError> {
        if !self.is_steward() {
            return Err(GroupError::StewardNotSet);
        }
        let steward = self.state_machine.get_steward().unwrap();
        let msg: KeyPackage = steward.decrypt_message(message)?;
        Ok(msg)
    }

    // Functions to store proposals in steward queue

    pub async fn store_invite_proposal(
        &mut self,
        key_package: Box<KeyPackage>,
    ) -> Result<(), GroupError> {
        self.state_machine
            .add_proposal(GroupUpdateRequest::AddMember(key_package))
            .await;
        Ok(())
    }

    pub async fn store_remove_proposal(&mut self, identity: Vec<u8>) -> Result<(), GroupError> {
        self.state_machine
            .add_proposal(GroupUpdateRequest::RemoveMember(identity))
            .await;
        Ok(())
    }

    // Fucntions to process protocol messages

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
                let res = mls_group
                    .store_pending_proposal(provider.storage(), proposal_ptr.as_ref().clone());
                if res.is_err() {
                    return Err(GroupError::UnableToStorePendingProposal(res.err().unwrap()));
                }
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
        if !self.is_steward() {
            return Err(GroupError::StewardNotSet);
        }
        let steward = self.state_machine.get_steward_mut().unwrap();
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
        // Check if message can be sent in current state
        let is_steward = self.is_steward();
        let has_proposals = self.get_pending_proposals_count().await > 0;

        if !self.can_send_message(is_steward, has_proposals) {
            return Err(GroupError::InvalidStateTransition);
        }
        let message_out = self
            .mls_group
            .as_mut()
            .unwrap()
            .lock()
            .await
            .create_message(provider, signer, &msg.encode_to_vec())?
            .to_bytes()?;
        Ok(WakuMessageToSend::new(
            message_out,
            APP_MSG_SUBTOPIC,
            self.group_name.clone(),
            self.app_id.clone(),
        ))
    }

    // State management methods
    pub fn get_state(&self) -> GroupState {
        self.state_machine.current_state()
    }

    pub fn can_send_message(&self, is_steward: bool, has_proposals: bool) -> bool {
        self.state_machine
            .can_send_message(is_steward, has_proposals)
    }

    /// Get the number of pending proposals for the current epoch
    pub async fn get_pending_proposals_count(&self) -> usize {
        let count = self.state_machine.get_current_epoch_proposals_count().await;
        info!("State machine reports {} current epoch proposals", count);
        count
    }

    /// Get the number of pending proposals for the voting epoch
    pub async fn get_voting_proposals_count(&self) -> usize {
        let count = self.state_machine.get_voting_epoch_proposals_count().await;
        info!("State machine reports {} voting proposals", count);
        count
    }

    /// Get the proposals for the voting epoch
    pub async fn get_proposals_for_voting_epoch(&self) -> Vec<GroupUpdateRequest> {
        self.state_machine.get_voting_epoch_proposals().await
    }

    /// Start a new steward epoch, moving proposals from the previous epoch to the voting epoch
    /// and transitioning to Waiting state.
    pub async fn start_steward_epoch(&mut self) -> Result<(), GroupError> {
        if !self.is_steward() {
            return Err(GroupError::StewardNotSet);
        }

        // Start new epoch and move proposals from current epoch to voting epoch
        self.state_machine.start_steward_epoch().await?;
        Ok(())
    }

    /// Start voting on proposals for the current epoch, transitioning to Voting state.
    pub fn start_voting(&mut self) -> Result<(), GroupError> {
        self.state_machine.start_voting()
    }

    /// Complete voting, updating group state based on the result.
    pub fn complete_voting(&mut self, vote_result: bool) -> Result<(), GroupError> {
        self.state_machine.complete_voting(vote_result)
    }

    /// Create a batch proposals message and welcome message for the current epoch.
    /// Returns [batch_proposals_msg, welcome_msg] where welcome_msg is only included if there are new members.
    pub async fn create_batch_proposals_message(
        &mut self,
        provider: &MlsProvider,
        signer: &SignatureKeyPair,
    ) -> Result<Vec<WakuMessageToSend>, GroupError> {
        if !self.is_steward() {
            return Err(GroupError::StewardNotSet);
        }

        let proposals = self.get_proposals_for_voting_epoch().await;

        if proposals.is_empty() {
            return Err(GroupError::EmptyProposals);
        }

        // Pre-collect member indices to avoid borrow checker issues
        let mut member_indices = Vec::new();
        for proposal in &proposals {
            if let GroupUpdateRequest::RemoveMember(identity) = proposal {
                let member_index = self.find_member_index(identity.clone()).await?;
                member_indices.push(member_index);
            } else {
                member_indices.push(None);
            }
        }

        let mut mls_proposals = Vec::new();
        let mut mls_group = self.mls_group.as_mut().unwrap().lock().await;

        // Convert each GroupUpdateRequest to MLS proposal
        for (i, proposal) in proposals.iter().enumerate() {
            match proposal {
                GroupUpdateRequest::AddMember(boxed_key_package) => {
                    let (mls_message_out, _proposal_ref) = mls_group.propose_add_member(
                        provider,
                        signer,
                        boxed_key_package.as_ref(),
                    )?;
                    mls_proposals.push(mls_message_out.to_bytes()?);
                }
                GroupUpdateRequest::RemoveMember(_identity) => {
                    if let Some(index) = member_indices[i] {
                        let (mls_message_out, _proposal_ref) =
                            mls_group.propose_remove_member(provider, signer, index)?;
                        mls_proposals.push(mls_message_out.to_bytes()?);
                    }
                }
            }
        }

        // Create commit with all proposals
        let (out_messages, welcome, _group_info) =
            mls_group.commit_to_pending_proposals(provider, signer)?;

        // Merge the commit
        mls_group.merge_pending_commit(provider)?;

        // Create batch proposals message (without welcome)
        let batch_msg = wrap_batch_proposals_into_application_msg(
            self.group_name.clone(),
            mls_proposals,
            out_messages.to_bytes()?,
        );

        let batch_waku_msg = WakuMessageToSend::new(
            batch_msg.encode_to_vec(),
            APP_MSG_SUBTOPIC,
            self.group_name.clone(),
            self.app_id.clone(),
        );

        let mut messages = vec![batch_waku_msg];

        // Create separate welcome message if there are new members
        if let Some(welcome) = welcome {
            let welcome_msg = wrap_invitation_into_welcome_msg(welcome)?;

            let welcome_waku_msg = WakuMessageToSend::new(
                welcome_msg.encode_to_vec(),
                WELCOME_SUBTOPIC,
                self.group_name.clone(),
                self.app_id.clone(),
            );

            messages.push(welcome_waku_msg);
        }

        Ok(messages)
    }

    pub async fn remove_proposals_and_complete(&mut self) -> Result<(), GroupError> {
        self.state_machine.remove_proposals_and_complete().await?;
        Ok(())
    }
}

impl Display for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Group: {:#?}", self.group_name)
    }
}
