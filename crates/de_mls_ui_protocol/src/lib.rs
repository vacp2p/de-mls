//! UI <-> Gateway protocol (PoC). Keep it dependency-light (serde only).
// crates/de_mls_ui_protocol/src/lib.rs
pub mod v1 {
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ChatMsg {
        pub id: String,
        pub group_id: String,
        pub author: String,
        pub body: String,
        pub ts_ms: i64,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum ProposalKind {
        AddMember,
        RemoveMember,
        AddSteward,
        RemoveSteward,
        UpdateEpoch,
        Custom,  // fallback / proto we don’t model yet
        Unknown, // parsing failed
    }

    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VotePayload {
        pub group_id: String,
        pub message: String,
        pub timeout_ms: u64,
        pub proposal_id: u32,
    }

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

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum AppEvent {
        LoggedIn(String),
        Groups(Vec<String>),
        GroupCreated(String),
        GroupRemoved(String),
        EnteredGroup { group_id: String },
        ChatMessage(ChatMsg),
        VoteRequested(VotePayload),
        VoteClosed { proposal_id: u32 },
        LeaveGroup { group_id: String },
        StewardStatus { group_id: String, is_steward: bool },
        Error(String),
    }
}
