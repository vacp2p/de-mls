//! Pluggable MLS backend trait.
//!
//! [`MlsService`] is the swap point for MLS implementations. The default
//! impl is [`OpenMlsService`](super::OpenMlsService), which is OpenMLS-backed.
//! Future impls (a libchat adapter, an alternative MLS engine) plug in by
//! implementing this trait without changing call sites in `core/` or `app/`.
//!
//! The trait surface uses only opaque boundary types ([`KeyPackageBytes`],
//! [`CommitCandidate`], [`DecryptResult`], [`StagedCandidateResult`],
//! [`MlsMessageKind`], [`GroupUpdate`]) — no `openmls::*` types appear here.
//! Identity is exposed via the associated [`MlsService::Identity`] type,
//! supplied at construction.
//!
//! All methods take `&self` so the trait stays object-safe; a heterogeneous
//! per-group backend map can be held behind `Arc<dyn MlsService>` later if
//! needed. Today, call sites bind it as a generic parameter (`<M: MlsService>`)
//! for monomorphization.

use openmls::prelude::Ciphersuite;

use crate::mls_crypto::{
    CommitCandidate, DecryptResult, GroupUpdate, IdentityProvider, KeyPackageBytes, MlsError,
    MlsMessageKind, StagedCandidateResult,
};

/// MLS ciphersuite used by the default OpenMLS-backed impl.
pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Pluggable MLS backend.
pub trait MlsService: Send + Sync + 'static {
    /// Identity attached to this service. Set at construction time and
    /// immutable thereafter.
    type Identity: IdentityProvider;

    /// Borrow the identity attached to this service.
    fn identity(&self) -> &Self::Identity;

    // ── Group lifecycle ──

    /// Create a new MLS group with the given id. The local identity becomes
    /// the sole initial member.
    fn create_group(&self, group_id: &str) -> Result<(), MlsError>;

    /// Join a group from a serialized welcome if it addresses one of our
    /// key packages. Returns `Some(group_id)` on success, `None` if the
    /// welcome is not for us. The welcome is parsed once.
    fn accept_welcome(&self, welcome_bytes: &[u8]) -> Result<Option<String>, MlsError>;

    /// Drop local MLS state for a group. Idempotent.
    fn delete_group(&self, group_id: &str) -> Result<(), MlsError>;

    // ── Membership / state queries ──

    /// Current group members, as serialized credential bytes.
    fn members(&self, group_id: &str) -> Result<Vec<Vec<u8>>, MlsError>;

    /// Whether `identity` is a current member. Returns `false` (not error)
    /// when the group is unknown locally.
    fn is_member(&self, group_id: &str, identity: &[u8]) -> bool;

    /// Current MLS epoch — the single source of truth.
    fn current_epoch(&self, group_id: &str) -> Result<u64, MlsError>;

    /// Whether local MLS state for this group exists.
    fn has_group(&self, group_id: &str) -> bool;

    // ── Key packages ──

    /// Generate a single-use key package for our identity.
    fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError>;

    // ── Local commit pipeline (steward) ──

    /// Stage a commit candidate from a list of membership updates. Does not
    /// merge; caller follows up with [`merge_own_commit`](Self::merge_own_commit)
    /// or [`discard_own_commit`](Self::discard_own_commit).
    fn create_commit_candidate(
        &self,
        group_id: &str,
        updates: &[GroupUpdate],
    ) -> Result<CommitCandidate, MlsError>;

    /// Merge our pending commit candidate, advancing the MLS epoch.
    fn merge_own_commit(&self, group_id: &str) -> Result<(), MlsError>;

    /// Discard our pending commit candidate.
    fn discard_own_commit(&self, group_id: &str) -> Result<(), MlsError>;

    // ── Inbound candidate pipeline (stage → merge/discard) ──

    /// Stage a remote commit candidate atomically: store every proposal in
    /// the MLS pending queue and stage the commit. The caller validates
    /// the result and then either commits via
    /// [`merge_staged_commit`](Self::merge_staged_commit) or rolls back via
    /// [`discard_staged_commit`](Self::discard_staged_commit).
    ///
    /// On benign mismatch (stale epoch, wrong group, non-proposal in a
    /// proposal slot, non-commit in the commit slot), returns
    /// [`StagedCandidateResult::Aborted`]; the caller still cleans MLS
    /// state via `discard_staged_commit`.
    fn apply_remote_candidate(
        &self,
        group_id: &str,
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError>;

    /// Merge a previously staged inbound commit, advancing the MLS epoch.
    fn merge_staged_commit(&self, group_id: &str) -> Result<(), MlsError>;

    /// Discard a previously staged inbound commit and clear pending
    /// proposals.
    fn discard_staged_commit(&self, group_id: &str) -> Result<(), MlsError>;

    // ── Application messages ──

    /// Encrypt an application message for the group.
    fn encrypt(&self, group_id: &str, plaintext: &[u8]) -> Result<Vec<u8>, MlsError>;

    /// Decrypt an inbound MLS message, accepting only application messages.
    /// Used on the application subtopic to avoid MLS state pollution from
    /// proposals/commits arriving on the wrong channel.
    fn decrypt_application_only(
        &self,
        group_id: &str,
        ciphertext: &[u8],
    ) -> Result<DecryptResult, MlsError>;

    /// Decrypt/process an inbound MLS message — application messages and
    /// proposals only. Commits are routed through
    /// [`apply_remote_candidate`](Self::apply_remote_candidate).
    fn decrypt(&self, group_id: &str, ciphertext: &[u8]) -> Result<DecryptResult, MlsError>;

    /// Inspect the untrusted outer kind of an MLS message — pre-dispatch
    /// lane check on raw bytes (no group state required).
    fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind, MlsError>;
}
