//! Crate-internal test fixtures: a real creator-side MLS service builder over
//! `OpenMlsRustCrypto` plus minimal scoring / steward stubs, for tests that
//! construct a [`crate::Conversation`] without standing up real scoring or
//! steward backends.
//!
//! The stub scoring / steward methods are `unreachable!()` — tests should only
//! exercise the handful of paths the test specifically targets. If a test
//! reaches into an `unreachable!()` branch, that's a sign the test is touching
//! state it shouldn't be (and the panic location pinpoints the leak).

use std::collections::HashMap;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;
use openmls::credentials::{BasicCredential, CredentialWithKey};
use openmls::group::MlsGroupCreateConfig;
use openmls::prelude::Ciphersuite;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

use crate::{
    ConsensusEngine, StewardListConfig, StewardListService,
    consensus::outcome_bus::OutcomeReceiver, defaults::DefaultConsensusPlugin,
    mls_crypto::MlsService,
};

/// Build a `ConsensusEngine<DefaultConsensusPlugin>` paired with a
/// subscribed receiver for [`crate::Conversation::new`].
pub(crate) fn make_test_consensus_service()
-> (ConsensusEngine<DefaultConsensusPlugin>, OutcomeReceiver) {
    use crate::consensus::outcome_bus::OutcomeBus;
    use hashgraph_like_consensus::{
        events::ConsensusEventBus, service::ConsensusService, storage::InMemoryConsensusStorage,
    };
    let bus = OutcomeBus::default();
    let rx = bus.subscribe();
    let service = ConsensusService::new_with_components(
        InMemoryConsensusStorage::new(),
        bus,
        EthereumConsensusSigner::new(PrivateKeySigner::random()),
        10,
    );
    (service, rx)
}

/// Ciphersuite the crate-internal fixtures pin.
pub(crate) const TEST_SUITE: Ciphersuite =
    Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// Concrete OpenMLS provider the crate-internal fixtures run.
pub(crate) type TestProvider = OpenMlsRustCrypto;

/// MLS service type the crate-internal unit tests run.
pub(crate) type TestMls = MlsService;

/// Build a real creator-side MLS service for `member_id` against a fresh
/// test-held provider, returning the service alongside the provider and the
/// signer that seeded it (both needed for any signing path the test
/// exercises). The guard tests this serves early-return before MLS advances,
/// but the service must still be a real, queryable group.
pub(crate) fn make_creator_mls(member_id: &[u8]) -> (TestMls, TestProvider, SignatureKeyPair) {
    let signer = SignatureKeyPair::new(TEST_SUITE.signature_algorithm()).expect("signer");
    let credential = CredentialWithKey {
        credential: BasicCredential::new(member_id.to_vec()).into(),
        signature_key: signer.to_public_vec().into(),
    };
    let provider = TestProvider::default();
    let mls = MlsService::new_as_creator(
        "test-conversation".to_string(),
        &provider,
        credential,
        &MlsGroupCreateConfig::builder()
            .use_ratchet_tree_extension(true)
            .build(),
        &signer,
    )
    .expect("create creator mls");
    (mls, provider, signer)
}

/// Steward list where the local member is NOT a steward (no list installed).
pub(crate) fn steward_service_member() -> StewardListService {
    StewardListService::empty(StewardListConfig::new(1, 5).unwrap())
}

/// Steward list where `member_id` IS the sole steward.
pub(crate) fn steward_service_steward(member_id: &[u8]) -> StewardListService {
    let mut service = StewardListService::empty(StewardListConfig::new(1, 5).unwrap());
    service
        .install_list(0, &[member_id.to_vec()], 1, 0)
        .expect("install single-member steward list");
    service
}

/// Peer-score storage whose writes fail from the `fail_after`-th `set` on.
///
/// The shipped [`crate::defaults::InMemoryPeerScoreStorage`] is `Infallible`,
/// so without this the entire reason [`crate::PeerScoreStorage`] is fallible
/// goes unexercised: nothing else in the crate can produce a
/// [`crate::ConversationError::ScoreStorage`].
#[derive(Debug, Default)]
pub(crate) struct FailingPeerScoreStorage {
    scores: HashMap<Vec<u8>, i64>,
    /// Successful `set` calls to allow before failing. `None` never fails.
    pub fail_after: Option<usize>,
    /// Fail `get` and `all_scores` too, not just writes.
    fail_reads: bool,
    writes: usize,
}

/// The failure a [`FailingPeerScoreStorage`] injects.
#[derive(Debug, thiserror::Error)]
#[error("peer-score storage write failed")]
pub(crate) struct ScoreStorageFailure;

impl FailingPeerScoreStorage {
    /// Fail every `set` once `n` have succeeded in total.
    pub(crate) fn failing_after(n: usize) -> Self {
        Self {
            fail_after: Some(n),
            ..Default::default()
        }
    }

    /// Fail every operation, reads included — for a test that only needs the
    /// scoring backend to be broken.
    pub(crate) fn always_failing() -> Self {
        Self {
            fail_reads: true,
            fail_after: Some(0),
            ..Default::default()
        }
    }

    /// Allow `n` more writes from wherever the counter now stands, then fail.
    /// Lets a test set up state freely and cut off a later batch partway.
    pub(crate) fn allow_further(&mut self, n: usize) {
        self.fail_after = Some(self.writes + n);
    }
}

impl crate::PeerScoreStorage for FailingPeerScoreStorage {
    type Error = ScoreStorageFailure;

    fn get(&self, member_id: &[u8]) -> Result<Option<i64>, Self::Error> {
        if self.fail_reads {
            return Err(ScoreStorageFailure);
        }
        Ok(self.scores.get(member_id).copied())
    }

    fn set(&mut self, member_id: &[u8], score: i64) -> Result<(), Self::Error> {
        if self.fail_after.is_some_and(|n| self.writes >= n) {
            return Err(ScoreStorageFailure);
        }
        self.writes += 1;
        self.scores.insert(member_id.to_vec(), score);
        Ok(())
    }

    fn remove(&mut self, member_id: &[u8]) -> Result<(), Self::Error> {
        self.scores.remove(member_id);
        Ok(())
    }

    fn all_scores(&self) -> Result<Vec<(Vec<u8>, i64)>, Self::Error> {
        if self.fail_reads {
            return Err(ScoreStorageFailure);
        }
        Ok(self.scores.iter().map(|(k, v)| (k.clone(), *v)).collect())
    }
}
