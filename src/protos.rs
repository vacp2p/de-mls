//! Protobuf message definitions, plus constructors for the common
//! request shapes.

/// Re-exported consensus protocol messages.
pub mod hashgraph_like_consensus {
    pub mod v1 {
        pub use ::hashgraph_like_consensus::protos::consensus::v1::*;
    }
}

/// DE-MLS application-level messages.
pub mod de_mls {
    pub mod messages {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/de_mls.messages.v1.rs"));
        }
    }
}

use de_mls::messages::v1::{
    ConversationUpdateRequest, MemberInvite, RemoveMember, StewardElectionProposal,
    conversation_update_request::Payload,
};

impl ConversationUpdateRequest {
    /// `RemoveMember` request targeting `member_id`.
    pub fn remove_member(member_id: impl Into<Vec<u8>>) -> Self {
        Self {
            payload: Some(Payload::RemoveMember(RemoveMember {
                member_id: member_id.into(),
            })),
        }
    }

    /// `MemberInvite` request carrying a joiner's key package.
    pub fn member_invite(invite: MemberInvite) -> Self {
        Self {
            payload: Some(Payload::MemberInvite(invite)),
        }
    }

    /// `StewardElection` request proposing `election`.
    pub fn steward_election(election: StewardElectionProposal) -> Self {
        Self {
            payload: Some(Payload::StewardElection(election)),
        }
    }
}
