//! The fake bed drives the engine end to end with no cryptography: one
//! creator, two seated joiners, chat crossing, ticks firing.

mod common;

use std::time::Duration;

use common::fake_router::Bed;
use de_mls::EngineConfig;

#[test]
fn creator_seats_two_joiners_and_chat_crosses() {
    let mut bed = Bed::new("conv", "alice", EngineConfig::default());
    let bob = bed.add_pending("bob");
    let carol = bed.add_pending("carol");
    bed.seed(&[bob, carol]);
    bed.process(Duration::from_millis(50));

    assert!(bed.is_live(bob) && bed.is_live(carol));
    assert_eq!(bed.router(0).mls.epoch(), 1);
    assert!(bed.epochs_agree());
    assert!(bed.converged());
    assert!(bed.membership_agrees());
    assert_eq!(bed.router(bob).mls.members().len(), 3);

    bed.router_mut(carol).send_chat(b"hi");
    bed.process(Duration::from_millis(50));
    let alice_id = bed.router(0).own_id().clone();
    let carol_id = bed.router(carol).own_id().clone();
    assert_eq!(
        bed.router(0).chats,
        vec![(carol_id.clone(), b"hi".to_vec())]
    );
    assert_eq!(bed.router(bob).chats, vec![(carol_id, b"hi".to_vec())]);
    assert!(bed.router(carol).chats.is_empty(), "no self echo");
    assert_ne!(alice_id, bed.router(bob).own_id().clone());

    bed.process_until("ticks keep running", |b| b.now.as_secs() > 1_005);
}

#[test]
fn partition_holds_frames_back() {
    let mut bed = Bed::new("conv", "alice", EngineConfig::default());
    let bob = bed.add_pending("bob");
    bed.seed(&[bob]);
    bed.process(Duration::from_millis(50));
    bed.mute(bob);
    bed.router_mut(0).send_chat(b"lost");
    bed.process(Duration::from_millis(50));
    assert!(bed.router(bob).chats.is_empty());
    bed.unmute(bob);
    bed.router_mut(0).send_chat(b"found");
    bed.process(Duration::from_millis(50));
    assert_eq!(bed.router(bob).chats.len(), 1);
}
