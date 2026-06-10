//! # DE-MLS: Decentralized MLS Chat Protocol
//!
//! A library for building decentralized, end-to-end encrypted chat applications
//! using the MLS (Messaging Layer Security) protocol with consensus-based membership management.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                         Your Application                            │
//! └───────────────────────────────┬─────────────────────────────────────┘
//!                                 │
//!                        ┌────────┴────────┐
//!                        │      app        │
//!                        │ (reference impl,│
//!                        │   optional)     │
//!                        └────────┬────────┘
//!                                 │
//!         ┌───────────────────────┼───────────────────────┐
//!         ▼                       ▼                       ▼
//! ┌───────────────┐      ┌───────────────┐      ┌───────────────┐
//! │     core      │      │   mls_crypto  │      │      ds       │
//! │  (protocol)   │      │ (encryption)  │      │ (transport)   │
//! └───────────────┘      └───────────────┘      └───────────────┘
//! ```
//!
//! ## Modules
//!
//! - **[`core`]** - Protocol implementation (message processing, consensus integration)
//! - **[`mls_crypto`]** - MLS cryptographic operations (OpenMLS wrapper)
//! - **[`app`]** - Reference application layer (per-conversation `SessionRunner`, state machine)
//! - **[`protos`]** - Protobuf message definitions
//!
//! The library carries no transport. The reference delivery service (the
//! `DeliveryService` trait + Waku implementation) lives in the `de-mls-ds`
//! crate, and the reference integrator (`User`) in `de-mls-gateway`.
//!
//! ## Getting Started
//!
//! Most developers should start with the [`core`] module documentation, which explains:
//! - What traits you need to implement (`core::SessionEvent`)
//! - Core operations (start conversation, join, send messages)
//! - The `ProcessResult` matching flow
//!
//! The library exposes the per-conversation [`app::SessionRunner`] handle.
//! It carries no transport: it buffers outbound and consumes inbound
//! payloads, and the integrator owns routing. A ready-to-use reference
//! integrator (the multi-conversation `User` with registry + routing +
//! lifecycle over a transport) lives in the `de-mls-gateway` crate.
//!
//! ## Quick Example
//!
//! ```ignore
//! use de_mls::app::{ConversationDeps, SessionRunner};
//!
//! // Build a per-conversation session from injected deps (plug-in factory,
//! // consensus context, identity).
//! let mut session = SessionRunner::create("de-mls-test", deps)?;
//!
//! // Send a chat message — buffered, never auto-sent.
//! session.push_message(b"Hello, world!".to_vec())?;
//!
//! // Drain outbound and publish it on your own transport.
//! for out in session.drain_outbound() { /* publish */ }
//! ```

/// Protocol implementation.
pub mod core;

/// Reference application layer.
pub mod app;

/// MLS cryptographic operations: OpenMLS wrapper for encryption/decryption.
pub mod mls_crypto;

/// Local member-id trait, decoupled from MLS state. Provides the
/// canonical bytes that name a participant in the protocol.
pub mod member_id;

/// Reference implementations of the library's plug-in traits — in-memory
/// MLS / peer-score storage, default consensus + per-conversation
/// plug-in bundles, and a reference key-package provider. Production
/// integrators swap one or more for their own implementations.
pub mod defaults;

#[cfg(test)]
pub(crate) mod test_fixtures;

/// Protobuf message definitions.
pub mod protos {
    /// Re-exported consensus protocol messages.
    pub mod hashgraph_like_consensus {
        pub mod v1 {
            pub use ::hashgraph_like_consensus::protos::consensus::v1::*;
        }
    }

    /// DE-MLS application-level messages.
    pub mod de_mls {
        pub mod messages {
            pub mod v1 {
                include!(concat!(env!("OUT_DIR"), "/de_mls.messages.v1.rs"));
            }
        }
    }
}
