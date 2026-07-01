//! Events a [`crate::Conversation`] reports to its integrator.
//!
//! A conversation performs no I/O of its own. As it is driven — handling
//! inbound traffic, polling, or a local action — it records what happened as
//! [`ConversationEvent`]s in an internal buffer. The integrator drains that
//! buffer with [`crate::Conversation::drain_events`] and acts on each event:
//! delivering a message, updating its view, routing to transport, and so on.
//! Events are fire-and-forget — recording one never blocks on the integrator.

use crate::{
    ConversationState,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest, MemberWelcome},
};

/// Something the conversation observed or did, handed to the integrator to act
/// on. Drained via [`crate::Conversation::drain_events`].
#[derive(Debug, Clone)]
pub enum ConversationEvent {
    /// A decrypted message addressed to the group's chat stream. The payload is
    /// either a `ConversationMessage` (text a member sent) or an
    /// `EventMembershipChange` (a system notice that someone joined) —
    /// both belong in the same stream, so the application renders whichever it
    /// finds. Proposals, votes, and ban requests are not chat traffic; they
    /// surface through [`Self::VoteRequested`] and the membership and consensus
    /// events instead.
    ConversationMessage(AppMessage),

    /// The local member has left the group — its own removal just committed,
    /// whether a self-leave it asked for or a removal the group decided.
    /// Nothing more will come from this handle; it can be dropped.
    Leaving,

    /// A protocol step the conversation was carrying out on its own — say,
    /// submitting a vote — didn't go through. It stays usable otherwise;
    /// surface or log the failure as suits the application.
    Error { operation: String, message: String },

    /// The local member just raised `request` as a proposal, with its own
    /// creator vote already bundled in. This is simply notice that a proposal
    /// the member started is now in flight — the peer-raised proposals that
    /// still need a vote arrive through [`Self::VoteRequested`].
    OwnProposalSubmitted {
        proposal_id: u32,
        request: ConversationUpdateRequest,
    },

    /// A peer raised a proposal and the local member's vote is now due. Present
    /// the choice and cast one; left alone, the conversation votes
    /// automatically once the configured delay passes. The peer-side
    /// counterpart of [`Self::OwnProposalSubmitted`].
    VoteRequested {
        proposal_id: u32,
        request: ConversationUpdateRequest,
    },

    /// A commit merged and the conversation advanced to a new MLS epoch.
    /// `batch` holds the proposals that landed, in the order they applied — the
    /// membership now reflects them.
    CommitApplied(Vec<ConversationUpdateRequest>),

    /// The conversation moved into a new lifecycle phase, such as a freeze
    /// round or steward selection — a window into where it sits in its
    /// commit-and-recovery cycle.
    PhaseChange(ConversationState),

    /// A merged commit let new members in. This carries the MLS welcome they
    /// need, with the encrypted `ConversationSync` bundled alongside so the two
    /// travel together. Every existing member sees it: the steward that
    /// committed mints it (`minted_locally == true`) and broadcasts it, so the
    /// rest surface the same welcome (`minted_locally == false`). Carrying it
    /// to the joiners is the integrator's call — it owns who delivers it and
    /// how.
    WelcomeReady {
        welcome: MemberWelcome,
        minted_locally: bool,
    },

    /// A consensus session resolved, `approved` carrying the verdict. It comes
    /// ahead of the effects it sets in motion — a commit candidate, a freeze, a
    /// score update — so the decision and the state changes it drives are seen
    /// together. `timestamp` is the consensus layer's own stamp on the
    /// resolution.
    ConsensusReached {
        proposal_id: u32,
        approved: bool,
        timestamp: u64,
    },

    /// During a freeze round, the tally of steward commit candidates moved:
    /// `received` of `expected` are now in. It ticks as candidates arrive,
    /// enough to drive a progress indicator.
    FreezeProgress { received: usize, expected: usize },
}
