//! Step 1 proof: a `Conversation` can be built and queried straight from
//! direct arguments, with no `User` in the picture. The integrator holds its
//! credential + signer plus the consensus backend, and builds each
//! conversation's plug-in instances inline — exactly what `User` does
//! internally, here done by hand.

mod common;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;
use openmls::credentials::CredentialWithKey;
use openmls::prelude::OpenMlsProvider;
use openmls_basic_credential::SignatureKeyPair;

use de_mls::defaults::{DefaultConsensusPlugin, DefaultPeerScoring, InMemoryPeerScoreStorage};
use de_mls::{Conversation, ConversationState, MockClock};
use de_mls::{ConversationEvent, ScoringConfig, StewardListConfig};

use common::{
    MintedKeyPackage, TestProvider, make_scoring, mint_key_package, test_credential,
    wallet::WalletIdentity,
};

use crate::common::harness::fast_config;
use crate::common::test_mls_group_config;

/// Per-conversation stack the standalone tests build, on virtual time.
type TestConversation = Conversation<DefaultConsensusPlugin, InMemoryPeerScoreStorage, MockClock>;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const CHARLIE: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const DAVE: &str = "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6";

/// The shared, conversation-agnostic state an integrator keeps once: its
/// credential + signer, the consensus backend, the identity, and one OpenMLS
/// provider reused across every call. The creator seeds its group into it; a
/// joiner mints its key package into it and later opens the welcome from it.
struct Integrator {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    consensus: DefaultConsensusPlugin,
    wallet: WalletIdentity,
    provider: TestProvider,
    /// The virtual clock this integrator drives; conversations own clones.
    clock: MockClock,
}

impl Integrator {
    fn new() -> Self {
        Self::with_key(ALICE)
    }

    fn with_key(private_key: &str) -> Self {
        let eth_signer = PrivateKeySigner::from_str(private_key).expect("valid private key");
        let wallet = WalletIdentity::from_address(eth_signer.address());
        let (credential, signer) = test_credential(wallet.address_bytes());
        Self {
            credential,
            signer,
            consensus: DefaultConsensusPlugin::new(EthereumConsensusSigner::new(eth_signer)),
            wallet,
            provider: TestProvider::default(),
            clock: MockClock::new(),
        }
    }

    fn scoring(&self) -> DefaultPeerScoring {
        make_scoring(&ScoringConfig::default())
    }

    /// Mint a single-use key package into this integrator's reused provider,
    /// which thereby holds the private keys for the matching welcome.
    fn mint_key_package(&self) -> MintedKeyPackage {
        mint_key_package(&self.provider, &self.credential, &self.signer)
    }

    /// This integrator's `app_id` — the member id doubles as it so two
    /// integrators in one test don't echo-drop each other's packets.
    fn app_id(&self) -> Arc<[u8]> {
        Arc::from(self.wallet.address_bytes())
    }
}

#[test]
fn create_builds_a_working_steward_session_without_user() {
    let integrator = Integrator::new();
    let conversation: TestConversation = Conversation::create(
        "standalone",
        &integrator.provider,
        integrator.credential.clone(),
        &test_mls_group_config(),
        &integrator.signer,
        &integrator.consensus,
        integrator.scoring(),
        integrator.clock.clone(),
        integrator.app_id(),
        de_mls::ConversationConfig::default(),
        &[],
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

/// Read access reaches OpenMLS's own API: values de-mls surfaces no accessor
/// for, and a secret exporter that takes the provider the integrator already
/// holds.
#[test]
fn mls_group_is_readable_through_openmls() {
    let integrator = Integrator::new();
    let conversation: TestConversation = Conversation::create(
        "standalone-read",
        &integrator.provider,
        integrator.credential.clone(),
        &test_mls_group_config(),
        &integrator.signer,
        &integrator.consensus,
        integrator.scoring(),
        integrator.clock.clone(),
        integrator.app_id(),
        de_mls::ConversationConfig::default(),
        &[],
    )
    .expect("create");

    let group = conversation.mls_group();

    // Read straight through OpenMLS, with no de-mls accessor behind them.
    assert_eq!(group.group_id().as_slice(), b"standalone-read");
    assert_eq!(group.own_leaf_index().u32(), 0);
    assert_eq!(group.members().count(), 1);

    // The tree agrees with what de-mls reports.
    let (epoch, _retry) = conversation.epoch_and_retry().expect("epoch");
    assert_eq!(group.epoch().as_u64(), epoch);

    // A secret exporter runs off the crypto provider the integrator already holds.
    let exported = group
        .export_secret(
            integrator.provider.crypto(),
            "de-mls-read-test",
            b"context",
            32,
        )
        .expect("export_secret");
    assert_eq!(exported.len(), 32);
}

/// Sub-second timers so the solo creator's bundled-YES consensus and the
/// inactivity commit land within a few polling rounds.

#[test]
fn join_completes_in_one_call() {
    let alice = Integrator::new();
    let bob = Integrator::with_key(BOB);

    let mut creator: TestConversation = Conversation::create(
        "standalone-welcome",
        &alice.provider,
        alice.credential.clone(),
        &test_mls_group_config(),
        &alice.signer,
        &alice.consensus,
        alice.scoring(),
        alice.clock.clone(),
        alice.app_id(),
        fast_config(),
        &[],
    )
    .expect("create");

    // Bob mints a key package out of band; Alice — the sole member — proposes
    // the add, so her bundled YES resolves consensus on its own.
    let bob_kp = bob.mint_key_package();
    creator
        .add_member(&alice.provider, &alice.signer, bob_kp.as_bytes())
        .expect("add member");

    // Drive the creator until the welcome is minted.
    let mut welcome = None;
    for _ in 0..40 {
        // Advance both integrators' clocks in lockstep so bob's later join
        // starts from the same virtual time alice's proposal was stamped at.
        alice.clock.advance(Duration::from_millis(30));
        bob.clock.advance(Duration::from_millis(30));
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
        &bystander.signer,
        &welcome.welcome_bytes,
        &welcome.conversation_sync_bytes,
        &bystander.consensus,
        bystander.scoring(),
        bystander.clock.clone(),
        bystander.app_id(),
        fast_config(),
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
        &bob.signer,
        &welcome.welcome_bytes,
        &welcome.conversation_sync_bytes,
        &bob.consensus,
        bob.scoring(),
        bob.clock.clone(),
        bob.app_id(),
        fast_config(),
    )
    .expect("join")
    .expect("welcome addresses bob");
    assert_eq!(joined.id(), "standalone-welcome");
    assert_eq!(joined.state(), ConversationState::Working);
    assert_eq!(joined.mls_group().members().count(), 2);
    let (epoch, _) = joined.epoch_and_retry().expect("epoch");
    assert_eq!(epoch, 1, "joiner lands on the post-add epoch");
}

#[test]
fn create_with_members_seeds_initial_members_at_genesis() {
    let alice = Integrator::new();
    let bob = Integrator::with_key(BOB);
    let charlie = Integrator::with_key(CHARLIE);

    // The initial members mint their key packages into their own providers and
    // hand the bytes to the creator.
    let bob_kp = bob.mint_key_package();
    let charlie_kp = charlie.mint_key_package();

    let config = de_mls::ConversationConfig {
        steward_list: StewardListConfig::new(1, 5).unwrap(),
        ..fast_config()
    };

    let creator: TestConversation = Conversation::create(
        "genesis",
        &alice.provider,
        alice.credential.clone(),
        &test_mls_group_config(),
        &alice.signer,
        &alice.consensus,
        alice.scoring(),
        alice.clock.clone(),
        alice.app_id(),
        config.clone(),
        &[bob_kp.as_bytes(), charlie_kp.as_bytes()],
    )
    .expect("create with members");

    // Genesis is immediate: at epoch 1 with all three members, no polling.
    let (epoch, _) = creator.epoch_and_retry().expect("epoch");
    assert_eq!(epoch, 1, "the genesis commit landed at creation");
    assert_eq!(creator.mls_group().members().count(), 3);
    assert_eq!(creator.state(), ConversationState::Working);
    assert!(
        creator.is_steward(),
        "the creator stewards the genesis epoch"
    );

    // Exactly one welcome, addressing both initial members.
    let welcome = creator
        .drain_events()
        .into_iter()
        .find_map(|e| match e {
            ConversationEvent::WelcomeReady {
                welcome,
                minted_locally: true,
            } => Some(welcome),
            _ => None,
        })
        .expect("genesis mints one welcome");
    assert_eq!(
        welcome.joiner_identities.len(),
        2,
        "one welcome admits both initial members"
    );

    // Each initial member opens the single welcome and lands at epoch 1, Working,
    // and a steward — the property genesis beats the incremental baseline on.
    for member in [&bob, &charlie] {
        let joined: TestConversation = Conversation::join(
            &member.provider,
            &member.signer,
            &welcome.welcome_bytes,
            &welcome.conversation_sync_bytes,
            &member.consensus,
            member.scoring(),
            member.clock.clone(),
            member.app_id(),
            config.clone(),
        )
        .expect("join")
        .expect("welcome addresses the member");
        assert_eq!(joined.id(), "genesis");
        assert_eq!(joined.state(), ConversationState::Working);
        assert_eq!(joined.mls_group().members().count(), 3);
        let (e, _) = joined.epoch_and_retry().expect("epoch");
        assert_eq!(e, 1, "member lands on the genesis epoch");
        assert!(joined.is_steward(), "an initial member stewards genesis");
    }
}

#[test]
fn create_with_members_founds_a_subset_steward_group() {
    let alice = Integrator::new();
    let bob = Integrator::with_key(BOB);
    let charlie = Integrator::with_key(CHARLIE);
    let dave = Integrator::with_key(DAVE);

    let bob_kp = bob.mint_key_package();
    let charlie_kp = charlie.mint_key_package();
    let dave_kp = dave.mint_key_package();

    // sn_max = 2 with 4 members → the steward list is a genuine 2-of-4 subset.
    let config = de_mls::ConversationConfig {
        steward_list: StewardListConfig::new(1, 2).unwrap(),
        ..fast_config()
    };

    let creator: TestConversation = Conversation::create(
        "genesis-subset",
        &alice.provider,
        alice.credential.clone(),
        &test_mls_group_config(),
        &alice.signer,
        &alice.consensus,
        alice.scoring(),
        alice.clock.clone(),
        alice.app_id(),
        config.clone(),
        &[bob_kp.as_bytes(), charlie_kp.as_bytes(), dave_kp.as_bytes()],
    )
    .expect("create with members");

    let (epoch, _) = creator.epoch_and_retry().expect("epoch");
    assert_eq!(epoch, 1);
    assert_eq!(creator.mls_group().members().count(), 4);

    let welcome = creator
        .drain_events()
        .into_iter()
        .find_map(|e| match e {
            ConversationEvent::WelcomeReady {
                welcome,
                minted_locally: true,
            } => Some(welcome),
            _ => None,
        })
        .expect("genesis mints one welcome");
    assert_eq!(welcome.joiner_identities.len(), 3);

    let mut convos = vec![creator];
    for member in [&bob, &charlie, &dave] {
        let joined: TestConversation = Conversation::join(
            &member.provider,
            &member.signer,
            &welcome.welcome_bytes,
            &welcome.conversation_sync_bytes,
            &member.consensus,
            member.scoring(),
            member.clock.clone(),
            member.app_id(),
            config.clone(),
        )
        .expect("join")
        .expect("welcome addresses the member");
        assert_eq!(joined.mls_group().members().count(), 4);
        convos.push(joined);
    }

    // Exactly two of the four members steward — the deterministic 2-of-4 subset
    // the genesis install picked, seen consistently through the shared sync.
    let steward_count = convos.iter().filter(|c| c.is_steward()).count();
    assert_eq!(
        steward_count, 2,
        "sn_max = 2 founds a 2-member steward subset"
    );
}
