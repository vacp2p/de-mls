//! UI <-> Gateway protocol (PoC). Keep it dependency-light (serde only).
// crates/de_mls_ui_protocol/src/lib.rs
pub mod v1 {
    use de_mls::protos::messages::v1::{
        consensus::v1::{ProposalResult, VotePayload},
        ConversationMessage,
    };
    use serde::{Deserialize, Serialize};

    // #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    // pub struct ChatMsg {
    //     pub id: String,
    //     pub group_id: String,
    //     pub author: String,
    //     pub body: String,
    //     pub ts_ms: i64,
    // }

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
        QuerySteward {
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
        EnteredGroup { group_id: String },
        ChatMessage(ConversationMessage),
        LeaveGroup { group_id: String },

        StewardStatus { group_id: String, is_steward: bool },

        VoteRequested(VotePayload),
        ProposalDecided(ProposalResult),
        Error(String),
    }
}
