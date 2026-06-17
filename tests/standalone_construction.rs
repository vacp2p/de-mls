//! Step 1 proof: a `Conversation` can be built and queried straight from a
//! `ConversationDeps` bundle, with no `User` in the picture. The integrator
//! holds one plug-in factory plus shared consensus storage + signer, and
//! mints each conversation's consensus service from them — exactly what
//! `User` does internally, here done by hand.

mod common;

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;

use de_mls::member_id::MemberId;

use de_mls::core::{
    ConsensusPlugin, ConsensusServiceFor, ConversationEvent, ScoringConfig, StewardListConfig,
};
use de_mls::defaults::{
    DefaultConsensusPlugin, DefaultConversationPluginsFactory, MemoryDeMlsStorage,
};
use de_mls::mls_crypto::MlsCredentials;
use de_mls::session::{Conversation, ConversationDeps, ConversationState};

use common::wallet::WalletMemberId;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const BOB: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

/// The shared, conversation-agnostic state an integrator keeps once: the
/// plug-in factory, the consensus storage + signer, and the identity.
struct Integrator {
    plugins: DefaultConversationPluginsFactory,
    consensus_storage: <DefaultConsensusPlugin as ConsensusPlugin>::ConsensusStorage,
    consensus_signer: <DefaultConsensusPlugin as ConsensusPlugin>::Signer,
    member_id: WalletMemberId,
}

impl Integrator {
    fn new() -> Self {
        Self::with_key(ALICE)
    }

    fn with_key(private_key: &str) -> Self {
        let signer = PrivateKeySigner::from_str(private_key).expect("valid private key");
        let member_id = WalletMemberId::from_address(signer.address());
        let credentials =
            Arc::new(MlsCredentials::from_member_id(&member_id).expect("credentials"));
        let storage = Arc::new(MemoryDeMlsStorage::new());
        let plugins = DefaultConversationPluginsFactory::new(storage, credentials);
        Self {
            plugins,
            consensus_storage: DefaultConsensusPlugin::new_storage(),
            consensus_signer: EthereumConsensusSigner::new(signer),
            member_id,
        }
    }

    /// Fresh per-conversation deps drawn from the shared state. The
    /// consensus service clones the shared storage (scope-keyed) and gets
    /// its own private event bus. The member id doubles as the `app_id` so
    /// two integrators in one test don't echo-drop each other's packets.
    fn deps(
        &self,
    ) -> ConversationDeps<'_, DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
        self.deps_with_config(de_mls::session::ConversationConfig::default())
    }

    fn deps_with_config(
        &self,
        config: de_mls::session::ConversationConfig,
    ) -> ConversationDeps<'_, DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
        let consensus = ConsensusServiceFor::<DefaultConsensusPlugin>::new_with_components(
            self.consensus_storage.clone(),
            DefaultConsensusPlugin::new_event_bus(),
            self.consensus_signer.clone(),
            10,
        );
        ConversationDeps {
            plugins: &self.plugins,
            consensus,
            identity: &self.member_id,
            app_id: Arc::from(self.member_id.member_id_bytes()),
            config,
            scoring_config: ScoringConfig::default(),
            steward_list_config: StewardListConfig::default(),
        }
    }
}

#[test]
fn create_builds_a_working_steward_session_without_user() {
    let integrator = Integrator::new();
    let conversation = Conversation::create("standalone", integrator.deps()).expect("create");

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

#[test]
fn join_builds_a_pending_join_session_without_user() {
    let integrator = Integrator::new();
    let conversation = Conversation::join("standalone", integrator.deps()).expect("join");

    assert_eq!(conversation.state(), ConversationState::PendingJoin);
    assert!(
        !conversation.is_steward(),
        "a pending joiner holds no steward list yet"
    );
}

/// Sub-second timers so the solo creator's bundled-YES consensus and the
/// inactivity commit land within a few polling rounds.
fn fast_config() -> de_mls::session::ConversationConfig {
    de_mls::session::ConversationConfig {
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
fn from_welcome_joins_in_one_call() {
    let alice = Integrator::new();
    let bob = Integrator::with_key(BOB);

    let mut creator =
        Conversation::create("standalone-welcome", alice.deps_with_config(fast_config()))
            .expect("create");

    // Bob mints a key package out of band; Alice — the sole member — proposes
    // the add, so her bundled YES resolves consensus on its own.
    let bob_kp = bob.plugins.generate_key_package().expect("kp");
    creator.add_member(bob_kp.as_bytes()).expect("add member");

    // Drive the creator until the welcome is minted.
    let mut welcome = None;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(30));
        creator.poll();
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

    // A welcome not addressed to us yields `None` — a fresh integrator that
    // never minted the key package can't open it.
    let bystander = Integrator::with_key(ALICE);
    assert!(
        Conversation::from_welcome(bystander.deps_with_config(fast_config()), &welcome)
            .expect("from_welcome on a foreign welcome")
            .is_none(),
        "a welcome for someone else is ignored"
    );

    // The whole joiner path in one call: attach MLS, complete the join,
    // apply the bundled sync.
    let joined = Conversation::from_welcome(bob.deps_with_config(fast_config()), &welcome)
        .expect("from_welcome")
        .expect("welcome addresses bob");
    assert_eq!(joined.id(), "standalone-welcome");
    assert_eq!(joined.state(), ConversationState::Working);
    assert_eq!(joined.members().expect("members").len(), 2);
    let (epoch, _) = joined.epoch_and_retry().expect("epoch");
    assert_eq!(epoch, 1, "joiner lands on the post-add epoch");
}
