//! The emergency route on the fake bed: a score-driven removal commits alone
//! while ordinary approved work waits for the next batch.

mod common;

use common::fake_router::Bed;
use common::fast_config;

// A below-threshold score removal commits alone; the approved add that was
// queued beside it waits and lands in the next batch.
#[test]
fn score_removal_commits_alone_and_the_batch_waits() {
    let mut bed = Bed::new("conv", "alice", fast_config());
    let bob = bed.seat(0, "bob");
    let carol = bed.seat(0, "carol");
    let carol_id = bed.nodes[carol].own.clone();

    // Approved work waiting for the batch.
    let dave = bed.add_pending("dave");
    let kp_dave = bed.key_package_of(dave);
    let dave_id = bed.nodes[dave].own.clone();
    let now = bed.now;
    bed.router_mut(0).announce(now, dave_id, kp_dave);

    // At the same moment, an emergency removal: it commits alone rather than
    // waiting behind dave's add.
    let now = bed.now;
    bed.router_mut(0)
        .act(now, |e| e.propose_score_removal(now, carol_id))
        .expect("propose score removal of carol");

    bed.process_until("carol left", |b| b.router(carol).left);
    assert!(
        !bed.is_live(dave),
        "the urgent commit carried the removal alone, not dave's add"
    );

    bed.process_until("dave seated", |b| b.is_live(dave) && b.epochs_agree());

    assert!(bed.agree_among(&[0, bob, dave]));
    assert_eq!(bed.router(0).mls.members().len(), 3);
}
