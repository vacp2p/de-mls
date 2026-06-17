//! MLS cryptographic operations for DE-MLS.
//!
//! Two halves with a one-way dependency:
//!
//! - `api` — the protocol **contract**: the `MlsService` trait plus the byte
//!   boundary types (`KeyPackageBytes`, `MlsCommitInput`, …) and `MlsError`.
//!   Core and session depend only on this; it names no concrete engine.
//! - `engine` — the reference **OpenMLS implementation**: `OpenMlsService`,
//!   its `MlsCredentials`, the `DeMlsStorage` backend abstraction, and the
//!   pinned `CIPHERSUITE`. Only integrators and tests construct these.
//!
//! Everything is re-exported here so callers use `de_mls::mls_crypto::*`
//! regardless of which half an item lives in.
//!
//! Identity and MLS credentials are split: [`crate::member_id::MemberId`] is
//! the user-level abstraction (just `member_id_bytes` + display);
//! `MlsCredentials` holds the MLS-specific signing keypair and credential,
//! built from a `MemberId` and shared across every per-conversation service.

mod api;
mod engine;

pub use api::{
    CommitCandidate, DecryptResult, KeyPackageBytes, MlsCommitInput, MlsError, MlsMessageKind,
    MlsProposalOutput, MlsService, StagedCandidateResult, key_package_bytes_from_tls,
};
pub use engine::{
    CIPHERSUITE, DEFAULT_COMMIT_BATCH_MAX, DeMlsStorage, MlsCredentials, OpenMlsService,
};
