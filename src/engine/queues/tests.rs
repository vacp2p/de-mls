use super::*;
use crate::engine::types::{CommitHash, MemberId, MembershipDelta, StagedFacts};
use crate::protos::de_mls::messages::v1::{
    CommitCandidate, ConversationUpdateRequest, ViolationEvidence,
};

fn member(id: u8) -> Vec<u8> {
    vec![id; 20]
}

fn remove_request(target: &[u8]) -> ConversationUpdateRequest {
    ConversationUpdateRequest::remove_member(target.to_vec())
}

fn candidate(tag: u8, sender: &[u8], epoch: u64) -> EarlyCandidate {
    let commit = vec![tag; 8];
    EarlyCandidate {
        hash: CommitHash::of(&commit),
        envelope: CommitCandidate {
            conversation_id: b"g".to_vec(),
            commit_message: commit,
            steward_member_id: sender.to_vec(),
            proposal_count: 1,
        },
        facts: StagedFacts {
            sender: MemberId::from(sender),
            epoch,
            actions: Vec::new(),
            proposal_count: 1,
            self_removed: false,
        },
    }
}

#[test]
fn resolved_cache_records_and_evicts_fifo() {
    let mut cache = BoundedSet::new(3);
    cache.insert(1);
    cache.insert(2);
    cache.insert(3);
    assert!(cache.contains(&1));
    assert!(cache.contains(&2));
    assert!(cache.contains(&3));

    cache.insert(4);
    assert!(!cache.contains(&1), "oldest entry must be evicted");
    assert!(cache.contains(&2));
    assert!(cache.contains(&3));
    assert!(cache.contains(&4));
}

#[test]
fn resolved_cache_dedupes_and_does_not_bump_position() {
    let mut cache = BoundedSet::new(3);
    cache.insert(1);
    cache.insert(2);
    cache.insert(1); // no-op: 1 stays in its original slot
    cache.insert(3);
    assert!(cache.contains(&1));
    assert!(cache.contains(&2));
    assert!(cache.contains(&3));

    // Fourth distinct id evicts the oldest (1), not a later entry.
    cache.insert(4);
    assert!(!cache.contains(&1));
    assert!(cache.contains(&2));
    assert!(cache.contains(&3));
    assert!(cache.contains(&4));
}

#[test]
fn bounded_set_clamps_zero_capacity_so_dedup_still_works() {
    let mut cache = BoundedSet::new(0);
    assert!(cache.insert(1), "first insert is new");
    assert!(
        cache.contains(&1),
        "capacity 0 must clamp to 1, not disable dedup"
    );
    assert!(!cache.insert(1), "duplicate must be rejected");
}

#[test]
fn mark_consensus_outcome_persists_in_resolved_cache() {
    let mut queues = EngineQueues::new(10);
    assert!(!queues.is_consensus_outcome_applied(42));
    queues.mark_consensus_outcome_applied(42);
    assert!(queues.is_consensus_outcome_applied(42));
}

fn insert_remove_member(queues: &mut EngineQueues, target: &[u8], proposal_id: ProposalId) {
    queues.insert_approved_proposal(proposal_id, remove_request(target));
}

/// `approved_proposals` order is FIFO regardless of proposal-id ordering.
#[test]
fn test_approved_proposals_preserve_fifo_across_mutations() {
    let mut queues = EngineQueues::new(10);

    insert_remove_member(&mut queues, &member(2), 500);
    insert_remove_member(&mut queues, &member(3), 100);
    insert_remove_member(&mut queues, &member(4), 300);
    let order: Vec<ProposalId> = queues.approved_proposals().keys().copied().collect();
    assert_eq!(order, vec![500, 100, 300]);

    // Re-inserting an existing id does not duplicate or reorder.
    insert_remove_member(&mut queues, &member(2), 500);
    let order: Vec<ProposalId> = queues.approved_proposals().keys().copied().collect();
    assert_eq!(order, vec![500, 100, 300]);

    queues.drain_approved_proposals();
    assert!(queues.approved_proposals().is_empty());
}

#[test]
fn test_urgent_commit_target_set_take_clears() {
    let mut queues = EngineQueues::new(10);
    assert!(queues.urgent_commit_target().is_none());

    let target = member(7);
    queues.set_urgent_commit_target(target.clone());
    assert_eq!(queues.urgent_commit_target(), Some(target.as_slice()));

    let taken = queues.take_urgent_commit_target().unwrap();
    assert_eq!(taken, target);
    assert!(queues.urgent_commit_target().is_none());
}

#[test]
fn test_drop_approved_removals_for_target() {
    let mut queues = EngineQueues::new(10);
    let victim = member(7);
    let bystander = member(9);

    insert_remove_member(&mut queues, &victim, 100);
    insert_remove_member(&mut queues, &victim, 101);
    insert_remove_member(&mut queues, &bystander, 200);
    assert_eq!(queues.approved_proposals_count(), 3);

    queues.drop_approved_removals_for(&victim);

    assert_eq!(queues.approved_proposals_count(), 1);
    assert!(queues.approved_proposals().contains_key(&200));
    assert!(!queues.approved_proposals().contains_key(&100));
    assert!(!queues.approved_proposals().contains_key(&101));
}

fn buffer_remove_at(queues: &mut EngineQueues, target: &[u8], epoch: u64) {
    assert!(queues.insert_pending_update(remove_request(target), epoch));
}

/// Reducing `pending_update_max_epochs` (e.g. via a tightened
/// `ConversationSync`) must expire entries whose age now exceeds the new max.
/// Cutoff math: `current_epoch - max_age`; entries with
/// `first_seen_epoch < cutoff` are dropped.
#[test]
fn test_expire_pending_updates_drops_entries_older_than_max_age() {
    let mut queues = EngineQueues::new(10);
    let stale = member(7);
    let fresh = member(9);

    buffer_remove_at(&mut queues, &stale, 0);
    buffer_remove_at(&mut queues, &fresh, 4);
    assert_eq!(queues.pending_update_count(), 2);

    let expired = queues.expire_pending_updates(5, 1);

    assert_eq!(expired, vec![stale.clone()]);
    assert_eq!(queues.pending_update_count(), 1);
    assert!(queues.has_pending_update(&fresh));
    assert!(!queues.has_pending_update(&stale));
}

/// `max_age = 0` keeps only entries from the current epoch — the
/// boundary case a tightened sync hits when shrinking the window.
#[test]
fn test_expire_pending_updates_max_age_zero_keeps_only_current_epoch() {
    let mut queues = EngineQueues::new(10);
    let prior = member(7);
    let current = member(9);

    buffer_remove_at(&mut queues, &prior, 4);
    buffer_remove_at(&mut queues, &current, 5);

    let expired = queues.expire_pending_updates(5, 0);

    assert_eq!(expired, vec![prior.clone()]);
    assert!(queues.has_pending_update(&current));
    assert!(!queues.has_pending_update(&prior));
}

/// A score-below-threshold removal is a live change for its target across
/// its whole lifecycle: while the ECP is still voting (via the in-flight
/// index) and after it resolves into a queued `RemoveMember` (via the
/// approved queue). `active_proposal_targets` sees it the whole time, so a
/// second score removal for the same target is a no-op rather than a
/// duplicate round.
#[test]
fn score_removal_target_stays_covered_through_its_lifecycle() {
    let mut queues = EngineQueues::new(10);
    let creator = member(1);
    let target = member(7);

    let evidence =
        ViolationEvidence::score_below_threshold(target.clone(), 0, 0).with_creator(creator);
    let request = evidence.into_update_request().unwrap();
    let proposal_id = 300;

    // Voting: the ECP hasn't produced a RemoveMember yet, but the target is
    // already covered — the blind spot the ECP-aware index closes.
    queues.track_voting_proposal(proposal_id, &request);
    assert!(queues.is_voting(proposal_id));
    assert!(
        queues.active_proposal_targets().contains(target.as_slice()),
        "the in-flight ECP already covers its target"
    );
    assert!(!queues.has_approved_removal(&target));

    // Resolved: the ECP transforms into a queued RemoveMember, and the
    // target stays covered with no gap.
    queues.remove_voting_proposal(proposal_id);
    queues.insert_approved_proposal(proposal_id, remove_request(&target));
    assert!(queues.has_approved_removal(&target));
    assert!(
        queues.active_proposal_targets().contains(target.as_slice()),
        "the queued RemoveMember still covers the target"
    );
}

/// A removal targeting a member also makes them steward-ineligible, so the
/// list never picks a member MLS forbids from committing its own removal.
#[test]
fn steward_eligibility_skips_non_members_and_queued_removals() {
    let mut queues = EngineQueues::new(10);
    let victim = member(7);
    let bystander = member(9);
    let outsider = member(11);
    insert_remove_member(&mut queues, &victim, 100);

    let members = vec![victim.clone(), bystander.clone()];
    let eligible = queues.steward_eligibility(&members);
    assert!(eligible(&bystander));
    assert!(!eligible(&victim));
    assert!(!eligible(&outsider));
}

/// The join epoch is per member id and survives every later commit, so a
/// sync recomputing the unsettled set at a past epoch gets the same answer.
/// A departure drops the entry, so a re-join is unsettled again.
#[test]
fn membership_bookkeeping_tracks_join_epochs_and_clears_departures() {
    let mut queues = EngineQueues::new(10);
    let joiner = member(2);
    let leaver = member(3);

    // The leaver is buffered and queued for removal before the commit lands.
    buffer_remove_at(&mut queues, &leaver, 4);
    insert_remove_member(&mut queues, &leaver, 100);
    queues.member_join_epoch.insert(leaver.clone(), 1);

    queues.store_membership_delta(MembershipDelta {
        added: vec![joiner.clone()],
        removed: vec![leaver.clone()],
    });
    queues.apply_membership_delta_bookkeeping(5);

    assert!(!queues.has_pending_update(&leaver));
    assert!(!queues.has_approved_removal(&leaver));
    assert!(queues.is_settled(&leaver, 5), "an untracked id is settled");

    assert!(
        !queues.is_settled(&joiner, 5),
        "unsettled in its join epoch"
    );
    assert!(queues.is_settled(&joiner, 6));
    assert_eq!(
        queues.settled_members(std::slice::from_ref(&joiner), 6),
        vec![joiner]
    );

    // The delta stays available for the reconcile step that consumes it.
    let delta = queues.take_membership_delta();
    assert_eq!(delta.added.len(), 1);
    assert!(queues.take_membership_delta().is_empty());
}

/// A buffered invite is keyed by the joiner's id, which is unchanged by
/// seating — so the commit that seats them drops the buffer entry.
#[test]
fn membership_bookkeeping_drops_the_buffered_update_for_a_seated_joiner() {
    let mut queues = EngineQueues::new(10);
    let joiner = member(2);

    // A removal request stands in for any buffered update keyed by target.
    buffer_remove_at(&mut queues, &joiner, 4);
    queues.store_membership_delta(MembershipDelta {
        added: vec![joiner.clone()],
        removed: Vec::new(),
    });
    queues.apply_membership_delta_bookkeeping(5);

    assert!(!queues.has_pending_update(&joiner));
}

/// The stash keeps one entry per commit hash, holds only the current
/// epoch's candidates, and stops at `max`.
#[test]
fn early_candidates_dedupe_by_hash_and_drop_stale_epochs() {
    let mut queues = EngineQueues::new(10);
    let steward = member(2);

    queues.stash_early_candidate(5, candidate(1, &steward, 5), 2);
    queues.stash_early_candidate(5, candidate(1, &steward, 5), 2);
    assert_eq!(queues.take_early_candidates(5).len(), 1, "deduped by hash");

    queues.stash_early_candidate(5, candidate(1, &steward, 5), 2);
    queues.stash_early_candidate(5, candidate(2, &steward, 5), 2);
    queues.stash_early_candidate(5, candidate(3, &steward, 5), 2);
    assert_eq!(queues.take_early_candidates(5).len(), 2, "capped at max");

    // A candidate tagged with an older epoch is dropped by the next stash.
    queues.stash_early_candidate(5, candidate(1, &steward, 5), 2);
    queues.stash_early_candidate(6, candidate(2, &steward, 6), 2);
    let taken = queues.take_early_candidates(6);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].facts.epoch, 6);
    assert!(queues.take_early_candidates(6).is_empty(), "stash cleared");
}
