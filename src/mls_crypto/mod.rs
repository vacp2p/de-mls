//! MLS cryptographic operations for DE-MLS.
//!
//! - `service` — the `MlsService` trait, the protocol's only contract with an
//!   MLS engine (byte-in / byte-out; signs internally via a passed `&impl
//!   Signer`).
//! - `types` — the byte boundary types (`KeyPackageBytes`, `MlsCommitInput`, …).
//! - `error` — `MlsError`.
//! - `engine` — the reference `OpenMlsService`. It owns no OpenMLS provider:
//!   crypto + rand + storage arrive by reference per call, so one provider can
//!   back every conversation. It takes the credential + ciphersuite from the
//!   creator's key package, and the signer per call — so it pins no provider,
//!   credential, or suite. The integrator (the gateway) supplies the provider,
//!   credentials, and key-package generation.

mod engine;
mod error;
mod service;
mod types;

pub use engine::OpenMlsService;
pub use error::MlsError;
pub use service::{DEFAULT_COMMIT_BATCH_MAX, MlsService};
pub use types::{
    CommitCandidate, DecryptResult, KeyPackageBytes, MlsCommitInput, MlsMessageKind,
    MlsProposalOutput, StagedCandidateResult, key_package_bytes_from_tls,
};
