//! Facts a [`crate::Conversation`] reports to its integrator.
//!
//! A conversation does no I/O. As it is driven it records what happened in an
//! internal buffer, which the integrator drains with
//! [`crate::Conversation::drain_events`] once per cycle. One buffer in the
//! order things happened, so a decision and the effects it sets in motion stay
//! together.
//!
//! The category says what is lost by dropping one:
//!
//! - [`Obligation`] — the integrator must act; nothing else will.
//! - [`Request`] — a decision; each variant names its fallback, if it has one.
//! - [`Info`] — restates state that can be queried again.

use crate::{
    ConversationState, Member, MemberId,
    protos::de_mls::messages::v1::{AppMessage, ConversationUpdateRequest, MemberWelcome},
};

/// Something the conversation observed or did. Drained via
/// [`crate::Conversation::drain_events`].
#[derive(Debug, Clone)]
pub enum ConversationEvent {
    Obligation(Obligation),
    Request(Request),
    Info(Info),
}

/// Work only the integrator can do. Each is handed over once and kept
/// nowhere — no query brings one back, and no fallback covers it.
#[derive(Debug, Clone)]
pub enum Obligation {
    /// A decrypted group message and the MLS-authenticated member who signed
    /// it. The payload is a chat or a membership event.
    /// The integrator MUST deliver it.
    ConversationMessage { message: AppMessage, sender: Member },

    /// The welcome new members need, with their `ConversationSync` bundled in.
    /// Every member sees it: the committing steward mints and broadcasts it
    /// (`minted_locally == true`), the rest surface the copy that arrives.
    /// The integrator MUST deliver it to the new joiners.
    WelcomeReady {
        welcome: MemberWelcome,
        minted_locally: bool,
    },

    /// The local member has left the group — either it asked to, or the group
    /// removed it — and the commit has landed. Until that happens, a requested
    /// leave only shows up in [`crate::Conversation::pending_leave`].
    /// The integrator MUST drop this handle
    Left,
}

/// A decision the conversation would like from the integrator.
#[derive(Debug, Clone)]
pub enum Request {
    /// A peer's proposal needs this member's vote. Silence votes automatically
    /// once `voting_delay` passes.
    VoteRequested {
        proposal_id: u32,
        request: ConversationUpdateRequest,
    },

    /// The steward list deadlocked and any member may now commit. Call
    /// [`crate::Conversation::commit_in_recovery`], or leave it to the
    /// auto-commit after `recovery_auto_commit_delay`.
    /// Repeated requests for a `ConversationSync` have gone unanswered, so this
    /// member is still running without a steward list, peer scores, or protocol
    /// config of its own. No steward is answering — the epoch steward and every
    /// backup are silent or unreachable.
    RecoveryModeOpened,

    /// Recovery ran `recovery_max_rounds` rounds without producing a commit, so
    /// the conversation stops retrying: the relaxed steward gate closes again
    /// and the state goes back to `Working`. Nothing was fixed — the deadlock is
    /// most likely still there, and from here the conversation looks like any
    /// healthy one, so this event is the only notice of it.
    ///
    /// What happens next is the integrator's: call
    /// [`crate::Conversation::request_recovery`] to open a fresh recovery, or
    /// stop and tell its user. Never emitted at the default
    /// `recovery_max_rounds` of `0`, which retries forever.
    RecoveryExhausted,

    /// Sync requests have gone unanswered `unanswered_sync_rounds` times in a
    /// row, each a `backup_takeover_window` apart — sustained silence.
    /// One unanswered request is ordinary and passes without an event.
    ///
    /// `poll` keeps asking either way — the count decides when to say so, never
    /// whether to keep trying. Wait it out, reach the group over transport, or
    /// tear the conversation down. [`crate::Conversation::is_synced`] reports
    /// the state at any time.
    ConversationSyncUnanswered,
}

/// A notice about state the integrator can query whenever it likes.
#[derive(Debug, Clone)]
pub enum Info {
    /// A step the conversation ran on its own failed. `operation` says which.
    /// The conversation carries on; log it or show it.
    Error { operation: String, message: String },

    /// The conversation moved to a new lifecycle phase, such as a commit round
    /// or steward selection.
    PhaseChange(ConversationState),

    /// A consensus session has finished, and here's what was decided.
    /// The `timestamp` is straight from the consensus layer.
    /// To find out what this was about, use [`crate::Conversation::proposal`] with the `proposal_id`
    ConsensusReached {
        proposal_id: u32,
        approved: bool,
        timestamp: u64,
    },

    /// `received` of the `expected` stewards have broadcast their commit
    /// candidate for this round — `expected` being the size of the current
    /// steward list. Emitted only when the count changes; the round closes and
    /// selection begins once every candidate is in or the freeze window
    /// elapses.
    /// [`crate::Conversation::commit_candidate_count`] returns the same pair at any time.
    CommitRoundProgress { received: usize, expected: usize },

    /// A `ConversationSync` was adopted and its steward list installed. Either
    /// the first one, bootstrapping a member whose welcome carried none, or a
    /// newer election replacing a list that had aged out.
    ConversationSyncApplied,

    /// A peer's score changed — `score` is the new value,  and `previous` was the old one.
    /// Not shown if gains and losses cancel out.
    ///
    /// A fact, not a verdict: the member keeps every right it had.
    ///
    /// Acting is the integrator's policy — you can compare
    /// against [`crate::Conversation::score_threshold`], then call
    /// [`crate::Conversation::remove_member`] or emergency
    /// [`crate::Conversation::propose_score_removal`].
    ///
    /// Peer scores are based on each node's own view, so different members might
    /// see different scores for the same peer at a given moment, but they can resync
    /// to agree on state when needed. Never use a peer score alone to make decisions
    /// that require agreement from the group.
    MemberScoreChanged {
        member: MemberId,
        previous: i64,
        score: i64,
    },

    /// A commit changed the member set. `removed` handles were tagged before
    /// the commit, so a reused leaf yields distinct handles on each side.
    /// The set stays readable through [`crate::Conversation::mls_group`].
    MembersChanged {
        added: Vec<Member>,
        removed: Vec<MemberId>,
    },
}

impl From<Obligation> for ConversationEvent {
    fn from(o: Obligation) -> Self {
        Self::Obligation(o)
    }
}

impl From<Request> for ConversationEvent {
    fn from(r: Request) -> Self {
        Self::Request(r)
    }
}

impl From<Info> for ConversationEvent {
    fn from(i: Info) -> Self {
        Self::Info(i)
    }
}
