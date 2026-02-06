//! # DE-MLS: Decentralized MLS Chat Protocol
//!
//! A library for building decentralized, end-to-end encrypted group chat applications
//! using the MLS (Messaging Layer Security) protocol with consensus-based membership management.
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────┐
//! │                         Your Application                            │
//! └───────────────────────────────┬─────────────────────────────────────┘
//!                                 │
//!         ┌───────────────────────┼───────────────────────┐
//!         ▼                       ▼                       ▼
//! ┌───────────────┐      ┌───────────────┐      ┌───────────────┐
//! │     core      │      │   mls_crypto  │      │      ds       │
//! │  (protocol)   │      │ (encryption)  │      │ (transport)   │
//! └───────────────┘      └───────────────┘      └───────────────┘
//!         │                       │                       │
//!         └───────────────────────┼───────────────────────┘
//!                                 ▼
//!                        ┌───────────────┐
//!                        │      app      │
//!                        │ (reference)   │
//!                        └───────────────┘
//! ```
//!
//! ## Modules
//!
//! - **[`core`]** - Protocol implementation (message processing, consensus integration)
//! - **[`mls_crypto`]** - MLS cryptographic operations (OpenMLS wrapper)
//! - **[`ds`]** - Delivery service abstraction (Waku transport)
//! - **[`app`]** - Reference application layer (multi-group management, state machine)
//! - **[`protos`]** - Protobuf message definitions
//!
//! ## Getting Started
//!
//! Most developers should start with the [`core`] module documentation, which explains:
//! - What traits you need to implement ([`core::GroupEventHandler`])
//! - Core operations (create group, join, send messages)
//! - The ProcessResult → DispatchAction flow
//!
//! If you want a ready-to-use solution, see [`app::User`] which provides complete
//! group management with state machine and epoch handling.
//!
//! ## Quick Example
//!
//! ```ignore
//! use de_mls::core::{DefaultProvider, GroupEventHandler};
//! use de_mls::app::User;
//!
//! // Create a user with an Ethereum private key
//! let user: User<DefaultProvider> = User::with_private_key(
//!     "0xac0974...",  // Private key
//!     consensus,      // Consensus service
//!     handler,        // Your GroupEventHandler implementation
//! )?;
//!
//! // Create a group (as steward)
//! user.create_group("my-group", true).await?;
//!
//! // Send a message
//! user.send_message("my-group", b"Hello, world!").await?;
//! ```

/// Protocol implementation: message processing, consensus, and group operations.
pub mod core;

/// Reference application layer: multi-group management and state machine.
pub mod app;

/// Delivery service: transport-agnostic messaging (Waku implementation).
pub mod ds;

/// MLS cryptographic operations: OpenMLS wrapper for encryption/decryption.
pub mod mls_crypto;

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
