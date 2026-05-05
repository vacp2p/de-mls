//! Shared fixtures for integration tests.
//!
//! Rust compiles each file in `tests/` as its own binary; a file under
//! `tests/common/` is *not* a test binary, so this module can be reused by
//! adding `mod common;` to any test file. Helpers carry `#[allow(dead_code)]`
//! at the module level because not every binary exercises every helper.
#![allow(dead_code)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, Ordering},
};

use async_trait::async_trait;

use de_mls::app::{FixedScoringProvider, InMemoryPeerScoreStorage, PeerScoringService};
use de_mls::core::{
    CallbackError, FreezeOutcome, Group, GroupEventHandler, ProcessResult, ProtocolConfig,
    ScoringConfig, build_key_package_message, create_commit_candidate, create_group,
    finalize_freeze_round, prepare_to_join, process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::mls_crypto::{MemoryDeMlsStorage, MlsService, OpenMlsService, parse_wallet_address};
use de_mls::protos::de_mls::messages::v1::AppMessage;

pub const DEFAULT_SCORE: i64 = 100;

// ─────────────────────────── Mock handler ───────────────────────────

#[derive(Debug, Clone)]
pub enum Event {
    Outbound {
        group: String,
        packet: OutboundPacket,
    },
    AppMessage {
        group: String,
        msg: AppMessage,
    },
    LeaveGroup {
        group: String,
    },
    JoinedGroup {
        group: String,
    },
    Error {
        group: String,
        op: String,
        err: String,
    },
}

#[derive(Clone, Default)]
pub struct MockHandler {
    pub events: Arc<Mutex<Vec<Event>>>,
}

impl MockHandler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl GroupEventHandler for MockHandler {
    async fn on_outbound(
        &self,
        group_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CallbackError> {
        self.events.lock().unwrap().push(Event::Outbound {
            group: group_name.to_string(),
            packet,
        });
        Ok("mock-id".to_string())
    }

    async fn on_app_message(
        &self,
        group_name: &str,
        message: AppMessage,
    ) -> Result<(), CallbackError> {
        self.events.lock().unwrap().push(Event::AppMessage {
            group: group_name.to_string(),
            msg: message,
        });
        Ok(())
    }

    async fn on_leave_group(&self, group_name: &str) -> Result<(), CallbackError> {
        self.events.lock().unwrap().push(Event::LeaveGroup {
            group: group_name.to_string(),
        });
        Ok(())
    }

    async fn on_joined_group(&self, group_name: &str) -> Result<(), CallbackError> {
        self.events.lock().unwrap().push(Event::JoinedGroup {
            group: group_name.to_string(),
        });
        Ok(())
    }

    async fn on_error(&self, group_name: &str, operation: &str, error: &str) {
        self.events.lock().unwrap().push(Event::Error {
            group: group_name.to_string(),
            op: operation.to_string(),
            err: error.to_string(),
        });
    }
}

// ─────────────────────────── MLS + group setup ───────────────────────────

pub fn default_steward_config() -> ProtocolConfig {
    ProtocolConfig::new(1, 5).unwrap()
}

pub fn setup_mls(wallet_hex: &str) -> OpenMlsService<MemoryDeMlsStorage> {
    let storage = MemoryDeMlsStorage::new();
    let mls = OpenMlsService::new(storage);
    let wallet = parse_wallet_address(wallet_hex).unwrap();
    mls.init(wallet).unwrap();
    mls
}

pub fn setup_steward(
    group_name: &str,
    wallet_hex: &str,
) -> (OpenMlsService<MemoryDeMlsStorage>, Group) {
    setup_steward_with_config(group_name, wallet_hex, default_steward_config())
}

pub fn setup_steward_with_config(
    group_name: &str,
    wallet_hex: &str,
    config: ProtocolConfig,
) -> (OpenMlsService<MemoryDeMlsStorage>, Group) {
    let mls = setup_mls(wallet_hex);
    let group = create_group(group_name, &mls, config).unwrap();
    (mls, group)
}

pub fn setup_joiner(
    group_name: &str,
    wallet_hex: &str,
) -> (OpenMlsService<MemoryDeMlsStorage>, Group, OutboundPacket) {
    setup_joiner_with_config(group_name, wallet_hex, default_steward_config())
}

pub fn setup_joiner_with_config(
    group_name: &str,
    wallet_hex: &str,
    config: ProtocolConfig,
) -> (OpenMlsService<MemoryDeMlsStorage>, Group, OutboundPacket) {
    let mls = setup_mls(wallet_hex);
    let group = prepare_to_join(group_name, mls.wallet_bytes().to_vec(), config);
    let kp_packet = build_key_package_message(&group, &mls, b"test-app-id").unwrap();
    (mls, group, kp_packet)
}

/// Full join: steward processes the joiner's KP, commits, and finalizes the
/// freeze round. Returns `(welcome_packet, batch_packet)`; callers needing
/// only the welcome can take `.0` and drop the rest.
///
/// Proposal IDs come from a process-local atomic counter starting at 100 —
/// high enough not to collide with the manually-picked IDs test code uses
/// for direct `insert_approved_proposal` calls.
pub fn steward_add_joiner(
    steward_mls: &OpenMlsService<MemoryDeMlsStorage>,
    steward_handle: &mut Group,
    joiner_kp_packet: &OutboundPacket,
) -> (OutboundPacket, OutboundPacket) {
    static PROPOSAL_COUNTER: AtomicU32 = AtomicU32::new(100);

    let result = process_inbound(
        steward_handle,
        &joiner_kp_packet.payload,
        WELCOME_SUBTOPIC,
        steward_mls,
    )
    .unwrap();

    let gur = match result {
        ProcessResult::MembershipChangeReceived(gur) => gur,
        other => panic!("Expected MembershipChangeReceived, got {:?}", other),
    };

    let proposal_id = PROPOSAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    steward_handle.insert_approved_proposal(proposal_id, gur);
    let packets = create_commit_candidate(steward_handle, steward_mls, b"test-app-id").unwrap();

    let finalize =
        finalize_freeze_round(steward_handle, steward_mls, false, b"test-app-id").unwrap();
    let welcome_packet = match finalize.outcome {
        FreezeOutcome::Applied { result, outbound } => {
            assert!(
                matches!(*result, ProcessResult::GroupUpdated),
                "Expected GroupUpdated, got {:?}",
                result
            );
            outbound
                .into_iter()
                .find(|p| p.subtopic == WELCOME_SUBTOPIC)
                .expect("Expected a deferred welcome packet from finalize_freeze_round")
        }
        other => panic!("Expected Applied, got {:?}", other),
    };

    let batch_packet = packets
        .iter()
        .find(|p| p.subtopic == APP_MSG_SUBTOPIC)
        .expect("Expected batch proposals packet")
        .clone();

    (welcome_packet, batch_packet)
}

// ─────────────────────────── Scoring ───────────────────────────

pub fn make_scoring() -> PeerScoringService<InMemoryPeerScoreStorage, FixedScoringProvider> {
    PeerScoringService::new(
        InMemoryPeerScoreStorage::new(),
        FixedScoringProvider::with_default_deltas(),
        ScoringConfig {
            default_score: DEFAULT_SCORE,
        },
    )
}
