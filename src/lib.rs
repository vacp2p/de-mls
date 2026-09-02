//! # DE-MLS: Decentralized MLS Chat Protocol
//!
//! A library for building decentralized, end-to-end encrypted chat applications
//! using the MLS (Messaging Layer Security) protocol with consensus-based membership management.
//!
//! ## Modules
//!
//! - **[`conversation`]** - The [`Conversation`] handle plus its
//!   per-conversation state, state machine, and inbound dispatch
//! - **[`consensus`]** - Consensus plug-in contract, outcome application, and voting
//! - **[`peer_scoring`]** / **[`steward_list`]** - Protocol plug-ins
//! - **[`commit_round`]** - Commit-round candidate collection, selection, and apply
//! - **[`mls_crypto`]** - MLS cryptographic operations (OpenMLS wrapper)
//! - **[`protos`]** - Protobuf message definitions
//!
//! The library carries no transport. The reference delivery service (the
//! `DeliveryService` trait + Waku implementation) lives in the `de-mls-ds`
//! crate, and the reference integrator (`User`) in `de-mls-gateway`.
//!
//! ## Getting Started
//!
//! The library exposes the per-conversation [`Conversation`]
//! handle. It carries no transport: it buffers outbound and consumes inbound
//! payloads, and the integrator owns routing. Everything above one conversation
//! — the registry of many, transport, and identity — belongs to the
//! application.
//!
//! Integrators drain [`ConversationEvent`] from each conversation (for
//! transport and UI delivery), feed inbound packets to the conversation, and
//! dispatch the returned outcomes.
//!
//! ## Quick Example
//!
//! ```ignore
//! use de_mls::Conversation;
//!
//! // Everything the conversation needs is passed in: the OpenMLS provider and
//! // signer are borrowed per call and stored nowhere, the plug-ins are moved
//! // in, and `clock` is your `WallClock` (wrap `SystemTime` in production,
//! // `MockClock` in tests). Pass no `initial_members` for a plain create.
//! let mut conversation = Conversation::create(
//!     "de-mls-test",
//!     &provider,
//!     credential,
//!     &group_config,
//!     &signer,
//!     &consensus,
//!     scoring,
//!     clock,
//!     app_id,
//!     config,
//!     &[],
//! )?;
//!
//! // Send a chat message — buffered, never auto-sent.
//! conversation.send_message(&provider, &signer, b"Hello, world!".to_vec())?;
//!
//! // Drain outbound and publish it on your own transport.
//! for out in conversation.drain_outbound() { /* publish */ }
//! ```
//!
//! `tests/standalone_construction.rs` builds this end to end and is compiled on
//! every run, so it stays honest where this sketch cannot.

/// The [`Conversation`](conversation) handle, per-conversation
/// state, state machine, and inbound dispatch.
pub mod conversation;

/// Consensus plug-in contract, outcome application, and voting.
pub mod consensus;

/// Peer scoring: vocabulary, the library-owned service, and the one trait an
/// integrator implements ([`peer_scoring::PeerScoreStorage`]).
pub mod peer_scoring;

/// Steward-list plug-in: deterministic list and rotation queries.
pub mod steward_list;

/// Commit round candidate processing, selection, and commit application.
pub mod commit_round;

/// Conversation-event types produced for the integrator.
pub mod events;

/// [`ProcessResult`] returned by inbound processing.
pub mod process_result;

/// Proposal classification.
pub mod proposal_kind;

/// Wall-clock anchor combined with the conversation state machine.
pub mod phase_timer;

/// Caller-supplied time source: [`WallClock`] trait, [`Timestamp`],
/// and the test-oriented [`MockClock`].
pub mod wall_clock;

/// Library error types.
pub mod error;

// Crate-root re-exports so flat-name imports resolve without the owning
// module path.
pub use commit_round::*;
pub use conversation::*;
pub use error::*;
pub use events::*;
pub use peer_scoring::*;
pub use process_result::*;
pub use proposal_kind::*;
pub use steward_list::*;

// `consensus` and `phase_timer` are re-exported explicitly below to avoid glob
// collisions with the conversation glob.
pub(crate) use consensus::{ConsensusApplyResult, ConsensusEngine, apply_consensus_result};
pub use consensus::{ConsensusPlugin, CreatorVote};

pub(crate) use phase_timer::PhaseTimer;

pub use wall_clock::{MockClock, Timestamp, WallClock};

/// MLS cryptographic operations: OpenMLS wrapper for encryption/decryption.
pub mod mls_crypto;

pub use mls_crypto::MemberId;
pub use openmls::prelude::{Extensions, GroupContext, LeafNodeIndex, Member, MlsGroup};

/// Reference implementations of the library's plug-in traits — in-memory
/// MLS / peer-score storage, default consensus + per-conversation
/// plug-in bundles, and a reference key-package provider. Production
/// integrators swap one or more for their own implementations.
pub mod defaults;

#[cfg(test)]
pub mod test_fixtures;

pub mod protos;
