//! Send operations (key packages, app messages, ban requests).

use super::*;

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    /// Build and send a key package message for a group via the handler.
    pub async fn send_kp_message(&self, group_name: &str) -> Result<(), UserError> {
        let groups = self.groups.read().await;
        let entry = groups.get(group_name).ok_or(UserError::GroupNotFound)?;
        let packet =
            core::build_key_package_message(&entry.group, &self.mls_service, &self.app_id)?;
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Send a conversation message to a group.
    ///
    /// Blocked during states where MLS epoch keys are about to rotate:
    /// - `PendingJoin` — we don't have the group keys yet.
    /// - `Freezing` — a steward is building a commit candidate; messages
    ///   sent now may not decrypt on members who've already merged a commit.
    /// - `Selection` — a winning commit is being merged; same reason.
    ///
    /// Governance traffic (votes, proposal notifications) lives above MLS
    /// and is permitted in any state where its own check allows it — see
    /// `check_proposal_allowed` / `process_user_vote`.
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

            core::build_message(&entry.group, &self.mls_service, &app_msg, &self.app_id)?
        };
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Process a ban request.
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
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
        )
        .await?;

        Ok(())
    }
}
