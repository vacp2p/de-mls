//! [`KeyPackageProvider`] — identity-bound MLS key package generation.
//!
//! A key package is a single-use credential a joiner shares with a steward so
//! the steward can MLS-invite them. It is bound to the user's identity
//! (MLS credentials), **not** to any conversation. Generation happens
//! independently of `start_conversation`; sharing (broadcasting on the
//! welcome subtopic, posting to a directory, etc.) is a separate step.
//!
//! This separation lets integrators pre-mint or persist KPs (e.g., a
//! registration-service pattern) without involving any per-conversation
//! state.

use crate::mls_crypto::{KeyPackageBytes, MlsError};

/// Identity-bound, conversation-free key package generator.
///
/// The default implementation in the app layer wraps an
/// `Arc<MlsCredentials>` + `Arc<MemoryDeMlsStorage>`; integrators can plug
/// in alternative implementations backed by an HSM, a persisted KP store,
/// or a registration service.
pub trait KeyPackageProvider: Send + Sync + 'static {
    /// Mint a fresh key package for this user. Each call produces a new
    /// single-use credential signed by the user's MLS key.
    fn generate(&self) -> Result<KeyPackageBytes, MlsError>;
}
