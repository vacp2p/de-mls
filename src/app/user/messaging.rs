//! Send operations (key packages, app messages, ban requests).

use crate::{
    app::{GroupState, StateChangeHandler, User, UserError},
    core::{DeMlsProvider, GroupEventHandler, build_key_package_message, build_message},
    mls_crypto::parse_wallet_to_bytes,
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, ConversationMessage, GroupUpdateRequest, RemoveMember,
        group_update_request,
    },
};

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Broadcast our key-package on the welcome subtopic so the steward
    /// can invite us.
    pub async fn send_kp_message(&self, group_name: &str) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let packet = build_key_package_message(&entry.group, &self.mls_service, &self.app_id)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Send a chat message. Blocked in `PendingJoin` (no keys yet),
    /// `Freezing`, and `Selection` (epoch rotation in flight — the message
    /// might not decrypt on peers who have already merged the next commit).
    /// Governance traffic has its own gate (`check_proposal_allowed`).
    pub async fn send_app_message(
        &self,
        group_name: &str,
        message: Vec<u8>,
    ) -> Result<(), UserError> {
        let packet = {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;

            let state = entry.state_machine.current_state();
            if matches!(
                state,
                GroupState::PendingJoin | GroupState::Freezing | GroupState::Selection
            ) {
                return Err(UserError::GroupBlocked(state.to_string()));
            }

            let app_msg: AppMessage = ConversationMessage {
                message,
                sender: self.identity_string(),
                group_name: group_name.to_string(),
            }
            .into();

            build_message(&entry.group, &self.mls_service, &app_msg, &self.app_id)?
        };
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Start a `RemoveMember` consensus round targeting `ban_request.user_to_ban`.
    /// `creator_vote` is the submitter's own vote on their request (bundled
    /// with the outbound proposal); the UI collects it from the submit modal.
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
        creator_vote: bool,
    ) -> Result<(), UserError> {
        {
            let groups = self.groups.read().await;
            let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
            let state = entry.state_machine.current_state();
            if state != GroupState::Working {
                return Err(UserError::GroupBlocked(state.to_string()));
            }
        }

        self.initiate_proposal(
            group_name.to_string(),
            GroupUpdateRequest {
                payload: Some(group_update_request::Payload::RemoveMember(RemoveMember {
                    identity: parse_wallet_to_bytes(ban_request.user_to_ban.as_str())?,
                })),
            },
            creator_vote,
        )
        .await?;

        Ok(())
    }
}
