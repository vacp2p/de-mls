use alloy::hex;
use openmls::{
    group::{GroupEpoch, GroupId, MlsGroup, MlsGroupCreateConfig},
    prelude::{
        ApplicationMessage, CredentialWithKey, KeyPackage, LeafNodeIndex, OpenMlsProvider,
        ProcessedMessageContent, ProtocolMessage,
    },
};
use openmls_basic_credential::SignatureKeyPair;
use prost::Message;
use std::{fmt::Display, sync::Arc};
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info};
use uuid;

use crate::{
    error::GroupError,
    message::{message_types, MessageType},
    protos::{
        consensus::v1::{Proposal, RequestType, UpdateRequest, Vote},
        de_mls::messages::v1::{app_message, AppMessage, BatchProposalsMessage, WelcomeMessage},
    },
    state_machine::{GroupState, GroupStateMachine},
    steward::GroupUpdateRequest,
};
use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::{identity::normalize_wallet_address_str, openmls_provider::MlsProvider};

/// Represents the action to take after processing a group message or event.
///
/// This enum defines the possible outcomes when processing group-related operations,
/// allowing the caller to determine the appropriate next steps.
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

/// Represents a group in the MLS-based messaging system.
///
/// The Group struct manages the lifecycle of an MLS group, including member management,
/// proposal handling, and state transitions. It integrates with the state machine
/// to enforce proper group operations and steward epoch management.
///
/// ## Key Features:
/// - MLS group management and message processing
/// - Steward epoch coordination and proposal handling
/// - State machine integration for proper workflow enforcement
/// - Member addition/removal through proposals
/// - Message validation and permission checking
#[derive(Clone, Debug)]
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

    /// Get the identities of all current group members.
    ///
    /// ## Returns:
    /// - Vector of member identity bytes
    ///
    /// ## Errors:
    /// - `GroupError::MlsGroupNotSet` if MLS group is not initialized
    pub async fn members_identity(&self) -> Result<Vec<Vec<u8>>, GroupError> {
        let mls_group = self
            .mls_group
            .as_ref()
            .ok_or_else(|| GroupError::MlsGroupNotSet)?
            .lock()
            .await;
        let x = mls_group
            .members()
            .map(|m| m.credential.serialized_content().to_vec())
            .collect();
        Ok(x)
    }

    /// Find the leaf node index of a member by their identity.
    ///
    /// ## Parameters:
    /// - `identity`: The member's identity bytes
    ///
    /// ## Returns:
    /// - `Some(LeafNodeIndex)` if member is found, `None` otherwise
    ///
    /// ## Errors:
    /// - `GroupError::MlsGroupNotSet` if MLS group is not initialized
    pub async fn find_member_index(
        &self,
        identity: Vec<u8>,
    ) -> Result<Option<LeafNodeIndex>, GroupError> {
        let mls_group = self
            .mls_group
            .as_ref()
            .ok_or_else(|| GroupError::MlsGroupNotSet)?
            .lock()
            .await;
        let x = mls_group.members().find_map(|m| {
            if m.credential.serialized_content() == identity {
                Some(m.index)
            } else {
                None
            }
        });
        Ok(x)
    }

    /// Get the current epoch of the MLS group.
    ///
    /// ## Returns:
    /// - Current group epoch
    ///
    /// ## Errors:
    /// - `GroupError::MlsGroupNotSet` if MLS group is not initialized
    pub async fn epoch(&self) -> Result<GroupEpoch, GroupError> {
        let mls_group = self
            .mls_group
            .as_ref()
            .ok_or_else(|| GroupError::MlsGroupNotSet)?
            .lock()
            .await;
        Ok(mls_group.epoch())
    }

    /// Set the MLS group instance for this group.
    ///
    /// ## Parameters:
    /// - `mls_group`: The MLS group instance to set
    ///
    /// ## Effects:
    /// - Sets `is_kp_shared` to `true`
    /// - Stores the MLS group in an `Arc<Mutex<MlsGroup>>`
    pub fn set_mls_group(&mut self, mls_group: MlsGroup) -> Result<(), GroupError> {
        self.is_kp_shared = true;
        self.mls_group = Some(Arc::new(Mutex::new(mls_group)));
        Ok(())
    }

    /// Check if the MLS group is initialized.
    ///
    /// ## Returns:
    /// - `true` if MLS group is set, `false` otherwise
    pub fn is_mls_group_initialized(&self) -> bool {
        self.mls_group.is_some()
    }

    /// Check if the key package has been shared.
    ///
    /// ## Returns:
    /// - `true` if key package is shared, `false` otherwise
    pub fn is_kp_shared(&self) -> bool {
        self.is_kp_shared
    }

    /// Set the key package shared status.
    ///
    /// ## Parameters:
    /// - `is_kp_shared`: Whether the key package is shared
    pub fn set_kp_shared(&mut self, is_kp_shared: bool) {
        self.is_kp_shared = is_kp_shared;
    }

    /// Check if this group has a steward configured.
    ///
    /// ## Returns:
    /// - `true` if steward is configured, `false` otherwise
    pub async fn is_steward(&self) -> bool {
        self.state_machine.read().await.has_steward()
    }

    /// Get the application ID for this group.
    ///
    /// ## Returns:
    /// - Reference to the application ID bytes
    pub fn app_id(&self) -> &[u8] {
        &self.app_id
    }

    /// Get the group name as bytes.
    ///
    /// ## Returns:
    /// - Reference to the group name bytes
    pub fn group_name_bytes(&self) -> &[u8] {
        self.group_name.as_bytes()
    }

    /// Generate a steward announcement message for this group.
    ///
    /// ## Returns:
    /// - Waku message containing the steward announcement
    ///
    /// ## Errors:
    /// - `GroupError::StewardNotSet` if no steward is configured
    ///
    /// ## Effects:
    /// - Refreshes the steward's key pair
    /// - Creates a new group announcement
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

    /// Decrypt a steward message using the group's steward key.
    ///
    /// ## Parameters:
    /// - `message`: The encrypted message bytes
    ///
    /// ## Returns:
    /// - Decrypted KeyPackage
    ///
    /// ## Errors:
    /// - `GroupError::StewardNotSet` if no steward is configured
    /// - Various decryption errors from the steward
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

    /// Store an invite proposal in the steward queue for the current epoch.
    ///
    /// ## Parameters:
    /// - `key_package`: The key package of the member to add
    ///
    /// ## Effects:
    /// - Adds an AddMember proposal to the current epoch
    /// - Proposal will be processed in the next steward epoch
    /// - Returns a serialized `UiUpdateRequest` for UI notification
    pub async fn store_invite_proposal(
        &mut self,
        key_package: Box<KeyPackage>,
    ) -> Result<UpdateRequest, GroupError> {
        let mut state_machine = self.state_machine.write().await;
        state_machine
            .add_proposal(GroupUpdateRequest::AddMember(key_package.clone()))
            .await;

        let wallet_bytes = key_package
            .leaf_node()
            .credential()
            .serialized_content()
            .to_vec();

        Ok(UpdateRequest {
            request_type: RequestType::AddMember as i32,
            wallet_address: wallet_bytes,
        })
    }

    /// Store a remove proposal in the steward queue for the current epoch.
    ///
    /// ## Parameters:
    /// - `identity`: The identity string of the member to remove
    ///
    /// ## Returns:
    /// - Returns a serialized `UiUpdateRequest` for UI notification
    /// - `GroupError::InvalidIdentity` if the identity is invalid
    pub async fn store_remove_proposal(
        &mut self,
        identity: String,
    ) -> Result<UpdateRequest, GroupError> {
        let normalized_identity = normalize_wallet_address_str(&identity)?;
        let mut state_machine = self.state_machine.write().await;
        state_machine
            .add_proposal(GroupUpdateRequest::RemoveMember(
                normalized_identity.clone(),
            ))
            .await;

        let wallet_bytes = hex::decode(
            normalized_identity
                .strip_prefix("0x")
                .unwrap_or(&normalized_identity),
        )?;

        Ok(UpdateRequest {
            request_type: RequestType::RemoveMember as i32,
            wallet_address: wallet_bytes,
        })
    }

    /// Process an application message and determine the appropriate action.
    ///
    /// ## Parameters:
    /// - `message`: The application message to process
    ///
    /// ## Returns:
    /// - `GroupAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - For ban requests from stewards: automatically adds remove proposals
    /// - For other messages: processes normally
    ///
    /// ## Supported Message Types:
    /// - Conversation messages
    /// - Proposals
    /// - Votes
    /// - Ban requests
    pub async fn process_application_message(
        &mut self,
        message: ApplicationMessage,
    ) -> Result<GroupAction, GroupError> {
        let app_msg = AppMessage::decode(message.into_bytes().as_slice())?;
        match app_msg.payload {
            Some(app_message::Payload::ConversationMessage(conversation_message)) => {
                info!("[process_application_message]: Processing conversation message");
                Ok(GroupAction::GroupAppMsg(conversation_message.into()))
            }
            Some(app_message::Payload::Proposal(proposal)) => {
                info!("[process_application_message]: Processing proposal message");
                Ok(GroupAction::GroupProposal(proposal))
            }
            Some(app_message::Payload::Vote(vote)) => {
                info!("[process_application_message]: Processing vote message");
                Ok(GroupAction::GroupVote(vote))
            }
            Some(app_message::Payload::BanRequest(ban_request)) => {
                info!("[process_application_message]: Processing ban request message");

                if self.is_steward().await {
                    info!(
                        "[process_application_message]: Steward adding remove proposal for user {}",
                        ban_request.user_to_ban.clone()
                    );
                    let _ = self
                        .store_remove_proposal(ban_request.user_to_ban.clone())
                        .await?;
                } else {
                    info!(
                        "[process_application_message]: Non-steward received ban request message"
                    );
                }

                Ok(GroupAction::GroupAppMsg(ban_request.into()))
            }
            _ => Ok(GroupAction::DoNothing),
        }
    }

    /// Process a protocol message from the MLS group.
    ///
    /// ## Parameters:
    /// - `message`: The protocol message to process
    /// - `provider`: The MLS provider for processing
    ///
    /// ## Returns:
    /// - `GroupAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - Processes MLS group messages
    /// - Handles member removal scenarios
    /// - Stores pending proposals
    ///
    /// ## Supported Message Types:
    /// - Application messages
    /// - Proposal messages
    /// - External join proposals
    /// - Staged commit messages
    pub async fn process_protocol_msg(
        &mut self,
        message: ProtocolMessage,
        provider: &MlsProvider,
    ) -> Result<GroupAction, GroupError> {
        let group_id = message.group_id().as_slice();
        if group_id != self.group_name_bytes() {
            return Ok(GroupAction::DoNothing);
        }
        let mut mls_group = self
            .mls_group
            .as_ref()
            .ok_or_else(|| GroupError::MlsGroupNotSet)?
            .lock()
            .await;
        // If the message is from a previous epoch, we don't need to process it and it's a commit for welcome message
        if message.epoch() < mls_group.epoch() && message.epoch() == 0.into() {
            return Ok(GroupAction::DoNothing);
        }

        let processed_message = mls_group.process_message(provider, message)?;

        match processed_message.into_content() {
            ProcessedMessageContent::ApplicationMessage(application_message) => {
                drop(mls_group);
                self.process_application_message(application_message).await
            }
            ProcessedMessageContent::ProposalMessage(proposal_ptr) => {
                mls_group
                    .store_pending_proposal(provider.storage(), proposal_ptr.as_ref().clone())?;
                Ok(GroupAction::DoNothing)
            }
            ProcessedMessageContent::ExternalJoinProposalMessage(_external_proposal_ptr) => {
                Ok(GroupAction::DoNothing)
            }
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
                    Ok(GroupAction::LeaveGroup)
                } else {
                    Ok(GroupAction::DoNothing)
                }
            }
        }
    }

    /// Build and validate a message for sending to the group.
    ///
    /// ## Parameters:
    /// - `provider`: The MLS provider for message creation
    /// - `signer`: The signature key pair for signing
    /// - `msg`: The application message to build
    ///
    /// ## Returns:
    /// - Waku message ready for transmission
    ///
    /// ## Effects:
    /// - Validates message can be sent in current state
    /// - Creates MLS message with proper signing
    ///
    /// ## Validation:
    /// - Checks state machine permissions
    /// - Ensures steward status and proposal availability
    pub async fn build_message(
        &mut self,
        provider: &MlsProvider,
        signer: &SignatureKeyPair,
        msg: &AppMessage,
    ) -> Result<WakuMessageToSend, GroupError> {
        let is_steward = self.is_steward().await;
        let has_proposals = self.get_pending_proposals_count().await > 0;

        let message_type = msg
            .payload
            .as_ref()
            .map(|p| p.message_type())
            .unwrap_or(message_types::UNKNOWN);

        // Check if message can be sent in current state
        let state_machine = self.state_machine.read().await;
        let current_state = state_machine.current_state();
        if !state_machine.can_send_message_type(is_steward, has_proposals, message_type) {
            return Err(GroupError::InvalidStateToMessageSend {
                state: current_state.to_string(),
                message_type: message_type.to_string(),
            });
        }
        let message_out = self
            .mls_group
            .as_mut()
            .ok_or_else(|| GroupError::MlsGroupNotSet)?
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

    /// Get the current state of the group state machine.
    ///
    /// ## Returns:
    /// - Current `GroupState` of the group
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

    /// Get the current epoch proposals for UI display.
    pub async fn get_current_epoch_proposals(&self) -> Vec<GroupUpdateRequest> {
        self.state_machine
            .read()
            .await
            .get_current_epoch_proposals()
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

    pub async fn get_proposals_for_voting_epoch_as_ui_update_requests(&self) -> Vec<UpdateRequest> {
        self.get_proposals_for_voting_epoch()
            .await
            .iter()
            .map(|p| p.clone().into())
            .collect()
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

    /// Start waiting state (for non-steward peers after consensus or edge case recovery)
    pub async fn start_waiting(&mut self) {
        self.state_machine.write().await.start_waiting();
    }

    /// Start steward epoch with validation
    pub async fn start_steward_epoch_with_validation(&mut self) -> Result<usize, GroupError> {
        self.state_machine
            .write()
            .await
            .start_steward_epoch_with_validation()
            .await
    }

    /// Handle successful vote for group
    pub async fn handle_yes_vote(&mut self) -> Result<(), GroupError> {
        self.state_machine.write().await.handle_yes_vote().await
    }

    /// Handle failed vote for group
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
    ///
    /// ## Parameters:
    /// - `provider`: The MLS provider for proposal creation
    /// - `signer`: The signature key pair for signing
    ///
    /// ## Returns:
    /// - Vector of Waku messages: [batch_proposals_msg, welcome_msg]
    /// - Welcome message is only included if there are new members to add
    ///
    /// ## Preconditions:
    /// - Must be a steward
    /// - Must have proposals in the voting epoch
    ///
    /// ## Effects:
    /// - Creates MLS proposals for all pending group updates
    /// - Commits all proposals to the MLS group
    /// - Merges the commit to apply changes
    ///
    /// ## Supported Proposal Types:
    /// - AddMember: Adds new member with key package
    /// - RemoveMember: Removes member by identity
    ///
    /// ## Errors:
    /// - `GroupError::StewardNotSet` if not a steward
    /// - `GroupError::EmptyProposals` if no proposals exist
    /// - Various MLS processing errors
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

        let mut member_indices = Vec::new();
        for proposal in &proposals {
            if let GroupUpdateRequest::RemoveMember(identity) = proposal {
                // Convert the address string to bytes for proper MLS credential matching
                let identity_bytes = if let Some(hex_string) = identity.strip_prefix("0x") {
                    // Remove 0x prefix and convert to bytes
                    hex::decode(hex_string)?
                } else {
                    // Assume it's already a hex string without 0x prefix
                    hex::decode(identity)?
                };

                let member_index = self.find_member_index(identity_bytes).await?;
                member_indices.push(member_index);
            } else {
                member_indices.push(None);
            }
        }
        let mut mls_proposals = Vec::new();
        let (out_messages, welcome) = {
            let mut mls_group = self
                .mls_group
                .as_mut()
                .ok_or_else(|| GroupError::MlsGroupNotSet)?
                .lock()
                .await;

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
                            error!("[create_batch_proposals_message]: Failed to find member index for identity: {identity}");
                        }
                    }
                }
            }

            // Create commit with all proposals
            let (out_messages, welcome, _group_info) =
                mls_group.commit_to_pending_proposals(provider, signer)?;

            // Merge the commit
            mls_group.merge_pending_commit(provider)?;
            (out_messages, welcome)
        };
        // Create batch proposals message (without welcome)
        let batch_msg: AppMessage = BatchProposalsMessage {
            group_name: self.group_name_bytes().to_vec(),
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
