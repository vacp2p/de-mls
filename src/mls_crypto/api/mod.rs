//! The MLS contract: the [`MlsService`] trait plus the byte boundary types
//! and [`MlsError`] the protocol layer uses. No engine binding lives here.

mod error;
mod service;
mod types;

pub use error::MlsError;
pub use service::MlsService;
pub use types::{
    CommitCandidate, DecryptResult, KeyPackageBytes, MlsCommitInput, MlsMessageKind,
    MlsProposalOutput, StagedCandidateResult, key_package_bytes_from_tls,
};
