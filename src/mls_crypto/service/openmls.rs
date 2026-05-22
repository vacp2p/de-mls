//! [`OpenMlsService`] — the OpenMLS-backed
//! [`MlsService`](crate::mls_crypto::MlsService) impl, plus its
//! `new_as_creator` / `new_from_welcome` constructors and the
//! conversation-free [`OpenMlsService::generate_key_package`] used by
//! joiners before any MLS state exists.

use std::sync::Arc;

use openmls::prelude::tls_codec::Serialize as _;
use openmls::{
    group::{
        GroupId, MlsGroup, MlsGroupCreateConfig, MlsGroupJoinConfig, StagedCommit, StagedWelcome,
    },
    key_packages::KeyPackage,
    prelude::{DeserializeBytes, MlsMessageBodyIn, MlsMessageIn},
};
use openmls_rust_crypto::RustCrypto;
use openmls_traits::OpenMlsProvider;

use crate::mls_crypto::{
    DeMlsStorage, KeyPackageBytes, MlsCredentials, MlsError,
    service::{CIPHERSUITE, backend::MlsProvider},
};

/// OpenMLS-backed MLS service, scoped to a single conversation. Owns
/// one `MlsGroup` plus an optional staged-commit slot for the inbound
/// stage→merge/discard pipeline. Credentials are `Arc<MlsCredentials>`
/// so one user's keypair backs every per-conversation service.
pub struct OpenMlsService<S: DeMlsStorage> {
    pub(super) storage: S,
    pub(super) crypto: RustCrypto,
    pub(super) credentials: Arc<MlsCredentials>,
    pub(super) conversation_id: String,
    pub(super) group: MlsGroup,
    pub(super) pending_staged_commit: Option<StagedCommit>,
}

impl<S: DeMlsStorage> OpenMlsService<S> {
    /// Create a fresh MLS group as the sole initial member ("creator").
    pub fn new_as_creator(
        conversation_id: String,
        storage: S,
        credentials: Arc<MlsCredentials>,
    ) -> Result<Self, MlsError> {
        let crypto = RustCrypto::default();
        let group = {
            let provider = MlsProvider::new(&crypto, storage.mls_storage());
            let config = MlsGroupCreateConfig::builder()
                .use_ratchet_tree_extension(true)
                .build();
            MlsGroup::new_with_group_id(
                &provider,
                credentials.signer(),
                &config,
                GroupId::from_slice(conversation_id.as_bytes()),
                credentials.credential().clone(),
            )?
        };

        Ok(Self {
            storage,
            crypto,
            credentials,
            conversation_id,
            group,
            pending_staged_commit: None,
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
        credentials: Arc<MlsCredentials>,
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
            storage.remove_key_package_ref(secret.new_member().as_slice())?;
        }

        let group = {
            let provider = MlsProvider::new(&crypto, storage.mls_storage());
            let config = MlsGroupJoinConfig::builder()
                .use_ratchet_tree_extension(true)
                .build();
            StagedWelcome::new_from_welcome(&provider, &config, welcome, None)?
                .into_group(&provider)?
        };

        let conversation_id = String::from_utf8_lossy(group.group_id().as_slice()).to_string();
        Ok(Some(Self {
            storage,
            crypto,
            credentials,
            conversation_id,
            group,
            pending_staged_commit: None,
        }))
    }

    /// Generate a single-use key package for `credentials` backed by `storage`.
    ///
    /// Takes only storage + credentials rather than `&self`, so a joiner
    /// can publish a key package before any MLS group has been created.
    /// The resulting hash ref is registered in `storage` so a later
    /// `new_from_welcome` can identify the welcome as "for us".
    pub fn generate_key_package(
        storage: &S,
        credentials: &MlsCredentials,
    ) -> Result<KeyPackageBytes, MlsError> {
        let crypto = RustCrypto::default();
        let provider = MlsProvider::new(&crypto, storage.mls_storage());

        let kp_bundle = KeyPackage::builder().build(
            CIPHERSUITE,
            &provider,
            credentials.signer(),
            credentials.credential().clone(),
        )?;

        let kp = kp_bundle.key_package();
        let hash_ref = kp.hash_ref(provider.crypto())?.as_slice().to_vec();
        let bytes = kp
            .tls_serialize_detached()
            .map_err(MlsError::KeyPackageTls)?;

        storage.store_key_package_ref(&hash_ref)?;

        let member_id_bytes = credentials
            .credential()
            .credential
            .serialized_content()
            .to_vec();

        Ok(KeyPackageBytes::new(bytes, member_id_bytes))
    }
}
