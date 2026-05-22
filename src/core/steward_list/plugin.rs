//! [`StewardListPlugin`] trait, events, and election vocabulary.

use crate::core::error::CoreError;
use crate::core::steward_list::list::{StewardList, StewardListConfig};

/// Default steward-election retry ceiling before `Deadlock` escalation.
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

/// Side effect from a plug-in mutator; drained by the coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StewardListEvent {
    /// New list installed — broadcast `ConversationSync`.
    ListInstalled {
        epoch: u64,
        retry_round: u32,
        len: usize,
    },
    /// `bump_retry` exceeded [`StewardListPlugin::max_retries`] — escalate to `Deadlock` ECP.
    RetryExhausted { round: u32, max: u32 },
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

    /// Live retry counter for the *next* election attempt (not the frozen list tag).
    fn retry_round(&self) -> u32;

    fn max_retries(&self) -> u32;
    fn set_max_retries(&mut self, max: u32);

    fn is_steward(&self, member_id: &[u8]) -> bool;

    /// `true` when `epoch` is outside `[election_epoch, election_epoch + len)`.
    fn is_exhausted(&self, epoch: u64) -> bool;

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
    /// and is stored on the list; use `0` for bootstrap / auto-fill.
    fn install_list(
        &mut self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        sn: usize,
        retry_round: u32,
    ) -> Result<Vec<StewardListEvent>, CoreError>;

    /// Re-install when `members.len() < sn_min` (everyone becomes a steward).
    fn maybe_auto_fill(
        &mut self,
        epoch: u64,
        members: &[Vec<u8>],
    ) -> Result<Vec<StewardListEvent>, CoreError>;

    /// `true` if `proposed` matches [`StewardList::validate`] for these parameters.
    fn validate_proposed(
        &self,
        proposed: &[Vec<u8>],
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        retry_round: u32,
    ) -> Result<bool, CoreError>;

    /// Whether this node should propose a steward election at `epoch`.
    fn propose_election<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        self_member_id: &[u8],
        eligible: F,
        recovery: bool,
    ) -> Result<ElectionDecision, CoreError>;

    /// Increment retry round; may emit [`StewardListEvent::RetryExhausted`].
    fn bump_retry(&mut self) -> Vec<StewardListEvent>;

    fn reset_retry(&mut self);
}
