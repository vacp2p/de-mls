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
//! - **[`peer_scoring`]** / **[`steward_list`]** / **[`freeze`]** - Protocol plug-ins
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
//! payloads, and the integrator owns routing. A ready-to-use reference
//! integrator (the multi-conversation `User` with registry + routing +
//! lifecycle over a transport) lives in the `de-mls-gateway` crate.
//!
//! Integrators drain [`ConversationEvent`] from each conversation (for
//! transport and UI delivery), feed inbound packets to the conversation, and
//! dispatch the returned outcomes.
//!
//! ## Quick Example
//!
//! ```ignore
//! use de_mls::{ConversationDeps, Conversation};
//!
//! // Build a conversation from injected deps (pre-built plug-in
//! // instances, consensus service, identity).
//! let mut conversation = Conversation::create("de-mls-test", deps)?;
//!
//! // Send a chat message — buffered, never auto-sent.
//! conversation.send_message(b"Hello, world!".to_vec())?;
//!
//! // Drain outbound and publish it on your own transport.
//! for out in conversation.drain_outbound() { /* publish */ }
//! ```

/// The [`Conversation`](conversation) handle, per-conversation
/// state, state machine, and inbound dispatch.
pub mod conversation;

/// Consensus plug-in contract, outcome application, and voting.
pub mod consensus;

/// Peer-scoring plug-in: vocabulary, traits, and reference service.
pub mod peer_scoring;

/// Steward-list plug-in: deterministic roster and rotation queries.
pub mod steward_list;

/// Freeze round candidate processing, selection, and commit application.
pub mod freeze;

/// Conversation-event types produced for the integrator.
pub mod events;

/// [`ProcessResult`](process_result::ProcessResult) returned by inbound processing.
pub mod process_result;

/// Proposal classification.
pub mod proposal_kind;

/// Per-conversation plug-in types bundle.
pub mod conversation_plugins;

/// Wall-clock anchor combined with the conversation state machine.
pub mod phase_timer;

/// Library error types.
pub mod error;

// Crate-root re-exports so flat-name imports resolve without the owning
// module path.
pub use conversation::*;
pub use error::*;
pub use events::*;
pub use freeze::*;
pub use peer_scoring::*;
pub use process_result::*;
pub use proposal_kind::*;
pub use steward_list::*;

// `consensus`, `conversation_plugins`, and `phase_timer` are re-exported
// explicitly below to avoid glob collisions with the conversation glob.
pub use consensus::{
    ConsensusApplyResult, ConsensusPlugin, ConsensusServiceFor, CreatorVote, SyncConsensusReceiver,
    apply_consensus_result,
};
pub use conversation_plugins::ConversationPlugins;

pub(crate) use phase_timer::PhaseTimer;

/// MLS cryptographic operations: OpenMLS wrapper for encryption/decryption.
pub mod mls_crypto;

/// Reference implementations of the library's plug-in traits — in-memory
/// MLS / peer-score storage, default consensus + per-conversation
/// plug-in bundles, and a reference key-package provider. Production
/// integrators swap one or more for their own implementations.
pub mod defaults;

#[cfg(test)]
pub(crate) mod test_fixtures;

pub mod protos;
