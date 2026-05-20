//! User-level identity, decoupled from MLS state.
//!
//! The [`Identity`](crate::identity::Identity) trait defines a user —
//! the bytes that name them and a display form. MLS-specific binding
//! (signing keypair, credential) lives in
//! [`crate::mls_crypto::MlsCredentials`], constructed *from* an
//! `Identity` at User init and held shared.
//!
//! The library is identity-agnostic: the protocol carries identity
//! *bytes*. Integrators bring their own `Identity` impl that derives
//! bytes from whatever they use (Ethereum wallet address, Ed25519
//! public key, account ID, …) and a stable display form.

/// Pluggable user identity.
///
/// Bridges an authenticated user (wallet, account id, …) to anything
/// downstream that needs to name them.
pub trait Identity: Send + Sync + 'static {
    /// Canonical identity bytes — the unique on-the-wire identifier
    /// used by the MLS credential's serialized content, scoring keys,
    /// member-id comparisons, and so on.
    fn identity_bytes(&self) -> &[u8];

    /// Display form (e.g. checksummed `0x…` hex). Stable for the
    /// lifetime of the identity; intended for logs and UI.
    fn identity_display(&self) -> &str;
}
