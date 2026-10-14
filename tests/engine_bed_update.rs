//! A member changes its own leaf on the fake bed: a peer's update, an
//! update-only round, the epoch steward's own update, and an update naming
//! another member.

mod common;

use std::time::Duration;

use common::fake_mls::Kind;
use common::fake_router::Bed;
use common::fast_config;
use common::net::Frame;
use de_mls::engine::{Action, Decision, Event, MemberId, Phase};
use de_mls::protos::de_mls::messages::v1::{
    ControlMessage, ConversationUpdateRequest, control_message,
};
use de_mls::protos::hashgraph_like_consensus::v1::Proposal;
use prost::Message;

/// A seated three-member bed, with the epoch steward and a member that is
/// not the steward.
fn three_members() -> (Bed, usize, usize) {
    let mut bed = Bed::new("conv", "alice", fast_config());
    bed.seat(0, "bob");
    bed.seat(0, "carol");
    let steward = bed.epoch_steward();
    let other = (0..3).find(|&n| n != steward).expect("a non-steward");
    (bed, steward, other)
}

fn reported_update(bed: &Bed, node: usize, mark: usize, member: &MemberId) -> bool {
    bed.router(node).events[mark..]
        .iter()
        .any(|e| matches!(e, Event::MemberUpdated { member: m } if m == member))
}

fn marks(bed: &Bed) -> Vec<usize> {
    (0..3).map(|n| bed.router(n).events.len()).collect()
}

// A peer's update is checked by every other node, carried by the steward's
// commit as an action, and is reported on every node.
#[test]
fn a_members_update_lands_and_every_node_reports_it() {
    let (mut bed, steward, other) = three_members();
    let other_id = bed.nodes[other].own.clone();
    let epoch = bed.router(0).mls.epoch();
    let mark = marks(&bed);

    let now = bed.now;
    bed.router_mut(other).update_leaf(now);
    bed.process_until("the update landed everywhere", |b| {
        (0..3).all(|n| reported_update(b, n, mark[n], &other_id)) && b.converged()
    });

    for n in 0..3 {
        assert_eq!(bed.router(n).mls.epoch(), epoch + 1);
        assert_eq!(bed.router(n).mls.leaf_version(&other_id), 1);
    }
    for n in (0..3).filter(|&n| n != other) {
        assert!(
            bed.router(n).decisions.iter().any(
                |d| matches!(d, Decision::ValidateUpdate { member, .. } if *member == other_id)
            ),
            "node {n} validated the update on its group"
        );
    }
    assert!(bed.router(steward).decisions.iter().any(|d| matches!(
        d,
        Decision::BuildCommit { actions } if *actions == [Action::Update { member: other_id.clone() }]
    )));
    assert!(bed.membership_agrees());
    assert_eq!(bed.router(0).mls.members().len(), 3);
}

// Nothing is added or removed, yet the update alone opens a round and the
// epoch advances.
#[test]
fn an_update_only_batch_opens_a_round() {
    let (mut bed, steward, other) = three_members();
    let other_id = bed.nodes[other].own.clone();
    let epoch = bed.router(0).mls.epoch();
    let mark = bed.router(steward).events.len();

    let now = bed.now;
    bed.router_mut(other).update_leaf(now);
    bed.process_until("the round closed", |b| {
        b.router(steward).mls.epoch() == epoch + 1 && b.converged()
    });

    assert!(
        bed.router(steward).events[mark..]
            .iter()
            .any(|e| matches!(e, Event::PhaseChange(Phase::Freezing))),
        "a round opened for the update alone"
    );
    assert_eq!(bed.router(steward).mls.leaf_version(&other_id), 1);
    assert_eq!(bed.router(0).mls.members().len(), 3);
}

// The steward's own update is not an action: its commit carries it, and the
// commit has no actions at all.
#[test]
fn the_stewards_own_update_lands_with_an_action_less_commit() {
    let (mut bed, steward, _) = three_members();
    let steward_id = bed.nodes[steward].own.clone();
    let epoch = bed.router(0).mls.epoch();
    let mark = marks(&bed);

    let now = bed.now;
    bed.router_mut(steward).update_leaf(now);
    bed.process_until("the update landed everywhere", |b| {
        (0..3).all(|n| reported_update(b, n, mark[n], &steward_id)) && b.converged()
    });

    for n in 0..3 {
        assert_eq!(bed.router(n).mls.epoch(), epoch + 1);
        assert_eq!(bed.router(n).mls.leaf_version(&steward_id), 1);
    }
    assert!(
        bed.router(steward)
            .decisions
            .iter()
            .any(|d| matches!(d, Decision::BuildCommit { actions } if actions.is_empty())),
        "the build named no actions"
    );
    assert!(
        !bed.router(steward)
            .decisions
            .iter()
            .any(|d| matches!(d, Decision::BuildCommit { actions } if !actions.is_empty())),
    );
}

// A `LeafUpdate` whose member is not the member that sent it is dropped on
// every node: nothing is asked of the group and nothing lands. An honest
// update from the same sender afterwards still goes through.
#[test]
fn an_update_naming_another_member_is_dropped_everywhere() {
    let (mut bed, steward, other) = three_members();
    let forger = other;
    let victim = steward;
    let forger_id = bed.nodes[forger].own.clone();
    let victim_id = bed.nodes[victim].own.clone();
    let epoch = bed.router(0).mls.epoch();
    let mark = marks(&bed);

    let now_secs = bed.now.as_secs();
    let update = b"forged".to_vec();
    let proposal = Proposal {
        name: "leaf-update:forged".into(),
        payload: ConversationUpdateRequest::leaf_update(victim_id.as_bytes(), update)
            .encode_to_vec(),
        proposal_id: 7,
        proposal_owner: forger_id.as_bytes().to_vec(),
        votes: Vec::new(),
        expected_voters_count: 1,
        round: 1,
        timestamp: now_secs,
        expiration_timestamp: now_secs + 5,
        liveness_criteria_yes: true,
    };
    let bytes = ControlMessage {
        payload: Some(control_message::Payload::Proposal(proposal)),
    }
    .encode_to_vec();
    let sealed = bed.router(forger).mls.seal(Kind::Control, &bytes);
    let now = bed.now;
    bed.net.broadcast(now, forger, Frame::Sealed(sealed));

    for _ in 0..40 {
        bed.process(Duration::from_millis(50));
    }

    for (n, &mark) in mark.iter().enumerate() {
        assert_eq!(bed.router(n).mls.epoch(), epoch, "no round opened");
        assert!(
            !bed.router(n)
                .decisions
                .iter()
                .any(|d| matches!(d, Decision::ValidateUpdate { .. })),
            "node {n} was asked to check the forged update"
        );
        assert!(!reported_update(&bed, n, mark, &victim_id));
        assert_eq!(bed.router(n).mls.leaf_version(&victim_id), 0);
    }

    let now = bed.now;
    bed.router_mut(forger).update_leaf(now);
    bed.process_until("the honest update landed", |b| {
        (0..3).all(|n| reported_update(b, n, mark[n], &forger_id)) && b.converged()
    });
    assert_eq!(bed.router(0).mls.leaf_version(&victim_id), 0);
}
