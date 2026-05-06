//! MLS cryptographic operations for DE-MLS.
//!
//! Each `OpenMlsService` instance is scoped to a single MLS group. The
//! `MlsService` trait defines the per-group surface; constructors for the
//! OpenMLS impl live as inherent methods on `OpenMlsService`.
//!
//! Identity and MLS credentials are split: [`crate::identity::Identity`]
//! is the user-level abstraction (just `identity_bytes` + display);
//! `MlsCredentials` (re-exported here) holds the MLS-specific signing keypair and
//! credential, built from an `Identity` at User init and shared across
//! every per-group service.
//!
//! # Quick Start
//!
//! ```ignore
//! use std::sync::Arc;
//! use de_mls::identity::{WalletIdentity, parse_wallet_address};
//! use de_mls::mls_crypto::{
//!     MemoryDeMlsStorage, MlsCredentials, MlsService, OpenMlsService,
//! };
//!
//! let wallet = parse_wallet_address("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")?;
//! let identity = WalletIdentity::from_wallet(wallet);
//! let credentials = Arc::new(MlsCredentials::from_identity(&identity)?);
//! let storage = MemoryDeMlsStorage::new();
//!
//! // Create a fresh group as its sole initial member.
//! let mls = OpenMlsService::new_as_creator(
//!     "my-chat".into(),
//!     storage,
//!     Arc::clone(&credentials),
//! )?;
//!
//! // Encrypt a message.
//! let ciphertext = mls.encrypt(b"Hello!")?;
//! ```
//!
//! # Storage
//!
//! Each service requires a storage backend implementing `DeMlsStorage`.
//! Use `MemoryDeMlsStorage` for development or implement your own for
//! persistence. Storage may be shared across services via `Arc<S>`.

mod credentials;
mod error;
mod service;
pub mod storage;
mod types;

pub use credentials::MlsCredentials;
pub use error::MlsError;
pub use service::{CIPHERSUITE, MlsService, OpenMlsService};
pub use storage::{DeMlsStorage, MemoryDeMlsStorage};
pub use types::{
    CommitCandidate, DecryptResult, KeyPackageBytes, MlsCommitInput, MlsMessageKind,
    MlsProposalOutput, StagedCandidateResult, key_package_bytes_from_json,
};
