//! Step 1 proof: a `SessionRunner` can be built and queried straight from a
//! `ConversationDeps` bundle, with no `User` in the picture. The integrator
//! holds one plug-in factory + consensus context and constructs each
//! conversation from them — exactly what `User` does internally, here done
//! by hand.

mod common;

use std::str::FromStr;
use std::sync::Arc;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;

use de_mls::app::{ConsensusContext, ConversationDeps, ConversationState, SessionRunner};
use de_mls::core::{ScoringConfig, SessionEvent, StewardListConfig};
use de_mls::defaults::{
    DefaultConsensusPlugin, DefaultConversationPluginsFactory, MemoryDeMlsStorage,
};
use de_mls::ds::SharedDeliveryService;
use de_mls::mls_crypto::MlsCredentials;

use common::session_fixtures::CapturingTransport;
use common::wallet::WalletMemberId;

const ALICE: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

/// The shared, conversation-agnostic state an integrator keeps once: the
/// plug-in factory, the consensus context, the identity, and a transport.
struct Integrator {
    plugins: DefaultConversationPluginsFactory,
    consensus: ConsensusContext<DefaultConsensusPlugin>,
    member_id: WalletMemberId,
    transport: SharedDeliveryService,
}

impl Integrator {
    fn new() -> Self {
        let signer = PrivateKeySigner::from_str(ALICE).expect("valid private key");
        let member_id = WalletMemberId::from_address(signer.address());
        let credentials =
            Arc::new(MlsCredentials::from_member_id(&member_id).expect("credentials"));
        let storage = Arc::new(MemoryDeMlsStorage::new());
        let plugins = DefaultConversationPluginsFactory::new(storage, credentials);
        let consensus =
            ConsensusContext::<DefaultConsensusPlugin>::new(EthereumConsensusSigner::new(signer));
        let transport: SharedDeliveryService = CapturingTransport::new();
        Self {
            plugins,
            consensus,
            member_id,
            transport,
        }
    }

    /// Fresh per-conversation deps drawn from the shared state.
    fn deps(
        &self,
    ) -> ConversationDeps<'_, DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
        ConversationDeps {
            plugins: &self.plugins,
            consensus: &self.consensus,
            identity: &self.member_id,
            transport: Arc::clone(&self.transport),
            app_id: Arc::from(&[0u8; 16][..]),
            config: de_mls::app::ConversationConfig::default(),
            scoring_config: ScoringConfig::default(),
            steward_list_config: StewardListConfig::default(),
        }
    }
}

#[test]
fn create_builds_a_working_steward_session_without_user() {
    let integrator = Integrator::new();
    let runner = SessionRunner::create("standalone", integrator.deps()).expect("create");

    assert_eq!(runner.get_conversation_state(), ConversationState::Working);
    assert!(
        runner.is_steward_for_self(),
        "creator is the sole steward at epoch 0"
    );
    let (epoch, _retry) = runner.get_epoch_and_retry().expect("epoch");
    assert_eq!(epoch, 0);

    // The opening phase is buffered for whoever drains the session.
    let events = runner.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::PhaseChange(ConversationState::Working))),
        "create buffers an opening Working PhaseChange"
    );
}

#[test]
fn join_builds_a_pending_join_session_without_user() {
    let integrator = Integrator::new();
    let runner = SessionRunner::join("standalone", integrator.deps()).expect("join");

    assert_eq!(
        runner.get_conversation_state(),
        ConversationState::PendingJoin
    );
    assert!(
        !runner.is_steward_for_self(),
        "a pending joiner holds no steward list yet"
    );
}
