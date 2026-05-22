//! Per-conversation steward-list plug-in trait and event vocabulary.

use crate::core::error::CoreError;
use crate::core::steward_list::list::{StewardList, StewardListConfig};

/// Fallback ceiling on steward-election retries. One retry gives the
/// responsible proposer a second shot with a different list composition;
/// beyond that human/policy intervention is expected.
pub const DEFAULT_MAX_RETRIES: u32 = 1;

/// Outcome of [`StewardListPlugin::propose_election`]. `Skip` carries a
/// brief reason for the log; `Proposed` carries the ready-to-submit
/// election proposal (the plug-in's deterministic ordering of the
/// supplied candidate pool).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElectionDecision {
    Skip(&'static str),
    Proposed {
        proposed_stewards: Vec<Vec<u8>>,
        election_epoch: u64,
        retry_round: u32,
    },
}

/// Event emitted by [`StewardListPlugin`] mutators. The coordinator
/// drains them at known safe points and turns them into protocol
/// actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StewardListEvent {
    /// A new list has been installed (creator bootstrap, joiner sync,
    /// successful election, or `sn_min` auto-fill). Coordinator chains
    /// into pending-update drain and `ConversationSync` broadcast.
    ListInstalled {
        epoch: u64,
        retry_round: u32,
        len: usize,
    },
    /// `bump_retry` pushed `retry_round` past `max_retries`. Coordinator
    /// escalates to the Layer-3 `Deadlock` ECP.
    RetryExhausted { round: u32, max: u32 },
}

/// Per-conversation steward list. Passive design — answers questions, does not
/// call out to MLS, consensus, or other plug-ins.
///
/// Eligibility predicates flow in from the coordinator: every "live"
/// position query takes a `Fn(&[u8]) -> bool` so the plug-in needn't
/// know about MLS membership or removal queues. When all candidates are
/// eligible, the predicate-based queries return the nominal rotation
/// position; otherwise they walk forward to the next eligible steward.
pub trait StewardListPlugin {
    // ── Config & raw state ─────────────────────────────────────────

    /// Steward list bounds + protocol-level flags.
    fn config(&self) -> &StewardListConfig;

    /// Replace the active config — joiner sync path adopts the
    /// conversation-wide values. Preserves list and retry state; subsequent
    /// `install_list` calls use the new bounds.
    fn set_config(&mut self, config: StewardListConfig);

    /// Borrow the active list. `None` for joiners pre-`ConversationSync`.
    fn current_list(&self) -> Option<&StewardList>;

    /// Epoch at which the active list was elected. `None` if no list.
    fn election_epoch(&self) -> Option<u64>;

    /// Current retry round (0 for fresh elections, bumped on each
    /// rejected proposal within the same MLS epoch). Distinct from the
    /// list's frozen `retry_round` historical tag.
    fn retry_round(&self) -> u32;

    /// Conversation-configured ceiling on steward-election retries. Joiners
    /// pick this up via `ConversationSync`.
    fn max_retries(&self) -> u32;
    fn set_max_retries(&mut self, max: u32);

    // ── State predicates ───────────────────────────────────────────

    /// True iff `member_id` sits in the active list.
    fn is_steward(&self, member_id: &[u8]) -> bool;

    /// True iff `epoch` falls outside the list's covered window
    /// `[election_epoch, election_epoch + len)`. A new election MUST
    /// follow once the list is exhausted.
    fn is_exhausted(&self, epoch: u64) -> bool;

    // ── Position queries (eligibility-filtered) ────────────────────

    /// Steward responsible for `epoch`, walking the rotation past any
    /// candidate for whom `eligible` returns false. Pass `|_| true`
    /// for the nominal position.
    fn epoch_steward<F: Fn(&[u8]) -> bool>(&self, epoch: u64, eligible: F) -> Option<&[u8]>;

    /// Live epoch steward + backup, guaranteed distinct when ≥2 are
    /// eligible. Backup is `None` when fewer than two stewards are
    /// eligible.
    fn epoch_and_backup<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        eligible: F,
    ) -> (Option<&[u8]>, Option<&[u8]>);

    /// Steward roster filtered by `eligible`. Used by the coordinator
    /// to build `ConversationSync.steward_members` so joiners don't inherit
    /// ghosts or members queued for removal.
    fn steward_members<F: Fn(&[u8]) -> bool>(&self, eligible: F) -> Vec<Vec<u8>>;

    /// Deterministic election proposer when the list exhausts. Walks
    /// rotation from index 0; returns `None` if no steward is eligible.
    fn election_proposer<F: Fn(&[u8]) -> bool>(&self, eligible: F) -> Option<&[u8]>;

    // ── Mutators ───────────────────────────────────────────────────

    /// Generate and install a steward list of size `sn` from
    /// `candidate_pool`. `retry_round` is the seed fed into the
    /// SHA256 sort and stored on the resulting list as its historical
    /// tag — pass the round from the accepted election proposal, or 0
    /// for creator bootstrap and `sn_min` auto-fills (no election).
    fn install_list(
        &mut self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        sn: usize,
        retry_round: u32,
    ) -> Result<Vec<StewardListEvent>, CoreError>;

    /// Re-install the list when membership policy says it must change
    /// (e.g. RFC rule: `members.len() < sn_min` ⇒ everyone is a steward).
    /// Coordinator calls this after every membership-changing commit
    /// without checking the rule itself; the plug-in decides whether
    /// to act. Returns events only when a re-install actually fired.
    fn maybe_auto_fill(
        &mut self,
        epoch: u64,
        members: &[Vec<u8>],
    ) -> Result<Vec<StewardListEvent>, CoreError>;

    /// True iff `proposed` matches what this plug-in would generate
    /// for the same parameters. Coordinator calls this on the joiner
    /// path before applying an election result.
    fn validate_proposed(
        &self,
        proposed: &[Vec<u8>],
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        retry_round: u32,
    ) -> Result<bool, CoreError>;

    /// Decide if this node should propose a steward election and return the proposal if so.
    /// Coordinator provides the candidate pool, eligibility filter, self id, and `recovery` to force proposal.
    /// Plug-in handles proposal logic; coordinator submits if needed.
    fn propose_election<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        self_member_id: &[u8],
        eligible: F,
        recovery: bool,
    ) -> Result<ElectionDecision, CoreError>;

    /// Increment the retry round. Emits [`StewardListEvent::RetryExhausted`]
    /// once the new round exceeds `max_retries`.
    fn bump_retry(&mut self) -> Vec<StewardListEvent>;

    /// Reset the retry round to 0 (called on accepted election or
    /// successful commit).
    fn reset_retry(&mut self);
}
