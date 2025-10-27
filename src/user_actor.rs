use kameo::message::{Context, Message};
use waku_bindings::WakuMessage;

use ds::waku_actor::WakuMessageToSend;

use crate::{
    consensus::ConsensusEvent,
    error::UserError,
    protos::de_mls::messages::v1::BanRequest,
    user::{User, UserAction},
};

impl Message<WakuMessage> for User {
    type Reply = Result<UserAction, UserError>;

    async fn handle(
        &mut self,
        msg: WakuMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.process_waku_message(msg).await
    }
}

pub struct CreateGroupRequest {
    pub group_name: String,
    pub is_creation: bool,
}

impl Message<CreateGroupRequest> for User {
    type Reply = Result<(), UserError>;

    async fn handle(
        &mut self,
        msg: CreateGroupRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.create_group(&msg.group_name, msg.is_creation).await?;
        Ok(())
    }
}

pub struct StewardMessageRequest {
    pub group_name: String,
}

impl Message<StewardMessageRequest> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: StewardMessageRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.prepare_steward_msg(&msg.group_name).await
    }
}

pub struct LeaveGroupRequest {
    pub group_name: String,
}

impl Message<LeaveGroupRequest> for User {
    type Reply = Result<(), UserError>;

    async fn handle(
        &mut self,
        msg: LeaveGroupRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.leave_group(&msg.group_name).await?;
        Ok(())
    }
}

pub struct SendGroupMessage {
    pub message: Vec<u8>,
    pub group_name: String,
}

impl Message<SendGroupMessage> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: SendGroupMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.build_group_message(msg.message, &msg.group_name).await
    }
}

pub struct BuildBanMessage {
    pub ban_request: BanRequest,
    pub group_name: String,
}

impl Message<BuildBanMessage> for User {
    type Reply = Result<UserAction, UserError>;

    async fn handle(
        &mut self,
        msg: BuildBanMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.process_ban_request(msg.ban_request, &msg.group_name)
            .await
    }
}

// New state machine message types
pub struct StartStewardEpochRequest {
    pub group_name: String,
}

pub struct GetGroupMembersRequest {
    pub group_name: String,
}

impl Message<GetGroupMembersRequest> for User {
    type Reply = Result<Vec<String>, UserError>;

    async fn handle(
        &mut self,
        msg: GetGroupMembersRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.get_group_members(&msg.group_name).await
    }
}

impl Message<StartStewardEpochRequest> for User {
    type Reply = Result<usize, UserError>; // Returns number of proposals

    async fn handle(
        &mut self,
        msg: StartStewardEpochRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.start_steward_epoch(&msg.group_name).await
    }
}

pub struct GetProposalsForStewardVotingRequest {
    pub group_name: String,
}

impl Message<GetProposalsForStewardVotingRequest> for User {
    type Reply = Result<UserAction, UserError>; // Returns proposal_id

    async fn handle(
        &mut self,
        msg: GetProposalsForStewardVotingRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let (_, action) = self
            .get_proposals_for_steward_voting(&msg.group_name)
            .await?;
        Ok(action)
    }
}

pub struct UserVoteRequest {
    pub group_name: String,
    pub proposal_id: u32,
    pub vote: bool,
}

impl Message<UserVoteRequest> for User {
    type Reply = Result<Option<WakuMessageToSend>, UserError>;

    async fn handle(
        &mut self,
        msg: UserVoteRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let action = self
            .process_user_vote(msg.proposal_id, msg.vote, &msg.group_name)
            .await?;
        match action {
            UserAction::SendToWaku(waku_msg) => Ok(Some(waku_msg)),
            UserAction::DoNothing => Ok(None),
            _ => Err(UserError::InvalidUserAction(
                "Vote action must result in Waku message".to_string(),
            )),
        }
    }
}

// Consensus event message handler
pub struct ConsensusEventMessage {
    pub group_name: String,
    pub event: ConsensusEvent,
}

impl Message<ConsensusEventMessage> for User {
    type Reply = Result<Vec<WakuMessageToSend>, UserError>;

    async fn handle(
        &mut self,
        msg: ConsensusEventMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_consensus_event(&msg.group_name, msg.event)
            .await
    }
}

pub struct IsStewardStatusRequest {
    pub group_name: String,
}

impl Message<IsStewardStatusRequest> for User {
    type Reply = Result<bool, UserError>;

    async fn handle(
        &mut self,
        msg: IsStewardStatusRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let is_steward = self.is_user_steward_for_group(&msg.group_name).await?;
        Ok(is_steward)
    }
}

pub struct IsMLSGroupInitializedRequest {
    pub group_name: String,
}

impl Message<IsMLSGroupInitializedRequest> for User {
    type Reply = Result<bool, UserError>;

    async fn handle(
        &mut self,
        msg: IsMLSGroupInitializedRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let is_initialized = self
            .is_user_mls_group_initialized_for_group(&msg.group_name)
            .await?;
        Ok(is_initialized)
    }
}

pub struct GetCurrentEpochProposalsRequest {
    pub group_name: String,
}

impl Message<GetCurrentEpochProposalsRequest> for User {
    type Reply = Result<Vec<crate::steward::GroupUpdateRequest>, UserError>;

    async fn handle(
        &mut self,
        msg: GetCurrentEpochProposalsRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let proposals = self.get_current_epoch_proposals(&msg.group_name).await?;
        Ok(proposals)
    }
}
