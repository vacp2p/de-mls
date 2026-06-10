//! Reference session layer — wires [`crate::core`] into a working chat app
//! with consensus, state machine, peer scoring, and freeze/commit timing.
//!
//! The unit of integration is the per-conversation [`SessionRunner`](crate::session::SessionRunner): drive it
//! from a transport receive loop (feeding inbound packets) and a periodic poll
//! on each conversation's freeze/commit deadlines, draining outbound payloads
//! and session events as you go.
//!
//! [`crate::core::ConversationStateMachine`] owns the per-conversation state
//! enum (`PendingJoin → Working → Freezing → Selection → Reelection`);
//! `PhaseTimer` owns the wall-clock anchor; [`SessionRunner`](crate::session::SessionRunner) composes a
//! [`crate::core::Conversation`] with that timer through coordinator methods
//! that update both atomically. State transitions return the new
//! [`crate::core::ConversationState`]; the session-side dispatcher emits a
//! `SessionEvent::PhaseChange` on each one.
//!
//! Use this layer directly for epoch-based steward chat; write a custom session
//! layer if you need a different consensus model, state machine, or epoch
//! timing.
//!
//! [`SessionRunner`](crate::session::SessionRunner) is defined in the `runner` module; per-conversation method
//! bodies (proposal submission, voting, inbound dispatch, freeze ticks, steward
//! housekeeping, query getters) live in sibling modules and extend
//! `SessionRunner` via additional `impl` blocks.

mod consensus;
mod consensus_bridge;
mod consensus_context;
mod consensus_events;
mod construct;
mod display;
mod error;
mod freeze;
mod inbound;
mod messaging;
mod phase_timer;
mod poll;
mod query;
mod runner;
mod steward;
mod tick;

pub use crate::core::{
    ConversationConfig, ConversationState, DEFAULT_COMMIT_INACTIVITY_DURATION,
    DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_MAX_RETRIES,
    DEFAULT_PEER_SCORE, DEFAULT_PROPOSAL_EXPIRATION, DEFAULT_RECOVERY_INACTIVITY_DURATION,
    DEFAULT_THRESHOLD_PEER_SCORE, DEFAULT_VOTING_DELAY,
};
pub use consensus::CreatorVote;
pub use consensus_context::ConsensusContext;
pub use construct::ConversationDeps;
pub use display::{MemberRole, MessageType, message_types};
pub use error::SessionError;
pub use inbound::DispatchOutcome;
pub use messaging::{Outbound, build_key_package_announcement};
pub use poll::PollOutcome;
pub use runner::LeaveOutcome;
pub use runner::SessionRunner;
pub use tick::SessionTick;

pub(crate) use phase_timer::{FreezeTimeoutStatus, PhaseTimer};
