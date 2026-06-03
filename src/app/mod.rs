//! Reference application layer — wires [`crate::core`] into a working chat
//! app with consensus, state machine, peer scoring, and freeze/commit timing.
//!
//! Integrate by constructing a [`crate::app::User`] with your
//! `SessionEvent` / `ConversationLifecycle`, then drive it from a transport
//! receive loop ([`crate::app::User::process_inbound_packet`]) and a
//! periodic poll on each conversation's
//! [`crate::app::SessionRunner::check_member_freeze`] and
//! [`crate::app::SessionRunner::poll_freeze_status`].
//!
//! [`crate::core::ConversationStateMachine`] owns the per-conversation state
//! enum (`PendingJoin → Working → Freezing → Selection → Reelection`);
//! [`crate::app::PhaseTimer`] owns the wall-clock anchor;
//! [`crate::app::SessionRunner`] composes a [`crate::core::Conversation`]
//! with that timer through coordinator methods that update both atomically.
//! State transitions return the new [`crate::core::ConversationState`]; the
//! session-side dispatcher emits a `SessionEvent::PhaseChange` on each one.
//!
//! Use this layer directly for epoch-based steward chat; write a custom app
//! layer if you need a different consensus model, state machine, or epoch
//! timing.

mod display;
mod error;
mod phase_timer;
mod session;
mod user;

pub use crate::core::{
    ConversationConfig, ConversationState, DEFAULT_COMMIT_INACTIVITY_DURATION,
    DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_MAX_RETRIES,
    DEFAULT_PEER_SCORE, DEFAULT_PROPOSAL_EXPIRATION, DEFAULT_RECOVERY_INACTIVITY_DURATION,
    DEFAULT_THRESHOLD_PEER_SCORE, DEFAULT_VOTING_DELAY,
};
pub use display::{MemberRole, MessageType, message_types};
pub use error::UserError;
pub use phase_timer::{FreezeTimeoutStatus, PhaseTimer};
pub(crate) use session::LockExt;
pub use session::{
    CreatorVote, DispatchOutcome, PendingJoinTick, SessionRunner, SessionTick,
    build_key_package_packet,
};
pub use user::{ConsensusContext, SessionEntry, User, UserPlugins};
