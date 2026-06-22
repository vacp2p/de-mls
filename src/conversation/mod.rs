//! Per-conversation state and the [`Conversation`] handle.
//!
//! The leaf modules hold the protocol-state pieces — `queues`
//! ([`ConversationQueues`] + dedup caches), `state_machine` (state enum +
//! transitions), `config` (durable timing/protocol config), and `util`
//! (member-set helpers). `handle` defines the [`Conversation`] struct, and
//! its sibling modules (`construct`, `poll`, `steward`, `messaging`, `query`,
//! `display`, `inbound`) extend it with method bodies for proposal
//! submission, voting, inbound dispatch, freeze ticks, steward housekeeping,
//! and query getters.

mod config;
mod construct;
mod display;
mod handle;
mod inbound;
mod messaging;
mod poll;
mod query;
mod queues;
mod state_machine;
mod steward;
mod util;

pub use config::{
    ConversationConfig, DEFAULT_COMMIT_INACTIVITY_DURATION, DEFAULT_CONSENSUS_TIMEOUT,
    DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_LIVENESS_CRITERIA_YES,
    DEFAULT_PENDING_UPDATE_MAX_EPOCHS, DEFAULT_PROPOSAL_EXPIRATION,
    DEFAULT_RECOVERY_INACTIVITY_DURATION, DEFAULT_VOTING_DELAY,
};
pub use display::{MemberRole, MessageType, message_types};
pub(crate) use handle::ConversationServices;
pub use handle::{Conversation, LeaveOutcome};
pub use inbound::{DispatchOutcome, decode_inbound_payload};
pub use messaging::{Outbound, build_key_package_announcement};
pub use poll::PollOutcome;
pub use queues::{BufferedCommitCandidate, ConversationQueues, FreezeBufferOutcome, ProposalId};
pub use state_machine::{ConversationState, ConversationStateMachine, OperatingMode};
pub use util::{member_set, self_leave_proposal_id, target_member_id_of};
