//! Test-only helpers for constructing [`Engine`] fixtures, shared across the
//! engine's test modules.

use crate::engine::{config::EngineConfig, handle::Engine, store::InMemoryStore, types::MemberId};

/// A member id as the group would issue it: a signature key.
pub(crate) fn member(name: &str) -> Vec<u8> {
    format!("{name}-signature-key").into_bytes()
}

pub(crate) fn id(name: &str) -> MemberId {
    MemberId::from(member(name))
}

/// A one-member conversation: the creator is its own steward list, so it
/// is the epoch steward at every epoch.
pub(crate) fn creator() -> Engine<InMemoryStore> {
    Engine::create(
        "conv",
        id("alice"),
        1,
        &[id("alice")],
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create")
    .0
}

/// A two-member founding group, seen from alice: both members are
/// settled stewards from `epoch`.
pub(crate) fn founder() -> Engine<InMemoryStore> {
    Engine::create(
        "conv",
        id("alice"),
        1,
        &[id("alice"), id("bob")],
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("create")
    .0
}

/// A member seated by a welcome: no steward list until a sync is adopted,
/// so it is neither steward nor epoch steward.
pub(crate) fn joiner() -> Engine<InMemoryStore> {
    Engine::join(
        "conv",
        id("bob"),
        1,
        &[id("alice"), id("bob")],
        EngineConfig::default(),
        InMemoryStore::default(),
    )
    .expect("join")
    .0
}
