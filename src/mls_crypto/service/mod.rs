//! OpenMLS-backed implementation of [`MlsService`].

use std::sync::RwLock;

use openmls::{
    group::{
        GroupId, MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig, StagedCommit, StagedWelcome,
    },
    key_packages::KeyPackage,
    prelude::{DeserializeBytes, MlsMessageBodyIn, MlsMessageIn},
};
use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsProvider;

use crate::mls_crypto::{DeMlsStorage, IdentityProvider, KeyPackageBytes, MlsError};

mod api;
mod backend;

pub use api::{CIPHERSUITE, MlsService};
use backend::MlsProvider;

/// OpenMLS-backed MLS service, scoped to a single group.
///
/// The service owns one `MlsGroup` plus an optional staged-commit slot
/// for the inbound stage→merge/discard pipeline. Identity and storage are
/// supplied at construction and immutable for the service's lifetime.
/// The implementation of [`MlsService`] for this type lives in `backend.rs`.
pub struct OpenMlsService<S: DeMlsStorage, I: IdentityProvider> {
    pub(super) storage: S,
    pub(super) crypto: RustCrypto,
    pub(super) identity: I,
    pub(super) group_id: String,
    pub(super) mls_group: RwLock<MlsGroup>,
    pub(super) pending_staged_commit: RwLock<Option<StagedCommit>>,
}

impl<S: DeMlsStorage, I: IdentityProvider> OpenMlsService<S, I> {
    /// Create a fresh MLS group as the sole initial member ("creator").
    pub fn new_as_creator(group_id: String, storage: S, identity: I) -> Result<Self, MlsError> {
        let crypto = RustCrypto::default();
        let group = {
            let provider = MlsProvider::new(&crypto, storage.mls_storage());
            let config = MlsGroupCreateConfig::builder()
                .use_ratchet_tree_extension(true)
                .build();
            MlsGroup::new_with_group_id(
                &provider,
                identity.signer(),
                &config,
                GroupId::from_slice(group_id.as_bytes()),
                identity.credential().clone(),
            )?
        };

        Ok(Self {
            storage,
            crypto,
            identity,
            group_id,
            mls_group: RwLock::new(group),
            pending_staged_commit: RwLock::new(None),
        })
    }

    /// Try to join a group from a serialized welcome.
    ///
    /// Returns `Ok(None)` when the welcome doesn't address one of our key
    /// packages — that's the "not for us" branch, not an error. On
    /// `Ok(Some(svc))` the caller has a fully initialized service for the
    /// group the welcome described.
    pub fn new_from_welcome(
        welcome_bytes: &[u8],
        storage: S,
        identity: I,
    ) -> Result<Option<Self>, MlsError> {
        let crypto = RustCrypto::default();

        let (mls_message, _) = MlsMessageIn::tls_deserialize_bytes(welcome_bytes)?;
        let welcome = match mls_message.extract() {
            MlsMessageBodyIn::Welcome(w) => w,
            _ => return Ok(None),
        };

        let is_for_us = welcome.secrets().iter().any(|s| {
            storage
                .is_our_key_package(s.new_member().as_slice())
                .unwrap_or(false)
        });
        if !is_for_us {
            return Ok(None);
        }

        for secret in welcome.secrets() {
            let _ = storage.remove_key_package_ref(secret.new_member().as_slice());
        }

        let group = {
            let provider = MlsProvider::new(&crypto, storage.mls_storage());
            let config = MlsGroupJoinConfig::builder()
                .use_ratchet_tree_extension(true)
                .build();
            StagedWelcome::new_from_welcome(&provider, &config, welcome, None)?
                .into_group(&provider)?
        };

        let group_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();
        Ok(Some(Self {
            storage,
            crypto,
            identity,
            group_id,
            mls_group: RwLock::new(group),
            pending_staged_commit: RwLock::new(None),
        }))
    }

    /// Generate a single-use key package for `identity` backed by `storage`.
    ///
    /// This intentionally takes only storage + identity rather than `&self`,
    /// so a joiner can publish a key package before any MLS group has been
    /// created. The resulting hash ref is registered in `storage` so a
    /// later `new_from_welcome` can identify the welcome as "for us".
    pub fn generate_key_package(storage: &S, identity: &I) -> Result<KeyPackageBytes, MlsError> {
        let crypto = RustCrypto::default();
        let provider = MlsProvider::new(&crypto, storage.mls_storage());

        let kp_bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            &provider,
            identity.signer(),
            identity.credential().clone(),
        )?;

        let kp = kp_bundle.key_package();
        let hash_ref = kp.hash_ref(provider.crypto())?.as_slice().to_vec();
        let bytes = serde_json::to_vec(kp).map_err(MlsError::InvalidJson)?;

        storage.store_key_package_ref(&hash_ref)?;

        Ok(KeyPackageBytes::new(
            bytes,
            identity.identity_bytes().to_vec(),
        ))
    }
}
