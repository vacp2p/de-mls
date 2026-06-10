//! I/O contract between the protocol layer and an integrator.
//!
//! Two queues:
//!
//! - [`SessionEvent`] — fire-and-forget notifications about a single
//!   conversation. Each [`crate::session::SessionRunner`] holds a pending
//!   buffer; integrators drain it once per polling cycle.
//! - [`ConversationLifecycle`] — User-level create/remove notifications.
//!   Integrators use this to discover new sessions and start draining them.
//!
//! The library carries no transport: it buffers `Outbound` and consumes
//! inbound payloads. The integrator owns delivery (see the `de-mls-ds`
//! crate's `DeliveryService` for the reference transport).

use crate::{
    core::ConversationState,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest, MemberWelcome},
};

/// Per-conversation notification. Sessions append these to their pending
/// buffer; integrators drain via
/// [`crate::session::SessionRunner::drain_events`] once per polling cycle. All
/// variants are fire-and-forget — no failure path back to the session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Decrypted application message (chat, vote request, proposal
    /// notification, ban request, …).
    AppMessage(AppMessage),

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

    /// A peer's consensus proposal needs the local user's vote. The
    /// integrator surfaces a vote affordance however it wishes; an auto-vote
    /// fires after the configured delay if no manual vote arrives. Peer-side
    /// mirror of [`Self::OwnProposalSubmitted`].
    VoteRequested {
        proposal_id: u32,
        request: ConversationUpdateRequest,
    },

    /// A freeze round merged a commit; `batch` is the set of approved
    /// proposals that landed in this commit (in insertion order).
    CommitApplied(Vec<ConversationUpdateRequest>),

    /// Conversation transitioned into `state`.
    PhaseChange(ConversationState),

    /// Our merged commit added members. Carries the MLS welcome blob
    /// and the encrypted `ConversationSync` payload bundled for atomic
    /// delivery. The integrator owns delivery to each joiner.
    WelcomeReady(MemberWelcome),

    /// A consensus session on this conversation resolved. Emitted before
    /// the protocol effects (commit candidate, freeze, score apply, …)
    /// run, so UI fanout sees the decision in the same polling cycle as
    /// the state change that follows. `timestamp` is the upstream
    /// consensus library's stamp on the bus event.
    ConsensusReached {
        proposal_id: u32,
        approved: bool,
        timestamp: u64,
    },
}

/// User-level conversation lifecycle event. Appended to `User`'s
/// pending buffer; integrators drain via
/// `User::drain_lifecycle_events` once per polling cycle and
/// use `Created` as the trigger to begin draining per-session
/// [`SessionEvent`]s.
#[derive(Debug, Clone)]
pub enum ConversationLifecycle {
    /// A new conversation entry has been registered. The session is in the
    /// registry; the integrator can look it up and `subscribe()` to its
    /// per-session events.
    Created(String),

    /// A conversation entry has been removed from the registry.
    Removed(String),
}
