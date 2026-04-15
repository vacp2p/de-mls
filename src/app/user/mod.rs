//! User struct for managing multiple groups.
//!
//! This is the main entry point for the application layer,
//! managing multiple `Group`s and coordinating operations.

mod consensus;
mod freeze;
mod groups;
mod inbound;
mod messaging;
mod steward;

use alloy::signers::local::PrivateKeySigner;
use std::sync::Mutex;
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::sync::RwLock;
use tracing::{error, info};

use hashgraph_like_consensus::{
    api::ConsensusServiceAPI, service::DefaultConsensusService, types::ConsensusEvent,
};

use crate::app::config::GroupConfig;
use crate::app::consensus_bridge::{forward_incoming_proposal, forward_incoming_vote};
use crate::app::display::MemberRole;
use crate::app::error::UserError;
use crate::app::peer_scoring::{
    FixedScoringProvider, InMemoryPeerScoreStorage, PeerScoringService,
};
use crate::app::state_machine::{
    FreezeTimeoutStatus, GroupState, GroupStateMachine, StateChangeHandler,
};
use crate::core::{
    self, DeMlsProvider, DefaultProvider, FreezeFinalizeResult, Group, GroupEventHandler,
    ProcessResult, ProtocolConfig, ScoreEvent, ScoringConfig, StewardList, create_commit_candidate,
};
use crate::ds::InboundPacket;
use crate::mls_crypto::{
    MemoryDeMlsStorage, MlsService, format_wallet_address, parse_wallet_to_bytes,
};
use prost::Message;

use crate::protos::de_mls::messages::v1::{
    AppMessage, BanRequest, ConversationMessage, GroupUpdateRequest, RemoveMember,
    StewardElectionProposal, StewardListSync, ViolationEvidence, group_update_request,
};

/// Internal state for a group managed by User.
struct GroupEntry {
    group: Group,
    state_machine: GroupStateMachine,
}

/// User manages multiple MLS groups.
pub struct User<P: DeMlsProvider, H: GroupEventHandler, SCH: StateChangeHandler> {
    mls_service: MlsService<P::Storage>,
    groups: Arc<RwLock<HashMap<String, GroupEntry>>>,
    consensus_service: Arc<P::Consensus>,
    eth_signer: PrivateKeySigner,
    handler: Arc<H>,
    state_handler: Arc<SCH>,
    default_group_config: GroupConfig,
    /// Per-instance UUID for echo-dedup on pub/sub networks.
    /// Embedded in all outbound packets — inbound packets with matching
    /// app_id are self-echoes and get silently dropped.
    app_id: Vec<u8>,
    scoring_service: Mutex<PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider>>,
}

impl<P: DeMlsProvider, H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<P, H, SCH>
{
    fn new_with_config(
        mls_service: MlsService<P::Storage>,
        consensus_service: Arc<P::Consensus>,
        eth_signer: PrivateKeySigner,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
        default_group_config: GroupConfig,
    ) -> Self {
        Self {
            mls_service,
            groups: Arc::new(RwLock::new(HashMap::new())),
            consensus_service,
            eth_signer,
            handler,
            state_handler,
            default_group_config,
            scoring_service: Mutex::new(PeerScoringService::new(
                InMemoryPeerScoreStorage::new(),
                FixedScoringProvider::new(Self::default_score_deltas()),
                ScoringConfig {
                    default_score: 100,
                    removal_threshold: 0,
                },
            )),
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
        }
    }

    /// Default score deltas for all events.
    fn default_score_deltas() -> HashMap<ScoreEvent, i64> {
        HashMap::from([
            // ECP target penalties (violation-type-specific)
            (ScoreEvent::BrokenCommit, -50),
            (ScoreEvent::BrokenMlsProposal, -30),
            (ScoreEvent::CensorshipInactivity, -40),
            // ECP creator outcomes
            (ScoreEvent::EmergencyYesCreator, 20),
            (ScoreEvent::EmergencyNoCreator, -50),
            // Commit selection (M2)
            (ScoreEvent::SuccessfulCommit, 10),
            // Commit validation (M5)
            (ScoreEvent::NonFinalizedProposalCommit, -30),
        ])
    }

    /// Lock the scoring service, recovering from a poisoned mutex.
    fn scoring(
        &self,
    ) -> std::sync::MutexGuard<'_, PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider>>
    {
        self.scoring_service
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Get the user's identity string (wallet address as checksummed hex).
    pub fn identity_string(&self) -> String {
        self.mls_service.wallet_hex()
    }
}

// ─────────────────────────── DefaultProvider Convenience ───────────────────────────

impl<H: GroupEventHandler + 'static, SCH: StateChangeHandler + 'static>
    User<DefaultProvider, H, SCH>
{
    /// Convenience constructor for the default provider with default group config.
    pub fn with_private_key(
        private_key: &str,
        consensus_service: Arc<DefaultConsensusService>,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
    ) -> Result<Self, UserError> {
        Self::with_private_key_and_config(
            private_key,
            consensus_service,
            handler,
            state_handler,
            GroupConfig::default(),
        )
    }

    /// Convenience constructor for the default provider with custom group config.
    pub fn with_private_key_and_config(
        private_key: &str,
        consensus_service: Arc<DefaultConsensusService>,
        handler: Arc<H>,
        state_handler: Arc<SCH>,
        default_group_config: GroupConfig,
    ) -> Result<Self, UserError> {
        let signer = PrivateKeySigner::from_str(private_key)?;
        let user_address = signer.address();

        let mls_service = MlsService::new(MemoryDeMlsStorage::new());
        mls_service
            .init(user_address)
            .map_err(|e| UserError::Core(e.into()))?;

        Ok(Self::new_with_config(
            mls_service,
            consensus_service,
            signer,
            handler,
            state_handler,
            default_group_config,
        ))
    }
}
