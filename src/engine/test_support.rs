//! Test-only helpers for constructing [`Engine`] fixtures, shared across the
//! engine's test modules.

use crate::engine::{
    InMemoryStore,
    config::EngineConfig,
    handle::Engine,
    types::{MemberId, Timestamp},
};
use crate::traits::{InMemoryGroupView, InMemoryPublisher};

/// A member id as the group would issue it: a signature key.
pub(crate) fn member(name: &str) -> Vec<u8> {
    format!("{name}-signature-key").into_bytes()
}

pub(crate) fn id(name: &str) -> MemberId {
    MemberId::from(member(name))
}

/// A group view for the conversation `"conv"`, seen from `own`.
pub(crate) fn view(own: MemberId, epoch: u64, members: &[MemberId]) -> InMemoryGroupView {
    InMemoryGroupView {
        conversation_id: "conv".into(),
        own_id: own,
        epoch,
        members: members.to_vec(),
    }
}

/// [`view`] over member names.
pub(crate) fn group(own: &str, epoch: u64, members: &[&str]) -> InMemoryGroupView {
    let members: Vec<MemberId> = members.iter().map(|m| id(m)).collect();
    view(id(own), epoch, &members)
}

/// A one-member conversation: the creator is its own steward list, so it
/// is the epoch steward at every epoch.
pub(crate) fn creator() -> Engine<InMemoryStore> {
    let mut publisher = InMemoryPublisher::default();
    Engine::create(
        Timestamp::ZERO,
        &group("alice", 1, &["alice"]),
        EngineConfig::default(),
        InMemoryStore::default(),
        &mut publisher,
    )
    .expect("create")
    .0
}

/// A two-member founding group, seen from alice: both members are
/// settled stewards from `epoch`.
pub(crate) fn founder() -> Engine<InMemoryStore> {
    let mut publisher = InMemoryPublisher::default();
    Engine::create(
        Timestamp::ZERO,
        &group("alice", 1, &["alice", "bob"]),
        EngineConfig::default(),
        InMemoryStore::default(),
        &mut publisher,
    )
    .expect("create")
    .0
}

/// A member seated by a welcome: starts in `Syncing` with no steward list
/// until a sync is adopted, so it is neither steward nor epoch steward.
pub(crate) fn joiner() -> Engine<InMemoryStore> {
    let mut publisher = InMemoryPublisher::default();
    Engine::open(
        Timestamp::ZERO,
        &group("bob", 1, &["alice", "bob"]),
        EngineConfig::default(),
        InMemoryStore::default(),
        &mut publisher,
    )
    .expect("join")
    .0
}
