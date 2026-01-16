pub mod consensus;
pub mod groups;
pub mod messaging;
pub mod proposals;
pub mod steward;
pub mod waku;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::{service::DefaultConsensusService, types::ConsensusEvent};
use kameo::Actor;
use std::{collections::HashMap, fmt::Display, str::FromStr, sync::Arc};
use tokio::sync::{broadcast, RwLock};

use ds::transport::OutboundPacket;
use mls_crypto::{IdentityService, OpenMlsIdentityService};

use crate::{
    error::UserError, group::Group, protos::de_mls::messages::v1::AppMessage,
    user::proposals::PendingBatches,
};

/// Represents the action to take after processing a user message or event.
///
/// This enum defines the possible outcomes when processing user-related operations,
/// allowing the caller to determine the appropriate next steps for message handling,
/// group management, and network communication.
#[derive(Debug, Clone, PartialEq)]
pub enum UserAction {
    Outbound(OutboundPacket),
    SendToApp(AppMessage),
    LeaveGroup(String),
    DoNothing,
}

impl Display for UserAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserAction::Outbound(_) => write!(f, "Outbound"),
            UserAction::SendToApp(_) => write!(f, "SendToApp"),
            UserAction::LeaveGroup(group_name) => write!(f, "LeaveGroup({group_name})"),
            UserAction::DoNothing => write!(f, "DoNothing"),
        }
    }
}

/// Represents a user in the MLS-based messaging system.
///
/// The User struct manages the lifecycle of multiple groups, handles consensus operations,
/// and coordinates communication between the application layer and the Waku network.
/// It integrates with the consensus service for proposal management and voting.
///
/// ## Key Features:
/// - Multi-group management and coordination
/// - Consensus service integration for proposal handling
/// - Waku message processing and routing
/// - Steward epoch coordination
/// - Member management through proposals
#[derive(Actor)]
pub struct User {
    identity_service: OpenMlsIdentityService,
    // Each group has its own lock for better concurrency
    groups: Arc<RwLock<HashMap<String, Arc<RwLock<Group>>>>>,
    consensus_service: Arc<DefaultConsensusService>,
    eth_signer: PrivateKeySigner,
    // Queue for batch proposals that arrive before consensus is reached
    pending_batch_proposals: PendingBatches,
}

impl User {
    /// Create a new user instance with the specified Ethereum private key.
    ///
    /// ## Parameters:
    /// - `user_eth_priv_key`: The user's Ethereum private key as a hex string
    ///
    /// ## Returns:
    /// - New User instance with initialized identity and services
    ///
    /// ## Errors:
    /// - `UserError` if private key parsing or identity creation fails
    pub fn new(
        user_eth_priv_key: &str,
        consensus_service: Arc<DefaultConsensusService>,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(user_eth_priv_key)?;
        let user_address = signer.address();
        let identity_service = OpenMlsIdentityService::new(user_address.as_slice())?;
        let user = User {
            groups: Arc::new(RwLock::new(HashMap::new())),
            identity_service,
            eth_signer: signer,
            consensus_service,
            pending_batch_proposals: PendingBatches::default(),
        };
        Ok(user)
    }

    /// Get a subscription to consensus events
    pub fn subscribe_to_consensus_events(&self) -> broadcast::Receiver<(String, ConsensusEvent)> {
        self.consensus_service.subscribe_to_events()
    }

    /// Get the identity string for debugging and identification purposes.
    ///
    /// ## Returns:
    /// - String representation of the user's identity
    ///
    /// ## Usage:
    /// Primarily used for debugging, logging, and user identification
    pub fn identity_string(&self) -> String {
        self.identity_service.identity_string()
    }
}
