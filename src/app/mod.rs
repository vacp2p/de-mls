//! Reference application layer — wires [`crate::core`] into a working chat
//! app with consensus, state machine, peer scoring, and freeze/commit timing.
//!
//! Integrate by constructing a [`crate::app::User`] with your
//! [`crate::core::GroupEventHandler`] and [`crate::app::StateChangeHandler`],
//! then drive it from a transport receive loop
//! ([`crate::app::User::process_inbound_packet`]) and a periodic poll
//! ([`crate::app::User::check_member_freeze`],
//! [`crate::app::User::poll_freeze_status`]). [`crate::app::PhaseTimer`]
//! owns the per-group state transitions
//! (`PendingJoin → Working → Freezing → Selection → Reelection → Leaving`).
//!
//! Use directly for epoch-based steward chat; build a custom app layer if you
//! need a different consensus model, state machine, or epoch timing.

mod config;
mod consensus_bridge;
mod display;
mod error;
mod peer_scoring;
mod phase_timer;
mod user;

pub use config::{
    DEFAULT_CONSENSUS_TIMEOUT, DEFAULT_ELECTION_VOTING_DELAY, DEFAULT_EPOCH_DURATION,
    DEFAULT_LIVENESS_CRITERIA_YES, DEFAULT_MAX_RETRIES, DEFAULT_PEER_SCORE,
    DEFAULT_PENDING_UPDATE_MAX_EPOCHS, DEFAULT_PROPOSAL_EXPIRATION,
    DEFAULT_RETRY_INACTIVITY_DURATION, DEFAULT_THRESHOLD_PEER_SCORE, DEFAULT_VOTING_DELAY,
    GroupConfig,
};
pub use consensus_bridge::{
    ProposalParams, cast_vote, forward_incoming_proposal, forward_incoming_vote, submit_proposal,
    submit_self_leave_proposal,
};
pub use display::{
    MemberRole, MessageType, format_group_request, format_group_request_target, message_types,
};
pub use error::UserError;
pub use peer_scoring::{FixedScoringProvider, InMemoryPeerScoreStorage};
pub use phase_timer::{FreezeTimeoutStatus, GroupState, PhaseTimer, StateChangeHandler};
pub use user::{
    DefaultMlsService, DefaultPeerScoring, DefaultStewardList, KeyPackageGenerator,
    MlsCreatorFactory, MlsWelcomeFactory, ScoringFactory, StewardFactory, User,
};
