//! Reference application layer — wires [`crate::core`] into a working chat
//! app with consensus, state machine, peer scoring, and freeze/commit timing.
//!
//! Integrate by constructing a [`crate::app::User`] with your
//! [`crate::core::GroupEventHandler`] and [`crate::app::StateChangeHandler`],
//! then drive it from a transport receive loop
//! ([`crate::app::User::process_inbound_packet`]) and a periodic poll
//! ([`crate::app::User::check_member_freeze`],
//! [`crate::app::User::poll_freeze_status`]). [`crate::app::GroupStateMachine`]
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
mod state_machine;
mod user;

pub use config::GroupConfig;
pub use consensus_bridge::{
    cast_vote, forward_incoming_proposal, forward_incoming_vote, submit_proposal,
};
pub use display::{
    MemberRole, MessageType, format_group_request, format_group_request_target, message_types,
};
pub use error::UserError;
pub use peer_scoring::{FixedScoringProvider, InMemoryPeerScoreStorage, PeerScoringService};
pub use state_machine::{FreezeTimeoutStatus, GroupState, GroupStateMachine, StateChangeHandler};
pub use user::User;
pub use user::emergency::emergency_score_ops;
