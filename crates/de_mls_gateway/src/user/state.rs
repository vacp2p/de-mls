//! [`User`] struct definition, constructor, accessors, and the
//! consensus-context helpers shared across the User submodules
//! (`lifecycle`, `inbound`, `registry`).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use de_mls::{
    core::{
        ConsensusPlugin, ConversationLifecycle, ConversationPluginsFactory, ScoringConfig,
        SessionEvent, StewardListConfig,
    },
    member_id::MemberId,
    mls_crypto::{KeyPackageBytes, MlsError, MlsService},
    protos::de_mls::messages::v1::{BanRequest, ConversationUpdateRequest},
    session::{
        ConversationState, CreatorVote, MemberRole, PollOutcome, SessionError, SessionRunner,
        SessionTick,
    },
};
use de_mls_ds::{OutboundPacket, SharedDeliveryService};

use crate::user::{LockExt, UserPlugins};

/// Single registry entry: one `Arc<RwLock<SessionRunner>>` per conversation.
/// Cloned out of the registry under the outer read lock, then locked
/// independently — writes on one conversation don't block reads on another.
pub type SessionEntry<P, CP> = Arc<RwLock<SessionRunner<P, CP>>>;

/// Per-user registry of conversation runners. Each entry's inner per-runner
/// lock guards per-conversation reads/mutations so a write on conversation
/// A doesn't block reads on conversation B.
pub(crate) type ConversationRegistry<P, CP> = RwLock<HashMap<String, SessionEntry<P, CP>>>;

pub struct User<P: ConsensusPlugin, CP: ConversationPluginsFactory> {
    pub(crate) member_id: Box<dyn MemberId>,
    /// Per-instance UUID embedded in every outbound packet. Inbound packets
    /// carrying our `app_id` are self-echoes and silently dropped.
    pub(crate) app_id: Vec<u8>,
    /// Synchronous outbound transport. The sessions are pull-only and never
    /// send; [`Self::flush`] drains a session's buffered outbound and
    /// publishes it here. Behind a `Mutex` because the trait takes `&mut self`.
    pub(crate) transport: SharedDeliveryService,
    /// All User-level plugin state: the per-conversation factory, the
    /// consensus context, the key-package provider, and the three default
    /// configs cloned into newly-created sessions.
    pub(crate) plugins: UserPlugins<P, CP>,
    /// Per-conversation `SessionRunner`s.
    pub(crate) conversations: ConversationRegistry<P, CP>,
    /// User-level conversation lifecycle events: `Created(name)` /
    /// `Removed(name)`. Integrators drain via
    /// [`Self::drain_lifecycle_events`] once per polling cycle to learn
    /// when new sessions appear and old ones disappear. Interior `Mutex`
    /// so producer-side methods stay `&self`.
    pub(crate) pending_lifecycle_events: Mutex<Vec<ConversationLifecycle>>,
}

// ── Public API ──────────────────────────────────────────────────────────

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    /// Display form of the local member_id, derived from [`MemberId::member_id_display`].
    pub fn member_id_string(&self) -> String {
        self.member_id.member_id_display().to_string()
    }

    /// Identity bytes of the local user, via the [`MemberId`] trait.
    pub fn member_id_bytes(&self) -> &[u8] {
        self.member_id.member_id_bytes()
    }

    /// Per-instance `app_id` stamped on every outbound. Inbound carrying
    /// this `app_id` is a self-echo and is dropped by
    /// [`Self::handle_inbound`] / [`Self::receive_key_package`].
    pub fn app_id(&self) -> &[u8] {
        &self.app_id
    }

    /// Generate a single-use key package.
    pub fn generate_key_package(&self) -> Result<KeyPackageBytes, MlsError> {
        self.plugins.conversation_plugins.generate_key_package()
    }

    /// Drain every pending [`ConversationLifecycle`] event accumulated
    /// since the last call. Returns events in insertion order. Callers
    /// (gateway, integrator) invoke this once per polling cycle to discover
    /// `Created` / `Removed` sessions and wire up per-session event
    /// drains via [`SessionRunner::drain_events`].
    pub fn drain_lifecycle_events(&self) -> Vec<ConversationLifecycle> {
        match self.pending_lifecycle_events.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => {
                tracing::error!(
                    "lifecycle-event buffer mutex poisoned; integrator will miss Created/Removed events"
                );
                Vec::new()
            }
        }
    }

    /// Override the seed [`ScoringConfig`] used for newly-created per-conversation
    /// scoring plug-ins. Existing conversations are untouched; their plug-ins
    /// already own their live config (joiner-side overwritten by ConversationSync).
    pub fn set_default_scoring_config(&mut self, config: ScoringConfig) {
        self.plugins.default_scoring_config = config;
    }

    /// Override the seed [`StewardListConfig`] used for newly-created
    /// per-conversation steward-list plug-ins. Same lifecycle as
    /// [`Self::set_default_scoring_config`].
    pub fn set_default_steward_list_config(&mut self, config: StewardListConfig) {
        self.plugins.default_steward_list_config = config;
    }

    /// Send a chat message on `conversation_id`. Thin wrapper over
    /// [`SessionRunner::push_message`]. Errors with
    /// `ConversationNotFound` if the conversation has been removed, or
    /// `ConversationBlocked` if the session is gating chat traffic.
    pub fn push_message(
        &self,
        conversation_id: &str,
        message: Vec<u8>,
    ) -> Result<SessionTick, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        let tick = entry.write_or_err("session")?.push_message(message)?;
        self.flush(&entry)?;
        Ok(tick)
    }

    /// Announce `key_package` on `conversation_id` so existing members can
    /// propose adding us. Key-package creation is a user-level concern — the
    /// conversation knows nothing about how a key package is built — so this
    /// builds the announcement and publishes it straight to the transport,
    /// bypassing the session entirely.
    pub fn send_key_package(
        &self,
        conversation_id: &str,
        key_package: KeyPackageBytes,
    ) -> Result<(), SessionError> {
        let payload = de_mls::session::build_key_package_announcement(&key_package);
        let packet = OutboundPacket::key_package(conversation_id, &self.app_id, payload);
        self.transport
            .lock()
            .map_err(|_| SessionError::LockPoisoned("transport"))?
            .publish(packet)
            .map_err(|e| SessionError::Transport(e.to_string()))?;
        Ok(())
    }

    /// Drive one polling cycle on `conversation_id`: tick consensus deadlines,
    /// advance freeze state, check member-freeze inactivity, and check
    /// pending-join expiry. Returns [`PollOutcome`] with a wakeup hint and a
    /// `leave_requested` flag the caller uses to decide whether to finalize
    /// the leave.
    pub fn poll_session(&self, conversation_id: &str) -> Result<PollOutcome, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        let outcome = entry.write_or_err("session")?.poll()?;
        self.flush(&entry)?;
        Ok(outcome)
    }

    /// Drain pending [`SessionEvent`]s for `conversation_id`. Thin
    /// wrapper over [`SessionRunner::drain_events`].
    pub fn drain_events(&self, conversation_id: &str) -> Result<Vec<SessionEvent>, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.drain_events())
    }

    /// Earliest pending deadline on `conversation_id` relative to now,
    /// `None` if nothing is scheduled. Mirrors
    /// [`SessionRunner::next_wakeup_in`].
    pub fn next_wakeup_in(&self, conversation_id: &str) -> Result<Option<Duration>, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.next_wakeup_in())
    }

    /// Propose adding the holder of `key_package_bytes` to
    /// `conversation_id`. The local vote is bundled YES at submit. On
    /// consensus YES the epoch steward authors a commit containing the
    /// Add; the resulting welcome arrives via
    /// [`de_mls::core::SessionEvent::WelcomeReady`] for the integrator to
    /// deliver out of band.
    pub fn add_member(
        &self,
        conversation_id: &str,
        key_package_bytes: &[u8],
    ) -> Result<SessionTick, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        let tick = entry
            .write_or_err("session")?
            .add_member(key_package_bytes)?;
        self.flush(&entry)?;
        Ok(tick)
    }

    /// Ingest a raw MLS welcome blob delivered out of band (e.g. the
    /// inviter's [`de_mls::core::SessionEvent::WelcomeReady`] routed
    /// through the integrator's transport). Returns the joined
    /// conversation name, or [`SessionError::WelcomeNotForUs`] if the
    /// welcome doesn't address this user's key package.
    pub fn accept_welcome(
        &mut self,
        welcome_bytes: &[u8],
    ) -> Result<(String, SessionTick), SessionError> {
        let svc = self
            .plugins
            .conversation_plugins
            .welcome_mls(welcome_bytes)?
            .ok_or(SessionError::WelcomeNotForUs)?;
        let conversation_id = svc.conversation_id().to_string();

        if self.lookup_entry(&conversation_id)?.is_none() {
            self.start_conversation(&conversation_id, false)?;
        }
        let entry_arc = self
            .lookup_entry(&conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;

        entry_arc.write_or_err("session")?.complete_join(svc)?;
        self.flush(&entry_arc)?;
        let tick = SessionTick {
            next_wakeup_in: entry_arc.read_or_err("session")?.next_wakeup_in(),
        };
        Ok((conversation_id, tick))
    }

    // ── UI actions ─────────────────────────────────────────────────────

    /// Cast the local member's manual vote on `proposal_id`. Cancels any
    /// pending auto-vote so the manual choice wins. Thin wrapper over
    /// [`SessionRunner::process_user_vote`].
    pub fn process_user_vote(
        &self,
        conversation_id: &str,
        proposal_id: u32,
        vote: bool,
    ) -> Result<SessionTick, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        entry
            .write_or_err("session")?
            .process_user_vote(proposal_id, vote)?;
        self.flush(&entry)?;
        Ok(SessionTick {
            next_wakeup_in: entry.read_or_err("session")?.next_wakeup_in(),
        })
    }

    /// Open a `RemoveMember` consensus round targeting
    /// `ban_request.user_to_ban`. The local vote is bundled YES at submit.
    /// Thin wrapper over [`SessionRunner::process_ban_request`].
    pub fn process_ban_request(
        &self,
        conversation_id: &str,
        ban_request: BanRequest,
    ) -> Result<SessionTick, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        let tick = entry
            .write_or_err("session")?
            .process_ban_request(ban_request)?;
        self.flush(&entry)?;
        Ok(tick)
    }

    /// Submit `request` as a fresh consensus proposal with
    /// `creator_vote`. Lower-level than
    /// [`Self::add_member`] / [`Self::process_ban_request`]; use those
    /// for membership changes. Thin wrapper over
    /// [`SessionRunner::initiate_proposal`].
    pub fn initiate_proposal(
        &self,
        conversation_id: &str,
        request: ConversationUpdateRequest,
        creator_vote: CreatorVote,
    ) -> Result<SessionTick, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        entry
            .write_or_err("session")?
            .initiate_proposal(request, creator_vote)?;
        self.flush(&entry)?;
        Ok(SessionTick {
            next_wakeup_in: entry.read_or_err("session")?.next_wakeup_in(),
        })
    }

    // ── State queries ──────────────────────────────────────────────────

    /// Current state-machine value for `conversation_id`. Mirrors
    /// [`SessionRunner::get_conversation_state`].
    pub fn get_conversation_state(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationState, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_conversation_state())
    }

    /// `true` if the local user is on the current steward list for
    /// `conversation_id`. Mirrors [`SessionRunner::is_steward_for_self`].
    pub fn is_steward(&self, conversation_id: &str) -> Result<bool, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.is_steward_for_self())
    }

    /// MLS epoch + reelection retry round for `conversation_id`. `(0, 0)`
    /// before MLS is attached. Mirrors
    /// [`SessionRunner::get_epoch_and_retry`].
    pub fn get_epoch_and_retry(&self, conversation_id: &str) -> Result<(u64, u32), SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        entry.read_or_err("session")?.get_epoch_and_retry()
    }

    pub fn get_conversation_members(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Vec<u8>>, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        entry.read_or_err("session")?.get_conversation_members()
    }

    pub fn get_member_scores(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<(Vec<u8>, i64)>, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_member_scores())
    }

    pub fn get_member_score(
        &self,
        conversation_id: &str,
        member_id: &[u8],
    ) -> Result<Option<i64>, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_member_score(member_id))
    }

    pub fn get_member_roles(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<(Vec<u8>, MemberRole)>, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        entry.read_or_err("session")?.get_member_roles()
    }

    /// Identities with an in-flight self-leave request. Mirrors
    /// [`SessionRunner::get_pending_leave_member_ids`].
    pub fn get_pending_leave_member_ids(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<Vec<u8>>, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        entry.read_or_err("session")?.get_pending_leave_member_ids()
    }

    /// Buffered pending membership-update count. Mirrors
    /// [`SessionRunner::get_pending_update_count`].
    pub fn get_pending_update_count(&self, conversation_id: &str) -> Result<usize, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_pending_update_count())
    }

    /// Freeze-round progress as `(received, expected)`. Mirrors
    /// [`SessionRunner::get_freeze_candidate_count`].
    pub fn get_freeze_candidate_count(
        &self,
        conversation_id: &str,
    ) -> Result<(usize, usize), SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry.read_or_err("session")?.get_freeze_candidate_count())
    }

    /// Approved proposals for the current epoch. Mirrors
    /// [`SessionRunner::get_approved_proposals_for_current_epoch`].
    pub fn get_approved_proposals_for_current_epoch(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<ConversationUpdateRequest>, SessionError> {
        let entry = self
            .lookup_entry(conversation_id)?
            .ok_or(SessionError::ConversationNotFound)?;
        Ok(entry
            .read_or_err("session")?
            .get_approved_proposals_for_current_epoch())
    }
}

// ── User-internal helpers ───────────────────────────────────────────────

impl<P: ConsensusPlugin, CP: ConversationPluginsFactory> User<P, CP> {
    pub fn self_member_id(&self) -> &[u8] {
        self.member_id.member_id_bytes()
    }

    /// Append a [`ConversationLifecycle`] event to the pending-events buffer
    /// for [`Self::drain_lifecycle_events`]. Fire-and-forget (no `Result`),
    /// but a poisoned buffer is logged rather than silently dropped.
    pub fn emit_lifecycle(&self, event: ConversationLifecycle) {
        match self.pending_lifecycle_events.lock() {
            Ok(mut buf) => buf.push(event),
            Err(_) => tracing::error!(
                ?event,
                "lifecycle-event buffer mutex poisoned; event dropped"
            ),
        }
    }

    /// Publish a session's buffered outbound on the User's transport. The
    /// session is pull-only — it buffers packets and never sends; this is the
    /// User's push-adapter so its callers keep their send-on-op behaviour.
    /// (When `User` moves out of the library, the integrator drains and
    /// publishes directly instead.)
    pub(crate) fn flush(&self, entry: &SessionEntry<P, CP>) -> Result<(), SessionError> {
        let outbound = entry.read_or_err("session")?.drain_outbound();
        if outbound.is_empty() {
            return Ok(());
        }
        let mut transport = self
            .transport
            .lock()
            .map_err(|_| SessionError::LockPoisoned("transport"))?;
        for out in outbound {
            transport
                .publish(out.into())
                .map_err(|e| SessionError::Transport(e.to_string()))?;
        }
        Ok(())
    }

    /// Drop this conversation's consensus scope from the shared storage.
    /// Called on leave (after the runner has already cancelled its own
    /// auto-votes) and pending-join timeout.
    pub fn cleanup_consensus_scope(&self, conversation_id: &str) -> Result<(), SessionError> {
        let scope = P::Scope::from(conversation_id.to_string());
        self.plugins.consensus.delete_scope(&scope)?;
        Ok(())
    }

    pub fn new_with_plugins(
        member_id: Box<dyn MemberId>,
        plugins: UserPlugins<P, CP>,
        transport: SharedDeliveryService,
    ) -> Self {
        Self {
            member_id,
            app_id: uuid::Uuid::new_v4().as_bytes().to_vec(),
            transport,
            plugins,
            conversations: RwLock::new(HashMap::new()),
            pending_lifecycle_events: Mutex::new(Vec::new()),
        }
    }
}
