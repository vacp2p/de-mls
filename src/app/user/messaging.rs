//! Send operations (key packages, app messages, ban requests).

use crate::{
    app::{GroupState, StateChangeHandler, User, UserError},
    core::{DeMlsProvider, GroupEventHandler, build_key_package_message, build_message},
    mls_crypto::{MlsService, parse_wallet_to_bytes},
    protos::de_mls::messages::v1::{
        AppMessage, BanRequest, ConversationMessage, GroupUpdateRequest, RemoveMember,
        group_update_request,
    },
};

impl<
    P: DeMlsProvider,
    M: MlsService,
    H: GroupEventHandler + 'static,
    SCH: StateChangeHandler + 'static,
> User<P, M, H, SCH>
where
    M::Identity: Clone,
{
    /// Broadcast our key-package on the welcome subtopic so the steward
    /// can invite us.
    pub async fn send_kp_message(&self, group_name: &str) -> Result<(), UserError> {
        // Existence-check on the group; the packet itself is built from
        // `group_name` and a freshly-generated KP, neither of which need
        // the entry guard.
        let _ = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let key_package = self.generate_key_package()?;
        let packet = build_key_package_message(group_name, key_package, &self.app_id);
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
        let entry_arc = self
            .lookup_entry(group_name)
            .await
            .ok_or(UserError::GroupNotFound)?;
        let packet = {
            let entry = entry_arc.read().await;
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

            let mls = entry.group.mls().ok_or(UserError::MlsNotInitialized)?;
            build_message(mls, &app_msg, &self.app_id)?
        };
        self.handler.on_outbound(group_name, packet).await?;
        Ok(())
    }

    /// Start a `RemoveMember` consensus round targeting `ban_request.user_to_ban`.
    /// The requester's click means "I want this person removed" → the
    /// creator's vote is bundled as YES at submit; no banner is shown to
    /// the requester.
    pub async fn process_ban_request(
        &mut self,
        ban_request: BanRequest,
        group_name: &str,
    ) -> Result<(), UserError> {
        {
            let entry_arc = self
                .lookup_entry(group_name)
                .await
                .ok_or(UserError::GroupNotFound)?;
            let entry = entry_arc.read().await;
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
            Some(true),
        )
        .await?;

        Ok(())
    }
}
