//! Local participant identifier trait, decoupled from MLS state.
//!
//! [`MemberId`](crate::member_id::MemberId) is the bytes-and-display abstraction used wherever the
//! protocol needs to name a participant: the bytes go into the MLS
//! credential's serialized content, the steward list, scoring keys, and
//! every other "who is this?" comparison. MLS-specific binding (signing
//! keypair, credential) lives in [`crate::mls_crypto::MlsCredentials`],
//! built from a `MemberId` at User init and shared across every
//! per-conversation service.
//!
//! The library is source-agnostic: integrators bring their own impl
//! that derives bytes from whatever they use (Ethereum wallet address,
//! Ed25519 public key, account id, …) plus a stable display form.

/// Bytes that name the local participant in the protocol, plus a
/// human-readable display form.
pub trait MemberId: Send + Sync + 'static {
    /// Canonical bytes — the unique on-the-wire identifier consumed by
    /// the MLS credential's serialized content, the steward list,
    /// scoring keys, and member-equality checks.
    fn member_id_bytes(&self) -> &[u8];

    /// Display form (e.g. checksummed `0x…` hex). Stable for the
    /// lifetime of the value; intended for logs and UI.
    fn member_id_display(&self) -> &str;
}
