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
//! - **[`ds`]** - Delivery service abstraction (Waku transport)
//! - **[`app`]** - Reference application layer (multi-conversation management, state machine)
//! - **[`protos`]** - Protobuf message definitions
//!
//! ## Getting Started
//!
//! Most developers should start with the [`core`] module documentation, which explains:
//! - What traits you need to implement (`core::SessionEvent`)
//! - Core operations (start conversation, join, send messages)
//! - The `ProcessResult` matching flow
//!
//! If you want a ready-to-use solution, see [`app::User`] which provides complete
//! conversation management with state machine and epoch handling.
//!
//! ## Quick Example
//!
//! ```ignore
//! use de_mls::app::User;
//!
//! // Build a user from your own `MemberId` impl plus the default plug-in
//! // bundle.
//! let mut user = User::new_with_plugins(&member_id, plugins, transport);
//!
//! // Start a conversation (as steward).
//! user.start_conversation("de-mls-test", true).await?;
//!
//! // Send a message.
//! user.push_message("de-mls-test", b"Hello, world!".to_vec()).await?;
//!
//! // Drain lifecycle + per-session events on your polling cycle.
//! for event in user.drain_lifecycle_events() { /* … */ }
//! ```

/// Protocol implementation.
pub mod core;

/// Reference application layer.
pub mod app;

/// Delivery service: transport-agnostic messaging.
/// Enable the **`waku`** feature for the Waku relay implementation.
pub mod ds;

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
