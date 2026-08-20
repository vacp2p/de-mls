//! Events a [`crate::Conversation`] reports to its integrator.
//!
//! A conversation performs no I/O of its own. As it is driven — handling
//! inbound traffic, polling, or a local action — it records what happened as
//! [`ConversationEvent`]s in an internal buffer. The integrator drains that
//! buffer with [`crate::Conversation::drain_events`] and acts on each event:
//! delivering a message, updating its view, routing to transport, and so on.
//! Events are fire-and-forget — recording one never blocks on the integrator.

use crate::{
    ConversationState, Member, MemberId,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest, MemberWelcome},
};

/// Something the conversation observed or did, handed to the integrator to act
/// on. Drained via [`crate::Conversation::drain_events`].
#[derive(Debug, Clone)]
pub enum ConversationEvent {
    /// A decrypted group chat message with the MLS-authenticated [`Member`] who signed it.
    /// Sender info comes from MLS, Payload is either a chat (`ConversationMessage`)
    /// or a membership event (`EventMembershipChange`).
    ConversationMessage { message: AppMessage, sender: Member },

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

    /// The conversation moved into a new lifecycle phase, such as a commit
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

    /// Progress of a commit round: `received` of `expected` steward commit
    /// candidates have arrived. Re-emitted whenever the count changes, so it
    /// can back a progress indicator.
    CommitRoundProgress { received: usize, expected: usize },

    /// Layer-3 recovery opened: the steward list is deadlocked and the steward
    /// gate is relaxed so any member MAY commit. The integrator decides the
    /// policy — call [`crate::Conversation::commit_in_recovery`] to mint now,
    /// or leave it to the auto-fallback that mints on every online node after
    /// `recovery_auto_commit_delay`.
    RecoveryModeOpened,

    /// Layer-3 recovery gave up after `recovery_max_rounds` manual+auto rounds
    /// produced no commit. The conversation leaves recovery for `Working`; the
    /// integrator decides what an unrecoverable group does (alert, tear down).
    /// Never emitted when `recovery_max_rounds` is `0` (retry forever).
    RecoveryExhausted,

    /// The local member joined but its welcome carried no `ConversationSync`, so
    /// it has no steward list, scores, or protocol config — a degraded join.
    /// `poll` auto-requests a sync (see
    /// [`crate::Conversation::request_conversation_sync`]) until a steward
    /// re-sends it; this event is informational — the integrator can surface it
    /// but need not drive the request.
    ConversationSyncMissing,

    /// A `ConversationSync` was adopted and the local member is now bootstrapped.
    /// Ends the degraded state a [`Self::ConversationSyncMissing`] opened, and
    /// the poll-driven sync requests stop.
    ConversationSyncApplied,

    /// `member_id`'s peer score moved (RFC §Peer Scoring), from `previous` to
    /// `score`. Reported net of whatever caused it, so `score` is what
    /// [`crate::Conversation::member_score`] returns right now, and a member
    /// whose gains and penalties cancel out is not reported at all.
    ///
    /// Two inputs produce one, and they are different kinds of statement:
    ///
    /// - **Observed events** — a violation or a successful commit this node saw
    ///   for itself. "This score moved, `previous` → `score`": a delta.
    /// - **Synchronization** — a `ConversationSync` carrying what the group
    ///   already holds. "This score *is* `score`": current state, not
    ///   movement. `previous` is then what this node had assumed (the default
    ///   it seeded on join), so a joiner learns where everyone stands rather
    ///   than watching them get there.
    ///
    /// A fact, not a judgment. The library reads nothing into the movement:
    /// the member keeps every protocol right it had, and no removal is
    /// proposed on its own. Interpreting it is the integrator's policy —
    /// compare either end against [`crate::Conversation::score_threshold`] to
    /// catch a removal-threshold crossing in either direction, band the range
    /// however suits the application, or watch the trend. Acting on it means
    /// calling [`crate::Conversation::remove_member`], or
    /// [`crate::Conversation::propose_score_removal`] to raise the emergency
    /// removal the group then votes on.
    ///
    /// Peer-score tables are local — they travel only in the joiner bootstrap
    /// of `ConversationSync`, and each member scores what it observes — so two
    /// members hold different scores for the same peer and reach any given
    /// line at different moments, or never. Treat it as this node's reading,
    /// not a group verdict, and don't let it drive protocol decisions that
    /// members have to agree on.
    MemberScoreChanged {
        member_id: Vec<u8>,
        previous: i64,
        score: i64,
    },

    /// A merged commit changed the member set: `added` carries each new member as
    /// an OpenMLS [`Member`] (leaf index + credential + keys), `removed` the
    /// departed members as [`MemberId`] handles (tagged before the commit, so a
    /// reused leaf yields distinct handles in `added` and `removed`).
    MembersChanged {
        added: Vec<Member>,
        removed: Vec<MemberId>,
    },
}
