//! Step 1 proof: a `Conversation` can be built and queried straight from a
//! `ConversationDeps` bundle, with no `User` in the picture. The integrator
//! holds one plug-in factory plus shared consensus storage + signer, and
//! mints each conversation's consensus service from them — exactly what
//! `User` does internally, here done by hand.

mod common;

use std::str::FromStr;
use std::sync::Arc;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;

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
        let signer = PrivateKeySigner::from_str(ALICE).expect("valid private key");
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
    /// its own private event bus.
    fn deps(
        &self,
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
            app_id: Arc::from(&[0u8; 16][..]),
            config: de_mls::session::ConversationConfig::default(),
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
