use alloy::{
    primitives::Address,
    signers::{local::PrivateKeySigner, Signer},
};
use kameo::Actor;
use openmls::{
    group::MlsGroupJoinConfig,
    prelude::{DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, StagedWelcome, Welcome},
};
use prost::Message;
use std::{collections::HashMap, fmt::Display, str::FromStr, sync::Arc};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info};
use waku_bindings::WakuMessage;

use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::{
    identity::Identity,
    openmls_provider::{MlsProvider, CIPHERSUITE},
};

use crate::{
    consensus::{ConsensusEvent, ConsensusService},
    error::UserError,
    group::{Group, GroupAction},
    protos::{
        consensus::v1::{Proposal, Vote},
        de_mls::messages::v1::{
            app_message, welcome_message, AppMessage, BanRequest, BatchProposalsMessage,
            ConversationMessage, UserKeyPackage, VotingProposal, WelcomeMessage,
        },
    },
    state_machine::GroupState,
    LocalSigner,
};

/// Represents the action to take after processing a user message or event.
///
/// This enum defines the possible outcomes when processing user-related operations,
/// allowing the caller to determine the appropriate next steps for message handling,
/// group management, and network communication.
#[derive(Debug, Clone, PartialEq)]
pub enum UserAction {
    SendToWaku(WakuMessageToSend),
    SendToApp(AppMessage),
    LeaveGroup(String),
    DoNothing,
}

impl Display for UserAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserAction::SendToWaku(_) => write!(f, "SendToWaku"),
            UserAction::SendToApp(_) => write!(f, "SendToApp"),
            UserAction::LeaveGroup(group_name) => write!(f, "LeaveGroup({group_name})"),
            UserAction::DoNothing => write!(f, "DoNothing"),
        }
    }
}

/// Represents a user in the MLS-based messaging system.
///
/// The User struct manages the lifecycle of multiple groups, handles consensus operations,
/// and coordinates communication between the application layer and the Waku network.
/// It integrates with the consensus service for proposal management and voting.
///
/// ## Key Features:
/// - Multi-group management and coordination
/// - Consensus service integration for proposal handling
/// - Waku message processing and routing
/// - Steward epoch coordination
/// - Member management through proposals
#[derive(Actor)]
pub struct User {
    identity: Identity,
    // Each group has its own lock for better concurrency
    groups: Arc<RwLock<HashMap<String, Arc<RwLock<Group>>>>>,
    provider: MlsProvider,
    consensus_service: ConsensusService,
    eth_signer: PrivateKeySigner,
    // Queue for batch proposals that arrive before consensus is reached
    pending_batch_proposals: Arc<RwLock<HashMap<String, BatchProposalsMessage>>>,
}

impl User {
    /// Create a new user instance with the specified Ethereum private key.
    ///
    /// ## Parameters:
    /// - `user_eth_priv_key`: The user's Ethereum private key as a hex string
    ///
    /// ## Returns:
    /// - New User instance with initialized identity and services
    ///
    /// ## Errors:
    /// - `UserError` if private key parsing or identity creation fails
    pub fn new(
        user_eth_priv_key: &str,
        consensus_service: &ConsensusService,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(user_eth_priv_key)?;
        let user_address = signer.address();

        let crypto = MlsProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, user_address.as_slice())?;

        let user = User {
            groups: Arc::new(RwLock::new(HashMap::new())),
            identity: id,
            eth_signer: signer,
            provider: crypto,
            consensus_service: consensus_service.clone(),
            pending_batch_proposals: Arc::new(RwLock::new(HashMap::new())),
        };
        Ok(user)
    }

    /// Get a subscription to consensus events
    pub fn subscribe_to_consensus_events(&self) -> broadcast::Receiver<(String, ConsensusEvent)> {
        self.consensus_service.subscribe_to_events()
    }

    pub async fn set_up_consensus_threshold_for_group(
        &mut self,
        group_name: &str,
        proposal_id: u32,
        consensus_threshold: f64,
    ) -> Result<(), UserError> {
        self.consensus_service
            .set_consensus_threshold_for_group_session(group_name, proposal_id, consensus_threshold)
            .await?;
        Ok(())
    }

    /// Create a new group for this user.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to create
    /// - `is_creation`: Whether this is a group creation (true) or joining (false)
    ///
    /// ## Effects:
    /// - If `is_creation` is true: Creates MLS group with steward capabilities
    /// - If `is_creation` is false: Creates empty group for later joining
    /// - Adds group to user's groups map
    ///
    /// ## Errors:
    /// - `UserError::GroupAlreadyExistsError` if group already exists
    /// - Various MLS group creation errors
    pub async fn create_group(
        &mut self,
        group_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        if groups.contains_key(group_name) {
            return Err(UserError::GroupAlreadyExistsError);
        }
        let group = if is_creation {
            Group::new(
                group_name,
                true,
                Some(&self.provider),
                Some(self.identity.signer()),
                Some(&self.identity.credential_with_key()),
            )?
        } else {
            Group::new(group_name, false, None, None, None)?
        };

        groups.insert(group_name.to_string(), Arc::new(RwLock::new(group)));
        Ok(())
    }

    /// Check if a group exists for this user.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to check
    ///
    /// ## Returns:
    /// - `true` if group exists, `false` otherwise
    pub async fn if_group_exists(&self, group_name: &str) -> bool {
        let groups = self.groups.read().await;
        groups.contains_key(group_name)
    }

    /// Get the state of a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to get the state of
    ///
    /// ## Returns:
    /// - `GroupState` of the group
    pub async fn get_group_state(&self, group_name: &str) -> Result<GroupState, UserError> {
        let groups = self.groups.read().await;
        let state = groups
            .get(group_name)
            .cloned()
            .ok_or_else(|| UserError::GroupNotFoundError)?
            .read()
            .await
            .get_state()
            .await;

        Ok(state)
    }

    /// Get the number of members in a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to get the number of members of
    ///
    /// ## Returns:
    /// - The number of members in the group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    pub async fn get_group_number_of_members(&self, group_name: &str) -> Result<usize, UserError> {
        let groups = self.groups.read().await;
        let members = groups
            .get(group_name)
            .cloned()
            .ok_or_else(|| UserError::GroupNotFoundError)?
            .read()
            .await
            .members_identity()
            .await?;
        Ok(members.len())
    }

    /// Get the MLS epoch of a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to get the MLS epoch of
    ///
    /// ## Returns:
    /// - The MLS epoch of the group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    pub async fn get_group_mls_epoch(&self, group_name: &str) -> Result<u64, UserError> {
        let groups = self.groups.read().await;
        let epoch = groups
            .get(group_name)
            .cloned()
            .ok_or_else(|| UserError::GroupNotFoundError)?
            .read()
            .await
            .epoch()
            .await?;
        Ok(epoch.as_u64())
    }

    /// Check if a user is in a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to check if the user is in
    /// - `user_address`: The address of the user to check if they are in the group
    ///
    /// ## Returns:
    /// - `true` if the user is in the group, `false` otherwise
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    pub async fn check_if_user_in_group(
        &self,
        group_name: &str,
        user_address: &str,
    ) -> Result<bool, UserError> {
        let groups = self.groups.read().await;
        let members = groups
            .get(group_name)
            .cloned()
            .ok_or_else(|| UserError::GroupNotFoundError)?
            .read()
            .await
            .members_identity()
            .await?;
        Ok(members.contains(&user_address.as_bytes().to_vec()))
    }

    pub async fn is_user_steward_for_group(&self, group_name: &str) -> Result<bool, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };
        let is_steward = group.read().await.is_steward().await;
        Ok(is_steward)
    }

    pub async fn get_current_epoch_proposals(
        &self,
        group_name: &str,
    ) -> Result<Vec<crate::steward::GroupUpdateRequest>, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };
        let proposals = group.read().await.get_current_epoch_proposals().await;
        Ok(proposals)
    }

    /// Process messages from the welcome subtopic.
    ///
    /// ## Parameters:
    /// - `msg`: The Waku message to process
    /// - `group_name`: The name of the group this message is for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Message Types Handled:
    /// - **GroupAnnouncement**: Steward announcements for group joining
    /// - **UserKeyPackage**: Encrypted key packages from new members
    /// - **InvitationToJoin**: MLS welcome messages for group joining
    ///
    /// ## Effects:
    /// - For group announcements: Generates and sends key package
    /// - For user key packages: Decrypts and stores invite proposals (steward only)
    /// - For invitations: Processes MLS welcome and joins group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::MessageVerificationFailed` if announcement verification fails
    /// - Various MLS and encryption errors
    pub async fn process_welcome_subtopic(
        &mut self,
        msg: WakuMessage,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        // Get the group lock first
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        let is_steward = {
            let group = group.read().await;
            group.is_steward().await
        };
        let is_kp_shared = {
            let group = group.read().await;
            group.is_kp_shared()
        };
        let is_mls_group_initialized = {
            let group = group.read().await;
            group.is_mls_group_initialized()
        };

        let received_msg = WelcomeMessage::decode(msg.payload())?;
        if let Some(payload) = &received_msg.payload {
            match payload {
                welcome_message::Payload::GroupAnnouncement(group_announcement) => {
                    if is_steward || is_kp_shared {
                        Ok(UserAction::DoNothing)
                    } else {
                        info!(
                            "[user::process_welcome_subtopic]: User received group announcement message for group {group_name}"
                        );
                        if !group_announcement.verify()? {
                            return Err(UserError::MessageVerificationFailed);
                        }

                        let new_kp = self.identity.generate_key_package(&self.provider)?;
                        let encrypted_key_package = group_announcement.encrypt(new_kp)?;
                        group.write().await.set_kp_shared(true);

                        let welcome_msg: WelcomeMessage = UserKeyPackage {
                            encrypt_kp: encrypted_key_package,
                        }
                        .into();
                        Ok(UserAction::SendToWaku(WakuMessageToSend::new(
                            welcome_msg.encode_to_vec(),
                            WELCOME_SUBTOPIC,
                            group_name,
                            group.read().await.app_id(),
                        )))
                    }
                }
                welcome_message::Payload::UserKeyPackage(user_key_package) => {
                    if is_steward {
                        info!(
                            "[user::process_welcome_subtopic]: Steward received key package for the group {group_name}"
                        );
                        let key_package = group
                            .write()
                            .await
                            .decrypt_steward_msg(user_key_package.encrypt_kp.clone())
                            .await?;

                        let (action, address) = group
                            .write()
                            .await
                            .store_invite_proposal(Box::new(key_package))
                            .await?;

                        // Send notification to UI about the new proposal
                        let proposal_added_msg: AppMessage =
                            crate::protos::de_mls::messages::v1::ProposalAdded {
                                group_id: group_name.to_string(),
                                action,
                                address,
                            }
                            .into();

                        Ok(UserAction::SendToApp(proposal_added_msg))
                    } else {
                        Ok(UserAction::DoNothing)
                    }
                }
                welcome_message::Payload::InvitationToJoin(invitation_to_join) => {
                    if is_steward || is_mls_group_initialized {
                        Ok(UserAction::DoNothing)
                    } else {
                        // Release the lock before calling join_group
                        drop(group);

                        // Parse the MLS message to get the welcome
                        let (mls_in, _) = MlsMessageIn::tls_deserialize_bytes(
                            &invitation_to_join.mls_message_out_bytes,
                        )?;

                        let welcome = match mls_in.extract() {
                            MlsMessageBodyIn::Welcome(welcome) => welcome,
                            _ => return Err(UserError::FailedToExtractWelcomeMessage),
                        };

                        if welcome.secrets().iter().any(|egs| {
                            let hash_ref = egs.new_member().as_slice().to_vec();
                            self.identity.is_key_package_exists(&hash_ref)
                        }) {
                            self.join_group(welcome).await?;
                            let msg = self
                                .build_system_message(
                                    format!(
                                        "User {} joined to the group",
                                        self.identity.identity_string()
                                    )
                                    .as_bytes()
                                    .to_vec(),
                                    group_name,
                                )
                                .await?;
                            Ok(UserAction::SendToWaku(msg))
                        } else {
                            Ok(UserAction::DoNothing)
                        }
                    }
                }
            }
        } else {
            Err(UserError::EmptyWelcomeMessageError)
        }
    }

    /// Process messages from the application message subtopic.
    ///
    /// ## Parameters:
    /// - `msg`: The Waku message to process
    /// - `group_name`: The name of the group this message is for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Message Types Handled:
    /// - **BatchProposalsMessage**: Batch proposals from steward
    /// - **MLS Protocol Messages**: Encrypted group messages
    /// - **Application Messages**: Various app-level messages
    ///
    /// ## Effects:
    /// - Processes batch proposals and applies them to the group
    /// - Handles MLS protocol messages through the group
    /// - Routes consensus proposals and votes to appropriate handlers
    ///
    /// ## Preconditions:
    /// - Group must be initialized with MLS group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various MLS processing errors
    pub async fn process_app_subtopic(
        &mut self,
        msg: WakuMessage,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if !group.read().await.is_mls_group_initialized() {
            return Ok(UserAction::DoNothing);
        }

        // Try to parse as AppMessage first
        // This one required for commit messages as they are sent as AppMessage
        // without group encryption
        if let Ok(app_message) = AppMessage::decode(msg.payload()) {
            match app_message.payload {
                Some(app_message::Payload::BatchProposalsMessage(batch_msg)) => {
                    info!(
                        "[user::process_app_subtopic]: Processing batch proposals message for group {group_name}"
                    );
                    // Release the lock before calling self methods
                    return self
                        .process_batch_proposals_message(batch_msg, group_name)
                        .await;
                }
                _ => {
                    error!(
                        "[user::process_app_subtopic]: Cannot process another app message here: {:?}",
                        app_message.to_string()
                    );
                    return Err(UserError::InvalidAppMessageType);
                }
            }
        }

        // Fall back to MLS protocol message
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(msg.payload())?;
        let mls_message = mls_message_in.try_into_protocol_message()?;

        let res = {
            info!("[user::process_app_subtopic]: processing encrypted protocol message");
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
                .write()
                .await
                .process_protocol_msg(mls_message, &self.provider)
                .await
        }?;

        // Handle the result outside of any lock scope
        match res {
            GroupAction::GroupAppMsg(msg) => {
                info!("[user::process_app_subtopic]: sending to app");
                Ok(UserAction::SendToApp(msg))
            }
            GroupAction::LeaveGroup => {
                info!("[user::process_app_subtopic]: leaving group");
                Ok(UserAction::LeaveGroup(group_name.to_string()))
            }
            GroupAction::DoNothing => {
                info!("[user::process_app_subtopic]: doing nothing");
                Ok(UserAction::DoNothing)
            }
            GroupAction::GroupProposal(proposal) => {
                info!("[user::process_app_subtopic]: processing consensus proposal");
                self.process_consensus_proposal(proposal, group_name).await
            }
            GroupAction::GroupVote(vote) => {
                info!("[user::process_app_subtopic]: processing consensus vote");
                self.process_consensus_vote(vote, group_name).await
            }
        }
    }

    /// Process batch proposals message from the steward.
    ///
    /// ## Parameters:
    /// - `batch_msg`: The batch proposals message to process
    /// - `group_name`: The name of the group these proposals are for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - Processes all MLS proposals in the batch
    /// - Applies the commit message to complete the group update
    /// - Transitions group to Working state after successful processing
    ///
    /// ## State Requirements:
    /// - Group must be in Waiting state to process batch proposals
    /// - If not in correct state, stores proposals for later processing
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various MLS processing errors
    async fn process_batch_proposals_message(
        &mut self,
        batch_msg: BatchProposalsMessage,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        // Get the group lock
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        let initial_state = group.read().await.get_state().await;
        if initial_state != GroupState::Waiting {
            info!(
                "[user::process_batch_proposals_message]: Cannot process batch proposals in {initial_state} state, storing for later processing"
            );
            // Store the batch proposals for later processing
            self.store_pending_batch_proposals(group_name, batch_msg)
                .await;
            return Ok(UserAction::DoNothing);
        }

        // Process all proposals before the commit
        for proposal_bytes in batch_msg.mls_proposals {
            let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&proposal_bytes)?;
            let protocol_message = mls_message_in.try_into_protocol_message()?;

            let _res = group
                .write()
                .await
                .process_protocol_msg(protocol_message, &self.provider)
                .await?;
        }

        // Then process the commit message
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&batch_msg.commit_message)?;
        let protocol_message = mls_message_in.try_into_protocol_message()?;

        let res = group
            .write()
            .await
            .process_protocol_msg(protocol_message, &self.provider)
            .await?;

        group.write().await.start_working().await;

        match res {
            GroupAction::GroupAppMsg(msg) => Ok(UserAction::SendToApp(msg)),
            GroupAction::LeaveGroup => Ok(UserAction::LeaveGroup(group_name.to_string())),
            GroupAction::DoNothing => Ok(UserAction::DoNothing),
            GroupAction::GroupProposal(proposal) => {
                self.process_consensus_proposal(proposal, group_name).await
            }
            GroupAction::GroupVote(vote) => self.process_consensus_vote(vote, group_name).await,
        }
    }

    /// Process incoming Waku messages and route them to appropriate handlers.
    ///
    /// ## Parameters:
    /// - `msg`: The Waku message to process
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Message Routing:
    /// - **Welcome Subtopic**: Routes to `process_welcome_subtopic()`
    /// - **App Message Subtopic**: Routes to `process_app_subtopic()`
    /// - **Unknown Topics**: Returns error
    ///
    /// ## Effects:
    /// - Processes messages based on content topic
    /// - Skips messages from the same app instance
    /// - Routes to appropriate subtopic handlers
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::UnknownContentTopicType` for unsupported topics
    /// - Various processing errors from subtopic handlers
    pub async fn process_waku_message(
        &mut self,
        msg: WakuMessage,
    ) -> Result<UserAction, UserError> {
        let group_name = msg.content_topic.application_name.to_string();
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(&group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if msg.meta == group.read().await.app_id() {
            debug!("Message is from the same app, skipping");
            return Ok(UserAction::DoNothing);
        }

        let ct_name = msg.content_topic.content_topic_name.to_string();
        match ct_name.as_str() {
            WELCOME_SUBTOPIC => self.process_welcome_subtopic(msg, &group_name).await,
            APP_MSG_SUBTOPIC => self.process_app_subtopic(msg, &group_name).await,
            _ => Err(UserError::UnknownContentTopicType(ct_name)),
        }
    }

    /// Join a group after receiving a welcome message.
    ///
    /// ## Parameters:
    /// - `welcome`: The MLS welcome message containing group information
    ///
    /// ## Effects:
    /// - Creates new MLS group from welcome message
    /// - Sets the MLS group in the user's group instance
    /// - Updates group state to reflect successful joining
    ///
    /// ## Preconditions:
    /// - Group must already exist in user's groups map
    /// - Welcome message must be valid and contain proper group data
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various MLS group creation errors
    async fn join_group(&mut self, welcome: Welcome) -> Result<(), UserError> {
        let group_config = MlsGroupJoinConfig::builder().build();
        let mls_group =
            StagedWelcome::new_from_welcome(&self.provider, &group_config, welcome, None)?
                .into_group(&self.provider)?;

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id)?;

        let groups = self.groups.read().await;
        groups
            .get(&group_name)
            .cloned()
            .ok_or_else(|| UserError::GroupNotFoundError)?
            .write()
            .await
            .set_mls_group(mls_group)?;

        info!("[user::join_group]: User joined group {group_name}");
        Ok(())
    }

    /// Leave a group and clean up associated resources.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to leave
    ///
    /// ## Effects:
    /// - Removes group from user's groups map
    /// - Cleans up all group-related resources
    ///
    /// ## Preconditions:
    /// - Group must exist in user's groups map
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!("[user::leave_group]: Leaving group {group_name}");
        if !self.if_group_exists(group_name).await {
            return Err(UserError::GroupNotFoundError);
        }
        let mut groups = self.groups.write().await;
        groups.remove(group_name);
        Ok(())
    }

    /// Prepare a steward announcement message for a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to prepare the message for
    ///
    /// ## Returns:
    /// - Waku message containing the steward announcement
    ///
    /// ## Preconditions:
    /// - Group must exist and be initialized
    ///
    /// ## Effects:
    /// - Generates new steward announcement with refreshed keys
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various steward message generation errors
    pub async fn prepare_steward_msg(
        &mut self,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        let msg_to_send = group.write().await.generate_steward_message().await?;
        Ok(msg_to_send)
    }

    /// Build a group message for sending to the group.
    ///
    /// ## Parameters:
    /// - `msg`: The message content as bytes
    /// - `group_name`: The name of the group to send the message to
    ///
    /// ## Returns:
    /// - Waku message ready for transmission
    ///
    /// ## Preconditions:
    /// - Group must be initialized with MLS group
    ///
    /// ## Effects:
    /// - Creates conversation message with sender identity
    /// - Builds MLS message through the group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::MlsGroupNotInitialized` if MLS group not initialized
    /// - Various MLS message building errors
    pub async fn build_group_message(
        &mut self,
        msg: Vec<u8>,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if !group.read().await.is_mls_group_initialized() {
            return Err(UserError::MlsGroupNotInitialized);
        }

        let app_msg = ConversationMessage {
            message: msg,
            sender: self.identity.identity_string(),
            group_name: group_name.to_string(),
        }
        .into();

        let msg_to_send = group
            .write()
            .await
            .build_message(&self.provider, self.identity.signer(), &app_msg)
            .await?;

        Ok(msg_to_send)
    }

    /// Build a system message for sending to the group.
    ///
    /// ## Parameters:
    /// - `system_message`: The message content as bytes
    /// - `group_name`: The name of the group to send the message to
    ///
    /// ## Returns:
    /// - Waku message ready for transmission
    ///
    /// ## Preconditions:
    /// - Group must be initialized with MLS group
    ///
    /// ## Effects:
    /// - Creates encrypted system message
    /// - Builds MLS message through the group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::MlsGroupNotInitialized` if MLS group not initialized
    /// - Various MLS message building errors
    pub async fn build_system_message(
        &mut self,
        system_message: Vec<u8>,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if !group.read().await.is_mls_group_initialized() {
            return Err(UserError::MlsGroupNotInitialized);
        }

        let app_msg = ConversationMessage {
            message: system_message,
            sender: "SYSTEM".to_string(),
            group_name: group_name.to_string(),
        }
        .into();

        let msg_to_send = group
            .write()
            .await
            .build_message(&self.provider, self.identity.signer(), &app_msg)
            .await?;

        Ok(msg_to_send)
    }

    /// Build a special message (ban request/vote) preserving the original AppMessage structure.
    ///
    /// ## Parameters:
    /// - `app_message`: The application message to build
    /// - `group_name`: The name of the group to send the message to
    ///
    /// ## Returns:
    /// - Waku message ready for transmission
    ///
    /// ## Preconditions:
    /// - Group must be initialized with MLS group
    ///
    /// ## Effects:
    /// - Preserves original AppMessage structure without wrapping
    /// - Builds MLS message through the group
    ///
    /// ## Usage:
    /// Used for consensus-related messages like proposals and votes
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::MlsGroupNotInitialized` if MLS group not initialized
    /// - Various MLS message building errors
    pub async fn build_changer_message(
        &mut self,
        app_message: AppMessage,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if !group.read().await.is_mls_group_initialized() {
            return Err(UserError::MlsGroupNotInitialized);
        }

        let msg_to_send = group
            .write()
            .await
            .build_message(&self.provider, self.identity.signer(), &app_message)
            .await?;

        Ok(msg_to_send)
    }

    /// Get the identity string for debugging and identification purposes.
    ///
    /// ## Returns:
    /// - String representation of the user's identity
    ///
    /// ## Usage:
    /// Primarily used for debugging, logging, and user identification
    pub fn identity_string(&self) -> String {
        self.identity.identity_string()
    }

    /// Apply proposals for the given group, returning the batch message(s).
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to apply proposals for
    ///
    /// ## Returns:
    /// - Vector of Waku messages containing batch proposals and welcome messages
    ///
    /// ## Preconditions:
    /// - Group must be initialized with MLS group
    /// - User must be steward for the group
    ///
    /// ## Effects:
    /// - Creates MLS proposals for all pending group updates
    /// - Commits all proposals to the MLS group
    /// - Generates batch proposals message and welcome message if needed
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::MlsGroupNotInitialized` if MLS group not initialized
    /// - Various MLS proposal creation errors
    pub async fn apply_proposals(
        &mut self,
        group_name: &str,
    ) -> Result<Vec<WakuMessageToSend>, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if !group.read().await.is_mls_group_initialized() {
            return Err(UserError::MlsGroupNotInitialized);
        }

        let messages = group
            .write()
            .await
            .create_batch_proposals_message(&self.provider, self.identity.signer())
            .await?;
        info!("[user::apply_proposals]: Applied proposals for group {group_name}");
        Ok(messages)
    }

    /// Start a new steward epoch for the given group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to start steward epoch for
    ///
    /// ## Returns:
    /// - Number of proposals that will be voted on (0 if no proposals)
    ///
    /// ## Effects:
    /// - Starts steward epoch through the group's state machine
    /// - Collects proposals for voting
    /// - Transitions group to appropriate state based on proposal count
    ///
    /// ## State Transitions:
    /// - **With proposals**: Working → Waiting (returns proposal count)
    /// - **No proposals**: Working → Working (stays in Working, returns 0)
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various state machine errors
    pub async fn start_steward_epoch(&mut self, group_name: &str) -> Result<usize, UserError> {
        let groups = self.groups.read().await;
        let proposal_count = groups
            .get(group_name)
            .cloned()
            .ok_or_else(|| UserError::GroupNotFoundError)?
            .write()
            .await
            .start_steward_epoch_with_validation()
            .await?;

        if proposal_count == 0 {
            info!("[user::start_steward_epoch]: No proposals to vote on, skipping steward epoch");
        } else {
            info!("[user::start_steward_epoch]: Started steward epoch with {proposal_count} proposals");
        }

        Ok(proposal_count)
    }

    /// Start voting for the given group, returning the proposal ID.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to start voting for
    ///
    /// ## Returns:
    /// - Tuple of (proposal_id, UserAction) for steward actions
    ///
    /// ## Effects:
    /// - Starts voting phase in the group
    /// - Creates consensus proposal for voting
    /// - Sends voting proposal to frontend
    ///
    /// ## State Transitions:
    /// - **Waiting → Voting**: If proposals found and steward starts voting
    /// - **Waiting → Working**: If no proposals found (edge case fix)
    ///
    /// ## Edge Case Handling:
    /// If no proposals are found during voting phase (rare edge case where proposals
    /// disappear between epoch start and voting), transitions back to Working state
    /// to prevent getting stuck in Waiting state.
    ///
    /// ## Preconditions:
    /// - User must be steward for the group
    /// - Group must have proposals in voting epoch
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::NoProposalsFound` if no proposals exist
    /// - Various consensus service errors
    pub async fn get_proposals_for_steward_voting(
        &mut self,
        group_name: &str,
    ) -> Result<(u32, UserAction), UserError> {
        info!(
            "[user::get_proposals_for_steward_voting]: Getting proposals for steward voting in group {group_name}"
        );

        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        // If this is the steward, create proposal with vote and send to group
        if group.read().await.is_steward().await {
            let proposals = group
                .read()
                .await
                .get_proposals_for_voting_epoch_as_ui_update_requests()
                .await;
            if !proposals.is_empty() {
                group.write().await.start_voting().await?;

                // Get group members for expected voters count
                let members = group.read().await.members_identity().await?;
                let participant_ids: Vec<Vec<u8>> = members.into_iter().collect();
                let expected_voters_count = participant_ids.len() as u32;

                // Create consensus proposal
                let proposal = self
                    .consensus_service
                    .create_proposal(
                        group_name,
                        "Group Update Proposal".to_string(),
                        proposals.clone(),
                        self.identity.identity_string().into(),
                        expected_voters_count,
                        3600, // 1 hour expiration
                        true, // liveness criteria
                    )
                    .await?;

                info!(
                    "[user::get_proposals_for_steward_voting]: Created consensus proposal with ID {} and {} expected voters",
                    proposal.proposal_id, expected_voters_count
                );

                // Send voting proposal to frontend
                let voting_proposal: AppMessage = VotingProposal {
                    proposal_id: proposal.proposal_id,
                    group_name: group_name.to_string(),
                    group_requests: proposal.group_requests.clone(),
                }
                .into();

                Ok((proposal.proposal_id, UserAction::SendToApp(voting_proposal)))
            } else {
                error!("[user::get_proposals_for_steward_voting]: No proposals found");
                Err(UserError::NoProposalsFound)
            }
        } else {
            // Not steward, do nothing
            info!("[user::get_proposals_for_steward_voting]: Not steward, doing nothing");
            Ok((0, UserAction::DoNothing))
        }
    }

    /// Add a remove proposal to the steward for the given group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to add the proposal to
    /// - `identity`: The identity string of the member to remove
    ///
    /// ## Effects:
    /// - Stores remove proposal in the group's steward queue
    /// - Proposal will be processed in the next steward epoch
    ///
    /// ## Preconditions:
    /// - Group must exist
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various proposal storage errors
    pub async fn add_remove_proposal(
        &mut self,
        group_name: &str,
        identity: String,
    ) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        groups
            .get(group_name)
            .cloned()
            .ok_or_else(|| UserError::GroupNotFoundError)?
            .write()
            .await
            .store_remove_proposal(identity)
            .await?;
        Ok(())
    }

    /// Handle consensus result after it's determined.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group the consensus is for
    /// - `vote_result`: Whether the consensus passed (true) or failed (false)
    ///
    /// ## Returns:
    /// - Vector of Waku messages to send (if any)
    ///
    /// ## State Transitions:
    /// **Steward:**
    /// - **Vote YES**: Voting → ConsensusReached → Waiting → Working (creates and sends batch proposals, then applies them)
    /// - **Vote NO**: Voting → Working (discards proposals)
    ///
    /// **Non-Steward:**
    /// - **Vote YES**: Voting → ConsensusReached → Waiting → Working (waits for consensus + batch proposals, then applies them)
    /// - **Vote NO**: Voting → Working (no proposals to apply)
    ///
    /// ## Effects:
    /// - Completes voting in the group
    /// - Handles proposal application or cleanup based on result
    /// - Manages state transitions for both steward and non-steward users
    /// - Processes pending batch proposals if available
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various state machine and proposal processing errors
    async fn handle_consensus_result(
        &mut self,
        group_name: &str,
        vote_result: bool,
    ) -> Result<Vec<WakuMessageToSend>, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        group.write().await.complete_voting(vote_result).await?;

        // Handle vote result based on steward status
        if group.read().await.is_steward().await {
            if vote_result {
                // Vote YES: Apply proposals and send commit messages
                info!("[user::complete_voting_for_steward]: Vote YES, sending commit message");

                // Apply proposals and complete (state must be ConsensusReached for this)
                let messages = self.apply_proposals(group_name).await?;
                group.write().await.handle_yes_vote().await?;
                Ok(messages)
            } else {
                // Vote NO: Empty proposal queue without applying, no commit messages
                info!("[user::complete_voting_for_steward]: Vote NO, emptying proposal queue without applying");

                // Empty proposals without state requirement (direct steward call)
                group.write().await.handle_no_vote().await?;

                Ok(vec![])
            }
        } else if vote_result {
            // Vote YES: Transition to ConsensusReached state to await batch proposals
            group.write().await.start_consensus_reached().await;
            info!("[user::handle_consensus_result]: Non-steward user transitioning to ConsensusReached state to await batch proposals");

            // Now transition to Waiting state to follow complete state machine flow
            group.write().await.start_waiting().await;
            info!("[user::handle_consensus_result]: Non-steward user transitioning to Waiting state to await batch proposals");

            // Check if there are pending batch proposals that can now be processed
            if self.has_pending_batch_proposals(group_name).await {
                info!("[user::handle_consensus_result]: Non-steward user has pending batch proposals, processing them now");
                let action = self.process_pending_batch_proposals(group_name).await?;
                info!("[user::handle_consensus_result]: Successfully processed pending batch proposals");
                if let Some(action) = action {
                    match action {
                        UserAction::SendToWaku(waku_message) => {
                            info!(
                                "[user::handle_consensus_result]: Sending waku message to backend"
                            );
                            Ok(vec![waku_message])
                        }
                        UserAction::LeaveGroup(group_name) => {
                            self.leave_group(group_name.as_str()).await?;
                            info!("[user::handle_consensus_result]: Non-steward user left group {group_name}");
                            Ok(vec![])
                        }
                        UserAction::DoNothing => {
                            info!("[user::handle_consensus_result]: No action to process");
                            Ok(vec![])
                        }
                        _ => {
                            error!("[user::handle_consensus_result]: Invalid action to process");
                            Err(UserError::InvalidUserAction(action.to_string()))
                        }
                    }
                } else {
                    info!("[user::handle_consensus_result]: No action to process");
                    Ok(vec![])
                }
            } else {
                info!("[user::handle_consensus_result]: No pending batch proposals to process");
                Ok(vec![])
            }
        } else {
            // Vote NO: Transition to Working state
            group.write().await.start_working().await;
            info!("[user::handle_consensus_result]: Non-steward user transitioning to Working state after failed vote");
            Ok(vec![])
        }
    }

    /// Handle incoming consensus events and return commit messages if needed.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group the consensus event is for
    /// - `event`: The consensus event to handle
    ///
    /// ## Returns:
    /// - Vector of Waku messages to send (if any)
    ///
    /// ## Event Types Handled:
    /// - **ConsensusReached**: Handles successful consensus with result
    /// - **ConsensusFailed**: Handles consensus failure with liveness criteria
    ///
    /// ## Effects:
    /// - Routes consensus events to appropriate handlers
    /// - Manages state transitions based on consensus results
    /// - Applies liveness criteria for failed consensus
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - `UserError::InvalidGroupState` if group is in invalid state
    /// - Various consensus handling errors
    pub async fn handle_consensus_event(
        &mut self,
        group_name: &str,
        event: ConsensusEvent,
    ) -> Result<Vec<WakuMessageToSend>, UserError> {
        match event {
            ConsensusEvent::ConsensusReached {
                proposal_id,
                result,
            } => {
                info!(
                    "[user::handle_consensus_event]: Consensus reached for proposal {proposal_id} in group {group_name}: {result}"
                );

                let group = {
                    let groups = self.groups.read().await;
                    groups
                        .get(group_name)
                        .cloned()
                        .ok_or_else(|| UserError::GroupNotFoundError)?
                };

                let current_state = group.read().await.get_state().await;
                info!("[user::handle_consensus_event]: Current state: {:?} for proposal {proposal_id}", current_state);

                // Handle the consensus result and return commit messages
                let messages = self.handle_consensus_result(group_name, result).await?;
                Ok(messages)
            }
            ConsensusEvent::ConsensusFailed {
                proposal_id,
                reason,
            } => {
                info!(
                    "[user::handle_consensus_event]: Consensus failed for proposal {proposal_id} in group {group_name}: {reason}"
                );

                let group = {
                    let groups = self.groups.read().await;
                    groups
                        .get(group_name)
                        .cloned()
                        .ok_or_else(|| UserError::GroupNotFoundError)?
                };

                let current_state = group.read().await.get_state().await;

                info!("[user::handle_consensus_event]: Handling consensus failure in {:?} state for proposal {proposal_id}", current_state);

                // Handle consensus failure based on current state
                match current_state {
                    GroupState::Voting => {
                        // If we're in Voting state, complete voting with liveness criteria
                        // Get liveness criteria from the actual proposal
                        let liveness_result = self
                            .consensus_service
                            .get_proposal_liveness_criteria(group_name, proposal_id)
                            .await
                            .unwrap_or(false); // Default to false if proposal not found

                        info!("Applying liveness criteria for failed proposal {proposal_id}: {liveness_result}");
                        let messages = self
                            .handle_consensus_result(group_name, liveness_result)
                            .await?;
                        Ok(messages)
                    }
                    _ => Err(UserError::InvalidGroupState(current_state.to_string())),
                }
            }
        }
    }

    /// Process incoming consensus proposal.
    ///
    /// ## Parameters:
    /// - `proposal`: The consensus proposal to process
    /// - `group_name`: The name of the group the proposal is for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - Stores proposal in consensus service
    /// - Starts voting phase in the group
    /// - Creates voting proposal for frontend
    ///
    /// ## State Transitions:
    /// - Any state → Voting (starts voting phase)
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various consensus service errors
    pub async fn process_consensus_proposal(
        &mut self,
        proposal: Proposal,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        self.consensus_service
            .process_incoming_proposal(group_name, proposal.clone())
            .await?;

        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        group.write().await.start_voting().await?;
        info!(
            "[user::process_consensus_proposal]: Starting voting for proposal {}",
            proposal.proposal_id
        );

        // Send voting proposal to frontend
        let voting_proposal: AppMessage = VotingProposal {
            proposal_id: proposal.proposal_id,
            group_requests: proposal.group_requests.clone(),
            group_name: group_name.to_string(),
        }
        .into();

        Ok(UserAction::SendToApp(voting_proposal))
    }

    /// Process user vote from frontend.
    ///
    /// ## Parameters:
    /// - `proposal_id`: The ID of the proposal to vote on
    /// - `user_vote`: The user's vote (true for yes, false for no)
    /// - `group_name`: The name of the group the vote is for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - For stewards: Creates consensus vote and sends to group
    /// - For regular users: Processes user vote in consensus service
    /// - Builds and sends appropriate message to group
    ///
    /// ## Message Types:
    /// - **Steward**: Sends consensus vote message
    /// - **Regular User**: Sends user vote message
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various consensus service and message building errors
    pub async fn process_user_vote(
        &mut self,
        proposal_id: u32,
        user_vote: bool,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };
        let app_message = if group.read().await.is_steward().await {
            info!(
                "[user::process_user_vote]: Steward voting for proposal {proposal_id} in group {group_name}"
            );
            let proposal = self
                .consensus_service
                .vote_on_proposal(group_name, proposal_id, user_vote, self.eth_signer.clone())
                .await?;
            proposal.into()
        } else {
            info!(
                "[user::process_user_vote]: User voting for proposal {proposal_id} in group {group_name}"
            );
            let vote = self
                .consensus_service
                .process_user_vote(group_name, proposal_id, user_vote, self.eth_signer.clone())
                .await?;
            vote.into()
        };

        let waku_msg = self.build_changer_message(app_message, group_name).await?;

        Ok(UserAction::SendToWaku(waku_msg))
    }

    /// Process incoming consensus vote and handle immediate state transitions.
    ///
    /// ## Parameters:
    /// - `vote`: The consensus vote to process
    /// - `group_name`: The name of the group the vote is for
    ///
    /// ## Returns:
    /// - `UserAction` indicating what action should be taken
    ///
    /// ## Effects:
    /// - Stores vote in consensus service
    /// - Handles immediate state transitions if consensus is reached
    ///
    /// ## State Transitions:
    /// When consensus is reached immediately after processing a vote:
    /// - **Vote YES**: Non-steward transitions to Waiting state to await batch proposals
    /// - **Vote NO**: Non-steward transitions to Working state immediately
    /// - **Steward**: Relies on event-driven system for full proposal management
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various consensus service errors
    async fn process_consensus_vote(
        &mut self,
        vote: Vote,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        self.consensus_service
            .process_incoming_vote(group_name, vote.clone())
            .await?;

        Ok(UserAction::DoNothing)
    }

    /// Process incoming ban request.
    ///
    /// ## Parameters:
    /// - `ban_request`: The ban request to process
    /// - `group_name`: The name of the group the ban request is for
    ///
    /// ## Returns:
    /// - Waku message to send to the group
    ///
    /// ## Effects:
    /// - **For stewards**: Adds remove proposal to steward queue and sends system message
    /// - **For regular users**: Forwards ban request to the group
    ///
    /// ## Message Types:
    /// - **Steward**: Sends system message about proposal addition
    /// - **Regular User**: Sends ban request message to group
    ///
    /// ## Errors:
    /// - `UserError::GroupNotFoundError` if group doesn't exist
    /// - Various message building errors
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        let user_to_ban = ban_request.user_to_ban.clone();
        info!(
            "[user::process_ban_request]: Processing ban request for user {user_to_ban} in group {group_name}"
        );

        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        let is_steward = {
            let group = group.read().await;
            group.is_steward().await
        };
        if is_steward {
            // Steward: add the remove proposal to the queue
            info!(
                "[user::process_ban_request]: Steward adding remove proposal for user {user_to_ban}"
            );
            self.add_remove_proposal(group_name, user_to_ban.to_string())
                .await?;

            // Send notification to UI about the new proposal
            let proposal_added_msg: AppMessage =
                crate::protos::de_mls::messages::v1::ProposalAdded {
                    group_id: group_name.to_string(),
                    action: "Remove Member".to_string(),
                    address: user_to_ban,
                }
                .into();

            Ok(UserAction::SendToApp(proposal_added_msg))
        } else {
            // Regular user: send the ban request to the group
            let updated_ban_request = BanRequest {
                user_to_ban: ban_request.user_to_ban,
                requester: self.identity_string(),
                group_name: ban_request.group_name,
            };
            let msg = self
                .build_changer_message(updated_ban_request.into(), group_name)
                .await?;
            Ok(UserAction::SendToWaku(msg))
        }
    }

    /// Store batch proposals for later processing when state becomes correct.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to store proposals for
    /// - `batch_msg`: The batch proposals message to store
    ///
    /// ## Effects:
    /// - Stores batch proposals in pending queue for later processing
    /// - Used when proposals arrive before group is in correct state
    ///
    /// ## Usage:
    /// Called when batch proposals cannot be processed immediately
    /// due to incorrect group state
    async fn store_pending_batch_proposals(
        &self,
        group_name: &str,
        batch_msg: BatchProposalsMessage,
    ) {
        let mut pending = self.pending_batch_proposals.write().await;
        pending.insert(group_name.to_string(), batch_msg);
        info!(
            "[user::store_pending_batch_proposals]: Stored batch proposals for group {} to be processed later",
            group_name
        );
    }

    /// Check if there are pending batch proposals for a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to check
    ///
    /// ## Returns:
    /// - `true` if pending proposals exist, `false` otherwise
    async fn has_pending_batch_proposals(&self, group_name: &str) -> bool {
        let pending = self.pending_batch_proposals.read().await;
        pending.contains_key(group_name)
    }

    /// Retrieve and remove pending batch proposals for a group.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to retrieve proposals for
    ///
    /// ## Returns:
    /// - `Some(BatchProposalsMessage)` if proposals exist, `None` otherwise
    ///
    /// ## Effects:
    /// - Removes proposals from pending queue after retrieval
    async fn retrieve_pending_batch_proposals(
        &self,
        group_name: &str,
    ) -> Option<BatchProposalsMessage> {
        let mut pending = self.pending_batch_proposals.write().await;
        pending.remove(group_name)
    }

    /// Process any pending batch proposals that can now be processed.
    ///
    /// ## Parameters:
    /// - `group_name`: The name of the group to process proposals for
    ///
    /// ## Returns:
    /// - `Some(UserAction)` if proposals were processed, `None` otherwise
    ///
    /// ## Effects:
    /// - Processes any stored batch proposals that were waiting for correct state
    /// - Removes processed proposals from pending queue
    ///
    /// ## Usage:
    /// Called when group state changes to allow processing of previously stored proposals
    async fn process_pending_batch_proposals(
        &mut self,
        group_name: &str,
    ) -> Result<Option<UserAction>, UserError> {
        if self.has_pending_batch_proposals(group_name).await {
            if let Some(batch_msg) = self.retrieve_pending_batch_proposals(group_name).await {
                info!(
                    "[user::process_pending_batch_proposals]: Processing pending batch proposals for group {}",
                    group_name
                );
                let action = self
                    .process_batch_proposals_message(batch_msg, group_name)
                    .await?;
                Ok(Some(action))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }
}

impl LocalSigner for PrivateKeySigner {
    async fn local_sign_message(&self, message: &[u8]) -> Result<Vec<u8>, anyhow::Error> {
        let signature = self.sign_message(message).await?;
        let signature_bytes = signature.as_bytes().to_vec();
        Ok(signature_bytes)
    }

    fn address(&self) -> Address {
        self.address()
    }

    fn address_string(&self) -> String {
        self.address().to_string()
    }

    fn address_bytes(&self) -> Vec<u8> {
        self.address().as_slice().to_vec()
    }
}
