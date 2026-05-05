//! OpenMLS-backed implementation of [`MlsService`].

use std::{
    collections::HashMap,
    sync::{OnceLock, RwLock},
};

use openmls::group::{MlsGroup, StagedCommit};
use openmls_rust_crypto::RustCrypto;

use crate::mls_crypto::{DeMlsStorage, identity::IdentityData};

mod backend;

pub use backend::{CIPHERSUITE, MlsService};

/// OpenMLS-backed MLS service.
///
/// Holds per-group MLS state centrally in `RwLock<HashMap<String, MlsGroup>>`,
/// keyed by `group_id`. The implementation of [`MlsService`] for this type
/// lives in `backend.rs`.
pub struct OpenMlsService<S: DeMlsStorage> {
    storage: S,
    crypto: RustCrypto,
    wallet_bytes: OnceLock<Vec<u8>>,
    wallet_hex: OnceLock<String>,
    identity: RwLock<Option<IdentityData>>,
    groups: RwLock<HashMap<String, MlsGroup>>,
    pending_staged_commits: RwLock<HashMap<String, StagedCommit>>,
}

impl<S: DeMlsStorage> OpenMlsService<S> {
    /// Create a new MLS service with the given storage backend.
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            crypto: RustCrypto::default(),
            wallet_bytes: OnceLock::new(),
            wallet_hex: OnceLock::new(),
            identity: RwLock::new(None),
            groups: RwLock::new(HashMap::new()),
            pending_staged_commits: RwLock::new(HashMap::new()),
        }
    }
}
