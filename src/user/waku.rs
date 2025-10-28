use prost::Message;
use tracing::{debug, error, info};
use waku_bindings::WakuMessage;

use crate::error::UserError;
use crate::group::GroupAction;
use crate::protos::de_mls::messages::v1::{app_message, welcome_message};
use crate::protos::de_mls::messages::v1::{
    AppMessage, ConversationMessage, UserKeyPackage, WelcomeMessage,
};
use crate::user::{User, UserAction};
use ds::waku_actor::WakuMessageToSend;
use ds::{APP_MSG_SUBTOPIC, WELCOME_SUBTOPIC};
use openmls::prelude::{DeserializeBytes, MlsMessageBodyIn, MlsMessageIn};

impl User {
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
        let group = self.group_ref(group_name).await?;

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
                            let app_msg = ConversationMessage {
                                message: format!(
                                    "User {} joined to the group",
                                    self.identity.identity_string()
                                )
                                .as_bytes()
                                .to_vec(),
                                sender: "SYSTEM".to_string(),
                                group_name: group_name.to_string(),
                            }
                            .into();
                            let msg = self.build_group_message(app_msg, group_name).await?;
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
        let group = self.group_ref(group_name).await?;

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

        let group = self.group_ref(group_name).await?;
        let res = group
            .write()
            .await
            .process_protocol_msg(mls_message, &self.provider)
            .await?;

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
        let group = self.group_ref(&group_name).await?;
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
}
