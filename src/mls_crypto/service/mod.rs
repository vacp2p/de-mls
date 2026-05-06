//! OpenMLS-backed implementation of [`MlsService`].

use std::{collections::HashMap, sync::RwLock};

use openmls::group::{MlsGroup, StagedCommit};
use openmls_rust_crypto::RustCrypto;

use crate::mls_crypto::{DeMlsStorage, IdentityProvider};

mod api;
mod backend;

pub use api::{CIPHERSUITE, MlsService};

/// OpenMLS-backed MLS service.
///
/// Holds per-group MLS state centrally in `RwLock<HashMap<String, MlsGroup>>`,
/// keyed by `group_id`. Identity is supplied at construction via an
/// [`IdentityProvider`] and is immutable for the lifetime of the service.
/// The implementation of [`MlsService`] for this type lives in `backend.rs`.
pub struct OpenMlsService<S: DeMlsStorage, I: IdentityProvider> {
    pub(super) storage: S,
    pub(super) crypto: RustCrypto,
    pub(super) identity: I,
    pub(super) groups: RwLock<HashMap<String, MlsGroup>>,
    pub(super) pending_staged_commits: RwLock<HashMap<String, StagedCommit>>,
}

impl<S: DeMlsStorage, I: IdentityProvider> OpenMlsService<S, I> {
    /// Create a new MLS service with the given storage backend and
    /// identity. The identity's signing keypair must already be registered
    /// with the OpenMLS storage if the impl needs it (see
    /// [`crate::mls_crypto::WalletIdentity::from_wallet`]).
    pub fn new(storage: S, identity: I) -> Self {
        Self {
            storage,
            crypto: RustCrypto::default(),
            identity,
            groups: RwLock::new(HashMap::new()),
            pending_staged_commits: RwLock::new(HashMap::new()),
        }
    }
}
