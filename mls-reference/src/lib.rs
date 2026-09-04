//! The MLS group-operations contract, a reference implementation of it, and a
//! conformance suite.
//!
//! de-mls's engine decides *what* should happen to a conversation — which
//! commit wins a round, who the stewards are, when a member is removed — but it
//! never touches MLS. The application owns the group: the provider, the signer,
//! key-package minting, welcome delivery, and every MLS call. [`GroupOps`] is
//! the seam between the two, and this crate is the handover artefact for it:
//!
//! - [`GroupOps`] — the operations and their guarantees, one method per row of
//!   the contract table in `README.md`. Construction is impl-specific and
//!   therefore not on the trait; the requirements for it are documented on
//!   [`Reference::create`], [`Reference::open_welcome`] and [`Reference::load`].
//! - [`Reference`] — an OpenMLS 0.8.1 implementation that satisfies the
//!   contract. It is a worked example, not a component to depend on: copy it,
//!   reorganise it, replace it.
//! - `tests/conformance.rs` — the suite the contract is defined by. It is
//!   written against [`GroupOps`], so pointing its `Suite` at another
//!   implementation runs the same cases against that one.
//!
//! A member is named everywhere by **its leaf's signature key**. MLS makes that
//! value unique within a group, every node computes the same one, and a joiner's
//! key package already carries it — so the pre-seat id and the seated id are one
//! value and nothing above this crate maps between two vocabularies. Leaf
//! indices never leave this crate.
//!
//! This crate does not depend on de-mls, and de-mls does not depend on it.

mod contract;
mod error;
mod group_config;
mod reference;

pub use contract::{Action, Applied, Built, GroupOps, Opened, StagedFacts};
pub use error::{BoxedError, Error};
pub use group_config::{
    MAX_FORWARD_DISTANCE, OUT_OF_ORDER_TOLERANCE, PADDING_SIZE, PAST_EPOCH_WINDOW, RESUMPTION_PSKS,
    pin, pin_join,
};
pub use reference::{Reference, commit_hash, key_package_identity, validate_key_package};
