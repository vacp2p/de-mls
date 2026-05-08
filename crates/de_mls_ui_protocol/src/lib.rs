//! UI <-> Gateway protocol (PoC)
pub mod v1 {
    use hashgraph_like_consensus::types::ConsensusEvent;
    use serde::{Deserialize, Serialize};

    use de_mls::{
        app::{MessageType, format_group_request_target},
        protos::de_mls::messages::v1::{
            BanRequest, ConversationMessage, ProposalAdded, VotePayload,
        },
    };

    /// Information about a group member, including their peer score and steward role.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    pub struct MemberInfo {
        /// Formatted wallet address (e.g. `0x...`).
        pub address: String,
        /// Peer reputation score (default: 100 for new members).
        pub score: i64,
        /// Steward role: "epoch_steward", "backup_steward", "steward", or "member".
        pub role: String,
        /// `true` if this member has broadcast a self-leave request that
        /// hasn't been committed yet. UI should show a "leaving next epoch"
        /// badge and suppress actions targeting them.
        #[serde(default)]
        pub pending_leave: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[non_exhaustive]
    pub enum AppCmd {
        Login {
            private_key: String,
        },
        ListGroups,
        CreateGroup {
            conversation_id: String,
        },
        JoinGroup {
            conversation_id: String,
        },
        EnterGroup {
            conversation_id: String,
        },
        SendMessage {
            conversation_id: String,
            body: String,
        },
        LoadHistory {
            conversation_id: String,
        },
        Vote {
            conversation_id: String,
            proposal_id: u32,
            choice: bool,
        },
        LeaveConversation {
            conversation_id: String,
        },
        GetStewardStatus {
            conversation_id: String,
        },
        GetGroupState {
            conversation_id: String,
        },
        GetCurrentEpochProposals {
            conversation_id: String,
        },
        SendBanRequest {
            conversation_id: String,
            user_to_ban: String,
        },
        GetGroupMembers {
            conversation_id: String,
        },
        GetEpochHistory {
            conversation_id: String,
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
            conversation_id: String,
        },
        ChatMessage(ConversationMessage),
        LeaveConversation {
            conversation_id: String,
        },

        StewardStatus {
            conversation_id: String,
            is_steward: bool,
        },

        GroupStateChanged {
            conversation_id: String,
            state: String,
        },
        /// Current MLS epoch + reelection retry round. Pushed alongside
        /// other consensus-state refreshes so the UI can display live
        /// epoch/retry for debugging.
        GroupEpoch {
            conversation_id: String,
            epoch: u64,
            retry_round: u32,
        },

        VoteRequested(VotePayload),
        /// Our own proposal was just submitted to the group. The creator's
        /// vote is already bundled in the outbound wire message, so the UI
        /// records the proposal for history but must not offer a "please
        /// vote" banner for it.
        OwnProposalSubmitted {
            conversation_id: String,
            proposal_id: u32,
            action: String,
            address: String,
        },
        ProposalDecided(String, ConsensusEvent),
        CurrentEpochProposals {
            conversation_id: String,
            proposals: Vec<(String, String)>,
        },
        ProposalAdded {
            conversation_id: String,
            action: String,
            address: String,
        },
        CurrentEpochProposalsCleared {
            conversation_id: String,
        },
        GroupMembers {
            conversation_id: String,
            members: Vec<MemberInfo>,
        },
        FreezeCandidates {
            conversation_id: String,
            received: usize,
            expected: usize,
        },
        EpochHistory {
            conversation_id: String,
            epochs: Vec<Vec<(String, String)>>,
        },
        Error(String),
    }

    impl From<ProposalAdded> for AppEvent {
        fn from(proposal_added: ProposalAdded) -> Self {
            let request = proposal_added.request.unwrap();
            let address = format_group_request_target(&request);
            AppEvent::ProposalAdded {
                conversation_id: proposal_added.conversation_id.clone(),
                action: request.message_type().to_string(),
                address,
            }
        }
    }

    impl From<BanRequest> for AppEvent {
        fn from(ban_request: BanRequest) -> Self {
            AppEvent::ProposalAdded {
                conversation_id: ban_request.conversation_name.clone(),
                action: "Remove Member".to_string(),
                address: ban_request.user_to_ban.clone(),
            }
        }
    }
}
