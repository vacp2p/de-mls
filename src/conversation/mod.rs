//! Per-conversation state and the [`Conversation`] handle.
//!
//! The leaf modules hold the protocol-state pieces — `queues`
//! (`ConversationQueues` + dedup caches), `state_machine` (state enum +
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
    ConversationConfig, DEFAULT_COMMIT_BATCH_MAX, DEFAULT_COMMIT_BATCH_WINDOW,
    DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_DEDUP_WINDOW, DEFAULT_LIVENESS_CRITERIA_YES,
    DEFAULT_MAX_CONSENSUS_SESSIONS, DEFAULT_PENDING_UPDATE_MAX_EPOCHS, DEFAULT_PROPOSAL_EXPIRATION,
    DEFAULT_RECOVERY_AUTO_COMMIT_DELAY, DEFAULT_RECOVERY_MAX_ROUNDS, DEFAULT_RETRY_WINDOW,
    DEFAULT_UNANSWERED_SYNC_ROUNDS, DEFAULT_VOTING_DELAY,
};
pub use display::{MemberRole, MessageType, message_types};
pub use handle::Conversation;
pub use messaging::Outbound;
pub use state_machine::ConversationState;

pub(crate) use handle::ConversationServices;
pub(crate) use inbound::decode_inbound_payload;
pub(crate) use queues::ConversationQueues;
pub(crate) use state_machine::{ConversationStateMachine, OperatingMode};
pub(crate) use util::{in_flight_target, member_set, self_leave_proposal_id, target_member_id_of};
