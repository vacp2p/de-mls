//! Gateway-side MLS defaults: the concrete plug-in factory the reference
//! integrator wires into [`User`](crate::user::User).
//!
//! de-mls names no concrete MLS backend — the engine is generic over an
//! `OpenMlsProvider`, takes the credential + ciphersuite from a key package,
//! and the signer per call. This module supplies the integrator half: the
//! OpenMLS reference provider (`OpenMlsRustCrypto`), credential generation
//! from a member id, and key-package minting.

use std::sync::Mutex;

use de_mls::{
    ConversationPlugins, DeterministicStewardList, PeerScoringService, ScoringConfig,
    StewardListConfig, default_score_deltas,
    defaults::{DefaultPeerScoring, DefaultStewardList, InMemoryPeerScoreStorage},
    mls_crypto::{KeyPackageBytes, MlsError, OpenMlsService},
};
use openmls::{
    credentials::{BasicCredential, CredentialWithKey},
    key_packages::KeyPackage,
    prelude::{Ciphersuite, tls_codec::Serialize as _},
};
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use openmls_traits::signatures::Signer;

/// Ciphersuite the gateway pins for every conversation it creates or joins.
pub const GATEWAY_SUITE: Ciphersuite = Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// MLS service the gateway runs: the reference engine over `OpenMlsRustCrypto`.
pub type GatewayMls = OpenMlsService<OpenMlsRustCrypto>;

/// Build a fresh MLS credential + signing keypair for `member_id`. The
/// member-id bytes become the credential's serialized content, so the MLS
/// leaf and the protocol's "who is this?" checks agree.
pub fn build_credential(
    member_id: &[u8],
) -> Result<(CredentialWithKey, SignatureKeyPair), MlsError> {
    let signer = SignatureKeyPair::new(GATEWAY_SUITE.signature_algorithm())?;
    let credential = CredentialWithKey {
        credential: BasicCredential::new(member_id.to_vec()).into(),
        signature_key: signer.to_public_vec().into(),
    };
    Ok((credential, signer))
}

/// Reference plug-in factory over `OpenMlsRustCrypto`. Holds the member's
/// credential + signer and mints key packages plus per-conversation plug-in
/// instances. One factory per `User`.
pub struct DefaultConversationPluginsFactory {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    /// Provider stashed by [`Self::generate_key_package`] so the matching
    /// [`Self::welcome_mls`] can reuse it — it owns the key package's private
    /// keys, which the welcome's `StagedWelcome` needs to decrypt the group
    /// secrets. `Mutex` (not `RefCell`) because `User` is shared across tasks.
    pending_provider: Mutex<Option<OpenMlsRustCrypto>>,
}

impl DefaultConversationPluginsFactory {
    pub fn new(credential: CredentialWithKey, signer: SignatureKeyPair) -> Self {
        Self {
            credential,
            signer,
            pending_provider: Mutex::new(None),
        }
    }

    /// Mint a single-use key package in a fresh provider, stashing that
    /// provider so a later [`Self::welcome_mls`] can open the welcome with the
    /// key package's private keys.
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        let provider = OpenMlsRustCrypto::default();
        let member_id = self.credential.credential.serialized_content().to_vec();
        let bundle = KeyPackage::builder().build(
            GATEWAY_SUITE,
            &provider,
            &self.signer,
            self.credential.clone(),
        )?;
        let bytes = bundle
            .key_package()
            .tls_serialize_detached()
            .map_err(MlsError::KeyPackageTls)?;
        if let Ok(mut stash) = self.pending_provider.lock() {
            *stash = Some(provider);
        }
        Ok(KeyPackageBytes::new(bytes, member_id))
    }

    /// Build an MLS service seeding a brand-new conversation we create.
    pub fn create_mls(
        &self,
        conversation_id: String,
        signer: &impl Signer,
    ) -> Result<GatewayMls, MlsError> {
        OpenMlsService::new_as_creator(
            conversation_id,
            OpenMlsRustCrypto::default(),
            self.credential.clone(),
            GATEWAY_SUITE,
            signer,
        )
    }

    /// Try to open an MLS service from a serialized welcome. `Ok(None)` when
    /// the welcome isn't for us.
    pub fn welcome_mls(&self, welcome_bytes: &[u8]) -> Result<Option<GatewayMls>, MlsError> {
        // Reuse the provider stashed when we minted our key package; with no
        // stash (we never minted one) a fresh empty provider holds no matching
        // key package, so the join cleanly yields `None`.
        let provider = self
            .pending_provider
            .lock()
            .ok()
            .and_then(|mut stash| stash.take())
            .unwrap_or_default();
        OpenMlsService::new_from_welcome(welcome_bytes, provider)
    }

    /// Build a fresh peer-scoring plug-in.
    pub fn make_scoring(&self, config: &ScoringConfig) -> DefaultPeerScoring {
        PeerScoringService::new(
            InMemoryPeerScoreStorage::new(),
            default_score_deltas(),
            config.clone(),
        )
    }

    /// Build a fresh (empty) steward-list plug-in.
    pub fn make_steward_list(
        &self,
        conversation_id: &[u8],
        config: StewardListConfig,
    ) -> DefaultStewardList {
        DeterministicStewardList::empty(conversation_id.to_vec(), config)
    }
}

impl ConversationPlugins for DefaultConversationPluginsFactory {
    type Mls = GatewayMls;
    type Scoring = DefaultPeerScoring;
    type StewardList = DefaultStewardList;
}
