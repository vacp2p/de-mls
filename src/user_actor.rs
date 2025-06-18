use kameo::message::{Context, Message};
use openmls::prelude::{hash_ref::ProposalRef, KeyPackage};
use waku_bindings::WakuMessage;

use ds::waku_actor::WakuMessageToSend;

use crate::{
    steward::GroupUpdateRequest,
    user::{User, UserAction},
    UserError,
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

pub struct RemoveUserRequest {
    pub user_to_ban: String,
    pub group_name: String,
}

impl Message<RemoveUserRequest> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: RemoveUserRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.remove_group_users(vec![msg.user_to_ban], msg.group_name.clone())
            .await
    }
}

pub struct GetGroupUpdateRequest {
    pub group_name: String,
}

impl Message<GetGroupUpdateRequest> for User {
    type Reply = Result<Vec<GroupUpdateRequest>, UserError>;

    async fn handle(
        &mut self,
        msg: GetGroupUpdateRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.group_drain_pending_proposals(msg.group_name.clone())
            .await
    }
}

pub struct GetProposalsHrefRequest {
    pub group_name: String,
}

impl Message<GetProposalsHrefRequest> for User {
    type Reply = Result<Vec<GroupUpdateRequest>, UserError>;

    async fn handle(
        &mut self,
        msg: GetProposalsHrefRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.group_drain_pending_proposals(msg.group_name.clone())
            .await
    }
}

pub struct ProcessProposalsRequest {
    pub group_name: String,
    pub proposals: Vec<GroupUpdateRequest>,
}

impl Message<ProcessProposalsRequest> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: ProcessProposalsRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.process_proposals(msg.group_name.clone(), msg.proposals.clone())
            .await
    }
}

pub struct ApplyProposalsRequest {
    pub group_name: String,
}

impl Message<ApplyProposalsRequest> for User {
    type Reply = Result<Vec<WakuMessageToSend>, UserError>;

    async fn handle(
        &mut self,
        msg: ApplyProposalsRequest,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.apply_proposals(msg.group_name.clone()).await
    }
}

pub struct SendGroupMessage {
    pub msg: String,
    pub group_name: String,
}

impl Message<SendGroupMessage> for User {
    type Reply = Result<WakuMessageToSend, UserError>;

    async fn handle(
        &mut self,
        msg: SendGroupMessage,
        _ctx: Context<'_, Self, Self::Reply>,
    ) -> Self::Reply {
        self.build_group_message(&msg.msg, msg.group_name.clone())
            .await
    }
}
