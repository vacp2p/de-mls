//! Test-side wallet identity adapter.
//!
//! Bridges an Ethereum `PrivateKeySigner` to the library's
//! identity-agnostic interfaces: builds a [`WalletMemberId`] that
//! implements [`de_mls::member_id::MemberId`] and constructs a [`User`]
//! with the default plug-in bundle wired to an
//! [`EthereumConsensusSigner`]. Production callers ship their own
//! identity adapter — this lives under `tests/common/` only because the
//! integration suite uses Ethereum keys for convenience.

use std::str::FromStr;

use alloy::signers::local::PrivateKeySigner;
use de_mls_gateway::WalletMemberId;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;

use de_mls::defaults::DefaultConsensusPlugin;
use de_mls::{ConversationConfig, ScoringConfig, StewardListConfig};
use de_mls_ds::SharedDeliveryService;
use de_mls_gateway::mls::{DefaultConversationPluginsFactory, build_credential};
use de_mls_gateway::user::ConsensusContext;
use de_mls_gateway::user::{User, UserPlugins};
use openmls_basic_credential::SignatureKeyPair;

/// Build a [`User`] keyed by an Ethereum private-key string. Uses the
/// default plug-in bundle (in-memory MLS storage, default scoring +
/// steward-list backends, [`EthereumConsensusSigner`] wrapping the parsed
/// `PrivateKeySigner`).
pub fn user_from_private_key(
    private_key: &str,
    transport: SharedDeliveryService,
    cfg: ConversationConfig,
) -> User<DefaultConsensusPlugin, SignatureKeyPair> {
    let eth_signer = PrivateKeySigner::from_str(private_key).expect("valid private key");
    let member_id = WalletMemberId::from_address(eth_signer.address());

    let (credential, mls_signer) =
        build_credential(member_id.member_id_bytes()).expect("credentials");
    let conversation_plugins =
        DefaultConversationPluginsFactory::new(credential, mls_signer.clone());

    let consensus_signer = EthereumConsensusSigner::new(eth_signer);
    let consensus = ConsensusContext::<DefaultConsensusPlugin>::new(consensus_signer);

    let plugins = UserPlugins {
        conversation_plugins,
        consensus,
        default_conversation_config: cfg,
        default_scoring_config: ScoringConfig::default(),
        default_steward_list_config: StewardListConfig::default(),
    };

    User::new_with_plugins(member_id, mls_signer, plugins, transport)
}
