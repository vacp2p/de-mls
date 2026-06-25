//! MLS cryptographic operations for DE-MLS.
//!
//! - `service` — `MlsService`, the OpenMLS-backed per-conversation engine: the
//!   group, its `new_as_creator` / `new_from_welcome` constructors, and the
//!   commit / message operations the conversation drives. Provider, signer, and
//!   credential arrive by reference per call; the integrator owns them.
//! - `types` — the boundary value types the engine takes and returns
//!   (`MlsCommitInput`, `CommitArtifacts`, `StagedCandidateResult`, …).
//! - `error` — `MlsError`.

mod error;
mod service;
mod types;

pub use error::MlsError;
pub use service::MlsService;
pub use types::{
    CommitArtifacts, DecryptedMessage, MlsCommitInput, MlsMessageKind, MlsProposalOutput,
    StagedCandidateResult,
};
