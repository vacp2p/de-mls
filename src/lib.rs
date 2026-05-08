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
//! - What traits you need to implement ([`core::ConversationEventHandler`])
//! - Core operations (start conversation, join, send messages)
//! - The `ProcessResult` matching flow
//!
//! If you want a ready-to-use solution, see [`app::User`] which provides complete
//! conversation management with state machine and epoch handling.
//!
//! ## Quick Example
//!
//! ```ignore
//! use de_mls::core::{DefaultProvider, ConversationEventHandler, ProcessResult};
//! use de_mls::app::User;
//!
//! // Build a user from an Ethereum private key. The convenience
//! // constructor pins every `User` generic to the default provider
//! // and plug-in bundle, so type inference works without annotation.
//! let user = User::with_private_key(
//!     "0xac0974...",   // Private key
//!     consensus,       // Consensus service
//!     event_handler,   // Your ConversationEventHandler implementation
//! )?;
//!
//! // Start a conversation (as steward).
//! user.start_conversation("de-mls-test", true).await?;
//!
//! // Send a message.
//! user.send_app_message("de-mls-test", b"Hello, world!".to_vec()).await?;
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

/// User-level identity, decoupled from MLS state.
pub mod identity;

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
