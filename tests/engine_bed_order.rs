//! Delivery order across senders is nobody's promise: the router holds what
//! the engine refuses and feeds it again, so a vote that outruns its
//! proposal still lands.
//!
//! The scenarios run on plain `fast_config()`: a vote that is lost shows as
//! the member's YES arriving only at the consensus timeout, after the
//! steward's commit.

mod common;

use std::time::Duration;

use common::fake_router::Bed;
use common::fast_config;
use de_mls::EngineConfig;
use de_mls::engine::Event;

fn config() -> EngineConfig {
    let config = fast_config();
    config.validate().expect("the scenario config is valid");
    config
}

/// Without the held-and-fed-again rule, dave drops the two votes that reach
/// it before alice's proposal, its verdict splits from the others', and it
/// rejects the commit the group merges.
#[test]
fn votes_that_arrive_before_their_proposal_are_counted() {
    let mut bed = Bed::new("conv", "alice", config());
    bed.seat(0, "bob");
    bed.seat(0, "carol");
    let dave = bed.seat(0, "dave");
    let now = bed.now;
    bed.hold_control(dave, &[0], now + Duration::from_millis(250));
    let erin = bed.add_pending("erin");
    let key_package = bed.key_package_of(erin);
    let id = bed.nodes[erin].own.clone();
    bed.router_mut(0).announce(now, id, key_package);

    bed.process_until("dave holds bob's and carol's votes", |b| {
        b.router(dave).held_count() == 2
    });
    bed.process_until("erin seated and the group converged", |b| {
        b.is_live(erin) && b.converged()
    });

    assert!(
        !bed.router(dave)
            .events
            .iter()
            .any(|e| matches!(e, Event::CandidateRejected { .. })),
        "dave counted the votes and accepted the commit"
    );
}
