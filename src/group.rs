use alloy::hex;
use kameo::Actor;
use log::{error, info};
use openmls::{
    group::{GroupEpoch, GroupId, MlsGroup, MlsGroupCreateConfig},
    prelude::{
        CredentialWithKey, KeyPackage, LeafNodeIndex, OpenMlsProvider, ProcessedMessageContent,
        ProtocolMessage,
    },
};
use openmls_basic_credential::SignatureKeyPair;
use prost::Message;
use std::{fmt::Display, sync::Arc};
use tokio::sync::{Mutex, RwLock};
use uuid;

use crate::{
    consensus::v1::{Proposal, Vote},
    error::GroupError,
    message::{message_types, MessageType},
    protos::messages::v1::{app_message, AppMessage, BatchProposalsMessage, WelcomeMessage},
    state_machine::{GroupState, GroupStateMachine},
    steward::GroupUpdateRequest,
};
use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::openmls_provider::MlsProvider;

#[derive(Clone, Debug)]
pub enum GroupAction {
    GroupAppMsg(AppMessage),
    GroupProposal(Proposal),
    GroupVote(Vote),
    LeaveGroup,
    DoNothing,
}

impl Display for GroupAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupAction::GroupAppMsg(_) => write!(f, "Message will be printed to the app"),
            GroupAction::GroupProposal(_) => write!(f, "Get proposal for voting"),
            GroupAction::GroupVote(_) => write!(f, "Get vote for proposal"),
            GroupAction::LeaveGroup => write!(f, "User will leave the group"),
            GroupAction::DoNothing => write!(f, "Do Nothing"),
        }
    }
}

#[derive(Clone, Debug, Actor)]
pub struct Group {
    group_name: String,
    mls_group: Option<Arc<Mutex<MlsGroup>>>,
    is_kp_shared: bool,
    app_id: Vec<u8>,
    state_machine: Arc<RwLock<GroupStateMachine>>,
}

impl Group {
    pub fn new(
        group_name: &str,
        is_creation: bool,
        provider: Option<&MlsProvider>,
        signer: Option<&SignatureKeyPair>,
        credential_with_key: Option<&CredentialWithKey>,
    ) -> Result<Self, GroupError> {
        let uuid = uuid::Uuid::new_v4().as_bytes().to_vec();
        let mut group = Group {
            group_name: group_name.to_string(),
            mls_group: None,
            is_kp_shared: false,
            app_id: uuid.clone(),
            state_machine: if is_creation {
                Arc::new(RwLock::new(GroupStateMachine::new_with_steward()))
            } else {
                Arc::new(RwLock::new(GroupStateMachine::new()))
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

    pub async fn is_steward(&self) -> bool {
        let state_machine = self.state_machine.read().await;
        state_machine.has_steward()
    }

    pub fn app_id(&self) -> Vec<u8> {
        self.app_id.clone()
    }

    pub fn group_name_bytes(&self) -> Vec<u8> {
        self.group_name.as_bytes().to_vec()
    }

    pub async fn generate_steward_message(&mut self) -> Result<WakuMessageToSend, GroupError> {
        let mut state_machine = self.state_machine.write().await;
        let steward = state_machine
            .get_steward_mut()
            .ok_or(GroupError::StewardNotSet)?;
        steward.refresh_key_pair().await;

        let welcome_msg: WelcomeMessage = steward.create_announcement().await.into();
        let msg_to_send = WakuMessageToSend::new(
            welcome_msg.encode_to_vec(),
            WELCOME_SUBTOPIC,
            &self.group_name,
            self.app_id(),
        );
        Ok(msg_to_send)
    }

    pub async fn decrypt_steward_msg(
        &mut self,
        message: Vec<u8>,
    ) -> Result<KeyPackage, GroupError> {
        let state_machine = self.state_machine.read().await;
        let steward = state_machine
            .get_steward()
            .ok_or(GroupError::StewardNotSet)?;
        let msg: KeyPackage = steward.decrypt_message(message).await?;
        Ok(msg)
    }

    // Functions to store proposals in steward queue

    pub async fn store_invite_proposal(
        &mut self,
        key_package: Box<KeyPackage>,
    ) -> Result<(), GroupError> {
        let mut state_machine = self.state_machine.write().await;
        state_machine
            .add_proposal(GroupUpdateRequest::AddMember(key_package))
            .await;
        Ok(())
    }

    pub async fn store_remove_proposal(&mut self, identity: String) -> Result<(), GroupError> {
        let mut state_machine = self.state_machine.write().await;
        state_machine
            .add_proposal(GroupUpdateRequest::RemoveMember(identity))
            .await;
        Ok(())
    }

    // Functions to process protocol messages
    pub async fn process_protocol_msg(
        &mut self,
        message: ProtocolMessage,
        provider: &MlsProvider,
    ) -> Result<GroupAction, GroupError> {
        let group_id = message.group_id().as_slice().to_vec();
        if group_id != self.group_name_bytes() {
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

        match processed_message.into_content() {
            ProcessedMessageContent::ApplicationMessage(application_message) => {
                info!("[group::process_protocol_msg]: Processing application message");
                drop(mls_group);
                let app_msg = AppMessage::decode(application_message.into_bytes().as_slice())?;
                match app_msg.payload {
                    Some(app_message::Payload::ConversationMessage(conversation_message)) => {
                        info!("[group::process_protocol_msg]: Processing conversation message");
                        return Ok(GroupAction::GroupAppMsg(conversation_message.into()));
                    }
                    Some(app_message::Payload::Proposal(proposal)) => {
                        info!("[group::process_protocol_msg]: Processing proposal message");
                        return Ok(GroupAction::GroupProposal(proposal));
                    }
                    Some(app_message::Payload::Vote(vote)) => {
                        info!("[group::process_protocol_msg]: Processing vote message");
                        return Ok(GroupAction::GroupVote(vote));
                    }
                    Some(app_message::Payload::BanRequest(ban_request)) => {
                        info!("[group::process_protocol_msg]: Processing ban request message");

                        if self.is_steward().await {
                            info!(
                                "[group::process_protocol_msg]: Steward adding remove proposal for user {}",
                                ban_request.user_to_ban.clone()
                            );
                            self.store_remove_proposal(ban_request.user_to_ban.clone())
                                .await?;
                        } else {
                            info!(
                                "[group::process_protocol_msg]: Non-steward received ban request message"
                            );
                        }

                        return Ok(GroupAction::GroupAppMsg(ban_request.into()));
                    }
                    _ => return Ok(GroupAction::DoNothing),
                }
            }
            ProcessedMessageContent::ProposalMessage(proposal_ptr) => {
                mls_group
                    .store_pending_proposal(provider.storage(), proposal_ptr.as_ref().clone())?;
            }
            ProcessedMessageContent::ExternalJoinProposalMessage(_external_proposal_ptr) => (),
            ProcessedMessageContent::StagedCommitMessage(commit_ptr) => {
                let mut remove_proposal: bool = false;
                if commit_ptr.self_removed() {
                    remove_proposal = true;
                }
                mls_group.merge_staged_commit(provider, *commit_ptr)?;
                if remove_proposal {
                    if mls_group.is_active() {
                        return Err(GroupError::GroupStillActive);
                    }
                    return Ok(GroupAction::LeaveGroup);
                }
            }
        };
        Ok(GroupAction::DoNothing)
    }

    pub async fn build_message(
        &mut self,
        provider: &MlsProvider,
        signer: &SignatureKeyPair,
        msg: &AppMessage,
    ) -> Result<WakuMessageToSend, GroupError> {
        let is_steward = self.is_steward().await;
        let has_proposals = self.get_pending_proposals_count().await > 0;

        // Determine message type for state checking using trait
        let message_type = msg
            .payload
            .as_ref()
            .map(|p| p.message_type())
            .unwrap_or(message_types::UNKNOWN);

        // Check if message can be sent in current state
        let state_machine = self.state_machine.read().await;
        let current_state = state_machine.current_state();
        if !state_machine.can_send_message_type(is_steward, has_proposals, message_type) {
            info!(
                "[group::build_message]: Unable to send message - Current state: {}, Message type: {}, Is steward: {}, Has proposals: {}",
                current_state, message_type, is_steward, has_proposals
            );
            return Err(GroupError::UnableToSendMessage);
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
            &self.group_name,
            self.app_id(),
        ))
    }

    // State management methods
    pub async fn get_state(&self) -> GroupState {
        self.state_machine.read().await.current_state()
    }

    /// Get the number of pending proposals for the current epoch
    pub async fn get_pending_proposals_count(&self) -> usize {
        self.state_machine
            .read()
            .await
            .get_current_epoch_proposals_count()
            .await
    }

    /// Get the number of pending proposals for the voting epoch
    pub async fn get_voting_proposals_count(&self) -> usize {
        self.state_machine
            .read()
            .await
            .get_voting_epoch_proposals_count()
            .await
    }

    /// Get the proposals for the voting epoch
    pub async fn get_proposals_for_voting_epoch(&self) -> Vec<GroupUpdateRequest> {
        self.state_machine
            .read()
            .await
            .get_voting_epoch_proposals()
            .await
    }

    /// Start voting on proposals for the current epoch
    pub async fn start_voting(&mut self) -> Result<(), GroupError> {
        self.state_machine.write().await.start_voting()
    }

    /// Complete voting and update state based on result
    pub async fn complete_voting(&mut self, vote_result: bool) -> Result<(), GroupError> {
        self.state_machine
            .write()
            .await
            .complete_voting(vote_result)
    }

    /// Start working state (for non-steward peers after consensus or edge case recovery)
    pub async fn start_working(&mut self) {
        self.state_machine.write().await.start_working();
    }

    /// Start consensus reached state (for non-steward peers after consensus)
    pub async fn start_consensus_reached(&mut self) {
        self.state_machine.write().await.start_consensus_reached();
    }

    /// Recover from consensus failure by transitioning back to Working state
    pub async fn recover_from_consensus_failure(&mut self) -> Result<(), GroupError> {
        self.state_machine
            .write()
            .await
            .recover_from_consensus_failure()
    }

    /// Start waiting state (for non-steward peers after consensus or edge case recovery)
    pub async fn start_waiting(&mut self) {
        self.state_machine.write().await.start_waiting();
    }

    /// Start steward epoch with validation using centralized logic
    pub async fn start_steward_epoch_with_validation(&mut self) -> Result<usize, GroupError> {
        self.state_machine
            .write()
            .await
            .start_steward_epoch_with_validation()
            .await
    }

    /// Apply proposals and complete using centralized logic
    pub async fn handle_yes_vote(&mut self) -> Result<(), GroupError> {
        self.state_machine.write().await.handle_yes_vote().await
    }

    /// Handle failed vote using centralized logic
    pub async fn handle_no_vote(&mut self) -> Result<(), GroupError> {
        self.state_machine.write().await.handle_no_vote().await
    }

    /// Start waiting state when steward sends batch proposals after consensus
    pub async fn start_waiting_after_consensus(&mut self) -> Result<(), GroupError> {
        self.state_machine
            .write()
            .await
            .start_waiting_after_consensus()
    }

    /// Create a batch proposals message and welcome message for the current epoch.
    /// Returns [batch_proposals_msg, welcome_msg] where welcome_msg is only included if there are new members.
    pub async fn create_batch_proposals_message(
        &mut self,
        provider: &MlsProvider,
        signer: &SignatureKeyPair,
    ) -> Result<Vec<WakuMessageToSend>, GroupError> {
        if !self.is_steward().await {
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
                // Convert the address string to bytes for proper MLS credential matching
                let identity_bytes = if let Some(hex_string) = identity.strip_prefix("0x") {
                    // Remove 0x prefix and convert to bytes
                    hex::decode(hex_string).map_err(|e| {
                        error!("[create_batch_proposals_message]: Failed to decode hex address '{}': {}", identity, e);
                        GroupError::UnableToSendMessage
                    })?
                } else {
                    // Assume it's already a hex string without 0x prefix
                    hex::decode(identity).map_err(|e| {
                        error!("[create_batch_proposals_message]: Failed to decode hex address '{}': {}", identity, e);
                        GroupError::UnableToSendMessage
                    })?
                };

                let member_index = self.find_member_index(identity_bytes).await?;
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
                GroupUpdateRequest::RemoveMember(identity) => {
                    if let Some(index) = member_indices[i] {
                        let (mls_message_out, _proposal_ref) =
                            mls_group.propose_remove_member(provider, signer, index)?;
                        mls_proposals.push(mls_message_out.to_bytes()?);
                    } else {
                        error!("[create_batch_proposals_message]: Failed to find member index for identity: {:?}", identity);
                    }
                }
            }
        }

        // Create commit with all proposals
        let (out_messages, welcome, _group_info) =
            mls_group.commit_to_pending_proposals(provider, signer)?;

        // Merge the commit
        mls_group.merge_pending_commit(provider)?;

        drop(mls_group);

        // Create batch proposals message (without welcome)
        let batch_msg: AppMessage = BatchProposalsMessage {
            group_name: self.group_name_bytes(),
            mls_proposals,
            commit_message: out_messages.to_bytes()?,
        }
        .into();

        let batch_waku_msg = WakuMessageToSend::new(
            batch_msg.encode_to_vec(),
            APP_MSG_SUBTOPIC,
            &self.group_name,
            self.app_id(),
        );

        let mut messages = vec![batch_waku_msg];

        // Create separate welcome message if there are new members
        if let Some(welcome) = welcome {
            let welcome_msg: WelcomeMessage = welcome.try_into()?;
            let welcome_waku_msg = WakuMessageToSend::new(
                welcome_msg.encode_to_vec(),
                WELCOME_SUBTOPIC,
                &self.group_name,
                self.app_id(),
            );
            messages.push(welcome_waku_msg);
        }

        Ok(messages)
    }
}

impl Display for Group {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Group: {:#?}", self.group_name)
    }
}
