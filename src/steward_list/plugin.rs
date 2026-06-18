//! [`StewardListPlugin`] trait and election vocabulary.

use crate::error::ConversationError;
use crate::steward_list::list::{StewardList, StewardListConfig};

/// Steward-election retries *after* the initial attempt, before `Deadlock`
/// escalation. So `1` = two attempts (rounds 0 and 1) total.
pub const DEFAULT_MAX_RETRIES: u32 = 1;

/// Result of [`StewardListPlugin::propose_election`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElectionDecision {
    Skip(&'static str),
    Proposed {
        proposed_stewards: Vec<Vec<u8>>,
        election_epoch: u64,
        retry_round: u32,
    },
}

/// Per-conversation steward roster. Eligibility flows in via `Fn(&[u8]) -> bool`
/// on position queries; the plug-in does not inspect MLS or removal state.
pub trait StewardListPlugin {
    fn config(&self) -> &StewardListConfig;

    /// Adopt conversation-wide bounds; keeps list and retry state.
    fn set_config(&mut self, config: StewardListConfig);

    /// Active list, or `None` before the first `ConversationSync`.
    fn current_list(&self) -> Option<&StewardList>;

    fn election_epoch(&self) -> Option<u64>;

    /// Round the *next* election attempt will use as its generation seed.
    /// Distinct from [`StewardList::retry_round`] — the seed frozen into the
    /// current list.
    fn next_retry_round(&self) -> u32;

    fn max_retries(&self) -> u32;
    fn set_max_retries(&mut self, max: u32);

    fn is_steward(&self, member_id: &[u8]) -> bool;

    /// `true` when `epoch` is outside `[election_epoch, election_epoch + len)`.
    fn is_exhausted(&self, epoch: u64) -> bool;

    /// Whether a steward list over `member_count` members needs a *voted*
    /// election. `false` while every member is a steward (`member_count <=
    /// sn_max`): the list is then the full membership, fully determined and
    /// regenerated deterministically with no consensus. `true` once the list
    /// is a genuine subset, where peers must agree which members serve.
    fn election_required(&self, member_count: usize) -> bool {
        member_count > self.config().sn_max
    }

    /// See [`StewardList::epoch_steward`].
    fn epoch_steward<F: Fn(&[u8]) -> bool>(&self, epoch: u64, eligible: F) -> Option<&[u8]>;

    /// See [`StewardList::epoch_and_backup`].
    fn epoch_and_backup<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        eligible: F,
    ) -> (Option<&[u8]>, Option<&[u8]>);

    /// Eligible stewards on the active list (for `ConversationSync.steward_members`).
    fn steward_members<F: Fn(&[u8]) -> bool>(&self, eligible: F) -> Vec<Vec<u8>>;

    /// Member at rotation index 0 when the list was elected.
    fn election_proposer<F: Fn(&[u8]) -> bool>(&self, eligible: F) -> Option<&[u8]>;

    /// Build and install a list of size `sn`. `retry_round` seeds the SHA256 sort
    /// and is stored on the list; use `0` for bootstrap and deterministic regen.
    fn install_list(
        &mut self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        sn: usize,
        retry_round: u32,
    ) -> Result<(), ConversationError>;

    /// `true` if `proposed` matches [`StewardList::validate`] for these parameters.
    fn validate_proposed(
        &self,
        proposed: &[Vec<u8>],
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        retry_round: u32,
    ) -> Result<bool, ConversationError>;

    /// Whether this node should propose a steward election at `epoch`.
    fn propose_election<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        self_member_id: &[u8],
        eligible: F,
        recovery: bool,
    ) -> Result<ElectionDecision, ConversationError>;

    /// Increment the retry round. Exhaustion is detected by the caller via
    /// [`Self::next_retry_round`] vs [`Self::max_retries`].
    fn bump_retry(&mut self);

    fn reset_retry(&mut self);
}
