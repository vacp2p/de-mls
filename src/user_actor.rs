use kameo::message::{Context, Message};
use waku_bindings::WakuMessage;

use ds::waku_actor::WakuMessageToSend;

use crate::{
    error::UserError,
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
        self.create_group(msg.group_name.clone(), msg.is_creation)
            .await?;
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
        self.prepare_steward_msg(msg.group_name.clone()).await
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
        self.leave_group(msg.group_name.clone()).await?;
        Ok(())
    }
}

pub struct SendGroupMessage {
    pub message: String,
    pub group_name: String,
}

impl Message<SendGroupMessage> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: SendGroupMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.build_group_message(&msg.message, msg.group_name).await
    }
}

// New state machine message types
pub struct StartStewardEpochRequest {
    pub group_name: String,
}

impl Message<StartStewardEpochRequest> for User {
    type Reply = Result<usize, UserError>; // Returns number of proposals

    async fn handle(
        &mut self,
        msg: StartStewardEpochRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.start_steward_epoch(msg.group_name).await
    }
}

pub struct StartVotingRequest {
    pub group_name: String,
}

impl Message<StartVotingRequest> for User {
    type Reply = Result<(u32, UserAction), UserError>; // Returns proposal_id

    async fn handle(
        &mut self,
        msg: StartVotingRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.start_voting(msg.group_name).await
    }
}

pub struct CompleteVotingRequest {
    pub group_name: String,
    pub proposal_id: u32,
}

impl Message<CompleteVotingRequest> for User {
    type Reply = Result<Vec<WakuMessageToSend>, UserError>; // Returns vote result

    async fn handle(
        &mut self,
        msg: CompleteVotingRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.complete_voting_for_steward(msg.group_name, msg.proposal_id)
            .await
    }
}

pub struct UserVoteRequest {
    pub group_name: String,
    pub proposal_id: u32,
    pub vote: bool,
}

impl Message<UserVoteRequest> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: UserVoteRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        let action = self
            .process_user_vote(msg.proposal_id, msg.vote, msg.group_name)
            .await?;
        match action {
            UserAction::SendToWaku(waku_msg) => Ok(waku_msg),
            _ => Err(UserError::InvalidUserAction(
                "Vote action must result in Waku message".to_string(),
            )),
        }
    }
}
