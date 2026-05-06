//! MLS cryptographic operations for DE-MLS.
//!
//! Each `OpenMlsService` instance is scoped to a single MLS group. The
//! `MlsService` trait defines the per-group surface; constructors for the
//! OpenMLS impl live as inherent methods on `OpenMlsService`.
//!
//! # Quick Start
//!
//! ```ignore
//! use de_mls::mls_crypto::{OpenMlsService, MemoryDeMlsStorage, MlsService,
//!     WalletIdentity, parse_wallet_address};
//!
//! // Identity + storage are constructor inputs, shared across groups.
//! let wallet = parse_wallet_address("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266")?;
//! let identity = WalletIdentity::from_wallet(wallet)?;
//! let storage = MemoryDeMlsStorage::new();
//!
//! // Create a fresh group as its sole initial member.
//! let mls = OpenMlsService::new_as_creator("my-chat".into(), storage, identity)?;
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

mod error;
mod identity;
mod service;
pub mod storage;
mod types;

pub use error::MlsError;
pub use identity::{
    IdentityProvider, ShortId, WalletIdentity, format_wallet_address, parse_wallet_address,
    parse_wallet_to_bytes,
};
pub use service::{CIPHERSUITE, MlsService, OpenMlsService};
pub use storage::{DeMlsStorage, MemoryDeMlsStorage};
pub use types::{
    CommitCandidate, DecryptResult, KeyPackageBytes, MlsCommitInput, MlsMessageKind,
    MlsProposalOutput, StagedCandidateResult, key_package_bytes_from_json,
};
