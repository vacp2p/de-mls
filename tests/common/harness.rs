//! Mock-member test harness for driving the protocol through bare
//! [`Conversation`] handles — no `User`, no gateway, no transport trait.
//!
//! Modeled on the integrator's own mock-user test client: a [`TestHarness`]
//! owns `N` [`Member`]s wired to one in-memory bus, and [`TestHarness::process`]
//! / [`TestHarness::process_until`] drive the polling-and-relay loop until a
//! predicate holds. Each `Member` bundles the per-participant integrator state
//! (credential + signer, consensus backend, member id) with its live
//! `Conversation` and a captured-chat buffer.
//!
//! Relay model (de-mls layer has no transport subtopic):
//! - `Conversation::drain_outbound` emits only broadcast traffic (chat, votes,
//!   sync, commit candidates) — delivered to every member via `process_inbound`
//!   (self-echoes drop internally).
//! - Key-package announcements and welcomes never traverse `drain_outbound`:
//!   the harness mints/announces key packages explicitly and routes
//!   `WelcomeReady` events to joiners via `Conversation::join`.

#![allow(dead_code)]

use std::str::FromStr;
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;
use openmls::credentials::CredentialWithKey;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use prost::Message;

use de_mls::StewardListConfig;
use de_mls::defaults::{DefaultConsensusPlugin, DefaultPeerScoring, InMemoryPeerScoreStorage};
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, ConversationUpdateRequest, MemberInvite, MemberWelcome, app_message,
};
use de_mls::{Conversation, ConversationConfig, CreatorVote, MemberId, MemberRole, Outbound};
use de_mls::{ConversationError, ConversationEvent, ConversationState, MockClock, ScoringConfig};
use hashgraph_like_consensus::error::ConsensusError;

use crate::common::test_mls_group_config;
use crate::common::{
    MintedKeyPackage, make_scoring, mint_key_package, test_credential, wallet::WalletIdentity,
};
use de_mls::{Info, Obligation, Request};

/// Per-conversation MLS service stack the harness runs, on virtual time.
pub type TestConversation =
    Conversation<DefaultConsensusPlugin, InMemoryPeerScoreStorage, MockClock>;

/// Where every member's virtual clock starts (as a duration past the Unix
/// epoch): a realistic wall time, so consensus wire timestamps carry
/// production-scale values instead of the 0..2s band.
pub const HARNESS_EPOCH: Duration = Duration::from_secs(1_750_000_000);

/// Fast sub-second timing so the virtual-clock polling loop converges in a
/// handful of rounds.
pub fn fast_config() -> ConversationConfig {
    ConversationConfig {
        commit_batch_window: Duration::from_millis(50),
        freeze_duration: Duration::from_millis(20),
        voting_delay: Duration::from_millis(30),
        consensus_timeout: Duration::from_millis(150),
        proposal_expiration: Duration::from_millis(2000),
        backup_takeover_window: Duration::from_millis(150),
        ..ConversationConfig::default()
    }
}

/// Per-member integrator state: the member's credential + signer, the consensus
/// backend, the seed configs, and the provider it mints a key package into while
/// waiting for the matching welcome.
struct Integrator {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    consensus: DefaultConsensusPlugin,
    wallet: WalletIdentity,
    scoring_config: ScoringConfig,
    steward_list_config: StewardListConfig,
    /// One OpenMLS provider reused across every driving call (OpenMLS's native
    /// model). The creator seeds its group into it; a joiner mints its key
    /// package into it and later opens the welcome from it.
    provider: OpenMlsRustCrypto,
    /// This member's virtual clock. The harness keeps this handle and moves
    /// it forward each driving round; the conversation owns a clone.
    clock: MockClock,
}

impl Integrator {
    fn new(private_key: &str, steward_list_config: StewardListConfig) -> Self {
        let eth_signer = PrivateKeySigner::from_str(private_key).expect("valid private key");
        let wallet = WalletIdentity::from_address(eth_signer.address());
        let (credential, signer) = test_credential(wallet.address_bytes());
        Self {
            credential,
            signer,
            consensus: DefaultConsensusPlugin::new(EthereumConsensusSigner::new(eth_signer)),
            wallet,
            scoring_config: ScoringConfig::default(),
            steward_list_config,
            provider: OpenMlsRustCrypto::default(),
            clock: MockClock::at(HARNESS_EPOCH),
        }
    }

    /// Build a fresh scoring plug-in from this integrator's seed config.
    fn scoring(&self) -> DefaultPeerScoring {
        make_scoring(&self.scoring_config)
    }

    /// Mint a single-use key package into this integrator's reused provider,
    /// which thereby holds the private keys for the matching welcome.
    fn mint_key_package(&mut self) -> MintedKeyPackage {
        mint_key_package(&self.provider, &self.credential, &self.signer)
    }

    /// This integrator's `app_id` — its wallet address.
    fn app_id(&self) -> &[u8] {
        self.wallet.address_bytes()
    }
}

/// A captured inbound chat message: the decrypted body plus the MLS-authenticated
/// sender as a [`de_mls::Member`] (credential + signing key).
#[derive(Debug, Clone)]
pub struct ReceivedChat {
    pub body: Vec<u8>,
    pub sender: de_mls::Member,
}

/// One protocol participant: integrator state + its `Conversation` (absent
/// until a joiner accepts its welcome) + the log of every
/// [`ConversationEvent`] it has emitted. Query methods take `&self`; driving
/// methods take `&mut self`.
pub struct Member {
    integ: Integrator,
    convo: Option<TestConversation>,
    events: Vec<ConversationEvent>,
    /// Errors returned by `process_inbound` on delivery, in arrival order —
    /// delivery keeps going, but tests can assert a rejection happened.
    inbound_errors: Vec<ConversationError>,
    /// The encrypted `conversation_sync_bytes` from the welcome this member
    /// joined with — captured so a test can replay it (idempotency check).
    last_sync: Option<Vec<u8>>,
    /// Config passed to `Conversation::join` when this member accepts a welcome
    /// (joiner path) or rejoins.
    pending_config: ConversationConfig,
    /// When `false`, this member ignores every welcome addressed to it. Models
    /// an invitee that is in the ratchet tree but has never run the protocol:
    /// distinct from `mute`, which silences a member that did join.
    accepts_welcome: bool,
}

impl Member {
    /// Create a conversation as its sole founding steward, seeding the group
    /// with a freshly minted key package.
    pub fn create(
        private_key: &str,
        conversation_id: &str,
        config: ConversationConfig,
        steward_list_config: StewardListConfig,
    ) -> Self {
        let mut config = config;
        config.steward_list = steward_list_config.clone();
        let mls_group_config = test_mls_group_config();
        let integ = Integrator::new(private_key, steward_list_config);
        let scoring = integ.scoring();
        let convo = Conversation::create(
            conversation_id,
            &integ.provider,
            integ.credential.clone(),
            &mls_group_config,
            &integ.signer,
            &integ.consensus,
            scoring,
            integ.clock.clone(),
            integ.app_id(),
            config.clone(),
            &[],
        )
        .expect("create conversation");
        Self {
            integ,
            convo: Some(convo),
            events: Vec::new(),
            inbound_errors: Vec::new(),
            last_sync: None,
            pending_config: config,
            accepts_welcome: true,
        }
    }

    /// Build a prospective joiner: it holds integrator state and can mint /
    /// announce its key package, but has no conversation until its welcome
    /// arrives (`try_accept_welcome` installs it via `Conversation::join`).
    pub fn join(
        private_key: &str,
        _conversation_id: &str,
        config: ConversationConfig,
        steward_list_config: StewardListConfig,
    ) -> Self {
        let mut config = config;
        config.steward_list = steward_list_config.clone();
        let integ = Integrator::new(private_key, steward_list_config);
        Self {
            integ,
            convo: None,
            events: Vec::new(),
            inbound_errors: Vec::new(),
            last_sync: None,
            pending_config: config,
            accepts_welcome: true,
        }
    }

    // ── Queries (`&self`) ────────────────────────────────────────────────

    /// The live conversation. Panics if this member hasn't joined yet — only
    /// call on a member known to hold a conversation.
    pub fn convo(&self) -> &TestConversation {
        self.convo.as_ref().expect("member has joined")
    }

    /// The `app_id` this member's conversation was built with — its wallet
    /// address, the same bytes its credential carries.
    pub fn app_id(&self) -> &[u8] {
        self.integ.wallet.address_bytes()
    }

    /// This member's de-mls `member_id` (its leaf index) in the conversation —
    /// the value a `RemoveMember` proposal targets and that senders/members are
    /// reported by. A member's leaf index is the same in every node's tree, so
    /// this doubles as the id peers use to name it. Panics if not joined.
    pub fn member_id_bytes(&self) -> &[u8] {
        self.convo
            .as_ref()
            .expect("member has joined")
            .member_id_bytes()
    }

    /// This member's MLS signature public key — the key its leaf is bound to.
    /// Ground truth for asserting the key peers see on a `Member`.
    pub fn signing_pubkey(&self) -> Vec<u8> {
        self.integ.signer.to_public_vec()
    }

    /// Reelection retry round; `0` while no recovery election is in flight or
    /// the member hasn't joined.
    pub fn retry_round(&self) -> u32 {
        self.convo
            .as_ref()
            .and_then(|c| c.epoch_and_retry().ok())
            .map(|(_, r)| r)
            .unwrap_or(0)
    }

    /// Membership-change proposals buffered locally but not yet committed.
    pub fn pending_update_count(&self) -> usize {
        self.convo
            .as_ref()
            .map(|c| c.pending_update_count())
            .unwrap_or(0)
    }

    /// Count of proposals approved for the current epoch.
    pub fn approved_count(&self) -> usize {
        self.convo
            .as_ref()
            .map(|c| c.approved_proposals_for_current_epoch().len())
            .unwrap_or(0)
    }

    /// Per-member roles (steward / backup / member) in this conversation.
    pub fn member_roles(&self) -> Vec<(MemberId, MemberRole)> {
        self.convo
            .as_ref()
            .and_then(|c| c.member_roles().ok())
            .unwrap_or_default()
    }

    /// Per-member peer scores.
    pub fn member_scores(&self) -> Vec<(MemberId, i64)> {
        self.convo
            .as_ref()
            .map(|c| c.member_scores().expect("member_scores"))
            .unwrap_or_default()
    }

    /// The encrypted sync bytes this member joined with (for idempotency tests).
    pub fn last_welcome_sync(&self) -> Option<&[u8]> {
        self.last_sync.as_deref()
    }

    /// Proposal id of the most recent `VoteRequested` this member surfaced, if
    /// any — the id to pass to [`Self::vote`].
    pub fn pending_vote_request(&self) -> Option<u32> {
        self.events.iter().rev().find_map(|e| match e {
            ConversationEvent::Request(Request::VoteRequested { proposal_id, .. }) => {
                Some(*proposal_id)
            }
            _ => None,
        })
    }

    /// The `approved` outcome this member observed for `proposal_id`, if its
    /// consensus session has resolved.
    pub fn consensus_outcome(&self, proposal_id: u32) -> Option<bool> {
        self.events.iter().rev().find_map(|e| match e {
            ConversationEvent::Info(Info::ConsensusReached {
                proposal_id: id,
                approved,
                ..
            }) if *id == proposal_id => Some(*approved),
            _ => None,
        })
    }

    /// Current conversation state. Panics if this member hasn't joined yet.
    pub fn state(&self) -> ConversationState {
        self.convo.as_ref().expect("member has joined").state()
    }

    pub fn is_steward(&self) -> bool {
        self.convo.as_ref().is_some_and(|c| c.is_steward())
    }

    pub fn is_epoch_steward(&self) -> bool {
        self.convo
            .as_ref()
            .and_then(|c| c.is_epoch_steward().ok())
            .unwrap_or(false)
    }

    /// Ask a steward to re-send the `ConversationSync` (degraded-join recovery).
    pub fn request_conversation_sync(&mut self) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .request_conversation_sync(&self.integ.provider, &self.integ.signer)
            .expect("request conversation sync");
    }

    /// `true` once this member holds a conversation in `Working` (joiners
    /// without a welcome yet are not working).
    pub fn is_working(&self) -> bool {
        self.convo
            .as_ref()
            .is_some_and(|c| c.state() == ConversationState::Working)
    }

    pub fn epoch(&self) -> u64 {
        self.convo
            .as_ref()
            .and_then(|c| c.epoch_and_retry().ok())
            .map(|(e, _)| e)
            .unwrap_or(0)
    }

    /// This member's epoch authenticator, or empty if it hasn't joined.
    pub fn epoch_authenticator(&self) -> Vec<u8> {
        self.convo
            .as_ref()
            .map(|c| c.mls_group().epoch_authenticator().as_slice().to_vec())
            .unwrap_or_default()
    }

    pub fn member_count(&self) -> usize {
        self.convo
            .as_ref()
            .map(|c| c.mls_group().members().count())
            .unwrap_or(0)
    }

    /// This member's own [`MemberId`] handle. Valid group-wide (the tree is
    /// shared), so pass it to another member's `remove_member`.
    pub fn member_id(&self) -> MemberId {
        self.convo.as_ref().expect("member has joined").own_id()
    }

    /// Sorted signature keys of every member in this member's own tree view —
    /// the group's identity set, for agreement and membership checks.
    pub fn member_keys(&self) -> Vec<Vec<u8>> {
        let mut keys: Vec<Vec<u8>> = self
            .convo
            .as_ref()
            .map(|c| c.mls_group().members().map(|m| m.signature_key).collect())
            .unwrap_or_default();
        keys.sort();
        keys
    }

    /// Every [`ConversationEvent`] this member has emitted, in order.
    pub fn events(&self) -> &[ConversationEvent] {
        &self.events
    }

    /// Whether any delivered packet was rejected as an expired proposal —
    /// the receiver-side signature of clock skew.
    pub fn saw_expired_proposal(&self) -> bool {
        self.inbound_errors.iter().any(|e| {
            matches!(
                e,
                ConversationError::Consensus(ConsensusError::ProposalExpired)
            )
        })
    }

    /// Inbound chat messages, decoded from the event log in arrival order.
    pub fn received(&self) -> Vec<ReceivedChat> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ConversationEvent::Obligation(Obligation::ConversationMessage {
                    message:
                        AppMessage {
                            payload: Some(app_message::Payload::ConversationMessage(cm)),
                        },
                    sender,
                }) => Some(ReceivedChat {
                    body: cm.message.clone(),
                    sender: sender.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Whether a chat with `body` was received from any peer.
    pub fn got_chat(&self, body: &[u8]) -> bool {
        self.received().iter().any(|c| c.body == body)
    }

    /// The ordered sequence of `PhaseChange` states this member emitted.
    pub fn phase_changes(&self) -> Vec<ConversationState> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ConversationEvent::Info(Info::PhaseChange(s)) => Some(*s),
                _ => None,
            })
            .collect()
    }

    /// Whether this member ever transitioned into `state`.
    pub fn saw_phase(&self, state: ConversationState) -> bool {
        self.events.iter().any(|e| {
            matches!(
                e,
                ConversationEvent::Info(Info::PhaseChange(s)) if *s == state
            )
        })
    }

    /// Whether this member emitted a `Left` event (self-left or evicted).
    pub fn saw_left(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e, ConversationEvent::Obligation(Obligation::Left)))
    }

    /// Every `WelcomeReady` this member emitted, with its `minted_locally` flag
    /// (`true` only on the committing steward).
    pub fn welcome_readys(&self) -> Vec<(MemberWelcome, bool)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ConversationEvent::Obligation(Obligation::WelcomeReady {
                    welcome,
                    minted_locally,
                }) => Some((welcome.clone(), *minted_locally)),
                _ => None,
            })
            .collect()
    }

    /// Signature keys of every member added by a commit this member applied,
    /// in commit order. One entry per seated joiner, so a leaf reused by two
    /// different members appears twice.
    pub fn committed_adds(&self) -> Vec<Vec<u8>> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ConversationEvent::Info(Info::MembersChanged { added, .. }) => Some(added),
                _ => None,
            })
            .flatten()
            .map(|m| m.signature_key.clone())
            .collect()
    }

    /// Count of commits this member has applied. Every commit carries a
    /// membership change, so counting those counts commits.
    pub fn commits_applied(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, ConversationEvent::Info(Info::MembersChanged { .. })))
            .count()
    }

    // ── Actions (`&mut self`) ────────────────────────────────────────────

    pub fn send_message(&mut self, message: Vec<u8>) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .send_message(&self.integ.provider, &self.integ.signer, message)
            .expect("send message");
    }

    /// Invite `key_package`. A joiner already in the group is a no-op, which is
    /// how a relaying integrator treats it — every other failure panics.
    pub fn add_member(&mut self, key_package: &MintedKeyPackage) {
        match self.try_add_member(key_package) {
            Ok(()) | Err(ConversationError::AlreadyMember) => {}
            Err(e) => panic!("add member: {e}"),
        }
    }

    /// [`Self::add_member`] with the error surfaced, for tests asserting that a
    /// key package is turned away.
    pub fn try_add_member(
        &mut self,
        key_package: &MintedKeyPackage,
    ) -> Result<(), ConversationError> {
        self.convo.as_mut().expect("member has joined").add_member(
            &self.integ.provider,
            &self.integ.signer,
            key_package.as_bytes(),
        )
    }

    /// Propose removing `target` (another member's [`MemberId`] handle).
    pub fn remove_member(&mut self, target: MemberId) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .remove_member(&self.integ.provider, &self.integ.signer, &target)
            .expect("remove member");
    }

    /// File a `Deadlock` ECP to open Layer-3 recovery (the manual trigger).
    pub fn request_recovery(&mut self) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .request_recovery(&self.integ.provider, &self.integ.signer)
            .expect("request recovery");
    }

    /// File a `ScoreBelowThreshold` ECP against `member_id` — the emergency
    /// removal route an integrator drives off a low score.
    pub fn propose_score_removal(&mut self, member_id: &[u8]) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .propose_score_removal(&self.integ.provider, &self.integ.signer, member_id)
            .expect("propose score removal");
    }

    /// Cast a manual vote on `proposal_id` (cancels any pending auto-vote).
    pub fn vote(&mut self, proposal_id: u32, vote: bool) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .vote(&self.integ.provider, &self.integ.signer, proposal_id, vote)
            .expect("vote");
    }

    /// Submit a raw proposal with the local vote bundled. Lower-level than
    /// [`Self::remove_member`]; used for emergency/violation proposals.
    pub fn initiate_proposal(&mut self, request: ConversationUpdateRequest, vote: CreatorVote) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .initiate_proposal(&self.integ.provider, request, vote, &self.integ.signer)
            .expect("initiate proposal");
    }

    pub fn leave(&mut self) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .leave(&self.integ.provider, &self.integ.signer)
            .expect("leave");
    }

    /// Drain this member's buffered outbound directly (bypassing the bus) — for
    /// tests that count or inspect what a single member emitted.
    pub fn take_outbound(&mut self) -> Vec<Outbound> {
        self.drain_outbound()
    }

    /// Move this member's virtual clock forward by `d` — for tests that
    /// drive one member's `poll` directly instead of going through
    /// [`TestHarness::process`].
    pub fn advance_clock(&self, d: Duration) {
        self.integ.clock.advance(d);
    }

    /// Deliver a raw payload to this member as if it arrived from `sender`.
    /// No-op when the member hasn't joined.
    pub fn deliver_raw(&mut self, sender: &[u8], payload: &[u8]) {
        if let Some(convo) = self.convo.as_mut()
            && let Err(e) =
                convo.process_inbound(&self.integ.provider, &self.integ.signer, sender, payload)
        {
            self.inbound_errors.push(e);
        }
    }

    /// Mint a key package and return its announcement as an [`Outbound`] (the
    /// integrator publishes this on the welcome channel; the harness routes it
    /// to existing members via `add_member`).
    pub fn announce_key_package(&mut self, conversation_id: &str) -> Outbound {
        let key_package = self.integ.mint_key_package();
        Outbound {
            conversation_id: conversation_id.to_string(),
            sender: self.app_id().to_vec(),
            payload: build_key_package_announcement(key_package.as_bytes()),
        }
    }

    /// Mint a key package and hand back its bytes — for an explicit
    /// `add_member(kp_bytes)` (the proposer-side path, where a member is added
    /// without a prior key-package announcement).
    pub fn mint_key_package(&mut self) -> MintedKeyPackage {
        self.integ.mint_key_package()
    }

    /// Never open a welcome addressed to this member. Its key package still
    /// mints and still lands in the ratchet tree, so peers count it as a full
    /// member and a first-class voter — it simply never runs.
    pub fn withhold_welcome(&mut self) {
        self.accepts_welcome = false;
    }

    /// Reset this member to a fresh prospective joiner of `conversation_id`
    /// (same identity / keys) — for rejoining after eviction. The conversation
    /// rebuilds from the next welcome.
    pub fn rejoin(&mut self, _conversation_id: &str, config: ConversationConfig) {
        self.convo = None;
        self.last_sync = None;
        self.pending_config = config;
    }

    /// Advance this member's timers/state machine one tick. No-op when the
    /// member hasn't joined.
    pub fn poll(&mut self) {
        if let Some(convo) = self.convo.as_mut() {
            convo.poll(&self.integ.provider, &self.integ.signer);
        }
    }

    /// Drain this member's pending events into its log (no welcome routing) —
    /// for standalone members driven outside a [`TestHarness`].
    pub fn pump_events(&mut self) {
        let _ = self.drain_events_collect_welcomes();
    }

    fn drain_outbound(&mut self) -> Vec<Outbound> {
        self.convo
            .as_ref()
            .map(|c| c.drain_outbound())
            .unwrap_or_default()
    }

    fn deliver(&mut self, packet: &Outbound) {
        if let Some(convo) = self.convo.as_mut()
            && let Err(e) = convo.process_inbound(
                &self.integ.provider,
                &self.integ.signer,
                &packet.sender,
                &packet.payload,
            )
        {
            self.inbound_errors.push(e);
        }
    }

    /// Route a peer's key-package announcement to an existing member, which
    /// proposes the add. No-op when this member hasn't joined or the announcer
    /// is itself.
    fn deliver_key_package(&mut self, packet: &Outbound) {
        if packet.sender == self.app_id() {
            return;
        }
        let Some(convo) = self.convo.as_mut() else {
            return;
        };
        if convo.state() != ConversationState::Working {
            return;
        }
        let invite = MemberInvite::decode(packet.payload.as_slice()).expect("decode key package");
        // Mirror the integrator: an announced joiner is relayed via
        // `sponsor_member` (epoch steward proposes; others buffer for backup
        // takeover), not the explicit any-member `add_member`.
        let _ = convo.sponsor_member(
            &self.integ.provider,
            &self.integ.signer,
            &invite.key_package_bytes,
        );
    }

    /// Try to open `welcome` with this member's stashed key-package provider.
    /// Returns `true` only for the addressed joiner (builds the conversation
    /// from the welcome + bundled sync); `false` for everyone else. Already-
    /// joined members ignore further welcomes.
    fn try_accept_welcome(&mut self, welcome: &MemberWelcome) -> bool {
        if self.convo.is_some() || !self.accepts_welcome {
            return false;
        }
        // The integrator's reused provider holds the minted KP's private keys;
        // a member that never minted a KP holds none, so a welcome not for us
        // cleanly yields `Ok(None)`.
        let scoring = self.integ.scoring();
        // `join` opens the welcome internally; only the addressed joiner gets
        // `Some`.
        match Conversation::join(
            &self.integ.provider,
            &self.integ.signer,
            &welcome.welcome_bytes,
            &welcome.conversation_sync_bytes,
            &self.integ.consensus,
            scoring,
            self.integ.clock.clone(),
            self.integ.app_id(),
            self.pending_config.clone(),
        ) {
            Ok(Some(convo)) => {
                self.convo = Some(convo);
                self.last_sync = Some(welcome.conversation_sync_bytes.clone());
                true
            }
            Ok(None) | Err(_) => false,
        }
    }

    /// Drain events into the log, returning any locally-minted welcomes for the
    /// harness to route to joiners.
    fn drain_events_collect_welcomes(&mut self) -> Vec<MemberWelcome> {
        let mut welcomes = Vec::new();
        let Some(convo) = self.convo.as_ref() else {
            return welcomes;
        };
        for event in convo.drain_events() {
            if let ConversationEvent::Obligation(Obligation::WelcomeReady {
                welcome,
                minted_locally: true,
            }) = &event
            {
                welcomes.push(welcome.clone());
            }
            self.events.push(event);
        }
        welcomes
    }
}

/// Encode a key package into the wire format used for KP announcements.
/// Returns the prost-encoded `MemberInvite` bytes ready for broadcast on the
/// welcome subtopic.
pub fn build_key_package_announcement(key_package_bytes: &[u8]) -> Vec<u8> {
    MemberInvite {
        key_package_bytes: key_package_bytes.to_vec(),
    }
    .encode_to_vec()
}
/// `N` members on one in-memory bus. Drive with [`Self::process`] /
/// [`Self::process_until`].
pub struct TestHarness<const N: usize> {
    members: Vec<Member>,
    conversation_id: String,
    /// Members whose outbound is dropped instead of relayed — used to model a
    /// silent/partitioned participant (e.g. a steward that stops committing).
    muted: Vec<bool>,
}

impl<const N: usize> TestHarness<N> {
    /// Build `keys[0]` as creator and the rest as prospective joiners, with no
    /// driving: joiners hold no conversation yet. Use for tests that need to
    /// observe an intermediate state before the join cycle runs.
    pub fn start(
        keys: [&str; N],
        conversation_id: &str,
        config: ConversationConfig,
        steward_list_config: StewardListConfig,
    ) -> Self {
        assert!(N > 0, "harness needs at least one member");
        let mut members = Vec::with_capacity(N);
        members.push(Member::create(
            keys[0],
            conversation_id,
            config.clone(),
            steward_list_config.clone(),
        ));
        for key in keys.iter().skip(1) {
            members.push(Member::join(
                key,
                conversation_id,
                config.clone(),
                steward_list_config.clone(),
            ));
        }
        Self {
            members,
            conversation_id: conversation_id.to_string(),
            muted: vec![false; N],
        }
    }

    /// [`Self::start`] then drive the full join cycle until every joiner is
    /// `Working`. Panics if it does not converge.
    pub fn bootstrap(
        keys: [&str; N],
        conversation_id: &str,
        config: ConversationConfig,
        steward_list_config: StewardListConfig,
    ) -> Self {
        let mut harness = Self::start(keys, conversation_id, config, steward_list_config);

        // Joiners announce key packages to the creator, who promotes each to an
        // InviteMember proposal.
        let mut announcements = Vec::new();
        for member in harness.members.iter_mut().skip(1) {
            announcements.push(member.announce_key_package(&harness.conversation_id));
        }
        for packet in &announcements {
            harness.deliver_key_package_all(packet);
        }

        harness.process_until("bootstrap: all joiners working", |h| {
            h.members.iter().skip(1).all(Member::is_working)
        });
        harness
    }

    /// Stop relaying `index`'s outbound (its packets are drained and dropped).
    pub fn mute(&mut self, index: usize) {
        self.muted[index] = true;
    }

    /// Resume relaying `index`'s outbound.
    pub fn unmute(&mut self, index: usize) {
        self.muted[index] = false;
    }

    /// Deliver a key-package announcement to every member (self-echoes drop
    /// inside `deliver_key_package`).
    pub fn deliver_key_package_all(&mut self, packet: &Outbound) {
        for member in &mut self.members {
            member.deliver_key_package(packet);
        }
    }

    /// Deliver a key-package announcement to a single member — models an
    /// announcement that only some members observed (e.g. the epoch steward
    /// missed it, so a backup must carry the join).
    pub fn deliver_key_package_to(&mut self, index: usize, packet: &Outbound) {
        self.members[index].deliver_key_package(packet);
    }

    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    /// Every member reports the same MLS epoch — the core convergence check.
    pub fn epochs_agree(&self) -> bool {
        let mut epochs = self.members.iter().map(Member::epoch);
        match epochs.next() {
            Some(first) => epochs.all(|e| e == first),
            None => true,
        }
    }

    /// The shared epoch (member 0's view); pair with [`Self::epochs_agree`].
    pub fn epoch(&self) -> u64 {
        self.members[0].epoch()
    }

    /// Every member is in `Working`.
    pub fn all_working(&self) -> bool {
        self.members.iter().all(Member::is_working)
    }

    /// Every joined member shares the same epoch *and* the same epoch
    /// authenticator — the real convergence check.
    ///
    /// [`Self::epochs_agree`] compares epoch *numbers* and
    /// [`Self::membership_agrees`] compares member-id *sets*, and both report
    /// `true` across a forked group whose branches share an epoch and member set
    /// but hold different epoch secrets. The authenticator is the value that
    /// tells them apart, so this is the check that actually detects a fork.
    pub fn converged(&self) -> bool {
        let mut joined = self.members.iter().filter(|m| m.convo.is_some());
        let Some(first) = joined.next() else {
            return true;
        };
        let key = (first.epoch(), first.epoch_authenticator());
        joined.all(|m| (m.epoch(), m.epoch_authenticator()) == key)
    }

    /// Every member sees the same set of MLS members (order-independent).
    pub fn membership_agrees(&self) -> bool {
        let first = self.members[0].member_keys();
        self.members.iter().all(|m| m.member_keys() == first)
    }

    pub fn member(&self, index: usize) -> &Member {
        &self.members[index]
    }

    pub fn member_mut(&mut self, index: usize) -> &mut Member {
        &mut self.members[index]
    }

    pub fn members(&self) -> &[Member] {
        &self.members
    }

    /// One driving round: advance every member's virtual clock a step, poll
    /// every member, route welcomes, then broadcast all buffered outbound.
    /// Welcomes are routed before app traffic so a joiner's MLS is attached
    /// before same-round commit/election packets arrive. Events are drained
    /// twice — before and after relay — so phase/chat events emitted by this
    /// round's welcome acceptance and delivery land in the log before the
    /// next predicate check.
    pub fn process(&mut self, step: Duration) {
        for member in &self.members {
            member.integ.clock.advance(step);
        }
        for member in &mut self.members {
            member.poll();
        }
        self.drain_and_route_welcomes();
        self.relay_outbound();
        self.drain_and_route_welcomes();
    }

    /// Drain every member's events into its log and route any locally-minted
    /// welcome to its joiner.
    fn drain_and_route_welcomes(&mut self) {
        let mut welcomes = Vec::new();
        for (index, member) in self.members.iter_mut().enumerate() {
            let minted = member.drain_events_collect_welcomes();
            // A muted member's welcome is dropped like the rest of its
            // outbound: a welcome is something a member *sends*, so routing it
            // out of band would let a silenced steward keep seeding joiners.
            if !self.muted[index] {
                welcomes.extend(minted);
            }
        }
        for welcome in &welcomes {
            for member in &mut self.members {
                member.try_accept_welcome(welcome);
            }
        }
    }

    /// Drain every member's buffered outbound and deliver it to all members
    /// (self-echoes drop inside `process_inbound`). A muted member's outbound
    /// is drained but dropped, modelling a silent/partitioned participant.
    fn relay_outbound(&mut self) {
        let mut bus = Vec::new();
        for (index, member) in self.members.iter_mut().enumerate() {
            let outbound = member.drain_outbound();
            if !self.muted[index] {
                bus.extend(outbound);
            }
        }
        for packet in &bus {
            for member in &mut self.members {
                member.deliver(packet);
            }
        }
    }

    /// Drive `process` in 50 ms virtual-time steps until `predicate` holds,
    /// panicking after a fixed virtual-time budget. `label` is logged for
    /// diagnosis.
    pub fn process_until(&mut self, label: &str, predicate: impl Fn(&Self) -> bool) {
        const TIMEOUT: Duration = Duration::from_secs(30);
        const STEP: Duration = Duration::from_millis(50);
        let mut elapsed = Duration::ZERO;
        while !predicate(self) {
            if elapsed >= TIMEOUT {
                panic!("process_until timed out: {label}");
            }
            self.process(STEP);
            elapsed += STEP;
        }
    }
}
