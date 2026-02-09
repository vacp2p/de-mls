//! MLS cryptographic operations for DE-MLS.
//!
//! This module provides `MlsService`, a unified API for all MLS operations:
//!
//! - Identity management (wallet-based)
//! - Key package generation
//! - Group creation and joining
//! - Message encryption and decryption
//! - Proposal and commit handling
//!
//! # Quick Start
//!
//! ```ignore
//! use de_mls::mls_crypto::{MlsService, MemoryDeMlsStorage, parse_wallet_address};
//!
//! // Create service with in-memory storage
//! let storage = MemoryDeMlsStorage::new();
//! let mls = MlsService::new(storage);
//!
//! // Initialize identity
//! let wallet = parse_wallet_address("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")?;
//! mls.init(wallet)?;
//!
//! // Create a group
//! mls.create_group("my-chat")?;
//!
//! // Encrypt a message
//! let ciphertext = mls.encrypt("my-chat", b"Hello!")?;
//! ```
//!
//! # Storage
//!
//! The service requires a storage backend implementing `DeMlsStorage`.
//! Use `MemoryDeMlsStorage` for development or implement your own for persistence.

mod error;
mod identity;
mod service;
pub mod storage;
mod types;

pub use error::{IdentityError, MlsError, MlsServiceError, Result, StorageError};
pub use identity::{format_wallet_address, parse_wallet_address, parse_wallet_to_bytes};
pub use service::{MlsService, CIPHERSUITE};
pub use storage::{DeMlsStorage, MemoryDeMlsStorage};
pub use types::{
    key_package_bytes_from_json, CommitResult, DecryptResult, GroupUpdate, KeyPackageBytes,
};
