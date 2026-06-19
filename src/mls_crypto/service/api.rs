//! [`MlsService`] trait and protocol constants for one MLS group per instance.
//!
//! DE-MLS talks to an MLS engine only through [`MlsService`]. Methods take
//! boundary types from [`crate::mls_crypto`] (wire bytes, [`MlsCommitInput`],
//! etc.) so protocol and app code do not depend on OpenMLS. The only
//! engine-specific export here is [`CIPHERSUITE`], which pins the default
//! [`OpenMlsService`](super::OpenMlsService) backend.
//!
//! # Construction
//!
//! Creating a group, joining from a welcome, and publishing key packages are
//! not on the trait: they are inherent methods on [`OpenMlsService`](super::OpenMlsService),
//! because a joiner must publish a key package before any per-conversation
//! service exists. See the quick-start in [`crate::mls_crypto`].
//!
//! # Constants
//!
//! * [`CIPHERSUITE`] — algorithm suite for [`OpenMlsService`](super::OpenMlsService).
//! * [`DEFAULT_COMMIT_BATCH_MAX`] — default cap on proposals per steward commit;
//!   override per instance with [`MlsService::commit_batch_max`].

use openmls::prelude::Ciphersuite;

use crate::{
    mls_crypto::{
        CommitCandidate, DecryptResult, MlsCommitInput, MlsError, MlsMessageKind,
        StagedCandidateResult,
    },
    protos::de_mls::messages::v1::AppMessage,
};

/// MLS ciphersuite used by the default OpenMLS-backed impl.
pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Default ceiling on MLS proposals per commit batch. Defends against
/// runaway batch growth when freeze recovery preserves work across
/// multiple failed cycles. Per-node config; not synced via `ConversationSync`.
pub const DEFAULT_COMMIT_BATCH_MAX: usize = 50;

/// Per-conversation MLS backend. Each instance corresponds to one MLS group.
///
/// Read-only methods take `&self`; methods that advance MLS state take
/// `&mut self`. Callers serialize via the outer per-session lock.
pub trait MlsService {
    /// The conversation id this service is scoped to.
    fn conversation_id(&self) -> &str;

    /// Maximum number of MLS proposals the steward will pack into one
    /// commit batch. Defaults to [`DEFAULT_COMMIT_BATCH_MAX`]; impls may
    /// override per-instance.
    fn commit_batch_max(&self) -> usize {
        DEFAULT_COMMIT_BATCH_MAX
    }

    // ── Conversation lifecycle ──

    /// Tear down all local MLS state for this conversation. Idempotent so
    /// repeated leave / cleanup is safe.
    fn delete(&mut self) -> Result<(), MlsError>;

    // ── Membership / state queries ──

    /// Current conversation members as serialized credential bytes (one entry
    /// per leaf, in MLS leaf order).
    fn members(&self) -> Result<Vec<Vec<u8>>, MlsError>;

    /// Whether user is currently a member.
    fn is_member(&self, member_id: &[u8]) -> bool;

    /// Current MLS epoch. This is the single source of truth — never
    /// maintain a parallel counter at the app layer.
    fn current_epoch(&self) -> Result<u64, MlsError>;

    // ── Steward-side commit pipeline (we are the committer) ──

    /// Build a commit candidate from a list of membership changes and
    /// stage it locally. Returns the wire bytes (proposals + commit + an
    /// optional welcome) for the steward to broadcast.
    ///
    /// Side effect: leaves MLS holding our pending proposals and pending
    /// commit. The caller MUST follow up with
    /// [`merge_own_commit`](Self::merge_own_commit) once the candidate
    /// wins selection, or [`discard_own_commit`](Self::discard_own_commit)
    /// to roll back.
    fn create_commit_candidate(
        &mut self,
        updates: &[MlsCommitInput],
    ) -> Result<CommitCandidate, MlsError>;

    /// Apply our pending commit, advancing the MLS epoch. Call after a
    /// successful [`create_commit_candidate`](Self::create_commit_candidate)
    /// when our candidate has won the freeze round.
    fn merge_own_commit(&mut self) -> Result<(), MlsError>;

    /// Roll back the local side effects of
    /// [`create_commit_candidate`](Self::create_commit_candidate):
    /// drop the pending commit and the pending proposals it contained.
    fn discard_own_commit(&mut self) -> Result<(), MlsError>;

    // ── Inbound commit pipeline (someone else committed) ──

    /// Validate and stage a remote commit candidate atomically: each
    /// proposal is processed and stored as MLS-pending, then the commit
    /// is processed against that pending set, producing a staged commit
    /// held internally.
    ///
    /// Does **not** merge. The caller validates the result (sender,
    /// authorization, action set vs. voted-approved) and then calls
    /// [`merge_staged_commit`](Self::merge_staged_commit) to advance the
    /// epoch, or [`discard_staged_commit`](Self::discard_staged_commit)
    /// to roll back proposals + staged commit together.
    ///
    /// Returns [`StagedCandidateResult::Aborted`] for benign rejections
    /// (stale epoch, wrong conversation id, wire-shape mismatch). The caller
    /// must still call `discard_staged_commit` to clean up any partial
    /// state before trying the next candidate.
    fn stage_remote_commit(
        &mut self,
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError>;

    /// Apply the previously staged inbound commit, advancing the MLS
    /// epoch. Errors if no commit is staged.
    fn merge_staged_commit(&mut self) -> Result<(), MlsError>;

    /// Roll back [`stage_remote_commit`](Self::stage_remote_commit):
    /// drop the staged commit and clear the pending proposals it
    /// staged on top of.
    fn discard_staged_commit(&mut self) -> Result<(), MlsError>;

    // ── Application messages ──

    /// Encrypt an application message for the conversation, returning the raw
    /// MLS wire bytes.
    fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, MlsError>;

    /// Encode and encrypt `app_msg`, returning the raw payload bytes. The
    /// session wraps these into an [`Outbound`](crate::session::Outbound); the
    /// convenience path most senders use.
    fn build_message(&mut self, app_msg: &AppMessage) -> Result<Vec<u8>, MlsError>;

    /// Strict app-subtopic decrypt: accepts only `Application` messages,
    /// silently ignoring anything else (including proposals and commits).
    /// This guards the app subtopic against MLS-state pollution from
    /// peers that misroute control messages.
    fn decrypt_application_only(&mut self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError>;

    /// General decrypt: accepts `Application` messages and stores
    /// incoming proposals as pending. Commits are out of scope here —
    /// route them through
    /// [`stage_remote_commit`](Self::stage_remote_commit) so they pass
    /// the validation pipeline.
    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError>;

    /// Peek the untrusted outer kind of an MLS wire message without
    /// processing or signature-checking it. Used for cheap pre-dispatch
    /// lane checks (e.g. "is this a proposal or a commit").
    fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind, MlsError>;
}
