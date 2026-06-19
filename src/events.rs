//! I/O contract between the protocol layer and an integrator.
//!
//! [`ConversationEvent`] — fire-and-forget notifications about a single
//! conversation. Each [`crate::Conversation`] holds a pending
//! buffer; integrators drain it once per polling cycle.
//!
//! The library carries no transport: it buffers `Outbound` and consumes
//! inbound payloads. The integrator owns delivery (see the `de-mls-ds`
//! crate's `DeliveryService` for the reference transport).

use crate::{
    ConversationState,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest, MemberWelcome},
};

/// Per-conversation notification. Sessions append these to their pending
/// buffer; integrators drain via
/// [`crate::Conversation::drain_events`] once per polling cycle. All
/// variants are fire-and-forget — no failure path back to the conversation.
#[derive(Debug, Clone)]
pub enum ConversationEvent {
    /// Decrypted application message (chat, vote request, proposal
    /// notification, ban request, …).
    AppMessage(AppMessage),

    /// The local member is out of this conversation (self-leave commit
    /// merged, or removed by a steward). The integrator should remove the
    /// registry entry and clean up the consensus scope.
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

    /// A merged commit added members. Carries the MLS welcome blob and the
    /// encrypted `ConversationSync` payload bundled for atomic delivery.
    /// Fires on every member — the committing steward mints it
    /// (`minted_locally == true`) and broadcasts it to the group, so peers
    /// receive the same welcome (`minted_locally == false`). The
    /// application decides who delivers it to the joiners and how.
    WelcomeReady {
        welcome: MemberWelcome,
        minted_locally: bool,
    },

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

    /// Freeze-round candidate progress changed: `received` stewards have
    /// submitted candidates out of `expected`. Emitted by `poll()` when in
    /// `Freezing` and the count changed since the previous emission. The
    /// integrator can surface this as a progress indicator without polling
    /// `freeze_candidate_count()`.
    FreezeProgress { received: usize, expected: usize },
}
