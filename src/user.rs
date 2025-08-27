use alloy::{
    primitives::Address,
    signers::{local::PrivateKeySigner, Signer},
};
use kameo::Actor;
use log::info;
use openmls::{
    group::MlsGroupJoinConfig,
    prelude::{DeserializeBytes, MlsMessageBodyIn, MlsMessageIn, StagedWelcome, Welcome},
};
use prost::Message;
use std::{collections::HashMap, fmt::Display, str::FromStr, sync::Arc};
use tokio::sync::{broadcast, RwLock};
use waku_bindings::WakuMessage;

use ds::{waku_actor::WakuMessageToSend, APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use mls_crypto::{
    identity::Identity,
    openmls_provider::{MlsProvider, CIPHERSUITE},
};

use crate::{
    consensus::{v1::Vote, ConsensusEvent, ConsensusService},
    error::UserError,
    group::{Group, GroupAction},
    protos::messages::v1::{
        app_message, consensus::v1::Proposal, welcome_message, AppMessage, BanRequest,
        BatchProposalsMessage, ConversationMessage, UserKeyPackage, VotingProposal, WelcomeMessage,
    },
    state_machine::GroupState,
    LocalSigner,
};

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
    pub fn new(user_eth_priv_key: &str) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(user_eth_priv_key)?;
        let user_address = signer.address();

        let crypto = MlsProvider::default();
        let id = Identity::new(CIPHERSUITE, &crypto, user_address.as_slice())?;

        let user = User {
            groups: Arc::new(RwLock::new(HashMap::new())),
            identity: id,
            eth_signer: signer,
            provider: crypto,
            consensus_service: ConsensusService::new(),
            pending_batch_proposals: Arc::new(RwLock::new(HashMap::new())),
        };
        Ok(user)
    }

    /// Get a subscription to consensus events
    pub fn subscribe_to_consensus_events(&self) -> broadcast::Receiver<(String, ConsensusEvent)> {
        self.consensus_service.subscribe_to_events()
    }

    pub async fn create_group(
        &mut self,
        group_name: &str,
        is_creation: bool,
    ) -> Result<(), UserError> {
        let mut groups = self.groups.write().await;
        if groups.contains_key(group_name) {
            return Err(UserError::GroupAlreadyExistsError(group_name.to_string()));
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

    pub async fn get_group(&self, group_name: &str) -> Result<Group, UserError> {
        let groups = self.groups.read().await;
        match groups.get(group_name) {
            Some(g) => Ok(g.read().await.clone()),
            None => Err(UserError::GroupNotFoundError),
        }
    }

    pub async fn if_group_exists(&self, group_name: &str) -> bool {
        let groups = self.groups.read().await;
        groups.contains_key(group_name)
    }

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
                    let app_id = group.read().await.app_id();
                    if is_steward || is_kp_shared {
                        info!("Its steward or key package already shared");
                        Ok(UserAction::DoNothing)
                    } else {
                        info!(
                            "User {:?} received group announcement message for group {:?}",
                            self.identity.identity_string(),
                            group_name
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
                            app_id.clone(),
                        )))
                    }
                }
                welcome_message::Payload::UserKeyPackage(user_key_package) => {
                    if is_steward {
                        info!(
                            "Steward {:?} received key package for the group {:?}",
                            self.identity.identity_string(),
                            group_name
                        );
                        let key_package = group
                            .write()
                            .await
                            .decrypt_steward_msg(user_key_package.encrypt_kp.clone())
                            .await?;

                        group
                            .write()
                            .await
                            .store_invite_proposal(Box::new(key_package))
                            .await?;
                        Ok(UserAction::DoNothing)
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
                            _ => return Err(UserError::EmptyWelcomeMessageError),
                        };

                        if welcome.secrets().iter().any(|egs| {
                            let hash_ref = egs.new_member().as_slice().to_vec();
                            self.identity.is_key_package_exists(&hash_ref)
                        }) {
                            self.join_group(welcome).await?;
                            let msg = self
                                .build_group_message(
                                    "User joined to the group".as_bytes().to_vec(),
                                    group_name,
                                )
                                .await?;
                            Ok(UserAction::SendToWaku(msg))
                        } else {
                            info!(
                                "User {:?} received invitation to join group {:?}, but key package is not shared or already joined",
                                self.identity.identity_string(),
                                group_name
                            );
                            Ok(UserAction::DoNothing)
                        }
                    }
                }
            }
        } else {
            Err(UserError::EmptyWelcomeMessageError)
        }
    }

    pub async fn process_app_subtopic(
        &mut self,
        msg: WakuMessage,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        // First check if group exists
        if !self.if_group_exists(group_name).await {
            return Err(UserError::GroupNotFoundError);
        }

        // Get group and check if initialized
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

        info!(
            "[user::process_app_subtopic]: User {:?} received app message for group {:?}",
            self.identity.identity_string(),
            group_name
        );

        // Try to parse as AppMessage first
        // This one required for commit messages as they are sent as AppMessage
        // without group encryption
        if let Ok(app_message) = AppMessage::decode(msg.payload()) {
            info!(
                "[user::process_app_subtopic]: Decoded AppMessage type: {:?}",
                app_message.payload
            );
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
                    info!(
                        "[user::process_app_subtopic]: Cannot process another app message here: {:?}",
                        app_message.to_string()
                    );
                    return Ok(UserAction::DoNothing);
                }
            }
        }

        // Fall back to MLS protocol message
        let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(msg.payload())?;
        let mls_message = mls_message_in.try_into_protocol_message()?;

        // Process the message
        let res = {
            info!("[user::process_app_subtopic]: processing encrypted protocol message");
            let groups = self.groups.read().await;
            if let Some(_group_lock) = groups.get(group_name) {
                _group_lock
                    .write()
                    .await
                    .process_protocol_msg(mls_message, &self.provider)
                    .await
            } else {
                return Err(UserError::GroupNotFoundError);
            }
        }?;

        info!(
            "[user::process_app_subtopic]: start processing group_action: {:?}",
            res.to_string()
        );

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

    /// Process batch proposals message
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

        // Log current state before processing
        let initial_state = group.read().await.get_state().await;
        info!(
            "[user::process_batch_proposals_message]: Initial state before processing: {initial_state}"
        );

        if initial_state != GroupState::Waiting {
            info!(
                "[user::process_batch_proposals_message]: Cannot process batch proposals in {initial_state} state, storing for later processing"
            );
            // Store the batch proposals for later processing instead of discarding them
            self.store_pending_batch_proposals(group_name, batch_msg)
                .await;
            return Ok(UserAction::DoNothing);
        }

        // Process all proposals before the commit
        for proposal_bytes in batch_msg.mls_proposals {
            let (mls_message_in, _) = MlsMessageIn::tls_deserialize_bytes(&proposal_bytes)?;
            let protocol_message = mls_message_in.try_into_protocol_message()?;

            // Process the proposal
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
        // Handle the result outside of any lock scope
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

    /// This function is used to process waku messages
    /// After processing the message, it returns an action to be performed by the user
    ///  - SendToWaku - send a message to the waku network
    ///  - SendToGroup - send a message to the group (print to the application)
    ///  - LeaveGroup - leave a group
    ///  - DoNothing - do nothing
    pub async fn process_waku_message(
        &mut self,
        msg: WakuMessage,
    ) -> Result<UserAction, UserError> {
        let group_name = msg.content_topic.application_name.to_string();

        // Check if group exists and get app_id
        let (group_exists, app_id) = {
            let groups = self.groups.read().await;
            let group = groups.get(&group_name);
            match group {
                Some(g) => (true, g.read().await.app_id()),
                None => (false, Vec::new()),
            }
        };

        if !group_exists {
            return Err(UserError::GroupNotFoundError);
        }

        if msg.meta == app_id {
            info!("Message is from the same app, skipping");
            return Ok(UserAction::DoNothing);
        }

        let ct_name = msg.content_topic.content_topic_name.to_string();
        match ct_name.as_str() {
            WELCOME_SUBTOPIC => self.process_welcome_subtopic(msg, &group_name).await,
            APP_MSG_SUBTOPIC => self.process_app_subtopic(msg, &group_name).await,
            _ => Err(UserError::UnknownContentTopicType(ct_name)),
        }
    }

    /// This function is used to join a group after receiving a welcome message
    async fn join_group(&mut self, welcome: Welcome) -> Result<(), UserError> {
        let group_config = MlsGroupJoinConfig::builder().build();
        let mls_group =
            StagedWelcome::new_from_welcome(&self.provider, &group_config, welcome, None)?
                .into_group(&self.provider)?;

        let group_id = mls_group.group_id().to_vec();
        let group_name = String::from_utf8(group_id)?;

        if !self.if_group_exists(&group_name).await {
            return Err(UserError::GroupNotFoundError);
        }

        // Get the group lock and set the MLS group
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(&group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        group.write().await.set_mls_group(mls_group)?;

        info!(
            "[user::join_group]: User {:?} joined group {:?}",
            self.identity.identity_string(),
            group_name
        );
        Ok(())
    }

    /// This function is used to leave a group after receiving a commit message
    pub async fn leave_group(&mut self, group_name: &str) -> Result<(), UserError> {
        info!("[user::leave_group]: Leaving group {group_name}");
        if !self.if_group_exists(group_name).await {
            return Err(UserError::GroupNotFoundError);
        }
        let mut groups = self.groups.write().await;
        groups.remove(group_name);
        Ok(())
    }

    pub async fn prepare_steward_msg(
        &mut self,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        // Get the group lock
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

    pub async fn build_group_message(
        &mut self,
        msg: Vec<u8>,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        // Get the group lock
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if !group.read().await.is_mls_group_initialized() {
            return Err(UserError::GroupNotFoundError);
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

    /// Build consensus message (proposal/vote) preserving the original AppMessage structure
    pub async fn build_changer_message(
        &mut self,
        app_message: AppMessage,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        // Get the group lock
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if !group.read().await.is_mls_group_initialized() {
            return Err(UserError::GroupNotFoundError);
        }

        // Use the AppMessage directly without wrapping as ConversationMessage
        let msg_to_send = group
            .write()
            .await
            .build_message(&self.provider, self.identity.signer(), &app_message)
            .await?;

        Ok(msg_to_send)
    }

    /// Get the identity string for debugging purposes
    pub fn identity_string(&self) -> String {
        self.identity.identity_string()
    }

    /// Apply proposals for the given group, returning the batch message(s).
    pub async fn apply_proposals(
        &mut self,
        group_name: &str,
    ) -> Result<Vec<WakuMessageToSend>, UserError> {
        if !self.if_group_exists(group_name).await {
            return Err(UserError::GroupNotFoundError);
        }

        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        if !group.read().await.is_mls_group_initialized() {
            return Err(UserError::GroupNotFoundError);
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
    /// Returns the number of proposals that will be voted on.
    pub async fn start_steward_epoch(&mut self, group_name: &str) -> Result<usize, UserError> {
        if !self.if_group_exists(group_name).await {
            return Err(UserError::GroupNotFoundError);
        }

        // Get the group and use centralized state machine logic
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        // Use centralized state machine method that handles all the logic
        let proposal_count = group
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
    /// ## State Transitions:
    /// - Waiting → Voting (if proposals found)
    /// - Waiting → Working (if no proposals found - edge case fix)
    ///
    /// ## Edge Case Handling:
    /// If no proposals are found during voting phase (rare edge case where proposals
    /// disappear between epoch start and voting), transitions back to Working state
    /// to prevent getting stuck in Waiting state.
    pub async fn get_proposals_for_steward_voting(
        &mut self,
        group_name: &str,
    ) -> Result<(u32, UserAction), UserError> {
        info!(
            "[user::get_proposals_for_steward_voting]: Getting proposals for steward voting in group {group_name}"
        );

        // Get the group and use centralized state machine logic
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        // If this is the steward, create proposal with vote and send to group
        if group.read().await.is_steward().await {
            let proposals = group.read().await.get_proposals_for_voting_epoch().await;
            if !proposals.is_empty() {
                // Use centralized state machine method to start voting
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
                        proposals.iter().map(|p| p.to_string()).collect(),
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
                    payload: proposal.payload,
                    group_name: group_name.to_string(),
                }
                .into();

                Ok((proposal.proposal_id, UserAction::SendToApp(voting_proposal)))
            } else {
                // No proposals found during voting phase - this can happen if proposals were removed
                // between start_steward_epoch() and get_proposals_for_steward_voting()
                info!("[user::get_proposals_for_steward_voting]: No proposals found during voting phase, transitioning back to working state");
                group.write().await.start_working().await;
                Ok((0, UserAction::DoNothing))
            }
        } else {
            // Not steward, do nothing
            info!("[user::get_proposals_for_steward_voting]: Not steward, doing nothing");
            Ok((0, UserAction::DoNothing))
        }
    }

    /// Add a remove proposal to the steward for the given group.
    pub async fn add_remove_proposal(
        &mut self,
        group_name: &str,
        identity: String,
    ) -> Result<(), UserError> {
        info!(
            "[user::add_remove_proposal]: Adding remove proposal for user {identity} in group {group_name}"
        );

        if !self.if_group_exists(group_name).await {
            return Err(UserError::GroupNotFoundError);
        }

        // Get the group lock
        let group = {
            let groups = self.groups.read().await;
            groups
                .get(group_name)
                .cloned()
                .ok_or_else(|| UserError::GroupNotFoundError)?
        };

        group.write().await.store_remove_proposal(identity).await?;
        Ok(())
    }

    /// Handle consensus result after it's determined.
    ///
    /// ## State Transitions:
    /// **Steward:**
    /// - Vote YES: Voting → ConsensusReached → Waiting → Working (creates and sends batch proposals, then applies them)
    /// - Vote NO: Voting → Working (discards proposals)
    ///
    /// **Non-Steward:**
    /// - Vote YES: Voting → ConsensusReached → Waiting → Working (waits for consensus + batch proposals, then applies them)
    /// - Vote NO: Voting → Working (no proposals to apply)
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
                            info!("[user::handle_consensus_result]: Invalid action to process");
                            Err(UserError::InvalidAction(action.to_string()))
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

    /// Handle incoming consensus events and return commit messages if needed
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

                // Also check current state as additional safeguard
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

                // Get the group and handle consensus failure
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
                    _ => Err(UserError::InvalidState(current_state.to_string())),
                }
            }
        }
    }

    /// Process incoming consensus proposal
    pub async fn process_consensus_proposal(
        &mut self,
        proposal: Proposal,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        info!(
            "[user::process_consensus_proposal]: Processing consensus proposal {} for group {}",
            proposal.proposal_id, group_name
        );

        // Process the proposal in consensus service (store it without voting)
        self.consensus_service
            .process_incoming_proposal(group_name, proposal.clone())
            .await?;

        // After successful proposal processing, start voting
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
            payload: proposal.payload.clone(),
            group_name: group_name.to_string(),
        }
        .into();

        Ok(UserAction::SendToApp(voting_proposal))
    }

    /// Process user vote from frontend
    pub async fn process_user_vote(
        &mut self,
        proposal_id: u32,
        user_vote: bool,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        // Process the user's vote in consensus service
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
    /// When consensus is reached immediately after processing a vote:
    /// - **Vote YES**: Non-steward transitions to Waiting state to await batch proposals
    /// - **Vote NO**: Non-steward transitions to Working state immediately
    /// - **Steward**: Relies on event-driven system for full proposal management
    async fn process_consensus_vote(
        &mut self,
        vote: Vote,
        group_name: &str,
    ) -> Result<UserAction, UserError> {
        // Process the vote in consensus service
        self.consensus_service
            .process_incoming_vote(group_name, vote.clone())
            .await?;

        Ok(UserAction::DoNothing)
    }

    /// Process incoming ban request
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        let user_to_ban = ban_request.user_to_ban.clone();
        info!(
            "[user::process_ban_request]: Processing ban request for user {user_to_ban} in group {group_name}"
        );

        // Check if this user is the steward for this group
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

            // Create system message directly without going through build_group_message
            let system_message = ConversationMessage {
                message: format!("Remove proposal for user {user_to_ban} added to steward queue")
                    .into_bytes(),
                sender: "SYSTEM".to_string(),
                group_name: group_name.to_string(),
            };

            // Convert to AppMessage and then to WakuMessageToSend
            let app_message: AppMessage = system_message.into();
            let msg_to_send = WakuMessageToSend::new(
                app_message.encode_to_vec(),
                APP_MSG_SUBTOPIC,
                group_name,
                self.identity.identity_string().into(),
            );

            Ok(msg_to_send)
        } else {
            let updated_ban_request = BanRequest {
                user_to_ban: ban_request.user_to_ban,
                requester: self.identity_string(),
                group_name: ban_request.group_name,
            };
            self.build_changer_message(updated_ban_request.into(), group_name)
                .await
        }
    }

    /// Store batch proposals for later processing when state becomes correct
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

    /// Check if there are pending batch proposals for a group
    async fn has_pending_batch_proposals(&self, group_name: &str) -> bool {
        let pending = self.pending_batch_proposals.read().await;
        pending.contains_key(group_name)
    }

    /// Retrieve and remove pending batch proposals for a group
    async fn retrieve_pending_batch_proposals(
        &self,
        group_name: &str,
    ) -> Option<BatchProposalsMessage> {
        let mut pending = self.pending_batch_proposals.write().await;
        pending.remove(group_name)
    }

    /// Process any pending batch proposals that can now be processed
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
