//! MLS cryptographic operations for DE-MLS.
//!
//! Each `OpenMlsService` instance is scoped to a single MLS group. The
//! `MlsService` trait defines the per-conversation surface; constructors for the
//! OpenMLS impl live as inherent methods on `OpenMlsService`.
//!
//! Identity and MLS credentials are split: [`crate::member_id::MemberId`]
//! is the user-level abstraction (just `member_id_bytes` + display);
//! `MlsCredentials` (re-exported here) holds the MLS-specific signing keypair and
//! credential, built from an `MemberId` at User init and shared across
//! every per-conversation service.
//!
//! # Quick Start
//!
//! ```ignore
//! use std::sync::Arc;
//! use de_mls::mls_crypto::{
//!     MemoryDeMlsStorage, MlsCredentials, MlsService, OpenMlsService,
//! };
//!
//! // `identity` is any type implementing `de_mls::member_id::MemberId`.
//! let credentials = Arc::new(MlsCredentials::from_member_id(&member_id)?);
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
mod storage;
mod types;

pub use credentials::MlsCredentials;
pub use error::MlsError;
pub use service::{CIPHERSUITE, DEFAULT_COMMIT_BATCH_MAX, MlsService, OpenMlsService};
pub use storage::DeMlsStorage;
pub use types::{
    CommitCandidate, DecryptResult, KeyPackageBytes, MlsCommitInput, MlsMessageKind,
    MlsProposalOutput, StagedCandidateResult, key_package_bytes_from_tls,
};
