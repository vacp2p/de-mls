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

use crate::core::{
    error::CoreError,
    steward_list::{ProtocolConfig, StewardList},
};

// ── Default policy ──────────────────────────────────────────────────

/// Fallback ceiling on steward-election retries. One retry gives the
/// responsible proposer a second shot with a different list composition;
/// beyond that human/policy intervention is expected.
pub const DEFAULT_MAX_RETRIES: u32 = 1;

// ── Plug-in events ──────────────────────────────────────────────────

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
    fn config(&self) -> &ProtocolConfig;

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
    config: ProtocolConfig,
    group_id: Vec<u8>,
    retry_round: u32,
    max_retries: u32,
}

impl DeterministicStewardList {
    /// Joiner-side: empty list. The coordinator fills it from the
    /// `GroupSync` it receives after the welcome.
    pub fn empty(group_id: impl Into<Vec<u8>>, config: ProtocolConfig) -> Self {
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
        config: ProtocolConfig,
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
    fn config(&self) -> &ProtocolConfig {
        &self.config
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

    fn config() -> ProtocolConfig {
        ProtocolConfig::new(1, 5).unwrap()
    }

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
            DeterministicStewardList::empty(b"g".to_vec(), ProtocolConfig::new(3, 3).unwrap());
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
            DeterministicStewardList::empty(b"g".to_vec(), ProtocolConfig::new(3, 3).unwrap());
        let mems = members(&[1, 2, 3]);
        let _ = p.install_list(0, &mems, 3, 0).unwrap();

        let proposer = p.election_proposer(&|_| true).unwrap().to_vec();
        let next = p.election_proposer(&|c| c != proposer.as_slice()).unwrap();
        assert_ne!(next, proposer.as_slice());
    }
}
