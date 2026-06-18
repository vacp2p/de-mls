//! Reference session layer — wires [`crate::core`] into a working chat app
//! with consensus, state machine, peer scoring, and freeze/commit timing.
//!
//! The unit of integration is [`Conversation`](crate::session::Conversation): drive it
//! from a transport receive loop (feeding inbound packets) and a periodic poll
//! on each conversation's freeze/commit deadlines, draining outbound payloads
//! and conversation events as you go.
//!
//! [`crate::core::ConversationStateMachine`] owns the per-conversation state
//! enum (`PendingJoin → Working → Freezing → Selection → Reelection`);
//! `PhaseTimer` owns the wall-clock anchor; [`Conversation`](crate::session::Conversation)
//! holds the state machine and that timer, driving both through coordinator
//! methods that update them atomically. State transitions return the new
//! [`crate::core::ConversationState`]; the session-side dispatcher emits a
//! `ConversationEvent::PhaseChange` on each one.
//!
//! Use this layer directly for epoch-based steward chat; write a custom session
//! layer if you need a different consensus model, state machine, or epoch
//! timing.
//!
//! [`Conversation`](crate::session::Conversation) is defined in the `conversation` module; per-conversation method
//! bodies (proposal submission, voting, inbound dispatch, freeze ticks, steward
//! housekeeping, query getters) live in sibling modules and extend
//! `Conversation` via additional `impl` blocks.

mod consensus;
mod construct;
mod conversation;
mod display;
mod error;
mod inbound;
mod messaging;
mod phase_timer;
mod poll;
mod query;
mod steward;

pub use crate::core::{
    ConversationConfig, ConversationState, DEFAULT_COMMIT_INACTIVITY_DURATION,
    DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_MAX_RETRIES,
    DEFAULT_PEER_SCORE, DEFAULT_PROPOSAL_EXPIRATION, DEFAULT_RECOVERY_INACTIVITY_DURATION,
    DEFAULT_THRESHOLD_PEER_SCORE, DEFAULT_VOTING_DELAY,
};
pub use consensus::CreatorVote;
pub use construct::ConversationDeps;
pub use conversation::{Conversation, LeaveOutcome};
pub use display::{MemberRole, MessageType, message_types};
pub use error::ConversationError;
pub use inbound::DispatchOutcome;
pub use messaging::{Outbound, build_key_package_announcement};
pub use poll::PollOutcome;

pub(crate) use phase_timer::PhaseTimer;
