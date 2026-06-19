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
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;

use de_mls::core::{ScoringConfig, StewardListConfig};
use de_mls::defaults::{
    DefaultConsensusPlugin, DefaultConversationPluginsFactory, MemoryDeMlsStorage,
};
use de_mls::member_id::MemberId;
use de_mls::mls_crypto::MlsCredentials;
use de_mls::session::ConversationConfig;
use de_mls_ds::SharedDeliveryService;
use de_mls_gateway::user::ConsensusContext;
use de_mls_gateway::user::{User, UserPlugins};

/// Wallet-flavoured [`Identity`] used by integration tests. Holds the
/// 20-byte Ethereum address bytes and its EIP-55 checksummed hex form.
#[derive(Debug, Clone)]
pub struct WalletMemberId {
    bytes: Vec<u8>,
    display: String,
}

impl WalletMemberId {
    /// Build from a parsed [`Address`].
    pub fn from_address(addr: Address) -> Self {
        Self {
            bytes: addr.as_slice().to_vec(),
            display: addr.to_checksum(None),
        }
    }

    /// Parse a `0x…`-prefixed wallet hex string.
    pub fn from_hex(hex: &str) -> Self {
        let addr = Address::from_str(hex.trim()).expect("valid wallet hex");
        Self::from_address(addr)
    }
}

impl MemberId for WalletMemberId {
    fn member_id_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn member_id_display(&self) -> &str {
        &self.display
    }
}

/// Build a [`User`] keyed by an Ethereum private-key string. Uses the
/// default plug-in bundle (in-memory MLS storage, default scoring +
/// steward-list backends, [`EthereumConsensusSigner`] wrapping the parsed
/// `PrivateKeySigner`).
pub fn user_from_private_key(
    private_key: &str,
    transport: SharedDeliveryService,
    cfg: ConversationConfig,
) -> User<DefaultConsensusPlugin, DefaultConversationPluginsFactory> {
    let signer = PrivateKeySigner::from_str(private_key).expect("valid private key");
    let member_id = WalletMemberId::from_address(signer.address());

    let credentials = Arc::new(MlsCredentials::from_member_id(&member_id).expect("credentials"));
    let storage = Arc::new(MemoryDeMlsStorage::new());
    let conversation_plugins = DefaultConversationPluginsFactory::new(storage, credentials);

    let consensus_signer = EthereumConsensusSigner::new(signer);
    let consensus = ConsensusContext::<DefaultConsensusPlugin>::new(consensus_signer);

    let plugins = UserPlugins {
        conversation_plugins,
        consensus,
        default_conversation_config: cfg,
        default_scoring_config: ScoringConfig::default(),
        default_steward_list_config: StewardListConfig::default(),
    };

    User::new_with_plugins(Box::new(member_id), plugins, transport)
}
