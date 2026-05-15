//! I/O contract between the protocol layer and an integrator.
//!
//! Two channels:
//!
//! - [`SessionEvent`] — fire-and-forget notifications about a single
//!   conversation. Each [`crate::app::SessionRunner`] owns a broadcast sender;
//!   integrators subscribe per session.
//! - [`ConversationLifecycle`] — User-level create/remove notifications.
//!   Integrators use this to discover new sessions and subscribe to them.
//!
//! Synchronous outbound transport is supplied by
//! [`crate::ds::DeliveryService`], passed to `User` at construction and
//! cloned into each session.

use hashgraph_like_consensus::types::ConsensusEvent;

use crate::{
    core::ConversationState,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest},
};

/// Per-conversation notification. Sessions broadcast these; integrators
/// subscribe via [`crate::app::SessionRunner::subscribe`]. All variants are
/// fire-and-forget — no failure path back to the session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Decrypted application message (chat, vote request, proposal
    /// notification, ban request, …).
    AppMessage(AppMessage),

    /// Welcome processed and MLS state initialised. Epoch timers + state
    /// transitions are already wired — surface to UI only.
    Joined,

    /// The user is out of this conversation (self-leave commit merged, or
    /// someone else removed us). The session entry is about to be removed
    /// from `User`'s registry.
    Leaving,

    /// A background operation (e.g., vote submission) failed. UI may surface;
    /// state has already been reconciled.
    Error { operation: String, message: String },

    /// The local user just submitted `request` as a new proposal with the
    /// creator's vote bundled. UI should record for history; no "please vote"
    /// affordance.
    OwnProposalSubmitted {
        proposal_id: u32,
        request: ConversationUpdateRequest,
    },

    /// A freeze round merged a commit; `batch` is the set of approved
    /// proposals that landed in this commit (in insertion order).
    CommitApplied(Vec<ConversationUpdateRequest>),

    /// Conversation transitioned into `state`.
    PhaseChange(ConversationState),

    /// A consensus session reached an outcome. Fired after the runner has
    /// applied the result to local state.
    ProposalDecided(ConsensusEvent),
}

/// User-level conversation lifecycle event. Sent on [`crate::app::User`]'s
/// lifecycle channel; integrators subscribe via
/// [`crate::app::User::subscribe_conversations`] and use `Created` as the
/// trigger to subscribe to the new session's [`SessionEvent`] stream.
#[derive(Debug, Clone)]
pub enum ConversationLifecycle {
    /// A new conversation entry has been registered. The session is in the
    /// registry; the integrator can look it up and `subscribe()` to its
    /// per-session events.
    Created(String),

    /// A conversation entry has been removed from the registry.
    Removed(String),
}
