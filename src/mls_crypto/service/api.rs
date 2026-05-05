//! Pluggable MLS backend trait, scoped per group.
//!
//! [`MlsService`] is the swap point for MLS implementations. The default
//! impl is [`OpenMlsService`](super::OpenMlsService). Each instance owns
//! exactly one MLS group; the trait's identity and group-id getters
//! describe that group, and every method operates on the implicit group.
//!
//! Group construction is deliberately *not* on the trait — concrete impls
//! expose their own constructors (e.g. `OpenMlsService::new_as_creator` /
//! `new_from_welcome`). Key-package generation is similarly off the trait,
//! since a joiner needs to publish a key package before any group exists.
//!
//! The trait surface uses only opaque boundary types ([`CommitCandidate`],
//! [`DecryptResult`], [`StagedCandidateResult`], [`MlsMessageKind`],
//! [`GroupUpdate`]) — no `openmls::*` types appear here. Identity is
//! exposed via the associated [`MlsService::Identity`] type, supplied at
//! construction.

use openmls::prelude::Ciphersuite;

use crate::mls_crypto::{
    CommitCandidate, DecryptResult, GroupUpdate, IdentityProvider, MlsError, MlsMessageKind,
    StagedCandidateResult,
};

/// MLS ciphersuite used by the default OpenMLS-backed impl.
pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Per-group MLS backend. Each instance corresponds to one MLS group.
pub trait MlsService: Send + Sync + 'static {
    /// Identity attached to this service. Set at construction time and
    /// immutable thereafter.
    type Identity: IdentityProvider;

    /// Borrow the identity attached to this service.
    fn identity(&self) -> &Self::Identity;

    /// The group id this service is scoped to. Set at construction time
    /// and immutable thereafter.
    fn group_id(&self) -> &str;

    // ── Group lifecycle ──

    /// Drop local MLS state for this group. Idempotent.
    fn delete(&self) -> Result<(), MlsError>;

    // ── Membership / state queries ──

    /// Current group members, as serialized credential bytes.
    fn members(&self) -> Result<Vec<Vec<u8>>, MlsError>;

    /// Whether `identity` is a current member.
    fn is_member(&self, identity: &[u8]) -> bool;

    /// Current MLS epoch — the single source of truth.
    fn current_epoch(&self) -> Result<u64, MlsError>;

    // ── Local commit pipeline (steward) ──

    /// Stage a commit candidate from a list of membership updates. Does not
    /// merge; caller follows up with [`merge_own_commit`](Self::merge_own_commit)
    /// or [`discard_own_commit`](Self::discard_own_commit).
    fn create_commit_candidate(&self, updates: &[GroupUpdate])
    -> Result<CommitCandidate, MlsError>;

    /// Merge our pending commit candidate, advancing the MLS epoch.
    fn merge_own_commit(&self) -> Result<(), MlsError>;

    /// Discard our pending commit candidate.
    fn discard_own_commit(&self) -> Result<(), MlsError>;

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
        proposals: &[Vec<u8>],
        commit_bytes: &[u8],
    ) -> Result<StagedCandidateResult, MlsError>;

    /// Merge a previously staged inbound commit, advancing the MLS epoch.
    fn merge_staged_commit(&self) -> Result<(), MlsError>;

    /// Discard a previously staged inbound commit and clear pending
    /// proposals.
    fn discard_staged_commit(&self) -> Result<(), MlsError>;

    // ── Application messages ──

    /// Encrypt an application message for the group.
    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>, MlsError>;

    /// Decrypt an inbound MLS message, accepting only application messages.
    /// Used on the application subtopic to avoid MLS state pollution from
    /// proposals/commits arriving on the wrong channel.
    fn decrypt_application_only(&self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError>;

    /// Decrypt/process an inbound MLS message — application messages and
    /// proposals only. Commits are routed through
    /// [`apply_remote_candidate`](Self::apply_remote_candidate).
    fn decrypt(&self, ciphertext: &[u8]) -> Result<DecryptResult, MlsError>;

    /// Inspect the untrusted outer kind of an MLS message — pre-dispatch
    /// lane check on raw bytes (no group state required).
    fn inspect_message_kind(&self, message_bytes: &[u8]) -> Result<MlsMessageKind, MlsError>;
}
