//! UI <-> Gateway protocol (PoC). Keep it dependency-light (serde only).
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
    pub struct VotePayload {
        pub group_id: String,
        pub message: String,
        pub timeout_ms: u64,
        pub vote_id: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum AppCmd {
        Login { private_key: String },
        ListGroups,
        CreateGroup { name: String },
        JoinGroup { name: String },
        EnterGroup { group_id: String },
        SendMessage { group_id: String, body: String },
        LoadHistory { group_id: String },
        Vote { vote_id: String, choice: bool },
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum AppEvent {
        LoggedIn(String),
        Groups(Vec<String>),
        EnteredGroup { group_id: String },
        ChatMessage(ChatMsg),
        VoteRequested(VotePayload),
        VoteClosed { vote_id: String },
        Error(String),
    }
}
