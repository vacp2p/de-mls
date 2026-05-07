//! Steward list plug-in: passive, per-group steward roster.
//!
//! Answers "who is the steward at epoch N?", "am I a steward?", and
//! "is the list exhausted?". Stores its own list, election epoch, retry
//! round, and retry policy. Never reaches into MLS, consensus, or other
//! plug-ins — the coordinator composes the eligibility predicate (MLS
//! membership minus pending-removal proposals) and passes it on every
//! position query.
//!
//! Mutators return [`StewardListEvent`]s so the coordinator can chain
//! protocol actions (broadcast `GroupSync` after install, escalate to a
//! Layer-3 `Deadlock` ECP after retry exhaustion).
//!
//! [`StewardList`] and [`StewardListConfig`] live here too: they're impl
//! details of the deterministic reference plug-in. A future stake-
//! weighted variant would not share them.

use sha2::{Digest, Sha256};

use crate::core::error::CoreError;

// ── Configuration ───────────────────────────────────────────────────

/// Steward-list configuration set at group creation. The deterministic
/// reference impl reads these bounds for size selection and validation;
/// commit-batch and other unrelated knobs live elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StewardListConfig {
    /// Minimum steward list size. If total members < sn_min, list size = total members.
    pub sn_min: usize,
    /// Maximum steward list size.
    pub sn_max: usize,
    /// Whether subset commit candidates are allowed during deterministic selection.
    pub allow_subset_candidates: bool,
}

impl StewardListConfig {
    /// Create a new config with the given bounds.
    ///
    /// Returns `Err` if `sn_min` is 0 or `sn_min > sn_max`.
    pub fn new(sn_min: usize, sn_max: usize) -> Result<Self, CoreError> {
        if sn_min < 1 || sn_min > sn_max {
            return Err(CoreError::InvalidConfigSize);
        }
        Ok(Self {
            sn_min,
            sn_max,
            allow_subset_candidates: false,
        })
    }

    /// Inclusive range of valid list sizes for `total_members` (RFC §Steward
    /// list creation). When `total_members < sn_min` the only valid size is
    /// `total_members`; otherwise the range is `[sn_min, min(sn_max, total)]`.
    fn size_bounds(&self, total_members: usize) -> std::ops::RangeInclusive<usize> {
        if total_members < self.sn_min {
            total_members..=total_members
        } else {
            self.sn_min..=self.sn_max.min(total_members)
        }
    }

    /// Preferred list size (the upper end of the valid range).
    pub fn compute_list_size(&self, total_members: usize) -> usize {
        *self.size_bounds(total_members).end()
    }

    /// `true` iff `size` lies within the valid range for this config.
    pub fn is_valid_size(&self, size: usize, total_members: usize) -> bool {
        self.size_bounds(total_members).contains(&size)
    }
}

// ── Steward list (deterministic data type) ──────────────────────────

/// An ordered list of steward identities for a range of epochs.
///
/// Generated deterministically so all group members arrive at the same list.
/// The list covers epochs `[election_epoch, election_epoch + len)`. Used
/// internally by [`DeterministicStewardList`]; surfaced through
/// [`StewardListPlugin::current_list`] for read-only inspection (e.g.
/// building `GroupSync`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StewardList {
    /// Ordered steward identities (sorted by deterministic hash).
    members: Vec<Vec<u8>>,
    /// Configuration bounds.
    config: StewardListConfig,
    /// The epoch at which this steward list became active.
    election_epoch: u64,
    /// The retry-round seed fed into the SHA256 sort that produced this
    /// list. Historical tag — frozen once the list is accepted. Distinct
    /// from the plug-in's dynamic counter for the *next* election attempt.
    retry_round: u32,
}

impl StewardList {
    /// Generate the deterministic steward list. Sorts candidates by
    /// `SHA256(epoch || retry_round || member_id || group_id)` and takes
    /// the first `sn`. Errors on empty `member_ids` or `sn` outside the
    /// config bounds.
    pub fn generate(
        election_epoch: u64,
        group_id: &[u8],
        member_ids: &[Vec<u8>],
        sn: usize,
        config: StewardListConfig,
        retry_round: u32,
    ) -> Result<Self, CoreError> {
        check_generation_inputs(&config, member_ids, sn)?;
        let ordered = sorted_steward_indices(election_epoch, retry_round, group_id, member_ids);
        let members = ordered
            .into_iter()
            .take(sn)
            .map(|i| member_ids[i].clone())
            .collect();
        Ok(Self {
            members,
            config,
            election_epoch,
            retry_round,
        })
    }

    /// True iff `proposed` equals what [`Self::generate`] would produce
    /// for the same parameters. Compares in place — does not allocate a
    /// full [`StewardList`].
    pub fn validate(
        proposed: &[Vec<u8>],
        election_epoch: u64,
        group_id: &[u8],
        member_ids: &[Vec<u8>],
        config: &StewardListConfig,
        retry_round: u32,
    ) -> Result<bool, CoreError> {
        let sn = proposed.len();
        check_generation_inputs(config, member_ids, sn)?;
        let ordered = sorted_steward_indices(election_epoch, retry_round, group_id, member_ids);
        Ok(ordered
            .iter()
            .take(sn)
            .zip(proposed.iter())
            .all(|(&i, want)| &member_ids[i] == want))
    }

    /// Nominal epoch steward at index `(epoch - election_epoch) % len`.
    /// Use [`Self::live_steward_from`] with the eligibility predicate to
    /// skip stewards no longer in the group.
    pub fn epoch_steward(&self, epoch: u64) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let index = ((epoch - self.election_epoch) as usize) % self.members.len();
        Some(&self.members[index])
    }

    /// Nominal backup steward at index `(epoch - election_epoch + 1) % len`.
    pub fn backup_steward(&self, epoch: u64) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let index = ((epoch - self.election_epoch) as usize + 1) % self.members.len();
        Some(&self.members[index])
    }

    /// Live epoch steward + a distinct backup. Resolving them together
    /// stops the epoch-steward walk from landing on the nominal backup
    /// and collapsing both roles onto the same identity. Backup is
    /// `None` when fewer than two stewards are eligible.
    pub fn live_epoch_and_backup<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        eligible: F,
    ) -> (Option<&[u8]>, Option<&[u8]>) {
        let epoch_steward = self.live_steward_from(epoch, 0, &eligible);
        let backup = epoch_steward
            .and_then(|es| self.live_steward_from(epoch, 1, |c| c != es && eligible(c)));
        (epoch_steward, backup)
    }

    /// Walk the rotation starting at `offset` past `eligible == false`
    /// and return the first eligible steward. `offset = 0` resolves the
    /// epoch steward; `offset = 1` resolves the backup. Returns `None`
    /// when the list is exhausted at `epoch` or no candidate is eligible.
    pub fn live_steward_from<F: Fn(&[u8]) -> bool>(
        &self,
        epoch: u64,
        offset: usize,
        eligible: F,
    ) -> Option<&[u8]> {
        if self.is_exhausted(epoch) {
            return None;
        }
        let len = self.members.len();
        let start = ((epoch - self.election_epoch) as usize + offset) % len;
        for step in 0..len {
            let idx = (start + step) % len;
            let candidate = &self.members[idx];
            if eligible(candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// `true` once every steward has served — the list covers
    /// `[election_epoch, election_epoch + len)`. A new election MUST follow.
    pub fn is_exhausted(&self, epoch: u64) -> bool {
        if epoch < self.election_epoch {
            return true;
        }
        (epoch - self.election_epoch) >= self.members.len() as u64
    }

    pub fn contains(&self, member_id: &[u8]) -> bool {
        self.members.iter().any(|m| m.as_slice() == member_id)
    }

    pub fn members(&self) -> &[Vec<u8>] {
        &self.members
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn config(&self) -> &StewardListConfig {
        &self.config
    }

    pub fn election_epoch(&self) -> u64 {
        self.election_epoch
    }

    /// Historical tag — the retry-round seed that was fed into the
    /// SHA256 sort when this list was accepted. Joiners carry this
    /// value in `GroupSync` so they can re-derive the same ordering.
    pub fn retry_round(&self) -> u32 {
        self.retry_round
    }
}

/// Shared precondition check for [`StewardList::generate`] and
/// [`StewardList::validate`].
fn check_generation_inputs(
    config: &StewardListConfig,
    member_ids: &[Vec<u8>],
    sn: usize,
) -> Result<(), CoreError> {
    if member_ids.is_empty() {
        return Err(CoreError::EmptyMembersList);
    }
    if !config.is_valid_size(sn, member_ids.len()) {
        return Err(CoreError::InvalidConfigSize);
    }
    Ok(())
}

/// Return candidate member indices sorted ascending by their steward
/// hash. Kept index-based so callers decide whether to clone or borrow.
fn sorted_steward_indices(
    election_epoch: u64,
    retry_round: u32,
    group_id: &[u8],
    member_ids: &[Vec<u8>],
) -> Vec<usize> {
    let mut scored: Vec<(Vec<u8>, usize)> = member_ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                compute_steward_hash(election_epoch, retry_round, id, group_id),
                i,
            )
        })
        .collect();
    scored.sort_by(|(a, _), (b, _)| a.cmp(b));
    scored.into_iter().map(|(_, i)| i).collect()
}

/// `SHA256(epoch || retry_round || member_id || group_id)`, big-endian
/// for the integers. `retry_round` is mixed in so successive election
/// retries within one MLS epoch propose different list compositions.
fn compute_steward_hash(
    epoch: u64,
    retry_round: u32,
    member_id: &[u8],
    group_id: &[u8],
) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(epoch.to_be_bytes());
    hasher.update(retry_round.to_be_bytes());
    hasher.update(member_id);
    hasher.update(group_id);
    hasher.finalize().to_vec()
}

// ── Default policy ──────────────────────────────────────────────────

/// Fallback ceiling on steward-election retries. One retry gives the
/// responsible proposer a second shot with a different list composition;
/// beyond that human/policy intervention is expected.
pub const DEFAULT_MAX_RETRIES: u32 = 1;

// ── Plug-in events ──────────────────────────────────────────────────

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
    /// into pending-update drain and `GroupSync` broadcast.
    ListInstalled {
        epoch: u64,
        retry_round: u32,
        len: usize,
    },
    /// `bump_retry` pushed `retry_round` past `max_retries`. Coordinator
    /// escalates to the Layer-3 `Deadlock` ECP.
    RetryExhausted { round: u32, max: u32 },
}

// ── Plug-in trait ───────────────────────────────────────────────────

/// Per-group steward list. Passive design — answers questions, does not
/// call out to MLS, consensus, or other plug-ins.
///
/// Eligibility predicates flow in from the coordinator: every "live"
/// position query takes a `Fn(&[u8]) -> bool` so the plug-in needn't
/// know about MLS membership or removal queues. When all candidates are
/// eligible, the predicate-based queries return the nominal rotation
/// position; otherwise they walk forward to the next eligible steward.
pub trait StewardListPlugin: Send + Sync + 'static {
    // ── Config & raw state ─────────────────────────────────────────

    /// Steward list bounds + protocol-level flags.
    fn config(&self) -> &StewardListConfig;

    /// Replace the active config — joiner sync path adopts the
    /// group-wide values. Preserves list and retry state; subsequent
    /// `install_list` calls use the new bounds.
    fn set_config(&mut self, config: StewardListConfig);

    /// Borrow the active list. `None` for joiners pre-`GroupSync`.
    fn current_list(&self) -> Option<&StewardList>;

    /// Epoch at which the active list was elected. `None` if no list.
    fn election_epoch(&self) -> Option<u64>;

    /// Current retry round (0 for fresh elections, bumped on each
    /// rejected proposal within the same MLS epoch). Distinct from the
    /// list's frozen `retry_round` historical tag.
    fn retry_round(&self) -> u32;

    /// Group-configured ceiling on steward-election retries. Joiners
    /// pick this up via `GroupSync`.
    fn max_retries(&self) -> u32;
    fn set_max_retries(&mut self, max: u32);

    // ── State predicates ───────────────────────────────────────────

    /// True iff `identity` sits in the active list.
    fn is_steward(&self, identity: &[u8]) -> bool;

    /// True iff `epoch` falls outside the list's covered window
    /// `[election_epoch, election_epoch + len)`. A new election MUST
    /// follow once the list is exhausted.
    fn is_exhausted(&self, epoch: u64) -> bool;

    // ── Position queries (eligibility-filtered) ────────────────────

    /// Steward responsible for `epoch`, walking the rotation past any
    /// candidate for whom `eligible` returns false. Pass `&|_| true`
    /// for the nominal position.
    fn epoch_steward(&self, epoch: u64, eligible: &dyn Fn(&[u8]) -> bool) -> Option<&[u8]>;

    /// Backup steward for `epoch` — distinct from the live epoch
    /// steward (resolving them together avoids the rotation collapsing
    /// both roles onto the same identity when an ineligible nominal
    /// shifts the walk).
    fn backup_steward(&self, epoch: u64, eligible: &dyn Fn(&[u8]) -> bool) -> Option<&[u8]>;

    /// Live epoch steward + backup, guaranteed distinct when ≥2 are
    /// eligible. Backup is `None` when fewer than two stewards are
    /// eligible.
    fn epoch_and_backup(
        &self,
        epoch: u64,
        eligible: &dyn Fn(&[u8]) -> bool,
    ) -> (Option<&[u8]>, Option<&[u8]>);

    /// Steward roster filtered by `eligible`. Used by the coordinator
    /// to build `GroupSync.steward_members` so joiners don't inherit
    /// ghosts or members queued for removal.
    fn steward_members(&self, eligible: &dyn Fn(&[u8]) -> bool) -> Vec<Vec<u8>>;

    /// Deterministic election proposer when the list exhausts. Walks
    /// rotation from index 0; returns `None` if no steward is eligible.
    fn election_proposer(&self, eligible: &dyn Fn(&[u8]) -> bool) -> Option<&[u8]>;

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

    /// Decide whether this node SHOULD file a steward-election
    /// proposal and, if so, return the proposal contents. Coordinator
    /// passes the candidate pool it built (MLS members minus
    /// pending-removal targets minus any extra excludes), the
    /// eligibility predicate (typically the same set as the pool),
    /// `recovery = true` to bypass the list-exhaustion gate, and the
    /// node's own identity. Plug-in handles authorization + ordering;
    /// coordinator handles `has_election_in_flight` + the I/O submit.
    fn propose_election(
        &self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        self_identity: &[u8],
        eligible: &dyn Fn(&[u8]) -> bool,
        recovery: bool,
    ) -> Result<ElectionDecision, CoreError>;

    /// Increment the retry round. Emits [`StewardListEvent::RetryExhausted`]
    /// once the new round exceeds `max_retries`.
    #[must_use]
    fn bump_retry(&mut self) -> Vec<StewardListEvent>;

    /// Reset the retry round to 0 (called on accepted election or
    /// successful commit).
    fn reset_retry(&mut self);
}

// ── Reference implementation ────────────────────────────────────────

/// Deterministic SHA256-sort steward list. Reference [`StewardListPlugin`]
/// implementation; one instance per group.
#[derive(Debug)]
pub struct DeterministicStewardList {
    list: Option<StewardList>,
    config: StewardListConfig,
    group_id: Vec<u8>,
    retry_round: u32,
    max_retries: u32,
}

impl DeterministicStewardList {
    /// Joiner-side: empty list. The coordinator fills it from the
    /// `GroupSync` it receives after the welcome.
    pub fn empty(group_id: impl Into<Vec<u8>>, config: StewardListConfig) -> Self {
        Self {
            list: None,
            config,
            group_id: group_id.into(),
            retry_round: 0,
            max_retries: DEFAULT_MAX_RETRIES,
        }
    }

    /// Creator-side: bootstrap with the creator as the sole steward at
    /// epoch 0. No election, no retries.
    pub fn with_creator(
        group_id: impl Into<Vec<u8>>,
        creator_identity: Vec<u8>,
        config: StewardListConfig,
    ) -> Result<Self, CoreError> {
        let group_id = group_id.into();
        let list = StewardList::generate(0, &group_id, &[creator_identity], 1, config.clone(), 0)?;
        Ok(Self {
            list: Some(list),
            config,
            group_id,
            retry_round: 0,
            max_retries: DEFAULT_MAX_RETRIES,
        })
    }
}

impl StewardListPlugin for DeterministicStewardList {
    fn config(&self) -> &StewardListConfig {
        &self.config
    }

    fn set_config(&mut self, config: StewardListConfig) {
        self.config = config;
    }

    fn current_list(&self) -> Option<&StewardList> {
        self.list.as_ref()
    }

    fn election_epoch(&self) -> Option<u64> {
        self.list.as_ref().map(|l| l.election_epoch())
    }

    fn retry_round(&self) -> u32 {
        self.retry_round
    }

    fn max_retries(&self) -> u32 {
        self.max_retries
    }

    fn set_max_retries(&mut self, max: u32) {
        self.max_retries = max;
    }

    fn is_steward(&self, identity: &[u8]) -> bool {
        self.list.as_ref().is_some_and(|l| l.contains(identity))
    }

    fn is_exhausted(&self, epoch: u64) -> bool {
        self.list.as_ref().is_some_and(|l| l.is_exhausted(epoch))
    }

    fn epoch_steward(&self, epoch: u64, eligible: &dyn Fn(&[u8]) -> bool) -> Option<&[u8]> {
        self.list
            .as_ref()
            .and_then(|l| l.live_steward_from(epoch, 0, eligible))
    }

    fn backup_steward(&self, epoch: u64, eligible: &dyn Fn(&[u8]) -> bool) -> Option<&[u8]> {
        // Resolve as a pair so the backup walk excludes the live epoch
        // steward — the standalone offset-1 lookup could collapse onto
        // the same identity once ineligibility shifts the rotation.
        self.list
            .as_ref()
            .and_then(|l| l.live_epoch_and_backup(epoch, eligible).1)
    }

    fn epoch_and_backup(
        &self,
        epoch: u64,
        eligible: &dyn Fn(&[u8]) -> bool,
    ) -> (Option<&[u8]>, Option<&[u8]>) {
        match self.list.as_ref() {
            Some(l) => l.live_epoch_and_backup(epoch, eligible),
            None => (None, None),
        }
    }

    fn steward_members(&self, eligible: &dyn Fn(&[u8]) -> bool) -> Vec<Vec<u8>> {
        self.list
            .as_ref()
            .map(|l| {
                l.members()
                    .iter()
                    .filter(|m| eligible(m))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn election_proposer(&self, eligible: &dyn Fn(&[u8]) -> bool) -> Option<&[u8]> {
        // Election proposer = nominal index 0 = the steward whose
        // rotation slot covers `election_epoch` itself.
        self.list
            .as_ref()
            .and_then(|l| l.live_steward_from(l.election_epoch(), 0, eligible))
    }

    fn install_list(
        &mut self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        sn: usize,
        retry_round: u32,
    ) -> Result<Vec<StewardListEvent>, CoreError> {
        let list = StewardList::generate(
            epoch,
            &self.group_id,
            candidate_pool,
            sn,
            self.config.clone(),
            retry_round,
        )?;
        let len = list.len();
        self.list = Some(list);
        Ok(vec![StewardListEvent::ListInstalled {
            epoch,
            retry_round,
            len,
        }])
    }

    fn validate_proposed(
        &self,
        proposed: &[Vec<u8>],
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        retry_round: u32,
    ) -> Result<bool, CoreError> {
        StewardList::validate(
            proposed,
            epoch,
            &self.group_id,
            candidate_pool,
            &self.config,
            retry_round,
        )
    }

    fn propose_election(
        &self,
        epoch: u64,
        candidate_pool: &[Vec<u8>],
        self_identity: &[u8],
        eligible: &dyn Fn(&[u8]) -> bool,
        recovery: bool,
    ) -> Result<ElectionDecision, CoreError> {
        if !recovery && !self.is_exhausted(epoch) {
            return Ok(ElectionDecision::Skip("steward list not exhausted"));
        }
        let is_authorized = self
            .election_proposer(eligible)
            .is_some_and(|proposer| proposer == self_identity);
        if !is_authorized {
            return Ok(ElectionDecision::Skip("not the responsible proposer"));
        }
        if candidate_pool.is_empty() {
            return Ok(ElectionDecision::Skip(
                "no eligible candidates after filter",
            ));
        }

        let retry_round = self.retry_round();
        let sn = self.config.compute_list_size(candidate_pool.len());
        let list = StewardList::generate(
            epoch,
            &self.group_id,
            candidate_pool,
            sn,
            self.config.clone(),
            retry_round,
        )?;
        Ok(ElectionDecision::Proposed {
            proposed_stewards: list.members().to_vec(),
            election_epoch: epoch,
            retry_round,
        })
    }

    fn maybe_auto_fill(
        &mut self,
        epoch: u64,
        members: &[Vec<u8>],
    ) -> Result<Vec<StewardListEvent>, CoreError> {
        // RFC §Steward list creation: when total membership drops below
        // `sn_min`, every member must be a steward. Re-derive over the
        // full member set with `retry_round = 0` (auto-fill is not an
        // election outcome).
        if members.len() >= self.config.sn_min {
            return Ok(Vec::new());
        }
        let sn = self.config.compute_list_size(members.len());
        self.install_list(epoch, members, sn, 0)
    }

    fn bump_retry(&mut self) -> Vec<StewardListEvent> {
        self.retry_round = self.retry_round.saturating_add(1);
        if self.retry_round > self.max_retries {
            vec![StewardListEvent::RetryExhausted {
                round: self.retry_round,
                max: self.max_retries,
            }]
        } else {
            Vec::new()
        }
    }

    fn reset_retry(&mut self) {
        self.retry_round = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: u8) -> Vec<u8> {
        vec![id; 20]
    }

    fn members(ids: &[u8]) -> Vec<Vec<u8>> {
        ids.iter().map(|&id| member(id)).collect()
    }

    fn config() -> StewardListConfig {
        StewardListConfig::new(1, 5).unwrap()
    }

    // ── StewardListConfig + StewardList unit tests ──

    #[test]
    fn test_config_validation() {
        let config = StewardListConfig::new(2, 5).unwrap();

        // Within [sn_min, sn_max]
        assert!(config.is_valid_size(3, 10));
        assert!(config.is_valid_size(2, 10));
        assert!(config.is_valid_size(5, 10));
        assert!(!config.is_valid_size(1, 10));
        assert!(!config.is_valid_size(6, 10));

        // Fewer members than sn_min → only `size == total` is valid.
        assert!(config.is_valid_size(1, 1));
        assert!(!config.is_valid_size(2, 1));
    }

    #[test]
    fn test_new_rejects_bad_bounds() {
        assert!(StewardListConfig::new(0, 5).is_err(), "sn_min == 0");
        assert!(StewardListConfig::new(5, 3).is_err(), "sn_min > sn_max");
    }

    #[test]
    fn test_generate_empty_members() {
        let config = StewardListConfig::new(1, 3).unwrap();
        assert!(StewardList::generate(0, b"group1", &[], 1, config, 0).is_err());
    }

    #[test]
    fn test_generate_invalid_sn() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        assert!(
            StewardList::generate(0, b"group1", &mems, 1, config.clone(), 0).is_err(),
            "below sn_min"
        );
        assert!(
            StewardList::generate(0, b"group1", &mems, 6, config, 0).is_err(),
            "above sn_max"
        );
    }

    #[test]
    fn test_deterministic_generation() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let group_id = b"test-group";

        let list1 = StewardList::generate(0, group_id, &mems, 3, config.clone(), 0).unwrap();
        let list2 = StewardList::generate(0, group_id, &mems, 3, config, 0).unwrap();

        assert_eq!(list1.members(), list2.members());
        assert_eq!(list1.len(), 3);
    }

    /// With the full candidate set and only the epoch differing, the order
    /// must shuffle for at least one epoch in a small window.
    #[test]
    fn test_different_epoch_shuffles() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let base = StewardList::generate(0, b"group", &mems, 5, config.clone(), 0).unwrap();
        let any_diff = (1..10).any(|e| {
            let other = StewardList::generate(e, b"group", &mems, 5, config.clone(), 0).unwrap();
            other.members() != base.members()
        });
        assert!(any_diff);
    }

    #[test]
    fn test_different_group_shuffles() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let base = StewardList::generate(0, b"group1", &mems, 5, config.clone(), 0).unwrap();
        let other = StewardList::generate(0, b"group2", &mems, 5, config, 0).unwrap();
        assert_ne!(base.members(), other.members());
    }

    #[test]
    fn test_member_order_does_not_affect_result() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems_a = members(&[1, 2, 3, 4, 5]);
        let mems_b = members(&[5, 3, 1, 4, 2]);

        let list_a = StewardList::generate(0, b"group", &mems_a, 3, config.clone(), 0).unwrap();
        let list_b = StewardList::generate(0, b"group", &mems_b, 3, config, 0).unwrap();

        assert_eq!(list_a.members(), list_b.members());
    }

    #[test]
    fn test_epoch_steward_rotation() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();
        let s0 = list.epoch_steward(0).unwrap().to_vec();
        let s1 = list.epoch_steward(1).unwrap().to_vec();
        let s2 = list.epoch_steward(2).unwrap().to_vec();

        assert_ne!(s0, s1);
        assert_ne!(s1, s2);
        assert_ne!(s0, s2);
    }

    /// Backup at epoch `e` is the epoch steward at `e + 1` (mod len).
    #[test]
    fn test_backup_steward() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();

        assert_eq!(list.backup_steward(0), list.epoch_steward(1));
        assert_eq!(list.backup_steward(1), list.epoch_steward(2));
        assert_eq!(list.backup_steward(2), list.epoch_steward(0));
    }

    #[test]
    fn test_list_exhaustion() {
        let config = StewardListConfig::new(2, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(5, b"group", &mems, 3, config, 0).unwrap();
        assert_eq!(list.election_epoch(), 5);

        // Covered epochs: [5, 8)
        assert!(!list.is_exhausted(5));
        assert!(!list.is_exhausted(7));
        assert!(list.is_exhausted(8));
        assert!(
            list.is_exhausted(4),
            "epochs before election_epoch are exhausted"
        );

        // Exhausted epochs return None from both rotation slots.
        assert!(list.epoch_steward(8).is_none());
        assert!(list.backup_steward(8).is_none());
    }

    #[test]
    fn test_validate_correct_list() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 3, config.clone(), 0).unwrap();
        let valid = StewardList::validate(list.members(), 0, b"group", &mems, &config, 0);
        assert!(valid.is_ok());
        assert!(valid.unwrap())
    }

    #[test]
    fn test_validate_tampered_list() {
        let config = StewardListConfig::new(2, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let mut list = StewardList::generate(0, b"group", &mems, 3, config.clone(), 0).unwrap();
        // Swap first two members
        list.members.swap(0, 1);

        let valid = StewardList::validate(list.members(), 0, b"group", &mems, &config, 0);
        assert!(valid.is_ok());
        assert!(!valid.unwrap())
    }

    #[test]
    fn test_validate_wrong_epoch() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 5, config.clone(), 0).unwrap();
        // Find an epoch that produces a different ordering
        let diff_epoch = (1..100u64)
            .find(|&e| {
                let o = StewardList::generate(e, b"group", &mems, 5, config.clone(), 0).unwrap();
                o.members() != list.members()
            })
            .expect("should differ within 100 epochs");

        let valid = StewardList::validate(list.members(), diff_epoch, b"group", &mems, &config, 0);
        assert!(valid.is_ok());
        assert!(!valid.unwrap());
    }

    /// `sn == total_members` so any change in the candidate set forces a
    /// different output ordering.
    #[test]
    fn test_validate_wrong_members() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let other_mems = members(&[1, 2, 3, 4, 6]);

        let list = StewardList::generate(0, b"group", &mems, 5, config.clone(), 0).unwrap();
        let valid = StewardList::validate(list.members(), 0, b"group", &other_mems, &config, 0);
        assert!(valid.is_ok());
        assert!(!valid.unwrap())
    }

    #[test]
    fn test_single_member() {
        let config = StewardListConfig::new(1, 3).unwrap();
        let mems = members(&[1]);

        let list = StewardList::generate(0, b"group", &mems, 1, config, 0).unwrap();
        assert_eq!(list.len(), 1);
        // With one steward, epoch and backup slots collapse to the same person.
        assert_eq!(list.epoch_steward(0), list.backup_steward(0));
        assert!(list.is_exhausted(1));
    }

    /// With everyone eligible, live == nominal. With the nominal filtered out,
    /// live rotates to the next eligible steward.
    #[test]
    fn test_live_steward_from_skips_ineligible() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();
        let nominal = list.epoch_steward(0).unwrap().to_vec();

        let all_eligible = |c: &[u8]| mems.iter().any(|m| m == c);
        assert_eq!(
            list.live_steward_from(0, 0, all_eligible),
            Some(nominal.as_slice())
        );

        let after: Vec<Vec<u8>> = mems.iter().filter(|m| **m != nominal).cloned().collect();
        let live = list
            .live_steward_from(0, 0, |c| after.iter().any(|m| m == c))
            .unwrap();
        assert_ne!(live, nominal.as_slice());
        assert!(after.iter().any(|m| m == live));
    }

    /// All stewards ineligible → both slots are None. One eligible → epoch
    /// resolves, backup stays None (can't be distinct from epoch).
    #[test]
    fn test_live_epoch_and_backup_all_ineligible_and_single_survivor() {
        let config = StewardListConfig::new(2, 2).unwrap();
        let mems = members(&[1, 2]);
        let list = StewardList::generate(0, b"group", &mems, 2, config, 0).unwrap();

        let (e, b) = list.live_epoch_and_backup(0, |_| false);
        assert!(e.is_none() && b.is_none());

        let survivor = mems[0].clone();
        let (e, b) = list.live_epoch_and_backup(0, |c| c == survivor.as_slice());
        assert_eq!(e.unwrap(), survivor.as_slice());
        assert!(b.is_none());
    }

    /// 3 stewards with the nominal epoch steward leaving: both slots must
    /// rotate and stay distinct. The pre-fix path would collapse them.
    #[test]
    fn test_live_epoch_and_backup_rotates_when_epoch_leaves() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();

        let nominal = list.epoch_steward(0).unwrap().to_vec();
        let (e, b) = list.live_epoch_and_backup(0, |c| c != nominal.as_slice());
        assert!(e.is_some() && b.is_some());
        assert_ne!(e.unwrap(), b.unwrap());
        assert_ne!(e.unwrap(), nominal.as_slice());
        assert_ne!(b.unwrap(), nominal.as_slice());
    }

    /// Happy path (no leavers) → matches the nominal `epoch_steward` /
    /// `backup_steward` assignment.
    #[test]
    fn test_live_epoch_and_backup_matches_nominal_when_all_eligible() {
        let config = StewardListConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();

        let (e, b) = list.live_epoch_and_backup(0, |_| true);
        assert_eq!(e, list.epoch_steward(0));
        assert_eq!(b, list.backup_steward(0));
    }

    #[test]
    fn test_sha256_sorting_is_ascending() {
        let config = StewardListConfig::new(5, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);

        let list = StewardList::generate(0, b"group", &mems, 5, config, 0).unwrap();
        let hashes: Vec<Vec<u8>> = list
            .members()
            .iter()
            .map(|m| compute_steward_hash(0, 0, m, b"group"))
            .collect();

        for window in hashes.windows(2) {
            assert!(window[0] < window[1], "hashes must be ascending");
        }
    }

    #[test]
    fn test_validate_rejects_empty_list() {
        let config = StewardListConfig::new(3, 5).unwrap();
        let mems = members(&[1, 2, 3, 4, 5]);
        let empty: Vec<Vec<u8>> = vec![];

        assert!(StewardList::validate(&empty, 0, b"group", &mems, &config, 0).is_err());
    }

    /// `sn_min=5` but only 3 members: `generate` clamps to total available.
    #[test]
    fn test_below_sn_min_uses_all_members() {
        let config = StewardListConfig::new(5, 10).unwrap();
        let mems = members(&[1, 2, 3]);

        let list = StewardList::generate(0, b"group", &mems, 3, config, 0).unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn test_large_group_subset_selection() {
        let config = StewardListConfig::new(3, 5).unwrap();
        let mems: Vec<Vec<u8>> = (1..=20).map(member).collect();

        let list = StewardList::generate(0, b"group", &mems, 5, config, 0).unwrap();
        assert_eq!(list.len(), 5);
        for steward in list.members() {
            assert!(mems.contains(steward));
        }
    }

    /// With 4 members and sn=2, different retry_round values MUST produce at
    /// least some different orderings. Otherwise the retry mechanism is broken.
    #[test]
    fn test_retry_rounds_produce_different_lists() {
        let config = StewardListConfig::new(1, 2).unwrap();
        let mems: Vec<Vec<u8>> = (1..=4u8).map(member).collect();

        let base = StewardList::generate(1, b"group", &mems, 2, config.clone(), 0).unwrap();
        let any_diff = (1..10u32).any(|r| {
            let other = StewardList::generate(1, b"group", &mems, 2, config.clone(), r).unwrap();
            other.members() != base.members()
        });
        assert!(
            any_diff,
            "retries should produce at least one different list"
        );
    }

    // ── Plug-in (DeterministicStewardList) tests ──

    #[test]
    fn empty_plugin_has_no_list() {
        let p = DeterministicStewardList::empty(b"g".to_vec(), config());
        assert!(p.current_list().is_none());
        assert!(!p.is_steward(&member(1)));
        assert!(!p.is_exhausted(0));
        assert_eq!(p.epoch_steward(0, &|_| true), None);
        assert_eq!(p.election_epoch(), None);
    }

    #[test]
    fn creator_bootstrap_makes_creator_a_steward() {
        let creator = member(1);
        let p = DeterministicStewardList::with_creator(b"g".to_vec(), creator.clone(), config())
            .unwrap();
        assert!(p.is_steward(&creator));
        assert_eq!(p.election_epoch(), Some(0));
        assert_eq!(p.current_list().unwrap().len(), 1);
    }

    #[test]
    fn install_emits_list_installed_event() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let events = p.install_list(0, &mems, 3, 0).unwrap();
        assert_eq!(
            events,
            vec![StewardListEvent::ListInstalled {
                epoch: 0,
                retry_round: 0,
                len: 3,
            }]
        );
        assert_eq!(p.current_list().unwrap().len(), 3);
        assert_eq!(p.election_epoch(), Some(0));
    }

    /// Filtering the nominal epoch steward out forces rotation to walk to
    /// the next eligible candidate.
    #[test]
    fn epoch_steward_filters_by_eligibility() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let nominal = p.epoch_steward(0, &|_| true).unwrap().to_vec();
        let next = p.epoch_steward(0, &|c| c != nominal.as_slice()).unwrap();
        assert_ne!(next, nominal.as_slice());
        assert!(mems.iter().any(|m| m == next));
    }

    #[test]
    fn epoch_and_backup_distinct_when_two_eligible() {
        let mut p =
            DeterministicStewardList::empty(b"g".to_vec(), StewardListConfig::new(3, 3).unwrap());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let (e, b) = p.epoch_and_backup(0, &|_| true);
        assert!(e.is_some() && b.is_some());
        assert_ne!(e.unwrap(), b.unwrap());
    }

    /// Single eligible survivor → epoch resolves, backup stays None
    /// (nothing to be distinct from).
    #[test]
    fn backup_is_none_when_only_one_eligible() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let survivor = mems[0].clone();
        let (e, b) = p.epoch_and_backup(0, &|c| c == survivor.as_slice());
        assert_eq!(e.unwrap(), survivor.as_slice());
        assert!(b.is_none());
    }

    /// `bump_retry` past `max_retries` emits `RetryExhausted` exactly
    /// once; default max is 1, so the second bump triggers it.
    #[test]
    fn bump_retry_emits_exhausted_after_max() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        assert!(p.bump_retry().is_empty());
        assert_eq!(p.retry_round(), 1);

        let events = p.bump_retry();
        assert_eq!(
            events,
            vec![StewardListEvent::RetryExhausted { round: 2, max: 1 }]
        );
        assert_eq!(p.retry_round(), 2);
    }

    #[test]
    fn reset_retry_clears_round() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let _ = p.bump_retry();
        let _ = p.bump_retry();
        assert_eq!(p.retry_round(), 2);
        p.reset_retry();
        assert_eq!(p.retry_round(), 0);
    }

    #[test]
    fn validate_proposed_against_self_derived_list() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();
        let proposed = p.current_list().unwrap().members().to_vec();
        assert!(p.validate_proposed(&proposed, 0, &mems, 0).unwrap());
    }

    #[test]
    fn validate_proposed_rejects_tampered_order() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();
        let mut tampered = p.current_list().unwrap().members().to_vec();
        tampered.swap(0, 1);
        assert!(!p.validate_proposed(&tampered, 0, &mems, 0).unwrap());
    }

    #[test]
    fn steward_members_returns_filtered_roster() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();
        let dropped = mems[0].clone();
        let filtered = p.steward_members(&|c| c != dropped.as_slice());
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|m| m != &dropped));
    }

    #[test]
    fn set_max_retries_updates_threshold() {
        let mut p = DeterministicStewardList::empty(b"g".to_vec(), config());
        p.set_max_retries(3);
        assert_eq!(p.max_retries(), 3);

        for _ in 0..3 {
            assert!(p.bump_retry().is_empty());
        }
        let events = p.bump_retry();
        assert_eq!(
            events,
            vec![StewardListEvent::RetryExhausted { round: 4, max: 3 }]
        );
    }

    #[test]
    fn election_proposer_walks_eligibility() {
        let mut p =
            DeterministicStewardList::empty(b"g".to_vec(), StewardListConfig::new(3, 3).unwrap());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let proposer = p.election_proposer(&|_| true).unwrap().to_vec();
        let next = p.election_proposer(&|c| c != proposer.as_slice()).unwrap();
        assert_ne!(next, proposer.as_slice());
    }
}
