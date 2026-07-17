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
use std::sync::Arc;
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
    conversation_update_request,
};
use de_mls::{
    Conversation, ConversationConfig, CreatorVote, MemberRole, Outbound, PollOutcome, WallClock,
};
use de_mls::{ConversationError, ConversationEvent, ConversationState, MockClock, ScoringConfig};
use hashgraph_like_consensus::error::ConsensusError;

use crate::common::test_mls_group_config;
use crate::common::{
    MintedKeyPackage, make_scoring, mint_key_package, test_credential, wallet::WalletMemberId,
};

/// Per-conversation MLS service stack the harness runs, on virtual time.
pub type TestConversation =
    Conversation<DefaultConsensusPlugin, InMemoryPeerScoreStorage, MockClock>;

/// Where every member's virtual clock starts (as a duration past the Unix
/// epoch): a realistic wall time, so consensus wire timestamps carry
/// production-scale values instead of the 0..2s band.
pub const HARNESS_EPOCH: Duration = Duration::from_secs(1_750_000_000);

/// Fast sub-second agreement/settle timing (the de-mls-owned config) so the
/// virtual-clock polling loop converges in a handful of rounds.
pub fn fast_config() -> ConversationConfig {
    ConversationConfig {
        freeze_duration: Duration::from_millis(20),
        voting_delay: Duration::from_millis(30),
        election_voting_delay: Duration::from_millis(30),
        consensus_timeout: Duration::from_millis(150),
        proposal_expiration: Duration::from_millis(2000),
        ..ConversationConfig::default()
    }
}

/// The app's commit-inactivity delay — how long the harness (standing in for a
/// real integrator) lets the epoch steward sit on approved work before driving
/// `commit_now`. de-mls no longer owns this liveness timing, so the reference
/// app carries it. A backup's takeover window derives from it plus the
/// de-mls-owned `recovery_inactivity_duration`.
pub const HARNESS_COMMIT_INACTIVITY: Duration = Duration::from_millis(50);

/// Per-member integrator state: the member's credential + signer, the consensus
/// backend, the seed configs, and the provider it mints a key package into while
/// waiting for the matching welcome.
struct Integrator {
    credential: CredentialWithKey,
    signer: SignatureKeyPair,
    consensus: DefaultConsensusPlugin,
    member_id: WalletMemberId,
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
        let member_id = WalletMemberId::from_address(eth_signer.address());
        let (credential, signer) = test_credential(member_id.member_id_bytes());
        Self {
            credential,
            signer,
            consensus: DefaultConsensusPlugin::new(EthereumConsensusSigner::new(eth_signer)),
            member_id,
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
    /// Virtual-time anchor for the harness's commit-inactivity policy: when this
    /// member first saw approved work to commit. de-mls no longer times commits,
    /// so the harness (the reference app) drives `commit_now` off this. `None`
    /// when there is nothing pending. See [`Member::drive_commit_policy`].
    commit_anchor: Option<Duration>,
    /// Anchors for the backup-takeover policies de-mls no longer times: silent
    /// reelection round, unanswered sync request, and unproposed buffered
    /// updates. Each records when its condition was first seen; the harness acts
    /// once the voting-inactivity window passes. See [`Member::drive_takeover_policy`].
    reelection_anchor: Option<Duration>,
    sync_resend_anchor: Option<Duration>,
    buffered_anchor: Option<Duration>,
    /// Recovery-commit policy: when `true` (the default), this member mints in
    /// Layer-3 recovery via `commit_in_recovery` each round it's open. de-mls no
    /// longer auto-mints; this is the harness standing in for that policy.
    /// A test flips it off (via [`TestHarness::set_recovery_auto_commit`]) to
    /// model a manual-only integrator that never presses the button.
    recovery_auto_commit: bool,
    /// The app's commit-inactivity delay for this member (see
    /// [`HARNESS_COMMIT_INACTIVITY`]). de-mls no longer carries it; the harness
    /// does, and a test tunes it via [`TestHarness::set_commit_inactivity`].
    commit_inactivity: Duration,
}

/// Arm `anchor` the first tick `active` holds and report whether `delay` has
/// since elapsed; clear it when `active` goes false. The shared shape behind
/// every harness takeover policy.
fn window_elapsed(
    anchor: &mut Option<Duration>,
    now: Duration,
    active: bool,
    delay: Duration,
) -> bool {
    if !active {
        *anchor = None;
        return false;
    }
    let start = *anchor.get_or_insert(now);
    now.saturating_sub(start) >= delay
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
            integ.member_id.member_id_bytes(),
            &integ.provider,
            integ.credential.clone(),
            &mls_group_config,
            &integ.signer,
            &integ.consensus,
            scoring,
            integ.clock.clone(),
            integ.app_id(),
            config.clone(),
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
            commit_anchor: None,
            reelection_anchor: None,
            sync_resend_anchor: None,
            buffered_anchor: None,
            recovery_auto_commit: true,
            commit_inactivity: HARNESS_COMMIT_INACTIVITY,
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
            commit_anchor: None,
            reelection_anchor: None,
            sync_resend_anchor: None,
            buffered_anchor: None,
            recovery_auto_commit: true,
            commit_inactivity: HARNESS_COMMIT_INACTIVITY,
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
            .map(|c| c.epoch_authenticator())
            .unwrap_or_default()
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
                ConversationEvent::ConversationMessage(AppMessage {
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

    /// Distinct proposals this member has seen that admit `member_id` — its own
    /// submissions plus peers' surfaced for a vote. Counts `MemberInvite` only:
    /// a group that outgrows `sn_max` also votes on a steward election, and that
    /// is not a duplicate invitation.
    pub fn observed_invite_proposals(&self, member_id: &[u8]) -> Vec<u32> {
        let invites_target = |req: &ConversationUpdateRequest| {
            matches!(
                req.payload.as_ref(),
                Some(conversation_update_request::Payload::MemberInvite(im))
                    if im.member_id == member_id
            )
        };
        let mut ids: Vec<u32> = self
            .events
            .iter()
            .filter_map(|e| match e {
                ConversationEvent::VoteRequested {
                    proposal_id,
                    request,
                }
                | ConversationEvent::OwnProposalSubmitted {
                    proposal_id,
                    request,
                } if invites_target(request) => Some(*proposal_id),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Member ids added by every commit this member applied, in commit order.
    /// One entry per `MemberInvite` in the committed batch, so a member added
    /// twice appears twice.
    pub fn committed_adds(&self) -> Vec<Vec<u8>> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ConversationEvent::CommitApplied(reqs) => Some(reqs),
                _ => None,
            })
            .flatten()
            .filter_map(|r| match r.payload.as_ref() {
                Some(conversation_update_request::Payload::MemberInvite(im)) => {
                    Some(im.member_id.clone())
                }
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
            .send_message(&self.integ.provider, &self.integ.signer, message)
            .expect("send message");
    }

    pub fn add_member(&mut self, key_package: &MintedKeyPackage) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .add_member(
                &self.integ.provider,
                &self.integ.signer,
                key_package.member_id(),
                key_package.as_bytes(),
            )
            .expect("add member");
    }

    pub fn remove_member(&mut self, member_id: &[u8]) {
        self.convo
            .as_mut()
            .expect("member has joined")
            .remove_member(&self.integ.provider, &self.integ.signer, member_id)
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
            payload: build_key_package_announcement(
                key_package.as_bytes(),
                key_package.member_id(),
            ),
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
        self.commit_anchor = None;
        self.reelection_anchor = None;
        self.sync_resend_anchor = None;
        self.buffered_anchor = None;
    }

    /// Advance this member's timers/state machine one tick, returning the
    /// poll outcome. No-op (empty outcome) when the member hasn't joined.
    pub fn poll(&mut self) -> PollOutcome {
        match self.convo.as_mut() {
            Some(convo) => convo.poll(&self.integ.provider, &self.integ.signer),
            None => PollOutcome {
                next_wakeup_in: None,
                leave_requested: false,
            },
        }
    }

    /// Reference commit-inactivity policy — the timing de-mls no longer keeps,
    /// reimplemented here as the harness's own liveness policy. Once this member
    /// holds approved work, wait a role-based delay off `commit_anchor`, then
    /// call `commit_now`: the epoch steward leads at `commit_inactivity` (the
    /// app's own delay); every other member waits an extra
    /// `recovery_inactivity_duration`. In the
    /// normal case the primary's commit lands first and clears the work before
    /// the longer delay fires; when the sole steward is silent, a non-steward's
    /// freeze-entry produces a NoCandidate that escalates to reelection — the
    /// path a group recovers by. Called once per member each `process` round,
    /// after `poll`.
    pub fn drive_commit_policy(&mut self) {
        let (lead, recovering) = {
            let Some(convo) = self.convo.as_ref() else {
                self.commit_anchor = None;
                return;
            };
            if convo.pending_commit_work().is_none() {
                self.commit_anchor = None;
                return;
            }
            (
                convo.is_epoch_steward().unwrap_or(false),
                convo.in_recovery_posture(),
            )
        };
        let now = self.integ.clock.now();
        let anchor = *self.commit_anchor.get_or_insert(now);
        // A recovery continuation (open recovery mode or a bumped retry round)
        // commits within the short recovery window rather than waiting out a
        // fresh full commit-inactivity epoch.
        let delay = if recovering {
            self.pending_config.recovery_inactivity_duration
        } else if lead {
            self.commit_inactivity
        } else {
            self.commit_inactivity + self.pending_config.recovery_inactivity_duration
        };
        if now.saturating_sub(anchor) < delay {
            return;
        }
        self.commit_anchor = None;
        // Best-effort, like the poll step it replaces: a transient mint failure
        // leaves the work pending and surfaces as non-convergence in the test.
        if let Some(convo) = self.convo.as_mut() {
            let _ = convo.commit_now(&self.integ.provider, &self.integ.signer);
        }
    }

    /// Reference backup-takeover policy — the three liveness timers de-mls no
    /// longer keeps, reimplemented as the harness's own policy. Each fires after
    /// the voting-inactivity window once its condition holds: a silent reelection
    /// round advances the retry ladder; an unanswered sync request is re-sent by
    /// a backup; and unproposed buffered updates are proposed — immediately by the
    /// epoch steward, after the window by a backup. Called once per member each
    /// `process` round, after `drive_commit_policy`.
    pub fn drive_takeover_policy(&mut self) {
        let (reelection, sync_resend, buffered, is_epoch, is_steward) = {
            let Some(convo) = self.convo.as_ref() else {
                self.reelection_anchor = None;
                self.sync_resend_anchor = None;
                self.buffered_anchor = None;
                return;
            };
            (
                convo.reelection_stalled(),
                convo.awaiting_sync_resend(),
                convo.pending_buffered_updates() > 0,
                convo.is_epoch_steward().unwrap_or(false),
                convo.is_steward(),
            )
        };
        let now = self.integ.clock.now();
        let window = self.commit_inactivity + self.pending_config.recovery_inactivity_duration;

        if window_elapsed(&mut self.reelection_anchor, now, reelection, window) {
            self.reelection_anchor = None;
            if let Some(convo) = self.convo.as_mut() {
                let _ = convo.advance_election_retry(&self.integ.provider, &self.integ.signer);
            }
        }
        if window_elapsed(&mut self.sync_resend_anchor, now, sync_resend, window) {
            self.sync_resend_anchor = None;
            if let Some(convo) = self.convo.as_mut() {
                let _ = convo.share_conversation_sync(&self.integ.provider, &self.integ.signer);
            }
        }
        // The epoch steward proposes buffered updates immediately; a backup waits
        // out the window. Plain members never drain (window_elapsed clears when
        // `buffered && is_steward` is false).
        let buffered_ready = if buffered && is_epoch {
            self.buffered_anchor = None;
            true
        } else {
            window_elapsed(
                &mut self.buffered_anchor,
                now,
                buffered && is_steward,
                window,
            )
        };
        if buffered_ready {
            self.buffered_anchor = None;
            if let Some(convo) = self.convo.as_mut() {
                let _ = convo.propose_buffered_updates(&self.integ.provider, &self.integ.signer);
            }
        }
    }

    /// Reference Layer-3 recovery-commit policy — the auto-mint de-mls no longer
    /// keeps. While recovery is open, mint via `commit_in_recovery` each round
    /// (idempotent: a no-op once a local candidate exists). A member with the
    /// policy off never presses the button, modelling a manual-only integrator.
    pub fn drive_recovery_policy(&mut self) {
        if !self.recovery_auto_commit {
            return;
        }
        if let Some(convo) = self.convo.as_mut()
            && convo.is_in_recovery_mode()
        {
            let _ = convo.commit_in_recovery(&self.integ.provider, &self.integ.signer);
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
            &invite.member_id,
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
            self.integ.member_id.member_id_bytes(),
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

/// Encode a key package and its owner's `member_id` into the wire format used
/// for KP announcements. Returns the prost-encoded `MemberInvite` bytes ready
/// for broadcast on the welcome subtopic.
pub fn build_key_package_announcement(key_package_bytes: &[u8], member_id: &[u8]) -> Vec<u8> {
    MemberInvite {
        key_package_bytes: key_package_bytes.to_vec(),
        member_id: member_id.to_vec(),
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

    /// Set every member's recovery-commit policy (see
    /// [`Member::drive_recovery_policy`]). `false` models a manual-only
    /// integrator that opens recovery but never calls `commit_in_recovery`.
    pub fn set_recovery_auto_commit(&mut self, enabled: bool) {
        for member in &mut self.members {
            member.recovery_auto_commit = enabled;
        }
    }

    /// Set every member's app commit-inactivity delay (see
    /// [`HARNESS_COMMIT_INACTIVITY`]) — the timing de-mls no longer owns.
    pub fn set_commit_inactivity(&mut self, delay: Duration) {
        for member in &mut self.members {
            member.commit_inactivity = delay;
        }
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
    /// `true` across a forked group whose branches share an epoch and roster
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
            let _ = member.poll();
            member.drive_commit_policy();
            member.drive_takeover_policy();
            member.drive_recovery_policy();
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
