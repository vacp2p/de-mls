//! Reference OpenMLS-backed implementation of the
//! [`MlsService`](crate::mls_crypto::MlsService) contract.
//!
//! `OpenMlsService` is the per-conversation engine — generic over the storage
//! backend and the crypto provider, with the signing keypair, credential, and
//! ciphersuite injected via `MlsCredentials`, so it pins none of those choices.
//! `DeMlsStorage` is the storage backend it runs on; `CIPHERSUITE` is the
//! default suite `MlsCredentials` falls back to when a caller has no preference.
//! Only integrators and tests construct these — the protocol layer talks to the
//! trait.

use openmls::prelude::Ciphersuite;

mod backend;
mod credentials;
mod service;
mod storage;

/// Default MLS ciphersuite [`MlsCredentials::from_member_id`] uses when a
/// caller expresses no preference. Callers wanting another suite pass it to
/// [`MlsCredentials::generate`].
pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Ceiling on MLS proposals per commit batch used by the reference
/// [`OpenMlsService`] backend. Defends against runaway batch growth when
/// freeze recovery preserves work across multiple failed cycles. Per-node
/// policy; not synced via `ConversationSync`.
pub const DEFAULT_COMMIT_BATCH_MAX: usize = 50;

pub use credentials::MlsCredentials;
pub use service::OpenMlsService;
pub use storage::DeMlsStorage;
