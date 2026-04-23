//! Main MLS service providing all cryptographic operations.

use std::{collections::HashMap, sync::RwLock};

use openmls::{
    group::{MlsGroup, StagedCommit},
    prelude::{Ciphersuite, Proposal},
};
use openmls_rust_crypto::{MemoryStorage, RustCrypto};
use openmls_traits::OpenMlsProvider;

use crate::mls_crypto::{
    DeMlsStorage, MlsError, MlsProposalAction, MlsServiceError, Result, identity::IdentityData,
};

mod commits;
mod groups;
mod identity;
mod key_packages;
mod messages;

/// The MLS ciphersuite used for all operations.
pub const CIPHERSUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Internal OpenMLS provider that wraps storage.
struct MlsProvider<'a> {
    crypto: &'a RustCrypto,
    storage: &'a MemoryStorage,
}

impl<'a> OpenMlsProvider for MlsProvider<'a> {
    type CryptoProvider = RustCrypto;
    type RandProvider = RustCrypto;
    type StorageProvider = MemoryStorage;

    fn crypto(&self) -> &Self::CryptoProvider {
        self.crypto
    }

    fn rand(&self) -> &Self::RandProvider {
        self.crypto
    }

    fn storage(&self) -> &Self::StorageProvider {
        self.storage
    }
}

/// Main MLS service - unified API for all MLS operations.
///
/// Groups are managed internally by group ID string. The service handles:
/// - Identity initialization and management
/// - Key package generation
/// - Group creation and joining
/// - Message encryption and decryption
/// - Steward commit operations
pub struct MlsService<S: DeMlsStorage> {
    storage: S,
    crypto: RustCrypto,
    identity: RwLock<Option<IdentityData>>,
    groups: RwLock<HashMap<String, MlsGroup>>,
    pending_staged_commits: RwLock<HashMap<String, StagedCommit>>,
}

impl<S> MlsService<S>
where
    S: DeMlsStorage<MlsStorage = MemoryStorage>,
{
    /// Create a new MLS service with the given storage backend.
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            crypto: RustCrypto::default(),
            identity: RwLock::new(None),
            groups: RwLock::new(HashMap::new()),
            pending_staged_commits: RwLock::new(HashMap::new()),
        }
    }

    // ══════════════════════════════════════════════════════════
    // Internal
    // ══════════════════════════════════════════════════════════

    fn extract_proposal_action(group: &MlsGroup, proposal: &Proposal) -> Result<MlsProposalAction> {
        match proposal {
            Proposal::Add(add) => {
                let id = add
                    .key_package()
                    .leaf_node()
                    .credential()
                    .serialized_content()
                    .to_vec();
                Ok(MlsProposalAction::Add(id))
            }
            Proposal::Remove(remove) => {
                let removed = remove.removed();
                let id = group
                    .member(removed)
                    .map(|c| c.serialized_content().to_vec())
                    .ok_or(MlsError::Service(MlsServiceError::UnknownLeafIndex(
                        removed.u32(),
                    )))?;
                Ok(MlsProposalAction::Remove(id))
            }
            other => Ok(MlsProposalAction::Other(format!("{other:?}"))),
        }
    }

    fn make_provider(&self) -> MlsProvider<'_> {
        MlsProvider {
            crypto: &self.crypto,
            storage: self.storage.mls_storage(),
        }
    }
}
