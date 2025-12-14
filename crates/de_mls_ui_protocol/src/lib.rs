//! UI <-> Gateway protocol (PoC)
// crates/de_mls_ui_protocol/src/lib.rs
pub mod v1 {
    use de_mls::{
        message::MessageType,
        protos::{
            consensus::v1::{ProposalResult, VotePayload},
            de_mls::messages::v1::{BanRequest, ConversationMessage, ProposalAdded},
        },
    };
    use mls_crypto::identity::normalize_wallet_address;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum AppCmd {
        Login {
            private_key: String,
        },
        ListGroups,
        CreateGroup {
            name: String,
        },
        JoinGroup {
            name: String,
        },
        EnterGroup {
            group_id: String,
        },
        SendMessage {
            group_id: String,
            body: String,
        },
        LoadHistory {
            group_id: String,
        },
        Vote {
            group_id: String,
            proposal_id: u32,
            choice: bool,
        },
        LeaveGroup {
            group_id: String,
        },
        GetStewardStatus {
            group_id: String,
        },
        GetCurrentEpochProposals {
            group_id: String,
        },
        SendBanRequest {
            group_id: String,
            user_to_ban: String,
        },
        GetGroupMembers {
            group_id: String,
        },
    }

    #[derive(Debug, Clone)]
    #[non_exhaustive]
    pub enum AppEvent {
        LoggedIn(String),
        Groups(Vec<String>),
        GroupCreated(String),
        GroupRemoved(String),
        EnteredGroup {
            group_id: String,
        },
        ChatMessage(ConversationMessage),
        LeaveGroup {
            group_id: String,
        },

        StewardStatus {
            group_id: String,
            is_steward: bool,
        },

        VoteRequested(VotePayload),
        ProposalDecided(ProposalResult),
        CurrentEpochProposals {
            group_id: String,
            proposals: Vec<(String, String)>,
        },
        ProposalAdded {
            group_id: String,
            action: String,
            address: String,
        },
        CurrentEpochProposalsCleared {
            group_id: String,
        },
        GroupMembers {
            group_id: String,
            members: Vec<String>,
        },
        Error(String),
    }

    impl From<ProposalAdded> for AppEvent {
        fn from(proposal_added: ProposalAdded) -> Self {
            AppEvent::ProposalAdded {
                group_id: proposal_added.group_id.clone(),
                action: proposal_added
                    .request
                    .as_ref()
                    .unwrap()
                    .message_type()
                    .to_string(),
                address: normalize_wallet_address(
                    &proposal_added.request.as_ref().unwrap().wallet_address,
                ),
            }
        }
    }

    impl From<BanRequest> for AppEvent {
        fn from(ban_request: BanRequest) -> Self {
            AppEvent::ProposalAdded {
                group_id: ban_request.group_name.clone(),
                action: "Remove Member".to_string(),
                address: ban_request.user_to_ban.clone(),
            }
        }
    }
}
