use super::*;
use crate::engine::types::{Action, ActionKind, MemberId, MembershipDelta};
use crate::protos::de_mls::messages::v1::{
    ConversationUpdateRequest, MemberInvite, ViolationEvidence,
};

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
}

fn update_request(owner: &[u8]) -> ConversationUpdateRequest {
    ConversationUpdateRequest::leaf_update(owner.to_vec(), b"update".to_vec())
}

#[test]
fn has_approved_change_matches_kind_and_member() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(1, update_request(&member(2)));
    insert_remove_member(&mut queues, &member(3), 2);

    assert!(queues.has_approved_change(ActionKind::Update, &member(2)));
    assert!(!queues.has_approved_change(ActionKind::Update, &member(3)));
    assert!(queues.has_approved_change(ActionKind::Remove, &member(3)));
    assert!(!queues.has_approved_change(ActionKind::Remove, &member(2)));
}

#[test]
fn drop_approved_update_removes_only_an_update() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(1, update_request(&member(2)));
    insert_remove_member(&mut queues, &member(3), 2);

    assert!(!queues.drop_approved_update(2));
    assert!(!queues.drop_approved_update(9));
    assert!(queues.drop_approved_update(1));
    assert!(!queues.has_approved_change(ActionKind::Update, &member(2)));
    assert_eq!(queues.approved_proposals().len(), 1);
}

/// A landed invite and removal and every update are dropped; an invite whose
/// member did not land stays.
#[test]
fn drop_landed_keeps_what_did_not_land() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(1, update_request(&member(2)));
    insert_remove_member(&mut queues, &member(3), 2);
    insert_remove_member(&mut queues, &member(4), 3);
    queues.insert_approved_proposal(
        4,
        ConversationUpdateRequest::member_invite(MemberInvite {
            key_package_bytes: b"kp".to_vec(),
            member_id: member(5),
        }),
    );
    queues.insert_approved_proposal(
        5,
        ConversationUpdateRequest::member_invite(MemberInvite {
            key_package_bytes: b"kp".to_vec(),
            member_id: member(6),
        }),
    );

    queues.drop_landed(&MembershipDelta {
        added: vec![member(5)],
        removed: vec![member(3)],
    });

    let left: Vec<ProposalId> = queues.approved_proposals().keys().copied().collect();
    assert_eq!(left, vec![3, 5]);
    assert!(queues.has_approved_change(ActionKind::Add, &member(6)));
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
    queues.track_voting_proposal(proposal_id, &request, 0);
    assert!(queues.is_voting(proposal_id));
    assert!(
        queues.active_proposal_targets().contains(target.as_slice()),
        "the in-flight ECP already covers its target"
    );
    assert!(!queues.has_approved_change(ActionKind::Remove, &target));

    // Resolved: the ECP transforms into a queued RemoveMember, and the
    // target stays covered with no gap.
    queues.remove_voting_proposal(proposal_id);
    queues.insert_approved_proposal(proposal_id, remove_request(&target));
    assert!(queues.has_approved_change(ActionKind::Remove, &target));
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

/// One applied delta drops the leaver's queued removal, marks the joiner
/// unsettled in its join epoch and settled from the next, and leaves an
/// untracked id settled.
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
    queues.apply_membership_delta(5, &delta);

    assert!(!queues.has_approved_change(ActionKind::Remove, &leaver));
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

fn invite_request(joiner: u8) -> ConversationUpdateRequest {
    ConversationUpdateRequest::member_invite(MemberInvite {
        key_package_bytes: format!("kp:{joiner}").into_bytes(),
        member_id: member(joiner),
    })
}

fn add_action(joiner: u8) -> Action {
    Action::Add {
        member: MemberId::from(member(joiner)),
        key_package: format!("kp:{joiner}").into_bytes(),
    }
}

fn remove_action(target: u8) -> Action {
    Action::Remove {
        member: MemberId::from(member(target)),
    }
}

fn update_action(owner: u8) -> Action {
    Action::Update {
        member: MemberId::from(member(owner)),
    }
}

/// Under an urgent target only the target's removal is allowed alone; once
/// the target is taken, the rest of the approved set is allowed again.
#[test]
fn an_urgent_candidate_matches_the_target_removal_alone() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(7, invite_request(4));
    insert_remove_member(&mut queues, &member(3), 8);
    queues.set_urgent_commit_target(member(3));

    assert!(queues.actions_within_approved(&member(1), &[remove_action(3)]));
    assert!(!queues.actions_within_approved(&member(1), &[add_action(4), remove_action(3)]));

    queues.take_urgent_commit_target();
    assert!(queues.actions_within_approved(&member(1), &[add_action(4), remove_action(3)]));
}

/// A commit carrying exactly the approved update and invite is valid.
#[test]
fn a_commit_matching_the_approved_updates_is_valid() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(7, update_request(&member(2)));
    queues.insert_approved_proposal(8, invite_request(3));
    assert!(queues.actions_within_approved(&member(1), &[update_action(2), add_action(3)]));
}

/// A commit carrying only part of the approved set is valid.
#[test]
fn a_commit_carrying_a_subset_of_the_approved_set_is_valid() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(7, update_request(&member(2)));
    queues.insert_approved_proposal(8, invite_request(3));
    assert!(queues.actions_within_approved(&member(1), &[add_action(3)]));
}

/// A commit carrying an update nobody approved is invalid.
#[test]
fn a_commit_carrying_an_unapproved_update_is_invalid() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(8, invite_request(3));
    assert!(!queues.actions_within_approved(&member(1), &[update_action(2), add_action(3)]));
}

/// An approved update owned by the commit's sender is carried by the commit
/// itself, so it is not allowed as an action; for any other sender it is.
#[test]
fn the_senders_own_update_is_not_allowed_as_an_action() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(7, update_request(&member(1)));
    let carried = [update_action(1)];
    assert!(!queues.actions_within_approved(&member(1), &carried));
    assert!(queues.actions_within_approved(&member(2), &carried));
}

/// A commit must carry something approved. Empty, it is invalid; empty but
/// from a sender whose own update is approved, it carries that update; in an
/// urgent round the own update does not count and empty stays invalid.
#[test]
fn an_empty_commit_is_invalid_unless_it_carries_the_senders_update() {
    let mut queues = EngineQueues::new();
    assert!(!queues.actions_within_approved(&member(1), &[]));

    queues.insert_approved_proposal(7, update_request(&member(1)));
    assert!(queues.actions_within_approved(&member(1), &[]));
    assert!(!queues.actions_within_approved(&member(2), &[]));

    insert_remove_member(&mut queues, &member(3), 8);
    queues.set_urgent_commit_target(member(3));
    assert!(!queues.actions_within_approved(&member(1), &[]));
}

/// With both an update and the target's removal approved, a commit carrying
/// the pair is rejected while the urgent target is set and accepted once it
/// is taken.
#[test]
fn an_urgent_round_with_an_update_is_invalid() {
    let mut queues = EngineQueues::new();
    queues.insert_approved_proposal(7, update_request(&member(2)));
    insert_remove_member(&mut queues, &member(3), 8);
    queues.set_urgent_commit_target(member(3));
    assert!(!queues.actions_within_approved(&member(1), &[update_action(2), remove_action(3)]));

    queues.take_urgent_commit_target();
    assert!(queues.actions_within_approved(&member(1), &[update_action(2), remove_action(3)]));
}
