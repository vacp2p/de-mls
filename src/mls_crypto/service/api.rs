//! Pluggable MLS backend trait, scoped per group.
//!
//! [`MlsService`] is the swap point for MLS implementations. The default
//! impl is [`OpenMlsService`](super::OpenMlsService). One service instance
//! corresponds to one MLS group; the user's MLS credentials and group id
//! are set at construction and every method operates on that implicit
//! group.
//!
//! Conversation construction is intentionally *not* on the trait — concrete impls
//! expose their own constructors (e.g. `OpenMlsService::new_as_creator` /
//! `new_from_welcome`), and key-package generation is also off the trait
//! because a joiner needs to publish a key package before any group exists.
//!
//! The trait surface uses only opaque boundary types: no `openmls::*`
//! types appear here, so swapping in a different MLS engine is purely a
//! matter of writing a new impl. Identity is a separate User-level
//! concept ([`crate::identity::Identity`]) — the MLS service consumes
//! credentials built from it but does not own the identity itself.

use openmls::prelude::Ciphersuite;

use crate::{
    ds::OutboundPacket,
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

/// Per-group MLS backend. Each instance corresponds to one MLS group.
pub trait MlsService: Send + Sync + 'static {
    /// The group id this service is scoped to.
    fn conversation_id(&self) -> &str;

    /// Maximum number of MLS proposals the steward will pack into one
    /// commit batch. Defaults to [`DEFAULT_COMMIT_BATCH_MAX`]; impls may
    /// override per-instance.
    fn commit_batch_max(&self) -> usize {
        DEFAULT_COMMIT_BATCH_MAX
    }

    // ── Conversation lifecycle ──

    /// Tear down all local MLS state for this group. Idempotent so
    /// repeated leave / cleanup is safe.
    fn delete(&self) -> Result<(), MlsError>;

    // ── Membership / state queries ──

    /// Current group members as serialized credential bytes (one entry
    /// per leaf, in MLS leaf order).
    fn members(&self) -> Result<Vec<Vec<u8>>, MlsError>;

    /// Whether `identity` is currently a member.
    fn is_member(&self, identity: &[u8]) -> bool;

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
        &self,
        updates: &[MlsCommitInput],
    ) -> Result<CommitCandidate, MlsError>;

    /// Apply our pending commit, advancing the MLS epoch. Call after a
    /// successful [`create_commit_candidate`](Self::create_commit_candidate)
    /// when our candidate has won the freeze round.
    fn merge_own_commit(&self) -> Result<(), MlsError>;

    /// Roll back the local side effects of
    /// [`create_commit_candidate`](Self::create_commit_candidate):
    /// drop the pending commit and the pending proposals it contained.
    fn discard_own_commit(&self) -> Result<(), MlsError>;

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
    /// (stale epoch, wrong group id, wire-shape mismatch). The caller
    /// must still call `discard_staged_commit` to clean up any partial
    /// state before trying the next candidate.
    fn stage_remote_commit(
        &self,
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError>;

    /// Apply the previously staged inbound commit, advancing the MLS
    /// epoch. Errors if no commit is staged.
    fn merge_staged_commit(&self) -> Result<(), MlsError>;

    /// Roll back [`stage_remote_commit`](Self::stage_remote_commit):
    /// drop the staged commit and clear the pending proposals it
    /// staged on top of.
    fn discard_staged_commit(&self) -> Result<(), MlsError>;

    // ── Application messages ──

    /// Encrypt an application message for the group, returning the raw
    /// MLS wire bytes.
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, MlsError>;

    /// Encode and encrypt `app_msg` and wrap the result as an
    /// [`OutboundPacket`] on the application subtopic. The convenience
    /// path most senders use.
    fn build_message(
        &self,
        app_msg: &AppMessage,
        app_id: &[u8],
    ) -> Result<OutboundPacket, MlsError>;

    /// Strict app-subtopic decrypt: accepts only `Application` messages,
    /// silently ignoring anything else (including proposals and commits).
    /// This guards the app subtopic against MLS-state pollution from
    /// peers that misroute control messages.
    fn decrypt_application_only(&self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError>;

    /// General decrypt: accepts `Application` messages and stores
    /// incoming proposals as pending. Commits are out of scope here —
    /// route them through
    /// [`stage_remote_commit`](Self::stage_remote_commit) so they pass
    /// the validation pipeline.
    fn decrypt(&self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError>;

    /// Peek the untrusted outer kind of an MLS wire message without
    /// processing or signature-checking it. Used for cheap pre-dispatch
    /// lane checks (e.g. "is this a proposal or a commit").
    fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind, MlsError>;
}
