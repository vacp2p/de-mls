use alloy::{
    primitives::Address,
    signers::{local::PrivateKeySigner, Signer},
};
use tracing::info;

use crate::{
    error::UserError,
    protos::de_mls::messages::v1::{AppMessage, BanRequest, ProposalAdded},
    user::{User, UserAction},
    LocalSigner,
};
use ds::waku_actor::WakuMessageToSend;
use mls_crypto::normalize_wallet_address_str;

impl User {
    /// Prepare data to build a waku message for a group.
    ///
    /// ## Parameters:
    /// - `app_message`: The application message to send to the group
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
    pub async fn build_group_message(
        &mut self,
        app_message: AppMessage,
        group_name: &str,
    ) -> Result<WakuMessageToSend, UserError> {
        let group = self.group_ref(group_name).await?;
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
        let normalized_user_to_ban = normalize_wallet_address_str(&ban_request.user_to_ban)?;
        info!("[process_ban_request]: Processing ban request for user {normalized_user_to_ban} in group {group_name}");

        let group = self.group_ref(group_name).await?;
        let is_steward = group.read().await.is_steward().await;
        if is_steward {
            // Steward: add the remove proposal to the queue
            info!(
                "[process_ban_request]: Steward adding remove proposal for user {normalized_user_to_ban}"
            );
            let request = self
                .add_remove_proposal(group_name, normalized_user_to_ban.clone())
                .await?;

            // Send notification to UI about the new proposal
            let proposal_added_msg: AppMessage = ProposalAdded {
                group_id: group_name.to_string(),
                request: request.into(),
            }
            .into();

            Ok(UserAction::SendToApp(proposal_added_msg))
        } else {
            // Regular user: send the ban request to the group
            let updated_ban_request = BanRequest {
                user_to_ban: normalized_user_to_ban,
                requester: self.identity_string(),
                group_name: ban_request.group_name,
            };
            let msg = self
                .build_group_message(updated_ban_request.into(), group_name)
                .await?;
            Ok(UserAction::SendToWaku(msg))
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
