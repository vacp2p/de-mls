//! Per-group state container for app-level operations.
//!
//! This module provides [`Group`], which holds app-level state for
//! a single group: proposal tracking, steward status, and freeze-round candidate buffer.
//!
//! **Note**: MLS cryptographic state is managed by `MlsService` internally.
//! This handle only tracks application-layer concerns.
//!
//! # Architecture
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │                        Group                                 │
//! ├──────────────────────────────────────────────────────────────┤
//! │  group_name      │  Human-readable group identifier          │
//! │  steward_list    │  Active steward list for epoch rotation   │
//! │  proposals       │  Voting + approved proposal queues        │
//! │  freeze_round    │  Candidate buffer for commit selection    │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Lifecycle
//!
//! **Creating a group (as steward):**
//! ```ignore
//! mls_service.create_group(group_name)?;
//! let config = ProtocolConfig::new(1, 5).unwrap();
//! let group = Group::new_as_creator(group_name, mls_service.wallet_bytes(), config);
//! // group.is_steward() == true
//! ```
//!
//! **Joining a group (as member):**
//! ```ignore
//! let config = ProtocolConfig::new(1, 5).unwrap();
//! let group = Group::new_as_joiner(group_name, self_identity, config);
//! // group.is_steward() == false
//!
//! // Later, when welcome is received:
//! mls_service.join_group(&welcome)?;
//! // MLS state now exists — check via mls_service.has_group(group_name)
//! ```
//!
//! # Proposal Flow
//!
//! The group tracks proposals through their lifecycle:
//!
//! ```text
//! 1. store_voting_proposal()   →  Proposal created, waiting for votes
//! 2. mark_proposal_as_approved()  →  Consensus reached, ready for commit
//!    OR mark_proposal_as_rejected()  →  Consensus rejected, discard
//! 3. approved_proposals()      →  Steward reads approved proposals
//! 4. clear_approved_proposals()  →  After commit, archive to history
//! ```
//!
//! Non-owners (members who didn't create the proposal) use:
//! - `insert_approved_proposal()` - Add proposal directly to approved queue

use std::collections::{HashMap, HashSet, VecDeque};

use crate::core::CoreError;
use crate::core::proposals::{CurrentEpochProposals, ProposalId};
use crate::core::steward_list::{ProtocolConfig, StewardList};
use crate::protos::de_mls::messages::v1::{CommitCandidate, GroupUpdateRequest};

/// Maximum number of committed batch hashes to remember for dedup.
const MAX_COMMITTED_HASHES: usize = 10;

/// A commit candidate buffered during freeze for later selection.
#[derive(Clone, Debug)]
pub(crate) struct BufferedCommitCandidate {
    pub candidate_msg: CommitCandidate,
    pub commit_hash: Vec<u8>,
    pub is_local_candidate: bool,
    pub welcome_bytes: Option<Vec<u8>>,
}

/// In-memory freeze-round state for deterministic selection.
#[derive(Clone, Debug)]
pub(crate) struct FreezeRound {
    pub epoch: u64,
    pub selection_locked: bool,
    pub candidates: Vec<BufferedCommitCandidate>,
}

/// Handle for a single MLS group's app-level state.
///
/// Contains state needed for group operations:
/// - Steward flag indicating whether this user batches commits
/// - Proposal queues for tracking voting and approved proposals
///
/// **Note**: MLS cryptographic state (encryption keys, group members) is
/// managed by `MlsService`. Use `mls_service.encrypt()`, `mls_service.decrypt()`,
/// etc. for MLS operations.
///
/// # Thread Safety
///
/// The `Group` should be wrapped in `RwLock` or similar by the
/// application layer (see `User.groups` in the app module).
///
/// # Steward vs Member
///
/// - **Steward**: Creates proposals, collects votes, batches approved proposals
///   into MLS commits. Created via `new_as_creator()`.
/// - **Member**: Votes on proposals, receives commits. Created via `new_as_joiner()`.
#[derive(Clone, Debug)]
pub struct Group {
    /// The name of the group.
    group_name: String,
    /// This user's wallet identity (for deriving steward status from the list).
    self_identity: Vec<u8>,
    /// Proposal lifecycle tracking (voting → approved → archived).
    proposals: CurrentEpochProposals,
    /// Active steward list for the current epoch window.
    /// `Some` after bootstrap (creator) or sync (joiner). `None` only for joiners pre-sync.
    steward_list: Option<StewardList>,
    /// Protocol configuration (steward list bounds and protocol-level flags).
    protocol_config: ProtocolConfig,
    /// Active emergency criteria proposals not yet finalized by consensus.
    /// While non-empty, lower-priority proposals MUST be blocked (RFC §Partial Freeze).
    active_emergency_ids: HashSet<ProposalId>,
    /// Members with pending score-based removal ECPs (dedup to prevent duplicates).
    pending_removal_targets: HashSet<Vec<u8>>,
    /// Recent commit hashes for dedup.
    committed_batch_hashes: VecDeque<Vec<u8>>,
    /// Freeze-round candidate buffer for deterministic selection.
    freeze_round: Option<FreezeRound>,
}

impl Group {
    fn new_base(group_name: &str, self_identity: Vec<u8>, protocol_config: ProtocolConfig) -> Self {
        Self {
            group_name: group_name.to_string(),
            self_identity,
            proposals: CurrentEpochProposals::new(),
            steward_list: None,
            protocol_config,
            active_emergency_ids: HashSet::new(),
            pending_removal_targets: HashSet::new(),
            committed_batch_hashes: VecDeque::new(),
            freeze_round: None,
        }
    }

    /// Create a new group handle for a joining member (not yet steward).
    pub fn new_as_joiner(
        group_name: &str,
        self_identity: Vec<u8>,
        protocol_config: ProtocolConfig,
    ) -> Self {
        Self::new_base(group_name, self_identity, protocol_config)
    }

    /// Create a new group handle for the group creator, initializing the steward list.
    pub fn new_as_creator(
        group_name: &str,
        creator_identity: Vec<u8>,
        protocol_config: ProtocolConfig,
    ) -> Result<Self, CoreError> {
        let mut group = Self::new_base(
            group_name,
            creator_identity.clone(),
            protocol_config.clone(),
        );
        let list = StewardList::generate(
            0,
            group_name.as_bytes(),
            &[creator_identity],
            1,
            protocol_config,
        )?;
        group.steward_list = Some(list);

        Ok(group)
    }

    /// Get the group name.
    pub fn group_name(&self) -> &str {
        &self.group_name
    }

    /// Get the group name as bytes.
    pub fn group_name_bytes(&self) -> &[u8] {
        self.group_name.as_bytes()
    }

    /// Whether subset commit candidates are allowed during selection.
    pub fn allow_subset_candidates(&self) -> bool {
        self.protocol_config.allow_subset_candidates
    }

    /// Derived from `steward_list.contains(self_identity)`. Returns `false`
    /// for joiners before they receive the list via sync.
    pub fn is_steward(&self) -> bool {
        self.steward_list
            .as_ref()
            .is_some_and(|l| l.contains(&self.self_identity))
    }

    // ─────────────────────────── Proposal Handle Operations ───────────────────────────

    /// Check if this user owns (created) the given proposal.
    pub fn is_owner_of_proposal(&self, proposal_id: ProposalId) -> bool {
        self.proposals.is_owner_of_proposal(proposal_id)
    }

    /// Get the count of approved proposals waiting to be committed.
    pub fn approved_proposals_count(&self) -> usize {
        self.proposals.approved_proposals_count()
    }

    /// Get a copy of all approved proposals.
    pub fn approved_proposals(&self) -> HashMap<ProposalId, GroupUpdateRequest> {
        self.proposals.approved_proposals()
    }

    /// Move a proposal from voting to approved queue.
    pub fn mark_proposal_as_approved(&mut self, proposal_id: ProposalId) {
        self.proposals.move_proposal_to_approved(proposal_id);
    }

    /// Remove a proposal from the voting queue (rejected or failed).
    pub fn mark_proposal_as_rejected(&mut self, proposal_id: ProposalId) {
        self.proposals.remove_voting_proposal(proposal_id);
    }

    /// Store a newly created proposal in the voting queue.
    pub fn store_voting_proposal(&mut self, proposal_id: ProposalId, proposal: GroupUpdateRequest) {
        self.proposals.add_voting_proposal(proposal_id, proposal);
    }

    /// Insert a proposal directly into the approved queue.
    pub fn insert_approved_proposal(
        &mut self,
        proposal_id: ProposalId,
        proposal: GroupUpdateRequest,
    ) {
        self.proposals.add_proposal(proposal_id, proposal);
    }

    /// Remove a single proposal from the approved queue.
    pub fn remove_approved_proposal(&mut self, proposal_id: ProposalId) {
        self.proposals.remove_approved_proposal(proposal_id);
    }

    /// Clear approved proposals after a commit, archiving to history.
    ///
    /// The MLS epoch advances automatically when the commit is merged by OpenMLS,
    /// so no manual epoch increment is needed here.
    pub fn clear_approved_proposals(&mut self) {
        self.proposals.clear_approved_proposals();
    }

    /// Reject all currently approved proposals without advancing epoch.
    ///
    /// Used when freeze times out with no valid candidate selected.
    pub fn reject_all_approved_proposals(&mut self) {
        self.proposals.discard_approved_proposals();
    }

    /// Reject all currently voting proposals without advancing epoch.
    ///
    /// Used alongside `reject_all_approved_proposals()` when freeze times out
    /// with no valid candidate selected.
    pub fn reject_all_voting_proposals(&mut self) {
        self.proposals.clear_voting_proposals();
    }

    /// Get the epoch history (past batches of approved proposals).
    pub fn epoch_history(&self) -> &VecDeque<HashMap<ProposalId, GroupUpdateRequest>> {
        self.proposals.epoch_history()
    }

    // ─────────────────────────── Partial Freeze (RFC §Partial Freeze Semantics) ───────────────────────────

    /// Record an active emergency criteria proposal.
    pub fn observe_emergency(&mut self, proposal_id: ProposalId) {
        self.active_emergency_ids.insert(proposal_id);
    }

    /// Mark an emergency criteria proposal as finalized.
    pub fn resolve_emergency(&mut self, proposal_id: ProposalId) {
        self.active_emergency_ids.remove(&proposal_id);
    }

    /// Check if any emergency criteria proposal is active (partial freeze).
    pub fn has_active_emergency(&self) -> bool {
        !self.active_emergency_ids.is_empty()
    }

    // ─────────────────────────── Removal Dedup ───────────────────────────

    /// Record that a score-based removal ECP has been submitted for this member.
    pub fn observe_pending_removal(&mut self, member_id: Vec<u8>) {
        self.pending_removal_targets.insert(member_id);
    }

    /// Check if a removal ECP is already pending for this member.
    pub fn has_pending_removal(&self, member_id: &[u8]) -> bool {
        self.pending_removal_targets.contains(member_id)
    }

    /// Mark a removal ECP as finalized (resolved regardless of outcome).
    pub fn resolve_pending_removal(&mut self, member_id: &[u8]) {
        self.pending_removal_targets.remove(member_id);
    }

    // ─────────────────────────── Steward List Operations ───────────────────────────

    /// Get the active steward list, if any.
    pub fn steward_list(&self) -> Option<&StewardList> {
        self.steward_list.as_ref()
    }

    /// Get the protocol config.
    pub fn protocol_config(&self) -> &ProtocolConfig {
        &self.protocol_config
    }

    /// Get the epoch steward identity from the list, if available.
    pub fn epoch_steward(&self, epoch: u64) -> Option<&[u8]> {
        self.steward_list
            .as_ref()
            .and_then(|l| l.epoch_steward(epoch))
    }

    /// Check if the steward list is exhausted at the given epoch.
    pub fn is_steward_list_exhausted(&self, epoch: u64) -> bool {
        self.steward_list
            .as_ref()
            .is_some_and(|l| l.is_exhausted(epoch))
    }

    /// Generate a steward list and set it on the group.
    pub fn generate_and_set_steward_list(
        &mut self,
        epoch: u64,
        member_ids: &[Vec<u8>],
        sn: usize,
    ) -> Result<(), CoreError> {
        let config = self.protocol_config.clone();
        let list =
            StewardList::generate(epoch, self.group_name.as_bytes(), member_ids, sn, config)?;
        self.steward_list = Some(list);
        Ok(())
    }

    // ─────────────────────────── Freeze Round Operations ───────────────────────────

    fn build_freeze_round(&self, epoch: u64) -> FreezeRound {
        FreezeRound {
            epoch,
            selection_locked: false,
            candidates: Vec::new(),
        }
    }

    /// Ensure a freeze round exists for the given MLS epoch.
    ///
    /// If absent or stale, initializes one with the current approved proposal IDs.
    /// The `epoch` parameter should be the current MLS epoch from `MlsService::current_epoch()`.
    pub(crate) fn ensure_freeze_round(&mut self, epoch: u64) {
        if matches!(self.freeze_round, Some(ref round) if round.epoch == epoch) {
            return;
        }
        self.freeze_round = Some(self.build_freeze_round(epoch));
    }

    /// Start a new freeze round for the given MLS epoch.
    ///
    /// Existing round state is replaced.
    pub fn start_freeze_round(&mut self, epoch: u64) {
        self.freeze_round = Some(self.build_freeze_round(epoch));
    }

    /// Add a validated candidate to the active freeze round.
    ///
    /// Returns `true` if buffered, `false` if ignored (locked round or duplicate).
    /// The `epoch` parameter should be the current MLS epoch from `MlsService::current_epoch()`.
    pub(crate) fn add_freeze_candidate(
        &mut self,
        candidate: BufferedCommitCandidate,
        epoch: u64,
    ) -> bool {
        self.ensure_freeze_round(epoch);
        let Some(round) = self.freeze_round.as_mut() else {
            return false;
        };

        if round.epoch != epoch || round.selection_locked {
            return false;
        }

        if round
            .candidates
            .iter()
            .any(|c| c.commit_hash == candidate.commit_hash)
        {
            return false;
        }

        round.candidates.push(candidate);
        true
    }

    /// Mark the active freeze round as selection-locked.
    /// The `epoch` parameter should be the current MLS epoch from `MlsService::current_epoch()`.
    pub(crate) fn lock_freeze_round_selection(&mut self, epoch: u64) {
        if let Some(round) = self.freeze_round.as_mut() {
            if round.epoch == epoch {
                round.selection_locked = true;
            }
        }
    }

    /// Read-only access to the active freeze round.
    pub(crate) fn freeze_round(&self) -> Option<&FreezeRound> {
        self.freeze_round.as_ref()
    }

    /// Get the number of buffered commit candidates in the active freeze round.
    pub fn freeze_candidate_count(&self) -> usize {
        self.freeze_round
            .as_ref()
            .map(|r| r.candidates.len())
            .unwrap_or(0)
    }

    /// Clear freeze-round state.
    pub(crate) fn clear_freeze_round(&mut self) {
        self.freeze_round = None;
    }

    // ─────────────────────────── Dedup Operations ───────────────────────────

    /// Check if a commit hash has already been committed (in committed history).
    ///
    /// Note: freeze round buffer dedup is handled separately by `add_freeze_candidate`.
    pub(crate) fn is_duplicate_commit_candidate(&self, commit_hash: &[u8]) -> bool {
        self.committed_batch_hashes
            .iter()
            .any(|ch| ch == commit_hash)
    }

    /// Record a committed batch's hash for future dedup.
    pub(crate) fn record_committed_batch(&mut self, commit_hash: Vec<u8>) {
        if self.committed_batch_hashes.len() >= MAX_COMMITTED_HASHES {
            self.committed_batch_hashes.pop_front();
        }
        self.committed_batch_hashes.push_back(commit_hash);
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

    fn default_config() -> ProtocolConfig {
        ProtocolConfig::new(1, 5).unwrap()
    }

    #[test]
    fn test_new_as_creator_with_protocol_config() {
        let config = ProtocolConfig::new(1, 3).unwrap();
        let creator = member(1);
        let group = Group::new_as_creator("test-group", creator.clone(), config).unwrap();

        assert!(group.is_steward());
        assert!(group.steward_list().is_some());

        let list = group.steward_list().unwrap();
        assert_eq!(list.len(), 1);
        assert!(list.contains(&creator));
    }

    #[test]
    fn test_set_steward_list_on_joiner() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_joiner("test-group", member(1), config.clone());
        assert!(group.steward_list().is_none());

        let mems = members(&[1, 2, 3]);
        group.generate_and_set_steward_list(0, &mems, 3).unwrap();

        assert!(group.steward_list().is_some());
        assert_eq!(group.steward_list().unwrap().len(), 3);
    }

    #[test]
    fn test_epoch_steward() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();

        group.generate_and_set_steward_list(0, &mems, 3).unwrap();

        // epoch_steward should delegate to the list
        for epoch in 0..3 {
            assert_eq!(
                group.epoch_steward(epoch),
                group.steward_list().unwrap().epoch_steward(epoch)
            );
        }

        // Exhausted epoch returns None
        assert!(group.epoch_steward(3).is_none());
    }

    #[test]
    fn test_epoch_steward_no_list() {
        // Joiner pre-sync: has config but no list yet
        let group = Group::new_as_joiner("test-group", member(1), default_config());

        // No list means None for any epoch
        assert!(group.epoch_steward(0).is_none());
    }

    #[test]
    fn test_is_steward_list_exhausted_boundary() {
        let config = ProtocolConfig::new(3, 3).unwrap();
        let mems = members(&[1, 2, 3]);

        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        group.generate_and_set_steward_list(5, &mems, 3).unwrap();

        // List covers epochs 5, 6, 7
        assert!(!group.is_steward_list_exhausted(5));
        assert!(!group.is_steward_list_exhausted(6));
        assert!(!group.is_steward_list_exhausted(7));

        // Epoch 8 is beyond the list
        assert!(group.is_steward_list_exhausted(8));

        // Before start is also exhausted
        assert!(group.is_steward_list_exhausted(4));
    }

    #[test]
    fn test_is_steward_list_exhausted_no_list() {
        // Joiner pre-sync: has config but no list yet
        let group = Group::new_as_joiner("test-group", member(1), default_config());

        // No list means not exhausted
        assert!(!group.is_steward_list_exhausted(0));
    }

    #[test]
    fn test_generate_and_set_steward_list() {
        let config = ProtocolConfig::new(2, 5).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();
        assert_eq!(group.steward_list().unwrap().len(), 1);

        let mems = members(&[1, 2, 3, 4]);
        assert!(group.generate_and_set_steward_list(1, &mems, 4).is_ok());

        let list = group.steward_list().unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list.start_epoch(), 1);
    }

    #[test]
    fn test_generate_and_set_caps_at_sn_max() {
        let config = ProtocolConfig::new(2, 3).unwrap();
        let mut group = Group::new_as_creator("test-group", member(1), config).unwrap();

        // sn=3 capped by config.sn_max=3
        let mems = members(&[1, 2, 3, 4, 5]);
        assert!(group.generate_and_set_steward_list(0, &mems, 3).is_ok());
        assert_eq!(group.steward_list().unwrap().len(), 3);
    }

    #[test]
    fn test_steward_flag_derived_from_list() {
        let mut group = Group::new_as_joiner("test-group", member(1), default_config());
        assert!(!group.is_steward());

        // After generating a steward list that includes member(1), is_steward should be true
        let mems = members(&[1, 2, 3]);
        group.generate_and_set_steward_list(0, &mems, 3).unwrap();
        assert!(group.is_steward());

        // A joiner whose identity is not in the list should not be steward
        let handle2 = Group::new_as_joiner("test-group", member(99), default_config());
        assert!(!handle2.is_steward());
    }
}
