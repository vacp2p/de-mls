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
mod member_id;
mod service;
mod types;

pub use error::MlsError;
pub use member_id::MemberId;
pub(crate) use member_id::member_id_of;
pub use service::MlsService;
pub use service::{signature_key_of_key_package, validate_key_package};
pub use types::{
    CommitArtifacts, DecryptedMessage, MembershipDelta, MlsCommitInput, MlsMessageKind,
    MlsProposalOutput, StagedCandidateResult,
};
