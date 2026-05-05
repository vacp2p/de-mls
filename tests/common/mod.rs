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
use de_mls::mls_crypto::{
    IdentityProvider, MemoryDeMlsStorage, MlsService, OpenMlsService, WalletIdentity,
    key_package_bytes_from_json, parse_wallet_address,
};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, GroupUpdateRequest, InviteMember, WelcomeMessage, group_update_request,
    welcome_message,
};
use prost::Message as _;

/// Test-side MLS service: storage and identity are `Arc`-shared so a single
/// helper can build many per-group services from one identity.
pub type TestMls = OpenMlsService<Arc<MemoryDeMlsStorage>, Arc<WalletIdentity>>;

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

/// Build a fresh identity + storage pair, both Arc-shared so a later helper
/// can construct multiple per-group services from one logical identity.
pub fn setup_identity_storage(wallet_hex: &str) -> (Arc<WalletIdentity>, Arc<MemoryDeMlsStorage>) {
    let wallet = parse_wallet_address(wallet_hex).unwrap();
    let identity = Arc::new(WalletIdentity::from_wallet(wallet).unwrap());
    let storage = Arc::new(MemoryDeMlsStorage::new());
    (identity, storage)
}

/// Build an [`OpenMlsService`] for a fresh group with a default test id.
/// Convenience for tests that don't care about a specific group id at the
/// MLS layer (most pure-state tests). For tests that need a specific id,
/// prefer [`setup_steward`] or [`setup_mls_for_group`].
pub fn setup_mls(wallet_hex: &str) -> TestMls {
    setup_mls_for_group("test-group", wallet_hex)
}

/// Like [`setup_mls`] but with an explicit group id. Use this when the
/// test asserts something about the MLS-tracked group id or when two
/// services share a group.
pub fn setup_mls_for_group(group_name: &str, wallet_hex: &str) -> TestMls {
    let (identity, storage) = setup_identity_storage(wallet_hex);
    OpenMlsService::new_as_creator(group_name.to_string(), storage, identity).unwrap()
}

/// Test-only compatibility shim for the pre-refactor `core::process_inbound`
/// 4-arg signature. Welcome `UserKeyPackage` packets are still handled
/// here so existing tests for steward-side intake keep working;
/// `InvitationToJoin` packets must be ported to the new
/// [`OpenMlsService::new_from_welcome`] constructor (via
/// [`JoinerHandle::try_accept_welcome`]).
pub fn process_inbound_compat<M: MlsService>(
    group: &mut Group,
    payload: &[u8],
    subtopic: &str,
    mls: &M,
) -> Result<ProcessResult, de_mls::core::CoreError> {
    if subtopic == WELCOME_SUBTOPIC {
        let welcome_msg = WelcomeMessage::decode(payload)?;
        match welcome_msg.payload {
            Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;
                if mls.is_member(&identity) {
                    return Ok(ProcessResult::Noop);
                }
                Ok(ProcessResult::MembershipChangeReceived(
                    GroupUpdateRequest {
                        payload: Some(group_update_request::Payload::InviteMember(InviteMember {
                            key_package_bytes,
                            identity,
                        })),
                    },
                ))
            }
            Some(welcome_message::Payload::InvitationToJoin(_)) => {
                panic!(
                    "InvitationToJoin no longer flows through core::process_inbound; \
                     use OpenMlsService::new_from_welcome (e.g. JoinerHandle::try_accept_welcome)"
                );
            }
            None => Ok(ProcessResult::Noop),
        }
    } else if subtopic == APP_MSG_SUBTOPIC {
        process_inbound(group, payload, mls)
    } else {
        Err(de_mls::core::CoreError::InvalidSubtopic(
            subtopic.to_string(),
        ))
    }
}

pub fn setup_steward(group_name: &str, wallet_hex: &str) -> (TestMls, Group) {
    setup_steward_with_config(group_name, wallet_hex, default_steward_config())
}

pub fn setup_steward_with_config(
    group_name: &str,
    wallet_hex: &str,
    config: ProtocolConfig,
) -> (TestMls, Group) {
    let (identity, storage) = setup_identity_storage(wallet_hex);
    let mls =
        OpenMlsService::new_as_creator(group_name.to_string(), storage, Arc::clone(&identity))
            .unwrap();
    let group = create_group(group_name, identity.identity_bytes().to_vec(), config).unwrap();
    (mls, group)
}

/// Pre-join handle for a joiner: the joiner has no MLS service yet (they
/// haven't accepted a welcome), so test code keeps `identity` and `storage`
/// to build one via [`OpenMlsService::new_from_welcome`] when a welcome
/// arrives. KP generation uses the same storage/identity pair.
pub struct JoinerHandle {
    pub identity: Arc<WalletIdentity>,
    pub storage: Arc<MemoryDeMlsStorage>,
    pub group: Group,
    pub kp_packet: OutboundPacket,
}

impl JoinerHandle {
    /// Try to accept a welcome packet payload, materialising the joiner's
    /// MLS service if it addresses our key package. Returns `Ok(None)`
    /// when the welcome isn't for us.
    pub fn try_accept_welcome(
        &self,
        welcome_bytes: &[u8],
    ) -> Result<Option<TestMls>, de_mls::mls_crypto::MlsError> {
        OpenMlsService::new_from_welcome(
            welcome_bytes,
            Arc::clone(&self.storage),
            Arc::clone(&self.identity),
        )
    }
}

pub fn setup_joiner(group_name: &str, wallet_hex: &str) -> JoinerHandle {
    setup_joiner_with_config(group_name, wallet_hex, default_steward_config())
}

pub fn setup_joiner_with_config(
    group_name: &str,
    wallet_hex: &str,
    config: ProtocolConfig,
) -> JoinerHandle {
    let (identity, storage) = setup_identity_storage(wallet_hex);
    let group = prepare_to_join(group_name, identity.identity_bytes().to_vec(), config);
    let key_package =
        OpenMlsService::<Arc<MemoryDeMlsStorage>, Arc<WalletIdentity>>::generate_key_package(
            &storage, &identity,
        )
        .unwrap();
    let kp_packet = build_key_package_message(&group, key_package, b"test-app-id");
    JoinerHandle {
        identity,
        storage,
        group,
        kp_packet,
    }
}

/// Full join: steward processes the joiner's KP, commits, and finalizes the
/// freeze round. Returns `(welcome_packet, batch_packet)`; callers needing
/// only the welcome can take `.0` and drop the rest.
///
/// Proposal IDs come from a process-local atomic counter starting at 100 —
/// high enough not to collide with the manually-picked IDs test code uses
/// for direct `insert_approved_proposal` calls.
pub fn steward_add_joiner(
    steward_mls: &TestMls,
    steward_handle: &mut Group,
    joiner_kp_packet: &OutboundPacket,
) -> (OutboundPacket, OutboundPacket) {
    static PROPOSAL_COUNTER: AtomicU32 = AtomicU32::new(100);

    // The welcome subtopic dispatch moved to the User layer (it can't run in
    // core anymore because constructing a service from a welcome needs the
    // app-supplied factory). Tests still drive packet relay manually, so
    // we decode the KP envelope here and surface the same membership-change
    // request that core used to emit.
    let welcome_msg = WelcomeMessage::decode(joiner_kp_packet.payload.as_slice()).unwrap();
    let user_kp = match welcome_msg.payload {
        Some(welcome_message::Payload::UserKeyPackage(kp)) => kp,
        other => panic!("Expected UserKeyPackage, got {:?}", other),
    };
    let (key_package_bytes, identity) =
        key_package_bytes_from_json(user_kp.key_package_bytes).unwrap();
    if steward_mls.is_member(&identity) {
        panic!("Expected key package skipped for already-member");
    }
    let gur = GroupUpdateRequest {
        payload: Some(group_update_request::Payload::InviteMember(InviteMember {
            key_package_bytes,
            identity,
        })),
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
