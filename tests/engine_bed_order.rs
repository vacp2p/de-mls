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

/// A joiner that has never held a list takes it from an election filed the
/// instant it is seated. The answer to its own ask arrives a step later
/// carrying the same list and is adopted: dave sees `SyncApplied`.
#[test]
fn a_joiner_adopts_its_answer_after_it_voted_on_an_election() {
    let mut bed = Bed::new("conv", "alice", config());
    bed.seat(0, "bob");
    let dave = bed.add_pending("dave");
    let now = bed.now;
    let key_package = bed.key_package_of(dave);
    let id = bed.nodes[dave].own.clone();
    bed.router_mut(0).announce(now, id, key_package);
    bed.process_until("dave seated", |b| b.is_live(dave) && b.epochs_agree());

    let now = bed.now;
    let mut filers = Vec::new();
    for node in bed.live_nodes() {
        if node == dave {
            continue;
        }
        let mut sent = false;
        bed.router_mut(node)
            .act(now, |e| {
                let out = e.propose_election(now);
                sent = out.as_ref().is_ok_and(|o| !o.outbound.is_empty());
                out
            })
            .expect("propose_election");
        if sent {
            filers.push(node);
        }
    }
    assert_eq!(filers, vec![bed.epoch_steward()], "the epoch steward files");

    bed.process_until("dave adopts the answer to its ask", |b| {
        b.router(dave)
            .events
            .iter()
            .any(|e| matches!(e, Event::SyncApplied))
    });
    assert!(bed.converged());
    assert!(
        bed.live_nodes()
            .iter()
            .all(|&i| bed.router(i).engine.is_synced())
    );
}
