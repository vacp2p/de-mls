//! Reference application layer — wires [`crate::core`] into a working chat
//! app with consensus, state machine, peer scoring, and freeze/commit timing.
//!
//! Integrate by constructing a [`crate::app::User`] with your
//! `SessionEvent` / `ConversationLifecycle`, then drive it from a transport
//! receive loop ([`crate::app::User::process_inbound_packet`]) and a
//! periodic poll ([`crate::app::User::check_member_freeze`],
//! [`crate::app::User::poll_freeze_status`]).
//!
//! [`crate::core::ConversationStateMachine`] owns the per-conversation state
//! enum (`PendingJoin → Working → Freezing → Selection → Reelection`);
//! [`crate::app::PhaseTimer`] owns the wall-clock anchor;
//! [`crate::app::SessionRunner`] composes a [`crate::core::ConversationHandle`]
//! with that timer through coordinator methods that update both atomically.
//! State transitions return the new [`crate::core::ConversationState`]; the
//! orchestrator dispatches it via
//! `SessionEvent::PhaseChange`.
//!
//! Use this layer directly for epoch-based steward chat; write a custom app
//! layer if you need a different consensus model, state machine, or epoch
//! timing.

mod consensus_bridge;
mod display;
mod error;
mod key_package;
mod orchestrator;
mod peer_scoring_backends;
mod phase_timer;
mod session_runner;

pub use crate::core::{
    ConversationConfig, ConversationState, DEFAULT_COMMIT_INACTIVITY_DURATION,
    DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_MAX_RETRIES,
    DEFAULT_PEER_SCORE, DEFAULT_PROPOSAL_EXPIRATION, DEFAULT_RECOVERY_INACTIVITY_DURATION,
    DEFAULT_THRESHOLD_PEER_SCORE, DEFAULT_VOTING_DELAY,
};
pub use consensus_bridge::{
    ProposalParams, cast_vote, forward_incoming_vote, relay_incoming_proposal, submit_proposal,
    submit_self_leave_proposal,
};
pub use display::{
    MemberRole, MessageType, format_conversation_request, format_conversation_request_target,
    message_types,
};
pub use error::UserError;
pub use key_package::DefaultKeyPackageProvider;
pub use orchestrator::{
    DefaultConversationPluginsFactory, DefaultMlsService, DefaultPeerScoring, DefaultStewardList,
    User,
};
pub use peer_scoring_backends::InMemoryPeerScoreStorage;
pub use phase_timer::{FreezeTimeoutStatus, PhaseTimer};
pub use session_runner::SessionRunner;
