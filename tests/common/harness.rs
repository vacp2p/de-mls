//! Mock-member test harness for driving the protocol through bare
//! [`Conversation`] handles — no `User`, no gateway, no transport trait.
//!
//! Modeled on the integrator's own mock-user test client: a [`TestHarness`]
//! owns `N` [`Member`]s wired to one in-memory bus, and [`TestHarness::process`]
//! / [`TestHarness::process_until`] drive the polling-and-relay loop until a
//! predicate holds. Each `Member` bundles the per-participant integrator state
//! (credential + signer, consensus storage + signer, member id) with its live
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
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use alloy::signers::local::PrivateKeySigner;
use hashgraph_like_consensus::signing::EthereumConsensusSigner;
use openmls::credentials::CredentialWithKey;
use openmls_basic_credential::SignatureKeyPair;
use openmls_rust_crypto::OpenMlsRustCrypto;
use prost::Message;

use de_mls::defaults::{DefaultConsensusPlugin, DefaultPeerScoring, DefaultStewardList};
use de_mls::mls_crypto::KeyPackageBytes;
use de_mls::protos::de_mls::messages::v1::{
    AppMessage, ConversationUpdateRequest, MemberInvite, MemberWelcome, app_message,
};
use de_mls::{
    ConsensusPlugin, ConsensusServiceFor, ConversationEvent, ConversationState, ScoringConfig,
    StewardListConfig,
};
use de_mls::{
    Conversation, ConversationConfig, CreatorVote, MemberRole, Outbound, PollOutcome,
    build_key_package_announcement,
};

use crate::common::{
    TEST_SUITE, TestProvider, make_scoring, make_steward, mint_key_package, test_credential,
    wallet::WalletMemberId,
};

/// Per-conversation MLS service stack the harness runs.
pub type TestConversation =
    Conversation<DefaultConsensusPlugin, TestProvider, DefaultPeerScoring, DefaultStewardList>;

const MAX_SESSIONS_PER_SCOPE: usize = 10;

/// Fast sub-second timing so a real-wall-clock polling loop converges in a
/// handful of rounds. Mirror of the gateway suite's `fast_test_config`.
pub fn fast_config() -> ConversationConfig {
    ConversationConfig {
        commit_inactivity_duration: Duration::from_millis(50),
        freeze_duration: Duration::from_millis(20),
        voting_delay: Duration::from_millis(30),
        election_voting_delay: Duration::from_millis(30),
        consensus_timeout: Duration::from_millis(150),
        proposal_expiration: Duration::from_millis(2000),
        ..ConversationConfig::default()
    }
}

/// Per-member integrator state: the member's credential + signer, the shared
/// consensus bits, the seed configs, and the provider it mints a key package
/// into while waiting for the matching welcome.
struct Integrator {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    consensus_storage: <DefaultConsensusPlugin as ConsensusPlugin>::ConsensusStorage,
    consensus_signer: <DefaultConsensusPlugin as ConsensusPlugin>::Signer,
    member_id: WalletMemberId,
    scoring_config: ScoringConfig,
    steward_list_config: StewardListConfig,
    /// Provider holding the private keys of a minted-but-not-yet-joined key
    /// package; `open_welcome` consumes it once the welcome arrives.
    pending_provider: Option<OpenMlsRustCrypto>,
}

impl Integrator {
    fn new(private_key: &str, steward_list_config: StewardListConfig) -> Self {
        let eth_signer = PrivateKeySigner::from_str(private_key).expect("valid private key");
        let member_id = WalletMemberId::from_address(eth_signer.address());
        let (credential, signer) = test_credential(member_id.member_id_bytes());
        Self {
            credential,
            signer,
            consensus_storage: DefaultConsensusPlugin::new_storage(),
            consensus_signer: EthereumConsensusSigner::new(eth_signer),
            member_id,
            scoring_config: ScoringConfig::default(),
            steward_list_config,
            pending_provider: None,
        }
    }

    /// Build a fresh scoring plug-in from this integrator's seed config.
    fn scoring(&self) -> DefaultPeerScoring {
        make_scoring(&self.scoring_config)
    }

    /// Build a fresh (empty) steward-list plug-in from this integrator's
    /// seed config.
    fn steward(&self) -> DefaultStewardList {
        make_steward(
            self.member_id.member_id_bytes(),
            self.steward_list_config.clone(),
        )
    }

    /// Mint a single-use key package, stashing its provider for the matching
    /// welcome.
    fn mint_key_package(&mut self) -> KeyPackageBytes {
        let (kp, provider) = mint_key_package(&self.credential, &self.signer);
        self.pending_provider = Some(provider);
        kp
    }

    /// Build a fresh per-conversation consensus service drawn from this
    /// integrator's shared (scope-keyed) storage and signer.
    fn consensus(&self) -> ConsensusServiceFor<DefaultConsensusPlugin> {
        ConsensusServiceFor::<DefaultConsensusPlugin>::new_with_components(
            self.consensus_storage.clone(),
            DefaultConsensusPlugin::new_event_bus(),
            self.consensus_signer.clone(),
            MAX_SESSIONS_PER_SCOPE,
        )
    }

    /// This integrator's `app_id` (its member-id bytes).
    fn app_id(&self) -> Arc<[u8]> {
        Arc::from(self.member_id.member_id_bytes())
    }
}

/// A captured inbound chat message: the decrypted body plus the sender's
/// opaque member-id bytes.
#[derive(Debug, Clone)]
pub struct ReceivedChat {
    pub body: Vec<u8>,
    pub sender: Vec<u8>,
}

/// One protocol participant: integrator state + its `Conversation` (absent
/// until a joiner accepts its welcome) + the log of every
/// [`ConversationEvent`] it has emitted. Query methods take `&self`; driving
/// methods take `&mut self`.
pub struct Member {
    integ: Integrator,
    convo: Option<TestConversation>,
    events: Vec<ConversationEvent>,
    /// The encrypted `conversation_sync_bytes` from the welcome this member
    /// joined with — captured so a test can replay it (idempotency check).
    last_sync: Option<Vec<u8>>,
    /// Config passed to `Conversation::join` when this member accepts a welcome
    /// (joiner path) or rejoins.
    pending_config: ConversationConfig,
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
        let integ = Integrator::new(private_key, steward_list_config);
        let scoring = integ.scoring();
        let steward = integ.steward();
        let convo = Conversation::create(
            conversation_id,
            TestProvider::default(),
            integ.credential.clone(),
            TEST_SUITE,
            &integ.signer,
            scoring,
            steward,
            integ.consensus(),
            integ.app_id(),
            config.clone(),
            integ.member_id.member_id_bytes(),
        )
        .expect("create conversation");
        Self {
            integ,
            convo: Some(convo),
            events: Vec::new(),
            last_sync: None,
            pending_config: config,
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
        let integ = Integrator::new(private_key, steward_list_config);
        Self {
            integ,
            convo: None,
            events: Vec::new(),
            last_sync: None,
            pending_config: config,
        }
    }

    // ── Queries (`&self`) ────────────────────────────────────────────────

    /// The live conversation. Panics if this member hasn't joined yet — only
    /// call on a member known to hold a conversation.
    pub fn convo(&self) -> &TestConversation {
        self.convo.as_ref().expect("member has joined")
    }

    pub fn app_id(&self) -> &[u8] {
        self.integ.member_id.member_id_bytes()
    }

    /// MLS member-id bytes (identical to `app_id` in this harness) — the value
    /// a `RemoveMember` proposal targets.
    pub fn member_id_bytes(&self) -> &[u8] {
        self.integ.member_id.member_id_bytes()
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
    pub fn member_roles(&self) -> Vec<(Vec<u8>, MemberRole)> {
        self.convo
            .as_ref()
            .and_then(|c| c.member_roles().ok())
            .unwrap_or_default()
    }

    /// Per-member peer scores.
    pub fn member_scores(&self) -> Vec<(Vec<u8>, i64)> {
        self.convo
            .as_ref()
            .map(|c| c.member_scores())
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
            ConversationEvent::VoteRequested { proposal_id, .. } => Some(*proposal_id),
            _ => None,
        })
    }

    /// The `approved` outcome this member observed for `proposal_id`, if its
    /// consensus session has resolved.
    pub fn consensus_outcome(&self, proposal_id: u32) -> Option<bool> {
        self.events.iter().rev().find_map(|e| match e {
            ConversationEvent::ConsensusReached {
                proposal_id: id,
                approved,
                ..
            } if *id == proposal_id => Some(*approved),
            _ => None,
        })
    }

    pub fn member_id_display(&self) -> String {
        self.integ.member_id.member_id_display().to_string()
    }

    /// Current conversation state. Panics if this member hasn't joined yet.
    pub fn state(&self) -> ConversationState {
        self.convo.as_ref().expect("member has joined").state()
    }

    pub fn is_steward(&self) -> bool {
        self.convo.as_ref().is_some_and(|c| c.is_steward())
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

    pub fn member_count(&self) -> usize {
        self.convo
            .as_ref()
            .and_then(|c| c.members().ok())
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Every [`ConversationEvent`] this member has emitted, in order.
    pub fn events(&self) -> &[ConversationEvent] {
        &self.events
    }

    /// Inbound chat messages, decoded from the event log in arrival order.
    pub fn received(&self) -> Vec<ReceivedChat> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ConversationEvent::AppMessage(AppMessage {
                    payload: Some(app_message::Payload::ConversationMessage(cm)),
                }) => Some(ReceivedChat {
                    body: cm.message.clone(),
                    sender: cm.sender.clone(),
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
                ConversationEvent::PhaseChange(s) => Some(*s),
                _ => None,
            })
            .collect()
    }

    /// Whether this member ever transitioned into `state`.
    pub fn saw_phase(&self, state: ConversationState) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e, ConversationEvent::PhaseChange(s) if *s == state))
    }

    /// Whether this member emitted a `Leaving` event (self-left or evicted).
    pub fn saw_leaving(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e, ConversationEvent::Leaving))
    }

    /// Every `WelcomeReady` this member emitted, with its `minted_locally` flag
    /// (`true` only on the committing steward).
    pub fn welcome_readys(&self) -> Vec<(MemberWelcome, bool)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ConversationEvent::WelcomeReady {
                    welcome,
                    minted_locally,
                } => Some((welcome.clone(), *minted_locally)),
                _ => None,
            })
            .collect()
    }

    /// Count of commits this member has applied (membership batches landed).
    pub fn commits_applied(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, ConversationEvent::CommitApplied(_)))
            .count()
    }

    // ── Actions (`&mut self`) ────────────────────────────────────────────

    pub fn send_message(&mut self, message: Vec<u8>) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .send_message(message, &self.integ.signer)
            .expect("send message");
    }

    pub fn add_member(&mut self, key_package: &[u8]) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .add_member(key_package, &self.integ.signer)
            .expect("add member");
    }

    pub fn remove_member(&mut self, member_id: &[u8]) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .remove_member(member_id, &self.integ.signer)
            .expect("remove member");
    }

    /// Cast a manual vote on `proposal_id` (cancels any pending auto-vote).
    pub fn vote(&mut self, proposal_id: u32, vote: bool) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .vote(proposal_id, vote, &self.integ.signer)
            .expect("vote");
    }

    /// Submit a raw proposal with the local vote bundled. Lower-level than
    /// [`Self::remove_member`]; used for emergency/violation proposals.
    pub fn initiate_proposal(&mut self, request: ConversationUpdateRequest, vote: CreatorVote) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .initiate_proposal(request, vote, &self.integ.signer)
            .expect("initiate proposal");
    }

    pub fn leave(&mut self) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .leave(&self.integ.signer)
            .expect("leave");
    }

    /// Drain this member's buffered outbound directly (bypassing the bus) — for
    /// tests that count or inspect what a single member emitted.
    pub fn take_outbound(&mut self) -> Vec<Outbound> {
        self.drain_outbound()
    }

    /// Deliver a raw payload to this member as if it arrived from `sender`.
    /// No-op when the member hasn't joined.
    pub fn deliver_raw(&mut self, sender: &[u8], payload: &[u8]) {
        if let Some(convo) = self.convo.as_mut() {
            let _ = convo.process_inbound(sender, payload, &self.integ.signer);
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
            payload: build_key_package_announcement(&key_package),
        }
    }

    /// Mint a key package and hand back its bytes — for an explicit
    /// `add_member(kp_bytes)` (the proposer-side path, where a member is added
    /// without a prior key-package announcement).
    pub fn mint_key_package(&mut self) -> KeyPackageBytes {
        self.integ.mint_key_package()
    }

    /// Reset this member to a fresh prospective joiner of `conversation_id`
    /// (same identity / keys) — for rejoining after eviction. The conversation
    /// rebuilds from the next welcome.
    pub fn rejoin(&mut self, _conversation_id: &str, config: ConversationConfig) {
        self.convo = None;
        self.last_sync = None;
        self.pending_config = config;
    }

    /// Advance this member's timers/state machine one tick, returning the
    /// poll outcome. No-op (empty outcome) when the member hasn't joined.
    pub fn poll(&mut self) -> PollOutcome {
        match self.convo.as_mut() {
            Some(convo) => convo.poll(&self.integ.signer),
            None => PollOutcome {
                next_wakeup_in: None,
                leave_requested: false,
            },
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
        if let Some(convo) = self.convo.as_mut() {
            let _ = convo.process_inbound(&packet.sender, &packet.payload, &self.integ.signer);
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
        let _ = convo.add_member(&invite.key_package_bytes, &self.integ.signer);
    }

    /// Try to open `welcome` with this member's stashed key-package provider.
    /// Returns `true` only for the addressed joiner (builds the conversation
    /// from the welcome + bundled sync); `false` for everyone else. Already-
    /// joined members ignore further welcomes.
    fn try_accept_welcome(&mut self, welcome: &MemberWelcome) -> bool {
        if self.convo.is_some() {
            return false;
        }
        // A fresh provider (we never minted a KP) holds no matching key package,
        // so a welcome not for us cleanly yields `Ok(None)`.
        let provider = self.integ.pending_provider.take().unwrap_or_default();
        let scoring = self.integ.scoring();
        let steward = self.integ.steward();
        // `join` opens the welcome internally; only the addressed joiner gets
        // `Some`.
        match Conversation::join(
            provider,
            &welcome.welcome_bytes,
            &welcome.conversation_sync_bytes,
            scoring,
            steward,
            self.integ.consensus(),
            self.integ.app_id(),
            self.pending_config.clone(),
            self.integ.member_id.member_id_bytes(),
            &self.integ.signer,
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
            if let ConversationEvent::WelcomeReady {
                welcome,
                minted_locally: true,
            } = &event
            {
                welcomes.push(welcome.clone());
            }
            self.events.push(event);
        }
        welcomes
    }
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

    /// Every member sees the same set of MLS members (order-independent).
    pub fn membership_agrees(&self) -> bool {
        let canonical = |m: &Member| {
            let mut ids = m.convo().members().unwrap_or_default();
            ids.sort();
            ids
        };
        let first = canonical(&self.members[0]);
        self.members.iter().all(|m| canonical(m) == first)
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

    /// One driving round: sleep a step, poll every member, route welcomes,
    /// then broadcast all buffered outbound. Welcomes are routed before app
    /// traffic so a joiner's MLS is attached before same-round commit/election
    /// packets arrive. Events are drained twice — before and after relay — so
    /// phase/chat events emitted by this round's welcome acceptance and
    /// delivery land in the log before the next predicate check.
    pub fn process(&mut self, step: Duration) {
        sleep(step);
        for member in &mut self.members {
            let _ = member.poll();
        }
        self.drain_and_route_welcomes();
        self.relay_outbound();
        self.drain_and_route_welcomes();
    }

    /// Drain every member's events into its log and route any locally-minted
    /// welcome to its joiner.
    fn drain_and_route_welcomes(&mut self) {
        let mut welcomes = Vec::new();
        for member in &mut self.members {
            welcomes.append(&mut member.drain_events_collect_welcomes());
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

    /// Drive `process` in 50 ms steps until `predicate` holds, panicking after
    /// a fixed timeout. `label` is logged for diagnosis.
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
