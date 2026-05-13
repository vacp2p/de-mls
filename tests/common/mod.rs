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
    BufferedCommitCandidate, CallbackError, Conversation, ConversationEventHandler, CoreError,
    DeterministicStewardList, FreezeOutcome, OperatingMode, ProcessResult, ProposalKind,
    ScoringConfig, StewardListConfig, StewardListPlugin, build_key_package_message,
    compute_commit_hash, finalize_freeze_round, member_set, process_inbound,
};
use de_mls::ds::{APP_MSG_SUBTOPIC, OutboundPacket, WELCOME_SUBTOPIC};
use de_mls::identity::{Identity, WalletIdentity, parse_wallet_address};
use de_mls::mls_crypto::{
    CommitCandidate as MlsCommitCandidate, KeyPackageBytes, MemoryDeMlsStorage, MlsCommitInput,
    MlsCredentials, MlsService, OpenMlsService, key_package_bytes_from_json,
};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, CommitCandidate, ConversationUpdateRequest, InviteMember, WelcomeMessage,
    conversation_update_request, welcome_message,
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
    LeaveConversation {
        group: String,
    },
    JoinedConversation {
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
impl ConversationEventHandler for MockHandler {
    async fn on_outbound(
        &self,
        conversation_name: &str,
        packet: OutboundPacket,
    ) -> Result<String, CallbackError> {
        self.events.lock().unwrap().push(Event::Outbound {
            group: conversation_name.to_string(),
            packet,
        });
        Ok("mock-id".to_string())
    }

    async fn on_app_message(
        &self,
        conversation_name: &str,
        message: AppMessage,
    ) -> Result<(), CallbackError> {
        self.events.lock().unwrap().push(Event::AppMessage {
            group: conversation_name.to_string(),
            msg: message,
        });
        Ok(())
    }

    async fn on_leave_conversation(&self, conversation_name: &str) -> Result<(), CallbackError> {
        self.events.lock().unwrap().push(Event::LeaveConversation {
            group: conversation_name.to_string(),
        });
        Ok(())
    }

    async fn on_joined_conversation(&self, conversation_name: &str) -> Result<(), CallbackError> {
        self.events.lock().unwrap().push(Event::JoinedConversation {
            group: conversation_name.to_string(),
        });
        Ok(())
    }

    async fn on_error(&self, conversation_name: &str, operation: &str, error: &str) {
        self.events.lock().unwrap().push(Event::Error {
            group: conversation_name.to_string(),
            op: operation.to_string(),
            err: error.to_string(),
        });
    }
}

// ─────────────────────────── Test-only protocol helper ───────────────────────────

/// Test-side mirror of `ConversationHandle::create_commit_candidate`.
///
/// Production callers go through `ConversationHandle::create_commit_candidate`,
/// which pulls `group`, `mls`, and `steward` from `&mut self`. Tests
/// keep these as separate fields on `StewardHandle` / `JoinerHandle`
/// (no `state_machine` / `scoring`), so we can't use the entry method
/// directly. This helper takes the same fields as parameters and runs
/// identical logic. **Keep in sync with `ConversationHandle::create_commit_candidate`.**
pub fn build_commit_candidate(
    group: &mut Conversation,
    mls: &TestMls,
    steward: &DeterministicStewardList,
    in_recovery: bool,
    self_identity: &[u8],
    app_id: &[u8],
) -> Result<Option<OutboundPacket>, CoreError> {
    if !steward.is_steward(self_identity) && !in_recovery {
        return Err(CoreError::NotASteward);
    }
    if group.approved_proposals().is_empty() {
        return Err(CoreError::NoProposals);
    }

    let self_removal_pending = group.approved_proposals().values().any(|req| {
        matches!(
            req.payload.as_ref(),
            Some(conversation_update_request::Payload::RemoveMember(r))
                if r.identity == self_identity
        )
    });
    if self_removal_pending {
        return Ok(None);
    }

    let non_mls_ids: Vec<u32> = group
        .approved_proposals()
        .iter()
        .filter(|(_, req)| ProposalKind::of(req).is_governance())
        .map(|(&id, _)| id)
        .collect();
    if !non_mls_ids.is_empty() {
        return Err(CoreError::UnexpectedNonMlsProposals {
            proposal_ids: non_mls_ids,
        });
    }

    let current_members = mls.members()?;
    let current_members_set = member_set(&current_members);
    let is_member = |id: &[u8]| current_members_set.contains(id);
    let urgent_target = group.urgent_commit_target().map(|t| t.to_vec());

    let k_max = mls.commit_batch_max();
    let mut updates = Vec::with_capacity(group.approved_order().len().min(k_max));
    for pid in group.approved_order() {
        if updates.len() >= k_max {
            break;
        }
        let Some(proposal) = group.approved_proposals().get(pid) else {
            continue;
        };
        match proposal.payload.as_ref() {
            Some(conversation_update_request::Payload::InviteMember(im)) => {
                if urgent_target.is_some() {
                    continue;
                }
                if is_member(&im.identity) {
                    continue;
                }
                updates.push(MlsCommitInput::Add(KeyPackageBytes::new(
                    im.key_package_bytes.clone(),
                    im.identity.clone(),
                )));
            }
            Some(conversation_update_request::Payload::RemoveMember(rm)) => {
                if let Some(target) = urgent_target.as_deref()
                    && rm.identity != target
                {
                    continue;
                }
                if !is_member(&rm.identity) {
                    continue;
                }
                updates.push(MlsCommitInput::Remove(rm.identity.clone()));
            }
            _ => return Err(CoreError::InvalidConversationUpdateRequest),
        }
    }

    if updates.is_empty() {
        return Ok(None);
    }

    let MlsCommitCandidate {
        proposals: mls_proposals,
        commit,
        welcome,
    } = mls.create_commit_candidate(&updates)?;

    let candidate = CommitCandidate {
        conversation_name: group.name_bytes().to_vec(),
        mls_proposals,
        commit_message: commit,
        steward_identity: self_identity.to_vec(),
    };

    let commit_hash = compute_commit_hash(&candidate.commit_message);
    let epoch = mls.current_epoch()?;
    let _ = group.add_freeze_candidate(
        BufferedCommitCandidate {
            candidate_msg: candidate.clone(),
            commit_hash,
            is_local_candidate: true,
            welcome_bytes: welcome,
        },
        epoch,
    );

    let candidate_msg: AppMessage = candidate.into();
    Ok(Some(OutboundPacket::new(
        candidate_msg.encode_to_vec(),
        APP_MSG_SUBTOPIC,
        group.name(),
        app_id,
    )))
}

// ─────────────────────────── MLS + group setup ───────────────────────────

pub fn default_steward_config() -> StewardListConfig {
    StewardListConfig::new(1, 5).unwrap()
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
pub fn setup_mls_for_group(conversation_name: &str, wallet_hex: &str) -> TestMls {
    let (_, credentials, storage) = setup_identity_storage(wallet_hex);
    OpenMlsService::new_as_creator(conversation_name.to_string(), storage, credentials).unwrap()
}

/// Compat shim that mirrors the pre-refactor `core::process_inbound`
/// 4-arg surface for tests that drive packet relay manually. Routes
/// app-subtopic packets to the new `core::process_inbound`, and synthesises
/// the steward-side `UserKeyPackage` membership-change response that used
/// to live inside core. `InvitationToJoin` packets must instead be
/// accepted via [`JoinerHandle::accept_welcome_packet`], which constructs
/// a fresh MLS service and attaches it to the joiner's group.
pub fn process_inbound_compat(
    group: &mut Conversation,
    mls: Option<&TestMls>,
    payload: &[u8],
    subtopic: &str,
) -> Result<ProcessResult, de_mls::core::CoreError> {
    if subtopic == WELCOME_SUBTOPIC {
        let welcome_msg = WelcomeMessage::decode(payload)?;
        match welcome_msg.payload {
            Some(welcome_message::Payload::UserKeyPackage(user_kp)) => {
                let (key_package_bytes, identity) =
                    key_package_bytes_from_json(user_kp.key_package_bytes)?;
                if let Some(mls) = mls
                    && mls.is_member(&identity)
                {
                    return Ok(ProcessResult::Noop);
                }
                Ok(ProcessResult::MembershipChangeReceived(
                    ConversationUpdateRequest {
                        payload: Some(conversation_update_request::Payload::InviteMember(
                            InviteMember {
                                key_package_bytes,
                                identity,
                            },
                        )),
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
        let Some(mls) = mls else {
            // Pre-refactor behaviour: app messages on a group with no MLS
            // state are silently ignored. Mirrors the User-layer dispatch.
            return Ok(ProcessResult::Noop);
        };
        process_inbound(group, mls, payload)
    } else {
        Err(de_mls::core::CoreError::InvalidSubtopic(
            subtopic.to_string(),
        ))
    }
}

/// Test handle: bundles `Conversation`, the per-group MLS service, the
/// steward-list plug-in, and the self identity. Tests access them as
/// individual fields; `Deref<Target = Conversation>` keeps proposal-queue calls
/// working unchanged (e.g. `handle.insert_approved_proposal(...)`).
pub struct StewardHandle {
    pub group: Conversation,
    pub mls: TestMls,
    pub steward: DeterministicStewardList,
    pub identity: Vec<u8>,
    pub liveness_criteria_yes: bool,
    pub pending_update_max_epochs: u32,
    pub operating_mode: OperatingMode,
}

impl StewardHandle {
    /// Self-identity bytes for plug-in queries.
    pub fn self_id(&self) -> &[u8] {
        &self.identity
    }

    /// Test convenience: identity bytes stored on the handle; matches the
    /// value the orchestrator caches at the User level.
    pub fn self_identity(&self) -> &[u8] {
        &self.identity
    }
}

impl std::ops::Deref for StewardHandle {
    type Target = Conversation;
    fn deref(&self) -> &Conversation {
        &self.group
    }
}

impl std::ops::DerefMut for StewardHandle {
    fn deref_mut(&mut self) -> &mut Conversation {
        &mut self.group
    }
}

pub fn setup_steward(conversation_name: &str, wallet_hex: &str) -> StewardHandle {
    setup_steward_with_config(conversation_name, wallet_hex, default_steward_config())
}

pub fn setup_steward_with_config(
    conversation_name: &str,
    wallet_hex: &str,
    config: StewardListConfig,
) -> StewardHandle {
    let (identity, credentials, storage) = setup_identity_storage(wallet_hex);
    let mls = OpenMlsService::new_as_creator(
        conversation_name.to_string(),
        storage,
        Arc::clone(&credentials),
    )
    .unwrap();
    let identity_bytes = identity.identity_bytes().to_vec();
    let group = Conversation::create(conversation_name);
    let mut steward =
        DeterministicStewardList::empty(conversation_name.as_bytes().to_vec(), config);
    let _events = steward
        .install_list(0, std::slice::from_ref(&identity_bytes), 1, 0)
        .expect("bootstrap list install");
    StewardHandle {
        group,
        mls,
        steward,
        identity: identity_bytes,
        liveness_criteria_yes: de_mls::core::DEFAULT_LIVENESS_CRITERIA_YES,
        pending_update_max_epochs: de_mls::core::DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
        operating_mode: OperatingMode::Normal,
    }
}

/// Pre-join handle for a joiner: the joiner has no MLS service yet (they
/// haven't accepted a welcome), so test code keeps `credentials` and
/// `storage` to build one via [`OpenMlsService::new_from_welcome`] when a
/// welcome arrives. KP generation uses the same storage/credentials pair.
pub struct JoinerHandle {
    pub identity: Arc<WalletIdentity>,
    pub credentials: Arc<MlsCredentials>,
    pub storage: Arc<MemoryDeMlsStorage>,
    pub group: Conversation,
    pub mls: Option<TestMls>,
    pub steward: DeterministicStewardList,
    pub kp_packet: OutboundPacket,
    pub liveness_criteria_yes: bool,
    pub pending_update_max_epochs: u32,
    pub operating_mode: OperatingMode,
}

impl JoinerHandle {
    /// Test convenience: identity bytes stored on the handle; matches the
    /// value the orchestrator caches at the User level.
    pub fn self_identity(&self) -> Vec<u8> {
        self.identity.identity_bytes().to_vec()
    }

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
    /// returns) and store the resulting MLS service on the handle.
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
        self.mls = Some(svc);
    }
}

pub fn setup_joiner(conversation_name: &str, wallet_hex: &str) -> JoinerHandle {
    setup_joiner_with_config(conversation_name, wallet_hex, default_steward_config())
}

pub fn setup_joiner_with_config(
    conversation_name: &str,
    wallet_hex: &str,
    config: StewardListConfig,
) -> JoinerHandle {
    let (identity, credentials, storage) = setup_identity_storage(wallet_hex);
    let group = Conversation::prepare_to_join(conversation_name);
    let steward = DeterministicStewardList::empty(conversation_name.as_bytes().to_vec(), config);
    let key_package =
        OpenMlsService::<Arc<MemoryDeMlsStorage>>::generate_key_package(&storage, &credentials)
            .unwrap();
    let kp_packet = build_key_package_message(conversation_name, key_package, b"test-app-id");
    JoinerHandle {
        identity,
        credentials,
        storage,
        group,
        mls: None,
        steward,
        kp_packet,
        liveness_criteria_yes: de_mls::core::DEFAULT_LIVENESS_CRITERIA_YES,
        pending_update_max_epochs: de_mls::core::DEFAULT_PENDING_UPDATE_MAX_EPOCHS,
        operating_mode: OperatingMode::Normal,
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
    steward_handle: &mut StewardHandle,
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
    let already_member = steward_handle.mls.is_member(&identity);
    if already_member {
        panic!("Expected key package skipped for already-member");
    }
    let gur = ConversationUpdateRequest {
        payload: Some(conversation_update_request::Payload::InviteMember(
            InviteMember {
                key_package_bytes,
                identity,
            },
        )),
    };

    let proposal_id = PROPOSAL_COUNTER.fetch_add(1, Ordering::Relaxed);
    steward_handle
        .group
        .insert_approved_proposal(proposal_id, gur);
    let self_id = steward_handle.identity.clone();
    let packets = build_commit_candidate(
        &mut steward_handle.group,
        &steward_handle.mls,
        &steward_handle.steward,
        steward_handle.operating_mode == OperatingMode::Recovery,
        &self_id,
        b"test-app-id",
    )
    .unwrap();

    let finalize = finalize_freeze_round(
        &mut steward_handle.group,
        &steward_handle.mls,
        &steward_handle.steward,
        steward_handle.operating_mode == OperatingMode::Recovery,
        false,
        b"test-app-id",
        &self_id,
    )
    .unwrap();
    let welcome_packet = match finalize.outcome {
        FreezeOutcome::Applied { result, outbound } => {
            assert!(
                matches!(*result, ProcessResult::ConversationUpdated),
                "Expected ConversationUpdated, got {:?}",
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
