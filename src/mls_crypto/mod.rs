//! MLS cryptographic operations and identity management.
//!
//! This module wraps OpenMLS to provide a simplified API for MLS group operations.
//! It handles key management, group creation/joining, message encryption/decryption,
//! and batch proposal commits.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    IdentityService                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  create_identity()     │  Initialize from wallet address    │
//! │  generate_key_package()│  Create key package for joining    │
//! │  identity_string()     │  Get wallet address (0x...)        │
//! └─────────────────────────────────────────────────────────────┘
//!                              │
//!                              ▼
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    MlsGroupService                          │
//! ├─────────────────────────────────────────────────────────────┤
//! │  create_group()        │  Create new MLS group (as steward) │
//! │  join_group_from_invite│  Process welcome message           │
//! │  build_message()       │  Encrypt application message       │
//! │  process_inbound()     │  Decrypt incoming message          │
//! │  create_batch_proposals│  Commit add/remove proposals       │
//! └─────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Key Types
//!
//! - `IdentityService` - Trait for wallet identity + key package generation
//! - `MlsGroupService` - Trait for MLS group operations
//! - `OpenMlsIdentityService` - Default implementation using OpenMLS
//! - `MlsGroupHandle` - Wrapper around OpenMLS group state
//! - `KeyPackageBytes` - Serialized key package with identity
//! - `MlsProcessResult` - Result of processing an inbound MLS message
//!
//! # Wallet Address Utilities
//!
//! The module provides utilities for Ethereum wallet address handling:
//! - `normalize_wallet_address` - Normalize raw bytes to `0x`-prefixed hex
//! - `normalize_wallet_address_str` - Validate and normalize string addresses
//! - `parse_wallet_address` - Parse string to `alloy::Address`
//!
//! # Example
//!
//! ```ignore
//! use de_mls::mls_crypto::{OpenMlsIdentityService, IdentityService, MlsGroupService};
//!
//! // Create identity from wallet address
//! let wallet = hex::decode("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")?;
//! let identity = OpenMlsIdentityService::new(&wallet)?;
//!
//! // Create a group (as steward)
//! let group = identity.create_group("my-group")?;
//!
//! // Generate key package for joining
//! let key_package = identity.generate_key_package()?;
//! println!("Key package for: {}", key_package.address_hex());
//! ```
//!
//! # MLS Message Flow
//!
//! ```text
//! Sender                                              Receiver
//! ───────                                             ────────
//! build_message(plaintext)                              │
//!        │                                              │
//!        └──── MLS-encrypted ciphertext ───────────────►│
//!                                                       │
//!                                           process_inbound()
//!                                                       │
//!                                           MlsProcessResult::Application
//!                                                  (plaintext)
//! ```

mod api;
mod error;
mod identity;
mod openmls_identity_service;
mod openmls_provider;

pub use api::*;
pub use error::{IdentityError, MlsServiceError};
pub use identity::{
    normalize_wallet_address, normalize_wallet_address_bytes, normalize_wallet_address_str,
    parse_wallet_address,
};
pub use openmls_identity_service::*;
