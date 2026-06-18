//! Step 1 proof: a `Conversation` can be built and queried straight from
//! direct arguments, with no `User` in the picture. The integrator holds its
//! credential + signer plus shared consensus storage + signer, builds each
//! conversation's plug-in instances and consensus service inline — exactly what
//! `User` does internally, here done by hand.

mod common;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;
use openmls::credentials::CredentialWithKey;
use openmls_basic_credential::SignatureKeyPair;

use de_mls::defaults::{DefaultConsensusPlugin, DefaultPeerScoring, DefaultStewardList};
use de_mls::mls_crypto::KeyPackageBytes;
use de_mls::{
    ConsensusPlugin, ConsensusServiceFor, ConversationEvent, ScoringConfig, StewardListConfig,
};
use de_mls::{Conversation, ConversationState};

use common::{
    TEST_SUITE, TestProvider, make_scoring, make_steward, mint_key_package, test_credential,
    wallet::WalletMemberId,
};

/// Per-conversation stack the standalone tests build.
type TestConversation =
    Conversation<DefaultConsensusPlugin, DefaultPeerScoring, DefaultStewardList>;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// The shared, conversation-agnostic state an integrator keeps once: its
/// credential + signer, the consensus storage + signer, the identity, and one
/// OpenMLS provider reused across every call. The creator seeds its group into
/// it; a joiner mints its key package into it and later opens the welcome from
/// it.
struct Integrator {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    consensus_storage: <DefaultConsensusPlugin as ConsensusPlugin>::ConsensusStorage,
    consensus_signer: <DefaultConsensusPlugin as ConsensusPlugin>::Signer,
    member_id: WalletMemberId,
    provider: TestProvider,
}

impl Integrator {
    fn new() -> Self {
        Self::with_key(ALICE)
    }

    fn with_key(private_key: &str) -> Self {
        let eth_signer = PrivateKeySigner::from_str(private_key).expect("valid private key");
        let member_id = WalletMemberId::from_address(eth_signer.address());
        let (credential, signer) = test_credential(member_id.member_id_bytes());
        Self {
            credential,
            signer,
            consensus_storage: DefaultConsensusPlugin::new_storage(),
            consensus_signer: EthereumConsensusSigner::new(eth_signer),
            member_id,
            provider: TestProvider::default(),
        }
    }

    fn scoring(&self) -> DefaultPeerScoring {
        make_scoring(&ScoringConfig::default())
    }

    fn steward(&self) -> DefaultStewardList {
        make_steward(
            self.member_id.member_id_bytes(),
            StewardListConfig::default(),
        )
    }

    /// Mint a single-use key package into this integrator's reused provider,
    /// which thereby holds the private keys for the matching welcome.
    fn mint_key_package(&self) -> KeyPackageBytes {
        mint_key_package(&self.provider, &self.credential, &self.signer)
    }

    /// Fresh per-conversation consensus service drawn from the shared state.
    /// Clones the shared storage (scope-keyed) and gets its own private event
    /// bus.
    fn consensus(&self) -> ConsensusServiceFor<DefaultConsensusPlugin> {
        ConsensusServiceFor::<DefaultConsensusPlugin>::new_with_components(
            self.consensus_storage.clone(),
            DefaultConsensusPlugin::new_event_bus(),
            self.consensus_signer.clone(),
            10,
        )
    }

    /// This integrator's `app_id` — the member id doubles as it so two
    /// integrators in one test don't echo-drop each other's packets.
    fn app_id(&self) -> Arc<[u8]> {
        Arc::from(self.member_id.member_id_bytes())
    }
}

#[test]
fn create_builds_a_working_steward_session_without_user() {
    let integrator = Integrator::new();
    let conversation: TestConversation = Conversation::create(
        "standalone",
        &integrator.provider,
        integrator.credential.clone(),
        TEST_SUITE,
        &integrator.signer,
        integrator.scoring(),
        integrator.steward(),
        integrator.consensus(),
        integrator.app_id(),
        de_mls::ConversationConfig::default(),
        integrator.member_id.member_id_bytes(),
    )
    .expect("create");

    assert_eq!(conversation.state(), ConversationState::Working);
    assert!(
        conversation.is_steward(),
        "creator is the sole steward at epoch 0"
    );
    let (epoch, _retry) = conversation.epoch_and_retry().expect("epoch");
    assert_eq!(epoch, 0);

    // The opening phase is buffered for whoever drains the session.
    let events = conversation.drain_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ConversationEvent::PhaseChange(ConversationState::Working)
        )),
        "create buffers an opening Working PhaseChange"
    );
}

/// Sub-second timers so the solo creator's bundled-YES consensus and the
/// inactivity commit land within a few polling rounds.
fn fast_config() -> de_mls::ConversationConfig {
    de_mls::ConversationConfig {
        commit_inactivity_duration: Duration::from_millis(50),
        freeze_duration: Duration::from_millis(20),
        voting_delay: Duration::from_millis(30),
        election_voting_delay: Duration::from_millis(30),
        consensus_timeout: Duration::from_millis(150),
        proposal_expiration: Duration::from_millis(2000),
        ..Default::default()
    }
}

#[test]
fn join_completes_in_one_call() {
    let alice = Integrator::new();
    let bob = Integrator::with_key(BOB);

    let mut creator: TestConversation = Conversation::create(
        "standalone-welcome",
        &alice.provider,
        alice.credential.clone(),
        TEST_SUITE,
        &alice.signer,
        alice.scoring(),
        alice.steward(),
        alice.consensus(),
        alice.app_id(),
        fast_config(),
        alice.member_id.member_id_bytes(),
    )
    .expect("create");

    // Bob mints a key package out of band; Alice — the sole member — proposes
    // the add, so her bundled YES resolves consensus on its own.
    let bob_kp = bob.mint_key_package();
    creator
        .add_member(&alice.provider, bob_kp.as_bytes(), &alice.signer)
        .expect("add member");

    // Drive the creator until the welcome is minted.
    let mut welcome = None;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(30));
        creator.poll(&alice.provider, &alice.signer);
        for event in creator.drain_events() {
            if let ConversationEvent::WelcomeReady {
                welcome: w,
                minted_locally: true,
            } = event
            {
                welcome = Some(w);
            }
        }
        if welcome.is_some() {
            break;
        }
    }
    let welcome = welcome.expect("creator mints a welcome");

    // A welcome not addressed to us yields `Ok(None)` — a fresh integrator that
    // never minted the key package can't open it.
    let bystander = Integrator::with_key(ALICE);
    let bystander_join: Option<TestConversation> = Conversation::join(
        &bystander.provider,
        &welcome.welcome_bytes,
        &welcome.conversation_sync_bytes,
        bystander.scoring(),
        bystander.steward(),
        bystander.consensus(),
        bystander.app_id(),
        fast_config(),
        bystander.member_id.member_id_bytes(),
        &bystander.signer,
    )
    .expect("join is not an error for the wrong addressee");
    assert!(
        bystander_join.is_none(),
        "a welcome for someone else is ignored"
    );

    // `join` opens the welcome and completes the whole joiner path in one call:
    // run the join side-effects, apply the bundled sync. Only the addressed
    // joiner — holding the KP provider — gets `Some`.
    let joined: TestConversation = Conversation::join(
        &bob.provider,
        &welcome.welcome_bytes,
        &welcome.conversation_sync_bytes,
        bob.scoring(),
        bob.steward(),
        bob.consensus(),
        bob.app_id(),
        fast_config(),
        bob.member_id.member_id_bytes(),
        &bob.signer,
    )
    .expect("join")
    .expect("welcome addresses bob");
    assert_eq!(joined.id(), "standalone-welcome");
    assert_eq!(joined.state(), ConversationState::Working);
    assert_eq!(joined.members().expect("members").len(), 2);
    let (epoch, _) = joined.epoch_and_retry().expect("epoch");
    assert_eq!(epoch, 1, "joiner lands on the post-add epoch");
}
