//! Step 1 proof: a `Conversation` can be built and queried straight from a
//! `ConversationDeps` bundle, with no `User` in the picture. The integrator
//! holds its credential + signer plus shared consensus storage + signer,
//! builds each conversation's plug-in instances and consensus service inline —
//! exactly what `User` does internally, here done by hand.

mod common;

use std::cell::RefCell;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;
use openmls::credentials::CredentialWithKey;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;

use de_mls::defaults::{DefaultConsensusPlugin, DefaultPeerScoring, DefaultStewardList};
use de_mls::mls_crypto::KeyPackageBytes;
use de_mls::{
    ConsensusPlugin, ConsensusServiceFor, ConversationEvent, ScoringConfig, StewardListConfig,
};
use de_mls::{Conversation, ConversationDeps, ConversationState};

use common::{
    TestMls, TestPlugins, creator_mls, make_scoring, make_steward, mint_key_package, open_welcome,
    test_credential, wallet::WalletMemberId,
};

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// The shared, conversation-agnostic state an integrator keeps once: its
/// credential + signer, the consensus storage + signer, and the identity. The
/// `RefCell` provider stash carries a minted key package's private keys until
/// the matching welcome arrives — `&self` call sites mint and open in turn.
struct Integrator {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    consensus_storage: <DefaultConsensusPlugin as ConsensusPlugin>::ConsensusStorage,
    consensus_signer: <DefaultConsensusPlugin as ConsensusPlugin>::Signer,
    member_id: WalletMemberId,
    pending_provider: RefCell<Option<OpenMlsRustCrypto>>,
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
            pending_provider: RefCell::new(None),
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

    /// Build the creator's MLS service from this integrator's credential.
    fn creator_mls(&self, conversation_id: &str) -> TestMls {
        creator_mls(
            conversation_id.to_string(),
            self.credential.clone(),
            &self.signer,
        )
        .expect("create mls")
    }

    /// Mint a single-use key package, stashing its provider for the welcome.
    fn mint_key_package(&self) -> KeyPackageBytes {
        let (kp, provider) = mint_key_package(&self.credential, &self.signer);
        *self.pending_provider.borrow_mut() = Some(provider);
        kp
    }

    /// Open a welcome with the stashed provider. A fresh provider (we never
    /// minted a KP) holds no matching key package → `Ok(None)`.
    fn open_welcome(&self, welcome_bytes: &[u8]) -> Option<TestMls> {
        let provider = self
            .pending_provider
            .borrow_mut()
            .take()
            .unwrap_or_default();
        open_welcome(welcome_bytes, provider).expect("open welcome")
    }

    /// Fresh per-conversation deps drawn from the shared state, around a
    /// pre-built MLS service. The consensus service clones the shared storage
    /// (scope-keyed) and gets its own private event bus. The member id doubles
    /// as the `app_id` so two integrators in one test don't echo-drop each
    /// other's packets.
    fn deps(&self, mls: TestMls) -> ConversationDeps<DefaultConsensusPlugin, TestPlugins> {
        self.deps_with_config(mls, de_mls::ConversationConfig::default())
    }

    fn deps_with_config(
        &self,
        mls: TestMls,
        config: de_mls::ConversationConfig,
    ) -> ConversationDeps<DefaultConsensusPlugin, TestPlugins> {
        let consensus = ConsensusServiceFor::<DefaultConsensusPlugin>::new_with_components(
            self.consensus_storage.clone(),
            DefaultConsensusPlugin::new_event_bus(),
            self.consensus_signer.clone(),
            10,
        );
        ConversationDeps {
            mls,
            scoring: self.scoring(),
            steward: self.steward(),
            consensus,
            app_id: Arc::from(self.member_id.member_id_bytes()),
            config,
        }
    }
}

#[test]
fn create_builds_a_working_steward_session_without_user() {
    let integrator = Integrator::new();
    let mls = integrator.creator_mls("standalone");
    let conversation = Conversation::create(
        "standalone",
        integrator.deps(mls),
        integrator.member_id.member_id_bytes(),
        integrator.member_id.member_id_display(),
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

    let alice_mls = alice.creator_mls("standalone-welcome");
    let mut creator = Conversation::create(
        "standalone-welcome",
        alice.deps_with_config(alice_mls, fast_config()),
        alice.member_id.member_id_bytes(),
        alice.member_id.member_id_display(),
    )
    .expect("create");

    // Bob mints a key package out of band; Alice — the sole member — proposes
    // the add, so her bundled YES resolves consensus on its own.
    let bob_kp = bob.mint_key_package();
    creator
        .add_member(bob_kp.as_bytes(), &alice.signer)
        .expect("add member");

    // Drive the creator until the welcome is minted.
    let mut welcome = None;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(30));
        creator.poll(&alice.signer);
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

    // A welcome not addressed to us yields `None` at open time — a fresh
    // integrator that never minted the key package can't open it.
    let bystander = Integrator::with_key(ALICE);
    assert!(
        bystander.open_welcome(&welcome.welcome_bytes).is_none(),
        "a welcome for someone else is ignored"
    );

    // The integrator opens the welcome, then `join` completes the whole joiner
    // path in one call: run the join side-effects, apply the bundled sync.
    let bob_mls = bob
        .open_welcome(&welcome.welcome_bytes)
        .expect("welcome addresses bob");
    let joined = Conversation::join(
        bob.deps_with_config(bob_mls, fast_config()),
        &welcome.conversation_sync_bytes,
        bob.member_id.member_id_bytes(),
        bob.member_id.member_id_display(),
        &bob.signer,
    )
    .expect("join");
    assert_eq!(joined.id(), "standalone-welcome");
    assert_eq!(joined.state(), ConversationState::Working);
    assert_eq!(joined.members().expect("members").len(), 2);
    let (epoch, _) = joined.epoch_and_retry().expect("epoch");
    assert_eq!(epoch, 1, "joiner lands on the post-add epoch");
}
