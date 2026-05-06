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

use de_mls::app::{FixedScoringProvider, InMemoryPeerScoreStorage};
use de_mls::core::PeerScoringService;
use de_mls::core::{
    CallbackError, FreezeOutcome, Group, GroupEventHandler, ProcessResult, ProtocolConfig,
    ScoringConfig, build_key_package_message, create_commit_candidate, finalize_freeze_round,
    process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::identity::{Identity, WalletIdentity, parse_wallet_address};
use de_mls::mls_crypto::{
    MemoryDeMlsStorage, MlsCredentials, MlsService, OpenMlsService, key_package_bytes_from_json,
};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, GroupUpdateRequest, InviteMember, WelcomeMessage, group_update_request,
    welcome_message,
};
use prost::Message as _;

/// Test-side MLS service: storage is `Arc`-shared so a single helper can
/// build many per-group services from one identity. MLS credentials live
/// at the test-fixture level (one per identity), passed in via
/// `Arc<MlsCredentials>`.
pub type TestMls = OpenMlsService<Arc<MemoryDeMlsStorage>>;

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

/// Build a fresh identity, MLS credentials, and storage. Both
/// credentials and storage are `Arc`-shared so a later helper can
/// construct multiple per-group services from one logical identity.
pub fn setup_identity_storage(
    wallet_hex: &str,
) -> (
    Arc<WalletIdentity>,
    Arc<MlsCredentials>,
    Arc<MemoryDeMlsStorage>,
) {
    let wallet = parse_wallet_address(wallet_hex).unwrap();
    let identity = Arc::new(WalletIdentity::from_wallet(wallet));
    let credentials = Arc::new(MlsCredentials::from_identity(identity.as_ref()).unwrap());
    let storage = Arc::new(MemoryDeMlsStorage::new());
    (identity, credentials, storage)
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
    let (_, credentials, storage) = setup_identity_storage(wallet_hex);
    OpenMlsService::new_as_creator(group_name.to_string(), storage, credentials).unwrap()
}

/// Compat shim that mirrors the pre-refactor `core::process_inbound`
/// 4-arg surface for tests that drive packet relay manually. Routes
/// app-subtopic packets to the new `core::process_inbound`, and synthesises
/// the steward-side `UserKeyPackage` membership-change response that used
/// to live inside core. `InvitationToJoin` packets must instead be
/// accepted via [`JoinerHandle::accept_welcome_packet`], which constructs
/// a fresh MLS service and attaches it to the joiner's group.
pub fn process_inbound_compat(
    group: &mut Group<TestMls>,
    payload: &[u8],
    subtopic: &str,
) -> Result<ProcessResult, de_mls::core::CoreError> {
    if subtopic == WELCOME_SUBTOPIC {
        let welcome_msg = WelcomeMessage::decode(payload)?;
        match welcome_msg.payload {
            Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;
                if let Some(mls) = group.mls()
                    && mls.is_member(&identity)
                {
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
                     use JoinerHandle::accept_welcome_packet"
                );
            }
            None => Ok(ProcessResult::Noop),
        }
    } else if subtopic == APP_MSG_SUBTOPIC {
        if group.mls().is_none() {
            // Pre-refactor behaviour: app messages on a group with no MLS
            // state are silently ignored. Mirrors the User-layer dispatch.
            return Ok(ProcessResult::Noop);
        }
        process_inbound(group, payload)
    } else {
        Err(de_mls::core::CoreError::InvalidSubtopic(
            subtopic.to_string(),
        ))
    }
}

pub fn setup_steward(group_name: &str, wallet_hex: &str) -> Group<TestMls> {
    setup_steward_with_config(group_name, wallet_hex, default_steward_config())
}

pub fn setup_steward_with_config(
    group_name: &str,
    wallet_hex: &str,
    config: ProtocolConfig,
) -> Group<TestMls> {
    let (identity, credentials, storage) = setup_identity_storage(wallet_hex);
    let mls =
        OpenMlsService::new_as_creator(group_name.to_string(), storage, Arc::clone(&credentials))
            .unwrap();
    Group::create_group(group_name, identity.identity_bytes().to_vec(), config, mls).unwrap()
}

/// Pre-join handle for a joiner: the joiner has no MLS service yet (they
/// haven't accepted a welcome), so test code keeps `credentials` and
/// `storage` to build one via [`OpenMlsService::new_from_welcome`] when a
/// welcome arrives. KP generation uses the same storage/credentials pair.
pub struct JoinerHandle {
    pub identity: Arc<WalletIdentity>,
    pub credentials: Arc<MlsCredentials>,
    pub storage: Arc<MemoryDeMlsStorage>,
    pub group: Group<TestMls>,
    pub kp_packet: OutboundPacket,
}

impl JoinerHandle {
    /// Try to accept a serialized welcome, materialising the joiner's
    /// MLS service if it addresses our key package. Returns `Ok(None)`
    /// when the welcome isn't for us.
    pub fn try_accept_welcome(
        &self,
        welcome_bytes: &[u8],
    ) -> Result<Option<TestMls>, de_mls::mls_crypto::MlsError> {
        OpenMlsService::new_from_welcome(
            welcome_bytes,
            Arc::clone(&self.storage),
            Arc::clone(&self.credentials),
        )
    }

    /// Accept a welcome `OutboundPacket` (the kind `steward_add_joiner`
    /// returns) and attach the resulting MLS service to `self.group`.
    /// Panics if the packet isn't an `InvitationToJoin` or doesn't match
    /// this joiner's key package — both indicate a test setup bug.
    pub fn accept_welcome_packet(&mut self, welcome_packet: &OutboundPacket) {
        let welcome_msg = WelcomeMessage::decode(welcome_packet.payload.as_slice()).unwrap();
        let invitation = match welcome_msg.payload {
            Some(welcome_message::Payload::InvitationToJoin(inv)) => inv,
            other => panic!("expected InvitationToJoin, got {:?}", other),
        };
        let svc = self
            .try_accept_welcome(&invitation.mls_message_out_bytes)
            .unwrap()
            .expect("welcome did not match this joiner's KP");
        self.group.attach_mls(svc);
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
    let (identity, credentials, storage) = setup_identity_storage(wallet_hex);
    let group: Group<TestMls> =
        Group::prepare_to_join(group_name, identity.identity_bytes().to_vec(), config);
    let key_package =
        OpenMlsService::<Arc<MemoryDeMlsStorage>>::generate_key_package(&storage, &credentials)
            .unwrap();
    let kp_packet = build_key_package_message(group_name, key_package, b"test-app-id");
    JoinerHandle {
        identity,
        credentials,
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
    steward_handle: &mut Group<TestMls>,
    joiner_kp_packet: &OutboundPacket,
) -> (OutboundPacket, OutboundPacket) {
    static PROPOSAL_COUNTER: AtomicU32 = AtomicU32::new(100);

    // The welcome subtopic dispatch moved to the User layer (constructing a
    // service from a welcome needs the app-supplied factory). Tests still
    // drive packet relay manually, so decode the KP envelope here and
    // surface the same membership-change request that core used to emit.
    let welcome_msg = WelcomeMessage::decode(joiner_kp_packet.payload.as_slice()).unwrap();
    let user_kp = match welcome_msg.payload {
        Some(welcome_message::Payload::UserKeyPackage(kp)) => kp,
        other => panic!("Expected UserKeyPackage, got {:?}", other),
    };
    let (key_package_bytes, identity) =
        key_package_bytes_from_json(user_kp.key_package_bytes).unwrap();
    let already_member = steward_handle.mls().is_some_and(|m| m.is_member(&identity));
    if already_member {
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
    let packets = create_commit_candidate(steward_handle, b"test-app-id").unwrap();

    let finalize = finalize_freeze_round(steward_handle, false, b"test-app-id").unwrap();
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
            threshold: 0,
        },
    )
}
