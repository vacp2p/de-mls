//! # DE-MLS: Decentralized MLS Chat Protocol
//!
//! A library for building decentralized, end-to-end encrypted group chat applications
//! using the MLS (Messaging Layer Security) protocol with consensus-based membership management.
//!
//! ## Modules
//!
//! - **[`core`]** - Protocol implementation (MLS operations, consensus, message handling)
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

// Core library
pub mod core;

// Example application layer (reference implementation)
pub mod app;

pub mod ds;
pub mod mls_crypto;

pub mod protos {
    pub mod hashgraph_like_consensus {
        pub mod v1 {
            pub use ::hashgraph_like_consensus::protos::consensus::v1::*;
        }
    }

    pub mod de_mls {
        pub mod messages {
            pub mod v1 {
                include!(concat!(env!("OUT_DIR"), "/de_mls.messages.v1.rs"));
            }
        }
    }
}
