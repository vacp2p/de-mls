use super::*;
use crate::engine::types::MembershipDelta;
use crate::protos::de_mls::messages::v1::{ConversationUpdateRequest, ViolationEvidence};

fn member(id: u8) -> Vec<u8> {
    vec![id; 20]
}

fn remove_request(target: &[u8]) -> ConversationUpdateRequest {
    ConversationUpdateRequest::remove_member(target.to_vec())
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
    let mut queues = EngineQueues::new();
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
    let mut queues = EngineQueues::new();

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
    let mut queues = EngineQueues::new();
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
    let mut queues = EngineQueues::new();
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

/// A score-below-threshold removal is a live change for its target across
/// its whole lifecycle: while the ECP is still voting (via the in-flight
/// index) and after it resolves into a queued `RemoveMember` (via the
/// approved queue). `active_proposal_targets` sees it the whole time, so a
/// second score removal for the same target is a no-op rather than a
/// duplicate round.
#[test]
fn score_removal_target_stays_covered_through_its_lifecycle() {
    let mut queues = EngineQueues::new();
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
    let mut queues = EngineQueues::new();
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
    let mut queues = EngineQueues::new();
    let joiner = member(2);
    let leaver = member(3);

    // The leaver is queued for removal before the commit lands.
    insert_remove_member(&mut queues, &leaver, 100);
    queues.member_join_epoch.insert(leaver.clone(), 1);

    let delta = MembershipDelta {
        added: vec![joiner.clone()],
        removed: vec![leaver.clone()],
    };
    queues.apply_membership_delta_bookkeeping(5, &delta);

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
}
